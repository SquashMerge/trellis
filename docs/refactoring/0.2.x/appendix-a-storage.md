# Storage-layer deviations: diagnosis and refactoring plan

> Analysis notes behind the [0.2.x refactoring spec](../0.2.x-spec.md), track S. Written by a read-only analysis agent at v0.2.0 (`d84f264a`); "I" is that agent. Step numbers here are the analysis's own: the spec renumbers them (its S1 merges steps 1–2, S2–S7 are steps 3–8, S8 is step 11, S9–S10 are steps 9–10, S11–S13 are steps 13 (capture), 12 and 13 (classes), S14 is step 14). `rules.json` and `decls.json` are the rules-check and per-declaration metrics dumps taken for the baseline; they are not in the repository. Estimates and "not verified" marks are kept as written.

Scope: `src/store.rs`, `src/store/**`, `src/search_vectors.rs` on `main` at v0.2.0 (d84f264a). Read-only analysis.

**How the numbers were obtained**

- Pair and reference counts (`n=`) are from [`baseline-deviations.txt`](baseline-deviations.txt) (107 storage pairs, 899 refs). These are authoritative.
- Per-function attribution inside a file is from my own regex scan of `crate::<module>` paths and `use` aliases, binned by enclosing item. It is textual, so it over-counts type mentions relative to the rules tool (e.g. `store.rs -> native_evidence.rs` is n=64 in the tool, 194 textual mentions). Use it for *where*, not *how many*.
- "Est." marks a split of one pair's count between clusters; those splits are estimates, not tool output.
- Callers were found with grep, because the Trellis index went into `rebuild_in_progress` mid-session (see Tool gaps).

---

## 0. Diagnosis in one page

There are three independent causes. They need different fixes.

1. **`Store` / `EvidenceResponse` is the application facade, not a persistence layer.** Entry code calls view-shaped methods directly: `sequence_at`, `navigation_at`, `classes_at`, `class_diagram_at`, `architecture_response_at`, `architecture_view_at`, `metrics_at_prepared`, `mcp_search_candidates`, `files_at`, `syntax_matches_at` and so on. Counting `.method(` call sites of Store/EvidenceResponse methods outside storage: **entry 288, indexing 259, query 20**. The query layer almost never calls storage. The only query-to-store references are `store::SelectedIntegrity` (in `search_rank.rs`, `metrics.rs`), `store::id_codec` (`navigation.rs`) and test-only `WorkspaceIdentity::discover`. The query modules are a pure projection library; the code that loads rows, calls the library and shapes the answer lives under `store`. That is the whole `storage>query` problem (425 refs).

2. **The persisted record vocabulary is defined next to the code that produces it.** `native_evidence.rs:276-550` (`DocumentKey`, `Declaration`, `Call`, `Artifact`, `DeclarationRole`, ...) and the record types at the top of `semantic_evidence.rs` (`Target`, `SemanticPublication`, `CapturedComponents`, `Freshness`, ...) are exactly what storage encodes and decodes. They sit in 6,531- and 3,431-line indexing files alongside tree-sitter extraction and validation. The same holds for `CaptureStat` / `CaptureInputObservation`, `IndexOptions` / `ReconcileOptions`, `semantic_batch::limits`, `skipped_config`, `GenerationOrigin`, `BoundaryReason` and the search candidate/card types.

3. **Part of the indexing pipeline lives under `store`, in mutual recursion with indexing.** `store/native_first/**` is the semantic catch-up ("S") scheduler unit. `index_coordinator::cooperative_semantic_unit` (`src/index_coordinator.rs:2271`) calls `Store::semantic_maintenance_unit` (`src/store/native_first/continuation.rs:226`), which drives `semantic_import::JoinContinuation` and `semantic_batch::RetainedStageContinuation`, which call back into `store.with_native_reader` (7 call sites in `semantic_import/staged/continuation.rs`). `store.rs` also runs extraction itself: `recreate_index_exclusive_with_hook` calls `Capture::admit` and `native_evidence::spool_full_capture` (`store.rs:13651-13652`, `13682-13683`), and `prepare_local_revision` / `prepare_local_native` call `measure_captured_documents`, `assemble_selected_revision` and `indexer::measure_persisted_change` (`store.rs:17475`, `17507`, `17656-17669`).

The reverse direction also exists, so these are cycles rather than one-way leaks: `capture.rs` uses `store::compare_capture_observations` and `store::requests::TypedFailure`; `semantic_evidence.rs` uses `store::AttestedProfileProducer`, `store::SelectedRawBatch` and `store::id_codec`; `semantic_import` uses `store::NativeDocumentRows` and `store::native_first::witness::*`; `semantic_batch` uses `store::native_first::copy::CopyFilter` and `witness::OriginalSourceWitness`.

---

## 1. Responsibility map

### 1.1 `src/store.rs` (23,040 lines; 22,711 production)

