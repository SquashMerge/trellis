# Candle model parity (#211, slice 1)

This slice is a standalone runtime and parity proof. It does not add search to the daemon,
indexes or MCP. The model is local and explicitly supplied. There is no model download in
Trellis and no bundled weights.

## Pinned model

`voyageai/voyage-4-nano@5fb34e870f49f19e79c94d5e118f5ee730133279` (Apache-2.0).
The start-of-work upstream comparison with the evaluation revision
`433ece5e27a1611e9360dd7134ecaaf79fb3681d` found only a README change.
Weight, tokenizer, custom modelling code and pooling/config identities were unchanged.

The runtime verifies the supported files before loading them. The weight SHA-256 is
`3dae0c63c81dcab79ac213af331940a1bf2b8a53ec8646be878552890291ad30`.
A user must provide a directory containing the pinned files; a missing or different model
is unavailable, not a request to fetch one. The custom Python file is provenance only:
Trellis never executes it.

The Rust port uses Candle core/nn **0.11.0** and tokenizers **0.22.2**. Candle's public
Qwen3 model exposes only causal attention, so a small private adaptation supplies the
bidirectional key-padding mask. It retains the explicit 128-wide heads, per-head Q/K
normalization, split-half RoPE and learned tokenwise 1024→2048 projection. Masked mean pooling
includes prompt tokens. Normalization follows pooling. There is no KV cache or generation.

Production output is **1024-dimensional int8**. The 2048-dimensional floating-point
intermediate is exposed only with `test-support`, for comparison with the saved Python
vectors. It is not a full-dimension product mode.

## Input and compute bounds

- At most **16 inputs per call** and **512 tokens including the prompt**.
- Before tokenization, text is cut at a UTF-8 boundary to **16,384 bytes**. This bound is a
  slice-1 implementation choice; the largest saved benchmark card is 3,811 bytes, so it
  does not change the parity inputs.
- Query and document prompts retain their exact trailing space. Whole prompted strings are
  trimmed as in SentenceTransformer before tokenization. Padding is on the right and excluded
  from attention and pooling. There is no BOS/EOS insertion.
- CPU is explicit; Metal requires `--features metal`. There is no backend fallback. The
  initial port uses float32 compute; the saved vectors, not an assumption about the original
  loaded weight dtype, decide whether it passes parity.
- Cancellation is checked around bounded batches. Runtime failures and batch panics are typed.
  Daemon job ownership and lazy model lifetime are integration work, not enabled here.

## Int8 arithmetic

The new combined 1024/int8 representation takes the prefix and computes maxabs/127,
then coordinate/scale in f64, followed by ties-to-even rounding. A materialized prefix
L2 normalization is unnecessary: it cancels in that scale, and retrieval normalizes the
stored int8 direction. This deliberately avoids a dependency on BLAS reduction order.
It is a new measurement, not byte parity with historical full2048 document-int8.

An initial exact test against NumPy f32 prefix normalization failed at two half-step
coordinates. The failure is retained as evidence. An independent scalar calculation
confirmed both rounding paths were internally correct. The final arithmetic removes
that redundant reduction; exact integer assertions remain.

## Two different verification claims

The committed small fixture contains real saved input strings and reference vectors.
Offline tests check postprocessing and tiny deterministic architecture cases through the
same attention/model code. They do **not** establish full Voyage-model inference parity:
that requires the external weights. No test compares a reference with itself and calls
that model parity.

Before accepting this slice, run the explicit real-model utility on every saved card and
all current queries. Keep the full corpus and weights outside Git. The utility takes JSONL
rows `{ "id": "...", "kind": "document" | "query", "text": "..." }` and writes ordered
`{ "id": "...", "vector": [...] }` rows. It never overwrites the reference arrays.

```sh
cargo run --locked --release --features test-support,metal \
  --example semantic_search_parity -- \
  --model-dir /absolute/private/model-directory --backend metal \
  --input /absolute/parity-inputs/c.jsonl --output /absolute/candidate/c.jsonl
```

Run it for the chosen form `c` and `queries` on **both CPU and Metal**. Forms `a`
and `b` are optional diagnostics, not acceptance gates. Prepare inputs directly from current
`results/cards.jsonl` and `eval/queries.json`, preserving their line/array order. Use the
**current** `results/emb` arrays, not older result directories.

The offline checker uses NumPy in the evaluation's own Python environment; Python only
analyzes vectors and does not load or run the model:

```sh
/path/to/evaluation/venv/bin/python tools/semantic-search/check-parity.py \
  --evidence-root /absolute/semantic-search-evidence \
  --candidate-dir /absolute/candidate --variants c \
  --model-pin-manifest /absolute/verified-download-manifest.json \
  --output /absolute/parity-result.json
```

Use `/tmp/trellis-factory-verify.lock` only for timed performance measurements and
full `./tools/verify`, acquiring it before the deadline. Numerical parity, inference,
focused tests, clippy and reference-vector analysis run unlocked with bounded commands.
Use an isolated Cargo target and preserve failed inputs/results. Record source and lockfile
hashes, model pin, backend, dtype, prompts and batch policy with the run.

