"""Offline report qualification: synthetic report dicts, no generation or measurement."""

import copy
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "qualify-performance.py"
SPEC = importlib.util.spec_from_file_location("performance_qualification", SCRIPT)
Q = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(Q)
POLICY = json.loads((Q.POLICY_DIR / "benchmark-policy-v1.json").read_text())
COHORTS = json.loads((Q.POLICY_DIR / "cohorts-v1.json").read_text())
ALL_FLAGS = dict.fromkeys(("methodRatified", "workerRssMethodApproved",
                           "sqlitePeakMethodReviewed", "denominatorRatified"), True)


def repetition(ordinal, wall=1.0, rss=1024, steady=100, peak=150, denominator=50):
    return {"ordinal": ordinal, "status": "completed",
            "interval": {"wallSeconds": wall},
            "rss": {"workerMaxRssBytes": rss},
            "storage": {"steady": {"total": steady}, "peak": {"total": peak},
                        "denominator": {"status": "ratified", "bytes": denominator}}}


def report(sizes=("small", "medium", "large"), reference=True, smoke=False, **kwargs):
    return {"formatVersion": 1, "kind": "trellis.combined-gate-report", "smoke": smoke,
            "campaign": {"host": {"reference": reference}},
            "sizes": [{"size": size, "repetitions": [repetition(i, **kwargs) for i in (1, 2, 3)]}
                      for size in sizes]}


def qualify(rep, flags=None):
    return Q.qualify(rep, POLICY, COHORTS, flags)


def evaluation(result, size, criterion):
    entry = next(e for e in result["sizes"] if e["size"] == size)
    return entry["evaluations"][criterion]


