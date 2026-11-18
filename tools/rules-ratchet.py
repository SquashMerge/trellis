#!/usr/bin/env python3
"""Rules-check ratchet (#1008, refactoring spec G1): the recorded number of deviating
pairs and references may only fall.

    python3 tools/rules-ratchet.py            # check against the record
    python3 tools/rules-ratchet.py --update   # rewrite the record from a fresh measurement
    ./tools/verify --lane ratchet             # the check, as verify's opt-in lane

It runs on request and nightly, not in a default `./tools/verify` (owner ruling,
2026-11-14): the measurement takes minutes.

Only first-party references are counted. `rules check` gives every instance a
`destination`: a native module, null when the reference is unresolved, or
`{kind: "externalPackage", ...}` for a standard-library or external package. Instances
with an `externalPackage` destination are left out of every figure, so a new
`import json` in a tool changes nothing here.

The run fails when deviating pairs, deviating references or partial files rise above
the record, and when a deviating pair is measured that the record does not have. It also
fails on lost coverage, as spec G1 words it, decided per pair (#1140): a recorded pair
has fewer deviating references than recorded, or is gone, and its source file has more
unknown references than recorded for that file. That is what a deviating reference
that became unresolvable instead of being fixed looks like; the check compares counts
and does not prove it is the same reference. Unknown references are the references of
the instances whose status is `unknown`; each has a source file, the instance's `target`.
Any other rise in unknown references is only a notice: they are over a third of the
population's references, so ordinary new code raises them. The record keeps the
deviating references of each pair and the unknown references of each of those pairs'
source files for this comparison.

Excepted pairs, the pairs a reviewed `[[exceptions]]` entry of `.trellis/rules.toml`
approves, are in no total, but their source files are checked for lost coverage too
(#1211): the record keeps the references of each excepted pair whose status is
`deviates`, and the unknown references of its source file, and the same per-pair rule
applies to them. Any other change in an excepted pair, a new one included, is a notice.

A pair is its source path and its destination path; renames are not detected. A
deviating pair whose source or destination file was renamed, split or merged is a pair
the record does not have, so the run fails until the record is updated. The pair under
the old path counts as fallen.

Pair totals are recorded as information and never fail: an unknown pair is one source
file's aggregate, so splitting, merging or renaming a file moves pair counts without
changing what is known.

`--update` refuses to rewrite the record from a measurement that is lost coverage by
that per-pair rule, unless `--accept-unknown-rise` is given. It prints the
unknown-reference change either way.

Known gap: a bare JavaScript import the tool leaves unresolved (`destination` null, such
as `node:crypto` or `esbuild`) is not classified `externalPackage`. It stays in the
population as unknown references (see #1088).

The measurement runs through a daemon of its own under a private HOME, on a fresh index
of this checkout with fresh rust-analyzer and scip-typescript evidence; it never touches
the user's daemon or index. It runs only when `trellis producer doctor` reports both
producers `ready`; otherwise it prints a notice and exits 0. The nightly job passes
`--require-producers`, which makes that a failure.

Exit status: 0, the record holds (or the run was not made, without
`--require-producers`); 1, the measurement is worse than the record: deviations rose,
more files are partial, or coverage was lost (or `--update` refused to rewrite the
record); 3, no comparison was made: a producer is missing or failed, the build, the
daemon or the index failed, the record or the binary is missing or unreadable, the
cleanup failed, or anything else went wrong. An exit of 3 says nothing about the
layering. 2 is a usage error.

The script starts that daemon itself, so it knows its PID, and on every exit it stops
it, waits for the process to be gone and only then removes the HOME. A stop that fails
fails the run. SIGINT and SIGTERM are recorded and acted on between commands, never as
an exception in the middle of one, so they cannot skip or interrupt that cleanup.
"""
import argparse
import contextlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import traceback
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RECORD = ROOT / "docs/refactoring/0.2.x/rules-ratchet.json"
# The producers the spec's baseline was measured with, and the profile each runs under.
PRODUCERS = (("rust-analyzer", "trusted"), ("scip-typescript", "restricted"))
# Record keys, in file order. A rise in any of RATCHETED fails; the pair totals after
# them describe the population and are information only.
RATCHETED = ("deviatingPairs", "deviatingReferences", "unknownReferences", "partialFiles")
KEYS = (*RATCHETED, "comparablePairs", "conformingPairs", "unknownPairs")
# What the per-pair comparison needs (#1140): the deviating references of each pair, as
# {source path: {destination path: references}}, the same for each excepted pair (#1211),
# and the unknown references from each source file, as {source path: references}. The
# record keeps the last only for the sources of its deviating and excepted pairs.
BY_PAIR, EXCEPTED = "deviatingReferencesByPair", "exceptedReferencesByPair"
BY_SOURCE = "unknownReferencesBySource"
# The pair maps the per-pair lost-coverage rule reads, each with how a line names it.
PAIR_MAPS = ((BY_PAIR, "deviating references", ""),
             (EXCEPTED, "excepted references", " (excepted)"))
