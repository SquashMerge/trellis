#!/usr/bin/env python3
"""Run the #61 combined native + semantic-import publication gate campaign.

Three fixed repetitions per size (small, medium, large), no warmup, replacement
or extra sample. Each repetition times one contiguous interval: the full native
index request through the final successful semantic-import CLI return. The
report is never authoritative acceptance and leaves #11 open. `--smoke` runs the
untimed structural path with no binary, daemon or lock.

    python3 tools/verify-61-combined-gate.py --campaign-dir <absolute nonexistent dir> \
        [--binary <release trellis>] [--smoke]
"""

import argparse
import hashlib
import importlib.util
import json
import os
import platform
import shutil
import sqlite3
import subprocess
import sys
import threading
import time
from contextlib import closing
from datetime import datetime, timezone
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
EVIDENCE_DIR = REPO / "docs/semantic-evidence"
POLICY_PATH = EVIDENCE_DIR / "benchmark-policy-v1.json"
COHORTS_PATH = EVIDENCE_DIR / "cohorts-v1.json"
PREPARATION_PATH = EVIDENCE_DIR / "performance-gate-preparation.json"
SYNTHETIC_PROFILE_PATH = EVIDENCE_DIR / "synthetic-profiles/trellis-synthetic-scip-v1.json"
METHOD_PATH = EVIDENCE_DIR / "performance-gate-method.md"
GENERATE_COHORT = REPO / "tools/synthetic-cohorts/generate.mjs"
GENERATE_SCIP = REPO / "tools/semantic-contract/generate-performance-scip.py"
QUALIFIER = REPO / "tools/semantic-contract/qualify-performance.py"
BATCH_MODULE = REPO / "tools/semantic-contract/semantic_batch.py"
RUNNER_90 = REPO / "tools/verify-67-mixed-performance.py"
HASHED_FILES = (
    "docs/semantic-evidence/contract-v1.md",
    "docs/semantic-evidence/decisions/0002-publication-rejoin.md",
    "docs/semantic-evidence/benchmark-policy-v1.json",
    "docs/semantic-evidence/cohorts-v1.json",
    "docs/semantic-evidence/check-policy.mjs",
    "tools/synthetic-cohorts/manifest-v2.json",
    "docs/semantic-evidence/synthetic-profiles/trellis-synthetic-scip-v1.json",
    "tools/semantic-contract/generate-performance-scip.py",
    "tools/semantic-contract/semantic_batch.py",
    "tools/semantic-contract/qualify-performance.py",
    "docs/semantic-evidence/performance-gate-preparation.json",
    "docs/semantic-evidence/performance-gate-method.md",
    "tools/verify-61-combined-gate.py",
    "tools/verify-67-mixed-performance.py",
)
SIZES = ("small", "medium", "large")
REPETITIONS = 3
LANGUAGES = ("java", "rust", "python", "javascript")
EXTENSION_LANGUAGE = {".java": "java", ".rs": "rust", ".py": "python", ".js": "javascript",
                      ".mjs": "javascript", ".cjs": "javascript"}
NON_DURABLE_PREFIXES = ("/tmp", "/private/tmp", "/var/folders", "/private/var/folders")
SAMPLE_INTERVAL = 0.05
SMOKE_HEADER = {
    "trustOrigin": "syntheticProfile", "profileId": "trellis-synthetic-scip-v1",
    "decoderVersion": None,
    "producer": None,
    "sourceSet": {"id": "sourceset:smoke", "rootId": "root:smoke", "languages": list(LANGUAGES),
                  "dependencies": []},
    "sourceRevision": "revision:v2:smoke", "sourceManifestHash": None,
    "components": {"toolchainHash": "0" * 64, "configHash": "1" * 64, "dependencyHash": "2" * 64},
    "metadata": {"protocolVersion": 0, "projectRoot": "file:///smoke-workspace",
                 "textDocumentEncoding": 1},
}
STATEMENTS = {
    "cold": "virgin Trellis state: fresh HOME/XDG, workspace, daemon and derived index per "
            "repetition; OS and SSD caches are not purged; preparation and the header probe "
            "warm the page cache",
    "producerGenerationIncluded": False,
    "artifacts": "synthetic (trellis-synthetic-scip, generatorSource identity); "
                 "no real-producer qualification",
}
INCLUSION = {
    "included": ["native capture/index", "semantic admission", "decode", "join", "import",
                 "SQLite publication", "FIFO waits"],
    "excluded": ["cohort generation", "artifact generation", "header probe", "batch preparation",
                 "daemon start", "post-run validation", "daemon stop"],
}
WORKER_RSS_METHOD = ("os.wait4 ru_maxrss of the daemon process; lifetime high-water including "
                     "setup and post-run validation")
DENOMINATOR_REASON = ("combined canonical serialized-evidence container unratified "
                      "(#61 open item 2)")


class GateFault(RuntimeError):
    """Evidence is unqualified; the campaign stops."""


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