class QualificationTests(unittest.TestCase):
    def test_not_measured_without_reference_host(self):
        result = qualify(report(reference=False), ALL_FLAGS)
        self.assertEqual(result["status"], "notMeasured")
        self.assertFalse(result["qualification"]["referenceHost"])
        self.assertEqual(evaluation(result, "medium", "wall")["status"], "notMeasured")

    def test_storage_not_measured_without_denominator(self):
        result = qualify(report())
        self.assertEqual(result["status"], "notMeasured")
        self.assertEqual(evaluation(result, "medium", "steadyRatio")["status"], "notMeasured")
        self.assertEqual(evaluation(result, "medium", "peakRatio")["status"], "notMeasured")
        self.assertEqual(evaluation(result, "medium", "steadyAbsolute")["status"], "passed")
        missing = report()
        for entry in missing["sizes"]:
            for rep in entry["repetitions"]:
                rep["storage"]["denominator"] = {"status": "notMeasured", "bytes": None}
        flags = {"denominatorRatified": True, "sqlitePeakMethodReviewed": True}
        self.assertEqual(evaluation(qualify(missing, flags), "large", "steadyRatio")["status"], "notMeasured")

    def test_never_authoritative(self):
        for rep, flags in ((report(), None), (report(), ALL_FLAGS), (report(wall=10**6), ALL_FLAGS),
                           (report(smoke=True), None)):
            result = qualify(rep, flags)
            self.assertIs(result["authoritativeAcceptance"], False)
            self.assertEqual(result["issue11"], "open")

    def test_passed_is_unreachable_with_constant_flags(self):
        self.assertNotEqual(qualify(report())["status"], "passed")
        self.assertEqual(qualify(report(), ALL_FLAGS)["status"], "passed")

    def test_smoke_is_not_measured(self):
        result = qualify(report(smoke=True), ALL_FLAGS)
        self.assertEqual(result["status"], "notMeasured")
        self.assertIn("smoke", result["statusReasons"])
        self.assertTrue(result["qualification"]["smoke"])

    def test_missing_repetition_is_not_measured(self):
        rep = report(sizes=("medium",))
        rep["sizes"][0]["repetitions"][2] = {"ordinal": 3, "status": "notAttempted"}
        result = qualify(rep, ALL_FLAGS)
        self.assertEqual(result["status"], "notMeasured")
        self.assertEqual(evaluation(result, "medium", "wall")["status"], "notMeasured")
        self.assertIsNone(result["sizes"][0]["median"]["wallSeconds"])

    def test_missing_instrumentation_is_not_measured(self):
        rep = report(sizes=("medium",))
        rep["sizes"][0]["repetitions"][1]["rss"]["workerMaxRssBytes"] = None
        result = qualify(rep, ALL_FLAGS)
        self.assertEqual(evaluation(result, "medium", "rss")["status"], "notMeasured")
        self.assertEqual(evaluation(result, "medium", "wall")["status"], "passed")

    def test_wall_uses_median_of_raw_samples(self):
        limit = POLICY["performance"]["fullIndex"]["medium"]["medianSeconds"]
        rep = report(sizes=("medium",))
        for rec, wall in zip(rep["sizes"][0]["repetitions"], (1, limit, limit * 10)):
            rec["interval"]["wallSeconds"] = wall
        result = qualify(rep, ALL_FLAGS)
        self.assertEqual(result["sizes"][0]["median"]["wallSeconds"], limit)
        self.assertEqual(evaluation(result, "medium", "wall")["status"], "passed")
        rep["sizes"][0]["repetitions"][0]["interval"]["wallSeconds"] = limit + 1
        result = qualify(rep, ALL_FLAGS)
        self.assertEqual(evaluation(result, "medium", "wall")["status"], "failed")

    def test_rss_requires_every_sample_within_limit(self):
        limit = POLICY["performance"]["fullIndex"]["small"]["rssMiB"] * 1024**2
        rep = report(sizes=("small",))
        rep["sizes"][0]["repetitions"][2]["rss"]["workerMaxRssBytes"] = limit + 1
        result = qualify(rep, ALL_FLAGS)
        self.assertEqual(evaluation(result, "small", "rss")["status"], "failed")
        self.assertEqual(result["sizes"][0]["peakWorkerRssBytes"], limit + 1)
        rep["sizes"][0]["repetitions"][2]["rss"]["workerMaxRssBytes"] = limit
        self.assertEqual(evaluation(qualify(rep, ALL_FLAGS), "small", "rss")["status"], "passed")

    def test_storage_rows_apply_to_medium_and_large_only(self):
        result = qualify(report(steady=10**15, peak=10**18), ALL_FLAGS)
        for criterion in ("steadyAbsolute", "steadyRatio", "peakRatio"):
            self.assertEqual(evaluation(result, "small", criterion)["status"], "notApplicable")
            self.assertEqual(evaluation(result, "medium", criterion)["status"], "failed")

    def test_storage_limits_come_from_policy(self):
        storage = POLICY["performance"]["storage"]
        ratio = storage["steadyToCanonicalSerializedEvidenceMaxMultiplier"]
        peak = storage["publicationPeakToSteadyMaxMultiplier"]
        at_limit = report(sizes=("medium",), steady=100 * ratio, denominator=100, peak=100 * ratio * peak)
        result = qualify(at_limit, ALL_FLAGS)
        self.assertEqual(result["status"], "passed")
        over = report(sizes=("medium",), steady=100 * ratio + 1, denominator=100, peak=100 * ratio)
        self.assertEqual(evaluation(qualify(over, ALL_FLAGS), "medium", "steadyRatio")["status"], "failed")
        over = report(sizes=("medium",), steady=100, denominator=100, peak=100 * peak + 1)
        self.assertEqual(evaluation(qualify(over, ALL_FLAGS), "medium", "peakRatio")["status"], "failed")
        gib = report(sizes=("large",), steady=storage["steadyMaxGiB"] * 1024**3 + 1,
                     denominator=10**12, peak=1)
        self.assertEqual(evaluation(qualify(gib, ALL_FLAGS), "large", "steadyAbsolute")["status"], "failed")

    def test_threshold_failure_is_failed_and_leaves_issue_11_open(self):
        result = qualify(report(wall=10**6), ALL_FLAGS)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["issue11"], "open")
        self.assertTrue(result["statusReasons"])

    def test_fault_is_unqualified_and_outranks_failed(self):
        rep = report(wall=10**6)
        rep["sizes"][0]["repetitions"][1] = {"ordinal": 2, "status": "fault"}
        rep["sizes"][0]["repetitions"][2] = {"ordinal": 3, "status": "notAttempted"}
        result = qualify(rep, ALL_FLAGS)
        self.assertEqual(result["status"], "unqualified")
        self.assertIs(result["authoritativeAcceptance"], False)
        self.assertEqual(result["issue11"], "open")

    def test_failed_outranks_not_measured(self):
        result = qualify(report(wall=10**6))
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["qualification"]["denominatorRatified"], False)

    def test_input_is_not_mutated_and_output_is_deterministic(self):
        rep = report()
        original = copy.deepcopy(rep)
        first = qualify(rep)
        self.assertEqual(rep, original)
        self.assertEqual(json.dumps(first, sort_keys=True), json.dumps(qualify(rep), sort_keys=True))

    def test_cli_reads_report_and_prints_qualified_json(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "report.json"
            path.write_text(json.dumps(report()))
            done = subprocess.run([sys.executable, "-I", str(SCRIPT), "--report", str(path)],
                                  capture_output=True, text=True)
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual(json.loads(done.stdout)["status"], "notMeasured")
            path.write_text("[]")
            done = subprocess.run([sys.executable, "-I", str(SCRIPT), "--report", str(path)],
                                  capture_output=True, text=True)
            self.assertEqual(done.returncode, 2)


if __name__ == "__main__":
    unittest.main()
