#!/usr/bin/env python3
"""Private, opt-in #16 Stage3 watcher measurement; never a Factory verifier.

--prepare creates a pinned isolated fixture (optionally builds release).
--measure requires an explicit exclusive reference-host slot and consumes it.
--summarize only reads JSONL; it never launches Trellis or edits files.
No command is executed merely by importing this file.

Usage (reference host, quiet slot; scratch must be an absent directory outside the checkout):
  python3 tools/stage3-watcher-runner.py --prepare --build --scratch <dir>
  python3 tools/stage3-watcher-runner.py --measure --scratch <dir> --exclusive-slot-authorization "<who/when>"
  python3 tools/stage3-watcher-runner.py --summarize --events <dir>/watcher-events.jsonl
Gate: medium p95 <= 2 s, large p95 <= 5 s, 20 valid samples each, fallback parity.
Healthy native watching only (owner ruling 2026-10-15, Decision 0009 §5): a series
whose watcher reports degraded (before it or after any sample) is invalid, never
trimmed; the run stops and the series must be restarted when watching is healthy.
Degraded mode (no native watcher: periodic 60 s inventory) has no latency gate.
"""
import argparse
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import time
from urllib.request import urlopen

REPO = Path(__file__).resolve().parents[1]
PINNED = REPO / "tools/synthetic-cohorts/manifest-v1.json"
GENERATOR = REPO / "tools/synthetic-cohorts/generate.mjs"
BINARY = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target")) / "release/trellis"
MANIFEST_SHA = "03faaaa04c61ba7c18051e12386201898cd31625758c36a0202ff8da65da8882"
POLL_S = .020
SETUP_TIMEOUT_S = 1800
SAMPLE_TIMEOUT_S = 45
FALLBACK_TIMEOUT_S = 180  # separate bounded full-native budget; historical medium full/cold 68.8836/76.441 s
MODE_NOTE = "mode inferred from timing; the daemon doesn't expose publication mode"
FACTS = """SELECT m.language,SUM(
 (SELECT count(*) FROM native_version_declarations d WHERE d.version_id=m.document_version_id)+
 (SELECT count(*) FROM native_version_calls c WHERE c.version_id=m.document_version_id)+
 (SELECT count(*) FROM native_version_control_regions r WHERE r.version_id=m.document_version_id))
 FROM revision_documents m WHERE m.revision_id=? GROUP BY m.language ORDER BY m.language"""
LANGUAGES = ("java", "javascript", "python", "rust")
PARITY = {
 "document": "SELECT m.language,m.path,v.content_hash,hex(v.source_bytes),v.native_witness FROM revision_documents m JOIN document_versions v ON v.id=m.document_version_id WHERE m.revision_id=? ORDER BY m.language,m.path",
 "declarations": "SELECT m.language,m.path,d.syntax_id,d.kind,d.name,d.lookup_key,d.start_byte,d.end_byte FROM revision_documents m JOIN native_version_declarations d ON d.version_id=m.document_version_id WHERE m.revision_id=? ORDER BY m.language,m.path,d.syntax_id",
 "calls": "SELECT m.language,m.path,c.id,c.owner_syntax_id,c.ordinal,c.start_byte,c.end_byte,c.callee_start,c.callee_end,c.spelling FROM revision_documents m JOIN native_version_calls c ON c.version_id=m.document_version_id WHERE m.revision_id=? ORDER BY m.language,m.path,c.id",
 "regions": "SELECT m.language,m.path,c.id,c.owner_syntax_id,c.ordinal,c.kind,c.start_byte,c.end_byte,c.parent_id,c.arm FROM revision_documents m JOIN native_version_control_regions c ON c.version_id=m.document_version_id WHERE m.revision_id=? ORDER BY m.language,m.path,c.id",
 "graph_projection": "SELECT m.language,m.path,g.graph_hash,g.class_extraction_state,g.class_extraction_payload FROM revision_documents m JOIN graph_projections g ON g.id=m.graph_projection_id WHERE m.revision_id=? ORDER BY m.language,m.path",
 "graph_nodes": "SELECT m.language,m.path,n.id,n.name,n.kind FROM revision_documents m JOIN graph_nodes n ON n.projection_id=m.graph_projection_id WHERE m.revision_id=? ORDER BY m.language,m.path,n.id",
 "graph_display_labels": "SELECT m.language,m.path,n.id,n.label FROM revision_documents m JOIN graph_display_labels n ON n.projection_id=m.graph_projection_id WHERE m.revision_id=? ORDER BY m.language,m.path,n.id",
 "classes": "SELECT m.language,m.path,n.id,n.payload FROM revision_documents m JOIN classes n ON n.projection_id=m.class_projection_id WHERE m.revision_id=? ORDER BY m.language,m.path,n.id",
 "class_relations": "SELECT m.language,m.path,n.id,n.payload FROM revision_documents m JOIN class_relations n ON n.projection_id=m.class_projection_id WHERE m.revision_id=? ORDER BY m.language,m.path,n.id",
}

class Invalid(RuntimeError):
    pass

class Interrupted(Invalid):
    pass

class PollFailure(Invalid):
    def __init__(self, message, polls):
        super().__init__(message)
        self.polls = polls

def need(condition, message):
    if not condition:
        raise Invalid(message)

def sha(data):
    return hashlib.sha256(data).hexdigest()

def file_sha(path):
    with open(path, "rb") as stream:
        digest = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()

def command(argv, *, env=None, timeout=30):
    return subprocess.run([str(a) for a in argv], cwd=REPO, env=env,
                          capture_output=True, text=True, timeout=timeout, check=True)