SCHEMA_VERSION = 5
# What the record's figures count and how they were measured, written into the record
# so it can be read without this script.
DESCRIPTION = {
    "population": "Production instances of `trellis rules check` whose destination is "
                  "not an externalPackage: first-party sources to first-party or "
                  "unresolved destinations. Standard-library and external packages are "
                  "not counted. Known gap: a bare JavaScript import the tool leaves "
                  "unresolved (destination null, such as node:crypto or esbuild) is "
                  "not an externalPackage, so it stays in the population as unknown "
                  "references (see #1088).",
    "rule": "A run fails when deviatingPairs, deviatingReferences or partialFiles is "
            "above this record, or when it measures a deviating pair that is not in "
            "deviatingReferencesByPair. It also fails as lost coverage, decided per "
            "pair: a pair in deviatingReferencesByPair or exceptedReferencesByPair "
            "(source path, then destination path) has fewer references than that map "
            "records for it, or is gone from it, while its source file has more "
            "unknown references than unknownReferencesBySource records for it. Any "
            "other rise in unknown references is a notice, and so is any other change "
            "in an excepted pair, a new one included. unknownReferences is the sum of "
            "referenceCount over the population's instances with status unknown that "
            "are not excepted; exceptedReferencesByPair is that sum for each excepted "
            "instance with status deviates, a pair a reviewed exception in "
            ".trellis/rules.toml approves, which no total counts; "
            "unknownReferencesBySource is the unknown sum for each source file of a "
            "recorded deviating or excepted pair. Pairs are compared by path "
            "and renames are not detected: a deviating pair whose source or "
            "destination file was renamed, split or merged is a pair this record does "
            "not have, so the run fails until the record is updated, and the pair "
            "under the old path counts as fallen. An update is refused when the "
            "measurement is lost coverage by this rule, unless --accept-unknown-rise "
            "is given. comparablePairs, conformingPairs and unknownPairs are "
            "information only: an unknown pair is one source file's aggregate, so a "
            "file split, merge, rename or deletion moves them without changing "
            "coverage.",
    "measurement": "`python3 tools/rules-ratchet.py --update`: a debug build of this "
                   "checkout, its own daemon under a private HOME, a fresh index, then "
                   "rust-analyzer (trusted) and scip-typescript (restricted) imported.",
}
UPDATE = "python3 tools/rules-ratchet.py --update"
ACCEPT = "--accept-unknown-rise"
# Each `trellis status` waits a bounded time for the first index; this many calls is a
# backstop against a hang, not a speed expectation.
INDEX_ATTEMPTS = 20
# `trellis rules check` waits 30 s for the architecture build, so 20 reads are at most
# 10 minutes of waiting.
RULES_ATTEMPTS = 20
# The CLI prints a failure as `Error: <code>: <message>` and exits 1; this is the code.
ERROR_CODE = re.compile(r"^Error: ([a-z_]+)(?::|$)", re.MULTILINE)
# Hang backstops for the cleanup: `daemon stop` and the daemon's exit after it, then
# after SIGTERM, then after SIGKILL.
STOP_SECS, TERM_SECS, KILL_SECS = 120, 15, 15
# Exit statuses. WORSE: the measurement was made and is worse than the record.
# NOT_MEASURED: no comparison was made, so nothing is known about the layering. 2 is
# argparse's usage error.
WORSE, NOT_MEASURED = 1, 3


