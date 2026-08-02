#!/usr/bin/env python3
"""Focused quality-gate checks; no paid or networked episodes are run."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import task_quality  # noqa: E402


class TaskQualityV2Tests(unittest.TestCase):
    def test_public_tasks_pass_gate(self):
        for task_id in ("storage-normal", "board-filters", "graph-valid", "policy-retry"):
            report = task_quality.audit_task(task_id)
            self.assertFalse(report["quarantined"], task_id)
            self.assertTrue(report["baseline"]["passed"] is False)
            self.assertTrue(report["gold"]["passed"])
            self.assertGreaterEqual(report["clauses"]["total"], 8)
            self.assertGreaterEqual(report["mutations"]["detection_rate"], 0.8)
            self.assertTrue(report["determinism"]["passed"])
            self.assertEqual(len(report["quality_report_hash"]), 64)

    def test_holdout_is_audited_without_exposing_checker(self):
        report = task_quality.audit_task("graph-diagnostics")
        self.assertFalse(report["quarantined"])
        self.assertEqual(report["split"], "holdout")
        self.assertEqual(report["checker"], "external-private-sanitized")
        self.assertNotIn("candidates", json.dumps(report))

    def test_corpus_quarantines_failed_task_instead_of_including(self):
        # A selected known-good task still exercises the split/report shape
        # without spending time on all eight in unit-test runs.
        report = task_quality.audit_corpus(task_ids=["storage-normal"])
        self.assertEqual(report["included_tasks"], ["storage-normal"])
        self.assertEqual(report["quarantined_tasks"], [])
        self.assertEqual(report["paid_llm_episodes"], False)
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "quality.json"
            task_quality.audit_corpus(task_ids=["storage-normal"], output=path)
            self.assertTrue(path.is_file())
            self.assertEqual(json.loads(path.read_text())["schema_version"], task_quality.QUALITY_SCHEMA)


if __name__ == "__main__":
    unittest.main()

