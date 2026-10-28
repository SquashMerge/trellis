#!/usr/bin/env python3
"""Prepare deterministic synthetic binary SCIP; this is NOT profile admission.

Only the checked-in canonical cohort v2 is supported. No indexer, compiler,
clock, source mutation, native join, or real producer identity is involved.
"""

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import shutil
import sys


REPOSITORY = Path(__file__).resolve().parents[2]
CANONICAL_MANIFEST = REPOSITORY / "tools/synthetic-cohorts/manifest-v2.json"
LANGUAGES = ("java", "rust", "python", "javascript")
GENERATOR_NAME = "trellis-synthetic-performance-scip"
GENERATOR_VERSION = "1"
CALLS_PER_BODY = 2
DENSITY = "all-callable-declarations-first-two-proven-f0-calls-v1"


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def json_bytes(value):
    return (json.dumps(value, ensure_ascii=True, sort_keys=True, indent=2) + "\n").encode()


# Fields and enums are from locked scip 0.10.0 src/generated/scip.rs.
# Document: path=1, occurrences=2, symbols=3, language=4, encoding=6.
# SymbolInformation: symbol=1, kind=5, display_name=6.
def varint(value):
    if value < 0:
        raise ValueError("negative unsigned varint")
    result = bytearray()
    while value > 127:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def scalar(number, value):
    return varint(number << 3) + varint(value)


def field(number, value):
    if isinstance(value, str):
        value = value.encode("utf-8")
    return varint((number << 3) | 2) + varint(len(value)) + value


def occurrence(line, start, end, symbol, definition=False):
    # Legacy packed int32 range remains an actual SCIP 0.10 field.
    packed = b"".join(varint(n) for n in (line, start, end))
    return field(1, packed) + field(2, symbol) + (scalar(3, 1) if definition else b"")


def symbol_id(path, name, line=None):
    # Explicitly synthetic SCIP scheme/package. Paths are escaped descriptors,
    # not native IDs. f0 has one definition per ordinary canonical source file.
    escaped = path.replace("`", "``")
    disambiguator = "" if name == "f0" else str(line)
    return f"trellis-synthetic . cohort-v2 2 `{escaped}`/{name}({disambiguator})."


DECLARATIONS = {
    "java": re.compile(r"\bint (?P<name>m|f[0-9]+)\([^)]*\)(?P<tail>[{;])"),
    "rust": re.compile(r"\bfn (?P<name>m|observe|f[0-9]+)\([^)]*\)->i32(?P<tail>[{;])"),
    "python": re.compile(r"(?m)^ *(?:def) (?P<name>m|f[0-9]+)\([^)]*\):"),
    "javascript": re.compile(r"(?:\bfunction (?P<function>f[0-9]+)|(?P<method>\bm))\([^)]*\)(?P<tail>\{)"),
}
CALL = re.compile(r"(?<![\w.])(?:(?P<qualifier>[A-Za-z_][A-Za-z_0-9]*)\.)?(?P<name>f0|peer0|peer1)\(")


def declarations(language, text):
    """Narrow canonical-template parser, never a general language parser.

    The byte pin excludes strings/comments and arbitrary language syntax. Braces
    delimit C-family bodies; canonical Python defs end at the next dedented line.
    Abstract signatures have no body. Returns source offsets, not native joins.
    """
    if not text.isascii():
        raise ValueError("canonical template requires ASCII coordinates")
    result = []
    for match in DECLARATIONS[language].finditer(text):
        group = "name" if language != "javascript" else "function" if match.group("function") else "method"
        name = match.group(group)
        start = match.start(group)
        line = text.count("\n", 0, start)
        column = start - text.rfind("\n", 0, start) - 1
        body_start = match.end()
        body_end = body_start
        if language == "python":
            line_start = text.rfind("\n", 0, match.start()) + 1
            indent = len(text[line_start:match.end()]) - len(text[line_start:match.end()].lstrip(" "))
            next_line = text.find("\n", body_start)
            body_end = len(text)
            while next_line != -1 and next_line + 1 < len(text):
                following = next_line + 1
                stop = text.find("\n", following)
                content = text[following:stop if stop != -1 else len(text)]
                if content.strip() and len(content) - len(content.lstrip(" ")) <= indent:
                    body_end = following
                    break
                next_line = stop
        elif match.group("tail") == "{":
            depth = 1
            body_end = body_start
            while body_end < len(text) and depth:
                char = text[body_end]
                depth += (char == "{") - (char == "}")
                body_end += 1
            if depth:
                raise ValueError("unclosed canonical callable body")
            body_end -= 1
        result.append({"name": name, "line": line, "column": column,
                       "bodyStart": body_start, "bodyEnd": body_end})
    if not result and not (language == "rust" and re.fullmatch(r"(?:pub mod [A-Za-z_0-9]+;\n)+", text)):
        raise ValueError("source is not a supported canonical callable/module template")
    if result and sum(row["name"] == "f0" for row in result) != 1:
        raise ValueError("canonical file must declare exactly one f0")
    return result