m = load_module("mixed_runner", RUNNER_90)
batch = load_module("semantic_batch", BATCH_MODULE)
qualifier = load_module("qualify_performance", QUALIFIER)


def canonical_json(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, indent=2, allow_nan=False) + "\n"


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def utc_now():
    return datetime.fromtimestamp(time.time(), timezone.utc).isoformat(timespec="milliseconds")


def utc_from_ms(milliseconds):
    return datetime.fromtimestamp(milliseconds / 1000, timezone.utc).isoformat(timespec="milliseconds")


def capture(argv, cwd=REPO):
    """Run a short inspection command; None when it is absent or fails."""
    try:
        result = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    return (result.stdout or result.stderr).strip()


class Gate90Adapter:
    """Thin adapter over the #90 runner: no second lifecycle or command framework."""

    Daemon = m.Daemon
    command = staticmethod(m.command)
    env_for = staticmethod(m.env_for)
    db_for = staticmethod(m.db_for)
    canonical = staticmethod(m.canonical)
    sha_file = staticmethod(m.sha_file)
    copy_cohort = staticmethod(m.copy_cohort)
    capture_header = staticmethod(m.capture_header)
    run_semantic_import = staticmethod(m.run_semantic_import)
    FACTS = m.FACTS

    def __init__(self, binary):
        self.binary = binary
        m.BINARY = binary

    def begin_repetition(self, rep_dir):
        rep_dir.mkdir(parents=True, exist_ok=True)
        m.EVIDENCE = rep_dir
        m.DEADLINE = time.monotonic() + 1700
        m.BINARY = self.binary

    def daemon(self, home, label, record_rusage=False):
        return self.Daemon(self.binary, home, label, record_rusage=record_rusage)


# ---------------------------------------------------------------- policy and host

def read_policy():
    """Thresholds and floors, read from the ratified files only. Missing keys raise GateFault."""
    try:
        policy = json.loads(POLICY_PATH.read_text())
        cohorts = json.loads(COHORTS_PATH.read_text())
        full = policy["performance"]["fullIndex"]
        storage = policy["performance"]["storage"]
        sizes = {}
        for size in SIZES:
            row = full[size]
            if ("rssMiB" in row) == ("rssGiB" in row):
                raise KeyError(f"fullIndex.{size} needs exactly one of rssMiB/rssGiB")
            key, scale = ("rssMiB", 2**20) if "rssMiB" in row else ("rssGiB", 2**30)
            sizes[size] = {"medianSeconds": row["medianSeconds"], key: row[key],
                           "rssBytes": row[key] * scale}
        read = {
            "fullIndex": {"includedStages": full["includedStages"],
                          "producerGenerationIncluded": full["producerGenerationIncluded"],
                          "sizes": sizes},
            "storage": {key: storage[key] for key in (
                "steadyToCanonicalSerializedEvidenceMaxMultiplier", "steadyMaxGiB",
                "steadyLimitsCombinedBy", "publicationPeakToSteadyMaxMultiplier",
                "publicationPeakIncludes")},
            "cohorts": {size: {key: cohorts["synthetic"][size][key] for key in (
                "files", "minimumNormalizedFacts", "factsScope")} for size in SIZES},
        }
        profile_id = policy["referenceHost"]["profileId"]
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise GateFault(f"preflight: policy or cohort file unreadable or incomplete: {error!r}") from None
    return read, policy, cohorts, profile_id


def read_host_profile(profile_id):
    path = Path("docs/semantic-evidence/hardware") / f"{profile_id}.json"
    try:
        data = (REPO / path).read_bytes()
        profile = json.loads(data)
        if profile["profileId"] != profile_id:
            raise ValueError(f"profileId {profile['profileId']!r} differs from {profile_id!r}")
        model = profile["observed"]["modelIdentifier"]
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise GateFault(f"preflight: host profile {path} unusable: {error!r}") from None
    return {"profileId": profile_id, "profilePath": path.as_posix(),
            "profileSha256": sha256_bytes(data), "modelIdentifier": model}


def host_inventory(campaign_dir, capture_fn=capture):
    sysctl = capture_fn(["sysctl", "-n", "hw.model", "machdep.cpu.brand_string", "hw.memsize",
                         "hw.ncpu"])
    lines = (sysctl or "").splitlines()
    lines += [None] * (4 - len(lines))
    try:
        load = [round(value, 2) for value in os.getloadavg()]
    except OSError:
        load = None
    return {"hwModel": lines[0], "cpuBrand": lines[1], "memBytes": lines[2], "ncpu": lines[3],
            "swVers": capture_fn(["sw_vers"]), "uname": capture_fn(["uname", "-a"]),
            "dfK": capture_fn(["df", "-k", str(campaign_dir)]), "loadAvg": load}


def is_reference(profile, inventory, system=None):
    system = platform.system() if system is None else system
    return system == "Darwin" and inventory.get("hwModel") == profile["modelIdentifier"]