class Failed(Exception):
    """The measurement could not be made; the lane fails with this message."""


class Interrupted(Exception):
    """SIGINT or SIGTERM arrived; the run ends through the ordinary cleanup."""


class Signals:
    """SIGINT and SIGTERM without asynchronous exceptions: the handler records the
    signal and ends the command in progress, and `run` raises `Interrupted` at its own
    boundaries. During cleanup the signal is only recorded."""
    received = None
    current = None
    cleaning = False

    @classmethod
    def handle(cls, signum, _frame):
        cls.received = signum
        if cls.current is not None and not cls.cleaning:
            with contextlib.suppress(OSError):
                cls.current.terminate()

    @classmethod
    def check(cls):
        if cls.received is not None and not cls.cleaning:
            raise Interrupted(signal.Signals(cls.received).name)


def run(argv, timeout=None, **options):
    """One command to completion, as `subprocess.run` with captured text output."""
    Signals.check()
    process = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, **options)
    Signals.current = process
    try:
        # A signal that arrived before `current` was set has ended nothing yet.
        if Signals.received is not None and not Signals.cleaning:
            process.terminate()
        try:
            out, err = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            out, err = process.communicate()
            err += f"\n(killed after {timeout} s)"
    finally:
        Signals.current = None
    Signals.check()
    return subprocess.CompletedProcess(argv, process.returncode, out, err)


def is_external(row):
    """Whether the instance's destination is a standard-library or external package,
    by the tool's own typed destination (the closed union in docs/rules.md)."""
    destination = row["destination"]
    return destination is not None and destination.get("kind") == "externalPackage"


def summarize(payload):
    """The record's figures from one `trellis rules check` payload, and the number of
    instances left out because their destination is an external package."""
    counts, instances = payload["counts"], payload["instances"]

    def having(rows, status):
        return [row for row in rows if row["status"] == status and not row["excepted"]]

    # The instance list must be the whole population the tool counted.
    if len(instances) != counts["comparable"] or any(
            len(having(instances, status)) != counts[key]
            for status, key in (("deviates", "deviates"), ("unknown", "unknown"))):
        raise Failed("rules check counts disagree with its instances")
    first_party = [row for row in instances if not is_external(row)]
    deviating, unknown = having(first_party, "deviates"), having(first_party, "unknown")
    excepted = [row for row in first_party if row["status"] == "deviates" and row["excepted"]]
    by_pair, by_excepted, by_source = {}, {}, {}
    for rows, into in ((deviating, by_pair), (excepted, by_excepted)):
        for row in rows:
            destinations = into.setdefault(row["target"]["path"], {})
            destination = row["destination"]["path"]
            destinations[destination] = destinations.get(destination, 0) + row["referenceCount"]
    for row in unknown:
        source = row["target"]["path"]
        by_source[source] = by_source.get(source, 0) + row["referenceCount"]
    return {
        "deviatingPairs": len(deviating),
        "deviatingReferences": sum(row["referenceCount"] for row in deviating),
        "unknownReferences": sum(row["referenceCount"] for row in unknown),
        "partialFiles": len(payload["partialFiles"]),
        "comparablePairs": len(first_party),
        "conformingPairs": len(having(first_party, "conforms")),
        "unknownPairs": len(unknown),
        BY_PAIR: by_pair,
        EXCEPTED: by_excepted,
        BY_SOURCE: by_source,
    }, len(instances) - len(first_party)


DEVIATIONS = ("deviatingPairs", "deviatingReferences")


def pairs(figures, key=BY_PAIR):
    """The deviating pairs, or with `key` EXCEPTED the excepted ones, as
    {(source, destination): references}."""
    return {(source, destination): references
            for source, destinations in figures[key].items()
            for destination, references in destinations.items()}


