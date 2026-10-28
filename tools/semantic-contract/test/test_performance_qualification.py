"""Offline report qualification only; fixture flags are not acceptance receipts."""

import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "qualify-performance.py"
SPEC = importlib.util.spec_from_file_location("performance_qualification", SCRIPT)
QUALIFIER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(QUALIFIER)
COHORTS = json.loads((QUALIFIER.POLICY_DIR / "cohorts-v1.json").read_text())
POLICY = json.loads((QUALIFIER.POLICY_DIR / "benchmark-policy-v1.json").read_text())["performance"]


def report_for(tier="medium", runs=3):
    cohort = COHORTS["synthetic"][tier]
    floor = cohort["minimumNormalizedFacts"]
    per_language = floor if cohort["factsScope"] == "perLanguage" else floor // len(COHORTS["languages"])
    facts = dict.fromkeys(COHORTS["languages"], per_language)
    samples = [{"sampleId": f"run-{index}", "durationSeconds": 1,
                "peakRssBytes": 1024, "files": cohort["files"],
                "sourceBytes": cohort.get("sourceBytes", cohort.get("sourceMiB", 0) * 1024**2),
                "nativeFactsPerLanguage": facts.copy(), "nativeFactsOverall": sum(facts.values()),
                "steadySqliteBytes": 100, "peakDatabaseWalShmBytes": 150,
                "canonicalSerializedEvidenceBytes": 50,
                "coherentNativeSemantic": True, "requestedCoverageSatisfied": True}
               for index in range(runs)]
    return {"reportVersion": 1, "tier": tier, "runCount": runs, "samples": samples,
            **dict.fromkeys(QUALIFIER.REVIEW_FLAGS + QUALIFIER.RECEIPT_FLAGS, True)}


