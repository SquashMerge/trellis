#!/usr/bin/env python3
"""Evaluate reported #61 samples offline; never collect or attest measurements.

Validation flags describe receipts from the real measurement runner. Setting
flags here does not verify hashes, admission, provenance, or canonical evidence.
A reviewed method supplies runCount; this tool does not ratify a run count.
"""

import argparse
import json
import math
from pathlib import Path


POLICY_DIR = Path(__file__).resolve().parents[2] / "docs" / "semantic-evidence"
REVIEW_FLAGS = ("methodReviewed", "dependencies90Merged", "featureMerged", "hostRatified")
RECEIPT_FLAGS = ("rawAdmissionValidated", "provenanceValidated", "canonicalEvidenceValidated")
STORAGE_FIELDS = ("steadySqliteBytes", "peakDatabaseWalShmBytes", "canonicalSerializedEvidenceBytes")


def positive(value, integer=False):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    try:
        return (not integer or isinstance(value, int)) and value > 0 and math.isfinite(value)
    except OverflowError:
        return False


def evaluate(report):
    """Qualify the computational record, not its authority as acceptance evidence."""
    policy = json.loads((POLICY_DIR / "benchmark-policy-v1.json").read_text())["performance"]
    cohorts = json.loads((POLICY_DIR / "cohorts-v1.json").read_text())
    result = {"reportVersion": 1, "offlineEvaluationOnly": True,
              "authoritativeAcceptance": False, "qualified": False,
              "status": "notMeasured", "passes": False, "faults": []}
    faults = result["faults"]
    if not isinstance(report, dict):
        faults.append("report must be an object")
        return result
    tier = report.get("tier")
    result["tier"] = tier if isinstance(tier, str) else None
    if type(report.get("reportVersion")) is not int or report["reportVersion"] != 1:
        faults.append("reportVersion must be 1")
    if tier not in ("small", "medium", "large"):
        faults.append("tier must be small, medium, or large")
        return result
    for flag in REVIEW_FLAGS + RECEIPT_FLAGS:
        if report.get(flag) is not True:
            faults.append(f"{flag} must be true; real runner/review receipts are required")
    run_count = report.get("runCount")
    samples = report.get("samples")
    if not positive(run_count, integer=True):
        faults.append("runCount must be a reviewed positive integer")
    if not isinstance(samples, list):
        faults.append("samples must be a list of raw runs")
        return result
    if len(samples) != run_count:
        faults.append("raw sample count must exactly match reviewed runCount")
    cohort = cohorts["synthetic"][tier]
    source_bytes = cohort.get("sourceBytes", cohort.get("sourceMiB", 0) * 1024**2)
    seen = set()
    for index, sample in enumerate(samples):
        prefix = f"sample[{index}]"
        if not isinstance(sample, dict):
            faults.append(f"{prefix}: must be an object")
            continue
        sample_id = sample.get("sampleId")
        if not isinstance(sample_id, str) or not sample_id.strip():
            faults.append(f"{prefix}: sampleId must be a nonempty string")
        elif sample_id in seen:
            faults.append(f"{prefix}: duplicate sampleId {sample_id}")
        else:
            seen.add(sample_id)
        for flag in ("coherentNativeSemantic", "requestedCoverageSatisfied"):
            if sample.get(flag) is not True:
                faults.append(f"{prefix}: {flag} must be true")
        for field in ("durationSeconds", "peakRssBytes", "files", "sourceBytes", "nativeFactsOverall"):
            if not positive(sample.get(field), integer=field != "durationSeconds"):
                faults.append(f"{prefix}: {field} must be finite and positive"
                              + (" integer" if field != "durationSeconds" else ""))
        if sample.get("files") != cohort["files"]:
            faults.append(f"{prefix}: files must match the canonical cohort")
        if sample.get("sourceBytes") != source_bytes:
            faults.append(f"{prefix}: sourceBytes must match the canonical cohort")
        facts = sample.get("nativeFactsPerLanguage")
        if not isinstance(facts, dict):
            faults.append(f"{prefix}: nativeFactsPerLanguage must be an object")
        else:
            for language in cohorts["languages"]:
                count = facts.get(language)
                if not positive(count, integer=True):
                    faults.append(f"{prefix}: {language} fact count must be a positive integer")
                elif cohort["factsScope"] == "perLanguage" and count < cohort["minimumNormalizedFacts"]:
                    faults.append(f"{prefix}: {language} fact count is below the canonical floor")
            if all(positive(facts.get(lang), integer=True) for lang in cohorts["languages"]):
                if sample.get("nativeFactsOverall") != sum(facts[lang] for lang in cohorts["languages"]):
                    faults.append(f"{prefix}: overall fact count must equal the language counts")
        if (cohort["factsScope"] == "overall" and positive(sample.get("nativeFactsOverall"), integer=True)
                and sample["nativeFactsOverall"] < cohort["minimumNormalizedFacts"]):
            faults.append(f"{prefix}: overall fact count is below the canonical floor")
        for field in STORAGE_FIELDS:
            if (tier != "small" or field in sample) and not positive(sample.get(field), integer=True):
                faults.append(f"{prefix}: {field} must be a positive integer")
        if "fault" in sample:
            faults.append(f"{prefix}: reported fault: {sample['fault']!r}")
    if faults:
        return result

    durations = sorted(sample["durationSeconds"] for sample in samples)
    middle = len(durations) // 2
    median = (durations[middle] if len(durations) % 2
              else durations[middle - 1] / 2 + durations[middle] / 2)
    full_index = policy["fullIndex"][tier]
    rss_limit = (full_index["rssMiB"] * 1024**2 if "rssMiB" in full_index
                 else full_index["rssGiB"] * 1024**3)
    metrics = {"medianSeconds": median, "maxPeakRssBytes": max(s["peakRssBytes"] for s in samples)}
    failures = []
    if median > full_index["medianSeconds"]:
        failures.append("median duration exceeds policy")
    if metrics["maxPeakRssBytes"] > rss_limit:
        failures.append("peak RSS exceeds policy")
    storage_samples = [s for s in samples if all(field in s for field in STORAGE_FIELDS)]
    if storage_samples:
        metrics.update({
            "maxSteadySqliteBytes": max(s["steadySqliteBytes"] for s in storage_samples),
            "maxSteadyToCanonicalRatio": max(s["steadySqliteBytes"] / s["canonicalSerializedEvidenceBytes"]
                                             for s in storage_samples),
            "maxPublicationPeakToSteadyRatio": max(s["peakDatabaseWalShmBytes"] / s["steadySqliteBytes"]
                                                   for s in storage_samples),
        })
    if tier != "small":
        storage = policy["storage"]
        for sample in samples:
            steady = sample["steadySqliteBytes"]
            if (steady > storage["steadyMaxGiB"] * 1024**3
                    or steady > sample["canonicalSerializedEvidenceBytes"]
                    * storage["steadyToCanonicalSerializedEvidenceMaxMultiplier"]
                    or sample["peakDatabaseWalShmBytes"] > steady
                    * storage["publicationPeakToSteadyMaxMultiplier"]):
                failures.append(f"{sample['sampleId']}: storage exceeds policy")
    result.update(qualified=True, status="failed" if failures else "pass", passes=not failures,
                  metrics=metrics, thresholdFailures=failures, runCount=run_count)
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True, help="fresh output JSON path")
    args = parser.parse_args(argv)
    if args.out.exists():
        parser.error("--out must be fresh; existing files are never overwritten")
    try:
        report = json.loads(args.report.read_text())
        result = evaluate(report)
    except (OSError, ValueError) as error:
        result = {"reportVersion": 1, "offlineEvaluationOnly": True,
                  "authoritativeAcceptance": False, "qualified": False,
                  "status": "notMeasured", "passes": False, "faults": [str(error)]}
    encoded = json.dumps(result, indent=2, allow_nan=False) + "\n"
    with args.out.open("x") as output:
        output.write(encoded)
    print(encoded, end="")
    return 0 if result["passes"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