### Owner-ratified regression test

The [owner ruling on #211](https://github.com/SquashMerge/trellis/issues/211#issuecomment-6405701173)
and [#337](https://github.com/SquashMerge/trellis/pull/337#issuecomment-6405701393)
accepts the chosen card form **`c`** with this regression rule:

1. **Every** card vector has cosine **≥0.999** against the pinned saved reference;
   an average cannot satisfy this check.
2. **Recall@10 and MRR do not regress** on the complete #214 evaluation set.
   Compare actual candidate documents and queries with the saved reference documents
   and queries using full-2048 float vectors.
3. Both checks pass on **CPU and Metal**, recorded separately.

The checker implements this rule against the saved vectors. Run the checker once for
each backend's actual form-`c` output. Its self-test covers metric regressions and the
per-card criterion without claiming small synthetic data proves full-model parity.
Keep this external-model regression test for future runtime changes.

The earlier **0.07/0.08 thresholds are superseded** and are not acceptance limits.
Form `a`'s one-query rank 9→11 slip is out of scope because that card form is not used.
Recall@5, query cosine, subgroup metrics, other card forms and document/query cross-arms
remain diagnostics; they do not add acceptance gates.

The historical benchmark measured full-2048 document-only int8 and float-prefix-1024
separately. It did **not** measure combined 1024/int8 or int8 queries. Report the combined
representation as a new measurement; do not attribute the old full-dimensional result to it.
Hybrid fusion tuning and the ratified search contract follow this parity slice.


The small external-model fixture test is explicit and fails if provisioning is missing:

```sh
TRELLIS_PARITY_MODEL_DIR=/absolute/private/model-directory \
TRELLIS_PARITY_BACKEND=metal \
cargo test --locked --features test-support,metal --test core \
  search_embed::pinned_real_model_matches_saved_fixture -- --exact --ignored --nocapture
```

Run with `cpu` as well. The default test lane does not run this external-weight test.
The always-on `saved_voyage_vectors_match_independent_float64_postprocessing` test compares
seven saved vectors against an independent Python float64 direct-maxabs oracle.
The full-corpus utility/checker, not this seven-row test, supplies the all-card claim.

## Observed results

Release Metal inference covered all **6,722 cards in each form (`a`, `b`, `c`)**
and 50 queries. Every card exceeded cosine 0.999:

| Form | Minimum card cosine | Full-vector R@5 / R@10 / MRR |
| --- | --- | --- |
| `a` | 0.99984729 | 0.26 / 0.34 / 0.24631224 |
| `b` | 0.99930525 | 0.40 / 0.44 / 0.25997247 |
| `c` | 0.99964637 | 0.44 / 0.56 / 0.28934607 |

Form `a` recall@10 changed from 0.36 to 0.34 (one of 50 queries); its recall@5
was unchanged and MRR increased by 0.00032145. Form `b` recall@5/10 was unchanged,
with MRR delta −0.00001315. Both forms are diagnostic and outside the accepted
form-`c` regression rule.
For production-form `c`, full-vector recall@5/10 was unchanged from the reference;
MRR changed from **0.28623468** to **0.28934607**. Query minimum cosine was
**0.99993938** (diagnostic). Transforming actual vectors to combined 1024/int8
for both documents and queries measured **0.48/0.60 recall@5/10**, **0.29600597 MRR**.
This is not the historical int8 result or a separate full-corpus production-int8
execution; the explicit real-model fixture tests exercise the production API.

Full CPU inference of production-form `c` also covered **6,722 cards and 50
queries**. Every card exceeded cosine 0.999; minimum **0.99964625**.
Full-vector recall@5/10 and MRR were **0.44/0.56/0.28934607**.
CPU forms `a` and `b` were not run; no claim about those forms follows from `c`.

The source, binary, model, input and output hashes, ordered-ID restoration and full
per-card/per-query reports are local in
`~/projects/odin/trellis-evidence/issue-211/`. CPU and Metal seven-row model checks
and explicit real-model fixture tests passed separately. The first full `./tools/verify` passed
on Rust 1.99.1: all five lanes passed, including 1,510 Rust tests (7 skipped).
Format checks, default/test-support clippy and separate Metal all-target clippy passed.
The owner accepted form `c` on both CPU and Metal: every card exceeds 0.999,
recall@10 is unchanged at 0.56, and MRR improves from 0.28623468 to 0.28934607.
These measured results satisfy the regression rule above.
Unlocked numerical inference runs are correctness evidence, not controlled performance
measurements.

## Branch and contract boundary

The runtime-only slice targets main. Before any schema/index-facing slice, synchronize onto
main after `feature-scip` merges: main was schema 14 and feature-scip schema 21 at start.
Do not allocate a parallel schema or native producer number here.

D1–D13 owner rulings still govern later integration. The required record amendments land
before search is enabled. Counts in the old proposal are stale: at slice-1 start main has
nine MCP tools plus a prompt, and two existing trigram tables. Search adds a tool and a
separate head-only BM25 table without deleting either existing table.
