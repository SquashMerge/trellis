#!/usr/bin/env python3
"""Reproduce #40 bounded-query latency gates over the real MCP stdio path.

Measurement mode (reference host only) times eight kinds against the medium and
large synthetic cohorts: syntax-only by default, plus semantic-bound answers
with --semantic. --smoke --binary PATH is the untimed, host-independent proof
of the semantic path on a four-file workspace.

The #90 seeding flow is reused by importing tools/verify-67-mixed-performance.py
unedited as V67; only its call-site artifact builder is replaced (see
configure_reuse), so the importer writes call bindings the graph can follow.
"""
import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import re
import select
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parents[1]

_SPEC = importlib.util.spec_from_file_location(
    "verify_67_mixed", REPO / "tools/verify-67-mixed-performance.py")
V67 = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(V67)

PINNED = REPO / "tools/synthetic-cohorts/manifest-v2.json"
GENERATOR = REPO / "tools/synthetic-cohorts/generate.mjs"
POLICY = REPO / "docs/semantic-evidence/benchmark-policy-v1.json"
COHORTS = REPO / "docs/semantic-evidence/cohorts-v1.json"
TARGET_DIR = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target"))
if not TARGET_DIR.is_absolute():
    TARGET_DIR = REPO / TARGET_DIR
RELEASE_BINARY = TARGET_DIR / "release/trellis"
DEADLINE = time.monotonic() + 1700  # shared with V67.DEADLINE; below Factory's 1800 s timeout
SAMPLES = 200
WARMUPS = 20
LANGUAGES = ("java", "javascript", "python", "rust")
HANG_BACKSTOP = 120  # seconds; a liveness backstop for one MCP response, not a gate
ELECTION_RELEASE_WAIT = 120  # seconds; the daemon may be busy in background fill when stopped

GRAPH_KINDS = ("defaultBoundedQuery", "maximumBoundedQuery",
               "semanticDefaultBoundedQuery", "semanticMaximumBoundedQuery")
SYNTAX_KINDS = ("defaultBoundedQuery", "maximumBoundedQuery", "symbolSubstringSearch",
                "symbolSubstringSearchSparse", "status", "sourceByKey")
SEMANTIC_KINDS = ("semanticDefaultBoundedQuery", "semanticMaximumBoundedQuery")
POLICY_ENTRY = {"defaultBoundedQuery": "defaultBoundedQuery",
                "maximumBoundedQuery": "maximumBoundedQuery",
                "semanticDefaultBoundedQuery": "defaultBoundedQuery",
                "semanticMaximumBoundedQuery": "maximumBoundedQuery",
                "symbolSubstringSearch": "symbolSubstringSearch",
                "symbolSubstringSearchSparse": "symbolSubstringSearch",
                "status": "status", "sourceByKey": "sourceByKey"}

LIMITATION_IDENTICAL = ("defaultBoundedQuery and maximumBoundedQuery do identical work; "
                        "AC13 does not exercise multi-level traversal.")
LIMITATION_SYNTAX = ("Syntax-only mode: the cohorts carry no semantic evidence, so every edge from "
                     "the root is a missingEvidence boundary.")
LIMITATION_SEMANTIC = ("Semantic mode: the importer's only CallBinding construction sets "
                       "dispatch: Unknown (src/semantic_import.rs:1159) and the rejoined cohort "
                       "evidence is possiblyStale at r2 (src/semantic_evidence.rs:1200-1214), so "
                       "bound edges are stale boundaries (dispatch boundaries when fresh); "
                       "semantic-bound answers measure binding reads and boundary classification, "
                       "not multi-level expansion. Expansion cost at 150 nodes and 500 calls stays "
                       "unmeasured until a producer emits direct or constructor dispatch.")

SMOKE_SOURCES = {
    "javascript/Csmoke0010.js": (
        "export function f0(){return 1;}\n"
        "function f1(revision){\n"
        "const local=1, peerValue=2;\n"
        "return local+peerValue+revision+f0()+f0();\n"
        "}\n"),
    "java/Csmoke0010.java": (
        "public class Csmoke0010 {\n"
        "  int f0() { return 1; }\n"
        "  int f1() { return f0() + f0(); }\n"
        "}\n"),
    "python/Csmoke0010.py": "def f0(): return 1\n\n\ndef f1(): return f0() + f0()\n",
    "rust/g00/Csmoke0010.rs": "pub fn f0() -> i32 { 1 }\npub fn f1() -> i32 { f0() + f0() }\n",
}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def init_checkout(path):
    """`trellis mcp` needs a Git checkout, and the index identity changes once .git appears,
    so the workspace must be a checkout before its first index."""
    subprocess.run(["git", "init", "--quiet", str(path)], check=True, capture_output=True)


