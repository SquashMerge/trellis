#!/usr/bin/env python3
"""#225 single-file LOCAL gate on the frozen #67 mixed-language corpus.

Twenty observations per edit family; time from immediately before the edit
through the whole real CLI return, including watcher debounce. Verify the actual
LOCAL watcher/request publisher and complete selected FULL parity per family.
Failed evidence is retained.
"""
import argparse
from contextlib import closing, suppress
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import signal
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import importlib.util

REPO = Path(__file__).resolve().parents[1]
DECODER_VERSION = "scip-0.10.0/CapturedScipFactV1/2"
SCHEMA_VERSION = 33
PINNED = REPO / "tools/synthetic-cohorts/manifest-v2.json"
GENERATOR = REPO / "tools/synthetic-cohorts/generate.mjs"
BATCH_MODULE = REPO / "tools/semantic-contract/semantic_batch.py"
TARGET_DIR = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target"))
if not TARGET_DIR.is_absolute():
    TARGET_DIR = REPO / TARGET_DIR
BINARY = TARGET_DIR / "release/trellis"
DEADLINE = time.monotonic() + 1700  # unchanged global command budget
EVIDENCE = None
COMMAND_NUMBER = 0
FACTS = """SELECT m.language,SUM(
 (SELECT count(*) FROM native_version_declarations d WHERE d.vid=m.vid)+
 (SELECT count(*) FROM native_version_calls c WHERE c.vid=m.vid)+
 (SELECT count(*) FROM native_version_control_regions r WHERE r.vid=m.vid))
 FROM revision_documents m WHERE m.revision_id=? GROUP BY m.language ORDER BY m.language"""


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def record(kind, **fields):
    entry = {"event": kind, **fields}
    print(json.dumps(entry, sort_keys=True), flush=True)
    if EVIDENCE is not None:
        with (EVIDENCE / "observations.jsonl").open("a") as log:
            log.write(json.dumps(entry, sort_keys=True) + "\n")


def stop_child(proc, grace=20):
    if proc.poll() is not None:
        proc.wait()
        return
    interrupted = None
    try:
        proc.terminate()
        proc.wait(timeout=grace)
    except subprocess.TimeoutExpired:
        pass
    except BaseException as exc:
        interrupted = exc
    if proc.poll() is None:
        proc.kill()
        proc.wait(timeout=10)
    if interrupted is not None:
        raise interrupted


def maxrss_bytes(usage):
    # ru_maxrss is KiB on Linux and bytes on macOS.
    return usage.ru_maxrss * 1024 if sys.platform.startswith("linux") else usage.ru_maxrss


def reap_with_rusage(proc, grace=20):
    """Terminate and reap `proc` with wait4 so its resource usage is kept."""
    usage = None
    status = None
    killed = False
    deadline = time.monotonic() + grace
    # Signal by pid: Popen.terminate()/kill() call poll(), which would reap the child
    # before wait4 and lose its resource usage.
    with suppress(ProcessLookupError):
        os.kill(proc.pid, signal.SIGTERM)
    while True:
        pid, status, usage = os.wait4(proc.pid, os.WNOHANG)
        if pid:
            break
        if time.monotonic() >= deadline and not killed:
            with suppress(ProcessLookupError):
                os.kill(proc.pid, signal.SIGKILL)
            killed = True
        time.sleep(.02)
    proc.returncode = os.waitstatus_to_exitcode(status)
    return {"maxRssBytes": maxrss_bytes(usage)}


def command(args, *, env=None, timed=False, rusage=None):
    global COMMAND_NUMBER
    remain = DEADLINE - time.monotonic()
    require(remain > 10, "global 1700-second budget exhausted")
    COMMAND_NUMBER += 1
    prefix = EVIDENCE / f"command-{COMMAND_NUMBER:03d}"
    record("command_start", argv=[str(arg) for arg in args], log=str(prefix))
    # Real files preserve partial output on timeout, signal, or failed assertion.
    with prefix.with_suffix(".stdout").open("wb") as stdout, prefix.with_suffix(".stderr").open("wb") as stderr:
        start = time.monotonic()
        proc = subprocess.Popen(args, cwd=REPO, env=env, stdout=stdout, stderr=stderr,
                                stdin=subprocess.DEVNULL)
        try:
            if rusage is None:
                proc.wait(timeout=min(remain, 1700))
            else:
                limit = start + min(remain, 1700)
                while True:
                    pid, status, usage = os.wait4(proc.pid, os.WNOHANG)
                    if pid:
                        break
                    if time.monotonic() >= limit:
                        raise subprocess.TimeoutExpired(args, min(remain, 1700))
                    time.sleep(.01)
                proc.returncode = os.waitstatus_to_exitcode(status)
                rusage["maxRssBytes"] = maxrss_bytes(usage)
            ended = time.monotonic()
            ended_wall_ms = time.time_ns() / 1_000_000
            duration = ended - start
        finally:
            stop_child(proc)
    output = prefix.with_suffix(".stdout").read_text(errors="replace")
    errors = prefix.with_suffix(".stderr").read_text(errors="replace")
    record("command_finish", argv=[str(arg) for arg in args], seconds=duration, returncode=proc.returncode)
    require(proc.returncode == 0, f"{args} failed ({proc.returncode}): {errors[-2500:]} {output[-1000:]}")
    return (duration, output, errors, ended, ended_wall_ms) if timed else output


def env_for(home):
    env = os.environ.copy()
    env.update(HOME=str(home), XDG_CACHE_HOME=str(home / ".cache"),
               XDG_DATA_HOME=str(home / ".local/share"), RUST_LOG="off",
               TRELLIS_INDEX_DIAGNOSTICS="1")
    return env


def election_free(home):
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


def stop_home_processes(home, binary):
    """Clean up CLI-forked replacements, including before exec/election.

    Called only after the CLI has been terminated/reaped and our known daemon
    child stopped. No process can create another detached daemon now. A single
    HOME-scoped snapshot also finds a pre-exec child still displaying CLI args.
    Unknown children belong to the departed CLI: observe exit, never claim reap.
    """
    home_token = re.compile(r"(?:^| )HOME=" + re.escape(str(home))
                            + r"(?= [A-Za-z_][A-Za-z0-9_]*=|$)")

    def candidates(pid=None):
        args = ["/bin/ps", "eww"]
        args += ["-p", str(pid)] if pid is not None else ["-A"]
        args += ["-o", "pid=", "-o", "stat=", "-o", "command="]
        result = subprocess.run(args, capture_output=True, text=True, timeout=10)
        require(result.returncode == 0 or (pid is not None and result.returncode == 1),
                f"cannot enumerate private HOME daemon processes (ps exit {result.returncode}): {result.stderr.strip()}")
        found = set()
        for line in result.stdout.splitlines():
            fields = line.strip().split(None, 2)
            if len(fields) != 3:
                continue
            raw_pid, state, command_line = fields
            if (not state.startswith("Z") and command_line.startswith(str(binary) + " ")
                    and home_token.search(command_line)):
                found.add(int(raw_pid))
        return found

    remaining = candidates()
    if not remaining:
        return
    record("unexpected_home_processes", pids=sorted(remaining), home=str(home), attempt_valid=False)
    for pid in remaining:
        if candidates(pid):
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
    deadline = time.monotonic() + 20
    while remaining and time.monotonic() < deadline:
        remaining = {pid for pid in remaining if candidates(pid)}
        if remaining:
            time.sleep(.05)
    for pid in remaining:
        if candidates(pid):
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    deadline = time.monotonic() + 10
    while remaining and time.monotonic() < deadline:
        remaining = {pid for pid in remaining if candidates(pid)}
        if remaining:
            time.sleep(.05)
    require(not remaining and election_free(home), "private HOME processes did not exit/release election")
    record("replacement_stopped", home=str(home), election_free=True, reaped=False,
           note="CLI descendant, not our child; exit observed, cannot reap")


class Daemon:
    """Own only the production release daemon; CLI attaches to its listening socket."""
    def __init__(self, binary, home, label, record_rusage=False):
        self.binary, self.home = binary, home
        self.path = EVIDENCE / f"{label}-daemon.stderr.log"
        self.proc = self.log = None
        self.record_rusage = record_rusage
        self.rusage = None

    def start(self):
        self.log = self.path.open("ab", buffering=0)
        self.proc = subprocess.Popen([str(self.binary), "daemon"], cwd=REPO,
                                     env=env_for(self.home), stdin=subprocess.DEVNULL,
                                     stdout=subprocess.DEVNULL, stderr=self.log,
                                     start_new_session=True)
        deadline = min(DEADLINE - 10, time.monotonic() + 60)
        while True:
            require(self.proc.poll() is None, f"daemon exited before ready: {self.path}")
            require(time.monotonic() < deadline, "owned daemon readiness deadline exceeded")
            result = subprocess.run(["lsof", "-a", "-p", str(self.proc.pid), "-U", "-Fn"],
                                    capture_output=True, text=True, timeout=10)
            names = [line[1:].split(" type=")[0] for line in result.stdout.splitlines()
                     if line.startswith("n")]
            sockets = sorted({name for name in names if name.endswith("/daemon.sock")})
            if sockets:
                require(len(sockets) == 1, f"ambiguous owned daemon sockets: {sockets}")
                record("daemon_ready", pid=self.proc.pid, stderr=str(self.path))
                return
            time.sleep(.05)  # socket readiness only, never mode/publication proof

    def stop(self):
        # Finish bounded TERM/kill/reap/election proof even if the run's
        # deadline fires during a per-shape stop, then restore run handlers.
        handlers = {sig: signal.signal(sig, signal.SIG_IGN)
                    for sig in (signal.SIGALRM, signal.SIGTERM, signal.SIGINT)}
        errors = []
        try:
            try:
                if self.proc is not None:
                    if self.record_rusage:
                        self.rusage = reap_with_rusage(self.proc)
                    else:
                        stop_child(self.proc)
            except BaseException as exc:
                errors.append(exc)
            try:
                if self.log is not None:
                    self.log.close()
            except BaseException as exc:
                errors.append(exc)
            try:
                # Always enumerate after CLI reap, even if election is free:
                # a just-forked replacement may not have taken it yet.
                stop_home_processes(self.home, self.binary)
                require(election_free(self.home), f"daemon election still held: {self.home}")
                record("daemon_stopped", home=str(self.home), election_free=True)
            except BaseException as exc:
                errors.append(exc)
        finally:
            for sig, handler in handlers.items():
                signal.signal(sig, handler)
        if errors:
            raise errors[0]


def db_for(home):
    # HOME and both XDG directories are private scratch. Never use real user state.
    found = list(home.rglob("index.db"))
    require(len(found) == 1 and "indexes" in found[0].parts,
            f"expected exactly one isolated index.db, found {found}")
    return found[0]


