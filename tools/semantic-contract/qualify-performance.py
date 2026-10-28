#!/usr/bin/env python3
"""Qualify a #61 combined-gate report offline; never collect or attest measurements.

Thresholds are read from the pinned policy and cohort files and are never
changed here. The result is never authoritative acceptance and leaves #11 open.
Status precedence is unqualified > failed > notMeasured > passed; `passed` is
unreachable with the constant qualification flags below.
"""

import argparse
import copy
import json
import math
import sys
from pathlib import Path


POLICY_DIR = Path(__file__).resolve().parents[2] / "docs" / "semantic-evidence"
REPETITIONS = 3
CRITERIA = ("wall", "rss", "steadyAbsolute", "steadyRatio", "peakRatio")
STORAGE_SIZES = ("medium", "large")
FLAGS = {"methodRatified": True, "workerRssMethodApproved": True,
         "sqlitePeakMethodReviewed": False, "denominatorRatified": False}
NEEDS = {"wall": "methodRatified", "rss": "workerRssMethodApproved",
         "steadyAbsolute": "methodRatified", "steadyRatio": "denominatorRatified",
         "peakRatio": "sqlitePeakMethodReviewed"}


def number(value):
    return (not isinstance(value, bool) and isinstance(value, (int, float))
            and math.isfinite(value) and value >= 0)


def dig(value, *keys):
    for key in keys:
        value = value.get(key) if isinstance(value, dict) else None
    return value


def median(values):
    ordered = sorted(values)
    middle = len(ordered) // 2
    return ordered[middle] if len(ordered) % 2 else ordered[middle - 1] / 2 + ordered[middle] / 2


def limits(policy, size):
    performance = policy["performance"]
    full = performance["fullIndex"][size]
    rss = full["rssMiB"] * 1024**2 if "rssMiB" in full else full["rssGiB"] * 1024**3
    storage = performance["storage"]
    return {"wallSeconds": full["medianSeconds"], "rssBytes": rss,
            "steadyBytes": storage["steadyMaxGiB"] * 1024**3,
            "steadyToCanonicalMultiplier": storage["steadyToCanonicalSerializedEvidenceMaxMultiplier"],
            "peakToSteadyMultiplier": storage["publicationPeakToSteadyMaxMultiplier"]}


def verdict(status, numerator=None, denominator=None, limit=None, reason=None):
    result = {"status": status, "numerator": numerator, "denominator": denominator, "limit": limit}
    if reason:
        result["reason"] = reason
    return result


def judge(samples, limit, reason_if_missing, ratio=False):
    """samples are (numerator, denominator) pairs; every one must be within the limit."""
    if any(sample is None for sample in samples):
        return verdict("notMeasured", limit=limit, reason=reason_if_missing)
    worst = max(samples, key=lambda pair: pair[0] / pair[1] if ratio else pair[0])
    status = "passed" if all((n / d if ratio else n) <= limit for n, d in samples) else "failed"
    return verdict(status, worst[0], worst[1] if ratio else None, limit)