class CheckoutPath(type(Path())):
    """A workspace path whose bare mkdir() also makes it a checkout.

    V67.seed_workspace creates the workspace itself and indexes it at once, leaving
    no point at which to run git init; only its bare workspace.mkdir() is intercepted.
    """

    def mkdir(self, *args, **kwargs):
        super().mkdir(*args, **kwargs)
        if not args and not kwargs:
            init_checkout(self)


_RECORD = V67.record
OBSERVED = []


def _observing_record(kind, **fields):
    OBSERVED.append(kind)
    _RECORD(kind, **fields)


def configure_reuse(scratch, deadline):
    """Point the reused #90 helpers at this run; call before any of them runs.

    Also notes every V67 event name in OBSERVED so a run can assert that the
    teardown safety net found nothing (no unexpected_home_processes).
    """
    V67.EVIDENCE = scratch
    V67.DEADLINE = deadline
    V67.scip_anchor = call_site_anchor
    V67.record = _observing_record
    OBSERVED.clear()


def remaining():
    return V67.DEADLINE - time.monotonic()


def stop_all(daemons, binary):
    """Stop every daemon, even if one stop fails; call from a finally block.

    A failure raises only when the body succeeded. When the body already raised, that
    original exception propagates and teardown failures are recorded and noted on it.
    """
    in_flight = sys.exc_info()[1]
    errors = []
    for daemon in daemons:
        try:
            stop_cohort_daemon(daemon, binary)
        except BaseException as exc:
            errors.append(exc)
    if errors and in_flight is None:
        raise errors[0]
    for exc in errors:
        V67.record("teardown_failed", error=str(exc))
        if hasattr(in_flight, "add_note"):
            in_flight.add_note(f"teardown also failed: {exc}")


def require_clean_teardown():
    require("unexpected_home_processes" not in OBSERVED,
            "teardown found private HOME processes after the daemon stopped")


def stop_cohort_daemon(daemon, binary):
    """Stop the owned daemon, then prove its election is released; idempotent.

    Trellis's daemon registers no signal handler, so SIGTERM ends it at once and the
    kernel drops its daemon.lock flock. V67.stop_home_processes runs last, only as a
    safety net that must find nothing: it assumes the known daemon has already stopped.
    """
    if daemon.proc is not None:
        # TERM gets the whole generous bound; KILL only follows if it lapses.
        V67.stop_child(daemon.proc, grace=max(1, min(ELECTION_RELEASE_WAIT, remaining())))
    if daemon.log is not None and not daemon.log.closed:
        daemon.log.close()
    end = time.monotonic() + max(0, min(ELECTION_RELEASE_WAIT, remaining()))
    while not V67.election_free(daemon.home):
        require(time.monotonic() < end,
                f"daemon election not released within the remaining budget: {daemon.home}")
        time.sleep(.05)
    V67.stop_home_processes(daemon.home, binary)
    V67.record("daemon_stopped", home=str(daemon.home), election_free=True)


# --- call-site artifact -------------------------------------------------

def call_site_positions(raw):
    """(definition, calls): (line, column) of the f0 name and of each bare local f0() call."""
    require(raw.isascii(), "call-site artifact needs ASCII source: UTF-16 columns equal bytes only then")
    declarations = list(re.finditer(rb"export function f0\(\)", raw))
    require(len(declarations) == 1, f"expected one 'export function f0()' declaration, found {len(declarations)}")
    own = declarations[0].start() + len(b"export function ")

    def position(offset):
        return raw.count(b"\n", 0, offset), offset - (raw.rfind(b"\n", 0, offset) + 1)

    calls = [position(match.start()) for match in re.finditer(rb"(?<![\w.$])f0\(\)", raw)
             if match.start() != own]
    require(calls, "no local f0() call site in the anchor")
    return position(own), calls