def file_hashes():
    out = {}
    for relative in HASHED_FILES:
        path = REPO / relative
        out[relative] = sha256_bytes(path.read_bytes()) if path.exists() else None
    return out


def preflight(args, campaign_dir, smoke, capture_fn=capture):
    """Collect campaign.json fields. `failures` holds preflight reasons (unqualified)."""
    failures = []
    head = capture_fn(["git", "rev-parse", "HEAD"])
    if head is None:
        failures.append("preflight: git HEAD unavailable")
    tree_clean = None
    if not smoke:
        status = capture_fn(["git", "status", "--porcelain"])
        tree_clean = status == ""
        if not tree_clean:
            failures.append("preflight: working tree is not clean" if status is not None
                            else "preflight: git status unavailable")
    binary = None
    if args.binary_path is not None:
        data = args.binary_path.read_bytes() if args.binary_path.is_file() else None
        binary = {"path": str(args.binary_path),
                  "sha256": sha256_bytes(data) if data is not None else None,
                  "version": capture_fn([str(args.binary_path), "--version"])}
        if data is None:
            failures.append(f"preflight: binary {args.binary_path} is missing")
    elif not smoke:
        failures.append("preflight: no release binary")
    toolchain_file = REPO / "rust-toolchain.toml"
    toolchain = {"rustc": capture_fn(["rustc", "-V"]), "cargo": capture_fn(["cargo", "-V"]),
                 "rustToolchainSha256": sha256_bytes(toolchain_file.read_bytes())
                 if toolchain_file.exists() else None,
                 "node": capture_fn(["node", "-v"]), "python3": capture_fn(["python3", "-V"]),
                 "sqlite3": capture_fn(["sqlite3", "--version"])}
    policy_read = {}
    profile = {"profileId": None, "profilePath": None, "profileSha256": None, "modelIdentifier": None}
    policy_json = cohorts_json = None
    try:
        policy_read, policy_json, cohorts_json, profile_id = read_policy()
        profile = read_host_profile(profile_id)
    except GateFault as fault:
        failures.append(str(fault))
    inventory = None if smoke else host_inventory(campaign_dir, capture_fn)
    reference = False if smoke or profile["modelIdentifier"] is None else is_reference(profile, inventory)
    campaign = {
        "argv": [sys.executable, str(Path(__file__).resolve())]
        + (["--campaign-dir", "<campaign-dir>", "--smoke"] if smoke else [str(arg) for arg in sys.argv[1:]]),
        "source": {"head": head, "treeClean": tree_clean},
        "binary": binary, "toolchain": toolchain, "hashes": file_hashes(),
        "decoderVersion": m.DECODER_VERSION, "schemaVersion": m.SCHEMA_VERSION,
        "host": {**profile, "reference": reference, "inventory": inventory},
    }
    return campaign, policy_read, policy_json, cohorts_json, failures


# ------------------------------------------------------------------ preparation

def run_logged(argv, label, campaign_dir, smoke):
    started = time.monotonic()
    result = subprocess.run([str(arg) for arg in argv], cwd=REPO, capture_output=True, text=True)
    seconds = None if smoke else round(time.monotonic() - started, 3)
    log = campaign_dir / "preparation" / f"{label}.log"
    log.parent.mkdir(parents=True, exist_ok=True)
    log.write_text(result.stdout + result.stderr)
    if result.returncode != 0:
        raise GateFault(f"preflight: preparation step {label} failed ({result.returncode}): "
                        f"{result.stderr[-500:]}")
    return seconds


def prepare(campaign_dir, sizes, smoke):
    """Generate the cohort and artifacts untimed; artifact hashes must equal the record."""
    record = json.loads(PREPARATION_PATH.read_text())
    prep_dir = campaign_dir / "preparation"
    cohort = prep_dir / "cohort"
    prep_dir.mkdir(parents=True, exist_ok=True)
    argv = ["node", GENERATE_COHORT, "--out", cohort]
    if smoke:
        argv += ["--size", "small"]
    steps = {"cohort": run_logged(argv, "cohort", campaign_dir, smoke)}
    prepared = {}
    for size in sizes:
        out = prep_dir / "artifacts" / size
        steps[f"artifacts-{size}"] = run_logged(
            [sys.executable, GENERATE_SCIP, "--cohort", cohort, "--size", size, "--out", out],
            f"artifacts-{size}", campaign_dir, smoke)
        inventory_bytes = (out / "inventory.json").read_bytes()
        inventory = json.loads(inventory_bytes)
        expected = {a["language"]: a["sha256"] for a in record["sizes"][size]["artifacts"]}
        actual = {a["language"]: sha256_bytes((out / a["relativePath"]).read_bytes())
                  for a in inventory["artifacts"]}
        if actual != expected:
            raise GateFault(f"preflight: {size} artifact hashes differ from performance-gate-preparation.json")
        prepared[size] = {"dir": out, "inventory": inventory,
                          "inventorySha256": sha256_bytes(inventory_bytes)}
    summary = {"cohortDir": "preparation/cohort", "seconds": steps,
               "sizes": {size: {"inventorySha256": entry["inventorySha256"],
                                "artifacts": {a["language"]: a["sha256"] for a in entry["inventory"]["artifacts"]}}
                         for size, entry in prepared.items()}}
    return summary, prepared, cohort


