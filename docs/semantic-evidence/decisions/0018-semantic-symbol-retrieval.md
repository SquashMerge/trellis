# Decision 0018: semantic symbol retrieval

- Status: owner-ratified direction on #211 (2026-10-21), amended by the 2026-10-25 parity and
  backend rulings, the 2026-10-26 item-8 approval, the
  [2026-10-27 resident-matrix ruling](https://github.com/SquashMerge/trellis/issues/358#issuecomment-6441405819), and the
  [2026-10-28 exact-name ranking and role-filter ruling](https://github.com/SquashMerge/trellis/issues/389#issuecomment-6482754535).
  The initial reviewed constants are
  (k, D, wL, wV) = (20, 200, 1, 4), unchanged by the additive exact-name rule and
  optional stored-role filter below; no exact-only gate is added.
- Amends: #24’s catalog/wire projection and #23’s optional-vector-cache topology as stated in
  their amended sections. No #22 evidence record, identity, provenance or warning shape changes.
- Scope: one card/vector per eligible named declaration; no AST chunks, summary vectors,
  hosted provider, score field or debug-score flag.
- Compatibility: none for disposable indexes. Native search-fact output changes the native
  producer version; storage changes follow the current disposable-index contract. The owner's
  2026-10-26 capture-slice direction permits preparation on a separate feature-scip-based branch
  before the merge. Reconcile branch-local schema/producer identifiers onto merged main before
  opening its PR; never change the #90 factory branch or reserve a competing main format.

## Authority and activation boundary

- [D1–D13 and accepted proposal defaults, 2026-10-21](https://github.com/SquashMerge/trellis/issues/211#issuecomment-6324615865).
- [Chosen form-c parity acceptance](https://github.com/SquashMerge/trellis/issues/211#issuecomment-6405701173).
- [Automatic backend and status ruling](https://github.com/SquashMerge/trellis/issues/211#issuecomment-6405742145).

These amendments record that direction; they do not ratify the old plan/proposal wholesale.
The [2026-10-26 item-8 ruling](https://github.com/SquashMerge/trellis/issues/211#issuecomment-6416445655)
accepts the exact draft text below, keeps local voyage-4-nano, and permits enabling
`trellis_search_symbols` in the capture PR after `feature-scip` merges to main. The earlier
[pre-code plan](https://github.com/SquashMerge/trellis/issues/211#issuecomment-6411250597)
authorized capture/FTS/background-fill preparation on its own feature-scip-based branch.
Rebase onto merged main and run full verification before opening the capture PR; never
change the #90 factory branch.

## Cards and measured inputs

The chosen card form is (c): measured name/kind, separately labelled measured named ancestors,
Rust impl self type and trait, root-relative path, signature from stored header/parameters/header
items, doc comment, bounded body facts, and a capped source head. Callee spellings come from
native calls; type names, string literals, error-code-shaped literal tokens and container
member names are measured from the native extraction tree/captured bytes. Nothing is parsed
from displayKey and the card does not use the path#qualified-name presentation format.

Capture previously missing doc comments, literals, body type names and impl self/trait facts
during the existing native parse. No second parse, file reread or read-time extraction is
introduced. Store these in search-only version-owned tables, not #22 Declaration/Header or
another evidence record. Retain the proposed bounds: at most 20 literals of at most 100 bytes,
a 600-byte doc comment, 30 type names and 40 callees per declaration. The owner's 2026-10-26
capture-slice direction selects the written
[card serializer, 1,200-source-byte head cap and fixed stopwords](../../semantic-search-card-format-draft.md)
for implementation. The capture plan explicitly records the remaining eligibility, attachment
and splitter choices rather than inferring them from historical model parity. Use the chosen
(c) reference/evaluation artifacts when evaluating the resulting representation. Fusion and
tool activation remain subject to item 8.

cardHash = SHA-256(UTF8("trellis.search-card.v1\0") || exactCardBytes).
Cards include the measured root-relative path. Equal exact card bytes can reuse vectors
across checkouts. Card-format changes change modelId even when extraction is unchanged. They also regenerate affected current-head card bytes/hashes and ranking FTS rows through coherent publication; changing only the vector-cache identity would leave stale lexical rows. Derived state that is being rebuilt need not be validated first.

## Model identity and runtime

modelId = SHA-256(UTF8("trellis.search-model.v1\0") || canonicalJSON(descriptor)), reported as
64 lowercase hex. Use #22’s canonical-JSON byte rules. The descriptor has exactly
{name,revision,modelFiles,dimensions,quantization,maxTokens,queryPrompt,documentPrompt,pooling,
inputPolicy,cardFormat}. `modelFiles` is a nonempty array of exactly `{path,sha256}` objects,
ordered by relative filename UTF-8 bytes, without duplicate filenames; each SHA-256 is 64
lowercase hex. It replaces the proposal’s two scalar weight/tokenizer hashes, covering all
verified snapshot files without an ambiguous multi-file hash concatenation. One internal
supported-model descriptor supplies both loader pins and modelId generation, not two copied
pin lists. No public descriptor API is required.

The initial supported snapshot is `voyageai/voyage-4-nano` at upstream revision
`5fb34e870f49f19e79c94d5e118f5ee730133279`. Its nine filename/hash pairs are the reviewed
`MODEL_FILES` in `src/search_embed/mod.rs` at PR #337 commit
`a32ac03aa44d8ac6858f1a153efe273d50cafed3`; integration preserves those exact pins. All nine
files are verified; only verified owned config/tokenizer/weight bytes are parsed, and custom
Python is provenance only, never executed. This records the existing runtime pin, not a new
model choice. Exact quantization/pooling/inputPolicy identifiers and cardFormat serialization
must be fixed in the supported descriptor review before model/cache wiring. `inputPolicy`
binds the accepted preprocessing: the 16,384-byte UTF-8-boundary text cap, exact prompt and
whole-prompted-string Python-compatible strip semantics, NFC, no BOS/EOS insertion, 512-token
right truncation, and right padding with token 151643 excluded from attention/pooling. The
runtime’s implementation and parity documentation define these existing operations; versioned
identity must change when their meaning changes. Backend/build identities are not cache-key
inputs, and this adds no cross-call numerical equality requirement.

One supported model is active at a time, selected from Trellis’s closed reviewed pins.
Configured files are verified before loading against the reviewed immutable model descriptor.
The single embedding owner retains verified model weights for reuse by queries and fill
batches, dropping them after 600 seconds with no query or fill work; each use resets the idle
timer. When the model becomes ready, the owner pre-warms active-checkout head matrices.
Daemon startup pre-warms at most the four most recently opened checkouts whose recorded
last-opened time is within seven days, newest first (root-key byte order breaks ties).
Existing index metadata is the approved open-recency proxy; discovery is read-only, does
not activate checkouts or refresh open age, and creates no durable MRU catalog. Startup
work is low-priority background work, below normal query/active-checkout work, and never
delays daemon socket acceptance. Startup warming stops when the aggregate matrix memory
cap below prevents admission; it does not evict useful matrices to warm more checkouts.
This records the [#358 owner policy, 2026-10-27](https://github.com/SquashMerge/trellis/issues/358#issuecomment-6446057555).
File verification is reused per file identity and is repeated when files change, before reusing
changed model inputs. Loading remains lexical-only for concurrent callers and the explicit
Metal-unavailable error is unchanged. The owner avoids blocking checkout worker locks while
loading. This amends the earlier per-job drop rule (owner ruling
[#211, 2026-10-26](https://github.com/SquashMerge/trellis/issues/211#issuecomment-6427400386)). The model
is not a #22 Producer: vectors are retrieval metadata, not evidence. Other modelId cache
entries are ignored safely after switching models. The first model is voyageai/voyage-4-nano,
ported to Rust/Candle; no Python runtime or executed upstream custom code is used. The current
pin and parity artifacts identify the exact reviewed upstream revision. Updating weights or
custom modelling code requires the owner’s concept/role evaluation and custom-code review.
A copied evaluation revision or a moving “latest” label is not a new reviewed pin.

The chosen (c) parity acceptance is every-card cosine >=0.999 against the saved #214 Python
vectors and no regression in recall@10 or MRR on that evaluation, for both CPU and Metal.
The owner accepted the measured (c) CPU/Metal results. Form (a) is unused and its diagnostic
slip is not a blocker; the old 0.07/0.08 metric tolerances are superseded, not retained gates.

Default backend selection is automatic: Metal on macOS with the metal feature, CPU elsewhere
including Linux/OrbStack. Automatic mode may fall back. Explicit Metal unavailability is a
typed error, never a silent fallback. searchIndex reports the actual backend; it does not
make backend selection into evidence. Release artifact/platform builds are #338’s scope.

## Lexical query and ranking

The search table is head-only, using `porter unicode61 remove_diacritics 2`, default categories
L* N* Co, with no tokenchars/separators overrides. `_` is a separator. Card text and query
share the same measured-text splitter. Take maximal runs of unicode61 token characters,
split at camelCase and letter-digit boundaries, keep the run and its parts, drop terms shorter
than two characters or in the fixed versioned stopword list, deduplicate in first-occurrence
order, and retain at most 32 terms for both card and query, as the accepted proposal specifies.
Insert the card’s transformed terms as text into FTS. Only query terms are emitted as quoted
FTS5 string literals, joined with ` OR ` and bound as a SQL parameter to MATCH. Card insertion
is not a MATCH expression. Raw caller text never enters the FTS query parser.
No terms skips FTS MATCH, but does not skip exact-name admission below. Errors after FTS
runs follow the existing typed contention/store rules, never raw-query retry or vector-only
best-effort success. Mandatory head/corpus checks and observed store faults remain typed
failures, including for term-free queries. Decision 0013’s two trigram tables/rules are unchanged.

The [#389 owner ruling](https://github.com/SquashMerge/trellis/issues/389#issuecomment-6482754535)
ratifies the [ranking amendment](https://github.com/SquashMerge/trellis/issues/389#issuecomment-6482250936)
and adds the optional `trellis_search_symbols` input `role:"any"|"production"|"test"`, default
`"any"`. `production` or `test` admits only declarations with that stored measured role; `any`
retains both. Apply this filter to **both arms before ranking and depth cutoffs**, not to the
returned prefix, so a filtered search can return up to `limit` hits of the requested role.
There is no path guessing. Invalid values are typed `invalid_request`; successful data echoes
the effective `role`. Scope coverage and search-index counts keep their admitted-head meaning,
not filtered-result counts. No other filter, score/debug input or backend selector is added.

Exact means stored measured `Declaration.name` equals the **complete raw query byte-for-byte**.
Do not trim, case-fold, normalize, parse displayKey, compare substrings or use FTS stemming to
determine exactness. Use names and roles from stored declarations at the admitted head, not
read-time source parsing or paths. No kind preference is added: functions, methods and eligible
containers use the same ordering rules.

Candidates are unique by (document version, symbolId). Before the lexical depth D cutoff,
add all eligible exact-name declarations to ordinary FTS matches, even if their names yield
no terms or their ordinary BM25 position would be below D. Deduplicate the union by that full
candidate key. This is additive retrieval, never an exact-only gate: nonexact FTS matches remain
eligible. Sort the union by:

1. Exact name before nonexact.
2. Stored production role before stored test role.
3. Within each exact/role tier, candidates with an FTS score first, in BM25 ascending order;
   exact-only candidates without an FTS score follow.
4. SymbolId bytes for equal BM25 or for unscored exact-only candidates.

Retain at most D candidates, even when more than D exact names exist; l is the retained
candidate’s 1-based lexical rank. The same rule governs lexical-only search. In particular,
a weak one-term production match can outrank a stronger test match in its exact/nonexact tier.
Tests are retained unless the caller selects production, but may fall beyond the depth cutoff.

The role-filtered vector arm ranks candidates with current decoded vectors by cosine
descending then symbolId bytes, also at depth D; v is its 1-based rank. Ordinary fusion is
wL/(k+l) + wV/(k+v); an absent arm contributes zero. Its existing order is score descending,
then smaller available arm rank, then l (absent last), then symbolId bytes. After fusion,
**exact names precede nonexact candidates regardless of RRF score**. Within exact names,
production precedes test, then the existing RRF order applies. Nonexact hybrid candidates
keep the existing RRF order; there is no final production tier for nonexact hits. These rules
apply on every search route, including lexical-only and optional-vector-budget fallback.
`matchedBy` records the arms that returned the candidate after their depth cutoffs, not an
inferred relevance explanation. An additionally admitted exact candidate is lexical membership,
not a third arm.

The weighted-RRF constants remain **k=20, D=200, wL=1 and wV=4**. For a candidate with
1-based lexical rank l and vector rank v, its score is `1/(20+l) + 4/(20+v)`; an absent arm
contributes exactly zero. Fusion contributions are evaluated in binary64, lexical first and
vector second. D is an internal candidate depth, not a change to the public response limit.
The raw-name exact admission and ordering above do not change the shared card/query 32-term
transform, bound quoted OR expression or tokenizer. There is no additional exact-phrase,
literal, substring, language or query-category gate. A no-term query has only eligible exact
names in its lexical arm, which is empty when none exist. A store or FTS failure remains typed,
never vector-only best-effort success. Constants and future lexical restrictions are reviewed
contract choices, not silent runtime tuning. No card/model identity, storage schema, publication,
trigram, evidence closure, failure or limit change follows from #389. The role input and echo
are its only wire-field additions; there is no score field or cross-call/backend ranking-equality
promise.

The resident int8 head matrix is published as an immutable snapshot for one complete
index pin and active model. The single existing background fill owner builds it, patches
only changed document-version IDs from vectors.db for a new head pin, and tops up missing
vectors as fill completes. A query uses one published matrix snapshot matching its admitted
index pin for vector ranks, embedded counts and matchedBy/order, together with that same
mandatory index snapshot. Resident matrices have a 2 GiB aggregate allocation budget,
including in-progress builds, replacements and staging buffers. Admission accounts
conservatively for estimated allocation capacity and bookkeeping before allocation, not
just vector payload. This is a matrix-allocation budget, not an exact process-RSS limit or
a limit on model weights. Query use shares the owner's existing matrix charge, not a
second copy. Normal work evicts least-recently-used eligible matrices to admit a build. Startup warming
does not evict matrices and stops on cap admission failure. A head that alone exceeds the
cap stays lexical-only. Each matrix also expires 600 seconds after its last query use or
publication; background fill does not reset its timer. A running head is excluded from idle
eviction until its job ends. The cap does not replace that idle rule.

A checkout not already warmed returns complete lexical results on its first search while
the independent background owner builds its matrix. No query waits for that build or embeds
a query without an admitted resident matrix. If no matching matrix remains, including after
eviction, a head change or a concurrent acquisition race, the query also returns lexical-only;
it does not bypass the memory cap with the streaming scorer. These unavailable results
have null model identity, embedded:0, the selected backend and unavailableReason:null.
The admitted declaration count and lexical retrieval remain mandatory. Known observed
store faults retain typed failures rather than becoming lexical-only success. This
supersedes the earlier missing-matrix streaming fallback, under the same
[#358 owner policy](https://github.com/SquashMerge/trellis/issues/358#issuecomment-6446057555).
This does not adopt sqlite-vec or remove existing bounds.

Resident scoring uses NEON/SIMD with a scalar fallback and the stored-norm dot product.
Acceptance is no R@5, R@10 or MRR regression on the gold set, not bit-identical scores or
rankings. The earlier model-inference parity requirement is unchanged.

evidenceBasis pins only the index head, not the asynchronous evictable cache or resident
matrix publication. A valid retained published matrix remains usable after backing-cache
eviction; owner updates or residency changes can change retrieval between calls at one basis.
Re-embedding the same (modelId,cardHash) need not reproduce identical bytes. No cross-call
equality, backend equality or runtime-independent ranking guarantee is added. Existing
within-call ordering and typed numerical validity still hold; non-finite/malformed vectors
cannot enter cosine ordering as valid rows. Observed database/schema/I/O faults retain their
typed failures, never lexical-only success.

## Optional vector budget (supervisor review direction, 2026-10-26)

The call deadline stays 5,000ms. Optional vector work uses one absolute 3,000ms budget from
protocol acceptance, including admission, verification/read, load, embedding, cache access
and scoring. It never restarts on retries. Expiry stops the operation cooperatively and
discards its entire vector arm before completing lexical retrieval at the admitted head.
Query model inference is never detached to satisfy a timeout. The independent matrix build
belongs to the existing single background fill owner, not the query: it continues to completion
and publishes after the query's 3,000ms fallback or cancellation/drop. Query cancellation
does not cancel that shared build. Resident weights follow the query/fill idle rule above;
matrices follow the separate query-use/publication idle rule above. The remaining 2,000ms is reserved
for cleanup and mandatory lexical/projection/final checks, not a guarantee against an
uninterruptible filesystem/device operation or mandatory-work overrun.

`SearchIndexStatus` adds required nullable `unavailableReason`. It is null normally and
`vector_budget_exceeded` only for this fallback. State is then `unavailable`, model identity
is null and `embedded:0` is the unavailable sentinel, not a measured cache count. Declarations
remain the exact eligible head count, backend remains the selected backend, lexical is
complete, and every hit has lexical-only membership. This adds no warning, evidence meaning,
partial flag or truncation reason. Only optional-cache contention may consume this budget
and fall back on its expiry; BUSY itself does not authorize fallback. Observed database
faults, mandatory index/FTS contention, cancellation and root checks retain their typed rules.

## Model availability diagnostics (user-directed #393 amendment, 2026-10-28)

`unavailableReason` additionally reports `model_not_configured` when the daemon has no
configured model path, and `model_load_failed` after verification/loading fails. A successful
load clears the failure reason. Cancellation and vector-budget expiry alone do not create
a model-load failure. Temporary loading, idle residency and missing matrices alone keep null;
this diagnostic is not a readiness guarantee. Query-specific `vector_budget_exceeded` and
its lexical fallback behavior remain unchanged and take precedence for that query.

Workspace describe and CLI status add `searchIndex:{backend,unavailableReason}` with these
model diagnostics (not the query-specific budget reason). This pair is daemon-wide, not
query-pinned coverage, and includes no state, model identity or declaration/vector counts.
A standalone registry without a search service has null backend and reason. This amends
the earlier null-only model-unavailability wording; no evidence semantics or release-build
Metal defaults change.

## Retrieval is not evidence

A hit’s syntax Evidence proves only the measured declaration, not relevance. There is no
relevance provenance, confidence or numeric score. Zero hits proves no absence. Agents
inspect the exact declaration and call views, with optional returned-basis guards that can
conflict after publication. Search relevance never reattaches a durable anchor, creates a
binding or supplies evidence for another tool. Native completeness remains W2’s closure and
scope rule; vector completeness and selected backend are separate searchIndex metadata.

## Required observed checks

1. Both catalogs list ten tools in the specified order, schemas stay closed, describe reports
   toolVersion 2, existing tools/prompt remain unchanged, and the existing catalog headroom
   check still passes. Search accepts only the optional stored-role filter above, with an
   effective-role echo; no score/debug input, other filter or backend input is added.
2. Native card facts come from the existing capture/parse; displayKey changes cannot change
   card bytes/rank; source edits change the appropriate version-owned card/hash atomically.
3. Head publication atomically updates search FTS rows; retaining/releasing old revisions
   cannot change head BM25 statistics. Both existing trigram paths remain byte-identical.
4. Same-basis per-call index/cache coherence, optional-pin conflict, emitted-only closure,
   zero-hit incomplete native scope, byte-prefix clipping and syntaxOnly are observed.
   Missing vectors alone never set partial/coverageIncomplete.
5. A missing/evicted/decode-invalid vector is a miss; actual database/schema/I/O faults fail
   typed with no hits; contention is bounded and typed, never hidden as cache unavailability.
   Restart recomputes misses; background work does not delay publication or reorder FIFO.
6. Supported-model hash refusal, modelId separation, cap/LRU, cross-worktree/cache reuse and
   read-only gc size/count reporting are observed at ordinary local-tool scale.
7. Automatic backend choice and allowed fallback are observed; explicit Metal unavailability
   is a typed refusal. Status reports the actual backend and separate completeness. Existing
   accepted CPU/Metal (c) parity evidence is referenced, not replaced by weaker tolerances.
8. The #214 tuning report, chosen constants and any lexical-arm restriction receive review
   and become exact record text before the new tool is enabled. The reviewed tuning and built
   FTS size observations below satisfy item 8 under the 2026-10-26 owner ruling. No hosted
   production path or invented shipping gate is added.

## Item 8: reviewed tuning evidence (2026-10-26)

The 2026-10-26 item 8 report replays #214's corrected development set: 50 queries (30 concept, 6 named, 8 error-message, 6 JavaScript), 82 answer slots/73 distinct declarations, over 6,722 saved card-c candidates from Trellis `693c32cb2d47a16cf69c8265ae7a1291b71fbd93`. R@5/R@10 are any-answer query success rates; MRR uses the first correct hit through 100. This small, correlated, repeatedly inspected set is not a holdout or a generalization claim.

The frozen grid is k={20,60,100}, D={50,100,200}, wL=1 and wV={1,2,4}: 27 cells per backend. The actual capture-branch lexical implementation and matching bundled SQLite 3.53.2 supply transformed terms and BM25/ID ordering. Accepted saved CPU and Metal outputs from pinned voyage-4-nano revision `5fb34e870f49f19e79c94d5e118f5ee730133279` are joined by IDs, truncated to 1024 and quantized on both query and document sides with the accepted int8 representation. The same cells replay on Metal without backend-specific retuning. No model inference, hosted service or timing campaign was run.

The proposed 20/200/1/4 cell raises overall success@10 from 30/50 to 31/50 and concept success@10 from 12/30 to 13/30. It rescues q12 at 10 and loses no vector-only success at 10. Named 6/6, error 6/8 and JavaScript 6/6 success@10 are unchanged. It lowers overall success@5 from 24/50 to 20/50 (q22,q27,q35,q45 leave the top 5) and overall MRR from 0.2960059650230314 to 0.28429474173010805. Concept MRR rises from 0.1399098726919398 to 0.16323069981837932 and named MRR from 0.638888888888889 to 0.6444444444444445; error MRR falls from 0.40870859213250516 to 0.3188936781609195 and JavaScript MRR from 0.5833333333333334 to 0.48333333333333334. CPU and Metal aggregate metrics agree for every cell, but rankings are not promised equal.

Actual-transform BM25-only scores R@5=.16,R@10=.20,MRR=.10475726328435135; ordinary 60/100/1/1 scores .26/.34/.22488896033500286. The historical ASCII/no-cap BM25 control reproduces revision 3's .30/.46/.277 and missed-query IDs. Therefore historical unrestricted-BM25 or equal-weight results are not substituted for this actual 32-term representation. No ordinary cell rescues an error-message query at 10. The frozen optional exact-gate trigger was false, so no exact-limited cell was run; the report neither supports nor rejects an untested exact gate.

Saved vectors bind historical c card bytes, **not the newly serialized native capture cards**. This replay supports an explicit owner decision about the proposed fusion tradeoff on #214's saved representation using the current lexical transform; it does not claim new native-card embedding quality, native-card parity, or a new hardware-independent ranking guarantee. All cells, per-query arm/fusion ranks, wins/losses, input hashes and CPU/Metal receipts remain available in the report packet. No new recall, MRR, parity, timing or size acceptance threshold is introduced.

## Item 8: built FTS size observation

An untimed capture-branch build of the normal frozen MEDIUM cohort (1,000 files, 250 per language, 16,777,216 source bytes) published one head with 75,614 eligible cards and 75,614 ranking-FTS rows. All source hashes match the cohort manifest. Across the five physical FTS objects, `dbstat` reports **21,934,080 B (20.918 MiB)**: content 14,757,888; data 6,258,688; docsize 901,120; idx 12,288; config 4,096. The virtual table owns no additional pages outside those shadows. Version-owned cards/facts/terms and their two indexes are separate: **92,221,440 B**. Combined measured search objects are 114,155,520 B. The stopped whole `index.db` is **535,904,256 B**; FTS is 4.0929102% of that file and about 290.08 allocated bytes/card. No VACUUM/optimize or embedding run was applied. The private daemon was stopped and its object allocation/file size remained unchanged.

#199's linked original report §1.5 records **2,108,219,392 B** for the same MEDIUM source content. Independent manifest comparison confirms all 1,000 paths, lengths and SHA-256 values agree despite manifest versions 1/2. The current whole index is 25.419757% of that historical size, but **this is historical context, not a causal size delta for adding FTS**: extractor, schema, retained state and intervening compaction changes differ. #199's original 10k total 16,780,910,592 B and later stopped S-2 total 2,566,254,592 B are separate LARGE observations; no MEDIUM-to-LARGE extrapolation is made. Q8's names-only trigram tables are different indexes, unchanged by this measurement. Their reported 52,838,400 B whole-file increment and later 52,850,688 B two-group page sum are distinct statistics, not a comparable regression percentage. No new size budget or shipping gate follows from these measurements.

The [separate owner-approved hosted evaluation](https://github.com/SquashMerge/trellis/issues/211#issuecomment-6416200314)
compared voyage-4-lite at the same fixed hybrid settings: R@5 .42, R@10 .52 and
MRR@100 .29090339284698713, versus local nano .40/.62/.28429474173010805.
The owner kept local nano. That evaluation-only exception adds no hosted production path
and does not change the historical tuning report above.