def packed(a, b, c):
    return b"".join(V67.field(1, n, 0)[1:] for n in (a, b, c))


def call_site_anchor(anchor, relative):
    """V67.scip_anchor's document plus one enclosing-range reference per local f0() call.

    The importer writes a CallBinding only when the occurrence's enclosing_range
    (field 7) joins the whole native call, so each call carries both ranges.
    """
    definition, calls = call_site_positions(anchor.read_bytes())
    symbol = b"pkg f0()."
    occurrences = [(definition, V67.field(1, packed(definition[0], definition[1], definition[1] + 2))
                    + V67.field(2, symbol) + V67.field(3, 1, 0))]
    for line, column in calls:
        occurrences.append(((line, column),
                            V67.field(1, packed(line, column, column + 2)) + V67.field(2, symbol)
                            + V67.field(7, packed(line, column, column + 4))))
    information = V67.field(1, symbol) + V67.field(5, 26, 0) + V67.field(6, b"f0")
    document = (V67.field(1, relative.as_posix().encode())
                + b"".join(V67.field(2, body) for _, body in sorted(occurrences, key=lambda o: o[0]))
                + V67.field(3, information) + V67.field(4, b"javascript") + V67.field(6, 2, 0))
    return V67.field(2, document)


# --- policy, floors, statistics ------------------------------------------

def load_policy():
    return json.loads(POLICY.read_text())["performance"]


def load_floors():
    synthetic = json.loads(COHORTS.read_text())["synthetic"]
    return {size: (synthetic[size]["minimumNormalizedFacts"], synthetic[size]["factsScope"])
            for size in ("medium", "large")}


def lte(value, limit):
    return value <= limit


def percentile(values, q):
    ordered = sorted(values)
    return ordered[math.ceil(q * len(ordered)) - 1]


def summarize(millis):
    return {"p95Ms": percentile(millis, .95), "p99Ms": percentile(millis, .99),
            "count": len(millis), "minMs": min(millis), "maxMs": max(millis)}


def evaluate(kind, stats, policy):
    entry = policy[POLICY_ENTRY[kind]]
    p99_limit = entry.get("p99Ms")
    passed = lte(stats["p95Ms"], entry["p95Ms"]) and (p99_limit is None or lte(stats["p99Ms"], p99_limit))
    return {**stats, "limitMs": entry["p95Ms"], "p99LimitMs": p99_limit, "pass": passed}


def check_floor(counts, size, floor, scope):
    require(all(counts.get(language, 0) > 0 for language in LANGUAGES),
            f"{size}: a language produced no facts: {counts}")
    if scope == "perLanguage":
        ok = all(counts[language] >= floor for language in LANGUAGES)
    else:
        require(scope == "overall", f"{size}: unknown factsScope {scope}")
        ok = sum(counts.values()) >= floor
    require(ok, f"{size}: OWN produced-index facts below {scope} floor {floor}: {counts}")


def facts_for(db, pin):
    return dict(db.execute(V67.FACTS, (pin,)).fetchall())


def floored_samples(db, pin, size, floor, scope, producer=None):
    """Apply the floor to this run's own facts before any sampling."""
    counts = facts_for(db, pin)
    check_floor(counts, size, floor, scope)
    return counts, collect_samples(db, pin, producer, smoke=size == "smoke")


# --- deterministic samples ------------------------------------------------