# ------------------------------------------------------------------------ batch

def source_rows_for(workspace):
    """Source rows of a copied cohort, in the shape `capture_header` reports."""
    rows = []
    for path in sorted(workspace.rglob("*")):
        relative = path.relative_to(workspace)
        if not path.is_file() or relative.parts[0].startswith("."):
            continue
        language = EXTENSION_LANGUAGE.get(path.suffix)
        if language is not None:
            rows.append({"language": language, "relativePath": relative.as_posix(),
                         "contentHash": sha256_bytes(path.read_bytes())})
    return rows


def synthetic_producer(profile):
    generator = profile["generator"]
    return {"name": generator["name"], "version": generator["version"],
            "identityKind": profile["identityKind"], "executableHash": generator["sourceSha256"],
            "positionEncoding": 1, "languages": list(LANGUAGES)}


def build_batch(workspace, header, inventory, artifact_dir, profile):
    """Write the four-artifact batch into workspace/.trellis-semantic-batch; every document is covered."""
    ordinals = {(d["language"], d["relativePath"]): d["artifact"] for d in inventory["documents"]}
    artifacts = [{"language": a["language"], "data": (artifact_dir / a["relativePath"]).read_bytes()}
                 for a in inventory["artifacts"]]
    sources = sorted(({"byteLength": (workspace / row["relativePath"]).stat().st_size, **row}
                      for row in header["source_rows"]), key=batch.sort_key)
    if {(r["language"], r["relativePath"]) for r in sources} != set(ordinals):
        raise GateFault("captured source set differs from the artifact inventory documents")
    coverage = [{"artifact": ordinals[(r["language"], r["relativePath"])], "exclusion": None,
                 "language": r["language"], "relativePath": r["relativePath"]} for r in sources]
    descriptor_header = {
        "trustOrigin": "syntheticProfile", "profileId": profile["profileId"],
        "decoderVersion": m.DECODER_VERSION, "producer": synthetic_producer(profile),
        "sourceSet": header["source_set"], "sourceRevision": header["native_id"],
        "sourceManifestHash": header["manifest_hash"], "components": header["components"],
        "metadata": {"protocolVersion": 0, "projectRoot": workspace.as_uri(),
                     "textDocumentEncoding": 1},
    }
    return write_batch_summary(workspace, descriptor_header, sources, coverage, artifacts)


def write_batch_summary(workspace, descriptor_header, sources, coverage, artifacts):
    out_dir = workspace / ".trellis-semantic-batch"
    path, digest = batch.write_batch(out_dir, descriptor_header, sources, coverage, artifacts,
                                     root=workspace)
    descriptor = json.loads(path.read_bytes())
    pages = descriptor["pages"]
    return {"descriptorPath": path, "descriptorSha256": digest, "pageCount": len(pages),
            "pageBytes": descriptor["totals"]["pageBytes"],
            "pageMaxRows": max(p["rowCount"] for p in pages),
            "pageMaxBytes": max(p["byteLength"] for p in pages),
            "artifactBytes": descriptor["totals"]["artifactBytes"],
            "sourceBytes": sum(row["byteLength"] for row in sources),
            "descriptorBytes": len(path.read_bytes())}


# ---------------------------------------------------------------------- sampler

STAGE_PREFIX = "semantic-stage-"


class StorageSampler:
    """Poll database, WAL, SHM and stage-directory bytes every 50 ms, plus one final stat."""
    NAMES = ("index.db", "index.db-wal", "index.db-shm")

    def __init__(self, root, interval=SAMPLE_INTERVAL):
        self.root, self.interval = Path(root), interval
        self.sqlite_peak = self.stage_peak = 0
        self.samples = 0
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def sample(self):
        sqlite_bytes = stage_bytes = 0
        for directory, _dirs, files in os.walk(self.root):
            staged = any(part.startswith(STAGE_PREFIX) for part in Path(directory).relative_to(self.root).parts)
            for name in files:
                try:
                    size = os.stat(os.path.join(directory, name)).st_size
                except OSError:
                    continue
                if staged:
                    stage_bytes += size
                elif name in self.NAMES:
                    sqlite_bytes += size
        self.sqlite_peak = max(self.sqlite_peak, sqlite_bytes)
        self.stage_peak = max(self.stage_peak, stage_bytes)
        self.samples += 1

    def _run(self):
        while not self._stop.is_set():
            self.sample()
            self._stop.wait(self.interval)

    def start(self):
        self._thread.start()
        return self

    def stop(self):
        self._stop.set()
        self._thread.join()
        self.sample()
        return {"total": self.sqlite_peak, "method": "poll 50 ms plus final stat"}


