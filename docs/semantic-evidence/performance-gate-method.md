# #61 combined native + semantic-import gate — method for review

Status: preparation only. Base `origin/feature-scip` at `44320bc84425196e768a73f9e1123e4ade59c058`; branch `prep/61-combined-publication-gate`. No timed measurements, benchmark pass, verification lock, or parent #11 completion. #90 and feature-scip→main must land first. This method is a proposal, not ratified new acceptance criteria.

## Scope and reuse

Measure one contiguous full native index → supplied binary SCIP import(s) → coherent SQLite publication interval. Producer/artifact generation and manifest preparation are outside timing. Do not sum separately measured components. Preserve the CLI-return/published-pin/request consistency checks from the accepted #90 runner. #90's medium/large incremental LOCAL p95 and untimed semantic setup do not measure #61.

After #90 lands, reuse its daemon/environment/retained-command/artifact-import/selected-proof support. If necessary, extract only this common support, preserving #90 behavior. Do not live-copy its unaccepted Factory candidate or create a second lifecycle/parity framework. Add a thin combined adapter, SMALL support, raw sample aggregation, actual worker RSS, SQLite byte accounting and canonical serialized native+semantic byte accounting. The offline qualifier prepared here evaluates arithmetic/declared qualifications only; it is not a real measurement runner or proof of authenticated receipts.

## Artifact proposal — review required

Generate source bytes with the canonical manifest-v2 generator, then verify exact tier inventory, lengths and SHA-256 before emitting anything. LARGE is exactly 10,000 files / 134,217,728 source bytes, 2,500 files/language, including 40 Rust module roots. The generator emits one deterministic binary SCIP preparation artifact per language and a separate preparation inventory: source/artifact byte hashes, generator identity, encoding, counts and coverage. It must never masquerade as a closed admission manifest or claim actual execution of a pinned producer.

Proposed density: every template-backed callable declaration, including scaffold methods/abstract signatures, contributes a definition occurrence and SymbolInformation; up to the first two provable direct local/explicitly imported-peer `f0` calls per body contribute source-backed occurrences. No guessed dynamic/super/dispatch target. Rust module roots remain declared documents without invented callable claims. Exact per-language/document counts and omissions are retained for review. These density choices are proposals, not a new floor and not real-producer quality qualification. Definition/call exact native joins still need production admission/import qualification after integration; generator regex/template matching alone does not establish them.

### Blocking interface mismatch on this base

Current production admission (`src/scip_admission.rs`) admits one artifact and at most 1,000 covered documents per manifest, with a 1 MiB manifest cap. Every import must include the complete sourceDocuments inventory. LARGE's compact sourceDocuments array alone is **1,424,717 bytes** with cohort-root-relative `language/file` paths, above 1,048,576 before other fields. Lower semantic density/representative coverage does not fix this. A language has 2,500 documents; same-producer shard imports replace its previous facts/coverage, not accumulate (`src/store.rs`). Binary capture also limits total candidate facts to 100,000, each document to 10,000 occurrences and 10,000 symbols, relationships to 20,000, and artifact bytes to 64 MiB; full generated per-language loads must not be described as admitted. Direct synthetic publication bypassing raw admission would not prove the required import contract.

Producer IDs/versions/hashes are pinned to the real reference profile. #90 already constructs a synthetic fixture with the pinned scip-typescript selection; that does not prove that executable generated it. Reusing that convention for a representative four-language set needs explicit review and honest external synthetic labeling. No forged generator executable hash or unknown closed-manifest extension is proposed.

**Required owner decision:** approve a production-compatible bounded inventory/batch import design or another valid interface for this exact full source-set. No cap relaxation, partial source-set substitution, shard accumulation assertion, admission bypass, or fake producer run is implemented in this preparation. Until resolved or a valid staged alternative is ratified (below), the executable full LARGE gate remains blocked even after #90 merges.


### Existing #90 staged alternative (also review required)

Readonly runner review confirms #90 does **not** bypass these caps: it indexes four sentinel source files (one/language) at r1, imports one JavaScript `f0` artifact against that four-document revision, then expands to the complete native cohort and publishes FULL r2 with selected raw-linked evidence rejoined from r1. Import and native FULL are setup there, not its measured p95.

