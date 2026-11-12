#!/usr/bin/env python3
"""Offline isolation checks for rank training; requires the trainer's numpy."""
import unittest
import contextlib
import io
import json
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch
import numpy as np
import train_rank


class CrossValidationTests(unittest.TestCase):
    def row(self, name, value, set_name="gold"):
        return {"id": name, "set": set_name,
                "X": np.full((2, train_rank.FEATURES), value, dtype=float)}

    def test_validation_outliers_and_repeated_queries_do_not_train_the_scaler(self):
        fold_of = {"validation": 0, "first": 1, "second": 2}
        means = []
        for outlier in (10**6, 10**12):
            rows = [self.row("validation", outlier), self.row("validation", -outlier),
                    self.row("first", 2), self.row("second", 8)]
            observations = []
            def fit(training, mean, std, l2):
                observations.append(({r["id"] for r in training}, mean.copy(), std.copy()))
                return np.zeros(train_rank.FEATURES)
            with patch.object(train_rank, "fit", side_effect=fit):
                scores = train_rank.cross_validated_scores(rows, fold_of, .1)
            self.assertEqual(set(scores), set(range(len(rows))))
            names, mean, std = observations[0]
            self.assertEqual(names, {"first", "second"})
            self.assertEqual(mean[0], 5)
            self.assertEqual(std[0], 3)
            means.append(mean)
        np.testing.assert_array_equal(means[0], means[1])

    def test_held_rows_cannot_enter_cross_validation(self):
        with self.assertRaisesRegex(ValueError, "gold rows only"):
            train_rank.cross_validated_scores([self.row("held", 100, "held")], {"held": 0}, .1)
        with self.assertRaisesRegex(ValueError, "two nonempty folds"):
            train_rank.cross_validated_scores([self.row("same", 100)], {"same": 0}, .1)

    def test_repeated_queries_cannot_replace_a_corpus_or_overwrite_weights(self):
        with tempfile.TemporaryDirectory() as temp:
            source, output = Path(temp)/"features.jsonl", Path(temp)/"weights.rs"
            rows = [{"id": label, "set": label, "candidates": [
                {"x": [0]*train_rank.FEATURES, "label": 0},
                {"x": [1]*train_rank.FEATURES, "label": 2}]} for label in ("gold", "held")]
            source.write_text("\n".join(json.dumps(row) for row in rows for _ in range(25)))
            output.write_text("existing weights")
            error = io.StringIO()
            with patch.object(sys, "argv", ["train_rank", "--features", str(source), "--out", str(output)]), contextlib.redirect_stderr(error):
                self.assertEqual(train_rank.main(), 2)
            self.assertIn("distinct queries", error.getvalue())
            self.assertEqual(output.read_text(), "existing weights")


if __name__ == "__main__":
    unittest.main()