# ------------------------------------------------------------ interval and checks

def timed_interval(adapter, binary, workspace, descriptor, digest, env, cli_rusage):
    """Index then import back to back; the interval spans t1 request start to t2 return."""
    index_argv = [str(binary), "index", "--workspace", str(workspace)]
    import_argv = [str(binary), "semantic", "import", "--workspace", str(workspace),
                   "--batch", str(descriptor), "--batch-sha256", digest]
    t1 = adapter.command(index_argv, env=env, timed=True, rusage=cli_rusage["index"])
    t2 = adapter.command(import_argv, env=env, timed=True, rusage=cli_rusage["import"])
    started = t1[3] - t1[0]
    commands = []
    for argv, (duration, output, errors, _ended, wall_ms) in ((index_argv, t1), (import_argv, t2)):
        commands.append({"argv": argv, "exit": 0, "startedAt": utc_from_ms(wall_ms - duration * 1000),
                         "finishedAt": utc_from_ms(wall_ms), "durationSeconds": duration,
                         "stdout": output, "stderr": errors})
    return {"wallSeconds": t2[3] - started, "commands": commands}, t1[1], t2[1]


def native_facts(db, revision, floor):
    counts = dict(db.execute(m.FACTS, (revision,)).fetchall())
    missing = [lang for lang in LANGUAGES if counts.get(lang, 0) <= 0]
    overall = sum(counts.values())
    if floor["factsScope"] == "perLanguage":
        meets = not missing and all(counts[lang] >= floor["minimumNormalizedFacts"] for lang in LANGUAGES)
    else:
        meets = not missing and overall >= floor["minimumNormalizedFacts"]
    result = {"perLanguage": counts, "overall": overall,
              "floor": floor["minimumNormalizedFacts"], "scope": floor["factsScope"]}
    if not meets:
        raise GateFault(f"native facts below floor or language missing: {result}")
    return result


def pin_revision(output):
    pin = json.loads(output)["publishedRevision"]
    return pin, f"pin:v1:{pin['indexGeneration']}:{pin['indexRevision']}"


def request_table(home):
    paths = list(home.rglob("requests.db"))
    if len(paths) != 1:
        raise GateFault(f"expected exactly one requests.db: {paths}")
    with closing(sqlite3.connect(f"file:{paths[0]}?mode=ro", uri=True)) as db:
        return db.execute("""SELECT request_kind,state,result_generation,result_revision
                             FROM requests ORDER BY seq""").fetchall()


def expected_proof_count(inventory):
    return sum(a["occurrences"] + a["symbols"] + a["relationships"] for a in inventory["artifacts"])


def validate_run(db, home, index_out, import_out, descriptor_sha, probe, inventory, floor):
    """Post-run validation (read-only). Every failure is a fault."""
    if db.execute("PRAGMA user_version").fetchone()[0] != m.SCHEMA_VERSION:
        raise GateFault(f"index is not schema{m.SCHEMA_VERSION}")
    index_pin, index_revision = pin_revision(index_out)
    import_pin, head = pin_revision(import_out)
    stored = db.execute("SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1").fetchone()
    if tuple(stored) != (import_pin["indexGeneration"], import_pin["indexRevision"]):
        raise GateFault("head differs from the import's publishedRevision")
    requests = request_table(home)
    expected = [("index", "done", index_pin), ("semanticImport", "done", import_pin)]
    if [(r[0], r[1]) for r in requests] != [(k, s) for k, s, _ in expected] or any(
            (r[2], r[3]) != (p["indexGeneration"], p["indexRevision"])
            for r, (_k, _s, p) in zip(requests, expected)):
        raise GateFault(f"request rows are not one done index and one done semanticImport with their pins: {requests}")
    timed = m.capture_header(db, index_revision)
    for key in ("native_id", "manifest_hash"):
        if timed[key] != probe[key]:
            raise GateFault(f"probe and timed header differ: {key}")
    if timed["source_set"] != probe["source_set"]:
        raise GateFault("probe and timed header differ: source set")
    facts = native_facts(db, head, floor)
    documents = len(inventory["documents"])
    selection = db.execute("""SELECT producer_id,batch_id FROM semantic_revision_raw_selections
                              WHERE revision_id=?""", (head,)).fetchall()
    if len(selection) != 1:
        raise GateFault(f"head has no single semantic selection (native-only or mixed): {selection}")
    producer, batch_id = selection[0]
    states = dict(db.execute("""SELECT state,count(*) FROM semantic_coverage
                                WHERE revision_id=? AND producer_id=? GROUP BY state""", (head, producer)))
    distinct = db.execute("""SELECT count(*) FROM (SELECT DISTINCT path,language FROM semantic_coverage
                             WHERE revision_id=? AND producer_id=?)""", (head, producer)).fetchone()[0]
    if not states or set(states) - {"complete", "partial"} or sum(states.values()) != documents \
            or distinct != documents:
        raise GateFault(f"coverage is not one complete/partial row per document: {states}")
    captures = db.execute("""SELECT language,artifact_hash FROM semantic_raw_captures
                             WHERE batch_id=? ORDER BY ordinal""", (batch_id,)).fetchall()
    if captures != [(a["language"], a["sha256"]) for a in inventory["artifacts"]]:
        raise GateFault(f"selected batch captures differ from the four preparation artifacts: {captures}")
    proofs = db.execute("""SELECT count(*) FROM semantic_raw_proofs WHERE capture_id IN
                           (SELECT capture_id FROM semantic_raw_captures WHERE batch_id=?)""", (batch_id,)).fetchone()[0]
    if proofs != expected_proof_count(inventory):
        raise GateFault(f"proof count {proofs} differs from the inventory's emitted entries")
    stored_batch = db.execute("SELECT decoder_version,descriptor_sha256 FROM semantic_raw_batches WHERE batch_id=?",
                              (batch_id,)).fetchone()
    if stored_batch != (m.DECODER_VERSION, descriptor_sha):
        raise GateFault(f"stored batch decoder/descriptor differs: {stored_batch}")
    return {"pins": {"index": index_pin, "import": import_pin, "head": head},
            "requests": [{"kind": r[0], "state": r[1], "pin": {"indexGeneration": r[2], "indexRevision": r[3]}}
                         for r in requests],
            "nativeFacts": facts, "coverage": {"states": states}}