def append(log, kind, **values):
    log.write(json.dumps({"type": kind, "wall_ns": time.time_ns(), **values}, sort_keys=True) + "\n")
    log.flush()
    os.fsync(log.fileno())

def pinned():
    raw = PINNED.read_bytes()
    need(sha(raw) == MANIFEST_SHA, "canonical manifest SHA mismatch")
    doc = json.loads(raw)
    need(doc["version"] == 1 and doc["seed"] == "trellis-synthetic-cohorts-v1", "unexpected manifest")
    for size, count, byte_count in (("medium", 1000, 16777216), ("large", 10000, 134217728)):
        need(doc["totals"][size] == {"files": count, "sourceBytes": byte_count}, "cohort totals changed")
        rows = [row for row in doc["files"] if row["path"].startswith(size + "/")]
        need(len(rows) == count and {lang: sum(row["path"].startswith(f"{size}/{lang}/") for row in rows)
                                     for lang in LANGUAGES} == dict.fromkeys(LANGUAGES, count // 4),
             f"{size} language mix changed")
    return doc

def verify_files(root, rows):
    actual = {p.relative_to(root).as_posix() for p in root.rglob("*") if p.is_file()}
    expected = {r["path"] for r in rows}
    need(actual == expected, f"file-set mismatch at {root}: missing={list(expected-actual)[:3]} extra={list(actual-expected)[:3]}")
    for row in rows:
        p = root / row["path"]
        need(p.stat().st_size == row["sourceBytes"] and file_sha(p) == row["sha256"], f"source mismatch: {p}")

def private_scratch(value):
    path = Path(value).expanduser()
    need(path.is_absolute(), "--scratch must be absolute")
    path = path.resolve()
    need(path != REPO and REPO not in path.parents and path not in REPO.parents,
         "scratch must be outside checkout and its ancestors")
    return path

def env_for(home):
    home = home.resolve()
    need(home.is_dir() and home != Path.home().resolve() and REPO not in home.parents,
         "private HOME must be existing outside checkout and not real HOME")
    env = os.environ.copy()
    env.update(HOME=str(home), XDG_CACHE_HOME=str(home / ".cache"),
               XDG_DATA_HOME=str(home / ".local/share"), RUST_LOG="off")
    env.pop("TRELLIS_INDEX_DIAGNOSTICS", None)
    return env

def prepare(args):
    scratch = private_scratch(args.scratch)
    need(not scratch.exists(), "prepare requires absent scratch; never reuse a prior fixture")
    doc = pinned()
    scratch.mkdir(mode=0o700, parents=True)
    try:
        if args.build:
            command(["cargo", "build", "--locked", "--release"], timeout=1800)
        need(BINARY.is_file(), "release binary absent; pass --build to prepare explicitly")
        command(["node", GENERATOR, "--out", scratch / "canonical"], timeout=300)
        need((scratch / "canonical/manifest.json").read_bytes() == PINNED.read_bytes(), "generated manifest bytes differ")
        for size in ("medium", "large"):
            rows = [{**r, "path": r["path"].removeprefix(size + "/")}
                    for r in doc["files"] if r["path"].startswith(size + "/")]
            source = scratch / "canonical" / size
            verify_files(source, rows)
            shutil.copytree(source, scratch / f"{size}-workspace")
            verify_files(scratch / f"{size}-workspace", rows)
            (scratch / f"{size}-private-home").mkdir(mode=0o700)
        shutil.copytree(scratch / "canonical/medium", scratch / "fallback-workspace")
        (scratch / "fallback-private-home").mkdir(mode=0o700)
        (scratch / "cold-private-home").mkdir(mode=0o700)
        metadata = {"source_head": command(["git", "rev-parse", "HEAD"]).stdout.strip(),
                    "manifest_sha256": MANIFEST_SHA, "generator_sha256": file_sha(GENERATOR),
                    "binary_sha256": file_sha(BINARY), "binary": str(BINARY),
                    "runner_sha256": file_sha(Path(__file__)), "poll_seconds": POLL_S,
                    "source_note": "canonical generated file-list and per-file hashes: pinned manifest-v1.json"}
        (scratch / "prepared.json").write_text(json.dumps(metadata, sort_keys=True, indent=2) + "\n")
        print(json.dumps({"prepared": str(scratch), **metadata}, sort_keys=True))
    except BaseException:
        # Leave the prepared scratch for inspection; it cannot be mistaken for a complete fixture.
        raise

def db_for(home):
    found = list(home.rglob("index.db"))
    need(len(found) == 1 and "indexes" in found[0].parts, f"expected one private index.db: {found}")
    return found[0]

def db_open(home):
    return sqlite3.connect(f"file:{db_for(home)}?mode=ro", uri=True, timeout=5)

def pin_of(status):
    pin = status["revision"]
    need(isinstance(pin["indexRevision"], int) and pin["indexRevision"] > 0, "invalid status revision")
    return {"indexGeneration": pin["indexGeneration"], "indexRevision": pin["indexRevision"]}

def revision_id(pin):
    return f"pin:v1:{pin['indexGeneration']}:{pin['indexRevision']}"

def selected_check(home, pin, size, changed=None):
    revision = revision_id(pin)
    db = db_open(home)
    try:
        head = db.execute("SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1").fetchone()
        need(head == (pin["indexGeneration"], pin["indexRevision"]), "status pin not durable metadata head")
        need(db.execute("SELECT count(*) FROM native_revisions WHERE id=?", (revision,)).fetchone()[0] == 1,
             "missing selected native revision")
        count = db.execute("SELECT count(*) FROM revision_documents WHERE revision_id=?", (revision,)).fetchone()[0]
        need(count == (1000 if size == "medium" else 10000), f"incomplete selected documents: {count}")
        facts = dict(db.execute(FACTS, (revision,)).fetchall())
        need(set(facts) == set(LANGUAGES) and all(facts.values()), f"missing selected native language: {facts}")
        need((all(v >= 50000 for v in facts.values()) if size == "medium" else sum(facts.values()) >= 500000),
             f"selected native fact floor failed: {facts}")
        result = {"selected_revision_id": revision, "selected_documents": count,
                  "produced_native_facts": facts, "produced_native_facts_total": sum(facts.values())}
        if changed:
            path, language, raw = changed
            rows = db.execute("""SELECT v.content_hash,v.source_bytes FROM revision_documents m
                JOIN document_versions v ON v.id=m.document_version_id
                WHERE m.revision_id=? AND m.language=? AND m.path=?""", (revision, language, path)).fetchall()
            need(len(rows) == 1 and rows[0] == (sha(raw), raw), "edited selected source/hash not equal to written bytes")
            result.update(selected_source_sha256=sha(raw), selected_source_bytes=len(raw))
        return result
    finally:
        db.close()

def selected_reuse(home, prior, current, path, language, count):
    db = db_open(home)
    try:
        prior_id, current_id = revision_id(prior), revision_id(current)
        row = db.execute("""SELECT count(*),sum(a.document_version_id=b.document_version_id),
          sum(a.graph_projection_id=b.graph_projection_id),sum(a.class_projection_id IS b.class_projection_id),
          sum(CASE WHEN a.path=?3 AND a.language=?4 THEN 1 ELSE 0 END),
          sum(CASE WHEN a.path=?3 AND a.language=?4 AND a.document_version_id!=b.document_version_id
               AND a.graph_projection_id!=b.graph_projection_id
               AND a.class_projection_id IS NOT b.class_projection_id THEN 1 ELSE 0 END),
          sum(CASE WHEN NOT(a.path=?3 AND a.language=?4)
               AND (a.document_version_id!=b.document_version_id OR a.graph_projection_id!=b.graph_projection_id
                    OR a.class_projection_id IS NOT b.class_projection_id) THEN 1 ELSE 0 END)
          FROM revision_documents a JOIN revision_documents b USING(source_set_id,language,path)
          WHERE a.revision_id=?1 AND b.revision_id=?2""", (current_id, prior_id, path, language)).fetchone()
        need(row == (count, count-1, count-1, count-1, 1, 1, 0), f"not proven-local selected ID reuse: {row}")
        return dict(zip(("joined", "native_reused", "graph_reused", "class_reused",
                         "edited_key", "edited_changed", "other_changed"), row))
    finally:
        db.close()

def parity_digest(home, pin):
    db = db_open(home)
    result = {}
    try:
        for name, query in PARITY.items():
            digest = hashlib.sha256()
            count = 0
            for row in db.execute(query, (revision_id(pin),)):
                digest.update(json.dumps(row, ensure_ascii=False, separators=(",", ":")).encode() + b"\n")
                count += 1
            result[name] = {"rows": count, "sha256": digest.hexdigest()}
    finally:
        db.close()
    return result

def planned_edit(size, n):
    language = ("java", "python", "javascript", "rust")[n % 4]
    ordinal = n // 4
    suffix = {"java":"java", "python":"py", "javascript":"js", "rust":"rs"}[language]
    relative = f"{language}/C{size}{ordinal:04d}.{suffix}"
    if language == "rust":
        relative = f"rust/g00/C{size}{ordinal:04d}.rs"
    return language, relative, ("grow", "fixed", "grow", "identifier", "identifier")[ordinal]

def edit_bytes(raw, language, kind):
    if kind == "identifier":
        peer = b"peerValue" if language in ("java", "javascript") else b"peer_value"
        headers = {"java": b"public static int f1(int revision){\n",
                   "javascript": b"function f1(revision){\n",
                   "python": b"def f1(revision):\n",
                   "rust": b"pub fn f2(revision:i32)->i32{\n"}
        header = headers[language]
        need(raw.count(header) == 1, "ambiguous selected function definition")
        begin = raw.index(header) + len(header)
        end = raw.find(b"\ndef f2(" if language == "python" else b"\n}\n", begin)
        need(end > begin, "selected function body boundary missing")
        body = raw[begin:end]
        marker = (b"if revision&1==0{return local+" + peer + b"+f0()+"
                  if language == "rust" else b"return local+" + peer + b"+revision+f0()")
        need(body.count(marker) == 1, "ambiguous proven-local return expression")
        first = begin + body.index(marker) + (len(b"if revision&1==0{return ")
                                            if language == "rust" else len(b"return "))
        old, replacement = b"local", peer
    else:
        patterns = {"python":rb"def f0\(\): return (?P<value>\d+)",
                    "java":rb"public static int f0\(\)\{return (?P<value>\d+)",
                    "javascript":rb"export function f0\(\)\{return (?P<value>\d+)",
                    "rust":rb"pub fn f0\(\)->i32\{(?P<value>\d+)"}
        matches = list(re.finditer(patterns[language], raw))
        need(len(matches) == 1, "ambiguous numeric function body marker")
        first = matches[0].start("value")
        old = matches[0].group("value")
        replacement = str(int(old) + (10 ** len(old) if kind == "grow" else 1)).encode()
        need((len(replacement) > len(old)) == (kind == "grow"), "invalid grow/fixed edit width")
    need(raw[first:first+len(old)] == old and replacement != old, "invalid body edit")
    return raw[:first] + replacement + raw[first+len(old):], old.decode(), replacement.decode()

class Daemon:
    """One phase's Trellis daemon and serve, both children of this runner.

    The runner starts the daemon itself and starts serve only once that daemon
    listens, so serve attaches to it instead of starting a detached one. As
    children, neither PID can be reused until it is reaped here, so stop()
    signals exactly these processes. Construction has no side effects: assign
    the object first, then start(), so a failed start is still stopped."""
    GRACE_S = 20

    def __init__(self, scratch, label, workspace, home):
        self.workspace, self.home = workspace, home
        self.stderr_path = scratch / f"{label}-serve.stderr.log"
        self.daemon_stderr_path = scratch / f"{label}-daemon.stderr.log"
        self.argv = [str(BINARY), "serve", "--workspace", str(workspace), "--bind", "127.0.0.1:0",
                     "--token-file", str(scratch / f"{label}-token")]
        self.daemon = self.proc = None
        self.daemon_stderr = self.stderr = None
        self.url = None

    def start(self, listen_timeout_s=SETUP_TIMEOUT_S):
        env = env_for(self.home)
        self.daemon_stderr = open(self.daemon_stderr_path, "wb", buffering=0)
        self.daemon = subprocess.Popen([str(BINARY), "daemon"], cwd=REPO, env=env, stdin=subprocess.DEVNULL,
                                       stdout=subprocess.DEVNULL, stderr=self.daemon_stderr,
                                       start_new_session=True)
        deadline = time.monotonic() + listen_timeout_s
        while not self.listening():
            need(self.daemon.poll() is None, "runner's daemon exited before listening")
            need(time.monotonic() < deadline, "runner's daemon did not start listening")
            time.sleep(.05)
        self.stderr = open(self.stderr_path, "wb", buffering=0)
        self.proc = subprocess.Popen(self.argv, cwd=REPO, env=env, stdout=subprocess.DEVNULL,
                                     stderr=self.stderr, start_new_session=True)

    def listening(self):
        # The daemon binds its socket only after it holds the election. An lsof
        # that fails or hangs reads as "not yet", which the deadline bounds.
        try:
            result = subprocess.run(["lsof", "-a", "-p", str(self.daemon.pid), "-U", "-Fn"],
                                    capture_output=True, text=True, timeout=10)
        except (subprocess.TimeoutExpired, OSError):
            return False
        # Linux lsof appends " type=STREAM" to a socket's name; macOS does not.
        names = [line[1:].split(" type=")[0] for line in result.stdout.splitlines() if line.startswith("n")]
        return any(name.endswith("/daemon.sock") for name in names)

    def ready(self):
        deadline = time.monotonic() + SETUP_TIMEOUT_S
        while time.monotonic() < deadline:
            need(self.daemon.poll() is None, "runner's daemon exited")
            need(self.proc.poll() is None, f"serve exited: {self.stderr_path.read_text(errors='replace')[-3000:]}")
            match = re.search(r"Trellis: (http://127\.0\.0\.1:\d+)/", self.stderr_path.read_text(errors="replace"))
            if match:
                self.url = match.group(1)
                try:
                    with urlopen(self.url + "/healthz", timeout=3) as response:
                        if response.status == 200:
                            return
                except Exception:
                    pass
            time.sleep(.05)
        raise Invalid("serve readiness deadline exceeded")

    def stop(self):
        # Every step runs even if an earlier one fails or is interrupted; the
        # first error is raised at the end. Idempotent.
        errors = []
        for proc in (self.proc, self.daemon):
            if proc is not None:
                try:
                    stop_child(proc, self.GRACE_S)
                except BaseException as exc:
                    errors.append(exc)
        for handle in (self.stderr, self.daemon_stderr):
            if handle is not None:
                try:
                    handle.close()
                except BaseException as exc:
                    errors.append(exc)
        try:
            need(election_free(self.home), f"a daemon still holds the election for {self.home}")
        except BaseException as exc:
            errors.append(exc)
        if errors:
            raise errors[0]

def stop_child(proc, grace_s):
    # A child is reaped here, so its PID is never reused before the signal.
    # If the graceful wait is interrupted (a measurement signal raises inside
    # it), the child is still killed and reaped before the interrupt is
    # raised: SIGKILL cannot be ignored, so that wait is bounded.
    if proc.poll() is not None:
        return
    interrupted = None
    try:
        proc.terminate()
        proc.wait(timeout=grace_s)
    except subprocess.TimeoutExpired:
        pass
    except BaseException as exc:
        interrupted = exc
    if proc.poll() is None:
        proc.kill()
        proc.wait(timeout=10)
    if interrupted is not None:
        raise interrupted

def election_free(home):
    # True only if this process can take every daemon lock under the private
    # HOME: no daemon, ours or one serve started, still holds it.
    for lock in home.rglob("daemon.lock"):
        descriptor = os.open(lock, os.O_RDONLY)
        try:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return False
            fcntl.flock(descriptor, fcntl.LOCK_UN)
        finally:
            os.close(descriptor)
    return True

STATUS_CALL_S = 15

def read_status(workspace, home, timeout_s=STATUS_CALL_S):
    # One status call, no retry. Raises CalledProcessError, TimeoutExpired or
    # a decode/validation error; the timed sample polling uses only this.
    result = command([BINARY, "status", "--workspace", workspace], env=env_for(home), timeout=timeout_s)
    return pin_of(json.loads(result.stdout)), result.stdout

def baseline_status(workspace, home, log, phase):
    # Untimed baseline setup only: serve announces readiness before the first
    # head publishes, and a cold publication can stall a status call or make it
    # fail with a typed error. Retry both until one monotonic setup deadline,
    # each call bounded by min(15 s, the time left). Each attempt is journaled
    # as it finishes (setup_status_attempt), whatever happens next; the
    # attempts are also returned for the setup record's summary.
    deadline = time.monotonic() + SETUP_TIMEOUT_S
    attempts = []
    def record(began, outcome):
        attempt = {"attempt": len(attempts) + 1, "seconds": round(time.monotonic() - began, 3), "outcome": outcome}
        attempts.append(attempt)
        append(log, "setup_status_attempt", phase=phase, **attempt)
    while True:
        left = deadline - time.monotonic()
        if left <= 0:
            raise Invalid(f"{phase} baseline status not ready within {SETUP_TIMEOUT_S}s after "
                          f"{len(attempts)} attempts (each journaled as setup_status_attempt)")
        began = time.monotonic()
        try:
            pin, raw = read_status(workspace, home, timeout_s=min(STATUS_CALL_S, left))
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as exc:
            record(began, "timeout" if isinstance(exc, subprocess.TimeoutExpired)
                   else f"exit {exc.returncode}: {(exc.stderr or '').strip()[-200:]}")
        except BaseException as exc:
            record(began, f"{type(exc).__name__}: {str(exc)[:200]}")
            raise
        else:
            record(began, "ok")
            return pin, raw, attempts
        time.sleep(max(0, min(1, deadline - time.monotonic())))

WITNESS_FIELDS = {"epoch": int, "native": bool, "degraded": bool, "degradations": int}

def watcher_witness(workspace, home):
    # The daemon's watcher-health witness, read synchronously with one CLI
    # status call: the current watcher epoch, whether it holds a native
    # watcher, whether it is degraded, and a count of every transition to
    # degraded. Two equal readings mean watching stayed healthy between them;
    # the degraded bit alone could have returned to healthy unseen. None if
    # the witness cannot be read, which never counts as healthy.
    try:
        result = command([BINARY, "status", "--workspace", workspace], env=env_for(home), timeout=30)
        witness = json.loads(result.stdout).get("watcher")
    except (subprocess.SubprocessError, OSError, ValueError, AttributeError):
        return None
    if not isinstance(witness, dict) or set(witness) != set(WITNESS_FIELDS):
        return None
    if not all(type(witness[key]) is kind for key, kind in WITNESS_FIELDS.items()):
        return None
    return witness

def healthy_baseline(witness):
    return witness is not None and witness["native"] is True and witness["degraded"] is False

def stayed_healthy(baseline, witness):
    return healthy_baseline(baseline) and witness == baseline

def series_invalid(log, size, reason, sample_index=None):
    append(log, "series_invalid", size=size, sample_index=sample_index, reason=reason)
    raise RuntimeError(f"{size} series invalid: {reason}; restart it when watching is healthy")

def wait_selected(workspace, home, previous, write_done_ns, timeout_s=SAMPLE_TIMEOUT_S):
    polls = []
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        began = time.monotonic_ns()
        try:
            # One call, never the setup retry; bounded by the sample's budget.
            pin, raw = read_status(workspace, home,
                                   timeout_s=max(0.001, min(STATUS_CALL_S, deadline - time.monotonic())))
            ended = time.monotonic_ns()
            entry = {"start_ns": began, "return_ns": ended, "pin": pin, "status_sha256": sha(raw.encode())}
            polls.append(entry)
            if pin["indexGeneration"] != previous["indexGeneration"]:
                raise PollFailure("generation changed while watching edit", polls)
            if pin["indexRevision"] > previous["indexRevision"]:
                if pin["indexRevision"] != previous["indexRevision"] + 1:
                    raise PollFailure("intervening revision(s) not observed", polls)
                return pin, ended, polls
            if pin != previous:
                raise PollFailure("status moved backwards or returned unexpected pin", polls)
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired, json.JSONDecodeError) as exc:
            polls.append({"start_ns": began, "return_ns": time.monotonic_ns(), "error": str(exc)[:600]})
        remaining = (began + int(POLL_S * 1e9) - time.monotonic_ns()) / 1e9
        if remaining > 0:
            time.sleep(remaining)
    raise PollFailure(f"new selected revision not visible within {timeout_s}s; attempts={len(polls)}", polls)