def recorded(figures):
    """The part of a measurement the record keeps, in the record's order: the totals,
    every deviating and every excepted pair, and the unknown references of those pairs'
    source files."""
    maps = {key: {source: dict(sorted(destinations.items()))
                  for source, destinations in sorted(figures[key].items())}
            for key in (BY_PAIR, EXCEPTED)}
    sources = sorted({*maps[BY_PAIR], *maps[EXCEPTED]})
    return {**{key: figures[key] for key in KEYS}, **maps,
            BY_SOURCE: {source: figures[BY_SOURCE].get(source, 0) for source in sources}}


def lost_coverage(record, measured):
    """Spec G1's lost coverage, one line per pair: a recorded deviating or excepted pair
    with fewer references, or gone, whose source file has more unknown references than
    recorded. The check reports observed references, not absence, and these counts are
    what a reference that became unresolvable looks like; they do not prove it is the
    same reference."""
    lines = []
    for key, what, mark in PAIR_MAPS:
        now = pairs(measured, key)
        for (source, destination), references in pairs(record, key).items():
            before, after = record[BY_SOURCE][source], measured[BY_SOURCE].get(source, 0)
            if now.get((source, destination), 0) < references and after > before:
                lines.append(f"{source} -> {destination}{mark}: {what} recorded "
                             f"{references}, measured {now.get((source, destination), 0)}; "
                             f"unknown references from {source}: recorded {before}, "
                             f"measured {after} ({after - before:+d})")
    return lines


def unknown_change(record, measured):
    """One line saying how unknown references moved against the record."""
    delta = measured["unknownReferences"] - record["unknownReferences"]
    return (f"unknownReferences: recorded {record['unknownReferences']}, measured "
            f"{measured['unknownReferences']} ({delta:+d})")


def compare(record, measured):
    """Returns (verdict, lines); verdict is `fail`, `lower` or `equal`.

    A rise in deviating pairs, deviating references or partial files fails, and so does
    a deviating pair the record does not have. So does lost coverage: a recorded
    deviating or excepted pair with fewer references and more unknown references from
    its source file. Any other rise in unknown references is a notice, as is any other
    difference, in an excepted pair too. Pair totals never fail: they move when a file
    is split, merged, renamed or deleted.
    """
    def change(key):
        return f"{key}: recorded {record[key]}, measured {measured[key]}"

    before, now = pairs(record), pairs(measured)
    rises = [change(key) for key in DEVIATIONS if measured[key] > record[key]]
    rises += [f"new deviating pair: {source} -> {destination} ({references} references)"
              for (source, destination), references in sorted(now.items())
              if (source, destination) not in before]
    partial = measured["partialFiles"] > record["partialFiles"]
    lost = lost_coverage(record, measured)
    if rises or partial or lost:
        lines = []
        if rises:
            lines.append("rules ratchet FAILED: deviations rose")
            lines += [f"  {line}" for line in rises]
        if partial:
            lines.append("rules ratchet FAILED: more files are partial")
            lines.append(f"  {change('partialFiles')}")
        if lost:
            lines.append("rules ratchet FAILED: the comparison population lost coverage: "
                         "fewer deviating or excepted references in a pair on more "
                         "unknown references from its source file")
            lines += [f"  {line}" for line in lost]
        lines.append("  Remove the added deviation or restore the coverage. A reviewed "
                     f"re-baseline or exception updates the record explicitly: {UPDATE}")
        return "fail", lines
    if recorded(measured) != recorded(record):
        lines = ["rules ratchet NOTICE: the record can be updated"]
        lines += [f"  {change(key)}" for key in KEYS if measured[key] != record[key]]
        for key, what, mark in PAIR_MAPS:
            was, measured_pairs = pairs(record, key), pairs(measured, key)
            lines += [f"  {source} -> {destination}{mark}: {what} recorded "
                      f"{was.get((source, destination), 0)}, measured {references}"
                      for (source, destination), references in sorted(
                          {**{pair: 0 for pair in was}, **measured_pairs}.items())
                      if was.get((source, destination), 0) != references]
        lines += [f"  unknown references from {source}: recorded {references}, measured "
                  f"{measured[BY_SOURCE].get(source, 0)}"
                  for source, references in record[BY_SOURCE].items()
                  if measured[BY_SOURCE].get(source, 0) != references]
        if measured["unknownReferences"] > record["unknownReferences"]:
            lines.append(f"  NOTICE: unknown references rose; {unknown_change(record, measured)}. "
                         "Not a failure: no deviating or excepted pair with fewer "
                         "references has more unknown references from its source file.")
        return "lower", lines + [f"  Update it: {UPDATE}"]
    return "equal", ["rules ratchet: " + ", ".join(f"{key} {measured[key]}" for key in KEYS)]