def sqlite_steady(db_path):
    sizes = {name: (db_path.parent / name).stat().st_size if (db_path.parent / name).exists() else 0
             for name in ("index.db", "index.db-wal", "index.db-shm")}
    with closing(sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)) as db:
        pages = db.execute("PRAGMA page_count").fetchone()[0]
        page_size = db.execute("PRAGMA page_size").fetchone()[0]
        free = db.execute("PRAGMA freelist_count").fetchone()[0]
    return {"db": sizes["index.db"], "wal": sizes["index.db-wal"], "shm": sizes["index.db-shm"],
            "total": sum(sizes.values()), "measuredBeforeRunnerOpen": True,
            "pageBytes": pages * page_size, "freelistCount": free}, sizes


def storage_block(steady, peak, stage_peak, raw_inputs):
    return {"steady": steady, "peak": peak, "stagingPeakBytes": stage_peak,
            "rawInputBytes": raw_inputs,
            "denominator": {"status": "notMeasured", "reason": DENOMINATOR_REASON, "bytes": None},
            "ratios": {"steady": None,
                       "peak": {"numerator": peak["total"], "denominator": steady["total"],
                                "observed": peak["total"] / steady["total"] if steady["total"] else None,
                                "evaluation": "notMeasured"}}}


def remove_tree(path, label):
    try:
        shutil.rmtree(path)
        return {"path": label, "outcome": "removed"}
    except FileNotFoundError:
        return {"path": label, "outcome": "absent"}
    except OSError as error:
        return {"path": label, "outcome": f"failed: {error}"}


