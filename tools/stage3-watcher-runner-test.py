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

    def test_an_interrupt_during_the_graceful_wait_still_kills_and_reaps(self):
        # A measurement signal raises Interrupted inside the daemon's graceful
        # wait; injected deterministically at that wait.
        self.mode("ignore-term")
        phase = self.phase()
        phase.start(listen_timeout_s=30)
        wait = phase.daemon.wait
        calls = []
        def interrupted_once(*args, **kwargs):
            calls.append(kwargs.get("timeout"))
            if len(calls) == 1:
                raise RUNNER.Interrupted("signal 15 interrupted measurement")
            return wait(*args, **kwargs)
        phase.daemon.wait = interrupted_once
        with self.assertRaises(RUNNER.Interrupted):
            phase.stop()
        self.assertEqual(phase.daemon.returncode, -9, "the TERM-ignoring daemon kept running")
        self.assertTrue(RUNNER.election_free(self.home))

    def test_an_interrupted_teardown_is_journaled_as_a_failure(self):
        self.mode("ignore-term")
        phase = self.phase()
        phase.start(listen_timeout_s=30)
        wait = phase.daemon.wait
        state = {"first": True}
        def interrupted_once(*args, **kwargs):
            if state.pop("first", False):
                raise RUNNER.Interrupted("signal 2 interrupted measurement")
            return wait(*args, **kwargs)
        phase.daemon.wait = interrupted_once
        events = self.temp / "events.jsonl"
        with events.open("x") as log:
            with self.assertRaises(RUNNER.Interrupted):
                RUNNER.close_out(log, events, phase, {})
        rows = [json.loads(line) for line in events.read_text().splitlines()]
        self.assertEqual([r["type"] for r in rows], ["failure", "summary"])
        self.assertFalse(rows[-1]["pass_gate"])
        self.assertIsNotNone(phase.daemon.returncode)

    def test_a_failed_close_still_closes_the_other_log_and_checks_the_election(self):
        phase = self.phase()
        phase.start(listen_timeout_s=30)
        real_close = phase.stderr.close
        def failing_close():
            real_close()
            raise OSError("close failed")
        phase.stderr.close = failing_close
        with self.assertRaises(OSError):
            phase.stop()
        self.assertTrue(phase.daemon_stderr.closed)
        self.assertIsNotNone(phase.daemon.returncode)
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


class StatusReads(unittest.TestCase):
    """Baseline setup retries a stalled or failing status; timed polling never
    does. A stand-in replaces the status command."""

    def setUp(self):
        self.calls = []
        saved = (RUNNER.command, RUNNER.SETUP_TIMEOUT_S)
        self.addCleanup(lambda: (setattr(RUNNER, "command", saved[0]),
                                 setattr(RUNNER, "SETUP_TIMEOUT_S", saved[1])))
        self.home = Path(tempfile.mkdtemp(prefix="s3status"))
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", "--", str(self.home)]))

    def fake(self, outcomes):
        # outcomes: "timeout" | "error" | a revision number, consumed in order;
        # the last one repeats.
        def command(argv, *, env=None, timeout=30):
            self.calls.append(timeout)
            outcome = outcomes[min(len(self.calls), len(outcomes)) - 1]
            if outcome == "timeout":
                raise subprocess.TimeoutExpired(argv, timeout)
            if outcome == "error":
                raise subprocess.CalledProcessError(1, argv, "", "Error: index_not_ready: reindex required")
            out = json.dumps({"revision": {"indexGeneration": "g", "indexRevision": outcome}})
            return subprocess.CompletedProcess(argv, 0, out, "")
        RUNNER.command = command

    def journal(self):
        path = self.home / "events.jsonl"
        log = path.open("a")
        self.addCleanup(log.close)
        return path, log

    def attempts_in(self, path):
        return [json.loads(line) for line in path.read_text().splitlines()
                if json.loads(line)["type"] == "setup_status_attempt"]

    def test_a_baseline_status_timeout_is_retried_within_setup(self):
        self.fake(["timeout", "error", 1])
        path, log = self.journal()
        pin, _, attempts = RUNNER.baseline_status(self.home, self.home, log, "medium")
        self.assertEqual(pin["indexRevision"], 1)
        self.assertEqual([a["outcome"].split(":")[0] for a in attempts], ["timeout", "exit 1", "ok"])
        journaled = self.attempts_in(path)
        self.assertEqual([(r["phase"], r["attempt"], r["outcome"].split(":")[0]) for r in journaled],
                         [("medium", 1, "timeout"), ("medium", 2, "exit 1"), ("medium", 3, "ok")])

    def test_baseline_setup_gives_up_at_its_deadline_with_every_attempt_journaled(self):
        RUNNER.SETUP_TIMEOUT_S = 0.5
        self.fake(["error"])
        path, log = self.journal()
        started = time.monotonic()
        with self.assertRaises(RUNNER.Invalid):
            RUNNER.baseline_status(self.home, self.home, log, "large")
        self.assertLess(time.monotonic() - started, 5)
        self.assertTrue(all(t <= 0.5 for t in self.calls), self.calls)
        journaled = self.attempts_in(path)
        self.assertEqual(len(journaled), len(self.calls), "an attempt was not journaled")
        self.assertEqual([r["attempt"] for r in journaled], list(range(1, len(self.calls) + 1)))

    def test_attempts_survive_a_failed_selection_after_a_successful_baseline(self):
        # The attempts are in the journal before baseline_status returns, so a
        # later selected_check failure cannot lose them.
        self.fake(["timeout", 1])
        path, log = self.journal()
        with self.assertRaises(RUNNER.Invalid):
            RUNNER.baseline_status(self.home, self.home, log, "fallback-medium")
            raise RUNNER.Invalid("selected native fact floor failed")
        self.assertEqual([r["outcome"] for r in self.attempts_in(path)], ["timeout", "ok"])

    def test_an_interrupted_attempt_is_journaled(self):
        def interrupted(argv, *, env=None, timeout=30):
            raise RUNNER.Interrupted("signal 2 interrupted measurement")
        RUNNER.command = interrupted
        path, log = self.journal()
        with self.assertRaises(RUNNER.Interrupted):
            RUNNER.baseline_status(self.home, self.home, log, "cold-oracle")
        [only] = self.attempts_in(path)
        self.assertEqual(only["phase"], "cold-oracle")
        self.assertTrue(only["outcome"].startswith("Interrupted"))

    def test_a_timed_sample_never_uses_the_setup_retry(self):
        for outcome in ("timeout", "error"):
            with self.subTest(outcome=outcome):
                self.calls.clear()
                RUNNER.SETUP_TIMEOUT_S = 1800
                self.fake([outcome])
                started = time.monotonic()
                with self.assertRaises(RUNNER.PollFailure) as raised:
                    RUNNER.wait_selected(self.home, self.home, {"indexGeneration": "g", "indexRevision": 1},
                                         time.monotonic_ns(), timeout_s=0.3)
                self.assertLess(time.monotonic() - started, 5, "a timed sample waited on the setup retry")
                self.assertTrue(all(t <= 0.3 for t in self.calls), self.calls)
                self.assertTrue(all("error" in p for p in raised.exception.polls))


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