def update_refusal(previous, measured, accept_unknown_rise):
    """Returns (refused, lines) for rewriting the record `previous` with `measured`.

    A measurement that is lost coverage against the record would lock in an improvement
    claimed on less evidence, so writing it needs the flag."""
    lines = [f"rules ratchet: {unknown_change(previous, measured)}"]
    lost = lost_coverage(previous, measured)
    if not lost:
        return False, lines
    if accept_unknown_rise:
        return False, lines + ["rules ratchet: recording fewer deviating references on more "
                               f"unknown references from the same source file, as {ACCEPT} asks",
                               *(f"  {line}" for line in lost)]
    return True, lines + [
        "rules ratchet FAILED: the record was not rewritten: a deviating or excepted pair "
        "has fewer references than recorded while its source file has more unknown "
        "references",
        *(f"  {line}" for line in lost),
        f"  If the rise in unknown references is reviewed and accepted: {UPDATE} {ACCEPT}"]


def counts(value):
    """Whether `value` is a {path: non-negative integer} object."""
    return isinstance(value, dict) and all(
        isinstance(count, int) and count >= 0 for count in value.values())


def read_record(path):
    record = json.loads(path.read_text())
    if (record.get("schemaVersion") != SCHEMA_VERSION
            or any(record.get(key) != text for key, text in DESCRIPTION.items())
            or any(not isinstance(record.get(key), int) for key in KEYS)
            or any(not isinstance(record.get(key), dict)
                   or not all(counts(destinations) for destinations in record[key].values())
                   for key in (BY_PAIR, EXCEPTED))
            or not counts(record.get(BY_SOURCE))
            or set(record[BY_SOURCE]) != {*record[BY_PAIR], *record[EXCEPTED]}):
        raise Failed(f"{path}: not a version {SCHEMA_VERSION} rules-ratchet record; "
                     f"rewrite it with {UPDATE}")
    return record


def write_record(path, measured):
    path.write_text(json.dumps({"schemaVersion": SCHEMA_VERSION, **DESCRIPTION,
                                **recorded(measured)}, indent=2) + "\n")


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

    def run(self, *args, timeout=None):
        return run([self.binary, *args], timeout=timeout, env=self.env)

    def start_daemon(self):
        """The daemon as this script's own child, in its own session so a terminal's
        Ctrl-C reaches it only through the cleanup. The CLI attaches to it."""
        return subprocess.Popen([self.binary, "daemon"], cwd=ROOT, env=self.env,
                                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, start_new_session=True)

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


def without_producers(missing, required):
    """Returns (exit status, line) for a run that cannot be made because producers are
    not ready. On request that is a notice; where they are required, as in the nightly
    job, it is a failure to measure, never a worse measurement."""
    reasons = "; ".join(missing)
    if required:
        return NOT_MEASURED, ("rules ratchet FAILED: not measured: it needs both semantic "
                              f"producers, and {reasons}. Nothing was compared with the record.")
    return 0, ("rules ratchet NOT RUN: it needs both semantic producers, and "
               f"{reasons}. Run `trellis producer doctor` for the install "
               "commands. Deviations added by this change are not checked.")


def error_code(done):
    """The typed code of a failed CLI command that printed no reply, else None."""
    if done.returncode != 1 or done.stdout.strip():
        return None
    codes = ERROR_CODE.findall(done.stderr)
    return codes[-1] if codes else None


