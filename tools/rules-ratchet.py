#!/usr/bin/env python3
"""Rules-check ratchet (#1008, refactoring spec G1): the recorded number of deviating
pairs and references may only fall.

    python3 tools/rules-ratchet.py            # check against the record
    python3 tools/rules-ratchet.py --update   # rewrite the record from a fresh measurement

The measurement runs through a daemon of its own under a private HOME, on a fresh index
of this checkout with fresh rust-analyzer and scip-typescript evidence; it never touches
the user's daemon or index. It runs only when `trellis producer doctor` reports both
producers `ready`; otherwise it prints a notice and exits 0.
"""
import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RECORD = ROOT / "docs/refactoring/0.2.x/rules-ratchet.json"
# The producers the spec's baseline was measured with, and the profile each runs under.
PRODUCERS = (("rust-analyzer", "trusted"), ("scip-typescript", "restricted"))
# Record keys, in file order. The first two are the ratchet; the rest are the
# comparison population they were counted on.
KEYS = ("deviatingPairs", "deviatingReferences", "comparablePairs", "conformingPairs",
        "unknownPairs", "partialFiles")
UPDATE = "python3 tools/rules-ratchet.py --update"
# Each `trellis status` waits a bounded time for the first index; this many calls is a
# backstop against a hang, not a speed expectation.
INDEX_ATTEMPTS = 20
RULES_ATTEMPTS = 20


class Failed(Exception):
    """The measurement could not be made; the lane fails with this message."""


def summarize(payload):
    """The record's figures from one `trellis rules check` payload."""
    counts = payload["counts"]
    deviating = [row for row in payload["instances"]
                 if row["status"] == "deviates" and not row["excepted"]]
    if len(deviating) != counts["deviates"]:
        raise Failed("rules check counts disagree with its instances")
    return {
        "deviatingPairs": counts["deviates"],
        "deviatingReferences": sum(row["referenceCount"] for row in deviating),
        "comparablePairs": counts["comparable"],
        "conformingPairs": counts["conforms"],
        "unknownPairs": counts["unknown"],
        "partialFiles": len(payload["partialFiles"]),
    }


def compare(record, measured):
    """Returns (verdict, lines); verdict is `fail`, `lower` or `equal`.

    A rise in deviating pairs or references fails. So does worse coverage (fewer
    comparable pairs, more unknown pairs or more partial files), because the check
    reports observed references, not absence: fewer deviations on less evidence is not
    progress. Anything else that differs is only a notice that the record can be updated.
    """
    def change(key):
        return f"{key}: recorded {record[key]}, measured {measured[key]}"

    rises = [change(key) for key in ("deviatingPairs", "deviatingReferences")
             if measured[key] > record[key]]
    lost = [change(key) for key, worse in (
        ("comparablePairs", measured["comparablePairs"] < record["comparablePairs"]),
        ("unknownPairs", measured["unknownPairs"] > record["unknownPairs"]),
        ("partialFiles", measured["partialFiles"] > record["partialFiles"]),
    ) if worse]
    if rises or lost:
        lines = []
        if rises:
            lines.append("rules ratchet FAILED: deviations rose")
            lines += [f"  {line}" for line in rises]
        if lost:
            lines.append("rules ratchet FAILED: the comparison population lost coverage")
            lines += [f"  {line}" for line in lost]
        lines.append("  Remove the added deviation or restore the coverage. A reviewed "
                     f"re-baseline or exception updates the record explicitly: {UPDATE}")
        return "fail", lines
    differs = [change(key) for key in KEYS if measured[key] != record[key]]
    if differs:
        return "lower", (["rules ratchet NOTICE: the record can be updated"]
                         + [f"  {line}" for line in differs]
                         + [f"  Lower it in this change: {UPDATE}"])
    return "equal", ["rules ratchet: " + ", ".join(f"{key} {measured[key]}" for key in KEYS)]


def read_record(path):
    record = json.loads(path.read_text())
    if record.get("schemaVersion") != 1 or any(
            not isinstance(record.get(key), int) for key in KEYS):
        raise Failed(f"{path}: not a rules-ratchet record")
    return record


def write_record(path, measured):
    path.write_text(json.dumps({"schemaVersion": 1, **{key: measured[key] for key in KEYS}},
                               indent=2) + "\n")


def private_env(home):
    """A private HOME for the daemon and its index. The declared RUSTUP_HOME and
    CARGO_HOME (local-topology D12) still name the user's toolchains, which is how the
    product finds rust-analyzer; nothing of the user's Trellis state is visible."""
    real = Path.home()
    env = dict(os.environ)
    for name in ("TRELLIS_SEARCH_MODEL", "TRELLIS_SEARCH_BACKEND", "TRELLIS_INDEX_DIAGNOSTICS"):
        env.pop(name, None)
    env["RUSTUP_HOME"] = os.environ.get("RUSTUP_HOME") or str(real / ".rustup")
    env["CARGO_HOME"] = os.environ.get("CARGO_HOME") or str(real / ".cargo")
    env["HOME"] = str(home / "home")
    for name, leaf in (("XDG_CACHE_HOME", "cache"), ("XDG_DATA_HOME", "data"),
                       ("XDG_CONFIG_HOME", "config")):
        env[name] = str(home / leaf)
    for leaf in ("home", "cache", "data", "config"):
        (home / leaf).mkdir(mode=0o700)
    return env