def machine():
    need(platform.system() == "Darwin", "Stage3 reference host must be Darwin")
    model = command(["sysctl", "-n", "hw.model"]).stdout.strip()
    need(model == "Mac17,16", f"wrong reference machine: {model}")
    return {"host": model, "chip": "Apple M5 Pro", "uname": platform.uname()._asdict(),
            "uptime": command(["uptime"]).stdout.strip(),
            "ps": command(["ps", "-axo", "pid,ppid,pcpu,comm"]).stdout[-30000:],
            "disk_free_bytes": shutil.disk_usage(REPO).free}

def frozen_workspace(scratch, size, doc):
    rows = [{**r, "path": r["path"].removeprefix(size + "/")}
            for r in doc["files"] if r["path"].startswith(size + "/")]
    verify_files(scratch / f"{size}-workspace", rows)

def measure(args):
    need(args.exclusive_slot_authorization and args.exclusive_slot_authorization.strip()
         and args.exclusive_slot_authorization != "TODO", "explicit exclusive-slot authorization string required")
    scratch = private_scratch(args.scratch)
    prepared = json.loads((scratch / "prepared.json").read_text())
    need(prepared["manifest_sha256"] == MANIFEST_SHA and prepared["generator_sha256"] == file_sha(GENERATOR)
         and prepared["binary_sha256"] == file_sha(BINARY) and prepared["binary"] == str(BINARY)
         and prepared["runner_sha256"] == file_sha(Path(__file__)), "prepared artifacts/binary/runner changed")
    need(prepared["source_head"] == command(["git", "rev-parse", "HEAD"]).stdout.strip(), "source HEAD changed")
    doc = pinned()
    for size in ("medium", "large"):
        frozen_workspace(scratch, size, doc)
    verify_files(scratch / "fallback-workspace", [{**r, "path": r["path"].removeprefix("medium/")}
        for r in doc["files"] if r["path"].startswith("medium/")])
    host = machine()
    events = scratch / "watcher-events.jsonl"
    need(not events.exists(), "measurement journal already exists; no retry/overwrite of partial run")
    active = None
    def interrupted(signum, frame):
        raise Interrupted(f"signal {signum} interrupted measurement")
    old_handlers = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGINT, signal.SIGTERM)}
    with events.open("x", encoding="utf-8") as log:
        try:
            append(log, "run_header", authorization=args.exclusive_slot_authorization,
                   prepared=prepared, machine=host, poll_interval_ms=20,
                   debounce_quiet_ms=75, debounce_max_wait_ms=250,
                   status_timeout_seconds=SAMPLE_TIMEOUT_S,
                   fallback_status_timeout_seconds=FALLBACK_TIMEOUT_S,
                   corpus_manifest=str(PINNED), corpus_manifest_sha256=MANIFEST_SHA,
                   mode_note=MODE_NOTE, mode_directly_observed=False,
                   script_sha256=file_sha(Path(__file__)))
            for size in ("medium", "large"):
                workspace = scratch / f"{size}-workspace"
                home = scratch / f"{size}-private-home"
                active = Daemon(scratch, size, workspace, home)
                active.start()
                active.ready()
                previous, baseline_json, setup_attempts = baseline_status(workspace, home, log, size)
                baseline = selected_check(home, previous, size)
                append(log, "setup", size=size, serve_pid=active.proc.pid, serve_argv=active.argv,
                       serve_stderr=str(active.stderr_path), daemon_pid=active.daemon.pid,
                       daemon_stderr=str(active.daemon_stderr_path), url=active.url,
                       baseline_pin=previous, baseline_status_sha256=sha(baseline_json.encode()),
                       baseline_selected=baseline, cold_setup_timing="excluded",
                       setup_status_attempts=setup_attempts)
                # Untimed setup: the current watcher's native setup publishes
                # shortly after its epoch begins.
                watching = watcher_witness(workspace, home)
                settle = time.monotonic() + SETUP_TIMEOUT_S
                while (watching is not None and not watching["native"] and not watching["degraded"]
                       and time.monotonic() < settle):
                    time.sleep(.05)
                    watching = watcher_witness(workspace, home)
                append(log, "series_watcher", size=size, witness=watching,
                       native=healthy_baseline(watching))
                if not healthy_baseline(watching):
                    series_invalid(log, size, "no healthy native watcher before the series")
                edited = set()
                for n in range(20):
                    language, relative, kind = planned_edit(size, n)
                    sample = {"size": size, "sample_index": n + 1, "language": language,
                              "edit_kind": kind, "path": relative, "prior_pin": previous,
                              "poll_interval_ms": 20, "status_timeout_seconds": SAMPLE_TIMEOUT_S,
                              "mode_expected": "local", "mode_note": MODE_NOTE,
                              "mode_directly_observed": False}
                    try:
                        need(relative not in edited, "repeat path in local series")
                        edited.add(relative)
                        path = workspace / relative
                        before = path.read_bytes()
                        after, old, new = edit_bytes(before, language, kind)
                        sample.update(old_literal=old, new_literal=new, before_sha256=sha(before),
                                      after_sha256=sha(after))
                        stat = path.stat()
                        write_start_ns = time.monotonic_ns()
                        path.write_bytes(after)  # same pathname/inode; never atomic rename
                        write_done_ns = time.monotonic_ns()
                        sample.update(write_start_ns=write_start_ns, write_done_ns=write_done_ns,
                                      same_inode=(path.stat().st_ino == stat.st_ino))
                        need(sample["same_inode"], "write replaced source inode")
                        pin, seen_ns, polls = wait_selected(workspace, home, previous, write_done_ns)
                        sample.update(current_pin=pin, first_status_return_ns=seen_ns,
                                      latency_seconds=(seen_ns-write_done_ns)/1e9, polls=polls,
                                      status_polls=len(polls))
                        sample["selected"] = selected_check(home, pin, size, (relative, language, after))
                        sample["reuse"] = selected_reuse(home, previous, pin, relative, language,
                                                         1000 if size == "medium" else 10000)
                        need(path.read_bytes() == after, "source changed during selected checks")
                        need(active.proc.poll() is None, "serve died during watcher sample")
                        need(active.daemon.poll() is None, "runner's daemon died during watcher sample")
                        sample["valid"] = True
                        previous = pin
                    except BaseException as exc:
                        if isinstance(exc, PollFailure):
                            sample["polls"] = exc.polls
                            sample["status_polls"] = len(exc.polls)
                        sample["valid"] = False
                        sample["failure"] = f"{type(exc).__name__}: {exc}"
                        append(log, "sample", **sample)
                        raise
                    witness = watcher_witness(workspace, home)
                    sample["watcher_witness"] = witness
                    sample["watcher_degraded_after"] = witness is None or witness["degraded"]
                    # A new epoch or degradation since the series began means
                    # watching may not have stayed healthy, even if it reads
                    # healthy again now.
                    sample["watcher_witness_ok"] = stayed_healthy(watching, witness)
                    append(log, "sample", **sample)
                    if sample["watcher_degraded_after"] or not sample["watcher_witness_ok"]:
                        series_invalid(log, size, "watcher not continuously healthy during the series",
                                       n + 1)
                active.stop()
                active = None
            # Fallback is a distinct untouched medium workspace and independent setup.
            workspace = scratch / "fallback-workspace"
            home = scratch / "fallback-private-home"
            active = Daemon(scratch, "fallback", workspace, home)
            active.start()
            active.ready()
            prior, baseline_json, setup_attempts = baseline_status(workspace, home, log, "fallback-medium")
            baseline = selected_check(home, prior, "medium")
            append(log, "setup", size="fallback-medium", serve_pid=active.proc.pid,
                   serve_argv=active.argv, serve_stderr=str(active.stderr_path),
                   daemon_pid=active.daemon.pid, daemon_stderr=str(active.daemon_stderr_path), url=active.url,
                   baseline_pin=prior, baseline_status_sha256=sha(baseline_json.encode()),
                   baseline_selected=baseline, cold_setup_timing="excluded",
                   setup_status_attempts=setup_attempts)
            path = workspace / "python/Cmedium0000.py"
            before = path.read_bytes()
            needle = b"def f0(): return 1"
            need(before.count(needle) == 1, "fresh declaration edit marker missing")
            after = before.replace(needle, b"def f0(revision=0): return 1", 1)
            fallback = {"size":"medium", "path":"python/Cmedium0000.py",
                        "edit_kind":"declaration-surface", "prior_pin":prior,
                        "before_sha256":sha(before), "after_sha256":sha(after),
                        "poll_interval_ms":20, "status_timeout_seconds":FALLBACK_TIMEOUT_S,
                        "mode_expected":"full", "mode_note":MODE_NOTE,
                        "mode_directly_observed":False}
            try:
                stat = path.stat()
                write_start_ns = time.monotonic_ns()
                path.write_bytes(after)
                write_done_ns = time.monotonic_ns()
                need(path.stat().st_ino == stat.st_ino, "fallback replaced source inode")
                pin, seen_ns, polls = wait_selected(workspace, home, prior, write_done_ns,
                                                    timeout_s=FALLBACK_TIMEOUT_S)
                fallback.update(write_start_ns=write_start_ns, write_done_ns=write_done_ns,
                                first_status_return_ns=seen_ns, latency_seconds=(seen_ns-write_done_ns)/1e9,
                                current_pin=pin, polls=polls,
                                selected=selected_check(home, pin, "medium", ("python/Cmedium0000.py", "python", after)))
                need(path.read_bytes() == after, "fallback source changed unexpectedly")
                need(active.proc.poll() is None, "serve died during fallback")
                need(active.daemon.poll() is None, "runner's daemon died during fallback")
                active.stop()
                active = None
                # An independent cold watcher-driven Serve on SAME root, different private index state;
                # its untimed result is only a parity oracle, never a counted fallback sample.
                cold_home = scratch / "cold-private-home"
                cold_start_ns = time.monotonic_ns()
                active = Daemon(scratch, "cold-oracle", workspace, cold_home)
                active.start()
                active.ready()
                cold_pin, cold_json, cold_attempts = baseline_status(workspace, cold_home, log, "cold-oracle")
                cold_selected_ns = time.monotonic_ns()
                fallback["independent_cold_start_ns"] = cold_start_ns
                fallback["independent_cold_first_status_return_ns"] = cold_selected_ns
                fallback["independent_cold_start_to_selected_seconds"] = (cold_selected_ns-cold_start_ns)/1e9
                fallback["independent_cold_timing_note"] = "process-start to first selected status, not a counted watcher sample"
                fallback["independent_cold_selected"] = selected_check(cold_home, cold_pin, "medium",
                    ("python/Cmedium0000.py", "python", after))
                fallback["independent_cold_status_sha256"] = sha(cold_json.encode())
                fallback["independent_cold_status_attempts"] = cold_attempts
                fallback["warm_digest"] = parity_digest(home, pin)
                fallback["cold_digest"] = parity_digest(cold_home, cold_pin)
                need(fallback["warm_digest"] == fallback["cold_digest"], "FULL fallback vs cold native/graph/class parity failed")
                fallback["parity"] = True
                fallback["valid"] = True
            except BaseException as exc:
                if isinstance(exc, PollFailure):
                    fallback["polls"] = exc.polls
                    fallback["status_polls"] = len(exc.polls)
                fallback["valid"] = False
                fallback["failure"] = f"{type(exc).__name__}: {exc}"
                append(log, "fallback", **fallback)
                raise
            append(log, "fallback", **fallback)
            active.stop()
            active = None
        except BaseException as exc:
            append(log, "failure", message=f"{type(exc).__name__}: {exc}",
                   note="partial samples retained, never replace or drop")
            raise
        finally:
            close_out(log, events, active, old_handlers)
    print(json.dumps({"events": str(events), "sha256": file_sha(events)}, sort_keys=True))