def published(db, output, size):
    require(db.execute("PRAGMA user_version").fetchone()[0] == SCHEMA_VERSION,
            f"mixed performance requires schema{SCHEMA_VERSION}")
    index_result = json.loads(output)
    pin = index_result["publishedRevision"]
    revision = f"pin:v1:{pin['indexGeneration']}:{pin['indexRevision']}"
    head = db.execute("SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1").fetchone()
    require(head == (pin["indexGeneration"], pin["indexRevision"]),
            f"{size}: CLI pin differs from durable index_metadata")
    require(db.execute("SELECT count(*) FROM native_revisions WHERE id=?", (revision,)).fetchone()[0] == 1,
            f"{size}: missing selected revision header")
    counts = dict(db.execute(FACTS, (revision,)).fetchall())
    require(len(counts) == 4 and all(counts.get(k, 0) > 0 for k in
            ("java", "javascript", "python", "rust")), f"{size}: missing produced language: {counts}")
    floor = 50_000 if size == "medium" else 500_000
    require(all(v >= floor for v in counts.values()) if size == "medium" else sum(counts.values()) >= floor,
            f"{size} run {revision}: OWN selected produced-index facts below floor: {counts}")
    require(db.execute("SELECT count(*) FROM revision_documents WHERE revision_id=?", (revision,)).fetchone()[0]
            == (1000 if size == "medium" else 10000), f"{size}: incomplete selected manifest")
    return {"revision": revision, "produced_native_facts": counts,
            "produced_native_facts_total": sum(counts.values())}


def check_selected_local_ids(db, prior, current, changed_path, size, n, shape):
    expected_docs = 1000 if size == "medium" else 10000
    prior_count = db.execute(
        "SELECT count(*) FROM revision_documents WHERE revision_id=?", (prior,)
    ).fetchone()[0]
    # Join by the FULL selected document key. Counts alone could
    # hide a second changed path whose IDs coincidentally offset.
    identity = db.execute("""SELECT count(*),
        sum(a.vid=b.vid),
        sum(a.gid=b.gid),
        sum(a.cid IS b.cid),
        sum(CASE WHEN a.path=?3 THEN 1 ELSE 0 END),
        sum(CASE WHEN a.path=?3
            AND a.vid!=b.vid
            AND a.gid!=b.gid
            THEN 1 ELSE 0 END),
        sum(CASE WHEN a.path!=?3
            AND (a.vid!=b.vid
                 OR a.gid!=b.gid)
            THEN 1 ELSE 0 END)
        FROM revision_documents a JOIN revision_documents b
        USING(language,path)
        WHERE a.revision_id=?1 AND b.revision_id=?2""",
        (current, prior, changed_path)).fetchone()
    reuse = dict(zip(("documents", "native_versions", "graph_projections",
                      "class_projections"), identity[:4]))
    require(prior_count == expected_docs and reuse["documents"] == expected_docs
            and identity[4:] == (1, 1, 0)
            and all(reuse[name] == expected_docs - 1 for name in
                ("native_versions", "graph_projections")),
            f"{size} run {n}: full-key prior/selected ID proof failed: "
            f"prior={prior_count}, reuse={reuse}, target/changed/other={identity[4:]}")
    if shape == "body":
        require(reuse["class_projections"] == expected_docs - 1,
                f"{size} body run {n}: unchanged class IDs not reused: {reuse}")
    # Declaration/catalog edits can legitimately refresh unchanged class
    # projections at cap boundaries; native/graph reuse remains mandatory.
    return reuse



def markers(path):
    result = []
    pattern = r"^maintenance wall_ms=(\d+) monotonic_us=(\d+) incarnation=(\S+) event=(\S+) detail=(.*?) marker_lost=(\d+)$"
    for line in path.read_text(errors="replace").splitlines():
        match = re.fullmatch(pattern, line)
        if match:
            wall, monotonic, incarnation, event, detail, lost = match.groups()
            require(int(lost) == 0, f"diagnostic marker loss in {path}: {line}")
            result.append({"incarnation": incarnation, "event": event,
                           "detail": dict(re.findall(r"(\w+)=([^ ]+)", detail)),
                           "monotonic_us": int(monotonic), "wall_ms": int(wall)})
        else:
            require(not any(event in line for event in ("event=index_request_", "event=index_watcher_complete")),
                    f"malformed publication diagnostic: {line}")
    return result


def request_rows(home):
    paths = list(home.rglob("requests.db"))
    require(len(paths) <= 1, f"ambiguous request databases: {paths}")
    if not paths:
        return {}
    with closing(sqlite3.connect(f"file:{paths[0]}?mode=ro", uri=True)) as db:
        return {row[0]: row[1:] for row in db.execute(
            "SELECT id,state,result_generation,result_revision,claim_incarnation,error_code FROM requests")}


def writer_metrics(detail):
    families = ("manifest", "native", "graph", "class")
    writer = {name: {"rows": int(detail[f"{name}_rows"]),
                     "bind_bytes": int(detail[f"{name}_bind_bytes"])}
              for name in (*families, "total")}
    require(writer["total"] == {key: sum(writer[name][key] for name in families)
                                 for key in ("rows", "bind_bytes")},
            f"writer family accounting mismatch: {writer}")
    reused = int(detail["reused_occurrence_reads"])
    require(reused == 0, "unchanged occurrence was read under publish transaction")
    return writer, reused


def sample_publication(log_path, *, generation, revision, incarnation, request,
                       request_mode, start_wall_ms, cli_end_wall_ms):
    """Floor-free exact-pin attribution shared by the small runtime probe."""
    # An unchanged FIFO completion is not a publishing completion. Bind
    # exactly one actual request/watcher publisher to the CLI's same pin.
    deadline = min(DEADLINE - 10, time.monotonic() + 5)
    while True:
        publications = [m for m in markers(log_path)
                        if m["event"] in ("index_request_complete", "index_watcher_complete")
                        and m["detail"].get("generation") == generation
                        and m["detail"].get("revision") == str(revision)
                        and m["detail"].get("mode") != "unchanged"]
        if publications:
            break
        require(time.monotonic() < deadline, "missing exact-pin publishing completion")
        time.sleep(.01)  # diagnostic delivery only, not publication completion
    require(len(publications) == 1, f"ambiguous publishing completions: {publications}")
    publication = publications[0]
    detail = publication["detail"]
    require(detail.get("mode") == "local", f"selected publication is not proven LOCAL: {publication}")
    require(int(start_wall_ms) <= publication["wall_ms"] <= int(cli_end_wall_ms),
            "publishing completion lies outside edit-to-CLI observation window")
    require(publication["incarnation"] == incarnation, "publisher/request incarnation mismatch")
    publisher = "watcher" if publication["event"] == "index_watcher_complete" else "request"
    if publisher == "request":
        require(detail.get("request") == request and request_mode == "local",
                "publishing completion belongs to a different request")
    else:
        require(request_mode == "unchanged", "watcher and request both claim publication work")
    writer, reused_reads = writer_metrics(detail)
    return {"publisher": publisher, "publication_mode": "local", "writer_bind": writer,
            "reused_occurrence_reads": reused_reads}


def index(binary, root, daemon, size, *, cold_setup=False, warm_setup=None, sample=None):
    require(sum((bool(cold_setup), warm_setup is not None, sample is not None)) <= 1,
            "cold setup, warm activation and timed edit contexts must be separate")
    before = sample["before_requests"] if sample else request_rows(daemon.home)
    if warm_setup is not None:
        with closing(sqlite3.connect(f"file:{db_for(daemon.home)}?mode=ro", uri=True)) as db:
            generation, number = db.execute(
                "SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1").fetchone()
        require(f"pin:v1:{generation}:{number}" == warm_setup,
                "warm activation must start from the frozen prior pin")
    if cold_setup:
        require(not before and not list(daemon.home.rglob("index.db")),
                "cold setup requires virgin index state and an empty durable request queue before activation")
        record("virgin_cold_before_activation", home=str(daemon.home), index_absent=True, requests=0)
    elapsed, output, _, cli_end, cli_end_wall_ms = command(
        [str(binary), "index", "--workspace", str(root)], env=env_for(daemon.home), timed=True)
    seconds = cli_end - sample["start"] if sample else elapsed
    record("cli_returned", seconds=seconds, cli_seconds=elapsed, output=json.loads(output),
           daemon_stderr=str(daemon.path))
    pin = json.loads(output)["publishedRevision"]
    after = request_rows(daemon.home)
    added = set(after) - set(before)
    require(len(added) == 1, f"CLI did not create exactly one durable request: {added}")
    request = added.pop()
    state, generation, revision, incarnation, error = after[request]
    require((state, generation, revision, error) ==
            ("done", pin["indexGeneration"], pin["indexRevision"], None),
            f"request {request} terminal row does not bind CLI pin: {after[request]}")
    # Only bounded delivery wait for the asynchronous diagnostic writer. Never
    # poll progress or retry a publication/no-op to obtain a passing sample.
    deadline = min(DEADLINE - 10, time.monotonic() + 5)
    while True:
        all_markers = markers(daemon.path)
        own = [m for m in all_markers if m["detail"].get("request") == request]
        completed = [m for m in own if m["event"] == "index_request_complete"]
        if completed:
            break
        require(time.monotonic() < deadline, f"missing completion diagnostic for {request}")
        time.sleep(.01)
    require(len(completed) == 1, f"duplicate request completion: {request}")
    complete = completed[0]["detail"]
    require(all(m["incarnation"] == incarnation for m in own), "request claim/diagnostic incarnation mismatch")
    require((complete["generation"], int(complete["revision"])) == (generation, revision),
            "completion diagnostic pin mismatch")
    progress = [m["detail"] for m in own if m["event"] == "index_request_progress"]
    modes = [p["phase"][5:] for p in progress if p["phase"].startswith("mode:")]
    if cold_setup:
        allowed_modes = ("full", "unchanged")
    elif warm_setup is not None:
        allowed_modes = ("unchanged",)
    else:
        allowed_modes = ("local", "full", "unchanged") if sample else ("local", "full")
    require(len(modes) == 1 and modes[0] in allowed_modes and complete["mode"] == modes[0],
            f"request {request} has invalid actual mode: {modes}, {complete}")
    phases = {}
    for item in progress:
        if item["phase"].startswith("timing:"):
            name = item["phase"][7:]
            require(name not in phases and int(item["total"]) == 1, f"invalid duplicate timing phase: {item}")
            phases[name] = int(item["completed"]) / 1000
    required = {"capture", "publish"} if modes[0] == "unchanged" else {"capture", "measure", "attest", "publish"}
    if modes[0] == "unchanged":
        require(set(phases) == required, f"unchanged request reported publication work: {phases}")
    if modes[0] == "local":
        required.add("compose")
    require(required <= phases.keys(), f"missing actual request timings: {phases}")
    require(all(0 <= value <= elapsed * 1000 for value in phases.values()),
            f"phase timing outside full CLI wall time: {phases}, {elapsed}")
    request_writer = request_reads = None
    if modes[0] == "unchanged":
        require(set(complete) == {"request", "generation", "revision", "mode"},
                f"unchanged request must not carry stale writer metrics: {complete}")
    else:
        request_writer, request_reads = writer_metrics(complete)
    writer, reused_reads = request_writer, request_reads
    publisher = None
    publication_mode = modes[0]
    if sample:
        require(f"pin:v1:{generation}:{revision}" == sample["expected_revision"],
                "sample did not select exactly the next revision")
        attribution = sample_publication(
            daemon.path, generation=generation, revision=revision, incarnation=incarnation,
            request=request, request_mode=modes[0], start_wall_ms=sample["start_wall_ms"],
            cli_end_wall_ms=cli_end_wall_ms)
        publisher = attribution["publisher"]
        writer = attribution["writer_bind"]
        reused_reads = attribution["reused_occurrence_reads"]
        publication_mode = attribution["publication_mode"]
    with closing(sqlite3.connect(f"file:{db_for(daemon.home)}?mode=ro", uri=True)) as db:
        selected = published(db, output, size)
        if warm_setup is not None:
            require(selected["revision"] == warm_setup,
                    "untimed warm activation changed the frozen prior pin; never retry or advance it")
        if cold_setup:
            require(revision == 1 and db.execute("SELECT count(*) FROM native_revisions").fetchone()[0] == 1,
                    "virgin cold setup must select its sole first publication")
            # Mandatory startup runs before FIFO. With no prior revision, both
            # unchanged-head reuse and LOCAL preparation reject, so this sole
            # publication is necessarily FULL, even when FIFO then reuses it.
            options = json.loads(db.execute("SELECT reconcile_options FROM index_metadata WHERE singleton=1").fetchone()[0])
            require(options == {"version": 1, "maxFileBytes": 2 * 1024 * 1024,
                                "scipPath": None, "manifestPath": None},
                    f"cold setup capture options differ from unchanged CLI defaults: {options}")
    cold_origin = {"publication_origin": "virgin_first_full", "cold_setup": True} if cold_setup else {}
    activation = {"warm_setup": True, "expected_prior_revision": warm_setup} if warm_setup is not None else {}
    window = {"edit_start_wall_ms": sample["start_wall_ms"], "cli_end_wall_ms": cli_end_wall_ms} if sample else {}
    return seconds, {**selected, **cold_origin, **activation, **window, "request": request, "claim_incarnation": incarnation,
                     "mode": modes[0], "request_mode": modes[0], "phases_ms": phases,
                     "request_writer_bind": request_writer, "request_reused_occurrence_reads": request_reads,
                     "publisher": publisher, "publication_mode": publication_mode,
                     "writer_bind": writer, "cli_seconds": elapsed,
                     "cli_unaccounted_ms": elapsed * 1000 - sum(phases.values()),
                     "reused_occurrence_reads": reused_reads}