| Lines | Size | Responsibility | Upward modules (textual mentions) |
|---|---|---|---|
| 64-559 | 496 | Publication gate, permits, read gate, `RebuildInProgress`, test hook types | none in production (test-only hook type mentions `FullDocumentSpool`); `PrivateStageObservation` / `SemanticHistory` DTOs mention native/semantic records |
| 560-836 | 277 | `Store` struct, recovery enums, typed errors (`PinExpired`, `SelectedIntegrity`, ...) | `metrics::MetricsMemo` field (`store.rs:579`) |
| 837-1064 | 228 | Publish stage enums, schema constant `CACHE_SCHEMA_V43` (927-1064), `DATABASE_SCHEMA_VERSION` | none |
| 1065-1480 | 416 | `IndexConnection`, `NativeReader` (1184-1340), `InventoryStream`, `ReadRevision` | native_evidence 21 (types), semantic_batch 1, scip_admission 1, search_capture 1 |
| 1481-1660 | 180 | Metrics view DTOs (`MetricsOptions`, `MetricsPayload`, `Metrics*Row`), `EvaluateRules` | metrics, rules, references |
| 1661-1739 | 79 | `EvidenceResponse`, `OptionalRecovery`, `ReadFence` | native_evidence 1 (type) |
| 1740-1937 | 198 | Metrics read plumbing, `Store::metrics_document` (1802) | metrics 3, native_evidence 4 |
| **1938-3338** | **1,401** | **`impl EvidenceResponse`: Architecture.** `architecture_config` (2053), `architecture_response_at` (2070), `app_assignment_at`, `endpoints_at`, `architecture_response_scoped` (2351), `architecture_view_at` (2581, 252 lines), `architecture_details_at`, `validate_rules_edit`, and the one real read `architecture_snapshot_regions` (2941, 398 lines) | **rules 61, references 20, architecture_details 9, interfaces 4, rules_edit 3**, skipped_config 2 |
| **3339-3952** | **614** | **`impl EvidenceResponse`: Metrics.** `metrics_at_with_config` (3373, 580 lines) | rules 5, metrics 5 |
| 3953-4202 | 250 | `EvidenceResponse` delegating wrappers (`sequence_at`, `classes_at`, ...), `EvidenceFence` | class_diagram 3, behavior 2, navigation 2, file_tree 1 (signature types) |
| 4203-4865 | 663 | SQLite file witnesses, `protected_sqlite_open`, `replace_index_and_release_obsolete`, `gc_unlink_sqlite` | none |
| 4866-5550 | 685 | Schema shape validation, maintenance progress handler, retention helpers, read deadline, `open_index`, recovery classification | none |
| 5551-5794 | 244 | Typed storage errors and contention classification (`StorageFull`, `SqliteContention`, hot journal) | none |
| 5795-5935 | 141 | `validate_graph` | none |
| 5936-6156 | 221 | Class presentation helpers (`class_metadata`, `presentation_class`, `resolve_class*`) | class_diagram 4 |
| 6157-7022 | 866 | Write-side projection: `PublicationRows`, `ImmutableAppend` (6556-6885), `write_graph_document`, `precompute_document` | native_evidence 20, classes 6 |
| 7023-7504 | 482 | Semantic target codec, `AttestedProfileProducer`, `authenticate_stored_batch` (7179, 213 lines), `SelectedRawBatch` | semantic_evidence 11, native_evidence 5, semantic_batch 3, semantic_import 2, scip_admission 1 |
| 7505-7624 | 120 | Maintenance budget, manifest copy | none |
| 7625-8720 | 1,096 | Semantic read codecs: `read_semantic_tuple_with_reasons`, `read_semantic_history`, `read_semantic_facts`, `read_semantic_call_facts` | semantic_evidence 42, native_evidence 12 (all types) |
| 8721-9507 | 787 | Semantic write: coverage, `write_staged_raw_rows`, `write_staged_semantic`, `write_producer_descriptor`, `write_semantic` | semantic_evidence 13, semantic_batch 4, semantic_import 3 |
| 9508-10036 | 529 | `write_semantic_tuple`, `release_semantic_tuple` | semantic_evidence 2, semantic_import 1 |
| 10037-10677 | 641 | `write_native` (cognitive 104) | native_evidence 3, classes 3, capture 1 |
| 10678-11203 | 526 | Bootstrap and paired-manifest validation, `revision_producer_binding` | capture 6, native_evidence 3 |
| 11204-11833 | 630 | Capture comparison (`compare_capture_observations` 11244, `compare_capture_snapshot_inner`), `validate_reconcile_inventory` (11524) | **capture 35**, skipped_config 5, native_evidence 3 |
| 11834-12745 | 912 | Native row decoders (`fetch_native_declarations_v8`, `..._calls_v8`, `..._control_regions_v8`) | native_evidence 26 (types) |
| 12746-12920 | 175 | `impl Store`: gate, queue wakers, `enter_publication` | index_coordinator 1 (`diagnostic_marker`, 12817) |
| 12921-13511 | 591 | open, `unopened`, `status_existing_readonly`, staged index create/validate | metrics 1, indexer 1 |
| 13512-13808 | 297 | `recreate_index_exclusive_with_hook` | capture 2, native_evidence 2, indexer 2 (**calls `Capture::admit` and `spool_full_capture`**) |
| 13809-14658 | 850 | initialize, connect, hot-journal recovery, recovery baseline, status | none |
| 14659-14963 | 305 | Root/identity/writer-session accessors, `compose_selected_class_catalog` | indexer 4 (`IndexOptions`), classes 3 |
| 14964-15113 | 150 | Evidence admission (`with_evidence*`, `evidence_response`) | none |
| 15114-15677 | 564 | `publish*` entry points; `publish_semantic_batch_statements` (15295, 294 lines) | semantic_batch 11, capture 3, native_evidence 2, producer 1 |
| 15678-15963 | 286 | `publish_native_semantic*`, `publish_validated_stream_*` | semantic_evidence 10, capture 8, native_evidence 7 |
| 15964-16342 | 379 | `publish_*_for_tests` fixtures (feature `test-support`) | indexer 9 (`project_native_documents`), capture 9, semantic_evidence 10, native_evidence 8 |
| 16343-16850 | 508 | Recovery publish, bundle validation, selected-source admission | capture 14, native_evidence 10 |
| **16851-18296** | **1,446** | **Reuse/preflight/incremental preparation**: `selected_reusable_native`, `selected_capture_unchanged`, `reuse_unchanged_native_head` (283), `prepare_local_revision` (157), `compose_local_revision`, `prepare_local_native`, `preflight_*_reuse`, `prepare_stream_class_catalog` | native_evidence 23, capture 8, classes 8, indexer 5 |
| 18297-19195 | 899 | `publish_inner_checked_target_with_local`: the one publication transaction (closure at 18534 has cognitive 131) | classes 6, native_evidence 3, semantic_evidence 3 (`validate_evidence` at 19007) |
| 19196-19633 | 438 | `read_revision_for`, `with_native_reader`, raw proof/inputs reads, `semantic_at` | semantic_batch 2, semantic_evidence 2 |
| 19634-20470 | 837 | Retention, GC, `maintenance_step` (20042, 225 lines), `collect_unreferenced` | semantic_batch 4 (`BatchLimits`) |
| 20471-21325 | 855 | Selected-document fetch (`fetch_selected_header`, `project_fetched`), native reads `native_*_at` | native_evidence 40 (types), evidence_budget 6, indexer 2 (`project_document`), test counters |
| **21326-22479** | **1,154** | **View reads**: `navigation_in` (165), `classes_in` (109), `class_diagram_in`, `symbols_in`, source rows, `tree_metadata*`, `files_in`, `methods_in`, `file_symbols_in` (104), `syntax_matches_in` (137) | class_diagram 13, navigation 10, file_tree 3, classes 2 |
| **22480-22711** | **232** | **`sequence_in`, `read_graph_for`, `query_view_in_for`** | behavior 5, indexer 1 |
| 22712-23040 | 329 | Inline tests and 18 `mod *_tests;` declarations | n/a |

By enclosing item (textual scan), `store.rs` production lines split as:

- about **8,900** lines in items with no upward reference;
- about **5,000** in items whose only upward references are `native_evidence` / `semantic_evidence` / `capture` (almost all record types);
- about **3,400** with other indexing references;
- about **1,600** query-only;
- about **3,600** mixed query and indexing (dominated by the 899-line publication function and the Architecture block).

### 1.2 `src/store/*.rs` and `src/search_vectors.rs` (production lines, excluding inline test modules)