def close_out(log, events, active, old_handlers):
    # Teardown, handler restoration and the summary each run even if an
    # earlier one fails. A teardown failure is journaled as a failure (so the
    # gate fails) and raised after the summary.
    teardown = None
    if active is not None:
        try:
            active.stop()
        except BaseException as exc:
            teardown = exc
            try:
                append(log, "failure", message=f"teardown: {type(exc).__name__}: {exc}",
                       note="phase processes may remain; never replace or drop")
            except Exception:
                pass
    for sig, previous in old_handlers.items():
        signal.signal(sig, previous)
    # Even interrupted runs have an explicit conservative summary.
    try:
        records = [json.loads(line) for line in events.read_text().splitlines()]
        append(log, "summary", **summarize_records(records))
    except Exception as exc:
        append(log, "summary", pass_gate=False, error=f"summarizer failed: {exc}")
    if teardown is not None:
        raise teardown

def summarize_records(records):
    series = {}
    failures = [r for r in records if r["type"] == "failure"]
    invalid = [r for r in records if r["type"] == "series_invalid"]
    for size in ("medium", "large"):
        samples = [r for r in records if r["type"] == "sample" and r.get("size") == size]
        ordered = [planned_edit(size, n)[1] for n in range(20)]
        baseline = [r for r in records if r["type"] == "series_watcher" and r.get("size") == size]
        # Healthy native watching must be proven for the series and for every
        # sample, from the recorded witnesses: a sample without the proof does
        # not count (owner ruling, Decision 0009 §5).
        healthy = (len(baseline) == 1 and healthy_baseline(baseline[0].get("witness"))
                   and all(r.get("watcher_degraded_after") is False
                           and r.get("watcher_witness_ok") is True
                           and stayed_healthy(baseline[0]["witness"], r.get("watcher_witness"))
                           for r in samples))
        complete = (len(samples) == 20 and [r["path"] for r in samples] == ordered and healthy
                    and all(r.get("valid") is True and r.get("mode_expected") == "local"
                            and r.get("mode_note") == MODE_NOTE
                            and r.get("mode_directly_observed") is False for r in samples))
        times = [r["latency_seconds"] for r in samples if r.get("valid") is True]
        p95 = sorted(times)[18] if complete else None
        limit = 2 if size == "medium" else 5
        series[size] = {"count":len(samples), "valid":sum(r.get("valid") is True for r in samples),
                        "healthy_watching_proven":healthy,
                        "raw_latencies_seconds":times, "p95_nearest_rank_seconds":p95,
                        "limit_seconds":limit, "pass_gate":bool(complete and p95 <= limit),
                        "mode_expected":"local", "mode_note":MODE_NOTE,
                        "mode_directly_observed":False,
                        "failed_samples":[{"sample_index":r["sample_index"], "failure":r.get("failure")}
                                          for r in samples if not r.get("valid")]}
    fallbacks = [r for r in records if r["type"] == "fallback"]
    local_gate = (len(records) > 0 and records[0]["type"] == "run_header"
                  and not failures and not invalid
                  and all(r["pass_gate"] for r in series.values()))
    fallback_parity = (len(fallbacks) == 1 and fallbacks[0].get("valid") is True
                       and fallbacks[0].get("parity") is True
                       and fallbacks[0].get("mode_expected") == "full"
                       and fallbacks[0].get("mode_note") == MODE_NOTE
                       and fallbacks[0].get("mode_directly_observed") is False
                       and isinstance(fallbacks[0].get("independent_cold_start_to_selected_seconds"), (int, float)))
    warm = fallbacks[0].get("latency_seconds") if len(fallbacks) == 1 else None
    cold = fallbacks[0].get("independent_cold_start_to_selected_seconds") if len(fallbacks) == 1 else None
    return {"series":series, "fallback_count":len(fallbacks),
            "fallback_raw_seconds":[r.get("latency_seconds") for r in fallbacks],
            "fallback_independent_cold_raw_seconds":
                [r.get("independent_cold_start_to_selected_seconds") for r in fallbacks],
            "fallback_warm_to_cold_ratio":warm/cold if warm is not None and cold and cold > 0 else None,
            "fallback_timing_methods":"warm: file-write to watcher selected status; cold: process start to first selected status; not equivalent intervals",
            "fallback_parity":fallback_parity, "fallback_report_only":True,
            "fallback_latency_limit_seconds":None, "fallback_mode_expected":"full",
            "fallback_mode_directly_observed":False, "fallback_mode_note":MODE_NOTE,
            "failures":[r.get("message") for r in failures],
            "invalid_series":[{"size":r.get("size"), "sample_index":r.get("sample_index"),
                               "reason":r.get("reason")} for r in invalid],
            "local_watcher_latency_gate_pass":local_gate,
            "pass_gate":bool(local_gate and fallback_parity), "owner_review_required":True,
            "note":"Only the local series has latency thresholds. Fallback timing and same-root cold parity are report-only indirect mode evidence; no CLI index samples counted. Owner reviews raw logs before Gate 3."}


