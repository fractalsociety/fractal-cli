#!/usr/bin/env python3
"""Trusted corpus-v2 runner tests; none of these tests make a model call."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parents[1]
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import live_v2_runner as runner  # noqa: E402
from live_v2_adapter import (  # noqa: E402
    LUNA_STRUCTURED_PATCH_ROUTE,
    _planner_failure_summary,
    codex_argv,
    preload_seed_files,
    structured_patch_prompt,
    structured_patch_schema,
    trace_from_events,
    _write_sanitized_events,
)
from live_v2_policy import route_eligibility  # noqa: E402


class LiveV2RunnerTests(unittest.TestCase):
    def test_exact_allowlist_and_holdouts_refuse(self):
        self.assertEqual(runner.validate_live_tasks(None), runner.LIVE_TASK_ALLOWLIST)
        with self.assertRaises(runner.RunnerError):
            runner.validate_live_tasks(("policy-terminal",))
        with self.assertRaises(runner.RunnerError):
            runner.validate_live_tasks(("calculator-add",))

    def test_seed_copy_has_only_fixture_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "seed"
            metadata = runner.materialize_seed_only("storage-normal", destination)
            self.assertFalse((destination / ".git").exists())
            self.assertFalse(any("oracles" in path.parts for path in destination.rglob("*")))
            self.assertFalse(any("graph-state" in path.name for path in destination.rglob("*")))
            self.assertEqual(metadata["contains_git"], False)
            self.assertEqual(metadata["contains_oracle"], False)

    def test_plan_only_is_zero_model_calls(self):
        with tempfile.TemporaryDirectory() as temporary, mock.patch.object(runner, "run_planner_v2", side_effect=AssertionError("paid planner")):
            summary = runner.plan_only(("storage-normal", "graph-valid"), Path(temporary))
            self.assertEqual(summary["model_calls"], 0)
            self.assertEqual(summary["mode"], "plan-only")
            self.assertTrue((Path(temporary) / "plans" / "storage-normal.json").is_file())

    def test_calibrate_only_proves_all_local_denials_without_checker(self):
        with tempfile.TemporaryDirectory() as temporary, mock.patch.object(runner, "run_planner_v2", side_effect=AssertionError("paid planner")):
            summary = runner.calibrate_only(("storage-normal",), Path(temporary))
            self.assertEqual(summary["model_calls"], 0)
            self.assertTrue(summary["passed"], summary)
            checks = summary["tasks_detail"][0]["isolation"]["checks"]
            self.assertEqual(set(checks), {"relative_oracle_read", "absolute_oracle_read", "traversal_oracle_read", "evaluator_read", "filesystem_search", "environment_leakage", "process_inspection"})
            self.assertTrue(all(value == "denied" for value in checks.values()))

    def test_profile_has_protected_denies_and_workspace_service_grants(self):
        text = runner.sandbox_profile(Path("/tmp/pgc-worktree"), original_oracles=Path("/tmp/oracles"), evaluator_staging=Path("/tmp/evaluator"), readonly_roots=(Path("/tmp/staging"),))
        self.assertIn(f'(deny file-read* (subpath "{Path("/tmp/oracles").resolve()}"))', text)
        self.assertIn(f'(deny file-read* (subpath "{Path("/tmp/evaluator").resolve()}"))', text)
        self.assertIn(f'(allow file-write* (subpath "{Path("/tmp/pgc-worktree").resolve()}"))', text)
        self.assertIn('(allow network-outbound (remote tcp "*:443"))', text)
        self.assertIn("(allow network-outbound)", text)
        self.assertIn('(allow mach-lookup (global-name "com.apple.SystemConfiguration.configd"))', text)
        self.assertIn("(allow system-socket)", text)
        self.assertIn('(allow file-write* (literal "/dev/ptmx"))', text)
        self.assertIn('(allow signal (target children))', text)
        self.assertIn("(deny process-info-listpids)", text)

    def test_outer_transport_and_inner_worker_network_are_distinct(self):
        argv = codex_argv(model="gpt-5.6-luna", sandbox="workspace-write", cwd=Path("/tmp/pgc-worktree"))
        self.assertIn("sandbox_workspace_write.network_access=false", argv)
        self.assertIn("(allow network-outbound)", runner.sandbox_profile(Path("/tmp/pgc-worktree"), original_oracles=Path("/tmp/oracles"), evaluator_staging=Path("/tmp/evaluator")))

    def test_run_task_requires_go_before_any_call(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(runner.RunnerError):
                runner.run_task("storage-normal", Path(temporary), go=False)

    def test_result_shape_does_not_store_raw_checker_payload(self):
        # The checker result contract is intentionally narrow and hash based.
        safe = {"passed": False, "failure_code": "oracle_assertion_failed", "checker_exit_code": 1}
        payload = {"checker": {**safe, "sanitized_sha256": runner.sha256_bytes(runner.canonical_json(safe)), "checker_sha256": "a" * 64}}
        rendered = json.dumps(payload)
        self.assertNotIn("expected", rendered)
        self.assertNotIn("clauses", rendered)

    def test_sol_planner_launch_carries_seatbelt_profile(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            plan_dir = root / "plans"
            profile = root / "worker.sb"
            profile.write_text("(version 1)\n", encoding="utf-8")
            (plan_dir).mkdir()
            (plan_dir / "storage-normal.json").write_text(json.dumps(runner.build_plan("storage-normal")), encoding="utf-8")
            (plan_dir / "storage-normal.metadata.json").write_text("{}", encoding="utf-8")
            completed = runner.subprocess.CompletedProcess([], 0, b"planner-summary", b"")
            with mock.patch.object(runner.subprocess, "run", return_value=completed) as launch:
                _, metadata = runner._run_planner("storage-normal", plan_dir, profile)
            kwargs = launch.call_args.kwargs
            self.assertEqual(kwargs["env"]["FRACTAL_SANDBOX_PROFILE"], str(profile.resolve()))
            self.assertEqual(metadata["model"], "gpt-5.6-sol")

    def test_schema_smoke_reads_exact_planner_schema_under_seatbelt(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            worktree = root / "worktree"
            runner.materialize_seed_only("storage-normal", worktree)
            staging, evaluator, profile = root / "staging", root / "evaluator", root / "worker.sb"
            staging.mkdir()
            profile.write_text(runner.sandbox_profile(worktree, original_oracles=runner.ROOT / "fixtures_v2" / "oracles", evaluator_staging=evaluator, readonly_roots=(runner.ROOT / "schemas", staging, Path(tempfile.gettempdir()), *runner._codex_runtime_roots())), encoding="utf-8")
            profile.chmod(0o400)
            result = runner.schema_smoke(profile, worktree, runner.ROOT / "schemas" / "live-plan.v1.schema.json", runner._minimal_env())
            self.assertTrue(result["passed"], result)
            self.assertEqual(len(result["schema_sha256"]), 64)

    def test_fake_codex_freezes_plan_through_exact_sandboxed_planner_path(self):
        """Exercise adapter argv/profile/auth staging without a paid request."""
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            worktree = root / "worktree"
            runner.materialize_seed_only("storage-normal", worktree)
            staging, evaluator, profile, planner_dir, codex_home = root / "staging", root / "evaluator", root / "worker.sb", root / "plans", root / "codex-home"
            staging.mkdir()
            fake = root / "fake-codex"
            plan = runner.build_plan("storage-normal")
            event_text = json.dumps({"type": "item.completed", "item": {"type": "agent_message", "text": json.dumps(plan)}}) + "\n" + json.dumps({"type": "turn.completed", "usage": {"input_tokens": 11, "output_tokens": 7}}) + "\n"
            fake.write_text("#!/Applications/Xcode.app/Contents/Developer/usr/bin/python3\nimport os, sys\nfrom pathlib import Path\nPath(os.environ['CODEX_HOME'], 'smoke-write').write_text('ok')\nsys.stdout.write(" + repr(event_text) + ")\n", encoding="utf-8")
            fake.chmod(0o755)
            codex_home.mkdir()
            profile.write_text(runner.sandbox_profile(worktree, original_oracles=runner.ROOT / "fixtures_v2" / "oracles", evaluator_staging=evaluator, readonly_roots=(root, runner.ROOT / "schemas", Path(tempfile.gettempdir()), *runner._codex_runtime_roots()), writable_roots=(codex_home,)), encoding="utf-8")
            profile.chmod(0o400)
            with mock.patch.dict("os.environ", {"FRACTAL_CODEX_BIN": str(fake), "FRACTAL_SANDBOX_EXEC": "/usr/bin/sandbox-exec"}, clear=False):
                frozen, metadata = runner._run_planner("storage-normal", planner_dir, profile, codex_home)
            self.assertEqual(frozen, plan)
            self.assertEqual(metadata["model"], "gpt-5.6-sol")
            self.assertEqual(len(metadata["plan_sha256"]), 64)
            self.assertEqual(metadata["usage"], {"input_tokens": 11, "output_tokens": 7, "total_tokens": 18})

    def test_auth_or_codex_home_attempt_is_sanitized_as_safety_signal(self):
        trace = trace_from_events(
            [{"type": "item.completed", "item": {"type": "command_execution", "command": "cat $CODEX_HOME/auth.json", "exit_code": 1}}],
            ["app/storage.py"],
        )
        self.assertEqual(trace["leakage_attempts"], ["protected_path_or_content"])

    def test_planner_failure_summary_has_only_event_codes_and_category(self):
        summary = _planner_failure_summary(
            [{"type": "turn.failed", "code": "response_stream_disconnected", "item": {"type": "error", "status": "failed", "message": "secret text"}}],
            b"websocket closed by server before response.completed; bearer token redacted",
        )
        self.assertEqual(summary["event_type_counts"], {"turn.failed": 1})
        self.assertEqual(summary["error_events"][0]["code"], "response_stream_disconnected")
        self.assertNotIn("message", summary["error_events"][0])
        self.assertEqual(summary["stderr_category"], "auth")

    def test_worker_event_summary_has_item_counts_and_hashed_final_without_text(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "events.json"
            _write_sanitized_events(path, [{"type": "item.completed", "item": {"type": "agent_message", "text": "Implemented the change."}}], prefix={}, raw_stdout=b"raw")
            payload = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(payload["item_types"], {"agent_message": 1})
            self.assertEqual(payload["final_category"], "implemented")
            self.assertEqual(payload["final_length"], len("Implemented the change."))
            self.assertNotIn("Implemented the change.", path.read_text(encoding="utf-8"))

    def _patch_fixture(self):
        temporary = tempfile.TemporaryDirectory()
        root = Path(temporary.name)
        worktree = root / "worktree"
        runner.materialize_seed_only("storage-normal", worktree)
        target = worktree / "app/storage.py"
        current = target.read_text(encoding="utf-8")
        return temporary, worktree, target, current

    def test_structured_patch_schema_is_strict(self):
        schema = structured_patch_schema()
        self.assertEqual(schema["required"], ["changes", "summary", "checks"])
        self.assertFalse(schema["additionalProperties"])
        self.assertFalse(schema["properties"]["changes"]["items"]["additionalProperties"])

    def test_structured_patch_rejects_malicious_path_without_write(self):
        temporary, worktree, target, current = self._patch_fixture()
        try:
            with self.assertRaises(runner.StructuredPatchError) as raised:
                runner.validate_structured_patch(
                    {"changes": [{"path": "../escape.py", "content": "owned"}], "summary": "x", "checks": []},
                    worktree,
                    ["app/storage.py"],
                )
            self.assertIn("traversal", raised.exception.code)
            self.assertEqual(target.read_text(encoding="utf-8"), current)
        finally:
            temporary.cleanup()

    def test_structured_patch_rejects_duplicate_paths(self):
        temporary, worktree, _target, current = self._patch_fixture()
        try:
            with self.assertRaises(runner.StructuredPatchError) as raised:
                runner.validate_structured_patch(
                    {"changes": [{"path": "app/storage.py", "content": current + "\n# one"}, {"path": "app/storage.py", "content": current + "\n# two"}], "summary": "x", "checks": []},
                    worktree,
                    ["app/storage.py"],
                )
            self.assertEqual(raised.exception.code, "structured_patch_duplicate")
        finally:
            temporary.cleanup()

    def test_structured_patch_rejects_no_change(self):
        temporary, worktree, _target, current = self._patch_fixture()
        try:
            with self.assertRaises(runner.StructuredPatchError) as raised:
                runner.validate_structured_patch(
                    {"changes": [{"path": "app/storage.py", "content": current}], "summary": "x", "checks": []},
                    worktree,
                    ["app/storage.py"],
                )
            self.assertEqual(raised.exception.code, "structured_patch_no_change")
        finally:
            temporary.cleanup()

    def test_structured_patch_good_patch_applies_atomically(self):
        temporary, worktree, target, current = self._patch_fixture()
        try:
            payload = {"changes": [{"path": "app/storage.py", "content": current + "\n# structured patch\n"}], "summary": "bounded change", "checks": ["python -m compileall"]}
            validated = runner.validate_structured_patch(payload, worktree, ["app/storage.py"])
            applied = runner.apply_structured_patch(validated, worktree)
            self.assertTrue(applied["applied"])
            self.assertEqual(applied["changed_paths"], ["app/storage.py"])
            self.assertEqual(target.read_text(encoding="utf-8"), current + "\n# structured patch\n")
            self.assertEqual(len(applied["changed_file_hashes"]["app/storage.py"]), 64)
        finally:
            temporary.cleanup()

    def test_structured_patch_rejects_symlink_target(self):
        temporary, worktree, target, current = self._patch_fixture()
        try:
            outside = Path(temporary.name) / "outside.py"
            outside.write_text(current, encoding="utf-8")
            target.unlink()
            target.symlink_to(outside)
            with self.assertRaises(runner.StructuredPatchError) as raised:
                runner.validate_structured_patch(
                    {"changes": [{"path": "app/storage.py", "content": current + "\n# x"}], "summary": "x", "checks": []},
                    worktree,
                    ["app/storage.py"],
                )
            self.assertEqual(raised.exception.code, "structured_patch_symlink")
        finally:
            temporary.cleanup()

    def test_structured_prompt_contains_only_four_context_payloads_and_open_count(self):
        temporary, worktree, _target, _current = self._patch_fixture()
        try:
            intent = runner.task_manifest("storage-normal")
            context = runner.build_context("storage-normal")
            plan = runner.build_plan("storage-normal")
            seed, opened, _digest = preload_seed_files(worktree, intent["allowed_paths"])
            self.assertEqual(opened, len(intent["allowed_paths"]))
            prompt = structured_patch_prompt(intent, plan, context, seed)
            self.assertIn('"task_manifest"', prompt)
            self.assertIn('"context_condition_c"', prompt)
            self.assertIn('"frozen_sol_plan"', prompt)
            self.assertIn('"allowed_seed_files"', prompt)
            self.assertNotIn("fixtures_v2/oracles/checker.py", prompt)
            self.assertNotIn("CODEX_HOME", prompt)
        finally:
            temporary.cleanup()

    def test_structured_route_preflight_is_eligible_with_shell_boundary(self):
        route = route_eligibility(
            "codex",
            preflight={"provider": "codex", "status": "available", "version": "codex 0.145.0"},
            shell_allowed=True,
            network_denied=True,
        )
        self.assertEqual(route["status"], "eligible")
        self.assertEqual(route["controls"]["network"], "enforced")

    def test_mocked_structured_worker_transport_applies_after_exit(self):
        temporary, worktree, target, current = self._patch_fixture()
        try:
            staging, profile = Path(temporary.name) / "staging", Path(temporary.name) / "worker.sb"
            staging.mkdir()
            profile.write_text("(version 1)\n", encoding="utf-8")
            patch = {"changes": [{"path": "app/storage.py", "content": current + "\n# transport\n"}], "summary": "x", "checks": []}
            envelope = {"route": LUNA_STRUCTURED_PATCH_ROUTE, "patch": patch, "patch_status": "ready", "patch_sha256": "a" * 64, "preloaded_file_open_count": 3}
            completed = runner.subprocess.CompletedProcess([], 0, (json.dumps(envelope) + "\n").encode("utf-8"), b"")
            with mock.patch.dict("os.environ", {"FRACTAL_LUNA_ROUTE": LUNA_STRUCTURED_PATCH_ROUTE}, clear=False), mock.patch.object(runner.subprocess, "run", return_value=completed):
                result = runner._run_worker("storage-normal", worktree, staging, profile, "episode")
            self.assertTrue(result["applied"])
            self.assertEqual(result["patch_status"], "applied")
            self.assertEqual(result["preloaded_file_open_count"], 3)
            self.assertTrue(target.read_text(encoding="utf-8").endswith("# transport\n"))
        finally:
            temporary.cleanup()


if __name__ == "__main__":
    unittest.main()