class PerformanceQualificationTests(unittest.TestCase):
    def assert_not_measured(self, report):
        result = QUALIFIER.evaluate(report)
        self.assertFalse(result["qualified"])
        self.assertFalse(result["passes"])
        self.assertEqual(result["status"], "notMeasured")
        self.assertTrue(result["faults"])
        self.assertNotIn("metrics", result)
        return result

    def test_qualified_records_report_median_maxima_and_no_authority(self):
        for tier in ("small", "medium", "large"):
            with self.subTest(tier=tier):
                report = report_for(tier)
                for sample, duration, rss in zip(report["samples"], (1, 1.5, 2), (10, 30, 20)):
                    sample.update(durationSeconds=duration, peakRssBytes=rss)
                original = copy.deepcopy(report)
                result = QUALIFIER.evaluate(report)
                self.assertTrue(result["qualified"])
                self.assertTrue(result["passes"])
                self.assertEqual(result["status"], "pass")
                self.assertEqual(result["metrics"]["medianSeconds"], 1.5)
                self.assertEqual(result["metrics"]["maxPeakRssBytes"], 30)
                self.assertEqual(result["metrics"]["maxSteadyToCanonicalRatio"], 2)
                self.assertTrue(result["offlineEvaluationOnly"])
                self.assertFalse(result["authoritativeAcceptance"])
                self.assertEqual(original, report)

    def test_every_review_and_runner_receipt_is_required(self):
        for field in QUALIFIER.REVIEW_FLAGS + QUALIFIER.RECEIPT_FLAGS:
            for invalid in (None, False, 1):
                with self.subTest(field=field, invalid=invalid):
                    report = report_for()
                    report[field] = invalid
                    self.assert_not_measured(report)

    def test_exact_reviewed_raw_count_and_unique_ids_required(self):
        for count in (None, 0, True, 2, 4, 3.0):
            report = report_for()
            report["runCount"] = count
            self.assert_not_measured(report)
        for sample_id in (None, "", " ", "run-0"):
            report = report_for()
            report["samples"][1]["sampleId"] = sample_id
            self.assert_not_measured(report)
        self.assertTrue(QUALIFIER.evaluate(report_for(runs=2))["passes"])

    def test_no_partial_median_or_fault_rescue(self):
        for flag in ("coherentNativeSemantic", "requestedCoverageSatisfied"):
            report = report_for()
            report["samples"][1][flag] = False
            self.assert_not_measured(report)
        report = report_for()
        report["samples"][1]["fault"] = {"stage": "publication", "error": "storage_busy"}
        result = self.assert_not_measured(report)
        self.assertTrue(any("storage_busy" in fault for fault in result["faults"]))

    def test_finite_positive_raw_metrics_and_integral_counts(self):
        for field in ("durationSeconds", "peakRssBytes", "files", "sourceBytes", "nativeFactsOverall",
                      *QUALIFIER.STORAGE_FIELDS):
            for invalid in (None, True, 0, -1, float("nan"), float("inf"), "1"):
                with self.subTest(field=field, invalid=invalid):
                    report = report_for()
                    report["samples"][1][field] = invalid
                    self.assert_not_measured(report)
            if field != "durationSeconds":
                report = report_for()
                report["samples"][1][field] = 1.5
                self.assert_not_measured(report)

    def test_canonical_cohort_sizes_and_fact_floors(self):
        for tier in ("small", "medium", "large"):
            for field in ("files", "sourceBytes"):
                for delta in (-1, 1):
                    report = report_for(tier)
                    report["samples"][1][field] += delta
                    self.assert_not_measured(report)
            report = report_for(tier)
            sample = report["samples"][1]
            for language in sample["nativeFactsPerLanguage"]:
                sample["nativeFactsPerLanguage"][language] -= 1
            sample["nativeFactsOverall"] = sum(sample["nativeFactsPerLanguage"].values())
            self.assert_not_measured(report)
        report = report_for()
        report["samples"][1]["nativeFactsPerLanguage"]["java"] = True
        self.assert_not_measured(report)

    def test_thresholds_are_policy_limits_and_storage_is_per_sample(self):
        report = report_for()
        limits = POLICY["fullIndex"]["medium"]
        for sample in report["samples"]:
            sample.update(durationSeconds=limits["medianSeconds"], peakRssBytes=limits["rssGiB"] * 1024**3,
                          steadySqliteBytes=300, canonicalSerializedEvidenceBytes=100, peakDatabaseWalShmBytes=600)
        self.assertTrue(QUALIFIER.evaluate(report)["passes"])
        for field, value, all_samples in (("durationSeconds", limits["medianSeconds"] + 0.01, True),
                                         ("peakRssBytes", limits["rssGiB"] * 1024**3 + 1, False),
                                         ("steadySqliteBytes", 301, False),
                                         ("peakDatabaseWalShmBytes", 601, False),
                                         ("steadySqliteBytes", POLICY["storage"]["steadyMaxGiB"] * 1024**3 + 1, False)):
            changed = copy.deepcopy(report)
            for sample in changed["samples"] if all_samples else changed["samples"][:1]:
                sample[field] = value
            result = QUALIFIER.evaluate(changed)
            self.assertTrue(result["qualified"])
            self.assertFalse(result["passes"])
            self.assertEqual(result["status"], "failed")
        for tier in ("medium", "large"):
            for field in QUALIFIER.STORAGE_FIELDS:
                missing = report_for(tier)
                del missing["samples"][0][field]
                self.assert_not_measured(missing)
        small = report_for("small")
        for sample in small["samples"]:
            for field in QUALIFIER.STORAGE_FIELDS:
                del sample[field]
        self.assertTrue(QUALIFIER.evaluate(small)["passes"])
        small["samples"][0].update(steadySqliteBytes=10**10, canonicalSerializedEvidenceBytes=1,
                                   peakDatabaseWalShmBytes=10**12)
        self.assertTrue(QUALIFIER.evaluate(small)["passes"])

    def test_cli_writes_fresh_output_leaves_original_and_rejects_invalid_json(self):
        with tempfile.TemporaryDirectory() as directory, contextlib.redirect_stdout(io.StringIO()):
            source = Path(directory) / "input.json"
            output = Path(directory) / "output.json"
            raw = json.dumps(report_for())
            source.write_text(raw)
            self.assertEqual(QUALIFIER.main(["--report", str(source), "--out", str(output)]), 0)
            self.assertTrue(json.loads(output.read_text())["passes"])
            self.assertEqual(source.read_text(), raw)
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                QUALIFIER.main(["--report", str(source), "--out", str(output)])
            bad_source = Path(directory) / "bad.json"
            bad_source.write_text("{")
            bad_output = Path(directory) / "bad-output.json"
            self.assertEqual(QUALIFIER.main(["--report", str(bad_source), "--out", str(bad_output)]), 1)
            self.assertEqual(json.loads(bad_output.read_text())["status"], "notMeasured")


if __name__ == "__main__":
    unittest.main()
