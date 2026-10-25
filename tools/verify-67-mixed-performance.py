#!/usr/bin/env python3
"""#225 single-file LOCAL gate on the frozen #67 mixed-language corpus.

Twenty observations per edit family; time from immediately before the edit
through the whole real CLI return, including watcher debounce. Verify the actual
LOCAL watcher/request publisher and complete selected FULL parity per family.
Failed evidence is retained.
"""
import argparse
from contextlib import closing
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

REPO = Path(__file__).resolve().parents[1]
PINNED = REPO / "tools/synthetic-cohorts/manifest-v2.json"
GENERATOR = REPO / "tools/synthetic-cohorts/generate.mjs"
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


def command(args, *, env=None, timed=False):
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
            proc.wait(timeout=min(remain, 1700))
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
        args += ["-p", str(pid)] if pid is not None else ["-a", "-x"]
        args += ["-o", "pid=", "-o", "stat=", "-o", "command="]
        result = subprocess.run(args, capture_output=True, text=True, timeout=10)
        require(result.returncode == 0 or (pid is not None and result.returncode == 1),
                "cannot enumerate private HOME daemon processes")
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
    def __init__(self, binary, home, label):
        self.binary, self.home = binary, home
        self.path = EVIDENCE / f"{label}-daemon.stderr.log"
        self.proc = self.log = None

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
    require(db.execute("PRAGMA user_version").fetchone()[0] == 21,
            "merged native-v9 gate requires owner-directed schema21")
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


# Port of tests/index_atomic.rs::sqlite_snapshot: all selected evidence columns,
# 57 native-v9/schema21 tables, immutable surrogate keys, and current pin normalization.
REQUIRED_TABLES = set("""
native_producers native_producer_languages native_producer_inputs
native_source_sets native_source_set_languages native_source_set_dependencies
native_revisions revision_capture_inputs document_versions
graph_projections graph_nodes graph_display_labels
class_projections classes class_relations
revision_documents native_version_coverage_roles native_version_declarations
native_version_declaration_ancestors native_version_own_signature_types native_version_ancestor_signature_types
native_version_headers native_version_header_items native_version_parameters
native_version_references native_version_capability_families native_version_capability_gaps
native_version_calls native_version_control_regions native_version_call_regions
native_revision_release_debt native_release_candidate_versions native_release_candidate_graphs
native_release_candidate_classes semantic_producers semantic_producer_languages
semantic_revision_producers semantic_requested_producers semantic_requested_producer_languages
semantic_coverage semantic_coverage_roles semantic_basis
semantic_basis_lookup_dependencies semantic_provenance semantic_symbols
semantic_symbol_targets semantic_declaration_bindings semantic_declaration_binding_symbols
semantic_declaration_join_candidates semantic_references semantic_reference_roles
semantic_reference_candidates semantic_call_bindings semantic_call_candidates
semantic_call_possible_dispatch semantic_call_join_candidates semantic_type_relationships
""".split())
CONTROL_TABLES = {"native_revision_supersessions", "native_binding_epoch", "revision_producer_bindings"}
DEBT_TABLES = {"native_revision_release_debt", "native_release_candidate_versions",
               "native_release_candidate_graphs", "native_release_candidate_classes"}


