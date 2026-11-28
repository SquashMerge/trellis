"""Score BM25, vector and hybrid (RRF) retrieval against the eval set.

usage: python -I evaluate.py <cards.jsonl> <queries.json> <emb-dir> <out.json> <model,...> [variants]
"""
import json
import os
import re
import sqlite3
import sys

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from cards import split_ident  # noqa: E402

STOP = set("a an the and or of to in on for by with from is are be it its that this which when where what how "
           "code function find does do into before after while still their there them than then only one".split())
DEPTH = 100
RRF_K = 60


def query_tokens(q):
    toks = []
    for raw in re.findall(r"[A-Za-z0-9_]+", q):
        parts = split_ident(raw) or [raw.lower()]
        for p in [raw.lower()] + parts:
            if p not in STOP and len(p) > 1 and p not in toks:
                toks.append(p)
    return toks


def bm25_rank(cards, variant, queries):
    db = sqlite3.connect(":memory:")
    db.execute("CREATE VIRTUAL TABLE t USING fts5(x, tokenize='porter unicode61')")
    db.executemany("INSERT INTO t(rowid, x) VALUES (?, ?)", ((i, c["bm25"][variant]) for i, c in enumerate(cards)))
    out = []
    for q in queries:
        toks = query_tokens(q["query"])
        expr = " OR ".join('"' + t.replace('"', '""') + '"' for t in toks)
        rows = db.execute("SELECT rowid FROM t WHERE t MATCH ? ORDER BY bm25(t) LIMIT ?", (expr, DEPTH)).fetchall()
        out.append([r[0] for r in rows])
    return out


def vec_rank(docs, qv):
    sims = qv @ docs.T
    return [list(np.argsort(-s)[:DEPTH]) for s in sims]


def int8(v):
    scale = np.abs(v).max(axis=1, keepdims=True) / 127.0
    return np.round(v / scale).astype(np.int8), scale


def vec_rank_int8(docs, qv):
    q8, _ = int8(docs)
    d = q8.astype(np.float32)
    d /= np.linalg.norm(d, axis=1, keepdims=True)
    return vec_rank(d, qv)


def rrf(*lists, weights=None):
    score = {}
    for w, lst in zip(weights or [1.0] * len(lists), lists):
        for r, i in enumerate(lst):
            score[i] = score.get(i, 0.0) + w / (RRF_K + r + 1)
    return [i for i, _ in sorted(score.items(), key=lambda kv: -kv[1])][:DEPTH]


def metrics(ranks, gold, cats):
    def one(sel):
        n = len(sel)
        r5 = sum(1 for i in sel if any(x in gold[i] for x in ranks[i][:5])) / n
        r10 = sum(1 for i in sel if any(x in gold[i] for x in ranks[i][:10])) / n
        mrr = 0.0
        for i in sel:
            for r, x in enumerate(ranks[i]):
                if x in gold[i]:
                    mrr += 1.0 / (r + 1)
                    break
        return {"recall@5": round(r5, 3), "recall@10": round(r10, 3), "mrr": round(mrr / n, 3), "n": n}
    res = {"all": one(list(range(len(gold))))}
    for c in sorted(set(cats)):
        res[c] = one([i for i, x in enumerate(cats) if x == c])
    misses = [i for i in range(len(gold)) if not any(x in gold[i] for x in ranks[i][:10])]
    return res, misses


def main(cards_path, queries_path, emb_dir, out_path, models, variants="a,b,c"):
    cards = [json.loads(l) for l in open(cards_path)]
    idx = {c["id"]: i for i, c in enumerate(cards)}
    queries = json.load(open(queries_path))["queries"]
    gold = [{idx[a["declarationId"]] for a in q["answers"]} for q in queries]
    cats = [q["category"] for q in queries]
    results = {}
    bm = {}
    for v in variants.split(","):
        bm[v] = bm25_rank(cards, v, queries)
        m, miss = metrics(bm[v], gold, cats)
        results[f"bm25/{v}"] = {"metrics": m, "missesAt10": [queries[i]["id"] for i in miss]}
    for model in [m for m in models.split(",") if m]:
        qpath = os.path.join(emb_dir, f"{model}.queries.npy")
        if not os.path.exists(qpath):
            continue
        qv = np.load(qpath)
        for v in variants.split(","):
            dpath = os.path.join(emb_dir, f"{model}.{v}.docs.npy")
            if not os.path.exists(dpath):
                continue
            docs = np.load(dpath)
            vr = vec_rank(docs, qv)
            m, miss = metrics(vr, gold, cats)
            results[f"{model}/{v}/vector"] = {"metrics": m, "missesAt10": [queries[i]["id"] for i in miss]}
            hy = [rrf(vr[i], bm[v][i]) for i in range(len(queries))]
            m, miss = metrics(hy, gold, cats)
            results[f"{model}/{v}/hybrid"] = {"metrics": m, "missesAt10": [queries[i]["id"] for i in miss]}
            m, miss = metrics([rrf(vr[i], bm[v][i], weights=[2.0, 1.0]) for i in range(len(queries))], gold, cats)
            results[f"{model}/{v}/hybrid-w2"] = {"metrics": m, "missesAt10": [queries[i]["id"] for i in miss]}
            vr8 = vec_rank_int8(docs, qv)
            m, _ = metrics(vr8, gold, cats)
            results[f"{model}/{v}/vector-int8"] = {"metrics": m}
            m, _ = metrics([rrf(vr8[i], bm[v][i]) for i in range(len(queries))], gold, cats)
            results[f"{model}/{v}/hybrid-int8"] = {"metrics": m}
            if model in ("voyage-4-nano", "qwen3-embedding-0.6b", "nomic-embed-text-v2-moe"):  # Matryoshka-trained
                for dim in (1024, 512, 256):
                    if dim >= docs.shape[1]:
                        continue
                    d = docs[:, :dim] / np.linalg.norm(docs[:, :dim], axis=1, keepdims=True)
                    qd = qv[:, :dim] / np.linalg.norm(qv[:, :dim], axis=1, keepdims=True)
                    vrd = vec_rank(d, qd)
                    m, _ = metrics(vrd, gold, cats)
                    results[f"{model}/{v}/vector-d{dim}"] = {"metrics": m}
                    m, _ = metrics([rrf(vrd[i], bm[v][i]) for i in range(len(queries))], gold, cats)
                    results[f"{model}/{v}/hybrid-d{dim}"] = {"metrics": m}
    with open(out_path, "w") as f:
        json.dump(results, f, indent=1)
    for k, r in results.items():
        a = r["metrics"]["all"]
        print(f"{k:45s} R@5 {a['recall@5']:.2f}  R@10 {a['recall@10']:.2f}  MRR {a['mrr']:.3f}")


if __name__ == "__main__":
    main(*sys.argv[1:])
