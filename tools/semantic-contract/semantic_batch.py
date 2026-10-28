"""Emit a semantic import batch: a closed descriptor plus source and coverage pages.

The wire format is the one the Rust admission path parses (technical brief
section 2): a descriptor naming sorted, size-bounded pages and the SCIP
artifacts, and pages of source and coverage rows. Standard library only.
"""

import hashlib
import json
from pathlib import Path

FORMAT_VERSION = 1
MAX_PAGE_ROWS = 1000
MAX_PAGE_BYTES = 1 << 20
LANGUAGE_ORDER = {"java": 0, "rust": 1, "python": 2, "javascript": 3}
EXCLUSIONS = ("notRequested", "unsupported", "failed")

DESCRIPTOR_KEYS = frozenset({
    "formatVersion", "trustOrigin", "profileId", "decoderVersion", "producer", "sourceSet",
    "sourceRevision", "sourceManifestHash", "components", "metadata", "completion", "totals",
    "pages", "artifacts",
})
HEADER_KEYS = DESCRIPTOR_KEYS - {"formatVersion", "completion", "totals", "pages", "artifacts"}


def canonical(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def source_manifest_hash(source_set_id, rows):
    """Digest over rows of (language, relativePath, contentHash) in canonical order."""
    return sha256(canonical([
        {"document": {"sourceSetId": source_set_id, "language": row["language"],
                      "path": row["relativePath"]}, "contentHash": row["contentHash"]}
        for row in rows]))


def sort_key(row):
    return (LANGUAGE_ORDER[row["language"]], row["relativePath"].encode())


def check_strictly_sorted(rows, label):
    previous = None
    for row in rows:
        key = sort_key(row)
        if previous is not None and key <= previous:
            raise ValueError(f"{label} rows must be strictly increasing by (language, path)")
        previous = key


def paginate(kind, rows, rows_per_page, max_page_bytes):
    """Split rows so that a page closes before the next row would exceed a bound."""
    pages, current, size = [], [], 0
    base = len(canonical({"formatVersion": FORMAT_VERSION, "kind": kind, "ordinal": 0, "rows": []}))

    def overhead():
        return base + len(str(len(pages))) - 1

    def close():
        ordinal = len(pages)
        pages.append((ordinal, current[:], canonical(
            {"formatVersion": FORMAT_VERSION, "kind": kind, "ordinal": ordinal, "rows": current})))

    for row in rows:
        length = len(canonical(row))
        if overhead() + length > max_page_bytes:
            raise ValueError(f"a single {kind} row exceeds the page byte bound")
        grown = size + length + (1 if current else 0)
        if current and (len(current) == rows_per_page or overhead() + grown > max_page_bytes):
            close()
            current, size = [], 0
            grown = length
            if overhead() + length > max_page_bytes:
                raise ValueError(f"a single {kind} row exceeds the page byte bound")
        current.append(row)
        size = grown
    if current:
        close()
    return pages


def write_batch(out_dir, header, sources, coverage, artifacts, rows_per_page=MAX_PAGE_ROWS,
                max_page_bytes=MAX_PAGE_BYTES, root=None):
    """Write pages, any inline artifacts and the descriptor into the fresh `out_dir`.

    `header` holds the descriptor keys that are not derived (HEADER_KEYS).
    `sources` rows: byteLength, contentHash, language, relativePath.
    `coverage` rows: artifact (ordinal or None), exclusion (or None), language, relativePath.
    `artifacts`: dicts with `language` and either `data` (bytes, written to
    `artifact-<ordinal>.scip`) or `relativePath`, `byteLength` and `sha256` for a file
    that already exists. `root` is the workspace root that descriptor paths are
    relative to; it defaults to `out_dir`.
    Returns (descriptor_path, descriptor_sha256).
    """
    out_dir = Path(out_dir)
    root = Path(root) if root is not None else out_dir
    if set(header) != HEADER_KEYS:
        raise ValueError(f"header keys must be exactly {sorted(HEADER_KEYS)}")
    if not artifacts:
        raise ValueError("at least one artifact is required")
    check_strictly_sorted(sources, "source")
    check_strictly_sorted(coverage, "coverage")
    for row in coverage:
        if (row["artifact"] is None) == (row["exclusion"] is None):
            raise ValueError("coverage row needs exactly one of artifact or exclusion")
        if row["exclusion"] is not None and row["exclusion"] not in EXCLUSIONS:
            raise ValueError(f"unknown exclusion {row['exclusion']!r}")
        if row["artifact"] is not None and not 0 <= row["artifact"] < len(artifacts):
            raise ValueError("coverage artifact ordinal out of range")
    if not sources or not coverage:
        raise ValueError("at least one source and one coverage row are required")
    if out_dir.exists() and (not out_dir.is_dir() or any(out_dir.iterdir())):
        raise ValueError("the output directory must be new or empty")
    prefix = out_dir.resolve().relative_to(root.resolve()).as_posix()

    def relative(name):
        return name if prefix == "." else f"{prefix}/{name}"

    out_dir.mkdir(parents=True, exist_ok=True)
    page_entries = []
    for kind, rows in (("source", sources), ("coverage", coverage)):
        for ordinal, page_rows, data in paginate(kind, rows, rows_per_page, max_page_bytes):
            name = f"{kind}-{ordinal:05d}.json"
            (out_dir / name).write_bytes(data)
            page_entries.append({"kind": kind, "ordinal": ordinal, "relativePath": relative(name),
                                 "byteLength": len(data), "sha256": sha256(data),
                                 "rowCount": len(page_rows)})
    artifact_entries = []
    for ordinal, artifact in enumerate(artifacts):
        if "data" in artifact:
            name = f"artifact-{ordinal}.scip"
            (out_dir / name).write_bytes(artifact["data"])
            entry = {"relativePath": relative(name), "byteLength": len(artifact["data"]),
                     "sha256": sha256(artifact["data"])}
        else:
            entry = {key: artifact[key] for key in ("relativePath", "byteLength", "sha256")}
        artifact_entries.append({"ordinal": ordinal, **entry, "language": artifact["language"]})
    excluded = sum(row["exclusion"] is not None for row in coverage)
    descriptor = {
        "formatVersion": FORMAT_VERSION, **header,
        "completion": "complete" if excluded == 0 else "partial",
        "totals": {"sourceDocuments": len(sources), "coveredDocuments": len(coverage) - excluded,
                   "excludedDocuments": excluded,
                   "pageBytes": sum(entry["byteLength"] for entry in page_entries),
                   "artifactBytes": sum(entry["byteLength"] for entry in artifact_entries)},
        "pages": page_entries, "artifacts": artifact_entries,
    }
    data = canonical(descriptor)
    path = out_dir / "descriptor.json"
    path.write_bytes(data)
    return path, sha256(data)