def edit_leaf(path, language, kind):
    """One proven method/function-body edit, never a declaration or import.

    Every timed revision has its own changed path and full produced-fact checks.
    A returned identifier is accepted only when both names are local bindings.
    """
    raw = path.read_bytes()
    if kind == "identifier":
        peer = b"peerValue" if language in ("java", "javascript") else b"peer_value"
        headers = {"java": b"public static int f1(int revision){\n",
                   "javascript": b"function f1(revision){\n",
                   "python": b"def f1(revision):\n",
                   "rust": b"pub fn f2(revision:i32)->i32{\n"}
        begin = raw.index(headers[language]) + len(headers[language])
        end = raw.find(b"\ndef f2(" if language == "python" else b"\n}\n", begin)
        require(end > begin, f"body boundary absent: {path}")
        body = raw[begin:end]
        marker = (b"if revision&1==0{return local+" + peer + b"+f0()+"
                  if language == "rust" else b"return local+" + peer + b"+revision+f0()")
        require(body.count(marker) == 1, f"unique non-numeric local return absent: {path}")
        first = begin + body.index(marker) + (len(b"if revision&1==0{return ")
                                              if language == "rust" else len(b"return "))
        old, replacement = b"local", peer
    else:
        patterns = {
            "python": rb"def f0\(\): return (?P<value>\d+)",
            "java": rb"public static int f0\(\)\{return (?P<value>\d+)",
            "javascript": rb"export function f0\(\)\{return (?P<value>\d+)",
            "rust": rb"pub fn f0\(\)->i32\{(?P<value>\d+)",
        }
        matches = list(re.finditer(patterns[language], raw))
        require(len(matches) == 1, f"one declared numeric body return required: {path}")
        match = matches[0]
        first = match.start("value")
        old = match.group("value")
        replacement = str(int(old) + (10 ** len(old) if kind == "grow" else 1)).encode()
        require(kind in ("grow", "fixed") and old != replacement,
                f"numeric body edit must not be a no-op: {path}")
        require((len(replacement) > len(old)) == (kind == "grow"),
                f"grow/fixed body length contract failed: {path}")
    require(raw[first:first+len(old)] == old, f"body edit boundary moved: {path}")
    path.write_bytes(raw[:first] + replacement + raw[first+len(old):])
    return old.decode(), replacement.decode()



