"""Embed cards and eval queries with one local model.

usage: python -I embed.py <model-key> <models-dir> <cards.jsonl> <queries.json> <out-dir> <variants e.g. a,b,c> [max_tokens]

No network access is needed: models load from <models-dir>/<name> (pinned
snapshots). Writes <out-dir>/<model>.<variant>.docs.npy, <model>.queries.npy
and <model>.timing.json.
"""
import json
import os
import sys
import time

import numpy as np

os.environ["HF_HUB_OFFLINE"] = "1"
os.environ["TRANSFORMERS_OFFLINE"] = "1"
os.environ["TOKENIZERS_PARALLELISM"] = "false"


class STModel:
    def __init__(self, path, query_prompt, doc_prompt, max_tokens, remote=False, dtype=None):
        import torch
        from sentence_transformers import SentenceTransformer
        kwargs = {}
        if dtype is not None:
            kwargs["model_kwargs"] = {"torch_dtype": dtype}
        self.m = SentenceTransformer(path, device="mps", trust_remote_code=remote, local_files_only=True, **kwargs)
        self.m.max_seq_length = max_tokens
        self.qp, self.dp = query_prompt, doc_prompt
        self.torch = torch

    def encode(self, texts, query, batch):
        prompt = self.qp if query else self.dp
        v = self.m.encode(texts, prompt=prompt, batch_size=batch, normalize_embeddings=True,
                          convert_to_numpy=True, show_progress_bar=False)
        return v.astype(np.float32)


class OnnxMean:
    def __init__(self, path, max_tokens):
        import onnxruntime as ort
        from tokenizers import Tokenizer
        self.tok = Tokenizer.from_file(os.path.join(path, "tokenizer.json"))
        self.tok.enable_truncation(max_tokens)
        self.tok.enable_padding()
        so = ort.SessionOptions()
        so.intra_op_num_threads = 8
        self.sess = ort.InferenceSession(os.path.join(path, "onnx/model.onnx"), so, providers=["CPUExecutionProvider"])
        self.inputs = [i.name for i in self.sess.get_inputs()]

    def encode(self, texts, query, batch):
        out = []
        for i in range(0, len(texts), batch):
            enc = self.tok.encode_batch(texts[i:i + batch])
            ids = np.array([e.ids for e in enc], dtype=np.int64)
            mask = np.array([e.attention_mask for e in enc], dtype=np.int64)
            feed = {"input_ids": ids, "attention_mask": mask}
            if "token_type_ids" in self.inputs:
                feed["token_type_ids"] = np.zeros_like(ids)
            hidden = self.sess.run(None, feed)[0]
            m = mask[..., None].astype(np.float32)
            v = (hidden * m).sum(1) / np.maximum(m.sum(1), 1e-9)
            v /= np.linalg.norm(v, axis=1, keepdims=True)
            out.append(v.astype(np.float32))
        return np.concatenate(out)


def load(key, models, max_tokens):
    if key == "voyage-4-nano":
        return STModel(os.path.join(models, key), "Represent the query for retrieving supporting documents: ",
                       "Represent the document for retrieval: ", max_tokens, remote=True), 16
    if key == "qwen3-embedding-0.6b":
        return STModel(os.path.join(models, key),
                       "Instruct: Given a question about a code base, retrieve the declaration that implements it\nQuery:",
                       None, max_tokens), 16
    if key == "jina-v2-base-code":
        return OnnxMean(os.path.join(models, key), max_tokens), 16
    if key == "nomic-embed-text-v2-moe":
        # Custom code from nomic-ai/nomic-bert-2048@7710840 copied beside the weights, auto_map made local.
        return STModel(os.path.join(models, key), "search_query: ", "search_document: ", max_tokens, remote=True), 16
    if key == "nomic-embed-code":
        import torch
        return STModel(os.path.join(models, key), "Represent this query for searching relevant code: ", None,
                       max_tokens, dtype=torch.float16), 4
    raise SystemExit(f"unknown model {key}")


def main(key, models, cards_path, queries_path, out_dir, variants, max_tokens="512"):
    max_tokens = int(max_tokens)
    cards = [json.loads(l) for l in open(cards_path)]
    queries = json.load(open(queries_path))["queries"]
    os.makedirs(out_dir, exist_ok=True)
    t0 = time.time()
    model, batch = load(key, models, max_tokens)
    timing = {"model": key, "maxTokens": max_tokens, "loadSeconds": round(time.time() - t0, 2), "cards": len(cards)}
    model.encode(["warm up"], False, 1)
    t = time.time()
    q = model.encode([x["query"] for x in queries], True, batch)
    timing["querySeconds"] = round(time.time() - t, 3)
    np.save(os.path.join(out_dir, f"{key}.queries.npy"), q)
    for v in [x for x in variants.split(",") if x and x != "-"]:
        texts = [c["cards"][v] for c in cards]
        order = sorted(range(len(texts)), key=lambda i: len(texts[i]))
        t = time.time()
        emb = model.encode([texts[i] for i in order], False, batch)
        secs = time.time() - t
        full = np.empty_like(emb)
        full[order] = emb
        np.save(os.path.join(out_dir, f"{key}.{v}.docs.npy"), full)
        timing[f"embedSeconds_{v}"] = round(secs, 1)
        timing["dims"] = int(full.shape[1])
        print(key, v, round(secs, 1), "s", flush=True)
    tpath = os.path.join(out_dir, f"{key}.timing.json")
    if os.path.exists(tpath):  # queries-only rerun keeps the recorded card embedding times
        old = json.load(open(tpath))
        timing = {**old, **{k: v for k, v in timing.items() if k in ("loadSeconds", "querySeconds")}, "queriesRerun": True}
    with open(tpath, "w") as f:
        json.dump(timing, f, indent=1)
    print(json.dumps(timing))


if __name__ == "__main__":
    main(*sys.argv[1:])