def evaluate_size(entry, size, limit, flags, host_ok, smoke):
    reps = entry.get("repetitions")
    reps = reps if isinstance(reps, list) else []
    completed = [rep for rep in reps if isinstance(rep, dict) and rep.get("status") == "completed"]
    complete = len(reps) == REPETITIONS and len(completed) == REPETITIONS
    evaluations = {}
    storage_applies = size in STORAGE_SIZES

    def storage(rep, *keys):
        return dig(rep, "storage", *keys)

    def pairs(getter):
        out = []
        for rep in completed:
            pair = getter(rep)
            out.append(pair if pair is not None and all(number(v) for v in pair) else None)
        return out

    for criterion in CRITERIA:
        if criterion in ("steadyAbsolute", "steadyRatio", "peakRatio") and not storage_applies:
            evaluations[criterion] = verdict("notApplicable", reason=f"storage rows apply to {'/'.join(STORAGE_SIZES)} only")
            continue
        if smoke:
            evaluations[criterion] = verdict("notMeasured", reason="smoke")
            continue
        if not host_ok:
            evaluations[criterion] = verdict("notMeasured", reason="not the reference host")
            continue
        if not complete:
            evaluations[criterion] = verdict("notMeasured", reason="missing repetition")
            continue
        if not flags[NEEDS[criterion]]:
            evaluations[criterion] = verdict("notMeasured", reason=f"{NEEDS[criterion]} is false")
            continue
        missing = "instrumentation missing"
        if criterion == "wall":
            samples = pairs(lambda r: (dig(r, "interval", "wallSeconds"), 1))
            if None in samples:
                evaluations[criterion] = verdict("notMeasured", limit=limit["wallSeconds"], reason=missing)
            else:
                value = median([s[0] for s in samples])
                evaluations[criterion] = verdict(
                    "passed" if value <= limit["wallSeconds"] else "failed", value, None, limit["wallSeconds"])
        elif criterion == "rss":
            samples = pairs(lambda r: (dig(r, "rss", "workerMaxRssBytes"), 1))
            evaluations[criterion] = judge(samples, limit["rssBytes"], missing)
        elif criterion == "steadyAbsolute":
            samples = pairs(lambda r: (storage(r, "steady", "total"), 1))
            evaluations[criterion] = judge(samples, limit["steadyBytes"], missing)
        elif criterion == "steadyRatio":
            samples = pairs(lambda r: (storage(r, "steady", "total"), storage(r, "denominator", "bytes")))
            if any(s is not None and s[1] <= 0 for s in samples):
                samples = [None]
            evaluations[criterion] = judge(samples, limit["steadyToCanonicalMultiplier"], missing, ratio=True)
        else:
            samples = pairs(lambda r: (storage(r, "peak", "total"), storage(r, "steady", "total")))
            if any(s is not None and s[1] <= 0 for s in samples):
                samples = [None]
            evaluations[criterion] = judge(samples, limit["peakToSteadyMultiplier"], missing, ratio=True)
    walls = [dig(rep, "interval", "wallSeconds") for rep in completed]
    rsses = [dig(rep, "rss", "workerMaxRssBytes") for rep in completed]
    entry["median"] = {"wallSeconds": median(walls) if complete and all(number(w) for w in walls) else None}
    entry["peakWorkerRssBytes"] = max(rsses) if complete and all(number(r) for r in rsses) else None
    entry["evaluations"] = evaluations
    return evaluations


def qualify(report, policy, cohorts, flags=None):
    """Return a copy of the report with its qualification, evaluations and status filled in."""
    result = copy.deepcopy(report)
    flags = {**FLAGS, **(flags or {})}
    smoke = bool(result.get("smoke"))
    host_ok = dig(result, "campaign", "host", "reference") is True
    qualification = {"referenceHost": host_ok, **flags, "smoke": smoke}
    unqualified, failed, not_measured = [], [], []
    if result.get("status") == "unqualified":
        unqualified.append("preflight failure")
    for reason in result.get("statusReasons") or []:
        if isinstance(reason, str) and reason.startswith("preflight"):
            unqualified.append(reason)
    sizes = result.get("sizes")
    known = list(cohorts["synthetic"])
    policy_read = {}
    if not isinstance(sizes, list) or not sizes:
        unqualified.append("report has no sizes")
        sizes = []
    for entry in sizes:
        size = entry.get("size") if isinstance(entry, dict) else None
        if size not in ("small", "medium", "large") or size not in known:
            unqualified.append(f"unknown size {size!r}")
            continue
        limit = limits(policy, size)
        policy_read[size] = limit
        for rep in entry.get("repetitions") or []:
            if isinstance(rep, dict) and rep.get("status") == "fault":
                unqualified.append(f"{size} repetition {rep.get('ordinal')}: fault")
        evaluations = evaluate_size(entry, size, limit, flags, host_ok, smoke)
        for criterion, outcome in evaluations.items():
            if outcome["status"] == "failed":
                failed.append(f"{size} {criterion}: exceeds policy")
            elif outcome["status"] == "notMeasured":
                not_measured.append(f"{size} {criterion}: {outcome.get('reason', 'not measured')}")
    if smoke:
        not_measured.insert(0, "smoke")
    if unqualified:
        status, reasons = "unqualified", unqualified
    elif failed:
        status, reasons = "failed", failed
    elif not_measured:
        status, reasons = "notMeasured", not_measured
    else:
        status, reasons = "passed", []
    result.update(status=status, statusReasons=reasons, authoritativeAcceptance=False,
                  issue11="open", qualification=qualification, policy=policy_read)
    return result


def load_json(path):
    return json.loads(Path(path).read_text())


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        report = load_json(args.report)
        policy = load_json(POLICY_DIR / "benchmark-policy-v1.json")
        cohorts = load_json(POLICY_DIR / "cohorts-v1.json")
        if not isinstance(report, dict):
            raise ValueError("report must be an object")
        result = qualify(report, policy, cohorts)
    except (OSError, ValueError) as error:
        print(f"qualify-performance: {error}", file=sys.stderr)
        return 2
    sys.stdout.write(json.dumps(result, sort_keys=True, indent=2, allow_nan=False) + "\n")
    return 0 if result["status"] != "unqualified" else 1


if __name__ == "__main__":
    raise SystemExit(main())