A possible #61 alternative is this **explicitly representative staged scope**, with a contiguous interval that includes every required seed-native publication, semantic import and final full-cohort native publication/rejoin. It is not a direct import against a 10k captured source revision and does not provide semantic coverage of every generated document. A native-only r2 interval, an untimed r1/import, or summing separate component times would fail #61. Frozen manifests/identity preparation outside timing and deterministic virgin-state reset still need a real reviewed mechanism; do not silently move source-dependent preparation into the interval. The owner may ratify this narrower supplied semantic scope/timing boundary instead of changing production inventory admission. No choice is made in this preparation.

## Proposed run schedule / environment — review required

- Three fixed repetitions per tier, in SMALL, MEDIUM, LARGE order; no warmup measurement, replacements, favorable retry, adaptive density, or extra sample after looking at a result. Report each original attempt. This is a proposed finite run count, not ratified acceptance.
- A fresh isolated HOME/XDG, workspace, daemon and virgin derived index per repetition; generated source and immutable supplied artifacts prepared outside the interval. Record whether preparation/page cache is warm. “Cold” means virgin Trellis state, **not** purged OS/SSD caches. Do not claim OS-cold results without a separately approved method.
- Recheck after both merges: exact source/toolchain/release binary SHA, generator/cohort/artifact/manifest/contract/policy hashes, raw decoder version, source-set/root/revision/input digests, requested producer/language/document coverage and observed complete host inventory. Freeze every command and inclusion boundary before launch.
- Owner-local `mac-mini-m5-pro-v1` only for authoritative budgets. Record actual hardware/OS/toolchain inventory, not just intended profile. No CI/workstation extrapolation.
- Before any future timing, notify user, coordinate host reservation, inspect load, active builds/tests/profiles/timing, containers, disk, and canonical lock ownership. Idle daemons retained on supervisor direction are not owner exemptions; activity/load matters. Acquire `/tmp/trellis-factory-verify.lock` only after dependencies, method ratification and quiet-host clearance. This preparation neither probes nor acquires it.

## Interval and resource boundaries — review required

Start immediately before issuing real FULL native index against the frozen virgin state. Include native capture/index, all necessary semantic admission/decode/join/import CLI requests, FIFO waits and final SQLite publication through final successful CLI return. Final result must have coherent native+semantic selection and all requested coverage; intermediate native-only publication is not final success. Artifact/source manifest binding must already be prepared through the production identity contract outside the interval, without creating a premeasured native index. Exact accepted runner preflight/reset mechanism remains to be implemented and reviewed after merge.

RSS must include the long-lived indexing/import daemon, not only the CLI. Prefer the actual worker's OS high-water RSS from a fresh per-repetition process, and record CLI RSS separately. Any essential instrumentation is sequenced after #90 integration, not added to overlapping source now. A sampled RSS lower bound cannot establish a peak limit pass. State whether setup/post-run validation contributes to a conservative lifetime high-water value.

Record steady SQLite bytes only at the reviewed quiescent/retention boundary, and database+WAL+SHM publication high-water bytes over the interval. Sampling alone may miss peaks: the peak method/instrumentation and closure of all owned processes must be reviewed before acceptance. Review the separate warm-publication/retained-pin/GC storage trajectory described by #73/#100 and its relation to #61’s timed publication peak and one-revision steady boundary; neither a timed-interval-only peak nor discarding combined history is preapproved. Do not force a noncontract checkpoint/vacuum/delete retained history just to reduce storage. Document index/request/history/cache inclusion separately and honor the one-retained-revision/native-only distinction in Decision 0006/#73.

Canonical serialized evidence denominator must cover native **and imported semantic** evidence at the same selected snapshot/retention scope. Its exact combined byte container is **not yet ratified**. Prefer an owner-reviewed current-schema extension/re-pin of #73’s approved length-prefixed relational stream, preserving its stream rules and explicit reachable table/column ordering. #73’s owner withdrew an earlier logical-record replacement; a different ruler requires explicit owner approval. Authentication of raw/provenance/coverage is mandatory, but does not automatically authorize adding raw artifact/envelope bytes to inflate this denominator. Report original/raw input byte counts separately pending that ruling. It is not artifact size, SQL bind bytes, row counts, hash length or an estimate. The precise deduplicated record inventory/export and peak byte collector are unresolved implementation items; freeze their schema/inclusion/exclusion list and independent validation before timing. No passing ratio while this denominator is missing/unvalidated.

