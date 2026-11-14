#!/usr/bin/env python3
"""Offline checks of tools/rules-ratchet.py's comparison and record (#1008)."""
import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location(
    "rules_ratchet", Path(__file__).with_name("rules-ratchet.py"))
RR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RR)

RECORD = {"deviatingPairs": 3, "deviatingReferences": 10, "comparablePairs": 40,
          "conformingPairs": 30, "unknownPairs": 7, "partialFiles": 1}


def row(status, references, excepted=False):
    return {"status": status, "referenceCount": references, "excepted": excepted}


class Ratchet(unittest.TestCase):
    def verdict(self, **changed):
        return RR.compare(RECORD, {**RECORD, **changed})

    def test_the_same_figures_pass(self):
        verdict, lines = self.verdict()
        self.assertEqual(verdict, "equal")
        self.assertIn("deviatingPairs 3", lines[0])

    def test_an_added_pair_or_reference_fails(self):
        for key in ("deviatingPairs", "deviatingReferences"):
            verdict, lines = self.verdict(**{key: RECORD[key] + 1})
            self.assertEqual(verdict, "fail", key)
            self.assertIn("deviations rose", lines[0])
            self.assertIn(f"{key}: recorded {RECORD[key]}, measured {RECORD[key] + 1}", lines[1])

    def test_fewer_deviations_on_lost_coverage_fail(self):
        for key, value in (("comparablePairs", 39), ("unknownPairs", 8), ("partialFiles", 2)):
            verdict, lines = self.verdict(deviatingPairs=2, deviatingReferences=5, **{key: value})
            self.assertEqual(verdict, "fail", key)
            self.assertIn("lost coverage", lines[0])

    def test_fewer_deviations_on_no_worse_coverage_are_only_a_notice(self):
        verdict, lines = self.verdict(deviatingPairs=2, deviatingReferences=5,
                                      conformingPairs=31, comparablePairs=41, unknownPairs=7)
        self.assertEqual(verdict, "lower")
        self.assertIn("NOTICE", lines[0])
        self.assertIn(RR.UPDATE, lines[-1])

    def test_a_payload_is_summarized(self):
        payload = {
            "counts": {"comparable": 5, "conforms": 2, "deviates": 2, "unknown": 1, "excepted": 0},
            "instances": [row("deviates", 4), row("deviates", 6), row("conforms", 9),
                          row("conforms", 1), row("unknown", 2)],
            "partialFiles": [{"path": "a.rs"}],
        }
        self.assertEqual(RR.summarize(payload), {
            "deviatingPairs": 2, "deviatingReferences": 10, "comparablePairs": 5,
            "conformingPairs": 2, "unknownPairs": 1, "partialFiles": 1})

    def test_the_record_round_trips_byte_for_byte(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "record.json"
            RR.write_record(path, RECORD)
            first = path.read_bytes()
            self.assertEqual({key: RR.read_record(path)[key] for key in RR.KEYS}, RECORD)
            RR.write_record(path, RR.read_record(path))
            self.assertEqual(path.read_bytes(), first)

    def test_the_checked_in_record_is_well_formed(self):
        record = RR.read_record(RR.RECORD)
        self.assertEqual(RR.compare(record, record)[0], "equal")


def done(code, stdout="", stderr=""):
    return subprocess.CompletedProcess([], code, stdout, stderr)


class Stub:
    """A trellis whose commands return the queued results."""

    def __init__(self, *results):
        self.results, self.calls = list(results), []

    def run(self, *args, timeout=None):
        self.calls.append(args)
        return self.results.pop(0) if len(self.results) > 1 else self.results[0]


class DeadlineRetry(unittest.TestCase):
    DEADLINE = done(1, stderr="trellis: waiting\nError: deadline_exceeded: Architecture is being computed\n")

    def test_a_deadline_is_asked_again_until_the_reply(self):
        stub = Stub(self.DEADLINE, self.DEADLINE, done(1, '{"payload": {}}', "Error: architecture deviations\n"))
        self.assertEqual(RR.rules_check(stub), {"payload": {}})
        self.assertEqual(len(stub.calls), 3)

    def test_another_error_that_mentions_a_deadline_is_not_asked_again(self):
        for other in (done(1, stderr="Error: invalid_rules: layer deadline_exceeded is unknown\n"),
                      done(2, stderr="Error: deadline_exceeded: not the CLI's failure exit\n")):
            stub = Stub(other)
            with self.assertRaisesRegex(RR.Failed, "rules check"):
                RR.rules_check(stub)
            self.assertEqual(len(stub.calls), 1)

    def test_deadlines_are_bounded(self):
        stub = Stub(self.DEADLINE)
        with self.assertRaisesRegex(RR.Failed, "deadline_exceeded"):
            RR.rules_check(stub)
        self.assertEqual(len(stub.calls), RR.RULES_ATTEMPTS)


class ShutDown(unittest.TestCase):
    def setUp(self):
        self.home = Path(tempfile.mkdtemp())
        self.daemon = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(600)"])
        self.addCleanup(self.daemon.wait)
        self.addCleanup(self.daemon.kill)

    def test_a_failed_stop_is_reported_and_the_daemon_is_ended_by_pid_before_its_home_goes(self):
        seen = []

        class Watched(Stub):
            def run(inner, *args, timeout=None):
                seen.append((self.daemon.poll() is None, self.home.exists()))
                return super().run(*args, timeout=timeout)

        problems = RR.shut_down(Watched(done(1, stderr="Error: daemon_unavailable\n")), self.daemon, self.home)
        self.assertEqual(seen, [(True, True)])
        self.assertIn("daemon stop failed (exit 1)", problems[0])
        self.assertIn(f"daemon {self.daemon.pid} was still running; sent SIGTERM", problems[1])
        self.assertIsNotNone(self.daemon.poll())
        self.assertFalse(self.home.exists())

    def test_a_stopped_daemon_leaves_no_problem(self):
        class Stops(Stub):
            def run(inner, *args, timeout=None):
                self.daemon.terminate()
                return super().run(*args, timeout=timeout)

        self.assertEqual(RR.shut_down(Stops(done(0, '{"stopped": true}')), self.daemon, self.home), [])
        self.assertEqual(self.daemon.poll() is None, False)
        self.assertFalse(self.home.exists())

    def test_without_a_daemon_only_the_home_goes(self):
        self.assertEqual(RR.shut_down(None, None, self.home), [])
        self.assertFalse(self.home.exists())


class Build(unittest.TestCase):
    def test_the_build_writes_only_its_own_target_directory(self):
        calls = []
        original = RR.run
        RR.run = lambda argv, **options: calls.append((argv, options)) or done(0)
        self.addCleanup(setattr, RR, "run", original)
        binary = RR.build(Path("/checkout/target"))
        self.assertEqual(binary, Path("/checkout/target-ratchet/debug/trellis"))
        self.assertEqual(calls[0][0], ["cargo", "build", "--locked", "--bin", "trellis"])
        self.assertEqual(calls[0][1]["env"]["CARGO_TARGET_DIR"], "/checkout/target-ratchet")


if __name__ == "__main__":
    unittest.main()
