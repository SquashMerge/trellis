#!/usr/bin/env python3
"""Offline checks of the Stage 3 runner's summary gate and health witness.

Never launches Trellis: it imports the runner and feeds it records, or a
stand-in daemon that holds a private HOME's daemon lock.
"""
import copy
import importlib.util
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
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


FAKE_TRELLIS = """#!/usr/bin/env python3
import fcntl, os, signal, socket, sys, time
mode = os.environ.get("FAKE_MODE", "")
if sys.argv[1] == "daemon":
    if mode == "exit-early":
        sys.exit(3)
    if mode == "ignore-term":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    run = os.path.join(os.environ["HOME"], "run")
    os.makedirs(run, exist_ok=True)
    lock = os.open(os.path.join(run, "daemon.lock"), os.O_RDWR | os.O_CREAT, 0o600)
    fcntl.flock(lock, fcntl.LOCK_EX)
    if mode != "never-listen":
        path = os.path.join(run, "daemon.sock")
        if os.path.exists(path):
            os.unlink(path)  # the elected daemon replaces a stale socket
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(path)
        listener.listen()
while True:
    time.sleep(1)
"""


class PhaseProcesses(unittest.TestCase):
    """Daemon start/stop against a stand-in trellis binary."""

    def setUp(self):
        self.temp = Path(tempfile.mkdtemp(prefix="s3", dir="/tmp"))
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", "--", str(self.temp)]))
        fake = self.temp / "trellis"
        fake.write_text(FAKE_TRELLIS)
        fake.chmod(0o755)
        saved = RUNNER.BINARY
        RUNNER.BINARY = fake
        self.addCleanup(lambda: setattr(RUNNER, "BINARY", saved))
        self.home = self.temp / "home"
        self.home.mkdir()
        self.mode("")

    def mode(self, mode):
        saved = os.environ.get("FAKE_MODE")
        os.environ["FAKE_MODE"] = mode
        self.addCleanup(lambda: os.environ.__setitem__("FAKE_MODE", saved) if saved is not None
                        else os.environ.pop("FAKE_MODE", None))

    def phase(self):
        phase = RUNNER.Daemon(self.temp, "t", self.temp, self.home)
        phase.GRACE_S = 0.5
        self.addCleanup(lambda: [p.kill() for p in (phase.proc, phase.daemon) if p and p.poll() is None])
        return phase

    def test_a_phase_stops_its_own_daemon_and_serve(self):
        phase = self.phase()
        phase.start(listen_timeout_s=30)
        self.assertFalse(RUNNER.election_free(self.home))
        phase.stop()
        self.assertIsNotNone(phase.daemon.returncode)
        self.assertIsNotNone(phase.proc.returncode)
        self.assertTrue(RUNNER.election_free(self.home))
        phase.stop()  # idempotent

    def test_a_daemon_ignoring_sigterm_is_killed(self):
        self.mode("ignore-term")
        phase = self.phase()
        phase.start(listen_timeout_s=30)
        phase.stop()
        self.assertEqual(phase.daemon.returncode, -9)
        self.assertTrue(RUNNER.election_free(self.home))

    def test_a_failed_start_is_still_stopped(self):
        for mode in ("exit-early", "never-listen"):
            with self.subTest(mode=mode):
                self.mode(mode)
                phase = self.phase()
                with self.assertRaises(RUNNER.Invalid):
                    phase.start(listen_timeout_s=1)
                self.assertIsNone(phase.proc, "serve started beside a daemon that never listened")
                phase.stop()
                self.assertIsNotNone(phase.daemon.returncode)
                self.assertTrue(phase.daemon_stderr.closed)

    def test_an_unavailable_lsof_fails_closed(self):
        real = RUNNER.subprocess.run
        def broken(argv, *args, **kwargs):
            if argv[0] == "lsof":
                raise subprocess.TimeoutExpired(argv, 10)
            return real(argv, *args, **kwargs)
        RUNNER.subprocess.run = broken
        self.addCleanup(lambda: setattr(RUNNER.subprocess, "run", real))
        phase = self.phase()
        with self.assertRaises(RUNNER.Invalid):
            phase.start(listen_timeout_s=1)
        self.assertIsNone(phase.proc)
        phase.stop()

    def test_a_daemon_this_phase_did_not_start_fails_the_stop_and_is_left_alone(self):
        phase = self.phase()
        phase.start(listen_timeout_s=30)
        phase.stop()
        stray = subprocess.Popen([str(RUNNER.BINARY), "daemon"], env=dict(os.environ, HOME=str(self.home)),
                                 start_new_session=True)
        self.addCleanup(lambda: stray.poll() is None and (stray.kill(), stray.wait()))
        deadline = time.monotonic() + 30
        while RUNNER.election_free(self.home):
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.05)
        with self.assertRaises(RUNNER.Invalid):
            phase.stop()
        self.assertIsNone(stray.poll(), "stop signalled a process it did not start")


class CloseOut(unittest.TestCase):
    def test_a_failed_teardown_still_restores_handlers_and_summarizes(self):
        class Failing:
            def stop(self):
                raise RUNNER.Invalid("teardown failed")
        with tempfile.TemporaryDirectory() as temp:
            events = Path(temp) / "events.jsonl"
            previous = signal.getsignal(signal.SIGUSR1)
            signal.signal(signal.SIGUSR1, lambda *_: None)
            with events.open("x") as log:
                with self.assertRaises(RUNNER.Invalid):
                    RUNNER.close_out(log, events, Failing(), {signal.SIGUSR1: previous})
            self.assertEqual(signal.getsignal(signal.SIGUSR1), previous)
            rows = [json.loads(line) for line in events.read_text().splitlines()]
            self.assertEqual([r["type"] for r in rows], ["failure", "summary"])
            self.assertFalse(rows[-1]["pass_gate"])


if __name__ == "__main__":
    unittest.main()
