#!/usr/bin/env python3
"""Offline analysis of saved Voyage vectors versus ordered Candle JSONL outputs.

Run with the existing benchmark venv's Python. This program only reads saved
artifacts; it never loads a model, downloads files, or changes reference data.
The metric-noise thresholds below are PROPOSED, not owner-ratified acceptance.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import sys

import numpy as np

DOC_COUNT = 6722
QUERY_COUNT = 50
DIMS = 2048
DEPTH = 100
CARD_COSINE_MIN = 0.999
QUERY_COSINE_DIAGNOSTIC = 0.999
PROPOSED_ALL_DELTA = 0.07
PROPOSED_CONCEPT_DELTA = 0.08
METRICS = ("recall@5", "recall@10", "mrr")
MODEL = "voyage-4-nano"
SOURCE_HASHES = {
    "results/cards.jsonl": "4ca5a9d3588944094ad2e7ee12a454b60498a3ffff7783d78ee99cb4eb28abe0",
    "eval/queries.json": "b4cf25fbfd7ba8b91c7c49f11a558de6defe4d78f22642a1ef9249eaa13e5e55",
    "results/eval.json": "843a191d9a4fef6dd1bf975be5d04182e6fe71fe341bcb0e2a1d3fab756ea9e0",
    "scripts/evaluate.py": "1a76389a368fc823c6ba68f1ac206ca345cc11cedb15179cc041798322e41b1d",
    "results/emb/voyage-4-nano.a.docs.npy": "4c80fc0658344ae5427427fc21186087edc0b235896502090ee3b874d4bc0c3d",
    "results/emb/voyage-4-nano.b.docs.npy": "1e6757ed955de1d7b2816b98e2031b14f65dd8f1363a49bfe3938ad03a102529",
    "results/emb/voyage-4-nano.c.docs.npy": "55de9fe1ca2aa0b42a5ddb23d67df2b6ca4a0cffbcedcf8a454e79e1865bb72f",
    "results/emb/voyage-4-nano.queries.npy": "a42f4e5d9cbd6ccb65cd339dd2dff3addf099cd53ca504400f546139bf6efb3f",
}
ORDER_HASHES = {
    "documents": "87d8f149103449876e5bf781c20ed96f6d7ee2736c1b5eb538f7d20ede1db1dd",
    "queries": "7cbc016b8ab034ddca29ded94454cff6c2db555046b7ed98bd092a62cf97107f",
}


class InvalidInput(ValueError):
    def __init__(self, message, details=None):
        super().__init__(message)
        self.details = details


def require(condition, message, details=None):
    if not condition:
        raise InvalidInput(message, details)


def sha256(path):
    h = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def text_sha256(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def ordered_ids(rows, expected_count, label):
    require(len(rows) == expected_count, f"{label}: expected {expected_count} rows, got {len(rows)}")
    ids = [r["id"] for r in rows]
    require(all(isinstance(i, str) and i for i in ids), f"{label}: invalid ID")
    require(len(set(ids)) == len(ids), f"{label}: duplicate IDs")
    return ids


def id_order_hash(ids):
    return text_sha256("\n".join(ids) + "\n")


def read_jsonl(path):
    with path.open(encoding="utf-8") as source:
        return [json.loads(line) for line in source]


def validate_vectors(vectors, shape, label):
    require(vectors.shape == shape, f"{label}: shape {vectors.shape}, expected {shape}")
    require(vectors.dtype == np.dtype("float32"), f"{label}: dtype is not float32")
    bad = np.flatnonzero(~np.isfinite(vectors).all(axis=1)).tolist()
    require(not bad, f"{label}: nonfinite rows", bad)
    norms = np.linalg.norm(vectors, axis=1)
    bad = np.flatnonzero(~np.isfinite(norms) | (norms <= 0)).tolist()
    require(not bad, f"{label}: zero or nonfinite row norms", bad)
    return norms


def load_reference(path, rows):
    # Inspect the actual header, not just the array returned by np.load.
    with path.open("rb") as source:
        require(np.lib.format.read_magic(source) == (1, 0), f"{path}: expected NPY v1.0")
        shape, fortran, dtype = np.lib.format.read_array_header_1_0(source)
        require(shape == (rows, DIMS), f"{path}: wrong header shape {shape}")
        require(not fortran and dtype.str == "<f4", f"{path}: expected C-order little-endian f32")
        require(path.stat().st_size == source.tell() + rows * DIMS * 4, f"{path}: wrong payload size")
    array = np.load(path, allow_pickle=False)
    validate_vectors(array, (rows, DIMS), str(path))
    return array


def load_candidate(path, ids, dims=DIMS):
    rows = read_jsonl(path)
    require(len(rows) == len(ids), f"{path}: expected {len(ids)} rows, got {len(rows)}")
    errors = []
    vectors = []
    for i, (row, expected_id) in enumerate(zip(rows, ids)):
        if not isinstance(row, dict) or row.get("id") != expected_id:
            errors.append({"row": i, "expectedId": expected_id,
                           "actualId": row.get("id") if isinstance(row, dict) else None,
                           "reason": "ID mismatch (missing, duplicate, or reordered output)"})
            continue
        vector = row.get("vector")
        if not isinstance(vector, list) or len(vector) != dims:
            errors.append({"row": i, "id": expected_id, "reason": f"expected vector length {dims}"})
            continue
        if any(type(x) not in (int, float) for x in vector):
            errors.append({"row": i, "id": expected_id, "reason": "vector must contain JSON numbers, not strings/bools"})
            continue
        with np.errstate(over="ignore", invalid="ignore"):
            array = np.asarray(vector, dtype=np.float32)
            norm = np.linalg.norm(array)
        if not np.isfinite(array).all() or not np.isfinite(norm) or norm <= 0:
            errors.append({"row": i, "id": expected_id, "reason": "nonfinite or zero f32 vector/norm"})
            continue
        vectors.append(array)
    require(not errors, f"{path}: invalid candidate rows", errors)
    array = np.asarray(vectors, dtype=np.float32)
    validate_vectors(array, (len(ids), dims), str(path))
    return array


def normalize(vectors):
    array = np.ascontiguousarray(vectors, dtype=np.float32)
    norms = validate_vectors(array, array.shape, "normalization input")
    result = array / norms[:, None]
    validate_vectors(result, result.shape, "normalized output")
    return result


def quantize_int8(vectors):
    validate_vectors(vectors, vectors.shape, "quantization input")
    scale = np.abs(vectors).max(axis=1, keepdims=True) / np.float32(127.0)
    require(np.isfinite(scale).all() and (scale > 0).all(), "invalid quantization scale")
    scaled = vectors / scale
    rounded = np.rint(scaled)  # NumPy ties-to-even, then signed int8 cast.
    require(np.isfinite(rounded).all() and (np.abs(rounded) <= 127).all(), "invalid quantized value")
    return rounded.astype(np.int8)


def representation(vectors, name, role):
    if name == "full2048-f32":
        return vectors  # Original evaluator does not re-normalize full vectors.
    if name == "d1024-f32":
        return normalize(vectors[:, :1024])
    if name == "d1024-int8-both-NEW":
        prefix = vectors[:, :1024].astype(np.float64)
        maximum = np.max(np.abs(prefix), axis=1, keepdims=True)
        require(np.isfinite(maximum).all() and (maximum > 0).all(), "invalid prefix maximum")
        quantized = np.rint(prefix / (maximum / 127.0)).astype(np.int8)
        return normalize(quantized.astype(np.float32))
    if name == "full2048-doc-int8-historical":
        return normalize(quantize_int8(vectors).astype(np.float32)) if role == "docs" else vectors
    raise InvalidInput(f"unknown representation {name}")


def cosine_report(reference, candidate, ids, texts, threshold, blocking):
    ref_norm = np.linalg.norm(reference, axis=1)
    cand_norm = np.linalg.norm(candidate, axis=1)
    cosine = np.sum(reference * candidate, axis=1, dtype=np.float32) / (ref_norm * cand_norm)
    require(np.isfinite(cosine).all(), "nonfinite cosine")
    below = np.flatnonzero(cosine < threshold).tolist()
    return {
        "threshold": threshold, "blocking": blocking, "count": len(ids),
        "allAtOrAboveThreshold": not below,
        "min": float(cosine.min()),
        "percentiles": {str(p): float(np.percentile(cosine, p)) for p in (1, 5, 50, 95, 99)},
        "belowThresholdCount": len(below), "belowThresholdIds": [ids[i] for i in below],
        "referenceNormRange": [float(ref_norm.min()), float(ref_norm.max())],
        "candidateNormRange": [float(cand_norm.min()), float(cand_norm.max())],
        "rows": [{"row": i, "id": ident, "textSha256": text_sha256(texts[i]),
                  "cosine": float(cosine[i])} for i, ident in enumerate(ids)],
    }


def rank(docs, queries):
    require(docs.dtype == queries.dtype == np.dtype("float32"), "ranking requires f32")
    scores = queries @ docs.T
    require(np.isfinite(scores).all(), "nonfinite ranking scores")
    # Exact original NumPy ranker: no new stable/ID tie-break, no f64 arithmetic.
    order = np.argsort(-scores, axis=1)
    return order[:, :DEPTH], scores, order


def summarize(ranks, gold, categories):
    first = [next((j + 1 for j, row in enumerate(rank_rows) if int(row) in answers), None)
             for rank_rows, answers in zip(ranks, gold)]
    groups = {"all": list(range(len(gold)))}
    groups.update({c: [i for i, cat in enumerate(categories) if cat == c] for c in sorted(set(categories))})
    metrics = {}
    for category, selected in groups.items():
        n = len(selected)
        metrics[category] = {
            "recall@5": sum(first[i] is not None and first[i] <= 5 for i in selected) / n,
            "recall@10": sum(first[i] is not None and first[i] <= 10 for i in selected) / n,
            "mrr": sum(1.0 / first[i] if first[i] else 0.0 for i in selected) / n,
            "n": n,
        }
    return metrics, first


def replay(docs, qv, cards, queries, gold):
    ranks, scores, order = rank(docs, qv)
    metrics, first = summarize(ranks, gold, [q["category"] for q in queries])
    details = []
    for i, query in enumerate(queries):
        margins = {}
        for k in (5, 10, 100):
            left, right = order[i, k - 1:k + 1]
            margins[str(k)] = {
                "scoreAtK": float(scores[i, left]), "scoreAtNext": float(scores[i, right]),
                "margin": float(scores[i, left] - scores[i, right]),
                "idAtK": cards[int(left)]["id"], "idAtNext": cards[int(right)]["id"],
            }
        details.append({"row": i, "id": query["id"], "category": query["category"],
                        "goldIds": [a["declarationId"] for a in query["answers"]],
                        "firstGoldRank": first[i], "top100Ids": [cards[int(j)]["id"] for j in ranks[i]],
                        "cutoffs": margins})
    return {"metrics": metrics, "perQuery": details}


def compare_metrics(reference, candidate):
    return {cat: {name: {"reference": values[name], "candidate": candidate[cat][name],
                         "delta": candidate[cat][name] - values[name],
                         "absoluteDelta": abs(candidate[cat][name] - values[name])}
                  for name in METRICS} for cat, values in reference.items()}


def baseline_comparison(replayed, saved):
    rounded = {cat: {name: (values[name] if name == "n" else round(values[name], 3))
                     for name in (*METRICS, "n")} for cat, values in replayed["metrics"].items()}
    misses = [q["id"] for q in replayed["perQuery"] if q["firstGoldRank"] is None or q["firstGoldRank"] > 10]
    return {"matchesRoundedMetrics": rounded == saved["metrics"], "roundedRecomputedMetrics": rounded,
            "savedMetrics": saved["metrics"],
            "matchesMissesAt10": misses == saved["missesAt10"] if "missesAt10" in saved else None}


def analyze(args, result):
    root = args.evidence_root.resolve()
    candidate_dir = args.candidate_dir.resolve()
    variants = args.variants.split(",")
    require(variants and len(variants) == len(set(variants)) and set(variants) <= set("abc"),
            "--variants must be a nonempty comma-separated subset of a,b,c, without duplicates")
    selected_sources = [p for p in SOURCE_HASHES if not p.endswith(".docs.npy") or
                        any(p.endswith(f".{v}.docs.npy") for v in variants)]
    paths = {p: root / p for p in selected_sources}
    candidate_paths = {v: candidate_dir / f"{v}.jsonl" for v in variants}
    candidate_paths["queries"] = candidate_dir / "queries.jsonl"
    protected = [*paths.values(), *candidate_paths.values(), args.model_pin_manifest, Path(__file__)]
    require(args.output.resolve() not in [p.resolve() for p in protected], "output must not overwrite an input")
    result["sourceArtifacts"] = {}
    for name, path in paths.items():
        actual = sha256(path)
        result["sourceArtifacts"][name] = {"path": str(path), "sha256": actual,
                                            "expectedSha256": SOURCE_HASHES[name]}
        require(actual == SOURCE_HASHES[name], f"saved reference artifact changed: {name}")
    result["candidateArtifacts"] = {name: {"path": str(path), "sha256": sha256(path)}
                                     for name, path in candidate_paths.items()}
    manifest_bytes = args.model_pin_manifest.read_bytes()
    result["modelPinManifest"] = {"path": str(args.model_pin_manifest.resolve()),
                                   "sha256": hashlib.sha256(manifest_bytes).hexdigest(),
                                   "content": json.loads(manifest_bytes)}
    require(isinstance(result["modelPinManifest"]["content"], dict), "model manifest must be a JSON object")
    result["modelPinManifest"]["scope"] = "Provenance only: this offline analyzer does not load or attest model files."
    cards = read_jsonl(root / "results/cards.jsonl")
    query_file = json.loads((root / "eval/queries.json").read_text())
    require(query_file["format"] == "trellis-semantic-search-eval/v1", "unknown saved query schema")
    queries = query_file["queries"]
    ids = ordered_ids(cards, DOC_COUNT, "cards")
    qids = ordered_ids(queries, QUERY_COUNT, "queries")
    result["orderedIdSha256"] = {"documents": id_order_hash(ids), "queries": id_order_hash(qids)}
    require(result["orderedIdSha256"] == ORDER_HASHES, "saved ID order does not match current benchmark")
    result["resolvedAt"] = query_file["resolvedAt"]
    result["vectorFormats"] = {
        "reference": {"npyVersion": "1.0", "headerDtype": "<f4", "order": "C",
                      "documentShape": [DOC_COUNT, DIMS], "queryShape": [QUERY_COUNT, DIMS]},
        "candidate": {"format": "JSONL {id,vector}", "conversionDtype": "float32",
                      "documentShape": [DOC_COUNT, DIMS], "queryShape": [QUERY_COUNT, DIMS]},
        "cosineArithmeticDtype": "float32", "rankingArithmeticDtype": "float32",
        "quantizationStorageDtype": "int8", "quantizedRankingDtype": "float32",
    }
    idx = {ident: i for i, ident in enumerate(ids)}
    gold = []
    for query in queries:
        answers = [a["declarationId"] for a in query["answers"]]
        require(answers and all(a in idx for a in answers), f"{query['id']}: missing gold ID in saved cards")
        gold.append({idx[a] for a in answers})
    ref_queries = load_reference(root / f"results/emb/{MODEL}.queries.npy", QUERY_COUNT)
    cand_queries = load_candidate(candidate_paths["queries"], qids)
    result["queryCosines"] = cosine_report(ref_queries, cand_queries, qids, [q["query"] for q in queries],
                                             QUERY_COSINE_DIAGNOSTIC, False)
    baseline = json.loads((root / "results/eval.json").read_text())
    result["variants"] = {}
    gates = result["gates"]
    names = ("full2048-f32", "d1024-f32", "d1024-int8-both-NEW", "full2048-doc-int8-historical")
    historical = {"full2048-f32": "vector", "d1024-f32": "vector-d1024",
                  "full2048-doc-int8-historical": "vector-int8"}
    for variant in variants:
        ref_docs = load_reference(root / f"results/emb/{MODEL}.{variant}.docs.npy", DOC_COUNT)
        cand_docs = load_candidate(candidate_paths[variant], ids)
        entry = {"cardCosines": cosine_report(ref_docs, cand_docs, ids, [c["cards"][variant] for c in cards],
                                               CARD_COSINE_MIN, True), "representations": {}}
        result["variants"][variant] = entry
        gates.append({"name": f"{variant}/each-card-cosine", "ratification": "explicit per-card criterion",
                      "passed": entry["cardCosines"]["allAtOrAboveThreshold"]})
        for name in names:
            rd, cd = [representation(v, name, "docs") for v in (ref_docs, cand_docs)]
            rq, cq = [representation(v, name, "queries") for v in (ref_queries, cand_queries)]
            arms = {}
            for arm, docs, qv in (("referenceDocs_referenceQueries", rd, rq),
                                  ("candidateDocs_referenceQueries", cd, rq),
                                  ("referenceDocs_candidateQueries", rd, cq),
                                  ("candidateDocs_candidateQueries", cd, cq)):
                arms[arm] = replay(docs, qv, cards, queries, gold)
            reference = arms["referenceDocs_referenceQueries"]
            comparisons = {arm: compare_metrics(reference["metrics"], value["metrics"])
                           for arm, value in arms.items() if arm != "referenceDocs_referenceQueries"}
            representation_result = {
                "status": "NEW paired 1024-int8 docs AND queries baseline; no historical result" if name.endswith("NEW")
                          else "saved full-vector reference" if name == "full2048-f32" else "historical representation replay",
                "shape": [DOC_COUNT, int(rd.shape[1])], "rankingDtype": str(rd.dtype),
                "arms": arms, "comparisonsAgainstSameRepresentationReference": comparisons,
                "metricGateApplied": name == "full2048-f32",
            }
            entry["representations"][name] = representation_result
            if name in historical:
                reproduction = baseline_comparison(reference, baseline[f"{MODEL}/{variant}/{historical[name]}"])
                representation_result["historicalBaselineReproduction"] = reproduction
                if name == "full2048-f32":
                    gates.append({"name": f"{variant}/full-reference-reproduces-saved-eval",
                                  "passed": reproduction["matchesRoundedMetrics"] and reproduction["matchesMissesAt10"]})
            if name == "full2048-f32":
                final = comparisons["candidateDocs_candidateQueries"]
                for cat, limit in (("all", PROPOSED_ALL_DELTA), ("concept", PROPOSED_CONCEPT_DELTA)):
                    for metric in METRICS:
                        difference = final[cat][metric]["absoluteDelta"]
                        gates.append({"name": f"{variant}/{cat}/{metric}", "ratification": "PROPOSED, NOT OWNER-RATIFIED",
                                      "absoluteDelta": difference, "maximum": limit,
                                      "passed": difference <= limit})
    result["integrityPassed"] = True
    result["explicitCardCriterionPassed"] = all(g["passed"] for g in gates if g["name"].endswith("each-card-cosine"))
    result["proposedOperationalGatesPassed"] = all(g["passed"] for g in gates)
    result["passed"] = result["proposedOperationalGatesPassed"]
    result["conclusion"] = ("Passes explicit card criterion and PROPOSED metric gates; numerical noise policy still needs owner ratification."
                            if result["passed"] else "One or more integrity, explicit cosine, or PROPOSED metric checks failed.")


def self_test():
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        candidate = Path(tmp) / "candidate.jsonl"
        for record in ({"id": "wrong", "vector": [1, 0]}, {"id": "right", "vector": [1]}):
            candidate.write_text(json.dumps(record) + "\n")
            try:
                load_candidate(candidate, ["right"], dims=2)
            except InvalidInput:
                pass
            else:
                raise AssertionError("invalid ID/shape was accepted")
        candidate.write_text(json.dumps({"id": "right", "vector": [1, 0]}) + "\n")
        assert load_candidate(candidate, ["right"], dims=2).dtype == np.float32
    fixture = np.array([[127, 0.5, 1.5, 2.5, -0.5, -1.5, -2.5]], dtype=np.float32)
    assert quantize_int8(fixture).tolist() == [[127, 0, 2, 2, 0, -2, -2]]
    direct_input = np.zeros((1, 2048), dtype=np.float32)
    direct_input[0, :7] = fixture[0]
    direct_expected = normalize(np.pad(np.array([[127, 0, 2, 2, 0, -2, -2]], dtype=np.float32), ((0, 0), (0, 1017))))
    assert np.array_equal(representation(direct_input, "d1024-int8-both-NEW", "docs"), direct_expected)
    docs = np.array([[0, 1], [1, 0], [0, -1]], dtype=np.float32)
    ranks, _, _ = rank(docs, np.array([[1, 0]], dtype=np.float32))
    assert ranks.tolist() == [[1, 0, 2]], "descending original argsort tie semantics changed"
    metrics, first = summarize(ranks, [{0}], ["concept"])
    assert first == [2] and metrics["all"]["mrr"] == 0.5
    for vector in (np.zeros((1, 2), dtype=np.float32), np.array([[float("nan"), 1]], dtype=np.float32)):
        try:
            normalize(vector)
        except InvalidInput:
            pass
        else:
            raise AssertionError("zero/nonfinite vector was accepted")
    print("PASS: exact ID/shape, finite/nonzero, ties-to-even quantization, original ranker, first-gold metrics")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-root", type=Path)
    parser.add_argument("--candidate-dir", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--variants", default="a,b,c")
    parser.add_argument("--model-pin-manifest", type=Path)
    parser.add_argument("--self-test", action="store_true", help="run small offline synthetic checks; no model or saved corpus")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    for name in ("evidence_root", "candidate_dir", "output", "model_pin_manifest"):
        if getattr(args, name) is None:
            parser.error(f"--{name.replace('_', '-')} is required unless --self-test is used")
    # This check precedes failure-report writing: never overwrite ANY input.
    protected = [args.model_pin_manifest, Path(__file__),
                 *(args.evidence_root / name for name in SOURCE_HASHES),
                 *(args.candidate_dir / f"{name}.jsonl" for name in ("a", "b", "c", "queries"))]
    if args.output.resolve() in {p.resolve() for p in protected}:
        print("error: --output must not overwrite an input", file=sys.stderr)
        return 1
    result = {
        "format": "trellis-semantic-search-parity-analysis/v1", "passed": False, "integrityPassed": False,
        "environment": {"numpyVersion": np.__version__, "pythonVersion": platform.python_version(),
                        "platform": platform.platform(), "byteorder": sys.byteorder,
                        "analyzerSha256": sha256(Path(__file__)),
                        "threadEnvironment": {k: os.environ.get(k) for k in
                                              ("OPENBLAS_NUM_THREADS", "OMP_NUM_THREADS", "MKL_NUM_THREADS", "VECLIB_MAXIMUM_THREADS")}},
        "constants": {"documentCount": DOC_COUNT, "queryCount": QUERY_COUNT, "dimensions": DIMS,
                      "rankingDepth": DEPTH, "eachCardCosineMinimum": CARD_COSINE_MIN,
                      "queryCosineDiagnosticMinimum": QUERY_COSINE_DIAGNOSTIC,
                      "proposedMaximumAllAbsoluteMetricDelta": PROPOSED_ALL_DELTA,
                      "proposedMaximumConceptAbsoluteMetricDelta": PROPOSED_CONCEPT_DELTA,
                      "metricNoiseRatification": "PROPOSED operationalization, NOT owner-ratified"},
        "semantics": {
            "inputs": "Exact current saved corpus SHA-256, counts, NPY v1.0/C-order/<f4 headers, finite nonzero vectors, exact ID order and gold-ID join; no skips.",
            "ranking": "float32 q @ docs.T; np.argsort(-scores) default quicksort descending, depth 100, no secondary ID tie-break",
            "metrics": "Any-gold query success R@5/R@10; first-gold MRR to depth 100; unrounded aggregates for deltas",
            "newInt8": "First 1024 f32 coordinates converted exactly to f64; maxabs/127 and division f64, np.rint ties-to-even, cast int8, cast f32 and normalize, BOTH docs and queries; no redundant prefix L2",
            "newInt8Acceptance": "New reference/candidate same-transformation measurements ONLY; no gate against full-float or historical quality minimum",
            "queryCosine": "Diagnostic only; not an extra owner-blocking threshold",
            "hybrid": "Not replayed: no FTS/index work or production fusion tuning in this analyzer",
            "modelRuntime": "None. This analyzer does not import Torch/ST, execute embeddings, or download/load model files.",
        }, "gates": [], "errors": [],
    }
    try:
        analyze(args, result)
    except Exception as error:
        result["errors"].append({"type": type(error).__name__, "message": str(error),
                                 "details": getattr(error, "details", None)})
        result["conclusion"] = "Invalid/incomplete run; no passing acceptance claim. Partial diagnostics are not accepted results."
        result["passed"] = False
    try:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    except (OSError, ValueError) as error:
        print(f"error writing report: {error}", file=sys.stderr)
        return 1
    print(f"{'PASS (proposed noise policy)' if result['passed'] else 'FAIL'}: {args.output}")
    for error in result["errors"]:
        print(f"{error['type']}: {error['message']}", file=sys.stderr)
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
