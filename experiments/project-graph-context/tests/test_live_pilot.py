#!/usr/bin/env python3
"""Pure tests for live-pilot safety gates (no Codex calls)."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from live_adapter import _actual_usage_receipt, _trace_from_events  # noqa: E402
from live_pilot import calibration_budget_aborted, context_payload, _schedule  # noqa: E402


class LivePilotSafetyTests(unittest.TestCase):
    def test_schedule_is_counterbalanced_and_seeded(self):
        tasks = ("calculator-add", "calculator-subtract")
        first = _schedule(tasks, 1729)
        second = _schedule(tasks, 1729)
        self.assertEqual(first, second)
        first_order = [row["arm_id"] for row in first[:4]]
        second_order = [row["arm_id"] for row in second[4:]]
        self.assertEqual(second_order, list(reversed(first_order)))

    def test_context_exposure_has_no_map_in_a_and_prior_only_in_d(self):
        a = context_payload("calculator-add", "A", "calculator-subtract")
        b = context_payload("calculator-add", "B", "calculator-subtract")
        c = context_payload("calculator-add", "C", "calculator-subtract")
        d = context_payload("calculator-add", "D", "calculator-subtract")
        self.assertNotIn("layers", a)
        self.assertNotIn("prior", b)
        self.assertNotIn("prior", c)
        self.assertEqual(d["prior"]["outcomes"][0]["task_id"], "calculator-subtract")
        self.assertNotEqual(c.get("arm_id"), d.get("arm_id"))

    def test_budget_gate_blocks_missing_or_over_cap_receipts(self):
        self.assertTrue(calibration_budget_aborted({"result": {"timed_out": True, "exit_code": 0, "tokens": {"total": 100}}}))
        self.assertTrue(calibration_budget_aborted({"result": {"timed_out": False, "exit_code": 0, "tokens": {"total": 20_001}}}))
        self.assertTrue(calibration_budget_aborted({"result": {"timed_out": False, "exit_code": 0, "tokens": {"total": None}}}))
        self.assertFalse(calibration_budget_aborted({"result": {"timed_out": False, "exit_code": 0, "tokens": {"total": 20_000}}}))

    def test_usage_receipt_uses_cli_fields_and_leaves_cost_missing(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "usage.json"
            receipt, state = _actual_usage_receipt(
                [{"type": "turn.completed", "usage": {"input_tokens": 12, "output_tokens": 8, "cached_input_tokens": 4}}],
                path,
            )
            self.assertEqual(state, "valid")
            self.assertEqual(receipt["total_tokens"], 20)
            self.assertIsNone(receipt["cost_usd"])
            self.assertEqual(json.loads(path.read_text(encoding="utf-8"))["cli_usage_fields"][0]["input_tokens"], 12)

    def test_trace_does_not_infer_repairs_or_routing(self):
        trace = _trace_from_events(
            [
                {"type": "item.completed", "item": {"type": "command_execution", "command": "cat src/calculator.py", "exit_code": 0}},
            ],
            ["src/calculator.py"],
        )
        self.assertIn("opens", trace)
        self.assertNotIn("repair_iterations", trace)
        self.assertNotIn("routing", trace)


if __name__ == "__main__":
    unittest.main()
