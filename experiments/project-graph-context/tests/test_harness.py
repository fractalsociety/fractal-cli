#!/usr/bin/env python3
"""Focused unit/integration tests for the deterministic benchmark harness."""

from __future__ import annotations

import json
import os
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from analysis import analyze  # noqa: E402
from corpus import materialize_task_repo, task_manifest  # noqa: E402
from runner import EpisodeSpec, RunnerConfig, run_episode, score_path_scope  # noqa: E402
from scorer import ScoreError, validate_ledger  # noqa: E402


class HarnessTests(unittest.TestCase):
    def _episode(self, root: Path, task: str = "calculator-add", arm: str = "C", command: tuple[str, ...] | None = None, experiment: str = "test"):
        source = materialize_task_repo(task, root / f"source-{task}")
        commit = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
        context = root / f"context-{arm}.json"
        context.write_text(json.dumps({"arm": arm, "task_id": task}) + "\n", encoding="utf-8")
        intent = root / f"intent-{task}.json"
        intent.write_text(json.dumps(task_manifest(task)) + "\n", encoding="utf-8")
        worker = HERE / "scripted_worker.py"
        return EpisodeSpec(experiment, arm, task, source, commit, command or (sys.executable, str(worker), "--task-id", task), context, intent, root / "episodes", 0, RunnerConfig(timeout_seconds=2, max_output_bytes=100_000, max_repairs=8))

    def test_scope_counts_forbidden_and_out_of_scope_paths(self):
        intent = task_manifest("calculator-add")
        score = score_path_scope(["src/calculator.py", "README.md", "../escape", ".git/config"], intent)
        self.assertEqual(score["severe"], 3)
        self.assertEqual(score["weighted"], 5.0)

    def test_context_is_outside_worktree_and_read_only(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            command = (sys.executable, "-c", "from pathlib import Path; import os; p=Path(os.environ['FRACTAL_CONTEXT_PATH']);\ntry:\n p.write_text('tamper') if p.is_file() else (p/'tamper').write_text('tamper')\nexcept PermissionError: pass")
            ledger = run_episode(self._episode(root, command=command))
            mount = Path(next(event for event in ledger["events"] if event["kind"] == "context_mounted")["data"]["context"])
            self.assertNotEqual(mount.parent, Path(ledger["result"].get("worktree", "")))
            self.assertTrue(mount.exists())
            mode = stat.S_IMODE(mount.stat().st_mode)
            self.assertEqual(mode & stat.S_IWUSR, 0)

    def test_hidden_checker_is_not_in_worker_worktree(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            spec = self._episode(root, experiment="hidden", task="calculator-add", arm="C")
            spec = EpisodeSpec(**{**spec.__dict__, "config": RunnerConfig(timeout_seconds=2, max_output_bytes=100_000, keep_worktree=True)})
            ledger = run_episode(spec)
            worktree = root / "episodes" / "hidden-C-calculator-add-0" / "worktree"
            hidden = root / "episodes" / "hidden-C-calculator-add-0" / "hidden-checker"
            self.assertTrue(worktree.exists())
            self.assertTrue(hidden.exists())
            self.assertFalse((worktree / "checker.py").exists())
            self.assertNotEqual(worktree.resolve(), hidden.resolve())
            subprocess.run(["git", "-C", str(spec.source_repo), "worktree", "remove", "--force", str(worktree)], check=True)

    def test_missing_usage_receipt_stays_unavailable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            command = (sys.executable, "-c", "from pathlib import Path; import os; Path(os.environ['FRACTAL_WORKTREE'], 'src/calculator.py').write_text('def add(a,b): return a+b\\n')")
            ledger = run_episode(self._episode(root, command=command))
            tokens = ledger["result"]["tokens"]
            self.assertFalse(tokens["available"])
            self.assertIsNone(tokens["total"])

    def test_invalid_usage_receipt_is_not_telemetry(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            command = (sys.executable, "-c", "from pathlib import Path; import os, json; Path(os.environ['FRACTAL_USAGE_RECEIPT_PATH']).write_text(json.dumps({'total_tokens': 3}))")
            ledger = run_episode(self._episode(root, command=command))
            self.assertFalse(ledger["result"]["tokens"]["available"])
            self.assertTrue(any(event["kind"] == "usage_unavailable" for event in ledger["events"]))

    def test_scripted_checker_pass_and_fail(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            passed = run_episode(self._episode(root, task="calculator-add", arm="C", experiment="pass"))
            self.assertTrue(passed["result"]["correctness"]["passed"])
            failed = run_episode(self._episode(root, task="text-slugify-edge", arm="A", experiment="fail"))
            self.assertFalse(failed["result"]["correctness"]["passed"])
            self.assertEqual(failed["result"]["correctness"]["checker_failure_code"], "oracle_assertion_failed")

    def test_timeout_is_recorded(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            command = (sys.executable, "-c", "import time; time.sleep(2)")
            spec = self._episode(root, command=command)
            spec = EpisodeSpec(**{**spec.__dict__, "config": RunnerConfig(timeout_seconds=0.05, max_output_bytes=100_000)})
            ledger = run_episode(spec)
            self.assertTrue(ledger["result"]["timed_out"])
            self.assertIsNone(ledger["result"]["exit_code"])

    def test_hashes_and_order_are_stable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first = run_episode(self._episode(root / "one", experiment="one"))
            second = run_episode(self._episode(root / "two", experiment="two"))
            self.assertEqual(first["result"]["changed_paths"], second["result"]["changed_paths"])
            self.assertEqual(first["result"]["evidence_hashes"], second["result"]["evidence_hashes"])
            self.assertEqual([e["sequence"] for e in first["events"]], list(range(len(first["events"]))))
            self.assertEqual([e["sequence"] for e in second["events"]], list(range(len(second["events"]))))

    def test_analysis_thresholds_and_underpowered_no_go(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            ledgers = []
            # Four tasks x two replicates create complete pairs for a small
            # harness check; the preregistered 10-pair floor is tested below.
            for task in ("calculator-add", "calculator-subtract", "text-slugify", "text-slugify-edge"):
                for arm in "ABCD":
                    for replicate in range(2):
                        spec = self._episode(root / f"{task}-{arm}-{replicate}", task=task, arm=arm, experiment=f"{arm}{replicate}")
                        spec = EpisodeSpec(**{**spec.__dict__, "replicate": replicate})
                        ledgers.append(run_episode(spec))
            report = analyze(ledgers, seed=7, min_pairs=2, bootstrap_samples=50)
            self.assertEqual(report["comparisons"]["C_vs_A"]["decision"], "pass")
            self.assertEqual(report["comparisons"]["D_vs_C"]["decision"], "pass")
            underpowered = analyze(ledgers, seed=7, min_pairs=10, bootstrap_samples=20)
            self.assertTrue(underpowered["no_go"])
            self.assertTrue(underpowered["underpowered"])

    def test_ledger_validation_rejects_noncontiguous_events(self):
        with self.assertRaises(ScoreError):
            validate_ledger({"schema_version": "project-graph-context.event-result-ledger.v1", "episode_id": "x", "experiment_id": "x", "arm_id": "A", "task_id": "x", "events": [{"sequence": 2}], "result": {}})


if __name__ == "__main__":
    unittest.main()