def stride_sample(items, count):
    require(items, "cannot sample an empty population")
    return [items[i * len(items) // count] for i in range(count)]


_ROOTS = """SELECT m.language,m.path,'sid:v1:'||lower(hex(d.syntax_id))
 FROM revision_documents m JOIN native_version_declarations d ON d.vid=m.vid
 WHERE m.revision_id=?1 AND d.kind IN ('function','method','constructor')
 AND EXISTS(SELECT 1 FROM native_version_calls c WHERE c.vid=m.vid AND c.owner_syntax_id=d.syntax_id)
 {covered}
 ORDER BY m.language,m.path,d.syntax_id"""
_COVERED = """AND EXISTS(SELECT 1 FROM semantic_coverage s WHERE s.revision_id=m.revision_id
   AND s.producer_id=?2 AND s.language=m.language AND s.path=m.path AND s.selected=1
   AND s.state IN ('complete','partial'))"""


def callable_roots(db, pin):
    return [row[2] for row in db.execute(_ROOTS.format(covered=""), (pin,))]


def covered_roots(db, pin, producer):
    return [row[2] for row in db.execute(_ROOTS.format(covered=_COVERED), (pin, producer))]


def sparse_substrings(db, pin, smoke=False):
    """Choose bounded, real-name substring samples before any timing.

    Drop up to three trailing characters to group nearby synthetic class names.
    Counts cover retained history; active hits must also exceed W2's 21-row
    lookahead. Only the four-file smoke relaxes that minimum.
    """
    minimum, maximum = (1 if smoke else 22), 200
    names = [row[0] for row in db.execute(
        """SELECT DISTINCT d.name FROM revision_documents m
           JOIN native_version_declarations d ON d.vid=m.vid
           WHERE m.revision_id=? AND length(d.name)>=3 ORDER BY d.name LIMIT 4096""", (pin,))]
    selected = []
    queries = {name[:max(3, len(name) - cut)] for name in names for cut in range(4)}
    for query in sorted(queries):
        lowered = query.translate(str.maketrans("ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz"))
        expression = '"' + lowered.replace('"', '""') + '"'
        candidates = db.execute(
            """SELECT count(*) FROM (SELECT rowid FROM native_declaration_trigrams
               WHERE native_declaration_trigrams MATCH ? LIMIT ?)""",
            (expression, maximum + 1)).fetchone()[0]
        if not minimum <= candidates <= maximum:
            continue
        active = db.execute(
            """SELECT count(*) FROM native_declaration_trigrams
               CROSS JOIN native_version_declarations d ON d.rowid=native_declaration_trigrams.rowid
               JOIN revision_documents m ON m.vid=d.vid AND m.revision_id=?
               WHERE native_declaration_trigrams MATCH ? AND instr(lower(d.name),lower(?))>0""",
            (pin, expression, query)).fetchone()[0]
        if active >= minimum:
            selected.append({"query": query, "globalFtsCandidates": candidates, "activeRevisionHits": active})
    require(selected, f"no sparse real-name substring with {minimum}..{maximum} global FTS candidates and active hits")
    picked = stride_sample(selected, SAMPLES)
    return {"globalFtsRange": [minimum, maximum], "activeRevisionMinimum": minimum,
            "smokeRelaxedMinimum": smoke, "candidateNameLimit": 4096,
            "candidateNameCount": len(names), "eligibleQueryCount": len(selected), "samples": picked}


def collect_samples(db, pin, producer=None, smoke=False):
    roots = covered_roots(db, pin, producer) if producer else callable_roots(db, pin)
    require(roots, "no callable root with calls" + (" covered by the producer" if producer else ""))
    names = [row[0] for row in db.execute(
        """SELECT d.name FROM revision_documents m JOIN native_version_declarations d ON d.vid=m.vid
           WHERE m.revision_id=? AND length(d.name)>=3 ORDER BY m.language,m.path,d.syntax_id""", (pin,))]
    documents = [row[0] for row in db.execute(
        "SELECT path FROM revision_documents WHERE revision_id=? ORDER BY language,path", (pin,))]
    require(names and documents, "no declaration names or documents to sample")
    return {"roots": stride_sample(roots, SAMPLES), "rootCount": len(roots),
            "substrings": [(n[1:4] if len(n) >= 4 else n[0:3]) for n in stride_sample(names, SAMPLES)],
            "documents": stride_sample(documents, SAMPLES),
            "sparseSearch": sparse_substrings(db, pin, smoke)}


# --- edge counts ----------------------------------------------------------

def edge_counts(result):
    counts = {"edges": 0, "bound": 0, "boundary": 0, "new": 0, "seen": 0, "reasons": {}}
    for edge in result["edges"]:
        counts["edges"] += 1
        counts["bound"] += edge["binding"] is not None
        counts["boundary"] += edge["visit"] == "boundary"
        if edge["visit"] in ("new", "seen"):
            counts[edge["visit"]] += 1
        reason = edge["boundaryReason"]
        counts["reasons"][reason] = counts["reasons"].get(reason, 0) + 1
    return counts


def add_counts(total, part):
    for key in ("edges", "bound", "boundary", "new", "seen"):
        total[key] += part[key]
    for reason, n in part["reasons"].items():
        total["reasons"][reason] = total["reasons"].get(reason, 0) + n
    return total


def empty_counts():
    return {"edges": 0, "bound": 0, "boundary": 0, "new": 0, "seen": 0, "reasons": {}}


def check_bound(kind, counts):
    require(counts["bound"] > 0, f"{kind}: semantic run consumed no call bindings")


# --- MCP stdio session ----------------------------------------------------

class Mcp:
    def __init__(self, binary, workspace, home, log):
        self.log = log.open("wb")
        self.proc = subprocess.Popen([str(binary), "mcp", "--workspace", str(workspace)], cwd=REPO,
                                     env=V67.env_for(home), stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=self.log, bufsize=0)
        self.buffer = b""
        self.next_id = 0

    def send(self, message):
        self.proc.stdin.write(json.dumps(message, separators=(",", ":")).encode() + b"\n")

    def readline(self):
        end = time.monotonic() + max(0, min(HANG_BACKSTOP, remaining()))
        while b"\n" not in self.buffer:
            wait = end - time.monotonic()
            require(wait > 0 and select.select([self.proc.stdout], [], [], wait)[0],
                    "MCP response did not arrive")
            chunk = os.read(self.proc.stdout.fileno(), 1 << 20)
            require(chunk, f"MCP child exited (code {self.proc.poll()})")
            self.buffer += chunk
        line, self.buffer = self.buffer.split(b"\n", 1)
        return json.loads(line)

    def ask(self, method, params):
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        reply = self.readline()
        require(reply.get("id") == self.next_id, f"unexpected MCP reply: {str(reply)[:300]}")
        return reply

    def handshake(self):
        reply = self.ask("initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                        "clientInfo": {"name": "verify-40", "version": "1"}})
        require(reply["result"]["protocolVersion"] == "2025-11-25", f"MCP handshake failed: {reply}")
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def call(self, name, arguments, catching_up_ok=False):
        """(envelope, elapsed ms), timed from the request write to the complete response line."""
        start = time.perf_counter()
        reply = self.ask("tools/call", {"name": name, "arguments": {"schemaVersion": 1, **arguments}})
        elapsed = (time.perf_counter() - start) * 1000
        require("result" in reply, f"{name}: JSON-RPC error {str(reply)[:300]}")
        envelope = reply["result"]["structuredContent"]
        if catching_up_ok and envelope["catchingUp"]:
            return envelope, elapsed
        require(not reply["result"]["isError"] and "error" not in envelope,
                f"{name}: failed: {str(envelope)[:400]}")
        return envelope, elapsed

    def stop(self):
        try:
            if self.proc.poll() is None:
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    self.proc.wait(timeout=10)
        finally:
            self.log.close()


def wait_settled(mcp):
    end = time.monotonic() + max(0, min(HANG_BACKSTOP, remaining()))
    while True:
        envelope, _ = mcp.call("trellis_workspace_describe", {}, catching_up_ok=True)
        if not envelope["catchingUp"]:
            return
        require(time.monotonic() < end, "workspace still catching up")
        time.sleep(.25)


# --- one cohort -----------------------------------------------------------

def basis_of(pin):
    generation, revision = pin.removeprefix("pin:v1:").rsplit(":", 1)
    return {"indexGeneration": generation, "indexRevision": int(revision)}


def kind_arguments(kind, sample, samples, policy, producer, workspace):
    if kind in GRAPH_KINDS:
        entry = policy[POLICY_ENTRY[kind]]
        arguments = {"symbolId": samples["roots"][sample], "depth": entry["depth"],
                     "maxNodes": entry["maxNodes"], "maxCalls": entry["maxCalls"]}
        if kind in SEMANTIC_KINDS:
            arguments["semanticProducerId"] = producer
        return "trellis_call_graph", arguments
    if kind == "symbolSubstringSearch":
        return "trellis_find_symbols", {"query": samples["substrings"][sample]}
    if kind == "symbolSubstringSearchSparse":
        return "trellis_find_symbols", {"query": samples["sparseSearch"]["samples"][sample]["query"]}
    if kind == "status":
        return "trellis_workspace_describe", {}
    path = samples["documents"][sample]
    lines = max(1, len((workspace / path).read_bytes().splitlines()))
    return "trellis_read_source", {"path": path, "startLine": 1, "endLine": min(100, lines)}


def check_envelope(kind, envelope, basis, native_revision, producer):
    require(envelope["evidenceBasis"] == basis, f"{kind}: evidenceBasis {envelope['evidenceBasis']} != {basis}")
    if kind in GRAPH_KINDS:
        result = envelope["data"]["result"]
        require(result["resolvedRevisionId"] == native_revision,
                f"{kind}: resolvedRevisionId {result['resolvedRevisionId']} != {native_revision}")
        if kind in SEMANTIC_KINDS:
            require(envelope["data"]["requested"]["semanticProducerId"] == producer,
                    f"{kind}: requested producer is not {producer}")
            require(any(row["producerId"] == producer for row in result["coverage"]),
                    f"{kind}: no coverage row for {producer}")


def measure_kinds(mcp, kinds, samples, policy, producer, basis, native_revision, workspace, warmups, timed):
    results = {}
    for kind in kinds:
        millis, counts, reduced = [], empty_counts(), 0
        for n in range(warmups + timed):
            require(time.monotonic() < V67.DEADLINE, "global 1700-second budget exhausted")
            name, arguments = kind_arguments(kind, n % SAMPLES, samples, policy, producer, workspace)
            envelope, elapsed = mcp.call(name, arguments)
            check_envelope(kind, envelope, basis, native_revision, producer)
            if n < warmups:
                continue
            millis.append(elapsed)
            if kind in GRAPH_KINDS:
                add_counts(counts, edge_counts(envelope["data"]["result"]))
                reduced += envelope["truncationReason"] == "response_bytes"
        results[kind] = summarize(millis)
        if kind in GRAPH_KINDS:
            results[kind]["edgeCounts"] = counts
            results[kind]["responseBytesReduced"] = reduced
            if kind in SEMANTIC_KINDS:
                check_bound(kind, counts)
    return results


def native_revision_of(db, pin):
    row = db.execute("SELECT native_revision_id FROM native_revisions WHERE id=?", (pin,)).fetchone()
    require(row is not None, f"missing native revision for {pin}")
    return row[0]


def measure_cohort(binary, home, workspace, pin, size, floor, scope, semantic, proof, warmups, timed, policy):
    """Floor, coverage, then sample and time every kind at one pin.

    proof is (anchor path string, artifact hash) when semantic evidence is measured.
    """
    db = sqlite3.connect(f"file:{V67.db_for(home)}?mode=ro", uri=True)
    mcp = None
    try:
        producer = None
        if semantic:
            producer = V67.selected_proof(db, pin, proof[0], proof[1])["producer"]
        facts, samples = floored_samples(db, pin, size, floor, scope, producer)
        native_revision = native_revision_of(db, pin)
    finally:
        db.close()
    try:
        require((workspace / ".git").exists(), "the workspace must be a Git checkout before its first index")
        mcp = Mcp(binary, workspace, home, V67.EVIDENCE / f"{size}-mcp.stderr.log")
        mcp.handshake()
        wait_settled(mcp)
        kinds = SYNTAX_KINDS + (SEMANTIC_KINDS if semantic else ())
        results = measure_kinds(mcp, kinds, samples, policy, producer, basis_of(pin), native_revision,
                                workspace, warmups, timed)
    finally:
        if mcp is not None:
            mcp.stop()
    return {"facts": facts, "producer": producer, "coveredRoots": samples["rootCount"] if semantic else None,
            "kinds": results, "sparseSearch": samples["sparseSearch"]}


def pin_of(output):
    pin = json.loads(output)["publishedRevision"]
    return f"pin:v1:{pin['indexGeneration']}:{pin['indexRevision']}"


# --- modes ----------------------------------------------------------------

def smoke(binary):
    scratch = Path(tempfile.mkdtemp(prefix="trellis-40-smoke-")).resolve()
    configure_reuse(scratch, DEADLINE)
    workspace, home = scratch / "workspace", scratch / "home"
    home.mkdir(mode=0o700)
    init_checkout(workspace)
    for relative, text in SMOKE_SOURCES.items():
        path = workspace / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    daemon = V67.Daemon(binary, home, "smoke")
    stopped = ok = False
    try:
        daemon.start()
        r1 = pin_of(V67.command([str(binary), "index", "--workspace", str(workspace)],
                                env=V67.env_for(home)))
        artifact, pin = V67.import_fixture(binary, workspace, daemon, "smoke", r1)
        for rejected in ({}, {"java": 0, "javascript": 1, "python": 1, "rust": 1}):
            try:
                check_floor(rejected, "smoke", 1, "perLanguage")
            except RuntimeError:
                continue
            raise RuntimeError("smoke floor accepted a zero-fact input")
        cohort = measure_cohort(binary, home, workspace, pin, "smoke", 1, "perLanguage", True,
                                ("javascript/Csmoke0010.js", artifact), 0, 3, load_policy())
        stop_cohort_daemon(daemon, binary)
        stopped = True
        require_clean_teardown()
        ok = True
        print("SMOKE_40_RESULT=" + json.dumps(
            {"pin": pin, "unexpectedHomeProcesses": False, "producer": cohort["producer"], "facts": cohort["facts"],
             "coveredRoots": cohort["coveredRoots"],
             "kinds": cohort["kinds"], "sparseSearch": cohort["sparseSearch"],
             "edgeCounts": {k: cohort["kinds"][k]["edgeCounts"] for k in GRAPH_KINDS}},
            sort_keys=True), flush=True)
    finally:
        try:
            if not stopped:
                stop_all([daemon], binary)
        finally:
            if ok:
                shutil.rmtree(scratch, ignore_errors=True)
            else:
                print(f"smoke evidence retained at {scratch}", file=sys.stderr, flush=True)


def teardown_check(binary):
    """Untimed: seed and import the medium cohort as --semantic does, then tear it down."""
    require(binary.exists(), f"missing binary: {binary}")
    scratch = Path(tempfile.mkdtemp(prefix="trellis-40-teardown-")).resolve()
    configure_reuse(scratch, DEADLINE)
    corpus = scratch / "canonical"
    V67.command(["node", str(GENERATOR), "--out", str(corpus)])
    workspace, home = CheckoutPath(scratch / "medium-workspace"), scratch / "medium-private-home"
    home.mkdir(mode=0o700)
    daemon = V67.Daemon(binary, home, "medium")
    stopped = False
    try:
        daemon.start()
        pin, _, _, _ = V67.seed_workspace(binary, corpus, workspace, daemon, "medium")
        mcp = Mcp(binary, workspace, home, scratch / "medium-mcp.stderr.log")
        try:
            mcp.handshake()
            wait_settled(mcp)
        finally:
            mcp.stop()
        stop_cohort_daemon(daemon, binary)
        stopped = True
        require_clean_teardown()
    finally:
        if not stopped:
            stop_all([daemon], binary)
    print("TEARDOWN_40_RESULT=" + json.dumps({"pin": pin, "unexpectedHomeProcesses": False}), flush=True)
    shutil.rmtree(scratch, ignore_errors=True)


def measure(semantic):
    require(platform.system() == "Darwin", "AC12-AC16 must run on the reference Mac mini")
    scratch = Path(tempfile.mkdtemp(prefix="trellis-40-reference-")).resolve()
    configure_reuse(scratch, DEADLINE)
    model = V67.command(["sysctl", "-n", "hw.model"]).strip()
    require(model == "Mac17,16", f"wrong reference host: {model}")
    pinned = PINNED.read_bytes()
    V67.command(["cargo", "build", "--locked", "--release"])
    corpus = scratch / "canonical"
    V67.command(["node", str(GENERATOR), "--out", str(corpus)])
    require((corpus / "manifest.json").read_bytes() == pinned,
            "generator output does not exactly match pinned manifest bytes/hashes")
    policy, floors = load_policy(), load_floors()
    sizes, live = {}, []
    try:
        for size in ("medium", "large"):
            workspace, home = CheckoutPath(scratch / f"{size}-workspace"), scratch / f"{size}-private-home"
            home.mkdir(mode=0o700)
            daemon = V67.Daemon(RELEASE_BINARY, home, size)
            live.append(daemon)
            daemon.start()
            proof = None
            if semantic:
                pin, artifact, anchor, _ = V67.seed_workspace(RELEASE_BINARY, corpus, workspace, daemon, size)
                proof = (anchor.as_posix(), artifact)
            else:
                shutil.copytree(corpus / size, workspace)
                init_checkout(workspace)
                pin = pin_of(V67.command([str(RELEASE_BINARY), "index", "--workspace", str(workspace)],
                                         env=V67.env_for(home)))
            floor, scope = floors[size]
            sizes[size] = measure_cohort(RELEASE_BINARY, home, workspace, pin, size, floor, scope,
                                         semantic, proof, WARMUPS, SAMPLES, policy)
            sizes[size]["anchorPath"] = proof[0] if proof else None
            sizes[size]["artifactSha256"] = proof[1] if proof else None
            # Stop this cohort's daemon before the next one is timed.
            stop_cohort_daemon(daemon, RELEASE_BINARY)
            live.remove(daemon)
    finally:
        # Interrupted cohorts only: stopped daemons were already removed.
        stop_all(live, RELEASE_BINARY)
    require_clean_teardown()
    report = build_report(model, hashlib.sha256(pinned).hexdigest(), semantic, sizes, policy)
    print("REFERENCE_40_RESULT=" + json.dumps(report, sort_keys=True), flush=True)
    if not report["pass"]:
        return 1
    shutil.rmtree(scratch, ignore_errors=True)
    return 0


def build_report(host, manifest_sha, semantic, sizes, policy):
    report_sizes, passed = {}, True
    for size, cohort in sizes.items():
        kinds = {}
        for kind, stats in cohort["kinds"].items():
            if kind in SEMANTIC_KINDS:
                check_bound(kind, stats["edgeCounts"])
            result = evaluate(kind, {k: v for k, v in stats.items()
                                     if k not in ("edgeCounts", "responseBytesReduced")}, policy)
            if kind in GRAPH_KINDS:
                result["edgeCounts"] = stats["edgeCounts"]
                result["responseBytesReduced"] = stats["responseBytesReduced"]
            kinds[kind] = result
            passed = passed and result["pass"]
        entry = {"facts": cohort["facts"], "kinds": kinds}
        if "sparseSearch" in cohort:
            entry["sparseSearch"] = cohort["sparseSearch"]
        if semantic:
            entry.update(producer=cohort["producer"], anchorPath=cohort["anchorPath"],
                         artifactSha256=cohort["artifactSha256"], coveredRoots=cohort["coveredRoots"])
        report_sizes[size] = entry
    limitations = " ".join([LIMITATION_IDENTICAL, LIMITATION_SEMANTIC if semantic else LIMITATION_SYNTAX])
    return {"host": host, "manifestSha256": manifest_sha,
            "method": "trellis mcp --workspace over stdio on the release binary; 20 warm-up then 200 "
                      "sequential timed calls per kind, request write to complete response line; "
                      "nearest-rank p95/p99 after the facts floor; isolated HOME/XDG",
            "evidence": "semantic" if semantic else "syntax-only",
            "sizes": report_sizes, "limitations": limitations, "pass": passed}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--smoke", action="store_true", help="untimed semantic-path proof")
    parser.add_argument("--teardown-check", action="store_true",
                        help="untimed medium-cohort seed, MCP session and teardown; no gates")
    parser.add_argument("--binary", help="trellis binary, required with --smoke or --teardown-check")
    parser.add_argument("--semantic", action="store_true", help="also measure imported SCIP evidence")
    args = parser.parse_args(argv)
    if args.smoke and args.semantic:
        parser.error("--semantic cannot be combined with --smoke: the smoke always runs the semantic path")
    if args.teardown_check and (args.smoke or args.semantic):
        parser.error("--teardown-check cannot be combined with --smoke or --semantic")
    if (args.smoke or args.teardown_check) != (args.binary is not None):
        parser.error("--binary is required with --smoke or --teardown-check, and only valid with them")
    try:
        if args.smoke:
            smoke(Path(args.binary).resolve())
            return 0
        if args.teardown_check:
            teardown_check(Path(args.binary).resolve())
            return 0
        return measure(args.semantic)
    except (RuntimeError, subprocess.TimeoutExpired, sqlite3.Error, KeyError) as exc:
        print(f"query-performance gate FAILED: {exc}", file=sys.stderr, flush=True)
        return 1


if __name__ == "__main__":
    sys.exit(main())