class Trellis:
    def __init__(self, binary, env):
        self.binary, self.env = str(binary), env

    def run(self, *args):
        return subprocess.run([self.binary, *args], cwd=ROOT, env=self.env,
                              capture_output=True, text=True)

    def json(self, *args):
        done = self.run(*args)
        try:
            return json.loads(done.stdout)
        except ValueError:
            raise Failed(f"trellis {' '.join(args)} (exit {done.returncode}): "
                         f"{done.stderr.strip()[-2000:]}") from None


def missing_producers(trellis):
    """Why the ratchet cannot run, from the product's own detection; empty when it can."""
    report = {entry["name"]: entry for entry in trellis.json("producer", "doctor", "--json")["producers"]}
    reasons = []
    for name, _ in PRODUCERS:
        entry = report.get(name)
        if entry is None or entry["state"] != "ready":
            state = "not reported" if entry is None else entry["state"]
            reason = (entry or {}).get("reason")
            reasons.append(f"{name} is {state}" + (f" ({reason})" if reason else ""))
    return reasons


def measure(trellis):
    # The first read starts the daemon and indexes; it waits a bounded time per call.
    for attempt in range(INDEX_ATTEMPTS):
        status = trellis.run("status")
        if status.returncode == 0:
            break
    else:
        raise Failed(f"the index was not ready: {status.stderr.strip()[-2000:]}")
    for name, trust in PRODUCERS:
        plan = trellis.json("producer", "plan", "--producer", name, "--trust", trust)
        if "planSha256" not in plan:
            raise Failed(f"producer plan {name}: {json.dumps(plan)[:2000]}")
        run = trellis.json("producer", "run", "--producer", name, "--trust", trust,
                           "--plan-sha256", plan["planSha256"])
        if run.get("outcome") != "imported":
            raise Failed(f"producer run {name}: state {run.get('state')}, outcome "
                         f"{run.get('outcome')}, {run.get('errorCode')}")
    # After an import the daemon computes the architecture for the new revision. A read
    # waits 30 s for it and then fails with the typed `deadline_exceeded`
    # (docs/architecture-view.md); only that is asked again, a bounded number of times.
    for attempt in range(RULES_ATTEMPTS):
        check = trellis.run("rules", "check")
        if "deadline_exceeded" not in check.stderr:
            break
    # Exit 1 with a reply only says deviations exist.
    try:
        reply = json.loads(check.stdout)
    except ValueError:
        raise Failed(f"rules check (exit {check.returncode}): "
                     f"{check.stderr.strip()[-2000:]}") from None
    payload = reply.get("payload")
    if payload is None:
        raise Failed(f"rules check: {json.dumps(reply)[:2000]}")
    stale = {key: value for key, value in payload["semanticDocumentStatus"].items() if value}
    if reply.get("catchingUp") or payload["partial"] or stale:
        raise Failed(f"rules check did not run on fresh, whole evidence: catchingUp "
                     f"{reply.get('catchingUp')}, partial {payload['partial']}, {stale}")
    return summarize(payload)


def build(target_dir):
    """Builds the binary and takes a copy: the rust lane rebuilds `debug/trellis` with
    other features in the same target directory while this lane runs."""
    subprocess.run(["cargo", "build", "--locked", "--bin", "trellis"], cwd=ROOT, check=True)
    copy = target_dir / "debug" / "trellis-rules-ratchet"
    shutil.copy2(target_dir / "debug" / "trellis", copy)
    return copy


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--update", action="store_true", help="rewrite the record")
    parser.add_argument("--binary", type=Path, help="a built trellis; default: build one")
    parser.add_argument("--record", type=Path, default=RECORD)
    args = parser.parse_args()
    target_dir = ROOT / os.environ.get("CARGO_TARGET_DIR", "target")
    started = time.monotonic()
    # So a terminated lane still stops its daemon and removes its HOME.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    home = Path(tempfile.mkdtemp(prefix="trellis-ratchet-"))
    trellis = None
    try:
        record = None if args.update else read_record(args.record)
        binary = args.binary.resolve() if args.binary else build(target_dir)
        trellis = Trellis(binary, private_env(home))
        missing = missing_producers(trellis)
        if missing:
            print("rules ratchet NOT RUN: it needs both semantic producers, and "
                  + "; ".join(missing) + ". Run `trellis producer doctor` for the install "
                  "commands. Deviations added by this change are not checked.")
            return 0
        measured = measure(trellis)
        if args.update:
            write_record(args.record, measured)
            print(f"rules ratchet: wrote {args.record}: "
                  + ", ".join(f"{key} {measured[key]}" for key in KEYS))
            return 0
        verdict, lines = compare(record, measured)
        print("\n".join(lines))
        return 1 if verdict == "fail" else 0
    except Failed as error:
        print(f"rules ratchet FAILED: {error}", file=sys.stderr)
        return 1
    finally:
        if trellis is not None:
            trellis.run("daemon", "stop")
        shutil.rmtree(home, ignore_errors=True)
        print(f"rules ratchet took {int(time.monotonic() - started)} s")


if __name__ == "__main__":
    sys.exit(main())