| File | Prod lines | SQL lines | What it is | Upward pairs (n) |
|---|---|---|---|---|
| `topology.rs` | 1,798 | 6 | Filesystem identities, locks, writer session | none |
| `requests.rs` + `requests/failure.rs` | 2,326 + 246 | 45 | Durable request queue | index_coordinator 3 (`diagnostic_marker`), indexer 3 (`IndexOptions`, `ReconcileOptions`), scip_admission 2 (`validate_selected_role`, 1351/1388), descriptor 2 (`GenerationOrigin`) |
| `symbol_trigrams.rs` | 503 | 12 | FTS5 trigram prefilter | none |
| `id_codec.rs`, `anchors.rs`, `native_join_budget.rs`, `published_admission.rs` | 65 / 197 / 437 / 188 | - | Codecs, budgets, admission cache | none real (`published_admission` has a 1-ref `super::*` artifact) |
| `semantic_payload.rs` | 549 | 19 | Shared semantic payload rows | semantic_evidence 1, semantic_import 1 (`rejoin_provenance_id`, line 418), glob 1 |
| `semantic_retention.rs` | 386 | 34 | Revision-owned recovery status; attaches the producer retention database | producer/retention 10, producer/state 2, producer/mod 1, glob 1 |
| `captured_witness.rs` | 156 | 11 | Private capture-side admission witnesses | staged/publication 1 (`read_staged_documents`, line 22), glob 1 |
| `search.rs` | 917 | 33 | Capture cards, lexical corpus, MCP search reads | search_rank 20, search_card 4, search_model 4, search_capture 3, native_evidence 4, glob 1 |
| `search_startup.rs` | 192 | 1 | Startup head discovery | search_rank 1, glob 1 |
| `search_vectors.rs` | 514 | 13 | Machine-wide embedding cache | search_embed 12 (`QueryBudget` parameter at 368/392, `DIMENSIONS`) |
| **`architecture_core.rs`** | 501 | **0** | In-memory `ArchitectureCore`. "`compute` ... touches no SQLite" (its own doc, line 282). Owned by `http::architecture` | apps 4, interfaces 3, metrics 2, references 7, references/work 3, rules 7, native_evidence 2 |
| **`graph_read.rs`** | 1,522 | 23 | `SnapshotGraphSource` implementing the query-layer trait `graph_query::GraphSource` (183); `navigation_calls` (420); `sequence_calls` (595); sequence `Enrichment` (909-1460) | graph_query 37, behavior 34, navigation 11, behavior_columns 6, behavior_incidental 2, native_evidence 3 |
| **`graph_read/sequence_callbacks.rs`** | 466 | **0** | Sequence callback blocks (#888) | behavior 34 |
| **`sequence_hops.rs`** | 287 | **0** | Sequence boundary hops against interface evidence | behavior 8, interfaces 4, rules 3, behavior_columns 1 |
| **`semantic_display.rs`** | 2,104 | 33 | Mixed. Producer selection and `status_semantics` (storage: used by `read_status` via `published_admission.rs:176`), plus `view_walk` (721), `architecture_semantic` (1497, cognitive 115) and `mcp_*` tiers (view) | graph_query 24, references 4, semantic_evidence 4, indexer 1 (`compare_reader_calls`) |
| **`mcp_read.rs`** | 2,375 | 21 | MCP tool projections as `impl EvidenceResponse` (14 impl blocks) | native_evidence 4, semantic_evidence 3 (vocabulary and `language_order` only) |
| `native_first.rs` | 919 | 62 | N/P selection SQL, `write_pending`, `publish_pending_semantic` (542), projection reads | semantic_batch 10 (batch parameter methods), graph_query 4 (`BoundaryReason`), index_coordinator 1 (test fixture, 417), scip_admission 1, semantic_import 2 |
| **`native_first/continuation.rs`** | 903 | 2 | The bounded S unit state machine (`semantic_unit` 300-830, closure cognitive 99) | semantic_import 13, semantic_batch 12, staging 9, retained_continuation 6, staged/continuation 6, scip_admission 2, admit 1 |
| **`native_first/witness_preparation.rs`** | 332 | 7 | Stages original associations into the stage database | staging 26, semantic_batch 13, semantic_import 5, semantic_evidence 2 |
| **`native_first/oracle.rs`** | 362 | 1 | In-memory FULL comparison oracle | semantic_import 6, semantic_batch 3, staged/publication 1, semantic_evidence 1 (`validate_evidence_with_attested_batch`, 352) |
| **`native_first/context.rs`** | 250 | 18 | Pending eligibility from producer profiles | producer/profiles 5, producer/mod 3, worktree 1, descriptor 1 |
| `native_first/local_targets.rs` | 413 | 18 | Disposable LOCAL declaration index inside the stage | staging 7 |
| `native_first/witness.rs` | 2,144 | 43 | Original-admission readers over the main index | descriptor 8, semantic_evidence 6 (types), scip_admission 3 (`hexhash`, trust-origin constants) |
| `native_first/copy.rs` | 341 | 13 | Copy published semantic rows to a new revision | descriptor 3 (`SourceManifestHasher`) |

Tests: 36 `*_tests.rs` files under `src/store/` (13,846 lines) all use `use super::*`; inline test modules in non-test files add about 5,400 lines. `store.rs` has 60 mentions of `_for_tests` and 168 uses of the private `selected_ensure!` / `control_ensure!` macros.

---

## 2. Classification of the upward dependencies

Pair counts are from [`baseline-deviations.txt`](baseline-deviations.txt). Every one of the 107 pairs is assigned to exactly one cluster; totals sum to 899.

### (a) Misplaced orchestration: move up, call a narrow storage API

| Cluster | Pairs | Refs | Content |
|---|---|---|---|
| **Q1 Architecture / rules / metrics** | 14 | 166 | `store.rs` -> rules 85, metrics 19, references 17, architecture_details 9, interfaces 5, references/tsconfig 2, rules_edit 2, references/external 1; `architecture_core.rs` -> rules 7, references 7, apps 4, interfaces 3, references/work 3, metrics 2 |
| **Q2 Sequence / graph / navigation** | 14 | 177 | `graph_read.rs` -> graph_query 37, behavior 34, navigation 11, behavior_columns 6, behavior_incidental 2; `sequence_callbacks.rs` -> behavior 34; `sequence_hops.rs` -> behavior 8, interfaces 4, rules 3, behavior_columns 1; `semantic_display.rs` -> graph_query 24, references 4; `store.rs` -> navigation 8, behavior 1 |
| **Q3 Class reads** | 1 (+part) | 10 (+2 est.) | `store.rs` -> class_diagram 10; the read part of `store.rs` -> classes (21593, 21607) |
| **P1 Semantic catch-up (S) pipeline** | 29 | 151 | Every `native_first/**` pair into semantic_batch/**, semantic_import/**, producer/**, scip_admission, index_coordinator; plus `captured_witness.rs` -> staged/publication 1 and `semantic_payload.rs` -> semantic_import 1 |
| **P2 Incremental / recovery pipeline in `store.rs`** | part of 3 pairs | est. 35-45 | Function calls inside `store.rs` -> native_evidence (64), -> indexer (21), -> capture (81): `Capture::admit`, `spool_full_capture`, `measure_captured_document(s)`, `assemble_selected_revision`, `captured_components`, `captured_revision_header`, `document_witness`, `validate_native_graph(_records)`, `measure_persisted_change`, and the `*_for_tests` fixtures calling `project_native_documents` |
| **P3 Semantic batch publication in `store.rs`** | 9 | 66 | `store.rs` -> descriptor 16, semantic_batch 14, staging 11, semantic_import 8, limits 5, scip_admission 5, producer/retention 3, testing 2, staged/publication 2. Mixed: vocabulary (limits, `Descriptor`), instrumentation counters, and real orchestration (`publish_semantic_batch_statements`, `authenticate_stored_batch`) |
| **P4 Producer retention** | 3 | 13 | `semantic_retention.rs` -> producer/retention 10, producer/state 2, producer/mod 1 |

Evidence that Q1 and Q2 are view assembly rather than persistence:

- `EvidenceResponse::architecture_response_at` dispatches on a string `kind` (`"endpoints"`, `"references"`, `"prompt"`, `"check"`, `"explain"`) and calls `rules::prompt_apps`, `rules::evaluate_prepared`, `rules::path_apps` (`store.rs:2070-2125`).
- `Store::sequence_in` loads one document, then calls `behavior::build_sequence_work`, `graph_read::sequence_calls` and `graph_read::syntax_callbacks` (`store.rs:22499-22550`).
- `architecture_core.rs`, `sequence_hops.rs` and `sequence_callbacks.rs` contain no SQL at all.
- A clean seam already exists for Architecture: `EvidenceResponse::architecture_input` (`architecture_core.rs:244`) does the read, and `ArchitectureCore::compute` (285) does the rest without SQLite.

### (b) Misplaced vocabulary: move down to shared (or storage)

| Cluster | Pairs | Refs | Items |
|---|---|---|---|
| **V1 Native evidence records** | 5 | 77 | `native_evidence.rs:276-550`: `Producer`, `SourceSet`, `DocumentKey`, `Document`, `Revision`, `Coverage`, `Provenance`, `Range`, `Signature`, `Key`, `Parameter`, `Header`, `DeclarationRole` (406), `Declaration`, `Call`, `ControlRegion`, `Reference`, `Measurement*`, `CapabilityReport`, `Artifact` (struct only); constants `PRODUCER`, `NATIVE_VERSION`, `LANGUAGES`; pure name functions `lookup` (604) and `verified_callee_lookup` (703). Pairs: `store.rs` 64, `mcp_read.rs` 4, `search.rs` 4, `graph_read.rs` 3, `architecture_core.rs` 2. Est. 50-55 of the 77 are record types; the rest of `store.rs`'s 64 are P2 function calls and `FullDocumentSpool` methods |
| **V2 Semantic evidence records** | 6 | 38 | `Target`, `SemanticPublication`, `SemanticSnapshot`, `CapturedComponents`, `Producer`, `Coverage`, `CoverageReasons`, `Freshness`, `SymbolKey`, `Provenance`, `Resolution`, ..., `LANGUAGE_ORDER` / `language_order` (`semantic_evidence.rs:915-919`). Pairs: `store.rs` 22, `witness.rs` 6, `semantic_display.rs` 4, `mcp_read.rs` 3, `witness_preparation.rs` 2, `semantic_payload.rs` 1. All but the `validate_evidence` call (`store.rs:19007`) are types |
| **V3 Capture records** | 1 | 81 (est. 35-40 vocabulary) | `CaptureStat` (`capture.rs:92`), `CaptureInputObservation` (145), `ROOT_CONFIG_INPUTS`, `ROOT_TOOLCHAIN_INPUTS`, `is_root_toolchain_input`, `EXTRACTOR_INPUT_KEY`, `nested_config_names`, `language_of_path`. The other half is `&Capture` itself; see (c) |
| **V4 Small constants and helpers** | 14 | 33 | `skipped_config` (5): the whole 80-line file is constants and messages. `semantic_batch/limits` (5): the file has no crate dependencies. `evidence_budget` (2): `row_limit`, `max_row`, `total`, `CLASS_EXTRACTION`, `graph_bytes`. `index_coordinator::diagnostic_marker` (`index_coordinator.rs:77`): `store.rs` 1, `requests.rs` 3. `IndexOptions` / `ReconcileOptions` (`indexer.rs:15-36`): `requests.rs` 3, and est. 12 of `store.rs` -> indexer 21. `GenerationOrigin`: `requests.rs` 2. `scip_admission::{hexhash, SYNTHETIC_TRUST_ORIGIN, REFERENCE_TRUST_ORIGIN}`: `witness.rs` 3. `indexer::compare_reader_calls`: `semantic_display.rs` 1. `BoundaryReason`: `native_first.rs` -> graph_query 4 (also fixes the existing `shared>query` deviation `model.rs` -> `graph_query.rs`). `search_capture::CapturedCard`: `store.rs` 1. `requests.rs` -> scip_admission 2 is a validation call, listed in (c) |
| **V5 Search vocabulary** | 6 | 44 | `search_rank::{SearchCandidate, SearchCandidateKey, SearchRole, SearchPolicy, CANDIDATE_DEPTH}` (`search.rs` 20, `search_startup.rs` 1); `search_card::{SearchCard, PreparedFacts, Ancestor, is_eligible}` (4); `search_capture::CapturedCard` (3, one of which is the `render_card` call); `search_model::SearchPhaseTimer` (4); `search_embed::{QueryBudget, DIMENSIONS}` (`search_vectors.rs` 12) |
| **V6 Re-export / glob artifact** | 7 | 9 | `semantic_display.rs:22` has `pub use crate::graph_query::{LANGUAGE_NOTE, NO_PRODUCER_NOTE, multiple_producers_note}`, re-exported again at `store.rs:33-37`. That is `store.rs` -> graph_query 3. Six files then carry a 1-ref `super::*` import deviation to `graph_query.rs`: `captured_witness`, `published_admission`, `search`, `search_startup`, `semantic_payload`, `semantic_retention`. Not verified that each of the six would vanish, but none of them names a graph_query item |

Dependency closure checked for the candidates:

- The native record block uses only serde and a local helper.
- `semantic_batch/limits.rs` has no `crate::` references.
- The semantic record types use native record types. `store::id_codec` is used only by validation functions (`semantic_evidence.rs:250`, `254`, `1326`), so the types can go to shared.
- `search_rank.rs` references `store`, `search_vectors`, `search_model` and `native_evidence` in its functions. The closure of the candidate *types* alone was not verified.

### (c) Inversion candidates: pass a trait or closure in

| Site | Refs | Note |
|---|---|---|
| `&Capture` threaded through every `publish_*` | est. 40-45 of `store.rs` -> capture 81 | Storage uses `capture.files` (18), `verify` (8), `artifact_hash` (6), `source_operations` (5), `reconcile_options()` (5), `persisted_inputs()` (4), `binary_bytes` (4), `executable_path()` (3) and a few more. `Capture` is a behaviour-rich indexing object (`verify` re-stats the filesystem); it is not vocabulary. A storage-owned trait of about 15 methods implemented by `Capture` removes the edge |
| `semantic_evidence::validate_evidence(semantic, native, captured)` inside the publication transaction (`store.rs:19007`); `validate_evidence_with_attested_batch` (`oracle.rs:352`) | 2-3 | Closure supplied by the indexing caller. The argument `measured_stream` is computed inside the transaction, so the call cannot simply be hoisted |
| Class catalog composition during publication: `classes::CatalogComposer`, `plan_complete_catalog`, `complete_file_catalog`, `Catalog::compose`, `supports_language` (`store.rs:14858-14874`, `18076-18118`, `18382-18497`) | est. 20 of `store.rs` -> classes 22 | Composition reads prior summaries from the database mid-publication (`load_local_class_extractions`, 18035), so pre-composing above storage is not a pure move. Not verified how separable it is |
| `indexer::project_document` in `Store::project_fetched` (`store.rs:20770`) | 2 | A pure function from native rows to `model` graph nodes; moving it down beside the records is simpler than inverting |
| `search_capture::render_card` in `search::write_cards` (`search.rs:128`) | 1 | Caller could pass rendered cards |
| `recreate_index_exclusive_with_hook` running `Capture::admit` and `spool_full_capture` under the exclusive lock (`store.rs:13651`, `13682`) | 4-6 | The caller (`recreate_pending_writer_session`, `store.rs:14776`, driven from `index_coordinator`) supplies a `rebuild` closure. The lock choreography stays in storage |
| `producer::retention::publish_run(&tx, ...)` inside the batch transaction (`store.rs:15479`) | 1-3 | See (d) for retention |
| Work counters: `semantic_import::note_*`, `publication_work_checkpoint`, `restore_publication_work`, `reset_publication_work`, `semantic_batch::testing::note_*` | est. 40 across `store.rs` and `native_first/**` | Instrumentation called from storage. Moving the counter module down (vocabulary) is cheaper than inverting. Its dependency closure was not verified |
| `scip_admission::validate_selected_role` at enqueue (`requests.rs:1351`, `1388`) | 2 | The enqueue caller could validate first. Tiny |
| `index_coordinator::cooperative_semantic_unit` (`native_first.rs:417`) | 1 | A test fixture (`complete_pending_semantic_for_tests`); move the fixture up |

### (d) The rule is wrong: the file is not what its layer says

These are judged from file content only.

| File | Current layer | What it is | Effect on storage deviations |
|---|---|---|---|
| `src/store/architecture_core.rs`, `sequence_hops.rs`, `graph_read/sequence_callbacks.rs` | storage | Zero SQL; pure composition over query-layer functions. They are query code that sits in the `store/` directory | Covered by Q1/Q2. The honest fix is a physical move out of `src/store/`, not a path exception in `rules.toml`, because the directory name would keep lying |
| `src/semantic_batch/staging.rs` | indexing | "The per-operation stage: a private directory beside `index.db` holding `stage.db`" (its own doc); 44 SQL lines; production dependencies are `scip_raw`, `store` and `semantic_evidence` types. It is SQLite persistence of derived state | Once V2 lands it references only storage and shared. Re-homing it as storage removes `store.rs` -> staging 11 now, and the continuation 9 / witness_preparation 26 / local_targets 7 pairs if those files have not moved up (53 refs, 4 pairs in total) |
| `src/producer/state.rs` | indexing | Producer state directories under the cache; its only crate dependency is `store::topology` | With `retention.rs`, covers P4 (13 refs) |
| `src/producer/retention.rs` | indexing | "SQL-published last successful run per producer ... The index writer attaches this database to its rollback-journal transaction"; 26 SQL lines. Depends on `semantic_batch` (4 mentions, including `GenerationOrigin`), `scip_admission` (1) and `store::requests::GeneratedImportOptions` | Storage by content, but not clean until its `semantic_batch` uses are vocabulary. Not verified what the other 3 are |
| `src/classes.rs`, `src/classes/**` | query | A tree-sitter extractor (`impl FileExtraction` 754, `Extractor` 987) run at capture time (`native_evidence.rs:1475`), plus the persisted `Catalog` record. It is indexing by content | Fixes the `indexing>query` deviation `native_evidence.rs` -> `classes.rs` (3). It does **not** reduce storage's count: `store.rs` -> `classes.rs` 22 just becomes `storage>indexing`. Listed because it describes reality, not because it helps the number |
| `src/skipped_config.rs`, `src/semantic_batch/limits.rs` | indexing | Constants files with no behaviour. `limits.rs` has no crate dependencies | Already counted in V4; remapping or moving are equivalent here |

`src/search_vectors.rs` **is** storage (an SQLite cache with its own writer and reader); its 12 refs are parameter vocabulary (V5). `mcp_read.rs` is view code, but its only upward refs are vocabulary, so it stops deviating without moving.

### Cluster totals

| Class | Pairs | Refs |
|---|---|---|
| (a) query orchestration (Q1 + Q2 + Q3) | 29 | 353 |
| (a) indexing pipeline (P1 + P3 + P4, pair-level) | 41 | 230 |
| (b) vocabulary (V1, V2, V4, V5, V6, pair-level) | 38 | 201 |
| Mixed single pairs: `store.rs` -> capture 81, `store.rs` -> classes 22 (less the 2 est. in Q3), `store.rs` -> indexer 21 (less the 12 est. in V4). Part vocabulary, part P2, part (c) | 3 (shared with rows above) | 115 (rest) |

The est. splits mean rows overlap by a few references; the pair-level assignment is exact.

---

## 3. What a real storage core looks like

**Already pure persistence, no upward references** (about 8,900 lines of `store.rs` and about 5,800 lines of submodules):

- gate and permits (64-559);
- typed errors (649-836, 5551-5794);
- schema (837-1064);
- SQLite file witnesses, open, shape validation (4203-5550);
- `Store` open / initialize / connect / recovery / status (12921-13511, 13809-14658);
- evidence admission (14964-15113);
- retention and GC (19634-20470);
- `topology.rs`, `requests.rs` (except 10 small refs), `symbol_trigrams.rs`, `id_codec.rs`, `anchors.rs`, `native_join_budget.rs`, `published_admission.rs`.

**Pure persistence whose only upward references are record types** (about 5,000 lines of `store.rs` and about 4,500 lines of submodules), which becomes clean after V1-V3:

- native row decoders (11834-12745);
- semantic read and write codecs (7023-10036);
- `write_native` (10037-10677), apart from classes;
- write projection (6157-7022);
- selected fetch and native reads (20471-21325);
- `NativeReader` (1184-1340);
- capture comparison (11204-11833);
- `semantic_payload.rs`, `native_first/witness.rs`, `native_first/copy.rs`, `captured_witness.rs`, `mcp_read.rs`.

So a storage core of roughly **24-25k production lines** exists today, interleaved with about 7k lines of view assembly (11.5k if `semantic_display.rs` and `mcp_read.rs` are counted) and about 4.5k lines of indexing pipeline.

**How callers use `Store` today.** It is the facade:

- Entry (`http`, `mcp`, `daemon`, `main`) holds a `Store`, calls `evidence_response()` and then view-shaped methods, and calls `finish()` to fence (41 entry call sites of `finish`).
- Indexing (`index_coordinator`, `producer/**`, `semantic_batch`, `semantic_import`, `http/worker/semantic.rs`) uses `with_native_reader` (29 call sites), `writer_session`, `publish_*`, and the pipeline methods `prepare_local_revision`, `compose_local_revision`, `reuse_unchanged_native_head`, `selected_capture_unchanged`, `publish_local_native_recovery`, `publish_validated_stream_recovery`. All of those pipeline methods are called only from `index_coordinator.rs` (grep).
- Query calls essentially nothing.

**API the upper layers would need from a real core:**

1. *Snapshot handle* (today's `EvidenceResponse` minus view methods): admit, pin and fence (`validate_pin`, `finish`, `into_fence`, `require_mutation_ready`, `report_index_failure`, `report_optional_corruption`), plus `pub(crate)` access to the read transaction and `ReadRevision`.
2. *Row-level selected reads*, which already exist as private helpers:
   - `read_revision_for`, `selected_graph_document_for`, `fetch_selected_header/rows/raw`;
   - `read_native_declarations_for`, `read_native_calls_v8`, `read_native_control_regions_v8`, `read_native_coverage_for`, `read_native_source`;
   - `v8_native_scope_for`, `read_semantic_tuple*`, `read_semantic_facts`, `read_semantic_call_facts`, `read_semantic_history`;
   - `semantic_display::{select_producers, default_producers, SemanticCalls, document_projections, coverage_summary}`;
   - `architecture_snapshot_regions`, `class_metadata`, `symbol_trigrams::Search`, `search::read_cards` and the lexical queries.
3. *Publication*: `enter_publication`, `publish_native*`, `publish_semantic_batch`, `publish_pending_semantic`, `write_pending`, taking record-typed inputs plus a capture trait and validation closures.
4. *Queue, maintenance, topology*: unchanged.

The step that turns "SQL written in view code" into a deliberate API is the costly part; see step 14.

---

## 4. Target shape and sequenced plan

### Target shape

- **shared**: a new evidence-model module family (suggested `src/evidence/native.rs`, `src/evidence/semantic.rs`) for the persisted records, plus small shared homes for limits, diagnostics, index options and search DTOs. `shared_outbound = "forbid"` means these must have no layer references; checked for the native block and `limits.rs`, not for every item.
- **storage** (`src/store/**`, `src/search_vectors.rs`): topology, gate, connections, schema, row codecs, the publication transaction, the snapshot handle and row-level reads, the queue, retention; plus the stage database and producer retention database if the owner accepts (d).
- **query**: one read module per view (`architecture`, `metrics`, `behavior` / sequence, `graph_query` snapshot source, `navigation`, `class_diagram`), each an `impl EvidenceResponse` or `impl Store` block living in the query layer.
- **indexing**: the S unit (`native_first/{continuation, oracle, witness_preparation, context, local_targets}`), incremental and recovery preparation, and the publish test fixtures.

**The mechanism that keeps the moves behaviour-preserving.** Rust allows inherent `impl` blocks for a type anywhere in the crate. `impl EvidenceResponse { pub fn sequence_at(..) }` can therefore move to a query-layer file with **no call-site change** in `http` / `mcp` / `daemon`. What the move does need:

- (i) `pub(crate)` accessors for the private fields `db`, `store`, `fence`;
- (ii) `pub(crate)` on the private helpers it calls;
- (iii) `#[macro_use]` or `pub(crate) use` for `selected_ensure!`;
- (iv) relocating or re-pointing the tests that reach those methods through `super::*`.

The rules tool resolved `use super::*` re-exports through to the definition file (V6), so I expect it to attribute a relocated method to its new file. This was not verified on a relocated inherent impl; check it on step 6 before relying on it for the rest.

### Steps (each one PR)

Pure moves come first, seam extraction later. "Removes" is pairs / refs from [`baseline-deviations.txt`](baseline-deviations.txt); "~" marks estimates.

| # | Step | Removes | Risk | Size |
|---|---|---|---|---|
| 1 | **Drop the graph_query re-export from store.** Delete `semantic_display.rs:22` and the names at `store.rs:34-36`; callers (including 4 integration-test uses of `store::multiple_producers_note`) import from `graph_query` | 7 pairs / 9 refs (the 6 glob pairs not individually verified) | None beyond import churn | S |
| 2 | **Small vocabulary down.** `skipped_config.rs` and `semantic_batch/limits.rs` to shared (move or remap; both are constants). `evidence_budget::{row_limit, max_row, total, CLASS_EXTRACTION}`, `diagnostic_marker` and its registry, `IndexOptions` / `ReconcileOptions`, `compare_reader_calls`, `GenerationOrigin`, `scip_admission::{hexhash, *_TRUST_ORIGIN, file_uri}`, `BoundaryReason` to shared. Leave `pub use` at the old paths | 10 pairs / 29 refs fully; ~12 of `store.rs` -> indexer 21. Bonus outside storage: `scip_raw.rs` -> limits, `cargo_targets.rs` -> skipped_config, `syntax_bounds.rs` -> evidence_budget, `model.rs` -> graph_query | `IndexOptions::new` uses `CONFIG_INPUT_MAX_BYTES` (moves in the same PR). `file_uri` and `diagnostics()` closures not verified | M |
| 3 | **Native evidence records down.** Move `native_evidence.rs:276-550`, `PRODUCER`, `NATIVE_VERSION`, `LANGUAGES`, `lookup`, `verified_callee_lookup` to the shared evidence module; keep `impl Artifact` validation (3643-4250) and the spool in `native_evidence.rs`; re-export | 4 pairs / 13 refs fully (`architecture_core`, `graph_read`, `mcp_read`, `search`); ~40 of `store.rs` -> native_evidence 64 | serde derives and `deny_unknown_fields` move unchanged, so stored and wire bytes are identical. `lookup` uses the ICU normalizer (fine in shared). `evidence_budget::graph_bytes` and `indexer::project_document` can follow here | L (wide but mechanical) |
| 4 | **Semantic evidence records down.** Record types, `LANGUAGE_ORDER`, `language_order` | 5 pairs / 16 refs fully; ~20 of `store.rs` -> semantic_evidence 22 | The types reference native records (step 3 first). Validation functions stay and keep using `store::id_codec` | M-L |
| 5 | **Search vocabulary down.** `SearchCandidate*`, `SearchRole`, `SearchPolicy`, `CANDIDATE_DEPTH`, `SearchCard`, `PreparedFacts`, `Ancestor`, `is_eligible`, `CapturedCard`, `QueryBudget`, `DIMENSIONS`, `SearchPhaseTimer` | up to 7 pairs / 45 refs (leaves 1 `render_card` call) | Type closures in `search_rank.rs` not verified; `QueryBudget` holds a clock closure (fine in shared) | M |
| 6 | **Architecture and metrics read service up.** Move `store/architecture_core.rs` (501), `store.rs:1946-2930` and `3339-3952`, the metrics DTOs and plumbing (1481-1660, 1740-1937), `EvaluateRules` (1653) into query-layer files as `impl EvidenceResponse` / `impl Store` blocks. Storage keeps `architecture_snapshot_regions` (2941-3338) | Q1: up to 14 pairs / 166 refs. `architecture_snapshot_regions` builds `references::Snapshot` / `DocumentInput` / `ManifestInput` / `RootTsconfigInput`, so ~10 refs in `store.rs` -> references (and the 3 in external / tsconfig) stay until those input DTOs move down or the function returns storage rows | `Store.metrics_memo` (`store.rs:579`, used at 1837, 3563 and `architecture_core.rs:485`) must leave `Store` or become an opaque slot; it is shared across `Store` clones by `Arc`, so keep exactly one memo per checkout. `architecture_apps_tests.rs`, `catalog_summary_tests.rs`, `metrics_parallel_tests.rs` and the 22900-22998 inline tests reach these through `super::*`. No transaction or fence change: the methods keep running inside the response's snapshot | L |
| 7 | **Sequence / graph / navigation up.** Move `graph_read.rs` (1,522), `graph_read/sequence_callbacks.rs` (466), `sequence_hops.rs` (287), and `Store::{sequence_in, navigation_in, query_view_in_for}` plus `EvidenceResponse::graph_query` | 12 pairs / 149 refs | `SnapshotGraphSource` and `Enrichment` contain SQL and use `ReadRevision`, `V8NativeScope`, `v8_native_scope_for`, the `read_native_*_v8` decoders, `read_semantic_call_facts`, `selected_graph_document_for` and `semantic_display::{default_producers, SemanticCalls}`: about 25 helpers to widen. Until step 14 the query layer contains SQL. `sequence_hops.rs` needs step 6 (`ArchitectureCore`). Tests: `sequence_hops_tests.rs`, `sequence_callback_hops_tests.rs` | L |
| 8 | **Class reads up.** `classes_in`, `class_diagram_in` (21497-21659) and the presentation helpers (5936-6156) | 1 pair / 10 refs; 2 of `store.rs` -> classes | Small. `class_metadata` stays as a storage read | S-M |
| 9 | **S unit up.** Move `native_first/{continuation, oracle, witness_preparation, context, local_targets}.rs` (2,260 lines) to an indexing module, and the `complete_pending_semantic_for_tests` fixture with it. Keep `native_first.rs`, `witness.rs`, `copy.rs` in storage | 19 pairs / 121 refs | **Highest-risk move.** `SemanticContinuation` owns `HeldIndexRead` connections registered in `Store.semantic_reads` (`store.rs:614`) so exclusive-use transitions can close them first; `release_semantic_reads` (`continuation.rs:125`) must stay callable from storage. Under CLAUDE.md, never close a separate handle on a live SQLite file. `semantic_unit` (300-830) runs inside writer-session and gate ordering; move bytes only, with no reordering. All five files use `use super::*` over `store` privates, so the visibility surface is large. `local_targets.rs` has 375 lines of inline tests | L |
| 10 | **Incremental / recovery preparation up.** Move `prepare_local_revision`, `compose_local_revision`, `prepare_local_native`, `reuse_unchanged_native_head`, `selected_reusable_native`, `selected_capture_unchanged` (16851-17678, minus the pieces that run inside a storage transaction) and the `publish_*_for_tests` fixtures (15964-16342) to indexing. Invert `recreate_index_exclusive_with_hook`'s capture-and-spool into a caller-supplied closure | ~25-35 refs across `store.rs` -> native_evidence / indexer / capture. Eliminates `store.rs` -> indexer together with step 2 | `recreate` downgrades EX to shared and CASes `recovery_disposition` before rebuilding (`store.rs:13636-13650`); the closure must run at exactly that point. `preflight_*_reuse` read the prior snapshot; verify each moved function's transaction ownership before moving. `rebaseline_fault_tests.rs` (2,663 lines), `preflight_hash_reuse_tests.rs` and `unchanged_trust_tests.rs` depend on these privates. The fixtures are `pub` under `test-support` and used by integration tests | L |
| 11 | **Batch seam.** Move `Descriptor` grammar (`semantic_batch/descriptor.rs`: 735 lines, no SQL) and the work-counter module down. Re-home `semantic_batch/staging.rs`, `producer/state.rs` and `producer/retention.rs` as storage (rule change of type (d): needs owner agreement) | ~8-10 pairs / ~60-70 refs: `store.rs` -> descriptor 16, staging 11, testing 2, most of semantic_import 8; `witness.rs` -> descriptor 8; `copy.rs` -> descriptor 3; `semantic_retention.rs` -> producer/** 13 | `descriptor.rs` depends on `scip_admission::PinnedProducer`, `validate_selected_role` and `producer` (2 mentions), so its closure is not trivially shared; it may have to split into grammar (down) and admission checks (stay). Not verified. `retention.rs`'s 4 `semantic_batch` uses must be vocabulary first | M-L |
| 12 | **Split `semantic_display.rs`.** Keep producer selection, `status_semantics`, `SemanticCalls` in storage; move `view_walk`, `architecture_semantic`, `mcp_language_tiers`, `mcp_coverage_documents`, `mcp_call_semantics` up; move the `GraphSource` trait with `DocumentFacts`, `HistoryFacts`, `DefaultProducers`, `GraphSnapshot`, `SemanticDocument` to shared | 2 pairs / 28 refs | `status_semantics` is on the status read path (`published_admission.rs:176`). `Locate` implements `GraphSource` (177) and the kept half still calls `engine::semantic`, `call_disposition`, `valid_producer_id`, `occurrence_warnings`, so a clean cut needs those functions down or an inversion. **Cost exceeds benefit as a deviation fix** (28 refs for a risky split of a 2,100-line file with a cognitive-115 function). Do it only if step 14 is done | L |
| 13 | **Capture trait and validation closures.** Storage-owned trait implemented by `Capture`; `validate_evidence` passed as a closure; class catalog composition supplied by the caller | ~40-45 of `store.rs` -> capture, ~20 of classes, 2-3 of semantic_evidence | `capture.verify()` call points inside publication are correctness-critical ordering (inputs unchanged before commit); a trait keeps them in place. Class composition reads prior summaries mid-publication. **Cost exceeds benefit for the class part.** The capture trait alone is M and is fair value if zero is wanted | M (capture) / L (classes) |
| 14 | **Narrow the read API.** Replace the widened helpers from steps 6-8 with a deliberate `SelectedSnapshot` row API and remove SQL from query-layer files | 0 refs | Pure design work touching every moved view. **Removes no deviation**; it is what makes the layering true rather than nominal. Sequence it after the numbers are down, or decline it explicitly | XL |

Notes on ordering:

- Steps 1-5 are independent of each other except 4 after 3. They give the largest ratio of references to risk and should go first.
- Step 7 depends on step 6; step 8 is independent.
- Steps 9 and 10 are independent of 6-8. Step 11 is easier after 4 and 9.
- If `semantic_batch/staging.rs` is re-homed (11) *before* step 9, the `witness_preparation.rs` / `continuation.rs` / `local_targets.rs` -> staging pairs (42 refs) disappear without moving those files, which lowers the urgency of the riskiest step. Step 9 would then still remove about 79 refs.

Approximate cumulative effect (estimates):

- after steps 1-5: about 215 refs removed with no behavioural code touched;
- after 6-8: about 540;
- after 9-11: about 780-800;
- 12-13 take it to about 850-870 of 899.

Test-module risk applies to every move out of `store`. The 36 `*_tests.rs` files and the inline modules use `use super::*` against `store` privates. A moved function's tests either move with it (and then need the same `pub(crate)` widening) or keep working through a `pub(crate) use` shim. Per CLAUDE.md, tests that pin only the old location of a private are rewritten, not preserved.

---

## 5. Residual after steps 1-11

| Residual | Refs (est.) | Recommendation |
|---|---|---|
| `store.rs` -> `capture.rs` (`&Capture` in publication) | 40-45 | **Fix** with the capture trait (step 13, capture part), or **except** with the reason "publication verifies the admitted capture at commit". Either is honest; the trait is about one M-sized PR |
| `store.rs` -> `classes.rs` (catalog composition at publication) | ~20 | **Except with a reason** or leave. Reclassifying `classes` to indexing is correct on content but does not remove it. Inversion is L with mid-transaction reads |
| `semantic_display.rs` -> `graph_query.rs` 24, -> `references.rs` 4 | 28 | **Leave** until step 14 justifies step 12 |
| `store.rs` -> `semantic_batch.rs` 14, `native_first.rs` -> `semantic_batch.rs` 10 (methods on `AdmittedBatch` / `JoinedBatch` parameters: `destination_pin`, `captured_pin`, `join_parts`, `descriptor`) | ~24 | **Fix later**: these two types are the publication input of a batch; they could be storage-owned DTOs. Not analysed in depth |
| `store.rs` -> `native_evidence.rs` (`FullDocumentSpool` reads in stream publication; `Artifact::validate*`) | 5-10 | **Except or invert**: the spool is an indexing artifact that storage streams from; a reader trait would do it. Low value |
| `store.rs` -> `semantic_evidence.rs` (`validate_evidence`), `oracle.rs` (moves in step 9) | 1-3 | **Except** ("publication validates semantic evidence against the measured stream inside the transaction") or closure |
| `store.rs` -> `references*` (input DTOs of `architecture_snapshot_regions`) | ~13 | **Fix** in a follow-up to step 6: return storage rows or move the three input structs down |
| `search.rs` -> `search_capture.rs` (`render_card`), `requests.rs` -> `scip_admission.rs` (`validate_selected_role`), `native_first.rs` leftovers (scip_admission 1, semantic_import 2, staged/publication 1), `captured_witness.rs` -> staged/publication 1, `semantic_payload.rs` -> semantic_import 1 | ~9 | **Leave** or fix opportunistically; each is one call |
| `mcp_read.rs` (2,375 lines of MCP view code in storage, no upward refs after steps 3-4) | 0 | **Leave**; it is a facade smell, not a deviation. Moves naturally if step 14 happens |

Rule changes proposed, all on content grounds:

- `semantic_batch/staging.rs`, `producer/state.rs`, `producer/retention.rs` -> storage;
- `classes.rs`, `classes/**` -> indexing;
- `skipped_config.rs`, `semantic_batch/limits.rs` -> shared.

No exception is proposed merely to lower a count.

---

## 6. Tool gaps (Trellis MCP)

1. **`trellis_rules_explain` did not return the full evidence.** With `limit: 50` on `src/store.rs -> src/rules.rs` (`referenceCount: 85`) it returned 5 evidence entries and `omittedEvidence: 0`. The same 5 are in `rules.json`. There was no way to enumerate the other 80 references.
2. **Evidence entries are about 2 KB each and there is no compact mode** (basis hashes, provenance IDs, fact digests repeated per entry). Even if `limit: 50` worked, one pair would cost about 100 KB. A compact form (`caller`, `lines`, `spelling`, `relationKind`, `target.displayKey`) is what this analysis needed.
3. **No per-declaration aggregation of a rule instance.** The question "which methods of `impl Store` reference layer X, and how often" is the core need for a 23k-line file. Evidence is per file pair; I fell back to regex scans binned by method.
4. **`rebuild_in_progress` blocked all reads mid-session.** `trellis_incoming_calls` on `src/store.rs::Store::prepare_local_revision` and `::recreate_index_exclusive` failed with "Index is being rebuilt; retry after it finishes". There was no stale-snapshot fallback; callers were found with grep instead. `trellis_call_graph`, `trellis_outgoing_calls`, `trellis_find_symbols`, `trellis_read_source` and `trellis_references` were therefore not exercised.
5. **`use super::*` produces deviating pairs.** Six files have an `n=1 i1 ['super::*']` deviation to `graph_query.rs` because `store.rs` re-exports three graph_query items. A glob that brings an unused upward name into scope is reported the same as a real import: 6 of the 107 pairs.
6. **Reference counts differ from textual use by a wide margin.** `store.rs -> native_evidence.rs` is `n=64 (c44 i3 r12 t5)`, but the file has about 194 path mentions, e.g. `&crate::native_evidence::Artifact` 33 times in signatures. It is unclear whether type positions are deduplicated, unbound, or not counted; `t5` suggests most type uses are not counted. The documented limitation ("observed mapped references, not absence") does not say which. This makes `n` a poor effort estimator for vocabulary moves.
7. **Calls to methods on a type are attributed to the file holding the `impl`.** That is correct, but it means moving a struct while leaving its `impl` behind will not clear the pair. The explain output does not separate "type defined here" from "method defined here".
8. **`decls.json` lists closures as declarations** with heads such as `|| {` and `|db| {` (e.g. `src/store.rs:18534`, cognitive 131) and no owner name, so the most complex unit in the file cannot be named without opening the source.