def rules_check(trellis):
    """The `rules check` reply. After an import the daemon computes the architecture
    for the new revision; a read waits 30 s for it and then fails with the typed
    `deadline_exceeded` (docs/architecture-view.md). Only that is asked again."""
    for attempt in range(RULES_ATTEMPTS):
        check = trellis.run("rules", "check")
        if error_code(check) != "deadline_exceeded":
            break
    # Exit 1 with a reply only says deviations exist.
    try:
        return json.loads(check.stdout)
    except ValueError:
        raise Failed(f"rules check (exit {check.returncode}): "
                     f"{check.stderr.strip()[-2000:]}") from None


def measure(trellis, daemon):
    # The first read indexes; it waits a bounded time per call.
    for attempt in range(INDEX_ATTEMPTS):
        status = trellis.run("status")
        if status.returncode == 0:
            break
    else:
        raise Failed(f"the index was not ready: {status.stderr.strip()[-2000:]}")
    # A CLI command that found no listener yet starts a daemon too; the daemon's lock
    # elects one and the loser exits. Only our own child may be the one that answered.
    if daemon.poll() is not None:
        raise Failed("the daemon this run started is not the one serving it")
    for name, trust in PRODUCERS:
        plan = trellis.json("producer", "plan", "--producer", name, "--trust", trust)
        if "planSha256" not in plan:
            raise Failed(f"producer plan {name}: {json.dumps(plan)[:2000]}")
        run = trellis.json("producer", "run", "--producer", name, "--trust", trust,
                           "--plan-sha256", plan["planSha256"])
        if run.get("outcome") != "imported":
            raise Failed(f"producer run {name}: state {run.get('state')}, outcome "
                         f"{run.get('outcome')}, {run.get('errorCode')}")
    reply = rules_check(trellis)
    payload = reply.get("payload")
    if payload is None:
        raise Failed(f"rules check: {json.dumps(reply)[:2000]}")
    stale = {key: value for key, value in payload["semanticDocumentStatus"].items() if value}
    if reply.get("catchingUp") or payload["partial"] or stale:
        raise Failed(f"rules check did not run on fresh, whole evidence: catchingUp "
                     f"{reply.get('catchingUp')}, partial {payload['partial']}, {stale}")
    return payload


def build(target_dir):
    """Builds the binary in a target directory only this script writes, next to the one
    verify's rust lane owns, as the tests lane has its own: no other build can replace
    this binary, and this build cannot replace the one the browser suite copies."""
    own = Path(f"{target_dir}-ratchet")
    done = run(["cargo", "build", "--locked", "--bin", "trellis"],
               env={**os.environ, "CARGO_TARGET_DIR": str(own)})
    if done.returncode != 0:
        raise Failed(f"cargo build (exit {done.returncode}): {done.stderr.strip()[-4000:]}")
    return own / "debug" / "trellis"


