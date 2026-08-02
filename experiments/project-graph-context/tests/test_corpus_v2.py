#!/usr/bin/env python3
"""Deterministic and isolation checks for corpus v2 metadata."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import corpus_v2  # noqa: E402


class CorpusV2Tests(unittest.TestCase):
    def test_eight_tasks_four_pairs_and_splits(self):
        self.assertEqual(len(corpus_v2.task_ids()), 8)
        self.assertEqual(len(corpus_v2.related_pairs()), 4)
        self.assertEqual(len(corpus_v2.task_ids(split="public")), 4)
        self.assertEqual(len(corpus_v2.task_ids(split="holdout")), 4)
        self.assertEqual(len(set(corpus_v2.task_ids(split="public")) & set(corpus_v2.task_ids(split="holdout"))), 0)

    def test_fixture_trees_are_multifile_and_scoped(self):
        for task_id in corpus_v2.task_ids():
            files = corpus_v2.fixture_files(task_id)
            self.assertGreaterEqual(len(files), 3)
            self.assertLessEqual(len(files), 8)
            self.assertNotIn("oracles/checker.py", files)
            self.assertTrue(all("graph-state" not in path for path in files))

    def test_materialization_is_reproducible_and_oracle_external(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first = corpus_v2.materialize_task_repo_v2("storage-normal", root / "one")
            second = corpus_v2.materialize_task_repo_v2("storage-normal", root / "two")
            commit_a = subprocess.check_output(["git", "-C", str(first), "rev-parse", "HEAD"], text=True).strip()
            commit_b = subprocess.check_output(["git", "-C", str(second), "rev-parse", "HEAD"], text=True).strip()
            self.assertEqual(commit_a, commit_b)
            checker = corpus_v2.copy_hidden_oracle(root / "private")
            self.assertNotEqual(checker.parent.resolve(), first.resolve())
            result = corpus_v2.run_hidden_oracle("storage-normal", first, checker)
            self.assertFalse(result["passed"])
            self.assertNotIn("clauses", result)

    def test_split_metadata_has_stable_hashes_and_no_checker_content(self):
        first = corpus_v2.split_metadata()
        second = corpus_v2.split_metadata()
        self.assertEqual(first, second)
        self.assertEqual(len(first["corpus_hash"]), 64)
        self.assertEqual(first["holdout_checker_contents"], "sealed-external")
        rendered = json.dumps(first, sort_keys=True).lower()
        self.assertNotIn("expected", rendered)

    def test_telemetry_missing_is_null_not_zero(self):
        manifest = corpus_v2.telemetry_manifest()
        self.assertIsNone(manifest["missing_value"])
        self.assertIn("usage_receipt", manifest["required"])


if __name__ == "__main__":
    unittest.main()