def parity_digest(db, revision):
    require(db.execute("PRAGMA user_version").fetchone()[0] == 21,
            "independent merged native-v9 parity requires schema21")
    generation, number = revision.removeprefix("pin:v1:").rsplit(":", 1)
    require(db.execute("SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1").fetchone()
            == (generation, int(number)), "parity pin is not frozen head")
    require(db.execute("PRAGMA foreign_key_check").fetchone() is None, "retained evidence foreign-key violation")
    tables = {row[0] for row in db.execute("""SELECT name FROM sqlite_master WHERE type='table'
        AND name NOT LIKE 'sqlite_%' AND name!='index_metadata' AND name NOT LIKE '%trigrams%'""")}
    require(REQUIRED_TABLES <= tables and tables <= REQUIRED_TABLES | CONTROL_TABLES,
            f"unhandled evidence schema: missing={REQUIRED_TABLES-tables}, extra={tables-REQUIRED_TABLES-CONTROL_TABLES}")
    require(("native_binding_epoch" in tables) == ("revision_producer_bindings" in tables),
            "producer binding tables must be paired")
    if "native_binding_epoch" in tables:
        first = db.execute("SELECT first_revision FROM native_binding_epoch").fetchone()[0]
        bound = db.execute("SELECT count(*) FROM revision_producer_bindings WHERE revision_id=?", (revision,)).fetchone()[0]
        require(bound == int(int(number) >= first), "selected revision producer binding missing")
    result = {}
    for table in sorted(tables - CONTROL_TABLES):
        if table == "native_revisions":
            where = "id=?"
        elif table in DEBT_TABLES | {"revision_capture_inputs", "revision_documents"}:
            where = "revision_id=?"
        elif table == "native_source_sets":
            where = "id=(SELECT source_set_id FROM native_revisions WHERE id=?)"
        elif table in {"native_source_set_languages", "native_source_set_dependencies"}:
            where = "source_set_id=(SELECT source_set_id FROM native_revisions WHERE id=?)"
        elif table.startswith("native_producer"):
            id_column, version_column = ("id", "version") if table == "native_producers" else ("producer_id", "producer_version")
            where = ("EXISTS(SELECT 1 FROM document_versions v JOIN revision_documents d ON d.vid=v.vid "
                     f"WHERE d.revision_id=? AND v.producer_id=selected.{id_column} AND v.producer_version=selected.{version_column})")
        elif table == "document_versions" or table.startswith("native_version_"):
            where = "vid IN (SELECT vid FROM revision_documents WHERE revision_id=?)"
        elif table in {"graph_projections", "graph_nodes", "graph_display_labels"}:
            where = "gid IN (SELECT gid FROM revision_documents WHERE revision_id=?)"
        elif table in {"class_projections", "classes", "class_relations"}:
            where = "cid IN (SELECT cid FROM revision_documents WHERE revision_id=?)"
        elif table.startswith("semantic_"):
            where = "? IS NOT NULL"
        else:
            raise RuntimeError(f"unhandled evidence table: {table}")
        columns = [row[1] for row in db.execute(f'PRAGMA table_info("{table}")')]
        expressions = []
        for column in columns:
            expression = f'selected."{column}"'
            if column in ("vid", "gid", "cid"):
                parent, key = {"vid": ("document_versions", "id"),
                               "gid": ("graph_projections", "'graph:v1:'||graph_hash"),
                               "cid": ("class_projections", "'class-projection:v1:'||content_hash")}[column]
                expression = f'(SELECT {key} FROM {parent} immutable WHERE immutable.{column}=selected.{column})'
            elif column == "revision_id" or (table == "native_revisions" and column == "id"):
                expression = "'<current-pin>'"
            elif table == "native_revisions" and column == "published_index_revision":
                expression = "'<current-revision>'"
            expressions.append(expression)
        # Order by normalized complete rows, not history-dependent rowids.
        query = (f'SELECT {",".join(expressions)} FROM "{table}" selected WHERE {where} ORDER BY '
                 + ",".join(str(i+1) for i in range(len(columns))))
        digest, count = hashlib.sha256(), 0
        for row in db.execute(query, (revision,)):
            typed = [["blob", value.hex()] if isinstance(value, bytes) else
                     [type(value).__name__, value] for value in row]
            digest.update(json.dumps(typed, ensure_ascii=False, separators=(",", ":")).encode() + b"\n")
            count += 1
        require(not (table.startswith("semantic_") or table in DEBT_TABLES) or count == 0,
                f"native-only current head unexpectedly contains {table}")
        require(table != "native_revisions" or count == 1, "missing/duplicate selected header")
        result[table] = {"columns": columns, "rows": count, "sha256": digest.hexdigest()}
    return result


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
    parser.add_argument("--size", choices=("medium", "large"), required=True)
    parser.add_argument("--binary", type=Path, help="already-built release binary; skips untimed cargo build")
    parser.add_argument("--evidence-dir", type=Path,
                        help="new retained evidence directory (default: temporary directory, never auto-deleted)")
    args = parser.parse_args()
    EVIDENCE = args.evidence_dir.resolve() if args.evidence_dir else Path(tempfile.mkdtemp(prefix="trellis-225-mixed-evidence-"))
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
    size = args.size
    workspace, home = scratch / f"{size}-workspace", scratch / f"{size}-private-home"
    home.mkdir(mode=0o700)
    shutil.copytree(corpus / size, workspace)
    daemon = Daemon(BINARY, home, "warm")
    measurements = {}
    try:
        daemon.start()
        setup_seconds, initial = index(BINARY, workspace, daemon, size, cold_setup=True)
        record("initial_cold", size=size, seconds=setup_seconds, **initial)
        prior = initial["revision"]
        for shape in ("body", "declaration-surface", "class-projection"):
            samples = []
            edited_paths = set()
            for n in range(20):
                language, relative, kind = planned_edit(size, shape, n)
                leaf = workspace / relative
                path = relative.as_posix()
                require(path not in edited_paths, f"duplicate path in {shape}: {path}")
                edited_paths.add(path)
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
            # independent same-root FULL oracle. Restart is untimed, never a
            # substitute for an edit publication.
            daemon.stop()
            with closing(sqlite3.connect(f"file:{db_for(home)}?mode=ro", uri=True)) as db:
                warm_digest = parity_digest(db, prior)
            oracle_home = scratch / f"{size}-{shape}-independent-home"
            oracle_home.mkdir(mode=0o700)
            oracle = Daemon(BINARY, oracle_home, shape + "-oracle")
            try:
                oracle.start()
                cold_seconds, cold = index(BINARY, workspace, oracle, size, cold_setup=True)
            finally:
                oracle.stop()
            with closing(sqlite3.connect(f"file:{db_for(oracle_home)}?mode=ro", uri=True)) as db:
                cold_digest = parity_digest(db, cold["revision"])
            (EVIDENCE / f"{shape}-warm-digest.json").write_text(json.dumps(warm_digest, indent=2, sort_keys=True) + "\n")
            (EVIDENCE / f"{shape}-full-digest.json").write_text(json.dumps(cold_digest, indent=2, sort_keys=True) + "\n")
            require(warm_digest == cold_digest, f"{size} {shape}: complete selected output differs from FULL: " +
                    json.dumps({key: [warm_digest.get(key), cold_digest.get(key)] for key in warm_digest.keys() | cold_digest.keys()
                                if warm_digest.get(key) != cold_digest.get(key)}))
            measurements[shape] = {"runs": samples, "local_p95_seconds": p95, "local_threshold_seconds": limit,
                                   "independent_cold_seconds": cold_seconds, "independent_cold": cold,
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
              "cold_setup_seconds": setup_seconds, "cold_setup": initial, "measurements": measurements,
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