## Existing thresholds — unchanged

| Tier | Source files/bytes | Native floor, this run | Median wall | Peak RSS |
|---|---|---|---|---|
| SMALL | 100 / 1,677,722 | ≥5,000 per language | ≤2 s | ≤512 MiB |
| MEDIUM | 1,000 / 16,777,216 | ≥50,000 per language | ≤30 s | ≤2 GiB |
| LARGE | 10,000 / 134,217,728 | ≥500,000 overall | ≤180 s | ≤8 GiB |

Native fact qualification follows #73's revision-manifest count of declarations + calls + control regions through `revision_documents`, not reference counts or generator-declared SCIP totals. The current produced schema/columns must be rechecked after integration. #73's older schema-v8 denominator is native-only; do not copy it as a current combined schema23+ denominator without reviewing its selected-row inventory and semantic extensions.

MEDIUM and LARGE: steady SQLite ≤3× canonical serialized evidence **and** ≤4 GiB; peak database+WAL+SHM ≤2× steady. Show every numerator/denominator and exact ratios. Report raw samples, median, maximum RSS and storage, cache state, host, commands and hashes. LARGE alone does not complete SMALL/MEDIUM or parent #11. #73 native-only and #41/#90 incremental measurements are separate.

## Qualification, stop and drop rules

1. Missing integration/method/host/artifact/coverage/denominator/resource instrumentation: `notMeasured`, no launch/pass/baseline. Generation success is not import qualification.
2. Each actual attempt retains raw stdout/stderr, exit status, timestamp, binary/source/command/artifact bindings, request/pin identity, diagnostics/resource bytes, and cleanup outcome, including a failed first attempt. Never erase or relabel a failure as an unattempted run.
3. Source/cohort mismatch, unsupported artifact, source/raw digest mismatch or misrepresented freshness, corrupt/partial/mixed result, incomplete requested coverage, missing provenance, unexpected native-only result, missing facts/resources or foreign host contention: stop for diagnosis; the campaign is unqualified/incomplete and cannot claim a passing median. Contract-valid `possiblyStale` r1→r2 rejoin is not itself a fault: predeclare expected captured/derivedFrom/freshness and coverage states, and do not claim fresh executable evidence. No replacement run under the same frozen campaign. Any new campaign requires reviewed changes and fresh preserved identity.
4. Threshold failure on otherwise qualified data is `failed`, stays an open #11 blocker, and never establishes/refreshes semantic-v1. Use the full predeclared set for median wall time; all RSS/storage samples must satisfy limits. A single slow wall-time sample alone does not change the ratified **median** criterion; do not invent per-sample wall-time failure or stop early and report a partial median. Stop immediately for safety/correctness faults; retain completed threshold failures and remaining-not-attempted status if the campaign cannot finish.
5. No #332-style faster-pair claim: #61 has absolute budgets, not an A/C gain experiment. Apply analogous freeze/qualification/fault retention/no-rescue discipline without importing a new reproduced-gain acceptance rule.
6. Reap only owned processes; retain logs/reports/artifact and digest inventories; review scratch deletion against exact inventories. Preserve `./tools/verify`, pinned Clippy and fmt after integration when permitted, separately from timings. No verification lock or full verify during this preparation.

## Raw digest correction

#61's issue text names `baleyg.semantic-fact.v1\0`. Ratified contract-v1 § raw evidence, Decision 0002 and production code instead require **`trellis.semantic-fact.v1\0`** (and `trellis.capture-proof.v1\0`). This method explicitly follows that ratified domain; it does not silently equate domains. The original binary digest, closed CapturedScipFactV1 canonical bytes/full digest and deduplicated authenticated proof must be validated and retained through production import, not authored normalized SemanticCapture v1 or a second decoder.

## Review decisions before launch

1. Production-compatible large source inventory/coverage import interface, preserving raw admission.
2. Synthetic artifact density/omissions and honest reference-profile identity convention.
3. Fixed three repetitions per tier and fresh-state/cache policy.
4. Exact contiguous boundary, worker RSS, SQLite peak and canonical denominator inventory/instrumentation.
5. Complete actual host, merged-source/binary/artifact freeze and evidence/cleanup receipts.

No acceptance constant was changed. New density/run-count/cache/instrumentation choices remain proposals until reviewed. This preparation is not benchmark authorization.
