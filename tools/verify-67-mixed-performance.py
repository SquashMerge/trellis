#!/usr/bin/env python3
"""Supplemental #67 mixed-body local-update measurements on mac-mini-m5-pro-v1.

Runs the real release CLI. Every timed publication is checked against its own
persisted revision manifest before any percentile is computed.
"""
import argparse
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
PINNED = REPO / "tools/synthetic-cohorts/manifest-v1.json"
GENERATOR = REPO / "tools/synthetic-cohorts/generate.mjs"
TARGET_DIR = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target"))
if not TARGET_DIR.is_absolute():
    TARGET_DIR = REPO / TARGET_DIR
BINARY = TARGET_DIR / "release/trellis"
DEADLINE = time.monotonic() + 1700  # below Factory's 1800-second verifier timeout
FACTS = """SELECT m.language,SUM(
 (SELECT count(*) FROM native_version_declarations d WHERE d.version_id=m.document_version_id)+
 (SELECT count(*) FROM native_version_calls c WHERE c.version_id=m.document_version_id)+
 (SELECT count(*) FROM native_version_control_regions r WHERE r.version_id=m.document_version_id))
 FROM revision_documents m WHERE m.revision_id=? GROUP BY m.language ORDER BY m.language"""


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def command(args, *, env=None, timed=False):
    remain = DEADLINE - time.monotonic()
    require(remain > 10, "global 1700-second budget exhausted")
    start = time.monotonic()
    result = subprocess.run(args, cwd=REPO, env=env, capture_output=True, text=True,
                            timeout=min(remain, 1700))
    duration = time.monotonic() - start
    require(result.returncode == 0, f"{args[0]} {args[1:]} failed ({result.returncode}): "
            f"{result.stderr[-2500:]} {result.stdout[-1000:]}")
    return (duration, result.stdout, result.stderr) if timed else result.stdout


def db_for(home):
    # HOME and both XDG directories are private scratch. Never use real user state.
    found = list(home.rglob("index.db"))
    require(len(found) == 1 and "indexes" in found[0].parts,
            f"expected exactly one isolated index.db, found {found}")
    return found[0]


def published(db, output, size):
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


def check_selected_local_ids(db, prior, current, changed_path, size, n):
    expected_docs = 1000 if size == "medium" else 10000
    prior_count = db.execute(
        "SELECT count(*) FROM revision_documents WHERE revision_id=?", (prior,)
    ).fetchone()[0]
    # Join by the FULL selected document key. Counts alone could
    # hide a second changed path whose IDs coincidentally offset.
    identity = db.execute("""SELECT count(*),
        sum(a.document_version_id=b.document_version_id),
        sum(a.graph_projection_id=b.graph_projection_id),
        sum(a.class_projection_id IS b.class_projection_id),
        sum(CASE WHEN a.path=?3 THEN 1 ELSE 0 END),
        sum(CASE WHEN a.path=?3
            AND a.document_version_id!=b.document_version_id
            AND a.graph_projection_id!=b.graph_projection_id
            AND a.class_projection_id IS NOT b.class_projection_id
            THEN 1 ELSE 0 END),
        sum(CASE WHEN a.path!=?3
            AND (a.document_version_id!=b.document_version_id
                 OR a.graph_projection_id!=b.graph_projection_id
                 OR a.class_projection_id IS NOT b.class_projection_id)
            THEN 1 ELSE 0 END)
        FROM revision_documents a JOIN revision_documents b
        USING(source_set_id,language,path)
        WHERE a.revision_id=?1 AND b.revision_id=?2""",
        (current, prior, changed_path)).fetchone()
    reuse = dict(zip(("documents", "native_versions", "graph_projections",
                      "class_projections"), identity[:4]))
    require(prior_count == expected_docs and reuse["documents"] == expected_docs
            and identity[4:] == (1, 1, 0)
            and all(reuse[name] == expected_docs - 1 for name in
                ("native_versions", "graph_projections", "class_projections")),
            f"{size} run {n}: full-key prior/selected ID proof failed: "
            f"prior={prior_count}, reuse={reuse}, target/changed/other={identity[4:]}")
    return reuse