def imported_targets(language, path, text):
    parent = PurePosixPath(path).parent
    if language == "java":
        return {name: str(parent / (name + ".java"))
                for name in re.findall(r"(?m)^import [a-z]+\.java\.(C[a-z]+[0-9]+);$", text)}
    if language == "rust":
        return {alias: str(PurePosixPath(PurePosixPath(path).parts[0], "rust", *target.split("::"))) + ".rs"
                for target, alias in re.findall(r"(?m)^use crate::([A-Za-z_0-9:]+)::f0 as (peer[01]);$", text)}
    if language == "python":
        return {alias: str(parent / (target + ".py")) for target, alias in
                re.findall(r"(?m)^import (C[a-z]+[0-9]+) as (peer2?)[ \t]*$", text)}
    return {alias: str(parent / target) for alias, target in
            re.findall(r'(?m)^import \{f0 as (peer[01])\} from "\./(C[a-z]+[0-9]+\.js)";$', text)}


def encode_document(language, row, source, source_paths):
    path = row["path"]
    text = source.decode("utf-8")
    decls = declarations(language, text)
    imports = imported_targets(language, path, text)
    if any(target not in source_paths for target in imports.values()):
        raise ValueError(f"import target absent from canonical inventory: {path}")
    symbols, occurrences = [], []
    references = 0
    callable_bodies = 0
    bodies_with_two = 0
    for decl in decls:
        name, line, column = decl["name"], decl["line"], decl["column"]
        symbol = symbol_id(path, name, line)
        kind = 26 if name in ("m", "observe") or language == "java" else 17
        symbols.append(field(1, symbol) + scalar(5, kind) + field(6, name))
        occurrences.append((line, column, occurrence(line, column, column + len(name), symbol, True)))
        body = text[decl["bodyStart"]:decl["bodyEnd"]]
        if body:
            callable_bodies += 1
        selected = 0
        for call in CALL.finditer(body):
            qualifier, called = call.group("qualifier"), call.group("name")
            if called == "f0":
                target = path if qualifier is None else imports.get(qualifier)
            else:
                target = imports.get(called) if qualifier is None else None
            if target is None:
                continue  # super/dynamic/unknown receiver resolution is not claimed.
            offset = decl["bodyStart"] + call.start("name")
            call_line = text.count("\n", 0, offset)
            call_column = offset - text.rfind("\n", 0, offset) - 1
            occurrences.append((call_line, call_column, occurrence(
                call_line, call_column, call_column + len(called), symbol_id(target, "f0"))))
            references += 1
            selected += 1
            if selected == CALLS_PER_BODY:
                break
        bodies_with_two += selected == CALLS_PER_BODY
    occurrences.sort(key=lambda item: (item[0], item[1]))
    document = field(1, path)
    document += b"".join(field(2, encoded) for _, _, encoded in occurrences)
    document += b"".join(field(3, symbol) for symbol in symbols)
    document += field(4, language) + scalar(6, 1)  # Explicit UTF-8 byte columns.
    counters = {"documents": 1, "declarations": len(decls), "definitionOccurrences": len(decls),
                "referenceOccurrences": references, "symbolInformation": len(symbols),
                "callableBodies": callable_bodies, "bodiesWithTwoSelectedCalls": bodies_with_two,
                "emptyDocuments": int(not decls), "relationships": 0,
                "candidateFacts": len(occurrences) + len(symbols)}
    return document, counters


def checked_source(cohort_root, row):
    data = (cohort_root / row["path"]).read_bytes()
    if len(data) != row["sourceBytes"] or sha256(data) != row["sha256"]:
        raise ValueError(f"source bytes/hash mismatch: {row['path']}")
    return data


def verify_inputs(cohort_root, manifest_path, size):
    manifest_bytes = manifest_path.read_bytes()
    if manifest_bytes != CANONICAL_MANIFEST.read_bytes():
        raise ValueError("cohort manifest differs from checked-in canonical manifest-v2.json bytes")
    manifest = json.loads(manifest_bytes)
    rows = [row for row in manifest["files"] if row["path"].startswith(size + "/")]
    expected = {row["path"] for row in rows}
    if len(expected) != len(rows):
        raise ValueError("duplicate source path")
    size_root = cohort_root / size
    actual = {p.relative_to(cohort_root).as_posix() for p in size_root.rglob("*") if p.is_file()}
    if actual != expected:
        raise ValueError("source file inventory mismatch (missing or extra file)")
    if len(rows) != manifest["totals"][size]["files"] or sum(row["sourceBytes"] for row in rows) != manifest["totals"][size]["sourceBytes"]:
        raise ValueError("canonical cohort totals mismatch")
    for row in rows:
        checked_source(cohort_root, row)
    return manifest_bytes, rows