def edit_surface(path, language, shape, n):
    raw = path.read_bytes()
    if shape == "declaration-surface":
        patterns = {"java": b"int f1(int revision)", "python": b"def f1(revision)",
                    "javascript": b"function f1(revision)", "rust": b"fn f1(revision:i32)"}
        old = patterns[language]
        new = old.replace(b"f1(", b"f1_surface(")
    elif language == "java":
        # Alternate a class member signature and a base/interface surface.
        old = b"public int m()" if n % 4 == 0 else b"implements I" + str(n // 2).encode() + b"{"
        new = b"public int projectedMember()" if n % 4 == 0 else old.replace(b"implements ", b"implements java.io.Serializable, ")
    else:
        old = b"class Derived0(Base0):" if n % 4 == 1 else b"    def m(self): return super().m()"
        new = b"class Derived0(Base0, ProjectedBase):" if n % 4 == 1 else b"    def projected_member(self): return super().m()"
    require(raw.count(old) == 1 and old != new, f"ambiguous/no-op {shape} edit: {path}, {old!r}")
    path.write_bytes(raw.replace(old, new, 1))
    return old.decode(), new.decode()


def planned_edit(size, shape, n):
    languages = ("java", "python") if shape == "class-projection" else ("java", "python", "javascript", "rust")
    language = languages[n % len(languages)]
    ordinal = n // len(languages)
    suffix = {"java": "java", "python": "py", "javascript": "js", "rust": "rs"}[language]
    relative = Path("rust/g00" if language == "rust" else language) / f"C{size}{ordinal:04d}.{suffix}"
    return language, relative, ("grow", "fixed", "grow", "identifier", "identifier")[ordinal] if shape == "body" else shape


def check_changed_source(db, revision, path, raw):
    rows = db.execute("""SELECT v.content_hash,v.source_bytes FROM revision_documents m
        JOIN document_versions v ON v.vid=m.vid WHERE m.revision_id=? AND m.path=?""",
        (revision, path)).fetchall()
    require(rows == [(hashlib.sha256(raw).hexdigest(), raw)], "selected source bytes do not match timed edit")


def class_output(db, revision, path):
    extraction = db.execute("""SELECT g.class_extraction_state,g.class_extraction_payload
        FROM revision_documents m JOIN graph_projections g ON g.gid=m.gid
        WHERE m.revision_id=? AND m.path=?""", (revision, path)).fetchall()
    classes = db.execute("""SELECT c.id,c.payload FROM revision_documents m JOIN classes c ON c.cid=m.cid
        WHERE m.revision_id=? AND m.path=? ORDER BY c.id""", (revision, path)).fetchall()
    relations = db.execute("""SELECT c.id,c.payload FROM revision_documents m JOIN class_relations c ON c.cid=m.cid
        WHERE m.revision_id=? AND m.path=? ORDER BY c.id""", (revision, path)).fetchall()
    return extraction, classes, relations


# Port of tests/index/index_atomic.rs::sqlite_snapshot: all selected evidence columns,
# schema-33 tables, immutable surrogate keys, and current pin normalization.
# Match tests/daemon/cli.rs: check FTS shadow inventory, compare logical corpus cells.
REQUIRED_TABLES = set("""
native_producers native_producer_languages native_producer_inputs
native_source_sets native_source_set_languages native_source_set_dependencies
native_revisions revision_capture_inputs document_versions
graph_projections graph_nodes graph_display_labels
class_projections classes class_relations
revision_documents native_version_coverage_roles native_version_declarations
native_version_search_cards head_search_fts
native_version_declaration_ancestors native_version_own_signature_types native_version_ancestor_signature_types
native_version_headers native_version_header_items native_version_parameters
native_version_references native_version_capability_families native_version_capability_gaps native_version_macro_expansions
native_version_calls native_version_control_regions native_version_call_regions
native_revision_release_debt native_release_candidate_versions native_release_candidate_graphs
native_release_candidate_classes semantic_producers semantic_producer_languages
semantic_revision_producers semantic_requested_producers semantic_requested_producer_languages
semantic_coverage semantic_coverage_roles semantic_coverage_reasons semantic_basis
semantic_basis_lookup_dependencies semantic_provenance semantic_symbols
semantic_symbol_targets semantic_declaration_bindings semantic_declaration_binding_symbols
semantic_declaration_join_candidates semantic_references semantic_reference_roles
semantic_reference_candidates semantic_call_bindings semantic_call_candidates
semantic_call_possible_dispatch semantic_call_join_candidates semantic_type_relationships
semantic_raw_batches semantic_raw_captures semantic_raw_exclusions semantic_revision_raw_selections
semantic_raw_proofs semantic_raw_documents semantic_import_reports
""".split())
CONTROL_TABLES = {"native_revision_supersessions", "native_binding_epoch", "revision_producer_bindings"}
FTS_SHADOW_TABLES = {"head_search_fts_config", "head_search_fts_content", "head_search_fts_data",
                     "head_search_fts_docsize", "head_search_fts_idx"}
DEBT_TABLES = {"native_revision_release_debt", "native_release_candidate_versions",
               "native_release_candidate_graphs", "native_release_candidate_classes"}


def typed_cells(value):
    if isinstance(value, bytes):
        return {"blob": value.hex()}
    if isinstance(value, dict):
        return {key: typed_cells(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [typed_cells(item) for item in value]
    return value


def selected_where(table):
    """The per-table scope of the selected pin; every schema table must be classified."""
    selected = "SELECT tuple_id FROM semantic_coverage WHERE revision_id=?"
    if table == "native_revisions":
        return "id=?"
    elif table == "head_search_fts":
        # Read the entire corpus: filtering by selected vid would hide stale rows.
        return "? IS NOT NULL"
    elif table in {"revision_capture_inputs", "revision_documents", "semantic_revision_producers",
                   "semantic_requested_producers", "semantic_requested_producer_languages",
                   "semantic_coverage", "semantic_revision_raw_selections"}:
        return "revision_id=?"
    elif table == "native_source_sets":
        return "id=(SELECT source_set_id FROM native_revisions WHERE id=?)"
    elif table in {"native_source_set_languages", "native_source_set_dependencies"}:
        return "source_set_id=(SELECT source_set_id FROM native_revisions WHERE id=?)"
    elif table.startswith("native_producer"):
        id_col, ver_col = (("id", "version") if table == "native_producers" else
                           ("producer_id", "producer_version"))
        return ("EXISTS(SELECT 1 FROM document_versions v JOIN revision_documents d ON d.vid=v.vid "
                f"WHERE d.revision_id=? AND v.producer_id=selected.{id_col} AND "
                f"v.producer_version=selected.{ver_col})")
    elif table.startswith("semantic_producer"):
        id_col, ver_col = (("id", "version") if table == "semantic_producers" else
                           ("id", "version"))
        return ("EXISTS(SELECT 1 FROM semantic_revision_producers r WHERE r.revision_id=? "
                f"AND r.id=selected.{id_col} AND r.version=selected.{ver_col})")
    elif table == "document_versions" or table.startswith("native_version_"):
        return "vid IN (SELECT vid FROM revision_documents WHERE revision_id=?)"
    elif table in {"graph_projections", "graph_nodes", "graph_display_labels"}:
        return "gid IN (SELECT gid FROM revision_documents WHERE revision_id=?)"
    elif table in {"class_projections", "classes", "class_relations"}:
        return "cid IN (SELECT cid FROM revision_documents WHERE revision_id=?)"
    elif table in {"semantic_coverage_roles", "semantic_coverage_reasons", "semantic_basis",
                   "semantic_provenance",
                   "semantic_symbols", "semantic_declaration_bindings", "semantic_references",
                   "semantic_call_bindings", "semantic_type_relationships"}:
        return f"tuple_id IN ({selected})"
    elif table in {"semantic_basis_lookup_dependencies"}:
        return f"basis_id IN (SELECT basis_id FROM semantic_basis WHERE tuple_id IN ({selected}))"
    elif table in {"semantic_symbol_targets"}:
        return f"symbol_id IN (SELECT symbol_id FROM semantic_symbols WHERE tuple_id IN ({selected}))"
    elif table in {"semantic_declaration_binding_symbols", "semantic_declaration_join_candidates"}:
        return f"binding_id IN (SELECT binding_id FROM semantic_declaration_bindings WHERE tuple_id IN ({selected}))"
    elif table in {"semantic_reference_roles", "semantic_reference_candidates"}:
        return f"tuple_id IN ({selected})"
    elif table in {"semantic_call_candidates", "semantic_call_possible_dispatch", "semantic_call_join_candidates"}:
        return f"binding_id IN (SELECT binding_id FROM semantic_call_bindings WHERE tuple_id IN ({selected}))"
    elif table in {"semantic_raw_batches", "semantic_raw_captures", "semantic_raw_exclusions"}:
        return "batch_id IN (SELECT batch_id FROM semantic_revision_raw_selections WHERE revision_id=?)"
    elif table in {"semantic_raw_documents", "semantic_raw_proofs"}:
        return ("capture_id IN (SELECT c.capture_id FROM semantic_raw_captures c "
                "JOIN semantic_revision_raw_selections s ON s.batch_id=c.batch_id WHERE s.revision_id=?)")
    elif table == "semantic_import_reports":
        return ("EXISTS(SELECT 1 FROM semantic_revision_raw_selections r "
                "JOIN semantic_revision_raw_selections current ON current.batch_id=r.batch_id "
                "AND current.producer_id=r.producer_id "
                "WHERE current.revision_id=? AND r.revision_id=selected.revision_id "
                "AND r.producer_id=selected.producer_id)")
    else:
        raise RuntimeError(f"unclassified schema-{SCHEMA_VERSION} selected table: {table}")


def parity_digest(db, revision, physical_root):
    view = SelectedView(db, revision, physical_root)
    require(db.execute("PRAGMA user_version").fetchone()[0] == SCHEMA_VERSION,
            f"selected parity requires schema{SCHEMA_VERSION}")
    generation, number = revision.removeprefix("pin:v1:").rsplit(":", 1)
    require(db.execute("SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1").fetchone()
            == (generation, int(number)), "parity pin is not frozen head")
    require(db.execute("PRAGMA foreign_key_check").fetchone() is None, "retained evidence foreign-key violation")
    tables = {row[0] for row in db.execute("""SELECT name FROM sqlite_master WHERE type='table'
        AND name NOT LIKE 'sqlite_%' AND name!='index_metadata' AND name NOT LIKE '%trigrams%'""")}
    expected_tables = REQUIRED_TABLES | CONTROL_TABLES | FTS_SHADOW_TABLES
    require(tables == expected_tables,
            f"schema{SCHEMA_VERSION} selected inventory mismatch: missing={expected_tables-tables}, "
            f"extra={tables-expected_tables}")
    output = {}
    # FTS shadow segment bytes depend on publication history, not selected output.
    for table in sorted(tables - CONTROL_TABLES - DEBT_TABLES - FTS_SHADOW_TABLES):
        where = selected_where(table)
        columns = [row[1] for row in db.execute(f'PRAGMA table_info("{table}")')]
        require(columns, f"empty table schema: {table}")
        digest, count = hashlib.sha256(), 0
        # Each row retains all columns; sorted hashes preserve multiplicity without
        # depending on SQLite rowids, insertion order, or root-derived IDs.
        rows = []
        for row in db.execute(f'SELECT * FROM "{table}" selected WHERE {where}', (revision,)):
            cells = {column: value for column, value in zip(columns, row)}
            cells = view.row(table, cells)
            encoded = canonical(typed_cells(cells))
            rows.append(hashlib.sha256(encoded).digest())
            count += 1
        for encoded_hash in sorted(rows):
            digest.update(encoded_hash)
        output[table] = {"columns": columns, "rows": count, "sha256": digest.hexdigest()}
    require(output["native_revisions"]["rows"] == 1
            and output["revision_documents"]["rows"] in (1000, 10000),
            "selected native corpus incomplete")
    # Exact logical corpus equality, including multiplicity, as in tests/daemon/cli.rs.
    # Card keys are unique; equal counts plus both set differences reject missing,
    # extra, duplicate, stale-version, or altered-term FTS rows.
    require(output["head_search_fts"]["rows"] == output["native_version_search_cards"]["rows"],
            "logical head search corpus/card count mismatch")
    cards = ("SELECT terms,vid,syntax_id FROM native_version_search_cards WHERE vid IN "
             "(SELECT vid FROM revision_documents WHERE revision_id=?)")
    corpus = "SELECT terms,vid,syntax_id FROM head_search_fts"
    for query in (f"{corpus} EXCEPT {cards}", f"{cards} EXCEPT {corpus}"):
        require(db.execute(query, (revision,)).fetchone() is None,
                "logical head search corpus differs from selected version cards")
    # Root-derived opaque hashes cannot be compared byte-for-byte across roots.
    # Bind each normalized role to a complete, independently hashed input closure;
    # every input table above retains every selected row and column.
    closures = {
        "document_version_id": ("document_versions", "native_version_coverage_roles",
            "native_version_capability_families", "native_version_capability_gaps",
            "native_version_macro_expansions"),
        "extraction_context": ("native_producers", "native_producer_languages",
            "native_producer_inputs", "native_revisions", "revision_capture_inputs"),
        "native_witness": tuple(sorted(name for name in output if name.startswith("native_version_")))
            + ("document_versions",),
        "graph_hash": ("graph_projections", "graph_nodes", "graph_display_labels",
            "native_version_declarations", "native_version_calls", "native_version_references",
            "native_version_control_regions", "native_version_call_regions"),
        "class_content_hash": ("class_projections", "classes", "class_relations",
            "graph_projections", "graph_nodes", "native_version_declarations"),
    }
    output["opaque_input_closures"] = {
        role: {name: output[name] for name in tables} for role, tables in closures.items()}
    require(all(output[name]["rows"] > 0 for name in
                ("document_versions", "native_version_declarations", "native_version_calls",
                 "native_version_references", "native_version_control_regions", "graph_nodes",
                 "graph_projections", "class_projections", "classes")),
            "opaque hash input closure missing selected evidence")
    require(output["semantic_import_reports"]["rows"] == 1,
            "selected capture import report missing or ambiguous")
    require(output["semantic_raw_proofs"]["rows"] > 0
            and output["semantic_provenance"]["rows"] > 0
            and output["semantic_coverage"]["rows"] > 0, "no selected semantic/raw-linked evidence")
    return output



def canonical(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()


def field(number, value, wire=2):
    def varint(n):
        out = bytearray()
        while n > 127:
            out.append((n & 127) | 128)
            n >>= 7
        out.append(n)
        return bytes(out)
    return varint(number * 8 + wire) + (varint(value) if wire == 0 else
                                         varint(len(value)) + value)


def scip_anchor(anchor, relative):
    raw = anchor.read_bytes()
    declaration = re.search(rb"export function f0\(\)", raw)
    require(declaration is not None, f"missing frozen f0 declaration: {anchor}")
    line = raw[:declaration.start()].count(b"\n")
    column = declaration.start() - raw.rfind(b"\n", 0, declaration.start()) - 1 + len(b"export function ")
    symbol = b"pkg f0()."
    occurrence = (field(1, b"".join(field(1, n, 0)[1:] for n in (line, column, column + 2)))
                  + field(2, symbol) + field(3, 1, 0))
    information = field(1, symbol) + field(5, 26, 0) + field(6, b"f0")
    document = (field(1, relative.as_posix().encode()) + field(2, occurrence)
                + field(3, information) + field(4, b"javascript") + field(6, 2, 0))
    return field(2, document)


def capture_header(db, revision, expected_rows=None):
    header = db.execute("""SELECT source_set_id,toolchain_hash,config_hash,dependency_hash,
        native_revision_id,source_inventory FROM native_revisions WHERE id=?""", (revision,)).fetchone()
    require(header is not None, "missing retained r1 header")
    source_set, toolchain, config, dependency, native_id, inventory = header
    rows = json.loads(inventory)
    source_rows = [{"language": item["key"]["language"], "relativePath": item["key"]["path"],
                    "contentHash": item["contentHash"]} for item in rows]
    if expected_rows is not None:
        require(len(source_rows) == expected_rows,
                f"r1 must contain exactly {expected_rows} sources: {source_rows}")
    manifest_rows = [{"document": {"sourceSetId": source_set, "language": row["language"],
                                    "path": row["relativePath"]}, "contentHash": row["contentHash"]}
                     for row in source_rows]
    root_id = db.execute("SELECT root_id FROM native_source_sets WHERE id=?", (source_set,)).fetchone()[0]
    languages = db.execute("SELECT language FROM native_source_set_languages WHERE source_set_id=? ORDER BY ordinal",
                           (source_set,)).fetchall()
    return {"source_set": {"id": source_set, "rootId": root_id,
                            "languages": [row[0] for row in languages], "dependencies": []},
            "native_id": native_id, "source_rows": source_rows,
            "manifest_hash": hashlib.sha256(canonical(manifest_rows)).hexdigest(),
            "components": {"toolchainHash": toolchain, "configHash": config, "dependencyHash": dependency}}


def capture_r1(db, revision):
    return capture_header(db, revision, 4)


def selected_proof(db, revision, anchor_path, artifact_hash):
    rows = db.execute("""SELECT s.producer_id,c.artifact_hash,s.captured_pin,p.proof_id,
        v.derived_from_provenance_id,v.derived_from_fact_digest,b.artifact_hash,
        cov.state,cov.selected,cov.requested,b.captured_native_revision_id,r.native_revision_id
        FROM semantic_revision_raw_selections s
        JOIN semantic_raw_captures c ON c.batch_id=s.batch_id
        JOIN semantic_raw_proofs p ON p.capture_id=c.capture_id AND p.path=?
        JOIN semantic_coverage cov ON cov.revision_id=s.revision_id AND cov.producer_id=s.producer_id
            AND cov.path=p.path AND cov.language=p.language
        JOIN semantic_provenance v ON v.tuple_id=cov.tuple_id AND v.capture_id=c.capture_id
            AND v.derived_from_provenance_id=p.proof_id AND v.derived_from_fact_digest=p.fact_digest
        JOIN semantic_basis b ON b.basis_id=v.basis_id AND b.tuple_id=cov.tuple_id
        JOIN native_revisions r ON r.id=s.revision_id
        WHERE s.revision_id=?""", (anchor_path, revision)).fetchall()
    require(rows and all(row[0] == "scip-typescript" and row[1] == artifact_hash
                         and row[3] == row[4] and row[6] == artifact_hash
                         and row[7] in ("complete", "partial") and row[8:10] == (1, 1)
                         for row in rows),
            f"{revision}: selected raw-linked proof, basis, coverage or distinct r1/r2 missing: {rows}")
    require(db.execute("SELECT count(*) FROM native_revisions WHERE id=?", (revision,)).fetchone()[0] == 1,
            "selected raw proof lacks native pin")
    require(db.execute("SELECT count(*) FROM semantic_revision_raw_selections WHERE revision_id=? "
                       "AND captured_pin!=revision_id", (revision,)).fetchone()[0] == 1,
            "selected capture does not retain distinct authenticated r1 pin")
    result = {"pin": revision, "producer": rows[0][0], "artifact_hash": artifact_hash,
              "captured_pin": rows[0][2], "proof_count": len({r[3] for r in rows}), "raw_proof_ids": sorted({r[3] for r in rows})}
    record("selected_proof", **result)
    return result


def load_batch_module():
    spec = importlib.util.spec_from_file_location("semantic_batch", BATCH_MODULE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def write_import_batch(root, header, artifact_bytes, covered):
    """Write a one-artifact batch under root/.trellis-semantic-batch/.

    `header` is the `capture_header` result; `covered` is the list of its source
    rows that the artifact covers. Source byte lengths come from the files in `root`.
    Returns (descriptor_path, descriptor_sha256).
    """
    profile = json.loads((REPO / "docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json").read_text())
    producer = next(p for p in profile["semanticProducers"] if p["name"] == "scip-typescript")
    batch = load_batch_module()
    producer_languages = ["javascript"]
    descriptor_header = {
        "trustOrigin": "referenceProfile", "profileId": profile["profileId"],
        "decoderVersion": DECODER_VERSION,
        "producer": {"name": producer["name"], "version": producer["version"],
                     "identityKind": "executable", "executableHash": producer["executableHash"],
                     "positionEncoding": 2, "languages": producer_languages},
        "sourceSet": header["source_set"], "sourceRevision": header["native_id"],
        "sourceManifestHash": header["manifest_hash"], "components": header["components"],
        "metadata": {"protocolVersion": 0, "projectRoot": Path(root).as_uri(),
                     "textDocumentEncoding": 1},
    }
    sources = sorted(({"byteLength": (Path(root) / row["relativePath"]).stat().st_size, **row}
                      for row in header["source_rows"]), key=batch.sort_key)
    covered_keys = {(row["language"], row["relativePath"]) for row in covered}
    coverage = [{"artifact": 0 if (row["language"], row["relativePath"]) in covered_keys else None,
                 "exclusion": None if (row["language"], row["relativePath"]) in covered_keys
                 else "notRequested",
                 "language": row["language"], "relativePath": row["relativePath"]}
                for row in sources if row["language"] in producer_languages]
    return batch.write_batch(Path(root) / ".trellis-semantic-batch", descriptor_header, sources, coverage,
                             [{"language": "javascript", "data": artifact_bytes}], root=Path(root))


def run_semantic_import(binary, root, daemon, descriptor, sha, env, timed=False, rusage=None):
    before = request_rows(daemon.home)
    result = command([str(binary), "semantic", "import", "--workspace", str(root),
                      "--batch", str(descriptor), "--batch-sha256", sha],
                     env=env, timed=timed, rusage=rusage)
    output = result[1] if timed else result
    reply = json.loads(output)
    require("publishedRevision" in reply, f"semantic import reply lacks publishedRevision: {output[-500:]}")
    added = {key: value for key, value in request_rows(daemon.home).items() if key not in before}
    require(len(added) == 1 and next(iter(added.values()))[0] == "done",
            f"expected exactly one completed durable semantic import request row: {added}")
    return result


def import_fixture(binary, root, daemon, size, r1):
    anchor = Path("javascript") / f"C{size}0010.js"
    raw = scip_anchor(root / anchor, anchor)
    digest = hashlib.sha256(raw).hexdigest()
    with closing(sqlite3.connect(f"file:{db_for(daemon.home)}?mode=ro", uri=True)) as db:
        captured = capture_r1(db, r1)
    rows = captured["source_rows"]
    covered = [row for row in rows if row["language"] == "javascript"]
    require(len(covered) == 1 and covered[0]["relativePath"] == anchor.as_posix(),
            "r1 JavaScript closure must contain exactly the frozen anchor")
    # write_batch needs an empty directory, and a workspace root imports once per directory.
    shutil.rmtree(root / ".trellis-semantic-batch", ignore_errors=True)
    descriptor, descriptor_sha = write_import_batch(root, captured, raw, covered)
    output = run_semantic_import(binary, root, daemon, descriptor, descriptor_sha, env_for(daemon.home))
    pin = json.loads(output)["publishedRevision"]
    selected = f"pin:v1:{pin['indexGeneration']}:{pin['indexRevision']}"
    with closing(sqlite3.connect(f"file:{db_for(daemon.home)}?mode=ro", uri=True)) as db:
        proof = selected_proof(db, selected, anchor.as_posix(), digest)
    record("fixture_import", size=size, r1=r1, selection_pin=selected, descriptor_sha256=descriptor_sha,
           source_manifest_hash=captured["manifest_hash"], artifact_sha256=digest,
           requested_tuple=("scip-typescript", "javascript", anchor.as_posix()), proof=proof)
    return digest, selected


def copy_cohort(source_dir, root, relatives=None):
    """Create `root` and copy `relatives` (default: the whole cohort) from `source_dir`."""
    root.mkdir()
    if relatives is None:
        shutil.copytree(source_dir, root, dirs_exist_ok=True)
        return
    for relative in relatives:
        dest = root / relative
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source_dir / relative, dest)


def seed_workspace(binary, corpus, workspace, daemon, size, edited=None):
    anchor = Path("javascript") / f"C{size}0010.js"
    sentinels = [anchor, Path("java") / f"C{size}0010.java",
                 Path("python") / f"C{size}0010.py", Path("rust/g00") / f"C{size}0010.rs"]
    copy_cohort(corpus / size, workspace, sentinels)
    anchor_digest = sha_file(workspace / anchor)
    expected = {"medium": "7b08f0b2aad6ef5e309cb480fdb17d83539e2273d45aa74c83fe9bd58084c08b",
                "large": "e061783a302f0007e0f05881570f1041697516a22bb357a09df5eeebc71d724d"}[size]
    require(anchor_digest == expected, "frozen ordinal-10 anchor differs from pinned manifest")
    output = command([str(binary), "index", "--workspace", str(workspace)], env=env_for(daemon.home))
    pin = json.loads(output)["publishedRevision"]
    r1 = f"pin:v1:{pin['indexGeneration']}:{pin['indexRevision']}"
    with closing(sqlite3.connect(f"file:{db_for(daemon.home)}?mode=ro", uri=True)) as db:
        require(db.execute("SELECT count(*) FROM revision_documents WHERE revision_id=?", (r1,)).fetchone()[0] == 4,
                "r1 failed to capture all four sentinels")
    artifact, imported = import_fixture(binary, workspace, daemon, size, r1)
    shutil.copytree(corpus / size, workspace, dirs_exist_ok=True)
    if edited is not None:
        for relative, source in edited.items():
            shutil.copyfile(source, workspace / relative)
    require(sha_file(workspace / anchor) == expected, "full r2 changed frozen anchor")
    output = command([str(binary), "index", "--workspace", str(workspace)], env=env_for(daemon.home))
    full = json.loads(output)["publishedRevision"]
    r2 = f"pin:v1:{full['indexGeneration']}:{full['indexRevision']}"
    with closing(sqlite3.connect(f"file:{db_for(daemon.home)}?mode=ro", uri=True)) as db:
        selected_proof(db, r2, anchor.as_posix(), artifact)
        published(db, output, size)
    require(r2 not in (r1, imported), "untimed full r2 must publish a new selected pin")
    with closing(sqlite3.connect(f"file:{db_for(daemon.home)}?mode=ro", uri=True)) as db:
        native_ids = db.execute("SELECT native_revision_id FROM native_revisions WHERE id IN (?,?)",
                                (r1, r2)).fetchall()
        require(len(native_ids) == 2 and native_ids[0] != native_ids[1],
                "full r2 must differ from captured native r1")
    record("untimed_full_r2", size=size, r1=r1, import_pin=imported, r2=r2,
           anchor_sha256=expected, artifact_sha256=artifact)
    return r2, artifact, anchor, expected


class SelectedView:
    """Closed schema-33 view retaining all selected columns plus input-closure digests."""
    def __init__(self, db, revision, physical_root):
        self.db, self.revision = db, revision
        self.physical_root = physical_root.resolve(strict=True)
        self.root_id = "path-" + hashlib.sha256(str(self.physical_root).encode()).hexdigest()
        header = db.execute("SELECT source_set_id,native_revision_id,source_inventory FROM native_revisions WHERE id=?",
                            (revision,)).fetchone()
        require(header is not None, "missing selected native header")
        self.source_set, self.native_revision, inventory = header
        self.docs = {(d["key"]["language"], d["key"]["path"]): d["contentHash"]
                     for d in json.loads(inventory)}
        self.ids = {"source-set": {self.source_set: "<source-set>"},
                    "native-revision": {self.native_revision: "<r2>"},
                    "pin": {revision: "<r2-pin>"},
                    "vid": {}, "gid": {}, "cid": {}, "syntax": {}, "occurrence": {},
                    "region": {}, "class": {}, "class-ref": {}, "graph-node": {},
                    "tuple": {}, "basis": {}, "batch": {}, "capture": {}, "proof": {},
                    "provenance": {}, "symbol": {}, "binding": {}}
        self.r1 = None
        for language, path, vid, gid, cid in db.execute(
                "SELECT language,path,vid,gid,cid FROM revision_documents WHERE revision_id=?",
                (revision,)):
            key = (language, path, self.docs[(language, path)])
            for family, value in (("vid", vid), ("gid", gid), ("cid", cid)):
                if value is not None:
                    self.ids[family][value] = key
        for vid, syntax, kind, name, ordinal, start, end, name_start, name_end in db.execute(
                "SELECT vid,syntax_id,kind,name,key_ordinal,start_byte,end_byte,name_start,name_end "
                "FROM native_version_declarations WHERE vid IN (SELECT vid FROM revision_documents WHERE revision_id=?)",
                (revision,)):
            self.ids["syntax"][syntax] = (self.ids["vid"][vid], kind, name, ordinal,
                                           start, end, name_start, name_end)
        for table, family, parts in (
            ("native_version_calls", "occurrence", "ordinal,start_byte,end_byte,spelling"),
            ("native_version_references", "occurrence", "ordinal,start_byte,end_byte,spelling"),
            ("native_version_control_regions", "region", "ordinal,start_byte,end_byte,kind")):
            for vid, raw, *rest in db.execute(
                    f"SELECT vid,id,{parts} FROM {table} WHERE vid IN "
                    "(SELECT vid FROM revision_documents WHERE revision_id=?)", (revision,)):
                key = (self.ids["vid"][vid], table, *rest)
                previous = self.ids[family].get(raw)
                require(previous is None or previous == key, f"conflicting native {family} mapping")
                self.ids[family][raw] = key
        graph_count = 0
        for gid, raw, name, kind, syntax in db.execute(
                "SELECT n.gid,n.id,n.name,n.kind,d.syntax_id FROM graph_nodes n "
                "JOIN graph_projections g ON g.gid=n.gid "
                "JOIN native_version_declarations d ON d.vid=g.vid AND d.syntax_id=n.id "
                "WHERE n.gid IN (SELECT gid FROM revision_documents WHERE revision_id=?)", (revision,)):
            require(syntax in self.ids["syntax"], "graph node lacks selected native declaration")
            key = (self.ids["gid"][gid], self.ids["syntax"][syntax], name, kind)
            require(raw not in self.ids["graph-node"], "duplicate graph node identity")
            self.ids["graph-node"][raw] = key
            graph_count += 1
        require(graph_count == db.execute(
            "SELECT count(*) FROM graph_nodes WHERE gid IN "
            "(SELECT gid FROM revision_documents WHERE revision_id=?)", (revision,)).fetchone()[0],
            "unmapped selected graph node")
        require(len(set(map(repr, self.ids["graph-node"].values()))) == graph_count,
                "colliding normalized graph node identity")
        for cid, raw, name, qualified, path in db.execute(
                "SELECT cid,id,name,qualified_name,path FROM classes WHERE cid IN "
                "(SELECT cid FROM revision_documents WHERE revision_id=?)", (revision,)):
            self.ids["class"][raw] = (self.ids["cid"][cid], name, qualified, path)
        for cid, raw, owner, payload in db.execute(
                "SELECT cid,id,owner,payload FROM class_relations WHERE cid IN "
                "(SELECT cid FROM revision_documents WHERE revision_id=?)", (revision,)):
            parsed = json.loads(payload)
            self.ids["class-ref"][raw] = (self.ids["cid"][cid], self.ids["class"][owner],
                                           parsed["typeName"], parsed["kind"], parsed["range"])
        for raw, producer, language, path in db.execute(
                "SELECT tuple_id,producer_id,language,path FROM semantic_coverage WHERE revision_id=?", (revision,)):
            self.ids["tuple"][raw] = (producer, language, path, self.docs[(language, path)])
        selections = db.execute("""SELECT s.batch_id,b.producer_id,b.source_set_id,b.captured_revision,
            s.captured_pin,b.descriptor_bytes,b.descriptor_sha256,b.source_manifest_hash
            FROM semantic_revision_raw_selections s
            JOIN semantic_raw_batches b ON b.batch_id=s.batch_id WHERE s.revision_id=?""",
            (revision,)).fetchall()
        require(len(selections) == 1, f"expected exactly one selected batch: {selections}")
        for (batch, producer, source_set, captured_revision, captured_pin, descriptor_bytes,
             descriptor_sha, batch_manifest_hash) in selections:
            captures = db.execute("""SELECT capture_id,ordinal,artifact_hash,binary_bytes
                FROM semantic_raw_captures WHERE batch_id=? ORDER BY ordinal""", (batch,)).fetchall()
            require(len(captures) == 1, f"expected exactly one selected capture: {len(captures)}")
            capture, ordinal, artifact, binary = captures[0]
            descriptor = json.loads(descriptor_bytes)
            original = db.execute("SELECT source_inventory FROM native_revisions WHERE id=? "
                                  "AND native_revision_id=? AND source_set_id=?",
                                  (captured_pin, captured_revision, source_set)).fetchall()
            require(len(original) == 1, "captured r1 native source inventory missing")
            retained = json.loads(original[0][0])
            expected_rows = [{"document": {"sourceSetId": source_set,
                              "language": item["key"]["language"], "path": item["key"]["path"]},
                              "contentHash": item["contentHash"]} for item in retained]
            expected_hash = hashlib.sha256(canonical(expected_rows)).hexdigest()
            require(source_set == self.source_set and descriptor["sourceSet"]["id"] == source_set
                    and descriptor["sourceSet"]["rootId"] == self.root_id
                    and source_set == "source-set:v1:" + self.root_id
                    and descriptor["metadata"]["projectRoot"] == self.physical_root.as_uri()
                    and descriptor["sourceRevision"] == captured_revision
                    and descriptor["sourceManifestHash"] == batch_manifest_hash == expected_hash
                    and descriptor["totals"]["sourceDocuments"] == len(retained) == 4
                    and hashlib.sha256(descriptor_bytes).hexdigest() == descriptor_sha
                    and hashlib.sha256(binary).hexdigest() == artifact
                    == descriptor["artifacts"][0]["sha256"],
                    "selected raw batch/source root/r1 inventory/binary digest mismatch")
            r1_doc = [(item["key"]["language"], item["key"]["path"], item["contentHash"])
                      for item in retained]
            require(len(r1_doc) == 4 and len(set(r1_doc)) == 4, "r1 document closure missing")
            self.r1 = captured_revision
            self.ids["pin"][captured_pin] = "<captured-r1-pin>"
            self.ids["native-revision"][captured_revision] = "<captured-r1>"
            self.ids["batch"][batch] = (producer, tuple(r1_doc))
            self.ids["capture"][capture] = (producer, artifact, ordinal, tuple(r1_doc))
            self.capture_descriptor = descriptor
            self.capture_pin = captured_pin
            imported = db.execute("""SELECT r.revision_id FROM semantic_revision_raw_selections r
                JOIN semantic_import_reports i ON i.revision_id=r.revision_id AND i.producer_id=r.producer_id
                WHERE r.batch_id=? AND r.producer_id=?""", (batch, producer)).fetchall()
            require(len(imported) == 1 and imported[0][0] != revision,
                    "selected batch has no unique imported report pin")
            self.ids["pin"][imported[0][0]] = "<import-pin>"
            self.capture_bytes = descriptor_bytes
            self.r1_docs = r1_doc
            for raw, kind, language, path, fact, canonical_bytes in db.execute(
                    "SELECT proof_id,record_kind,language,path,fact_digest,canonical_bytes "
                    "FROM semantic_raw_proofs WHERE capture_id=?", (capture,)):
                self.ids["proof"][raw] = (self.ids["capture"][capture], kind, language,
                                           path, fact, canonical_bytes.hex())
        for raw, tuple_id, producer, artifact, language, source_revision, source_hash in db.execute(
                "SELECT basis_id,tuple_id,producer_id,artifact_hash,language,"
                "captured_native_revision_id,source_manifest_hash FROM semantic_basis "
                "WHERE tuple_id IN (SELECT tuple_id FROM semantic_coverage WHERE revision_id=?)", (revision,)):
            require(source_revision == self.r1 and source_hash == self.capture_descriptor["sourceManifestHash"],
                    "basis not bound to admitted r1")
            self.ids["basis"][raw] = (self.ids["tuple"][tuple_id], producer, artifact, language,
                                       tuple(self.r1_docs))
        for tuple_id, raw, evidence, basis, proof in db.execute(
                "SELECT tuple_id,provenance_id,evidence_kind,basis_id,derived_from_provenance_id "
                "FROM semantic_provenance WHERE tuple_id IN "
                "(SELECT tuple_id FROM semantic_coverage WHERE revision_id=?)", (revision,)):
            require(proof in self.ids["proof"], "provenance not linked to selected raw proof")
            self.ids["provenance"][raw] = (self.ids["tuple"][tuple_id], evidence,
                                            self.ids["basis"][basis], self.ids["proof"][proof])
        for raw, tuple_id, ordinal, scheme, symbol, scope in db.execute(
                "SELECT symbol_id,tuple_id,ordinal,key_scheme,key_symbol,key_scope FROM semantic_symbols "
                "WHERE tuple_id IN (SELECT tuple_id FROM semantic_coverage WHERE revision_id=?)", (revision,)):
            self.ids["symbol"][raw] = (self.ids["tuple"][tuple_id], ordinal, scheme, symbol, scope)
        for table in ("semantic_declaration_bindings", "semantic_call_bindings"):
            for raw, tuple_id, ordinal in db.execute(
                    f"SELECT binding_id,tuple_id,ordinal FROM {table} WHERE tuple_id IN "
                    "(SELECT tuple_id FROM semantic_coverage WHERE revision_id=?)", (revision,)):
                self.ids["binding"][raw] = (table, self.ids["tuple"][tuple_id], ordinal)

    def map(self, family, value):
        if value is None:
            return None
        require(value in self.ids[family], f"unmapped {family} identity: {value!r}")
        return (family, self.ids[family][value])

    def embedded(self, value):
        if isinstance(value, list):
            return [self.embedded(item) for item in value]
        if isinstance(value, dict):
            return {key: self.embedded(item) for key, item in value.items()}
        if isinstance(value, str):
            for family in ("syntax", "class-ref"):
                if family == "syntax" and value.startswith("sid:v1:"):
                    return self.map(family, bytes.fromhex(value[7:]))
                if family == "class-ref" and value.startswith("class-ref:"):
                    return self.map(family, value)
            return value
        return value

    def descriptor(self):
        descriptor = json.loads(self.capture_bytes)
        require(set(descriptor) == set(load_batch_module().DESCRIPTOR_KEYS),
                "unknown admitted descriptor field")
        descriptor["sourceSet"]["id"] = self.map("source-set", descriptor["sourceSet"]["id"])
        descriptor["sourceSet"]["rootId"] = "<physical-root>"
        descriptor["sourceRevision"] = self.map("native-revision", descriptor["sourceRevision"])
        descriptor["metadata"]["projectRoot"] = "file://<physical-root>"
        descriptor["sourceManifestHash"] = hashlib.sha256(canonical([
            {"document": {"sourceSetId": "<source-set>", "language": language, "path": path},
             "contentHash": content} for language, path, content in self.r1_docs])).hexdigest()
        return descriptor

    @staticmethod
    def stat(stat):
        require(set(stat) == {"version", "kind", "size", "device", "inode",
                              "mtimeSeconds", "mtimeNanoseconds", "ctimeSeconds",
                              "ctimeNanoseconds"}, "unknown captured filesystem stat field")
        require(stat["version"] == 1 and stat["kind"] in ("directory", "file"),
                "unexpected captured filesystem stat")
        for field_name in ("device", "inode", "mtimeSeconds", "mtimeNanoseconds",
                           "ctimeSeconds", "ctimeNanoseconds"):
            require(isinstance(stat[field_name], int), f"non-integer capture {field_name}")
            stat[field_name] = 0
        return stat

    def row(self, table, cells):
        # A per-table selected scope is established by parity_digest before this point.
        if table == "native_source_sets":
            require(cells["id"] == self.source_set and cells["root_id"] == self.root_id
                    and self.source_set == "source-set:v1:" + self.root_id,
                    "native source set is not tied to root identity")
            cells["root_id"] = "<physical-root>"
            cells["id"] = self.map("source-set", cells["id"])
        if table == "native_revisions":
            inventory = json.loads(cells["source_inventory"])
            require(len(inventory) == len(self.docs), "selected native inventory is incomplete")
            for item in inventory:
                require(item["key"]["sourceSetId"] == self.source_set
                        and item["revisionId"] == self.native_revision,
                        "native inventory is not from selected root/revision")
                item["key"]["sourceSetId"] = "<source-set>"
                item["revisionId"] = "<r2>"
            cells["source_inventory"] = inventory
            cells["published_index_revision"] = "<current-revision>"
        if table == "revision_documents":
            if cells["capture_stat"] is not None:
                stat = json.loads(cells["capture_stat"])
                cells["capture_stat"] = self.stat(stat)
        if table == "revision_capture_inputs":
            key = cells["input_key"]
            payload = json.loads(cells["payload"])
            if key.startswith("executable:"):
                require(key == "executable:" + str(BINARY), "unexpected native executable capture")
                cells["input_key"] = "executable:<pinned-binary>"
            if "stat" in payload:
                payload["stat"] = self.stat(payload["stat"])
            cells["payload"] = payload
        if table == "semantic_raw_captures":
            require(cells["binary_bytes"] == (self.db.execute(
                "SELECT binary_bytes FROM semantic_raw_captures WHERE capture_id=?",
                (cells["capture_id"],)).fetchone()[0]), "selected raw bytes changed")
        if table == "semantic_raw_batches":
            descriptor = self.descriptor()
            require(hashlib.sha256(cells["descriptor_bytes"]).hexdigest() == cells["descriptor_sha256"],
                    "selected descriptor digest changed")
            cells["descriptor_bytes"] = descriptor
            cells["descriptor_sha256"] = "<descriptor-sha256>"
            cells["source_manifest_hash"] = descriptor["sourceManifestHash"]
        if table == "semantic_import_reports":
            report = json.loads(cells["report_json"])
            require(set(report) == {"producerId", "identityKind", "sourceSetId", "capturedRevision",
                                    "destinationRevision", "descriptorSha256", "artifacts", "languages"},
                    "unknown import report field")
            report["descriptorSha256"] = "<descriptor-sha256>"
            report["capturedRevision"] = self.map("native-revision", report["capturedRevision"])
            report["destinationRevision"] = self.map("native-revision", report["destinationRevision"])
            report["sourceSetId"] = self.map("source-set", report["sourceSetId"])
            cells["report_json"] = report
        if table == "semantic_basis":
            cells["source_manifest_hash"] = self.descriptor()["sourceManifestHash"]
        if table in {"graph_projections", "classes", "class_relations"}:
            for column in ("class_extraction_payload", "class_summary_payload", "payload"):
                if column in cells and cells[column] is not None:
                    cells[column] = self.embedded(json.loads(cells[column]))
        if table == "class_projections":
            # Root-derived digest role is checked against the complete class/native
            # input closure published by parity_digest, not on its own.
            cells["content_hash"] = ("normalized-class-projection", self.map("cid", cells["cid"]))
        if table == "graph_projections":
            cells["graph_hash"] = ("normalized-graph-projection", self.map("gid", cells["gid"]))
        if table == "document_versions":
            cells["id"] = ("normalized-document-version", self.map("vid", cells["vid"]))
            cells["extraction_context"] = ("normalized-native-context", cells["producer_id"],
                                            cells["producer_version"], cells["language"])
            cells["native_witness"] = ("normalized-native-witness", self.map("vid", cells["vid"]))
        for column, value in list(cells.items()):
            if value is None:
                continue
            family = None
            if column in ("vid", "gid", "cid", "tuple_id", "basis_id", "batch_id", "capture_id", "symbol_id",
                          "binding_id"):
                family = column.removesuffix("_id") if column.endswith("_id") else column
            elif column == "source_set_id":
                family = "source-set"
            elif column in ("revision_id", "captured_pin") or (table == "native_revisions" and column == "id"):
                family = "pin"
            elif column in ("native_revision_id", "captured_revision", "captured_native_revision_id",
                            "join_native_revision_id", "internal_native_revision_id",
                            "declared_internal_native_revision_id", "source_internal_native_revision_id",
                            "target_internal_native_revision_id"):
                family = "native-revision"
            elif column in ("syntax_id", "owner_syntax_id", "internal_syntax_id",
                            "declared_internal_syntax_id", "source_internal_syntax_id",
                            "target_internal_syntax_id", "candidate_id"):
                family = "syntax"
            elif column in ("reference_id", "call_id", "region_id", "parent_id", "owner_id"):
                family = ("occurrence" if column in ("reference_id", "call_id") else
                          "syntax" if column == "owner_id" else "region")
            elif column in ("provenance_id", "derived_from_provenance_id"):
                family = "proof" if column == "derived_from_provenance_id" else "provenance"
            elif column == "proof_id":
                family = "proof"
            elif table in ("native_version_references", "native_version_calls", "native_version_control_regions") and column == "id":
                family = "region" if table == "native_version_control_regions" else "occurrence"
            elif table == "graph_nodes" and column == "id":
                family = "graph-node"
            elif table == "graph_display_labels" and column == "id":
                family = "graph-node"
            elif table == "classes" and column == "id":
                family = "class"
            elif table == "class_relations" and column in ("owner", "target"):
                family = "class"
            elif table == "class_relations" and column == "id":
                family = "class-ref"
            if family:
                cells[column] = self.map(family, value)
        return cells


def sha_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def interrupt(signum, _frame):
    raise RuntimeError(f"measurement interrupted by signal {signum}; cleanup required")


def main():
    global BINARY, EVIDENCE
    parser = argparse.ArgumentParser(description="#225 frozen mixed single-file gate; run sizes separately")
    parser.add_argument("--size", choices=("medium", "large"))
    parser.add_argument("--smoke", action="store_true", help="untimed authenticated import/full/local probe")
    parser.add_argument("--binary", type=Path, help="already-built release binary; skips untimed cargo build")
    parser.add_argument("--evidence-dir", type=Path,
                        help="new retained evidence directory (default: external target sibling, never auto-deleted)")
    args = parser.parse_args()
    require(args.smoke or args.size is not None, "--size required for measurements")
    evidence_root = Path(os.environ.get("TRELLIS_PERFORMANCE_EVIDENCE_DIR",
                                        str(TARGET_DIR.resolve().parent)))
    evidence_root.mkdir(parents=True, exist_ok=True)
    EVIDENCE = (args.evidence_dir.resolve() if args.evidence_dir else
                Path(tempfile.mkdtemp(prefix="trellis-90-mixed-", dir=evidence_root)))
    if args.evidence_dir:
        EVIDENCE.mkdir(parents=True, exist_ok=False)
    record("evidence", directory=str(EVIDENCE))
    for sig in (signal.SIGALRM, signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, interrupt)
    signal.alarm(1750)  # unchanged hard backstop
    require(platform.system() == "Darwin", "AC8 must run on actual Mac mini M5 Pro")
    model = command(["sysctl", "-n", "hw.model"]).strip()
    require(model == "Mac17,16", f"wrong reference host: {model}")
    pinned = PINNED.read_bytes()
    parsed = json.loads(pinned)
    require(parsed["version"] == 2 and parsed["seed"] == "trellis-synthetic-cohorts-v1", "unknown manifest")
    if args.binary:
        BINARY = args.binary.resolve()
    else:
        command(["cargo", "build", "--locked", "--release"])
    require(BINARY.is_file(), f"missing release binary: {BINARY}")
    record("provenance", binary=str(BINARY), binary_sha256=sha_file(BINARY),
           source_head=command(["git", "rev-parse", "HEAD"]).strip(),
           source_status=command(["git", "status", "--porcelain"]),
           runner_sha256=sha_file(Path(__file__)), generator_sha256=sha_file(GENERATOR),
           manifest_sha256=hashlib.sha256(pinned).hexdigest(), target_dir=str(TARGET_DIR),
           global_budget_seconds=1700, alarm_seconds=1750)
    # Retain scratch even on failure; never remove a HOME before child/election
    # cleanup. The caller may archive/remove this directory after inspecting logs.
    scratch = EVIDENCE / "scratch"
    scratch.mkdir()
    corpus = scratch / "canonical"
    command(["node", str(GENERATOR), "--out", str(corpus)])
    require((corpus / "manifest.json").read_bytes() == pinned,
            "generator output does not exactly match pinned manifest bytes/hashes")
    size = args.size or "medium"
    workspace, home = scratch / f"{size}-workspace", scratch / f"{size}-private-home"
    home.mkdir(mode=0o700)
    daemon = Daemon(BINARY, home, "warm")
    measurements = {}
    try:
        daemon.start()
        prior, artifact, anchor, anchor_hash = seed_workspace(BINARY, corpus, workspace, daemon, size)
        untimed_full_setup_revision = prior
        if args.smoke:
            language, relative, kind = planned_edit(size, "body", 0)
            before = request_rows(home)
            generation, number = prior.rsplit(":", 1)
            started = time.monotonic()
            wall = time.time_ns() / 1_000_000
            edit_leaf(workspace / relative, language, kind)
            _, local = index(BINARY, workspace, daemon, size, sample={
                "before_requests": before, "start": started, "start_wall_ms": wall,
                "expected_revision": f"{generation}:{int(number)+1}"})
            require(local["publication_mode"] == "local", "smoke edit did not publish LOCAL")
            with closing(sqlite3.connect(f"file:{db_for(home)}?mode=ro", uri=True)) as db:
                selected_proof(db, local["revision"], anchor.as_posix(), artifact)
                check_changed_source(db, local["revision"], relative.as_posix(),
                                     (workspace / relative).read_bytes())
            require(sha_file(workspace / anchor) == anchor_hash, "local edit modified frozen anchor")
            daemon.stop()
            oracle_home = scratch / f"{size}-smoke-oracle-home"
            oracle_home.mkdir(mode=0o700)
            oracle_root = scratch / f"{size}-smoke-oracle-root"
            oracle = Daemon(BINARY, oracle_home, "smoke-oracle")
            try:
                oracle.start()
                oracle_revision, oracle_artifact, oracle_anchor, oracle_hash = seed_workspace(
                    BINARY, corpus, oracle_root, oracle, size,
                    edited={relative.as_posix(): workspace / relative})
                require(sha_file(oracle_root / oracle_anchor) == oracle_hash == anchor_hash,
                        "smoke oracle anchor changed")
                with closing(sqlite3.connect(f"file:{db_for(oracle_home)}?mode=ro", uri=True)) as db:
                    selected_proof(db, oracle_revision, oracle_anchor.as_posix(), oracle_artifact)
            finally:
                oracle.stop()
            with closing(sqlite3.connect(f"file:{db_for(home)}?mode=ro", uri=True)) as db:
                selected_digest = parity_digest(db, local["revision"], workspace)
            with closing(sqlite3.connect(f"file:{db_for(oracle_home)}?mode=ro", uri=True)) as db:
                oracle_digest = parity_digest(db, oracle_revision, oracle_root)
            (EVIDENCE / "smoke-selected-digest.json").write_text(json.dumps(selected_digest, indent=2) + "\n")
            (EVIDENCE / "smoke-oracle-digest.json").write_text(json.dumps(oracle_digest, indent=2) + "\n")
            require(selected_digest == oracle_digest, "smoke selected/native/raw-linked FULL oracle mismatch: " +
                    json.dumps([table for table in selected_digest if selected_digest[table] != oracle_digest[table]]))
            # Regression: two same-name parameters in one selected Java document
            # must remain distinct destinations. Mutate only one on an isolated DB
            # copy and exercise the exact parity decision used above.
            negative_db = EVIDENCE / "smoke-negative-graph.db"
            shutil.copyfile(db_for(oracle_home), negative_db)
            with closing(sqlite3.connect(negative_db)) as db:
                rows = db.execute("""SELECT n.gid,n.name,n.kind FROM graph_nodes n
                    JOIN graph_projections g ON g.gid=n.gid
                    JOIN revision_documents d ON d.gid=n.gid AND d.revision_id=?
                    WHERE d.language='java' AND n.kind='parameter'
                    GROUP BY n.gid,n.name,n.kind HAVING count(*) > 1
                    ORDER BY n.gid,n.name LIMIT 1""", (oracle_revision,)).fetchone()
                require(rows is not None, "same-name graph-node control pair missing")
                gid, name, kind = rows
                pair = db.execute("SELECT id FROM graph_nodes WHERE gid=? AND name=? AND kind=? "
                                  "ORDER BY id LIMIT 2", (gid, name, kind)).fetchall()
                view = SelectedView(db, oracle_revision, oracle_root)
                require(len(pair) == 2 and view.map("graph-node", pair[0][0]) !=
                        view.map("graph-node", pair[1][0]),
                        "same-name graph destinations collapse")
                db.execute("UPDATE graph_nodes SET name=? WHERE gid=? AND id=?",
                           (name + "_negative", gid, pair[0][0]))
                db.commit()
                mutated_digest = parity_digest(db, oracle_revision, oracle_root)
            rejected = False
            try:
                require(selected_digest == mutated_digest, "expected graph-node parity mismatch")
            except RuntimeError as exc:
                require(str(exc) == "expected graph-node parity mismatch",
                        "negative control failed for unrelated reason")
                rejected = True
            require(rejected and oracle_digest["graph_nodes"] != mutated_digest["graph_nodes"]
                    and oracle_digest["opaque_input_closures"]["graph_hash"] !=
                    mutated_digest["opaque_input_closures"]["graph_hash"],
                    "single-node mutation did not alter graph parity closure")
            record("smoke_negative_graph_rejected", path="java", same_name=name,
                   original=oracle_digest["graph_nodes"]["sha256"],
                   mutated=mutated_digest["graph_nodes"]["sha256"])
            record("smoke_pass", r2=prior, local=local["revision"], oracle=oracle_revision,
                   parity_tables=len(selected_digest), timed=False)
            return
        all_edited_paths = set()
        for shape in ("body", "declaration-surface", "class-projection"):
            samples = []
            edited_paths = set()
            for n in range(20):
                language, relative, kind = planned_edit(size, shape, n)
                leaf = workspace / relative
                path = relative.as_posix()
                require(path not in edited_paths, f"duplicate path in {shape}: {path}")
                edited_paths.add(path)
                all_edited_paths.add(path)
                before_sha = sha_file(leaf)
                before_requests = request_rows(home)
                prior_generation, prior_number = prior.rsplit(":", 1)
                expected_revision = f"{prior_generation}:{int(prior_number)+1}"
                start_wall_ms = time.time_ns() / 1_000_000
                edit_start = time.monotonic()
                old_text, new_text = (edit_leaf(leaf, language, kind) if shape == "body" else
                                      edit_surface(leaf, language, shape, n))
                seconds, own = index(BINARY, workspace, daemon, size, sample={
                    "before_requests": before_requests, "start": edit_start,
                    "start_wall_ms": start_wall_ms, "expected_revision": expected_revision})
                record("edit", size=size, shape=shape, run=n+1, language=language, path=path,
                       before_sha256=before_sha, after_sha256=sha_file(leaf),
                       old_text=old_text, new_text=new_text)
                # Save the observation before assertions, including invalid/no-op
                # attempts. Never retry/exclude an attempt or round gate inputs.
                record("observed", size=size, shape=shape, run=n+1, seconds=seconds, **own)
                require(own["publication_mode"] == "local", f"{shape} run {n+1}: not an actual LOCAL publication")
                require(own["revision"] == expected_revision, "sample did not publish exactly the next revision")
                with closing(sqlite3.connect(f"file:{db_for(home)}?mode=ro", uri=True)) as db:
                    check_changed_source(db, own["revision"], path, leaf.read_bytes())
                    selected_proof(db, own["revision"], anchor.as_posix(), artifact)
                    require(sha_file(workspace / anchor) == anchor_hash, "sample changed frozen anchor")
                    reuse = check_selected_local_ids(db, prior, own["revision"], path, size, n, shape)
                    if shape == "class-projection":
                        before_class = class_output(db, prior, path)
                        after_class = class_output(db, own["revision"], path)
                        require(before_class[0] != after_class[0], "class edit did not change extracted class facts")
                        require(before_class[1:] != after_class[1:], "class edit did not change selected class output")
                counters = own["writer_bind"]
                require(counters["manifest"]["rows"] >= (1000 if size == "medium" else 10000), "missing O(documents) manifest")
                require(counters["native"]["rows"] > 0 and counters["graph"]["rows"] > 0,
                        "update did not create a native/graph projection")
                require(counters["native"]["rows"] < (100000 if size == "medium" else 1000000),
                        f"local update unexpectedly rewrote too many native rows: {counters}")
                sample = {"seconds": seconds, **own, "reuse": reuse, "changed_path": path,
                          "language": language, "edit_kind": kind, "old_text": old_text, "new_text": new_text}
                samples.append(sample)
                record("sample_validated", size=size, shape=shape, run=n+1, **sample)
                prior = own["revision"]
                limit = 2 if size == "medium" else 5
            p95 = sorted(s["seconds"] for s in samples)[math.ceil(.95 * len(samples)) - 1]
            require(p95 <= limit, f"{size} {shape} p95 {p95}s exceeds {limit}s")
            # Stop the warm writer before the complete selected snapshot and
            # separately rooted, independently admitted FULL oracle. Restart is untimed, never a
            # substitute for an edit publication.
            daemon.stop()
            with closing(sqlite3.connect(f"file:{db_for(home)}?mode=ro", uri=True)) as db:
                warm_digest = parity_digest(db, prior, workspace)
            oracle_home = scratch / f"{size}-{shape}-independent-home"
            oracle_home.mkdir(mode=0o700)
            oracle_root = scratch / f"{size}-{shape}-independent-root"
            oracle = Daemon(BINARY, oracle_home, shape + "-oracle")
            try:
                oracle.start()
                oracle_revision, oracle_artifact, oracle_anchor, oracle_hash = seed_workspace(
                    BINARY, corpus, oracle_root, oracle, size,
                    edited={relative: workspace / relative for relative in all_edited_paths})
                require(sha_file(oracle_root / oracle_anchor) == oracle_hash == anchor_hash,
                        "oracle anchor not frozen")
                with closing(sqlite3.connect(f"file:{db_for(oracle_home)}?mode=ro", uri=True)) as db:
                    selected_proof(db, oracle_revision, oracle_anchor.as_posix(), oracle_artifact)
                    cold = {"revision": oracle_revision, "mode": "full",
                            "produced_native_facts": dict(db.execute(FACTS, (oracle_revision,)).fetchall())}
                cold_seconds = None  # oracle setup is deliberately untimed
            finally:
                oracle.stop()
            with closing(sqlite3.connect(f"file:{db_for(oracle_home)}?mode=ro", uri=True)) as db:
                cold_digest = parity_digest(db, oracle_revision, oracle_root)
            (EVIDENCE / f"{shape}-warm-digest.json").write_text(json.dumps(warm_digest, indent=2, sort_keys=True) + "\n")
            (EVIDENCE / f"{shape}-full-digest.json").write_text(json.dumps(cold_digest, indent=2, sort_keys=True) + "\n")
            require(warm_digest == cold_digest, f"{size} {shape}: complete selected output differs from FULL: " +
                    json.dumps({key: [warm_digest.get(key), cold_digest.get(key)] for key in warm_digest.keys() | cold_digest.keys()
                                if warm_digest.get(key) != cold_digest.get(key)}))
            measurements[shape] = {"runs": samples, "local_p95_seconds": p95, "local_threshold_seconds": limit,
                                   "independent_full_setup_seconds": cold_seconds, "independent_full": cold,
                                   "complete_selected_parity": warm_digest}
            record("shape_pass", size=size, shape=shape, local_p95_seconds=p95, samples=20, parity="complete selected FULL")
            if shape != "class-projection":
                daemon = Daemon(BINARY, home, "warm-after-" + shape)
                daemon.start()
                # Socket readiness does not activate this checkout. Complete
                # mandatory startup against unchanged bytes before any edit.
                activation_seconds, activation = index(
                    BINARY, workspace, daemon, size, warm_setup=prior)
                record("warm_activation", size=size, after_shape=shape,
                       seconds=activation_seconds, **activation)
    finally:
        # Cleanup remains required after the budget expires or any assertion.
        # Ignore repeated deadline/interrupt signals only during bounded teardown.
        for sig in (signal.SIGALRM, signal.SIGTERM, signal.SIGINT):
            signal.signal(sig, signal.SIG_IGN)
        daemon.stop()
    report = {"host": model, "chip": "Apple M5 Pro", "size": size,
              "manifest_sha256": hashlib.sha256(pinned).hexdigest(),
              "method": ("20 distinct single-file edits EACH body/declaration-surface/class-projection; "
                         "own nearest-rank p95; immediately-before-edit through complete CLI return; "
                         "actual LOCAL watcher/request publisher recorded with pin-scoped diagnostics "
                         "and durable terminal-pin correlation; complete selected FULL parity after "
                         "each shape; no excluded failures"),
              "untimed_full_setup_revision": untimed_full_setup_revision,
              "selected_artifact_sha256": artifact,
              "measurements": measurements,
              "evidence": str(EVIDENCE)}
    (EVIDENCE / "result.json").write_text(json.dumps(report, sort_keys=True, indent=2) + "\n")
    print("REFERENCE_67_MIXED_RESULT=" + json.dumps(report, sort_keys=True), flush=True)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.TimeoutExpired, sqlite3.Error, OSError, ValueError, KeyError) as exc:
        record("gate_failed", error=str(exc), evidence=str(EVIDENCE))
        print(f"mixed reference-performance gate FAILED: {exc}", file=sys.stderr, flush=True)
        sys.exit(1)