def index(binary, root, home, size):
    env = os.environ.copy()
    env.update(HOME=str(home), XDG_CACHE_HOME=str(home / ".cache"),
               XDG_DATA_HOME=str(home / ".local/share"), RUST_LOG="off")
    elapsed, output, stderr = command([str(binary), "index", "--workspace", str(root)], env=env, timed=True)
    modes = re.findall(r"^index-mode (local|full)$", stderr, re.M)
    require(len(modes) == 1, f"expected exactly one completed release publication, got {modes}: {stderr[-1500:]}")
    phases = {name: float(ms) for name, ms in re.findall(r"^index-phase (\w+)_ms=([0-9.]+)", stderr, re.M)}
    require(all(name in phases for name in ("capture", "measure", "compose", "attest", "publish",
            "outside_setup", "queue_and_jobs", "outside_status_output")),
            f"missing release phase marker: {stderr[-1500:]}")
    # Postcommit release metrics are counted by the writer while binding rows.
    # No full-db dbstat scan can contaminate or consume the measurement budget.
    writer_lines = re.findall(r"^index-writer (.+)$", stderr, re.M)
    require(len(writer_lines) == 1, f"missing unique postcommit writer metrics: {stderr[-1500:]}")
    values = dict((key, int(value)) for key, value in
                  re.findall(r"(\w+)=(\d+)", writer_lines[0]))
    families = ("manifest", "native", "graph", "class")
    writer = {name: {"rows": values[f"{name}_rows"],
                     "bind_bytes": values[f"{name}_bind_bytes"]}
              for name in (*families, "total")}
    require(writer["total"] == {key: sum(writer[name][key] for name in families)
                                 for key in ("rows", "bind_bytes")},
            f"writer family accounting mismatch: {writer}")
    require(values["reused_occurrence_reads"] == 0,
            "unchanged occurrence was read under publish transaction")
    db = sqlite3.connect(f"file:{db_for(home)}?mode=ro", uri=True)
    try:
        selected = published(db, output, size)  # own produced-index floor before p95
    finally:
        db.close()
    return elapsed, {**selected, "mode": modes[0], "phases_ms": phases,
                     "writer_bind": writer,
                     "reused_occurrence_reads": values["reused_occurrence_reads"]}


def edit_leaf(path, language, kind):
    """One proven method/function-body edit, never a declaration or import.

    Every timed revision has its own changed path and full produced-fact checks.
    A returned identifier is accepted only when both names are local bindings.
    """
    raw = path.read_bytes()
    if kind == "identifier":
        peer = b"peerValue" if language in ("java", "javascript") else b"peer_value"
        marker = b"return local+" + peer
        require(marker in raw, f"non-numeric local return absent: {path}")
        first = raw.index(marker) + len(b"return ")
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