def summarize(args):
    events = Path(args.events)
    need(events.is_file(), "JSONL events file absent")
    records = [json.loads(line) for line in events.read_text().splitlines()]
    result = summarize_records([r for r in records if r.get("type") != "summary"])
    result["events_sha256"] = file_sha(events)
    print(json.dumps(result, sort_keys=True, indent=2))

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--prepare", action="store_true")
    mode.add_argument("--measure", action="store_true")
    mode.add_argument("--summarize", action="store_true")
    parser.add_argument("--scratch", help="absent external private fixture directory (--prepare/--measure)")
    parser.add_argument("--build", action="store_true", help="--prepare only: build release before freezing SHA")
    parser.add_argument("--exclusive-slot-authorization", help="--measure only: operator grant of exclusive reference-host slot")
    parser.add_argument("--events", help="--summarize only: completed or partial JSONL path")
    args = parser.parse_args()
    if args.prepare:
        need(bool(args.scratch) and not args.exclusive_slot_authorization and not args.events, "prepare argument mismatch")
        prepare(args)
    elif args.measure:
        need(bool(args.scratch) and not args.build and not args.events, "measure argument mismatch")
        measure(args)
    else:
        need(bool(args.events) and not args.scratch and not args.build and not args.exclusive_slot_authorization,
             "summarize argument mismatch")
        summarize(args)

if __name__ == "__main__":
    try:
        main()
    except (Invalid, OSError, subprocess.SubprocessError, sqlite3.Error, KeyboardInterrupt) as exc:
        print(f"Stage3 FAILED: {exc}", file=sys.stderr)
        sys.exit(1)
