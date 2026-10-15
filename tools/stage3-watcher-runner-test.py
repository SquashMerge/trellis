#!/usr/bin/env python3
"""Offline checks of the Stage 3 runner's summary gate and health witness.

Never launches Trellis: it imports the runner and feeds it records and logs.
"""
import importlib.util
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "stage3_runner", Path(__file__).with_name("stage3-watcher-runner.py"))
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


def records(overrides=None):
    overrides = overrides or {}
    """A complete, healthy local run (both sizes), with per-sample overrides."""
    out = [{"type": "run_header"}]
    for size in ("medium", "large"):
        out.append({"type": "series_watcher", "size": size, "watcher_epoch": 1, "native": True})
        for n in range(20):
            sample = {"type": "sample", "size": size, "sample_index": n + 1,
                      "path": RUNNER.planned_edit(size, n)[1], "valid": True,
                      "latency_seconds": 0.5, "mode_expected": "local",
                      "mode_note": RUNNER.MODE_NOTE, "mode_directly_observed": False,
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
        rows = records()
        del rows[5]["watcher_degraded_after"]
        self.assertFalse(self.gate(rows))
        rows = records()
        del rows[5]["watcher_witness_ok"]
        self.assertFalse(self.gate(rows))

    def test_a_degraded_or_unwitnessed_sample_fails_the_series(self):
        self.assertFalse(self.gate(records({("large", 3): {"watcher_degraded_after": True}})))
        self.assertFalse(self.gate(records({("medium", 7): {"watcher_witness_ok": False}})))

    def test_a_series_without_a_native_baseline_fails(self):
        rows = [r for r in records() if not (r["type"] == "series_watcher" and r["size"] == "medium")]
        self.assertFalse(self.gate(rows))
        rows = records()
        rows[1]["native"] = False
        self.assertFalse(self.gate(rows))

    def test_an_invalid_series_fails_the_gate(self):
        rows = records()
        rows.append({"type": "series_invalid", "size": "medium", "sample_index": 4,
                     "reason": "watcher not continuously healthy during the series"})
        self.assertFalse(self.gate(rows))


class Witness(unittest.TestCase):
    def marker(self, event, detail, lost=0):
        return (f"maintenance wall_ms=1 monotonic_us=1 incarnation=- event={event} "
                f"detail={detail} marker_lost={lost}\n")

    def test_a_brief_degraded_epoch_is_seen_even_if_health_recovers(self):
        with tempfile.TemporaryDirectory() as temp:
            log = Path(temp) / "daemon.log"
            log.write_text(self.marker("watcher_epoch", "epoch=1 native=true"))
            epoch, offset = RUNNER.healthy_epoch(log)
            self.assertEqual(epoch, 1)
            with log.open("a") as out:
                out.write(self.marker("publication_wait", "reason=None"))
                out.write(self.marker("watcher_degraded", "epoch=1"))
                out.write(self.marker("watcher_epoch", "epoch=2 native=true"))
            witness, _ = RUNNER.witness_since(log, offset)
            self.assertEqual(witness["degraded"], [1])
            self.assertEqual(witness["epochs"], [{"epoch": 2, "native": True}])

    def test_lost_markers_are_counted(self):
        with tempfile.TemporaryDirectory() as temp:
            log = Path(temp) / "daemon.log"
            log.write_text(self.marker("watcher_epoch", "epoch=1 native=true"))
            _, offset = RUNNER.witness_since(log, 0)
            with log.open("a") as out:
                out.write(self.marker("publication_wait", "reason=None", lost=3))
            witness, _ = RUNNER.witness_since(log, offset)
            self.assertEqual(witness["lost"], 3)

    def test_a_watcher_without_native_watching_is_not_a_healthy_baseline(self):
        with tempfile.TemporaryDirectory() as temp:
            log = Path(temp) / "daemon.log"
            log.write_text(self.marker("watcher_epoch", "epoch=1 native=false"))
            with self.assertRaises(RUNNER.Invalid):
                RUNNER.healthy_epoch(log)


if __name__ == "__main__":
    unittest.main()