def shut_down(trellis, daemon, home):
    """Stops the daemon this run started and removes its HOME; returns the problems.

    `daemon stop` succeeds only once the daemon has retired, and also when it finds no
    daemon. Whatever it says, the process is waited for by its PID, then terminated,
    then killed; the HOME, which holds its lock and state, goes only when the process
    is gone. A stopped daemon exits 0: any other exit status, or a signal, is a problem
    even when the stop command reported success."""
    problems = []
    if daemon is not None:
        stop = trellis.run("daemon", "stop", timeout=STOP_SECS)
        if stop.returncode != 0:
            problems.append(f"daemon stop failed (exit {stop.returncode}): "
                            f"{stop.stderr.strip()[-2000:]}")
        for wait, name, escalate in (
                (STOP_SECS if stop.returncode == 0 else 0, "SIGTERM", daemon.terminate),
                (TERM_SECS, "SIGKILL", daemon.kill), (KILL_SECS, None, None)):
            try:
                daemon.wait(timeout=wait)
                break
            except subprocess.TimeoutExpired:
                if escalate is None:
                    problems.append(f"daemon {daemon.pid} is still running; its HOME is "
                                    f"left at {home}")
                    return problems
                problems.append(f"daemon {daemon.pid} was still running; sent {name}")
                escalate()
        if daemon.returncode < 0:
            problems.append(f"daemon {daemon.pid} was ended by "
                            f"{signal.Signals(-daemon.returncode).name}")
        elif daemon.returncode > 0:
            problems.append(f"daemon {daemon.pid} exited with code {daemon.returncode}")
    shutil.rmtree(home, ignore_errors=True)
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--update", action="store_true", help="rewrite the record")
    parser.add_argument(ACCEPT, action="store_true",
                        help="with --update: rewrite the record although a deviating or "
                             "excepted pair has fewer references on more unknown "
                             "references from its source file")
    parser.add_argument("--require-producers", action="store_true",
                        help="fail (exit 3) when a producer is not ready, instead of "
                             "printing a notice and exiting 0")
    parser.add_argument("--binary", type=Path, help="a built trellis; default: build one")
    parser.add_argument("--record", type=Path, default=RECORD)
    parser.add_argument("--save-payload", type=Path,
                        help="also write the measured `rules check` payload here")
    args = parser.parse_args()
    target_dir = ROOT / os.environ.get("CARGO_TARGET_DIR", "target")
    started = time.monotonic()
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, Signals.handle)
    home = Path(tempfile.mkdtemp(prefix="trellis-ratchet-"))
    owned = []  # [trellis, daemon] once this run has started its daemon
    code = NOT_MEASURED
    try:
        code = check(args, target_dir, home, owned)
    except Failed as error:
        print(f"rules ratchet FAILED: not measured: {error}", file=sys.stderr)
        code = NOT_MEASURED
    except Interrupted as error:
        print(f"rules ratchet interrupted by {error}", file=sys.stderr)
        code = 128 + Signals.received
    except Exception as error:  # a missing record or binary, an I/O error, a defect here
        # Only an explicit comparison may exit WORSE; Python's own exit for an uncaught
        # exception is 1 too.
        traceback.print_exc()
        print(f"rules ratchet FAILED: not measured: {type(error).__name__}: {error}",
              file=sys.stderr)
        code = NOT_MEASURED
    finally:
        Signals.cleaning = True
        problems = shut_down(*(owned or (None, None)), home)
        for problem in problems:
            print(f"rules ratchet FAILED: cleanup: {problem}", file=sys.stderr)
        print(f"rules ratchet took {int(time.monotonic() - started)} s")
    return code or (NOT_MEASURED if problems else 0)


def check(args, target_dir, home, owned):
    if args.update:
        # A missing record, or one of another version or shape, has nothing to compare with.
        try:
            record = read_record(args.record)
        except (Failed, OSError, ValueError):
            record = None
    else:
        record = read_record(args.record)
    binary = args.binary.resolve() if args.binary else build(target_dir)
    trellis = Trellis(binary, private_env(home))
    missing = missing_producers(trellis)
    if missing:
        code, line = without_producers(missing, args.require_producers)
        print(line)
        return code
    Signals.check()
    daemon = trellis.start_daemon()
    owned += [trellis, daemon]
    payload = measure(trellis, daemon)
    if args.save_payload:
        args.save_payload.write_text(json.dumps(payload, indent=1) + "\n")
    measured, external = summarize(payload)
    print(f"rules ratchet: {external} pairs with a standard-library or external package "
          "destination are not counted")
    if args.update:
        if record is None:
            print("rules ratchet: no current record to compare the update with")
        else:
            refused, lines = update_refusal(record, measured, args.accept_unknown_rise)
            print("\n".join(lines))
            if refused:
                return WORSE
        write_record(args.record, measured)
        print(f"rules ratchet: wrote {args.record}: "
              + ", ".join(f"{key} {measured[key]}" for key in KEYS))
        return 0
    verdict, lines = compare(record, measured)
    print("\n".join(lines))
    return WORSE if verdict == "fail" else 0


if __name__ == "__main__":
    sys.exit(main())