def run_repetition(ctx, size, ordinal, rep):
    """One repetition; fills `rep` as it goes so a fault keeps what was already observed."""
    adapter, binary = ctx["adapter"], ctx["binary"]
    prepared = ctx["prepared"][size]
    rep_dir = ctx["campaign_dir"] / size / f"rep{ordinal}"
    adapter.begin_repetition(rep_dir)
    workspace = rep_dir / "workspace"
    rep["cleanup"] = []
    adapter.copy_cohort(ctx["cohort"] / size, workspace)

    started = time.monotonic()
    probe_home = rep_dir / "probe-home"
    probe_home.mkdir()
    probe_daemon = adapter.daemon(probe_home, "probe")
    try:
        probe_daemon.start()
        output = adapter.command([str(binary), "index", "--workspace", str(workspace)],
                                 env=adapter.env_for(probe_home))
        _pin, revision = pin_revision(output)
        with closing(sqlite3.connect(f"file:{adapter.db_for(probe_home)}?mode=ro", uri=True)) as db:
            header = adapter.capture_header(db, revision)
    finally:
        probe_daemon.stop()
        rep["cleanup"].append(remove_tree(probe_home, "probe-home"))
    rep["probe"] = {"seconds": round(time.monotonic() - started, 3), "revision": revision,
                    "sourceRevision": header["native_id"], "sourceSetId": header["source_set"]["id"],
                    "rootId": header["source_set"]["rootId"], "sourceManifestHash": header["manifest_hash"],
                    "sourceDocuments": len(header["source_rows"])}

    started = time.monotonic()
    built = build_batch(workspace, header, prepared["inventory"], prepared["dir"], ctx["synthetic_profile"])
    descriptor, digest = built["descriptorPath"], built["descriptorSha256"]
    rep["batch"] = {"descriptorSha256": digest, "pageCount": built["pageCount"],
                    "pageBytes": built["pageBytes"], "seconds": round(time.monotonic() - started, 3)}

    home = rep_dir / "home"
    home.mkdir()
    daemon = adapter.daemon(home, "timed", record_rusage=True)
    cli_rusage = {"index": {}, "import": {}}
    sampler = StorageSampler(home)
    daemon_started = False
    try:
        daemon.start()
        daemon_started = True
        sampler.start()
        env = adapter.env_for(home)
        interval, index_out, import_out = timed_interval(
            adapter, binary, workspace, descriptor, digest, env, cli_rusage)
        peak = sampler.stop()
        rep["interval"] = interval
        with closing(sqlite3.connect(f"file:{adapter.db_for(home)}?mode=ro", uri=True)) as db:
            validated = validate_run(db, home, index_out, import_out, digest, header,
                                     prepared["inventory"], ctx["policy"]["cohorts"][size])
        rep.update(pins=validated["pins"], requests=validated["requests"],
                   nativeFacts=validated["nativeFacts"], coverage=validated["coverage"])
    finally:
        if daemon_started and sampler._thread.is_alive():
            sampler.stop()
        daemon.stop()
    log = daemon.path
    rep["daemonLog"] = {"path": str(log.relative_to(ctx["campaign_dir"])),
                        "sha256": adapter.sha_file(log), "bytes": log.stat().st_size}
    rep["rss"] = {"workerMaxRssBytes": daemon.rusage["maxRssBytes"], "workerMethod": WORKER_RSS_METHOD,
                  "cliMaxRssBytes": {"index": cli_rusage["index"].get("maxRssBytes"),
                                     "import": cli_rusage["import"].get("maxRssBytes")}}
    steady, _ = sqlite_steady(adapter.db_for(home))
    rep["storage"] = storage_block(
        steady, peak, sampler.stage_peak,
        {"source": built["sourceBytes"], "artifacts": built["artifactBytes"],
         "pages": built["pageBytes"], "descriptor": built["descriptorBytes"]})
    rep["cleanup"].append(remove_tree(home, "home"))
    rep["cleanup"].append(remove_tree(workspace, "workspace"))
    rep["status"] = "completed"


# --------------------------------------------------------------------- campaign

def campaign_plan():
    return [(size, ordinal) for size in SIZES for ordinal in range(1, REPETITIONS + 1)]


def blank_rep(ordinal):
    return {"ordinal": ordinal, "status": "notAttempted", "startedAt": None, "finishedAt": None}


def execute_plan(runner, smoke, clock=utc_now):
    """Run repetitions in fixed order; a fault stops the campaign, a threshold miss does not."""
    sizes = {size: {"size": size, "repetitions": [blank_rep(n) for n in range(1, REPETITIONS + 1)]}
             for size in SIZES}
    faulted = None
    for size, ordinal in campaign_plan():
        if faulted or smoke:
            continue
        rep = sizes[size]["repetitions"][ordinal - 1]
        rep["startedAt"] = clock()
        try:
            runner(size, ordinal, rep)
            rep["status"] = "completed"
        except Exception as error:
            rep["status"] = "fault"
            rep["fault"] = f"{type(error).__name__}: {error}"
            faulted = f"{size} repetition {ordinal}: {rep['fault']}"
        rep["finishedAt"] = clock()
    return [sizes[size] for size in SIZES], faulted


def empty_sizes():
    return execute_plan(None, True)[0]


def build_report(campaign, policy_read, policy_json, cohorts_json, preparation, sizes, reasons, smoke):
    report = {
        "formatVersion": 1, "kind": "trellis.combined-gate-report", "status": "unqualified",
        "statusReasons": list(reasons), "authoritativeAcceptance": False, "issue11": "open",
        "smoke": smoke, "statements": STATEMENTS, "inclusion": INCLUSION, "campaign": campaign,
        "policy": policy_read, "preparation": preparation, "sizes": sizes,
    }
    try:
        return qualifier.qualify(report, policy_json, cohorts_json)
    except (KeyError, TypeError, ValueError) as error:
        report["statusReasons"] = report["statusReasons"] or [f"preflight: qualification unavailable: {error!r}"]
        report["qualification"] = {"referenceHost": False, "smoke": smoke}
        return report


def render_markdown(report):
    lines = [f"# #61 combined gate report ({report['status']})", "",
             f"- status: {report['status']}", f"- authoritativeAcceptance: false",
             f"- issue11: {report['issue11']}", f"- smoke: {str(report['smoke']).lower()}",
             f"- reference host: {str(report['campaign']['host']['reference']).lower()}",
             f"- host profile: {report['campaign']['host']['profileId']} "
             f"(sha256 {report['campaign']['host']['profileSha256']})", ""]
    lines += ["## Reasons", ""] + [f"- {reason}" for reason in report["statusReasons"]] + [""]
    lines += ["## Statements", "", f"- cold: {report['statements']['cold']}",
              f"- artifacts: {report['statements']['artifacts']}", ""]
    lines += ["## Sizes", ""]
    for entry in report["sizes"]:
        evaluations = entry.get("evaluations", {})
        lines.append(f"### {entry['size']}")
        lines.append(f"- median wall seconds: {entry.get('median', {}).get('wallSeconds')}")
        lines.append(f"- peak worker RSS bytes: {entry.get('peakWorkerRssBytes')}")
        for rep in entry["repetitions"]:
            wall = (rep.get("interval") or {}).get("wallSeconds")
            lines.append(f"- repetition {rep['ordinal']}: {rep['status']}, wall seconds {wall}")
        for name in sorted(evaluations):
            lines.append(f"- {name}: {evaluations[name]['status']}")
        lines.append("")
    return "\n".join(lines)