def generate(cohort_root, manifest_path, size, out):
    if out.exists():
        raise ValueError("output must be a fresh nonexistent directory")
    if out.resolve().is_relative_to(cohort_root.resolve()):
        raise ValueError("output must be outside the read-only cohort root")
    manifest_bytes, rows = verify_inputs(cohort_root, manifest_path, size)
    source_paths = {row["path"] for row in rows}
    generator_hash = sha256(Path(__file__).read_bytes())
    # No project_root is asserted: binding belongs to runner preflight. This is
    # intentionally not an artifact accepted by ReferenceProfileAdmission.
    metadata = field(2, field(1, GENERATOR_NAME) + field(2, GENERATOR_VERSION)) + scalar(4, 1)
    artifacts = []
    coverage = {}
    out.mkdir(parents=True)
    try:
        for language in LANGUAGES:
            language_rows = [row for row in rows if PurePosixPath(row["path"]).parts[1] == language]
            artifact = bytearray(field(1, metadata))
            counters = {}
            documents = []
            for row in language_rows:
                source = checked_source(cohort_root, row)
                document, counts = encode_document(language, row, source, source_paths)
                artifact.extend(field(2, document))
                for key, value in counts.items():
                    counters[key] = counters.get(key, 0) + value
                documents.append({"relativePath": row["path"], "sourceBytes": len(source), "sourceSha256": row["sha256"],
                                  "counters": counts})
            filename = language + ".scip"
            (out / filename).write_bytes(artifact)
            artifacts.append({"language": language, "path": filename, "byteLength": len(artifact),
                              "sha256": sha256(artifact), "documents": documents, "counters": counters})
            coverage[language] = {"sourceDocuments": len(language_rows), "artifactDocuments": len(documents),
                                  "declaredDocumentsComplete": True, "semanticCoverage": "boundedSyntheticSubset"}
        # Recheck after construction: do not publish a successful inventory of a
        # cohort that changed during preparation. Failures remove all outputs.
        verify_inputs(cohort_root, manifest_path, size)
        preparation = {
            "formatVersion": 1, "kind": "syntheticScipPerformancePreparation", "status": "unboundNotAdmitted",
            "size": size, "scipSchema": "scip-0.10.0", "positionEncoding": "utf8",
            "generator": {"name": GENERATOR_NAME, "version": GENERATOR_VERSION, "sha256": generator_hash},
            "input": {"canonicalManifestVersion": 2, "manifestByteLength": len(manifest_bytes),
                      "manifestSha256": sha256(manifest_bytes), "selectedSourceFiles": len(rows),
                      "selectedSourceBytes": sum(row["sourceBytes"] for row in rows)},
            "density": {"policy": DENSITY, "status": "proposalNotRatifiedMinimum",
                        "maximumCallsPerCallableBody": CALLS_PER_BODY,
                        "calls": "first proven local/imported f0 calls in source order; leaf/abstract/dispatch exceptions retained",
                        "nativeJoinsClaimed": False, "relationshipFactsClaimed": False},
            "currentAdmissionLimits": {"documentsPerArtifact": 1000, "candidateFactsPerArtifact": 100000,
                                       "occurrencesPerDocument": 10000, "symbolsPerDocument": 10000,
                                       "artifactBytes": 67108864,
                                       "note": "LARGE per-language output exceeds document and candidate-fact limits; not loadable unchanged"},
            "profileDraft": {"kind": "syntheticNotReferenceProfile", "producerName": GENERATOR_NAME,
                             "producerHashKind": "generatorSourceSha256", "producerHash": generator_hash,
                             "status": "requiresOwnerAndRunnerDecision"},
            "admission": {"admitted": False, "referenceProfileAdmissionBypassed": False,
                          "runnerPreflightRequired": ["physicalCheckoutRoot", "sourceSetIdentity", "capturedRevisionBinding",
                                                      "syntheticAdmissionPolicy", "documentLimitShardingPolicy"]},
            "languageCoverage": coverage, "artifacts": artifacts,
        }
        (out / "preparation-manifest.json").write_bytes(json_bytes(preparation))
        return preparation
    except BaseException:
        shutil.rmtree(out)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cohort-root", type=Path, required=True)
    parser.add_argument("--cohort-manifest", type=Path, required=True)
    parser.add_argument("--size", choices=("small", "medium", "large"), required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    try:
        generate(args.cohort_root, args.cohort_manifest, args.size, args.out)
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"prepared synthetic {args.size} SCIP in {args.out}; NOT admitted")
    return 0


if __name__ == "__main__":
    sys.exit(main())