def parity_digest(db, revision):
    """Independent row content comparison for the selected document versions and projections.

    Avoid generated revision IDs, volatile capture metadata, and linked row order.
    All hashed rows come from selected manifest joins, never the unselected history.
    """
    sql = {
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
    result = {}
    for name, query in sql.items():
        hash_value = hashlib.sha256()
        count = 0
        for row in db.execute(query, (revision,)):
            hash_value.update(json.dumps(row, ensure_ascii=False, separators=(",", ":")).encode())
            hash_value.update(b"\n")
            count += 1
        result[name] = (count, hash_value.hexdigest())
    return result


def main():
    parser = argparse.ArgumentParser(description="Additional mixed AC8 evidence; run each size separately under the verifier timeout")
    parser.add_argument("--size", choices=("medium", "large"), required=True)
    selected_size = parser.parse_args().size
    signal.signal(signal.SIGALRM, lambda *_: (_ for _ in ()).throw(RuntimeError("reference 1750-second hard deadline")))
    signal.alarm(1750)
    require(platform.system() == "Darwin", "AC8 must run on actual Mac mini M5 Pro")
    model = command(["sysctl", "-n", "hw.model"]).strip()
    require(model == "Mac17,16", f"wrong reference host: {model}")
    pinned = PINNED.read_bytes()
    parsed = json.loads(pinned)
    require(parsed["version"] == 1 and parsed["seed"] == "trellis-synthetic-cohorts-v1", "unknown manifest")
    command(["cargo", "build", "--locked", "--release"])
    with tempfile.TemporaryDirectory(prefix="trellis-67-reference-") as scratch:
        scratch = Path(scratch)
        corpus = scratch / "canonical"
        command(["node", str(GENERATOR), "--out", str(corpus)])
        require((corpus / "manifest.json").read_bytes() == pinned,
                "generator output does not exactly match pinned manifest bytes/hashes")
        print(f"canonical manifest sha256={hashlib.sha256(pinned).hexdigest()}; host={model}", flush=True)
        measurements = {}
        for size in (selected_size,):
            workspace = scratch / f"{size}-workspace"
            home = scratch / f"{size}-private-home"
            home.mkdir(mode=0o700)
            shutil.copytree(corpus / size, workspace)
            setup_seconds, initial = index(BINARY, workspace, home, size)
            require(initial["mode"] == "full", f"{size}: cold setup must fully measure the pinned corpus")
            print(json.dumps({"size": size, "phase": "initial_cold", "seconds": round(setup_seconds, 4),
                              "index_phases_ms": initial["phases_ms"]}), flush=True)
            samples = []
            edited_paths = set()
            for n in range(20):  # independent, mixed-language single-document revisions
                language = ("java", "python", "javascript", "rust")[n % 4]
                suffix = {"java": "java", "python": "py", "javascript": "js", "rust": "rs"}[language]
                leaf_ordinal = n // 4
                relative = Path(language) / f"C{size}{leaf_ordinal:04d}.{suffix}"
                if language == "rust":
                    relative = Path("rust/g00") / relative.name
                leaf = workspace / relative
                changed_path = leaf.relative_to(workspace).as_posix()
                require(changed_path not in edited_paths, f"{size}: duplicate edited leaf {changed_path}")
                edited_paths.add(changed_path)
                kind = ("grow", "fixed", "grow", "identifier", "identifier")[leaf_ordinal]
                before_literal, after_literal = edit_leaf(leaf, language, kind)
                seconds, own = index(BINARY, workspace, home, size)
                require(own["mode"] == "local", f"{size} run {n}: not a proven-local publication")
                if size == "large":
                    require(all(0 < own["phases_ms"][phase] <= seconds * 1000 for phase in
                                ("capture", "measure", "compose", "attest", "publish", "queue_and_jobs")),
                            f"large run {n}: missing or invalid per-local phase diagnostics: {own['phases_ms']}")
                after_db = sqlite3.connect(f"file:{db_for(home)}?mode=ro", uri=True)
                try:
                    current = own["revision"]
                    generation, number = current.rsplit(":", 1)
                    prior = f"{generation}:{int(number)-1}"
                    reuse = check_selected_local_ids(after_db, prior, current, changed_path, size, n)
                finally:
                    after_db.close()
                counters = own["writer_bind"]
                require(counters["manifest"]["rows"] >= (1000 if size == "medium" else 10000),
                        f"{size}: missing O(documents) manifest")
                require(counters["native"]["rows"] > 0 and counters["graph"]["rows"] > 0,
                        f"{size}: update did not create a native/graph projection")
                require(counters["native"]["rows"] < (100000 if size == "medium" else 1000000),
                        f"{size}: local update unexpectedly rewrote too many native rows: {counters}")
                sample = {"seconds": round(seconds, 4), **own, "reuse": reuse,
                          "changed_path": leaf.relative_to(workspace).as_posix(),
                          "old_literal": before_literal, "new_literal": after_literal}
                samples.append(sample)
                print(json.dumps({"size": size, "run": len(samples), **sample}), flush=True)
                # At sample 20 the nearest-rank p95 is the 19th observation:
                # two misses make success mathematically impossible. Stop before
                # wasting the verifier budget; this is NOT a calculated p95.
                limit = 2 if size == "medium" else 5
                if sum(s["seconds"] > limit for s in samples) >= 2:
                    raise RuntimeError(f"{size}: two local samples exceed {limit}s; "
                                       f"20-run nearest-rank p95 cannot pass; owner decision required. "
                                       f"observed seconds={[s['seconds'] for s in samples]}")
            # Nearest-rank empirical p95. Floor assertions above MUST precede this line.
            p95 = sorted(s["seconds"] for s in samples)[math.ceil(.95 * len(samples)) - 1]
            measurements[size] = {"cold_setup_seconds": round(setup_seconds, 4),
                                  "cold_setup": initial,
                                  "runs": samples,
                                  "local_p95_seconds": p95, "local_threshold_seconds": 2 if size == "medium" else 5}
            require(p95 <= measurements[size]["local_threshold_seconds"],
                    f"{size} local p95 {p95}s exceeds {measurements[size]['local_threshold_seconds']}s; owner decision required")
            # Independent FULL/cold publication of the exact final mixed
            # revision, using the same workspace root but isolated index state.
            # This is not an extra timed LOCAL sample and cannot hide misses.
            independent = scratch / f"{size}-mixed-independent-home"
            independent.mkdir(mode=0o700)
            cold_seconds, cold = index(BINARY, workspace, independent, size)
            require(cold["mode"] == "full", f"{size}: independent same-root oracle must be FULL")
            print(json.dumps({"size": size, "phase": "independent_cold", "seconds": round(cold_seconds, 4),
                              "index_phases_ms": cold["phases_ms"]}), flush=True)
            warm_db = sqlite3.connect(f"file:{db_for(home)}?mode=ro", uri=True)
            cold_db = sqlite3.connect(f"file:{db_for(independent)}?mode=ro", uri=True)
            try:
                warm_digest = parity_digest(warm_db, samples[-1]["revision"])
                cold_digest = parity_digest(cold_db, cold["revision"])
            finally:
                warm_db.close()
                cold_db.close()
            require(warm_digest == cold_digest,
                    f"{size}: final mixed LOCAL graph/native/class F differs from independent cold oracle: " +
                    json.dumps({key: [warm_digest[key], cold_digest[key]]
                                for key in warm_digest if warm_digest[key] != cold_digest[key]}))
            measurements[size]["independent_cold_seconds"] = round(cold_seconds, 4)
            measurements[size]["independent_cold"] = cold
            measurements[size]["row_digest_parity"] = warm_digest
        report = {"host": model, "chip": "Apple M5 Pro", "manifest_sha256": hashlib.sha256(pinned).hexdigest(),
                  "method": "ADDITIVE mixed-body cohort only: frozen canonical corpus and floors, 20 distinct local edits per size (5 Java, 5 Python, 5 JS, 5 Rust; 8 length-changing and 8 nonnumeric per size), full CLI wall time, nearest-rank p95, independent cold FULL parity at final selected mixed revision, no excluded failures; original frozen 20+20 Python method remains in verify-67-performance.py",
                  "size": selected_size, "measurements": measurements,
                  "writer_note": "postcommit per-family row and SQLite-bound byte counters and zero reused occurrence reads; no dbstat payload or file-system growth"}
        print("REFERENCE_67_MIXED_RESULT=" + json.dumps(report, sort_keys=True), flush=True)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.TimeoutExpired, sqlite3.Error) as exc:
        print(f"mixed reference-performance gate FAILED: {exc}", file=sys.stderr, flush=True)
        sys.exit(1)
