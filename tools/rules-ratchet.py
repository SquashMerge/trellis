#!/usr/bin/env python3
"""Rules-check ratchet (#1008, refactoring spec G1): the recorded number of deviating
pairs and references may only fall.

    python3 tools/rules-ratchet.py            # check against the record
    python3 tools/rules-ratchet.py --update   # rewrite the record from a fresh measurement

The measurement runs through a daemon of its own under a private HOME, on a fresh index
of this checkout with fresh rust-analyzer and scip-typescript evidence; it never touches
the user's daemon or index. It runs only when `trellis producer doctor` reports both
producers `ready`; otherwise it prints a notice and exits 0.

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
# `trellis rules check` waits 30 s for the architecture build, so 20 reads are at most
# 10 minutes of waiting.
RULES_ATTEMPTS = 20
# The CLI prints a failure as `Error: <code>: <message>` and exits 1; this is the code.
ERROR_CODE = re.compile(r"^Error: ([a-z_]+)(?::|$)", re.MULTILINE)
# Hang backstops for the cleanup: `daemon stop` and the daemon's exit after it, then
# after SIGTERM, then after SIGKILL.
STOP_SECS, TERM_SECS, KILL_SECS = 120, 15, 15


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

    `daemon stop` succeeds only once the daemon has retired. Whatever it says, the
    process is waited for by its PID, then terminated, then killed; the HOME, which
    holds its lock and state, goes only when the process is gone."""
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
    shutil.rmtree(home, ignore_errors=True)
    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--update", action="store_true", help="rewrite the record")
    parser.add_argument("--binary", type=Path, help="a built trellis; default: build one")
    parser.add_argument("--record", type=Path, default=RECORD)
    args = parser.parse_args()
    target_dir = ROOT / os.environ.get("CARGO_TARGET_DIR", "target")
    started = time.monotonic()
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, Signals.handle)
    home = Path(tempfile.mkdtemp(prefix="trellis-ratchet-"))
    owned = []  # [trellis, daemon] once this run has started its daemon
    code = 1
    try:
        code = check(args, target_dir, home, owned)
    except Failed as error:
        print(f"rules ratchet FAILED: {error}", file=sys.stderr)
        code = 1
    except Interrupted as error:
        print(f"rules ratchet interrupted by {error}", file=sys.stderr)
        code = 128 + Signals.received
    finally:
        Signals.cleaning = True
        problems = shut_down(*(owned or (None, None)), home)
        for problem in problems:
            print(f"rules ratchet FAILED: cleanup: {problem}", file=sys.stderr)
        print(f"rules ratchet took {int(time.monotonic() - started)} s")
    return code or (1 if problems else 0)


def check(args, target_dir, home, owned):
    record = None if args.update else read_record(args.record)
    binary = args.binary.resolve() if args.binary else build(target_dir)
    trellis = Trellis(binary, private_env(home))
    missing = missing_producers(trellis)
    if missing:
        print("rules ratchet NOT RUN: it needs both semantic producers, and "
              + "; ".join(missing) + ". Run `trellis producer doctor` for the install "
              "commands. Deviations added by this change are not checked.")
        return 0
    Signals.check()
    daemon = trellis.start_daemon()
    owned += [trellis, daemon]
    measured = summarize(measure(trellis, daemon))
    if args.update:
        write_record(args.record, measured)
        print(f"rules ratchet: wrote {args.record}: "
              + ", ".join(f"{key} {measured[key]}" for key in KEYS))
        return 0
    verdict, lines = compare(record, measured)
    print("\n".join(lines))
    return 1 if verdict == "fail" else 0


if __name__ == "__main__":
    sys.exit(main())
