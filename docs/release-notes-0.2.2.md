# Trellis 0.2.2 release notes

**Version:** `0.2.2`. **Release date:** on tag. These notes cover pull requests merged
after the [0.2.1 release](release-notes-0.2.1.md) (tag `v0.2.1`) through
[PR #1319](https://github.com/SquashMerge/trellis/pull/1319). This is the performance
release. Against 0.2.1, a first index takes 37–53% less time, a semantic edit settles
69–88% sooner, the Architecture snapshot is rebuilt incrementally after an ordinary edit,
`storage_busy` no longer shows up during semantic catch-up, peak memory on a real checkout
with semantic evidence falls by up to 90%, and `index.db` is 43–44% smaller after a
semantic import and 41–49% smaller after the edit series that follows. Some memory figures rose; they are listed under
[Memory regressions](#memory-regressions). The MCP `toolVersion` stays **34**, the native
extractor stays `native-v20` and `metricsVersion` stays `metrics:v1`. The index schema
goes from **43 to 49**, so every checkout is re-indexed on first use. See
[Compatibility](#compatibility).

## Performance

### How it was measured

0.2.1 and 0.2.2 were built fresh from their sources with the same release command and
measured on the same day, on the Apple M5 Pro (64 GiB) reference host. Every run had its
own private HOME and its own daemon, and runs of the two versions alternated. The
workloads were Trellis's own checkout (952 files; rust-analyzer and scip-typescript
evidence), a third-party Rust checkout (441 files; rust-analyzer evidence), synthetic
cohorts (small, medium and large: 100, 1,000 and 10,000 files) and mimir, a public
repository (1,135 files, Python and TypeScript, native only). Figures are medians over 3
to 6 runs per version. Edit refresh figures pool 15 timed edits (3 runs of 5); Architecture
edit builds take each run's median of 5 edits, then the median over 3 runs. The search
embedding ran once per version.

### Results

| Measure | 0.2.1 | 0.2.2 | Change |
| --- | ---: | ---: | ---: |
| First index, Trellis's own checkout | 16.9 s | 8.0 s | −53% |
| First index, third-party Rust checkout | 3.77 s | 2.16 s | −43% |
| First index, synthetic small / medium / large | 3.00 / 24.1 / 112.3 s | 1.68 / 13.8 / 71.3 s | −44% / −43% / −37% |
| Semantic edit to settled, Trellis's own checkout (13 KB, 194 KB and 893 KB files) | 16.4 / 16.8 / 16.7 s | 2.0 / 2.4 / 3.1 s | −88% / −86% / −81% |
| First semantic edit of the 893 KB file after start | 55.0 s | 3.0 s | −95% |
| Semantic edit to settled, third-party Rust checkout (13 KB and 234 KB files) | 3.81 / 3.76 s | 0.88 / 1.17 s | −77% / −69% |
| `trellis status` answers `storage_busy` during a semantic edit series (per run) | 15 | 0 | gone |
| Architecture edit build, Trellis's own checkout, semantic, body-only edit | 9.1 s, full | 3.4 s, incremental | −62% |
| Architecture edit build, Trellis's own checkout, native only | 4.2 s, full | 3.3 s, incremental | −21% |
| Architecture edit build, mimir, body-only Python / TypeScript edit | 3.16 / 3.16 s, full | 2.78 / 2.76 s, incremental | −12% / −13% |
| Lifetime footprint peak through semantic edits, Trellis's own checkout | 12.3 GB | 1.2 GB | −90% |
| Idle footprint after first index, Trellis's own / third-party checkout | 1,328 / 386 MB | 229 / 231 MB | −83% / −40% |
| Search fill footprint peak, Trellis's own / third-party checkout | 6.9 / 5.2 GB | 4.8 / 4.3 GB | −31% / −18% |
| `index.db` after semantic import, Trellis's own / third-party checkout | 3,016 / 873 MiB | 1,691 / 498 MiB | −44% / −43% |
| `index.db` after the edit series, Trellis's own / third-party checkout | 3,629 / 931 MiB | 1,862 / 551 MiB | −49% / −41% |

Little changed: native-only edit refresh (0.4–2.0 s, within noise except the 893 KB
file, 21% faster), daemon start to socket (0.05 s), producer runs and their imports (1–9%
faster), search embedding time (unchanged), and `index.db` after a first index, which is
the same size on every workload.

### Faster first index

- **Trigram search tables are filled once per publication** instead of by insert
  triggers ([#1289](https://github.com/SquashMerge/trellis/pull/1289)).
- **Immutable inserts** reuse their statements and insert with `ON CONFLICT DO NOTHING`,
  comparing only the rows that conflict
  ([#1283](https://github.com/SquashMerge/trellis/pull/1283)).
- **Canonical JSON** escapes strings in runs and canonicalizes each document witness row
  once ([#1286](https://github.com/SquashMerge/trellis/pull/1286)).
- **Extraction streams without a barrier:** each result goes to the writer as soon as it
  and every earlier one are ready, in the same order
  ([#1284](https://github.com/SquashMerge/trellis/pull/1284)).
- **The publisher prepares the next batch while it writes the current one,** in the same
  transaction. At most two decoded batches are alive at once. This trades peak memory for
  time; see [Memory regressions](#memory-regressions)
  ([#1296](https://github.com/SquashMerge/trellis/pull/1296)).

### Faster semantic catch-up after an edit

- **Publications no longer wait on the Architecture read.** A publication raises a write
  intent; the Architecture read gives its snapshot up at its next check and reads again
  only once the publication has ended. During a burst of edits the Architecture view
  therefore lags, answering `building`, until the edits stop
  ([#1256](https://github.com/SquashMerge/trellis/pull/1256)).
- **Witnessed catch-up reads only what it doesn't copy:** it reads the retained proofs of
  the documents it re-joins, not every proof of the capture. Copied documents' proofs are
  trusted as published, and corruption found by a later read still schedules a re-index
  ([#1261](https://github.com/SquashMerge/trellis/pull/1261)).
- **Bounded control validation is remembered** for an unchanged committed index file, so
  catch-up no longer re-validates the same head at every step
  ([#1262](https://github.com/SquashMerge/trellis/pull/1262)).
- **A dependency's edit no longer invalidates its callers' payloads.** A `direct` call
  bound to a method of another document becomes `unknown` once that document differs from
  the captured one. That demotion is now applied when the payload is read, so editing the
  target document no longer forces every caller's evidence to be settled again. Answers
  are unchanged ([#1294](https://github.com/SquashMerge/trellis/pull/1294)).

### Incremental Architecture

After an ordinary edit, the Architecture snapshot reuses the previous result for every
source the edit can't affect and recomputes the rest. The result is exactly what a full
build returns, except for the one-time tie order change under
[Compatibility](#compatibility).

- **Canonical core and split read:** the result is partitioned per document, and the read
  is split into native rows and semantic rows, two admitted reads at the same revision
  ([#1307](https://github.com/SquashMerge/trellis/pull/1307)).
- **Incremental core:** a build takes the newest published result of the same generation
  and rules as its base, and recomputes only Dirty sources: documents whose native or
  semantic input changed, and sources that point into documents whose declarations
  changed. Ordered gates fall back to a full build, for example when files are added,
  deleted or renamed, a manifest or the root `tsconfig`/`jsconfig` changes, producers or
  their selections change, more than half the documents are Dirty, or a language's
  reference catalog changes
  ([#1311](https://github.com/SquashMerge/trellis/pull/1311),
  [#1308](https://github.com/SquashMerge/trellis/issues/1308)).
- **Semantic checkouts build incrementally too:** revision-scoped semantic metadata
  (provenance IDs, freshness labels and the current pin) is rebased onto reused sources
  instead of making them Dirty. Semantic catch-up after an edit is built incrementally
  ([#1312](https://github.com/SquashMerge/trellis/pull/1312),
  [#1310](https://github.com/SquashMerge/trellis/issues/1310)).
- **Every reference language has a catalog digest:** Python
  ([#1317](https://github.com/SquashMerge/trellis/pull/1317)), JavaScript and TypeScript
  ([#1318](https://github.com/SquashMerge/trellis/pull/1318)) and Java
  ([#1319](https://github.com/SquashMerge/trellis/pull/1319)) join Rust, so a body-only
  edit in any of them builds incrementally.
- **Verify mode:** with `TRELLIS_ARCHITECTURE_VERIFY=1` (only that exact value), the
  daemon also builds each revision in full at the same revision and compares. A mismatch
  logs the first differing path and serves the full result
  ([#1311](https://github.com/SquashMerge/trellis/pull/1311)).

On Trellis's own checkout with semantic evidence, the edit build fell from 9.1 s to 3.4 s
and edit-to-settled Architecture from 11.5 s to 2.9 s. The gain on mimir and the small
cohort is 12–15%, because a build still pays fixed per-build costs.

### Less memory

- **Architecture semantic metadata is shared** per tuple and basis instead of copied into
  every anchor, relationship and reference site
  ([#1285](https://github.com/SquashMerge/trellis/pull/1285)), and each row's provenance
  stays typed until it is serialized
  ([#1305](https://github.com/SquashMerge/trellis/pull/1305)).
- **The search model's token-embedding table stays in bf16** (311 MB instead of 622 MB).
  Gathered rows are widened to f32 before the first layer, so computation stays f32 and
  the stored vectors are identical
  ([#1287](https://github.com/SquashMerge/trellis/pull/1287)).

### Smaller index

- **Semantic digests are 32-byte BLOBs,** not 64-character hex text; the wire still
  carries lowercase hex ([#1292](https://github.com/SquashMerge/trellis/pull/1292)).
- **Derived provenance reads its kind and digest from the raw proof,** and a payload
  stores its content hash once. A missing or corrupt raw proof still fails the read typed
  ([#1295](https://github.com/SquashMerge/trellis/pull/1295)).
- **Raw proofs no longer store their canonical bytes.** A stored raw proof keeps its
  identity (proof ID, kind, fact digest, language and path); the envelopes are
  reproduced from the retained original artifact with the pinned decoder when they are
  needed ([#1298](https://github.com/SquashMerge/trellis/pull/1298)).
- **Seven narrow semantic tables are `WITHOUT ROWID`,** and raw proofs name their document
  by an integer key ([#1302](https://github.com/SquashMerge/trellis/pull/1302)).

### Memory regressions

Not every memory figure fell. Against 0.2.1, on the same runs:

- **First index on the medium synthetic cohort:** the footprint peak is **+68%** (721 →
  1,213 MB, +492 MB) and RSS max +33% (1,303 → 1,738 MiB). It is still within the 2 GiB
  first-index budget for that size, and goes with the −43% first-index time: the
  publisher keeps a second batch in preparation
  ([#1296](https://github.com/SquashMerge/trellis/pull/1296)). The third-party Rust
  checkout's first-index peak is +22% (471 → 573 MB).
- **Idle after the first index on the synthetic cohorts:** small +107 MB (47 → 154),
  medium +112 MB (219 → 331), large +258 MB (611 → 869). On the two real checkouts it
  fell instead (Trellis's own −83%, third-party −40%).
- **Native-only Architecture on Trellis's own checkout:** the whole-run footprint peak is
  +190 MiB (+12%, 1,635 → 1,825 MiB), only while MCP and CLI answers run after the edits.
  Start-up, edits and restart are flat or lower.
- **Smaller:** the large cohort's daemon `ru_maxrss` is +5% (2,090 → 2,200 MiB).

There is no time regression beyond noise. The largest is mimir's Python edit to settled,
+0.06 s (0.57 → 0.63 s), whose Architecture build is 12% faster.

Trimming is planned for 0.2.3, starting with the state the incremental Architecture
snapshot retains ([#1313](https://github.com/SquashMerge/trellis/issues/1313)).

## Indexing and pathological sources

- **A Rust parse has a deterministic work budget.** tree-sitter-rust's error recovery
  could take minutes on a pathological file. Measurement now stops a Rust parse past 16
  tree-sitter allocator calls per source byte plus 1,000,000, a count that depends only on
  the source. Such a file publishes only its file module, as an `evidence_budget` partial
  file, and the rest of the checkout indexes. Real Rust files make at most 3 calls per
  byte ([#1299](https://github.com/SquashMerge/trellis/pull/1299),
  [#1282](https://github.com/SquashMerge/trellis/issues/1282)).
- **The body-only check is held to the same budget:** an edited file past it takes the
  full native path instead of an unbounded parse
  ([#1309](https://github.com/SquashMerge/trellis/pull/1309)).
- **Reads never re-parse such a file:** references, Architecture, metrics, rules,
  interfaces and the Files syntax-match scan skip a source already past the budget, and
  treat its syntax as one parser error over the whole file. Every read's own Rust parse
  is held to the same budget, and the Files outline names such a file in its warnings
  ([#1303](https://github.com/SquashMerge/trellis/pull/1303)).
- **Error recovery counts more toward the outline and lookup-key bound:** a parse-work
  tick taken while the parser is recovering from a syntax error counts 25, so a file that
  is mostly syntax errors is refused well before its parse ends. A clean file's count is
  unchanged ([#1281](https://github.com/SquashMerge/trellis/pull/1281)).

## MCP

- **`building`, not `deadline_exceeded`, while Architecture builds:** a rules check,
  explain, references, endpoints or layer-prompt read that finds the core building now
  waits holding no read, and answers `building` from the owner's state. Before, it
  admitted a second read near its deadline, and on a loaded host that could end as
  `deadline_exceeded` ([#1260](https://github.com/SquashMerge/trellis/pull/1260)).
- **Request `_meta` is accepted:** an object-valued `params._meta`, such as
  `{"progressToken": …}`, is accepted and ignored on every request and notification, in
  both protocol modes. Before, a legacy-mode session refused it, so every tool call from
  a client that always sends one, such as OpenCode, failed
  ([#1290](https://github.com/SquashMerge/trellis/pull/1290)).
- **The coverage note names a pending state:** for a language without a selected
  producer whose evidence needs re-import or is catching up, the call tools'
  `syntaxOnly` note, the Sequence reason and the CLI note say
  `semantic evidence needs re-import: <reason>` or
  `semantic evidence catching up: <reason>`, as status and describe do. "does not cover
  this language" is kept for a language no published producer covers
  ([#1259](https://github.com/SquashMerge/trellis/pull/1259)).

## Web app

- **A busy checkout stays selectable:** a checkout whose read meets `storage_busy` reads
  "Busy · retrying" in the picker instead of "Not indexed", stays enabled, and the list
  is read again with backoff (2 s doubling to 60 s) until it opens
  ([#1258](https://github.com/SquashMerge/trellis/pull/1258)).
- **Deviations fits its pane:** the table switches to its two-line layout by the pane's
  own width, so the File column no longer collapses to zero width with the Inspector open
  ([#1257](https://github.com/SquashMerge/trellis/pull/1257)).

## Development

- **Performance gates:** the #225 mixed LOCAL gate's timed loop waits for semantic
  catch-up, and the stale native-only gate tool is deleted
  ([#1280](https://github.com/SquashMerge/trellis/pull/1280)).
- **Test reliability:** the MCP `_meta` test waits for readiness
  ([#1297](https://github.com/SquashMerge/trellis/pull/1297)), and the captured
  Architecture fixture lets the owner settle after each build
  ([#1320](https://github.com/SquashMerge/trellis/pull/1320)).
- **Tooling:** the Feature Factory config takes an optional verify wrapper, a 4-hour
  verify budget and the Trellis MCP for specialists
  ([#1306](https://github.com/SquashMerge/trellis/pull/1306)).
- **Dependencies:** `clap` 4.6.8, `rand` 0.10.4, `rusqlite` 0.43.0 and `uuid` 1.29.3
  ([#1251](https://github.com/SquashMerge/trellis/pull/1251)); npm packages used by tests
  and tooling ([#1249](https://github.com/SquashMerge/trellis/pull/1249)); and a CI
  action ([#1250](https://github.com/SquashMerge/trellis/pull/1250)).

## Compatibility

Trellis is pre-release; there is no backward compatibility.

- **MCP `toolVersion` stays 34.** The catalog is unchanged. The changes above that clients
  can see are message values and timing: the request `_meta` rule (#1288), the coverage
  note (#1255), `building` instead of `deadline_exceeded` (#1252), and parse-work bounds
  that can now refuse a pathological document sooner with `input_bounds_exceeded`
  (#1227, #1300).
- **Native extractor stays `native-v20`; `metricsVersion` stays `metrics:v1`.**
- **Index schema 43 → 49.** An older index is recreated, not migrated, so every checkout
  is re-indexed on first use.
  - 44: trigram tables filled per publication, no insert triggers (#1246).
  - 45: capture-bound calls are marked for the read-time demotion (#1279).
  - 46: semantic digests stored as 32-byte BLOBs (#1248).
  - 47: derived provenance resolved from the raw proof; content hash once per payload
    (#1248).
  - 48: raw proofs without canonical bytes (#1248).
  - 49: seven `WITHOUT ROWID` semantic tables and an integer raw-proof document key
    (#1248).
- **Architecture behaviour** (approved for #1301):
  - Progress: `measuring` counts all reused documents in one step, then each newly
    measured one. Semantic Read B reports under `resolving`.
  - A failing revision reports later: an ordinary failure of the incremental attempt
    retries the full build at the same revision, which decides the kept failure, its
    code and its message. Give-way, cancellation and `storage_busy` don't take this retry.
  - Semantic catch-up is built incrementally.
  - Ties are broken by a stable proof identity (the raw `derivedFrom` ID and evidence
    kind), not the revision-derived provenance ID, in Architecture and the call graph.
    Tied proofs or anchors at one range may come back in a different order once; they
    then keep that order across unrelated edits (#1310).
  - Publications never wait on the Architecture read, so the view answers `building`
    during an edit burst (#1245).
- **Durable state and configuration** are unchanged: `requests.db`, producer state,
  `.trellis/rules.toml` and `.trellis/producers.toml`. `TRELLIS_ARCHITECTURE_VERIFY` is a
  new optional diagnostic variable.
- **Contract amendments:** Decision 0013 (#1246, the publication fill) with matching
  Decision 0016 wording; Decision 0021 D1 and D4 (#1279, read-time demotion);
  [contract-v1](semantic-evidence/contract-v1.md) (#1279 read projection and reuse key;
  #1248 raw proofs keep identity, not bytes); [local topology](local-topology.md) (#1279
  schema note); the [MCP contract](mcp-readonly-pilot-contract.md) (#1288, #1255,
  #1252, #1227); [architecture view](architecture-view.md) (#1245, #1252, #1301, #1308,
  #1310); [rules.md](rules.md) (#1282, #1300, #1310); [files-view.md](files-view.md)
  (#1227, #1282, #1300); [daemon-v1.md](daemon-v1.md) (#1255, #1282, #1300); and
  [semantic-search-candle.md](semantic-search-candle.md) (#1247, the bf16 table).

## Upgrade notes

The usual [upgrade recovery](quickstart.md#upgrading-trellis) applies. The index schema is
now 49, so each checkout is fully re-indexed on first use, and Trellis retries each
producer's last successful `producer run` batch. If a retry is refused and leaves
**needs re-import**, run the producer again.

## Known limitations

See [known limitations](known-limitations.md) before relying on the results. They are
unchanged from [0.2.1](release-notes-0.2.1.md#known-limitations), plus:

- **Architecture on large checkouts:** Architecture fails with `bounded_work`
  (`architecture source`) when the selected revision's stored source and native rows pass
  512 MiB. The 1,000- and 10,000-file synthetic cohorts (550 MiB and 2.8 GiB `index.db`
  after a first index) are past it; Trellis's own checkout is not. 0.2.1 has the same
  limit.
- **The memory regressions** [above](#memory-regressions).
- **MCP lookup keys under heavy load:** refusing a pathological document near the
  parse-work bound can take longer than MCP's 5-second call deadline, and the call then
  answers `deadline_exceeded` first ([#1227](https://github.com/SquashMerge/trellis/issues/1227)).
