#!/usr/bin/env python3
"""Offline checks of the Stage 3 runner's summary gate and health witness.

Never launches Trellis: it imports the runner and feeds it records.
"""
import copy
import importlib.util
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "stage3_runner", Path(__file__).with_name("stage3-watcher-runner.py"))
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)

HEALTHY = {"epoch": 2, "native": True, "degraded": False, "degradations": 0}


def records(overrides=None):
    """A complete, healthy local run (both sizes), with per-sample overrides."""
    overrides = overrides or {}
    out = [{"type": "run_header"}]
    for size in ("medium", "large"):
        out.append({"type": "series_watcher", "size": size, "witness": dict(HEALTHY), "native": True})
        for n in range(20):
            sample = {"type": "sample", "size": size, "sample_index": n + 1,
                      "path": RUNNER.planned_edit(size, n)[1], "valid": True,
                      "latency_seconds": 0.5, "mode_expected": "local",
                      "mode_note": RUNNER.MODE_NOTE, "mode_directly_observed": False,
                      "watcher_witness": dict(HEALTHY),
                      "watcher_degraded_after": False, "watcher_witness_ok": True}
            sample.update(overrides.get((size, n), {}))
            out.append(sample)
    return out


class Summary(unittest.TestCase):
    def gate(self, rows):
        return RUNNER.summarize_records(rows)["local_watcher_latency_gate_pass"]

    def test_a_healthy_proven_run_passes(self):
        self.assertTrue(self.gate(records()))

    def test_a_sample_without_health_proof_does_not_count(self):
        for field in ("watcher_degraded_after", "watcher_witness_ok", "watcher_witness"):
            rows = copy.deepcopy(records())
            del rows[5][field]
            self.assertFalse(self.gate(rows), field)

    def test_a_degraded_or_unwitnessed_sample_fails_the_series(self):
        self.assertFalse(self.gate(records({("large", 3): {"watcher_degraded_after": True}})))
        self.assertFalse(self.gate(records({("medium", 7): {"watcher_witness_ok": False}})))
        self.assertFalse(self.gate(records({("medium", 19): {"watcher_witness": None}})))

    def test_a_witness_that_changed_fails_even_if_flagged_ok(self):
        # The last sample: health reads healthy again, but a degradation or a
        # new epoch happened since the baseline.
        for change in ({"degradations": 1}, {"epoch": 3}):
            witness = dict(HEALTHY, **change)
            self.assertFalse(self.gate(records({("large", 19): {"watcher_witness": witness}})), change)

    def test_a_series_without_a_native_baseline_fails(self):
        rows = [r for r in records() if not (r["type"] == "series_watcher" and r["size"] == "medium")]
        self.assertFalse(self.gate(rows))
        rows = records()
        rows[1]["witness"]["native"] = False
        self.assertFalse(self.gate(rows))

    def test_an_invalid_series_fails_the_gate(self):
        rows = records()
        rows.append({"type": "series_invalid", "size": "medium", "sample_index": 4,
                     "reason": "watcher not continuously healthy during the series"})
        self.assertFalse(self.gate(rows))


class Witness(unittest.TestCase):
    def test_only_an_equal_reading_of_a_healthy_native_baseline_stayed_healthy(self):
        self.assertTrue(RUNNER.stayed_healthy(HEALTHY, dict(HEALTHY)))
        for change in ({"degradations": 1}, {"epoch": 3}, {"degraded": True}, {"native": False}):
            self.assertFalse(RUNNER.stayed_healthy(HEALTHY, dict(HEALTHY, **change)), change)
        self.assertFalse(RUNNER.stayed_healthy(HEALTHY, None))
        not_native = dict(HEALTHY, native=False)
        self.assertFalse(RUNNER.stayed_healthy(not_native, dict(not_native)))


if __name__ == "__main__":
    unittest.main()