def write_report(campaign_dir, report):
    campaign_dir.mkdir(parents=True, exist_ok=True)
    (campaign_dir / "report.json").write_text(canonical_json(report))
    (campaign_dir / "report.md").write_text(render_markdown(report))
    (campaign_dir / "campaign.json").write_text(canonical_json(report["campaign"]))


def smoke_batch(campaign_dir, prepared, cohort, profile):
    """Write a batch against the fixed SMOKE_HEADER; no binary or daemon is involved."""
    workspace = campaign_dir / "preparation" / "smoke-workspace"
    m.copy_cohort(cohort / "small", workspace)
    rows = source_rows_for(workspace)
    inventory = prepared["small"]["inventory"]
    ordinals = {(d["language"], d["relativePath"]): d["artifact"] for d in inventory["documents"]}
    if {(r["language"], r["relativePath"]) for r in rows} != set(ordinals):
        raise GateFault("preflight: smoke workspace differs from the artifact inventory documents")
    sources = sorted(({"byteLength": (workspace / r["relativePath"]).stat().st_size, **r} for r in rows),
                     key=batch.sort_key)
    coverage = [{"artifact": ordinals[(r["language"], r["relativePath"])], "exclusion": None,
                 "language": r["language"], "relativePath": r["relativePath"]} for r in sources]
    artifacts = [{"language": a["language"], "data": (prepared["small"]["dir"] / a["relativePath"]).read_bytes()}
                 for a in inventory["artifacts"]]
    header = {**SMOKE_HEADER, "decoderVersion": m.DECODER_VERSION, "producer": synthetic_producer(profile),
              "sourceManifestHash": batch.source_manifest_hash("sourceset:smoke", rows)}
    built = write_batch_summary(workspace, header, sources, coverage, artifacts)
    built.pop("descriptorPath")
    return built


def check_campaign_dir(path, smoke):
    if not path.is_absolute():
        raise SystemExit("--campaign-dir must be absolute")
    if path.exists():
        raise SystemExit("--campaign-dir must not exist")
    if not smoke and str(path).startswith(NON_DURABLE_PREFIXES):
        raise SystemExit("--campaign-dir must not be under /tmp, /private/tmp or /var/folders (durable evidence)")


def build_release():
    subprocess.run(["cargo", "build", "--locked", "--release"], cwd=REPO, check=True)
    return m.BINARY


def run_campaign(args):
    campaign_dir = args.campaign_dir
    smoke = args.smoke
    campaign_dir.mkdir(parents=True)
    campaign, policy_read, policy_json, cohorts_json, failures = preflight(args, campaign_dir, smoke)
    preparation = {}
    sizes = empty_sizes()
    reasons = list(failures)
    if not failures:
        try:
            profile = json.loads(SYNTHETIC_PROFILE_PATH.read_text())
            preparation, prepared, cohort = prepare(campaign_dir, ("small",) if smoke else SIZES, smoke)
            if smoke:
                preparation["smokeBatch"] = smoke_batch(campaign_dir, prepared, cohort, profile)
            else:
                ctx = {"adapter": Gate90Adapter(args.binary_path), "binary": args.binary_path,
                       "campaign_dir": campaign_dir, "prepared": prepared, "cohort": cohort,
                       "policy": policy_read, "synthetic_profile": profile}
                sizes, faulted = execute_plan(lambda s, n, rep: run_repetition(ctx, s, n, rep), smoke)
                if faulted:
                    reasons.append(f"fault: {faulted}")
        except GateFault as fault:
            reasons.append(str(fault))
        except Exception as error:
            reasons.append(f"preflight: preparation failed: {error!r}")
    report = build_report(campaign, policy_read, policy_json, cohorts_json, preparation, sizes, reasons, smoke)
    write_report(campaign_dir, report)
    return report


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--campaign-dir", type=Path, required=True)
    parser.add_argument("--binary", type=Path, dest="binary_path")
    parser.add_argument("--smoke", action="store_true")
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    check_campaign_dir(args.campaign_dir, args.smoke)
    if not args.smoke and args.binary_path is None:
        args.binary_path = build_release()
    report = run_campaign(args)
    print(f"{report['status']}: {args.campaign_dir / 'report.json'}")
    return 1 if report["status"] == "unqualified" else 0


if __name__ == "__main__":
    raise SystemExit(main())
