# Rust complexity: diagnosis and decomposition proposals

> Analysis notes behind the [0.2.x refactoring spec](../0.2.x-spec.md), track R. Written by a read-only analysis agent at v0.2.0 (`d84f264a`); "I" is that agent. Rust semantic evidence was possibly stale while this was written; the spec's baseline was re-measured on fresh evidence and did not change. "After" complexity figures are estimates.

Scope: `src/**` at v0.2.0 (main, d84f264a), excluding `src/store.rs` and `src/store/**`. Read-only analysis; nothing was built, run or edited.

How this was produced: five parallel readers each read every scoped function body in full and reported back; I did the metric, churn and threshold analysis and grep-confirmed the three claims that change the plan most (marked "confirmed by grep"). Everything else is the readers' reading of the code. "Not verified" marks what nobody checked. "After" complexity figures are estimates by eye, not measurements.

All paths are relative to the repository root.

---

## 1. Findings that change the plan

1. **The worst `semantic_import.rs` functions are not production code.** `finish_rejoin` (cog 150, 18 params) and `rejoin_with_stores` (cog 81) are reached only from `rejoin_inventory`, whose callers are `src/store/native_first/oracle.rs:338` (`#![cfg(feature = "test-support")]`) and tests (31 calls in `tests/semantic/semantic_rejoin.rs`). Confirmed by grep. They are the independent in-memory oracle for the staged join. Production settlement is `StagedJoin::settle_document`.
2. **About 150 lines of that chain are dead.** `rejoin_inventory_with_reuse` is called only by `rejoin_inventory_with_destination`, which is called only by `rejoin_inventory` and passes `None` for `reusable`. So `reuse_captured_source_associations` (`src/semantic_import.rs:3451-3528`) and the reuse branch at `:3878-3923` never run. Confirmed by grep.
3. **Tests make up most of several "large files".** In-file test modules are 74% of `mcp/projection.rs`, 53% of `scip_raw.rs`, 52% of `daemon/search_fill.rs`, 51% of `mcp/tools.rs` and 35% of `native_evidence.rs`. Moving them out is a pure move and the cheapest file-size win available.
4. **None of the proposed cross-cutting abstractions is real.** A `LanguageCatalog` trait for the four reference catalogs, a stepper trait for the `advance` machines, and a handler table for the dispatchers would each only rename an existing match. The real duplication is smaller and more specific (section 4).
5. **The metric understates functions that contain large closures.** Closures are separate declarations: `dispatch_connection` shows cog 37, but the closures inside it at `src/main.rs:2775` and `:2813` are 42 each; `run_call` shows 75 with a 63 closure at `:1407`. Parent loc includes the closure, parent cognitive does not. Any ratchet should count a closure against its enclosing function, or cap closures separately.
6. **Integration tests mostly exercise a different HTTP dispatcher than production.** `http::router` (the direct single-checkout router) is documented at `src/http.rs:4243` as a test-support fixture, and most `tests/http/*.rs` files build it (confirmed by grep). Production goes through `provisioned_core_answer`, which re-implements much of the same validation. Which arms are covered only through the fixture router is not verified per arm.

---

## 2. Baseline

The brief's counts include the store. Both views, production Rust declarations:

| Threshold | All (12,555) | Non-store (9,576) |
|---|---|---|
| cognitive ≥ 25 / 50 / 75 / 100 | 213 / 64 / 36 / 18 | 169 / 53 / 31 / 15 |
| loc ≥ 100 / 200 / 300 / 500 | 265 / 83 / 37 / 16 | 175 / 51 / 23 / 9 |
| maxNesting ≥ 5 / 6 / 8 / 10 | 159 / 97 / 30 / 15 | 136 / 84 / 29 / 15 |
| parameters ≥ 8 / 9 / 10 | 73 / 27 / 9 | 53 / 17 / 6 |

Churn since 2026-08-01 (the repo's first commit is 2026-09-19; 2,514 commits in total). "Fix" counts commits whose message contains "fix".

| File | Lines | Commits | Last 30 days | "fix" commits |
|---|---|---|---|---|
| `src/http.rs` | 5,760 | 184 | 99 | 88 |
| `src/daemon/registry.rs` | 4,444 | 132 | 100 | 64 |
| `src/main.rs` | 3,615 | 117 | 83 | 55 |
| `src/index_coordinator.rs` | 2,384 | 105 | 51 | 34 |
| `src/native_evidence.rs` | 6,531 | 97 | 74 | 43 |
| `src/mcp/mod.rs` | 3,293 | 91 | 78 | 44 |
| `src/semantic_import.rs` | 5,117 | 72 | 72 | 46 |
| `src/mcp/projection.rs` | 6,138 | 68 | 66 | 34 |
| `src/mcp/tools.rs` | 2,308 | 54 | 47 | 30 |
| `src/references.rs` | 2,400 | 43 | 43 | 32 |
| `src/rules.rs` | 4,210 | 37 | 37 | 26 |
| `src/semantic_evidence.rs` | 3,431 | 37 | 37 | 17 |
| `src/scip_raw.rs` | 4,300 | 33 | 33 | 23 |
| `src/classes.rs` | 2,631 | 26 | 20 | 10 |
| `src/metrics.rs` | 1,239 | 22 | 22 | 11 |
| `src/semantic_import/staged/settle.rs` | 1,253 | 17 | 17 | 13 |
| `src/references/rust.rs` | 2,302 | 15 | 15 | 11 |
| `src/graph_query.rs` | 1,695 | 14 | 14 | 10 |
| `src/references/java.rs` | 940 | 11 | 11 | 10 |
| `src/daemon/search_fill.rs` | 3,054 | 10 | 10 | 6 |
| `src/references/javascript.rs` | 2,775 | 9 | 9 | 6 |
| `src/scip_raw/cargo_targets.rs` | 1,572 | 9 | 9 | 9 |
| `src/references/python.rs` | 2,098 | 8 | 8 | 6 |
| `src/behavior_bindings.rs` | 453 | 7 | 6 | 3 |
| `src/dependency_rust.rs` | 885 | 2 | 0 | 1 |
| `src/semantic_batch/retained_continuation.rs` | 1,215 | 2 | 2 | 2 |
| `src/semantic_import/staged/continuation.rs` | 1,213 | 1 | 1 | 1 |

The three hottest files (`http.rs`, `registry.rs`, `main.rs`) also have the most fix commits. `dependency_rust.rs` has the fourth-worst function in the repo and has not been touched in 30 days.

---

## 3. Per-function diagnosis

For each: why it is complex; decomposition; risk; tests that pin it; size and estimated maximum cognitive complexity afterwards.

### 3.1 MCP

#### `coordinate` — `src/mcp/mod.rs:2479-3107` (165/71/11/629/6)

- **Why.** A whole connection actor in one function: setup, three closures (`fence` :2568, `cancel_dropped` :2593, `deadline_answer` :2603) and an eight-phase loop over seven local collections (`session`, `inbox`, `in_flight`, `checks`, `completed`, `checked`, `arrivals`). The closures take the state as arguments only because no struct owns it.
  - Setup and the `mcp-stdin` reader thread: 2487-2549.
  - A idle wait 2621-2649; B drain `completed` 2666-2723; C drain `checked` 2724-2775; D restart due checks 2776-2794; E fence 2795-2828; F send decisions 2836-2915; G expiry 2916-2934; H admission 2935-3105.
  - The nesting-11 peak is the call branch in H (2955-3064): `for` → `match` → five-way `if/else` → thread spawn → `match spawned`.
  - Duplication: `session.prepare(..).and_then(|p| session.commit(p))` four times (2901, 2928, 3071, 3099); input dispatch twice (2576-2584, 2634-2641); the `Finals::Selected | Finals::Capacity` destructure three times (2731, 2786, 2853-2864; the last rebuilds `Finals` because `SendGate` flattened it to `capacity: bool`); the "Too many concurrent tool calls" message twice (3055, 3060); the `send_decision` hook twice (2719, 2771).
- **Decomposition.** `struct Connection<W: Write>` owning the seven collections plus `workspace`, `sender`, `writer`, `registry`, `daemon_handle`, with one method per phase:
  - `spawn_reader(input, relayed, …)` as a free function (2493-2549).
  - `Connection::take(input) -> bool` replacing both dispatch copies.
  - `wait_idle` (A), `settle_completed` (B), `settle_checked` (C), `restart_due_checks` (D), `fence` and `cancel_dropped` (E).
  - `send_finished` (F) with `gate_decision(call, gate, result, show)` for 2843-2891.
  - `expire` (G).
  - `admit` (H) with `admit_call` (2955-3064), `spawn_worker` (2994-3035), `answer_non_call` (3065-3087).
  - `commit(token, response)`; `Finals::witness()`; `SendGate` keeping its `Finals` (removes 2853-2864).
- **Risk.**
  - Phase order B→C→D→E→F→G→H is Decision 0009 §7 per the comments at 2650 and 2829. Wire order is finished calls, expired (token-sorted), then arrivals.
  - Admission precedence: deadline → validation → per-connection capacity → worker permit (2974-2994).
  - Test synchronization hooks (`phase_hook("launch_final" | "call_send" | "selected_call_send")`, `send_decision`, `immediate_decision`, `fence_handled`, `committed_response`, `worker_done`) must keep their relative positions.
  - `InFlight::drop` sets `cancel` (:1037). A helper that takes a call by value and returns early cancels its worker; 2877 re-inserts deliberately.
  - Two clocks on purpose: `workspace.clock_now()` for deadlines, `Instant::now()` for `FINAL_RECHECK_TICK`.
  - `outcome?` (2672) and `result?` (2725) end the connection on a worker I/O error.
  - No async; the only borrow obstacle is the closures, which the struct removes.
- **Tests.** `tests/mcp/mcp_protocol.rs` (19,540 lines; sole user of most hooks), `tests/mcp/show.rs`, `tests/daemon/daemon_mcp.rs`; in-file `hang_up_tests` (2454), `w1_error_tests` (3211), `settlement_tests` (3250).
- **Size M-L.** After: about 15-18 (`send_finished`, `admit_call`); the loop about 10.

#### `run_call` — `src/mcp/mod.rs:1132-2248` (75/58/5/1117/1), plus the closure at `:1407` (63)

- **Why.** Selection, the describe special case, a nine-arm match that builds a boxed reader closure per tool, and finalization, all inline.
  - Selection 1146-1169; `tools::prepare` 1170-1199; describe branch 1200-1304 (three nested matches); `tool_read` match 1323-2053; `evidence_read` and error mapping 2054-2151; finalization 2152-2190; selection-error arms 2191-2246.
  - Arms: guard 1324; architecture and prompt 1327-1392; search 1393-1687; find 1688-1740; declaration 1741-1780; outgoing 1781-1854; incoming 1855-1928; call_graph 1929-2002; show 2003-2016; read_source 2017-2051.
  - Repeats: the cancel-or-deadline `check` closure six times (1345, 1409, 1698, 1886, 1938, 1966) differing only in the bail message; the budget-wrapped check three times (1455, 1516, 1546); the `lookup_root` preamble four times (1746, 1796, 1865, 1951); `ProducerNotPublished → producer_failure` four times (1823, 1836, 1897, 1910); the `.map_err(anyhow)?.unwrap_or_else(range_too_large)` tail about seven times.
  - The search closure (1407-1686) is its own state machine: pin and basis 1417-1430; policy and head 1431-1444; three ranking sources (resident matrix 1445-1478, cold head 1484-1537, standalone 1538-1577); error fallback 1579-1614; status 1615-1641; filter, hydrate, project 1642-1685.
- **Decomposition.**
  - `struct ReadCx<'a> { id, root, modern, deadline, accepted, cancel }` with `deadline_check(&self, what)`. Every capture is a reference or `Copy`, so each arm becomes `fn find_read<'a>(cx: &'a ReadCx<'a>, args: &'a Map) -> EvidenceToolRead<'a>` in a new `mcp/readers.rs`.
  - `resolve_selector(..)` for the four `lookup_root` preambles; `or_producer_failure` for the four mappings.
  - `select_checkout` (1146-1169), `describe_envelope` (1200-1304, beside `describe` :2265), `finish_envelope` (2152-2175), `capacity_outcome` (2191-2232).
  - Search into `src/mcp/search.rs` (14 lines today): `search_read`, `rank_resident`, `rank_cold_head`, `rank_standalone`, `lexical_fallback`, `search_status`.
- **Risk.**
  - Registry guards are statement-scoped (`registry.blocking_lock().selection_probe()` :1153, `.runtime(key)` :2123). An extraction must not hold a guard across `probe.prepare` or `evidence_read`, which does `block_on(registry.lock())` at :620.
  - `commit_selection` checks `cancel` under the lock (:1066); that orders against disconnect.
  - `resolved` is written at three points (1210, 2090, 2152); the `deadline_exceeded` shape at 2131 depends on whether it is still `None`.
  - `show_target` is reset per read attempt (2007) and taken only after a finished read (2118).
  - In search: `check()` before `budget.check()` (deadline beats budget); `head.fault()` before `declarations`.
  - The `expect("validated …")` calls rely on `tools::check` having run first.
  - Bail messages differ per arm; whether a test asserts them is not verified.
- **Tests.** `tests/mcp/{mcp_protocol,show,apps_endpoints,navigation_resolved,graph_query*}.rs`, `tests/daemon/daemon_mcp.rs`, `tests/mcp-smoke.test.mjs`; in-file `selection_commit_tests` (1073), `src/mcp/find_trigram_tests.rs`. Nothing unit-tests a per-tool reader today.
- **Size L.** After: `run_call` about 12, per-tool readers 5-10, `search_read` and `rank_cold_head` about 15-20.

#### `check` — `src/mcp/tools.rs:414-672` (82/57/5/259)

- **Why.** A flat prelude (415-470: known tool, object, unknown field, missing required, `schemaVersion`, `expectedBasis`, `compact`, `app`) followed by a nine-arm match on the tool name written as negated `is_none_or` chains: describe 472-491; references | rules_check 492-519; endpoints 520-539; explain 540-560; find with `lookupKey` 561-569; find | search 570-588; show 589-616; declaration | outgoing | incoming | call_graph 617-637; default (read_source) 638-666; `workspace` last 668-670. The arms are genuinely different; what repeats is the `limit`/`offset` tail (six times) and re-dispatch on `name` inside merged arms (572, 580, 625, 627).
- **Decomposition.** `struct Reject<'a> { name, args }` with `bad`, `object`, `coded` replacing the closures at 422-445; `common_fields` (457-470); one validator per tool (`describe_args`, `reference_args`, `endpoint_args`, `explain_args`, `find_args`, `search_args`, `show_args`, `selector_args`, `source_args`); `page(o, &r)`. `check` becomes prelude → dispatch → workspace.
- **Risk.** Error precedence is the contract (#769): unknown field before missing (comment at 450) → `schemaVersion` → `expectedBasis` → `compact` → `app` → per-tool order → `workspace` last. `range_too_large` is a distinct code at 660. `prepare` (1066-1079) re-runs validation without `workspace` and relies on workspace being last.
- **Tests.** `tools.rs` `mod tests` (1133-2308, 20 tests, including `invalid_fields_name_themselves_in_static_messages`, `source_path_hash_and_line_matrix`, `show_arguments_are_closed_per_view`), `tests/mcp/mcp_protocol.rs`, `tests/fixtures/mcp/failures-864.txt`, `tests/mcp-catalog-sdk.test.mjs` (not verified).
- **Size S-M.** After: about 10.

### 3.2 HTTP, CLI and daemon

#### `provisioned_core_answer` — `src/http.rs:4819-5718` (141/97/10/900/4)

- **Why.** One match over `BrowserCoreRoute` (30 variants, 21 arms) with every handler inlined. Four arm shapes:
  - Metrics / Architecture / ArchitectureDetails, 4838-5029 (192 lines): a local `CancelOnDrop`, a `spawn_blocking` closure building seven hook closures and a cancel/deadline `check`, then an error ladder (4965-5020).
  - RulesInventory / Preview / Apply, 5030-5216 (187 lines): a second `CancelOnDrop` (5062), a second `check` (5078), and another error ladder (5172-5214).
  - 15 thin read arms of 10-45 lines via `selected_evidence` or `selected_catalog_read` (5262-5423, 5498-5654).
  - Bespoke: `RefreshDependencies` 5217-5261, `Status|StatusHead` 5424-5497, `Source` 5655-5687, `Index|CurrentJob|Job` 5688-5716 (`Job` re-parses `suffix` because the variant carries no id).
  - The `internal_error` `ApiError` literal appears 9 times here and 4 more at 4423-4525. The path check in `Methods` (5581) and `Source` (5657) is identical apart from the error function.
  - Validation is duplicated in the direct-router handlers at 2704-3582 (`dependency_symbols` 2784-2829 versus arm 5291-5322; `methods` 3219 versus arm 5579).
- **Decomposition.** One `async fn` per arm group under `src/http/browser_core/`, leaving a flat match of about 35 lines:
  - `metrics_family_answer`, split into `metrics_family_options` (4841-4855), `run_metrics_family` (4867-4954), `metrics_family_error` (4965-5020).
  - `rules_answer`, split into `parse_rules_request` (5033-5061), `rules_inventory_blocking` (5114-5144), `rules_error` (5172-5214).
  - `refresh_dependencies_answer`, `status_answer`, `source_answer`, `jobs_answer`.
  - Shared: `fn internal_error()`, `struct RequestCancel` (the `CancelOnDrop` plus a checker), `fn valid_index_path()`, validators shared with the direct handlers such as `DependencySymbolsQuery::validate()`.
  - A handler table does not fit: arms differ in input, wrapper, error function, post-step and response type.
- **Risk.**
  - Hook names and order are test-pinned (`before_work`, `before_read_finish`, `after_read_finish`, `metrics_worker_done`, `metrics_serialize_last`, `metrics_cancelled`, `rules_before_rename`, `before_mutation`).
  - The two `check` closures are not identical: metrics fires `metrics_cancelled` on cancel, rules does not. Both test cancel before deadline.
  - `CancelOnDrop` must stay owned by the request future (comment at 4857), with `drop(guard)` after the await.
  - `RefreshDependencies` is a strict sequence and must move as one unit (config snapshot → preflight → disabled → stopped → postflight → `require_mutation_ready` → `start_dependency_index_for` → postflight → `committed` header); it is the only arm in coordination territory.
  - Within each arm: validation, then `browser_scheduler()?`, then evidence. No lock is held across an await.
- **Tests.** `tests/http/{metrics_http,architecture_http,rules_edit_http,files_view_http,browser_selection,dependency_http}.rs` (confirmed by grep); by name only: `browser_route_admission.rs`, `pins_http.rs`, `browse_http.rs`, `navigation_http.rs`, `class_diagram_http.rs`, `rust_sources_http.rs`, `file_tree_http.rs`; in-file `browser_core_route_tests` (5724); `src/http/cli_api.rs:117` calls `provisioned_core`, so `trellis api` tests pin it too.
- **Size M.** After: about 20-25 (`run_metrics_family` or `rules_answer`); dispatcher about 3.

#### `DaemonState::worker_step_inner` — `src/http.rs:1485-1768` (76/40/6/284/1)

- **Why.** A linear nine-phase owner script with early returns and one deep tail:

  | Phase | Lines | What |
  |---|---|---|
  | P0 | 1486-1503 | retired checks, `step_lock`, missing index |
  | P1 | 1508-1518 | root loss, retirement |
  | P2 | 1521-1535 | recreate → `Reconciling`, retry backoff |
  | P3 | 1536-1550 | rebuild reconciliation |
  | P4 | 1551-1569 | prune `pending_requests` |
  | P5 | 1573-1582 | startup reconciliation |
  | P6 | 1596-1656 | FIFO drain (claim closure loop 1608-1637, error triage 1640-1655) |
  | P7 | 1659-1677 | post-drain bookkeeping |
  | P8 | 1678-1764 | watcher install 1689-1717, gate and reconcile 1718-1762 |

  P8 nests five or six deep. About 20 test-support lines are interleaved.
- **Decomposition.** Keep the function as the ordered script (about 70 lines) and extract `prune_pending_requests() -> bool` (P4), `ensure_started(pending_local)` (P5), `drain_fifo_head(session) -> Result<Drain { processed, gate_deferred }>` with `claim_under_watcher_gate` (P6), `note_drain` (P7), `install_watcher` (1689-1717), `reconcile_watcher` (1718-1762). Move to `src/http/worker/step.rs` (`worker.rs` already holds `impl DaemonState` blocks).
- **Risk: high.** This is the single worker owner, the "ratified H, FIFO and watcher order" territory in CLAUDE.md.
  - `step_lock` is held for the whole step.
  - The claim closure holds `watcher_work`, then `explicit_pending`, through the claim's durable COMMIT (comment at 1622-1626). P8 clones `explicit_pending` and releases it (1685) before taking `watcher_work` (1688). Both lock orders must be kept.
  - `publish_watch_status` (1750) runs before `reconciled?` (1751).
  - `gate_error` and `gate_deferred` are `Cell`s shared between the closure and the triage; replace them with a returned value, not a reordering.
  - `test_queue_current_phase` values 1-5 and the `test_queue_*` hooks mark exact points.
- **Tests.** `src/http/exceptional_recovery_tests.rs` (2,859 lines), `src/http/serving_holder_tests.rs`; by name only: `rebuild_recovery_tests.rs`, `recovery_blocked_tests.rs`, `normal_post_capture_cancellation_tests.rs`, `worker/recovery_tests.rs`, `tests/daemon/{index_coordinator,watch,daemon_lifecycle,daemon_crash_recovery,requests}.rs`.
- **Size M in code, L in review.** After: about 18 (the script); `reconcile_watcher` about 12.

#### `dispatch_connection` — `src/main.rs:2342-3144` (37/35/6/803; true complexity higher, see finding 5)

- **Why.** Five connection kinds plus a retry machine in one function:

  | Phase | Lines | What |
  |---|---|---|
  | A | 2351-2394 | ingress read, metrics preface and ack |
  | B | 2395-2408 | `daemon_stop` handoff |
  | C | 2409-2520 | `mcp` attach (111 lines) |
  | D | 2521-2548 | `api` |
  | E | 2549-2559 | `gc_report` |
  | F | 2560-2625 | budgets, progress closure |
  | G | 2626-3093 | the `run` closure (467 lines) |
  | H | 3094-3143 | reply, transfer window, serve hold |

  - Inside G: serve registration 2637-2672, allowlist and marker 2673-2712, activation retry 2716-2737, first-head wait 2741-2748, and the read loop 2775-3092 holding a 13-arm string match (2780-3034) and two retry tails (3040-3063 contention, 3066-3089 `index_not_ready`).
  - The `metrics` arm is 128 lines (2866-2993), about 90 of them test-support env hooks. The function has 25 cfg-test lines and reads 20 `TRELLIS_TEST_*` variables.
  - Per-operation policy is scattered as string sets in about ten places (2372, 2566, 2588, 2593, 2615-2625, 2674-2690, 2705-2708, 2741, 2751-2754, 2780).
  - The Ok/Err → `{"result"}`/`{"error"}` mapping is written four times (2494, 2542, 2553, 3114).
- **Decomposition.**
  - `enum Operation`, parsed once, with policy methods `read_scope()`, `replayable()`, `marker_mode()`, `read_budget(&payload)`, `awaits_first_head()`. All 14 checkout operations fit; this is the one place where a table genuinely fits.
  - `struct ConnCtx { stream, session, registry, browser, search, runtime, browser_shutdown }`.
  - `serve_mcp_connection` with `open_mcp_workspace` (2415-2491); `result_reply(id, Result<Value>)`; `register_serve` (2637-2672); `activate_checkout` (2716-2737).
  - `run_operation(op, &checkout, args, …)` delegating to `cli_architecture_read` (2813-2865), `cli_metrics_read` (2866-2993), `cli_index` (2994-3025).
  - `MetricsTestHooks` under test-support (2868-2966); `read_with_retry` (2775-3092); `send_reply` (3110-3143).
- **Risk.**
  - `answered_deadline` is a `Cell` written inside `run` and read at 3095; have `run` return it.
  - Progress frames go out on a `try_clone`d stream before the reply.
  - `_serve_registration` must outlive `wait_for_serve_disconnect` and cover reply errors.
  - `disconnect(session)` precedes the reply write for non-serve operations.
  - Error text is the wire format (`{error:#}`). `unknown daemon operation` (2673) fires after workspace discovery and after the serve branch.
  - `?` in the `symbols`, `query`, `index`, `semantic_import` arms exits `run` directly, while `metrics` and `rules` errors reach the retry predicates (comment at 2967). Extracting to a `Result`-returning function changes that routing; harmless for parse errors, but check it.
  - Coordination territory: `state_version` is sampled before the read (2779) and passed to `wait_state_change`. The 20 ms sleeps are the documented external-contention exception (3035-3039) and should stay.
- **Tests.** `tests/daemon/{daemon_cli,cli_activation,cli}.rs` (confirmed by grep); `daemon_socket.rs`, `daemon_mcp.rs`, `pre_startup_read.rs` by name only; in-file `daemon_shutdown_tests` (2036), `serve_option_transport_tests` (2246), `metrics_request_tests` (3580) by name and proximity.
- **Size L.** After: about 20 (`read_with_retry` or `cli_metrics_read`).

#### `dispatch_cli` — `src/main.rs:1200-1572` (41/39/6/373/1)

- **Why.** A 14-arm match (1201-1462) builds `(operation, workspace, payload, read_only)`, followed by post-hoc `if let Command::X` patches for payload (1464-1483) and output or exit codes (1491-1570). The `Metrics` arm is 85 lines of validation (1264-1348). The absolute-UTF-8-path closure appears three times (1153, 1403, 1433); pin parsing twice with different messages (1282-1299 and `architecture_cli_arguments` :1587); the hex-digest check twice (1369, 1445).
- **Decomposition.** `struct CliCall { operation, workspace, payload, read_only }` built by `cli_call(command)`; `metrics_cli_payload` (absorbing 1464-1471), `index_cli_payload` (absorbing 1472-1483), `absolute_utf8(path, noun)`, `parse_pin`, `is_lower_hex64`; `report_cli_result(command, &value)` (1491-1570) with `print_metrics_text`.
- **Risk: low.** Folding the `max-file-bytes` check into the builder moves it ahead of `resolve_unattached` and changes error precedence (whether a test pins that is not verified). Error strings and exit conditions are user-visible.
- **Tests.** `tests/daemon/{cli,daemon_cli}.rs`, `tests/producer/*`, `tests/core/architecture_view.rs` (grep hits only).
- **Size S.** After: about 10-12.

#### `drive` — `src/daemon/search_fill.rs:1235-1466` (63/25/13/232/2)

- **Why.** An owner loop in three phases: select-next under the `inbox` mutex with five manual drop/re-lock points (1237-1342; covers matrix expiry, shutdown, startup discovery 1250-1283, promoting due retries 1284-1295, claim 1296-1311, model idle expiry 1312-1322, condvar wait); the attempt inside `catch_unwind` (1349-1392); settle (1397-1464) with four booleans and a six-branch chain whose first three branches are identical. The reader counted nesting of about 7, not 13; the metric probably counts closures and `let … else`.
- **Decomposition.** `struct Job { id, source, cancel, completed, head }`; `Core::next_job(&backend) -> Option<Job>` built from `Core::discover_startup(roots)` (1252-1281, unlocked), `Inbox::promote_due(now)`, `Inbox::claim_next()`, `Inbox::next_retry(now)`; `attempt(&core, &backend, &Job, source) -> Outcome`; `enum Verdict { Reset, RetryFault, FaultExhausted, Done }` with a pure `classify(&Outcome, retries)`; `Core::settle(id, &head, outcome)`.
- **Risk.** This is the owner-and-signal pattern from CLAUDE.md (#777).
  - Never call `expire_idle_matrices`, `finish_idle_expiry` or `head.stop` with `inbox` held.
  - `head.settle()` (1448) must stay inside the inbox critical section, ordered against `notify`'s `head.begin()`.
  - `head.finish` (1396) and `drop(source)` (1395) come before the re-lock.
  - The retry branch's `continue` (1436) skips the dirty requeue at 1460.
  - Guard threading through `Condvar::wait` blocks naive extraction; restructure as "decide under lock, act unlocked".
- **Tests.** In-file `mod tests` (1473-3054, 25 tests), `search_memory_tests.rs`, `search_startup_tests.rs`, `tests/mcp/mcp_protocol.rs`; `search_fill_pipeline_tests.rs` by name only.
- **Size S-M.** After: about 15 (`next_job`); `settle` about 8.

### 3.3 Semantic import and batch

Unqualified line numbers in this subsection refer to `src/semantic_import.rs`.

#### `finish_rejoin` — `:4098` (150/94/11/692/18) — oracle only

- **Why.** Fifteen sequential phases coupled by about 20 local maps and one closure:
  - 4118-4154 captured target and declaration indexes
  - 4155-4200 `retarget` closure (captures `r1`, `r2`, `internal_symbols`, `destination_native`)
  - 4201-4232 symbols; 4233-4244 declaration bindings
  - 4245-4310 calls, pass 1; 4311-4364 reference-claim merge; 4365-4399 final reference resolution
  - 4400-4434 calls, pass 2 (`retain_mut`, error smuggled out through `rejoin_failure`)
  - 4435-4462 relationships; 4463-4529 provenance and freshness; 4530-4571 outcomes
  - 4572-4606 five near-identical provenance-ID rewrite loops
  - 4607-4708 coverage; 4709-4766 language tally, report, 1 MiB bound; 4767-4788 sink emission
  - Parameter groups that travel together: identity (`inventory, captured_native, destination_native, producer, basis`); stores (`r1, r2, raw_index, filtered_index, filtered`; the caller passes `captured_store` as both `r1` and `raw_index`); eligibility result (`exclusions, requested, failed, unsupported_claims, unsupported_documents`); facts (`captured, destination`); `sink`.
- **Decomposition.**
  - `RejoinHeader<'a> { inventory, captured_native, destination_native, producer, basis }`
  - `RejoinStores<'a, F, P, Q> { r1, r2, raw_index, filtered_index, filtered }`
  - `Eligibility<'a> { requested, failed, unsupported_claims, unsupported_documents, exclusions }`, returned by the classification phase of `rejoin_with_stores`. Together these take 18 parameters to about 6.
  - `struct Retargeter` with `fn retarget(&self, &mut Target) -> bool`, replacing the closure.
  - `settle_symbols`, `settle_bindings`, `settle_references -> SettledReferences { rows, accepted_ids, additional_proofs }`, `settle_calls`, `settle_relationships`.
  - `mint_provenance` with `freshness_of(basis, old_doc, dst, header)`; `claim_outcomes`; `rewrite_ids<T: HasProvenanceId>` collapsing the five loops; `coverage_row`; `language_report`; `emit(sink, Settled)`.
- **Risk.**
  - Nothing reaches the sink before the report bound check (4763), so it is all-or-nothing.
  - Sink order is fixed: coverage, provenance, symbols, bindings, references, calls, relationships.
  - `outcomes` is last-write-wins across 4531-4571; that order is an error-precedence contract.
  - `omitted` is filled across several phases and read in coverage.
  - **Oracle independence:** `settle.rs:1-6` says it implements "the rules of `finish_rejoin`", and `settle.rs:888-891` records a deliberate divergence. Sharing settlement kernels between the two would weaken the oracle; share only trivial pure helpers.
- **Tests.** `tests/semantic/semantic_rejoin.rs` (about 30 `rejoin_inventory` calls), `tests/semantic/semantic_batch_store.rs` (memory versus staged parity), `src/semantic_import_complexity_tests.rs`; `tests/semantic/proptest_native_first.rs` via the oracle is not verified.
- **Size M.** After: about 20 (`coverage_row`, `settle_references`).

#### `rejoin_with_stores` — `:3628` (81/38/5/327/9) — oracle only

- **Why.** Captured join and proof sets 3639-3663; document classification 3664-3705; a 160-line per-claim eligibility loop 3706-3869 in which `if reused_source` forks four times; filtered and destination joins 3870-3926 including the dead reuse branch; hand-off 3927-3953.
- **Decomposition.** Delete `reusable`, both `_with_*` wrappers and `reuse_captured_source_associations` first (about 150 lines). Then `CapturedProofs<'a> { declarations, calls, associations }`, `classify_documents(...) -> (requested, failed)`, `claim_verdict(...) -> enum { Eligible, Failed, Unsupported(gap) }`, and `same_measured_call(store, doc, reference_id, call_join, op)` (the same predicate appears at :1967-1981, :2146-2161 and :3842-3857).
- **Risk.** `failed` beats `eligible` (3870). `requested.insert` precedes the document-existence error (3719-3722). `charge(1)` and `lookup_proof` run per claim before anything else. The staged twin `StagedJoin::eligibility_document` (`staged.rs:1805`) was not read.
- **Tests.** Same as `finish_rejoin`.
- **Size S-M.** After: about 25 (`claim_verdict`).

#### `join_occurrence` — `:1829` (90/61/5/384/7) — production, shared by both backends

- **Why.** Six phases: target resolution 1838-1900; declaration binding 1901-1931; invocation witness 1932-1960; role assignment 1961-2048 (seven role rules plus the `role_sites` cache); reference emit and merge 2049-2112; call binding 2114-2210 (nested `if` / `if let` / `filter` closure / `match`). About 20 `reserve_operation` calls are interleaved. `selected` (2119-2126) and `candidate` (2128-2137) compute the same choice twice.
- **Decomposition.** `resolve_targets(asserted) -> Resolved { resolution, resolved, candidates }` (the same 1-versus-many resolution appears at :1888, :2241, :4375 and `settle.rs:679`), `emit_declaration_binding`, `invocation_witness`, `assign_roles(...) -> BTreeSet<Role>`, `cached_role_sites`, `emit_reference`, `classify_dispatch`, `emit_call_binding`. An `OccurrenceCx { encoding, store, index, out, operation }` struct is optional at 7 parameters.
- **Risk.** Each reservation precedes its allocation; the order decides where `LimitExceeded` fires and the measured high-water. The `out.join` call order (1902, 1932, 2117, 2138) is the output order. The `role_sites` cache owns a separate `S::Operation` that outlives the call. `resolved` and `candidates` are moved into `CallBinding` at the end. The `#[cfg(test)] complexity_work` counter at 2096 is asserted.
- **Tests.** `tests/semantic/{semantic_import,semantic_batch_store,semantic_generated_import,java_semantic_import}.rs` (about 50 `join_inventory` calls in the first), `src/semantic_import_complexity_tests.rs`.
- **Size M.** After: about 20.

#### `emit_type_relationships` — `:2436` (93/44/5/352/3) — production

- **Why.** One per-document loop holding four sub-pipelines: parse direct bases 2444-2504 (four-language match); extends/implements 2505-2580; Java overrides 2582-2713 (12 early `continue`s, three diagnostic sites); Java implicit `Enum` extends 2714-2783.
- **Decomposition.** `document_bases(store, doc, bytes, op) -> Option<Vec<DirectBase>>`, `emit_base_relationships`, `emit_java_overrides`, `emit_java_enum_extends`, `owner_types_of` (duplicates `method_target_owner_matches`, 2317-2338 versus 2619-2634), `external_if_unclaimed` (repeated at 2527-2541, 2679-2698, 1868-1882).
- **Risk.** `relationship_operation` holds the reservations for `bases`, `java_bases` and `document_relationships` and must live for the whole document. `take_parsed_tree(doc)` is consuming. Emission order of `out.type_relationship` is the output order. Whether diagnostics are first-wins per document is not verified.
- **Tests.** `tests/semantic/{java_semantic_import,semantic_import}.rs`, in-file `typescript_tests`, complexity tests (`method_lookup_rows`).
- **Size M.** After: about 25 (overrides).

#### `settle_document` — `src/semantic_import/staged/settle.rs:339` (115/63/6/655/6) — production

- **Why.** Twelve phases: preparation 347-405 (witnessed versus measured versus prepared); state and two closures 413-451; captured targets 453-486; symbols 488-528; bindings 530-559; reference merge 561-662; final references 663-747; calls 749-830; call-site union 831-886; call emit 887-924; relationships 926-964; fused proofs, provenance, tally 966-992. One block repeats five times (499-524, 538-558, 702-736, 892-923, 939-960): raw row, `source()`, `id_for`, set ID, `edit_row` hook, `check_row` with 8 arguments, `emit_fact`. The closures `source` and `set_outcome` take `&mut` maps explicitly, the sign of a missing struct.
- **Decomposition.** `Settlement<'a> { view, index, owner, memo, sources, outcomes, outcome }` with `source`, `set_outcome` and a generic `publish<T>(raw, kind, anchor, witness, row, check, table, family)` for the five-times block; then `prepare_joins(...) -> PreparedJoins { captured, separate, _guards }`, `captured_targets`, `merge_reference_claims`, `emit_references`, `settle_calls`, `unite_call_sites`, `emit_calls`, `emit_relationships`.
- **Risk: highest of the set.** `Reservation` drop order is the contract (`settle.rs:105-107`); `_keys`, `_source_reservation` and `_destination_reservation` live to function end. Moving code into helpers moves drop points, which changes the live high-water (`join_reservation_high_water_bytes`) and where `LimitExceeded` fires. The `next_ord` sequence defines byte-identical `out_*` rows. The `WitnessFallback("fused_reference_target_lost")` check must stay after all `omitted` writes. `check_cancel` runs only in the symbols loop.
- **Tests.** In-file tests at `settle.rs:1121` and `:1191`, `continuation.rs:894` `yielded_join_matches_full_and_reopens_every_snapshot`, `tests/semantic/semantic_batch_store.rs`; by name only: `proptest_witness_rejoin.rs`, `proptest_native_first.rs`, `semantic_native_publication.rs`.
- **Size M-L.** After: about 25 (reference merge).

#### `JoinContinuation::unit` — `src/semantic_import/staged/continuation.rs:263` (156/74/7/424/3)

- **Why.** A match over 20 phases with every arm inline:

  | Phase | Lines |
  |---|---|
  | Start | 268-284 |
  | Reset | 285-312 |
  | ClearDestinationInventory | 313-323 |
  | Seed | 324-337 |
  | Captured/DestinationInventory | 338-400 |
  | Captured/DestinationDiscovery | 401-428 |
  | ClaimInventory | 429-455 |
  | Eligibility | 456-471 |
  | Destination/CapturedTargets | 472-502 |
  | WitnessTargets | 503-534 |
  | Settlement | 535-608 |
  | Closure | 609-618 |
  | OutputClosure | 619-644 |
  | OutputRows | 645-670 |
  | Report | 671-682 |

  `Settlement` hides a sub-phase (manifest hashing while `manifest_hash.is_empty()`, with a manual `self.last = -1` at 561). The next-phase expression is duplicated at 346-354 and 387-395; the "next `sources` row" query at 430-437 and 473-480.
- **Decomposition.** `unit` becomes a dispatcher over `step_reset`, `step_inventory(rev)`, `step_discovery(captured)`, `step_targets`, `step_witness_targets`, `step_manifest` (a new `Phase::Manifest`), `step_settlement`, `step_output_rows` and so on, each returning `Result<Option<JoinedBatch>>`; helpers `after_inventory(shared, rev) -> Phase` and `next_source(conn, last)`. No borrow obstacle: state is on `self` and `join` is `&`.
- **Risk.** One unit is one `in_transaction` (:224). Cursors advance only after the work; an error poisons the continuation (`Phase::Failed`). `transition()` also resets `inventory_cursor` (pinned at :873). The early `return Ok(None)` at 566 must stay. Hooks and counters sit at fixed positions (`run_before_closure_hook` at 605, `note_destination_*` at 367 and 383). Page size 128 is part of the bounded-unit contract.
- **Tests.** In-file at :840, :873, :894; `tests/retained_semantic_reimport.rs` and `proptest_native_first.rs` by name only.
- **Size M, mechanical.** After: about 18.

#### `RetainedStageContinuation::advance_unit` — `src/semantic_batch/retained_continuation.rs:256` (125/62/6/545/5)

- **Why.** Fifteen phases inline on a struct with about 50 flat fields that are really per-phase cursors:

  | Phase | Lines |
  |---|---|
  | Inventory | 292-327 |
  | Sources | 328-401 |
  | Cargo | 402-413 |
  | Coverage | 414-489 |
  | Assignment | 490-545 |
  | Exclusions | 546-561 |
  | Artifact | 562-592 |
  | Copy | 593-627 |
  | Decode | 628-653 |
  | Proofs | 654-703 |
  | Documents | 704-728 |
  | ProofInventory | 729-743 |
  | DocumentInventory | 744-759 |
  | Verify | 760-770 |
  | Done | 771-782 |
  | epilogue (budget, counters) | 784-799 |

- **Decomposition.** Extraction is blocked by borrows: the prologue holds `descriptor` (`&self.descriptor`), `conn` (`&mut self.stage`) and `passes` (`&self.producer`, `limits`, `module_roots`, `native_config`) while arms mutate other fields, so `&mut self` step methods cannot compile. Split the struct first:
  - `InventoryCursor { artifact, key, count }`
  - `SourceCursor { order, cursor, count, hasher, cargo_builder }`
  - `CoverageCursor { cursor, covered, excluded }`
  - `MissingAssignment { source, artifact, proof, found }`
  - `ArtifactCursor { artifact, capture_id, copy_offset, copy_hash, decoder, proof_cursor, document_cursor, proof_key, document_key, proof_count, document_count, retained_count, proof_documents }` (the reset at 568-576 becomes one assignment)
  - `StepEnv<'a> { db, reader, conn, descriptor, passes, dir, batch_id, trusted, witnessed }`
  - Steps then become free functions such as `fn coverage_step(cur, env) -> Result<Flow>`.
- **Risk.** The early `return Ok(None)` at 465 skips the epilogue (no budget check, no `retained_admission_units += 1`); decide deliberately whether to keep that. The `ensure!` message strings and the typed `WitnessFallback` are probably asserted (not verified). `payload` flows from Proofs to the epilogue. `total_changes()` is sampled before and after. The inventory arm reassigns `self.native_config` and `self.module_roots` (318-320) while `passes` borrows them, which compiles only because `passes` is not used afterwards on that path.
- **Tests.** In-file at :1073, :1089, :1148; `tests/retained_semantic_reimport.rs` and `tests/semantic/proptest_retention.rs` by name only.
- **Size L.** After: about 20 (Coverage).

### 3.4 References, behavior and dependencies

Tests for all the references functions: `tests/index/references.rs` (200 tests, all languages), `tests/index/proptest_{python,java}_absence.rs`, `tests/index/proptest_absence.rs`, `tests/index/typescript_mixed.rs`, `tests/core/{python_source_roots,js_workspaces,rust_workspace_members,architecture_external}.rs`, `tests/http/java_python_http.rs`, and the in-file modules.

Cross-cutting risk: `owner_step`/`charge` call counts are the bounded-work budget. No test pins absolute totals for the constructors, but serial-versus-parallel equality and several "limit = used + k" tests are pinned. Extractions should keep step counts identical, and any deduplication (JS double `target()`, Rust double `scope_imports`) should be a separate, flagged change.

#### `references/python.rs:1457` `scan` (136/63/6/225/8)

- **Why.** One explicit-stack walk carrying a 5-tuple `(node, module_scope, conditional, decorated, depth)`, with eight sequential `if kind ==` blocks instead of a match: imports 1479-1510; type alias 1514-1533; def/class 1534-1554; assignment with `__all__` 1555-1596; augmented/walrus 1597-1606; binding statements 1607-1627; dynamic-call hazards 1628-1649; child push 1650-1678.
- **Decomposition.** `struct Scanner<'a> { doc, document, native, unit, selected }` with `work`/`check` passed through, and `struct Frame`. Methods: `on_import`, `on_type_alias`, `on_definition`, `on_assignment` (containing `record_all`), `on_binding_statement` (merges 1597-1627), `on_call_hazard`, `child_flags(kind, frame) -> Frame`.
- **Risk.** The blocks are not mutually exclusive (`except_clause` binds and is also `controlled`; `assignment` binds and makes its children conditional), so "continue versus fall through to push children" must be kept per kind. `Imported(index)` relies on `unit.imports.len()` before the push (1492/1507). `__all__` is first-write-wins on `All::Absent`. `check()` and `owner_step` counts feed the budget. Over-bound nodes `continue` at 1472 without setting `unit.dynamic`; whether `syntax_bounds::scan` in `references.rs:583` covers that is not verified.
- **Size M.** After: about 20 (`on_assignment`).

#### `references/python.rs:92` `new` (100)

- **Why.** Five passes: per-doc inventory 117-177; membership proof 178-204; child names 205-219; `check_followed_bodies` :222; star precompute 223-318 (three sub-loops over `unit.imports`, coupled by `star_index` and `contributors`).
- **Decomposition.** `index_document`, `prove_membership`, `index_child_names`, `resolve_unit_imports(document) -> Vec<((usize, usize, usize), SiteResolution)>`, `star_resolution`.
- **Risk.** Passes 2, 3 and 5 iterate `&result.*` while calling `&self` methods (`layout`, `binding`, `import_exports`, which use the `export_memo` `RefCell`) and then insert into another field. As `&mut self` helpers they must collect then insert; inserts are keyed `BTreeMap`s, so order does not matter. The `contributors` zip with `filter(|i| i.star)` is positional. Later residual assignments overwrite earlier ones (:311 overrides everything).
- **Size M.** After: about 22.

#### `references/java.rs:112` `new` (104)

- **Why.** Three passes: package exports 125-192, duplicate marking 193-199, per-selected unit 200-296 (top-level children 211-246, blockers and `static_unknown` 247-260, on-demand merge 262-291, publish 292-295). Nesting peaks at the import `if let` ladder 214-237.
- **Decomposition.** `index_package_exports`, `export_bucket(child, rows, document)`, `mark_duplicate_types`, `build_unit` (calling `Unit::record_import` and `merge_on_demand(&self, &mut Unit, ..)`).
- **Risk.** `package` is a live `&mut` into `result.packages` across 146-191, so the helper must take `&mut Package`. `syntax_bounds::scan(root, check)` sits inside an OR chain that short-circuits on `unit.cut`, which changes the `check()` count. Keep the single `owner_step` calls one for one.
- **Size S-M.** After: about 18.

#### `references/javascript.rs:94` `new` (82, 10 params)

- **Why.** Four passes: tsconfig 107-119, inventory 131-161, parallel hazards 165-187, star precompute 192-301 (with package fallback 263-295).
- **Decomposition.** `parse_root_tsconfig`, `Resolver::index_units`, `Resolver::compute_hazards(selected, workers, request)`, `resolve_document_sources`, `star_resolution`, `package_fallback_target`.
- **Risk.** Hazard batches each run against `work.remaining()` and admit in document order (170-186), pinned by `serial_and_parallel_keep_payload_and_exact_work_counts`. `resolver.target()` is called twice for non-star sources (253, 267) and charges twice; deduplicating changes work totals. `stars.next().expect` is positional.
- **Size M.** After: about 20.

#### `references/rust.rs:201` `new` (76)

- **Why.** Eight flat passes: declarations and file scopes 234-281, `scan` 283-293, parse-error opacity 295-300, edge-to-child resolution 301-329, parents and ambiguity 330-343, externs and root reachability 345-388, `keep_lost_owners_unknown` 389, scope indexes 390-408. The `crate_root` closure (:223) is shared by passes 1 and 6.
- **Decomposition.** `crate_root(workspace, doc)` as a free function, `index_files`, `link_module_edges`, `assign_parents`, `bind_externs`, `mark_roots`, `build_scope_indexes`.
- **Risk.** Pass order is semantic: opacity must follow scan, and roots need both edges and a single explicit binding. `result.files.clone()` at 283 and 345 exists only to satisfy the borrow checker. `mark_roots` rescans all `edges` per visited scope; a per-scope edge index would be simpler but changes `step` counts. In-file `catalog_steps_use_the_injected_request_limit` pins `used == 2`.
- **Size S (mechanical).** After: about 15.

#### `references/rust.rs:523` `scan` (79, nesting 11)

- **Why.** A recursive per-child loop with attribute state carried across siblings (`uncertain`, `scope_uncertain`, `custom_path`): attribute classification 551-590, extern crate 595-612, `use` 613-636, macro opacity 637-642, item binding 646-699, `mod` recursion and edge 700-728, body recursion 730-749. The visibility triple is computed twice (614-616, 679-683).
- **Decomposition.** `enum AttributeEffect { ScopeUncertain, ItemUncertain, CustomPath, Opaque { inner: bool }, Benign }` from `classify_attribute(&str)`; `struct Visibility { public, crate_only, restricted }` from `item_visibility`; `item_namespaces(child) -> &'static [u8]`; `bind_extern_crate`, `bind_use`, `bind_item`, `enter_mod`, `descend`.
- **Risk.** The attribute flags reset only when a non-attribute item is reached (591-594) and `scope_uncertain` is sticky; that state must stay in the loop. Recursion is depth-bounded, with opacity set at `DEPTH`. `struct_item` is in both the binding list and the `scan_bodies` list. A `mod_item` without a name falls through to `self.scan`. `self.step` counts matter.
- **Size M.** After: about 18.

#### `references/rust.rs:1602` `resolve` (83)

- **Why.** Five stages: locate scope and import 1611-1630, external shortcut 1631-1655, star-import competition 1668-1736, named path 1737-1745, publish 1746-1777.
- **Decomposition.** `find_import_at`, `record_external`, `star_names(scope, &import, ..) -> (Names, bool)` containing `competing_star_names` and `name_is_explicit`, `publish_names`.
- **Risk.** `&mut self` throughout (memo and work), so helpers must be methods taking owned or cloned imports. `route` is shared across `expand` and `path` calls and accumulates; do not create a fresh one per helper. Residual precedence is unknown set, then ambiguous overwrite, then empty fallback. `scope_imports` (1618, 1670) and `parts()` (1630, 1741) are each called twice.
- **Size M.** After: about 20.

#### `references.rs:497` `resolve` (89/44/8/428/5)

- **Why.** An eight-stage pipeline: admission and byte budgets 505-578; parse 580-587; partial and selected lists 589-611; catalogs 613-692; semantic declared set 695-715; site loop 716-890; tree drop 892-901; assemble 902-923. The site loop is where the complexity sits: structural dispatch 759-796, semantic/structural merge 797-827, target attach 833-855, coverage and edge tallies 856-886.
- **Decomposition.** `admit_snapshot(..) -> Admitted { docs, sorted, identities, modules, document_map, total_bytes }`, `build_catalogs(..) -> Catalogs { rust, packages, java, python, javascript, work }`, `Catalogs::structural(&mut self, doc, source, raw, &mut found.packages, ..)`, `apply_semantic(&mut site, anchor)`, `attach_target`, `SourceReferences::record(site, resolution, sample_limit)`, `drop_trees`.
- **Risk.** The shared budget is `rust.work`: moved into the Rust catalog then lent to java/python/js through disjoint field borrows, so `Catalogs` must own a separate `work` or it will not compile. Error precedence is `invalid_request`, `store_unavailable`, `bounded_work`, in document-key order. The `site_count` limit is checked after the two `continue`s. Sample order is site order; batch admission is in source order.
- **Tests (additional).** In-file `parallel_tests` (8), `classification_tests`, `historical_projection_tests`; `tests/core/rules.rs`, `tests/http/architecture_precompute.rs`, `tests/semantic/semantic_batch_store.rs`.
- **Size L.** After: about 20.

#### `behavior_bindings.rs:73` `new` (124/51/8/209/2)

- **Why.** One bounded walk: four fail-closed prechecks 89-109, a 12-arm `match n.kind()` 110-257, function handling 258-277. The import arm alone (111-176) reaches nesting 8, and `child_by_field_name("left")` → `write` is repeated four times.
- **Decomposition.** `hazard(n, source) -> bool`, `bind_import(n, tree)` with `bind_import_clause(clause, module)`, `bind_named_declaration`, `bind_for_in`, `bind_function`, one merged write arm.
- **Risk: low.** Outputs are a `Vec<Binding>` consumed by `min_by_key` and a `HashSet`, so push order is not observable. The `DEPTH`/`VISITS` check must stay first. The escaped-module path (115-123) still records shadowing.
- **Tests.** `tests/core/behavior.rs` (25 tests), in-file (2), `tests/browser/sequence.test.cjs` (not verified).
- **Size S.** After: about 12.

#### `dependency_rust.rs:169` `build` (152/72/11/425/4)

- **Why.** Seven sequential phases with no helpers: manifest and config 175-240; stdlib candidates 241-269; lock and registry dirs 270-296; lock packages 297-433 (registry cache match 365-422); direct and path dependencies 434-534; sort and index 535-575; catalog id 576-592. The "limit reached (1024)" warning is duplicated at 304-308 and 437-441, and the `[lib] name` override at 399-410 and 506-517.
- **Decomposition.** `struct Discovery<'a> { catalog, budget: MetadataBudget, cancel, workspace, cargo_home, blocked_config, direct, candidates }` with `read_root_manifest`, `stdlib_candidates`, `registry_dirs`, `lock_candidate(entry) -> Option<Candidate>`, `locate_in_registry(name, version) -> RegistryMatch`, `unlocked_candidate(dep)`, `path_candidate(dep)`, `lib_name_override(&Value, &mut Package, &mut String)`, `catalog_id`.
- **Risk: highest in this group.** `catalog.id` hashes `serde_json::to_vec(&catalog.packages)`, which includes each package's `warnings` in push order, plus sorted fingerprints. So warning text and order per package, and candidate sort order, are identity-bearing. Catalog-level warning order is user-visible. `MetadataBudget` exhaustion is sticky and checked inside the registry loop. The `matches.len() == 2` early break affects read counts and fingerprints.
- **Tests.** `tests/core/dependencies.rs` (19), `tests/core/dependency_declarations.rs`, `tests/core/dependency_links.rs`, `tests/http/dependency_http.rs`, in-file (2).
- **Size L.** After: about 20.

#### `dependency_rust.rs:596` `index_package` (114/41/6/205/9)

- **Why.** Directory walk × pagination loop × entry loop × extraction match, with budget checks repeated at four points (616-636, 676-680, 693-696, 778-787).
- **Decomposition.** `struct Totals { files, bytes, symbol_bytes }` replaces three `&mut usize` parameters; `struct PackageWalk { files, bytes, visited, symbols }`; `budget_stop(&self, ..) -> Option<&'static str>`; `enum EntryAction { Skip, Queue, Index }` from `classify_entry`; `index_file(..) -> Result<ControlFlow<()>>`; `admit_declarations(..) -> ControlFlow<()>`.
- **Risk.** Three distinct exits must be preserved: `return Ok(())` (stop the package), `break` (this directory only, :644), `continue`. Which "Partial:" string fires at which check is pinned. Type-first truncation at 738-744 is a stable sort. `files` is incremented before the read.
- **Tests.** In-file `exhausted_declaration_budget_marks_package_partial_without_storing_text` calls `index_package` directly with all nine arguments, so it must be rewritten. Also `tests/core/dependencies.rs`.
- **Size M.** After: about 15.

### 3.5 Validation, SCIP, metrics, graph and rules

All eight get a refactor verdict; `wire` gets leaf extractions only.

#### `Artifact::validate_structure` — `src/native_evidence.rs:3685-4247` (160/124/3/563/2)

- **Why.** A flat sequence of about 12 independent validation passes sharing five locally built indexes (`docs`, `proofs`, `declarations`, `regions`, `candidates`). Nesting is only 3; the score is `ensure!` volume plus duplication.
  - 3686-3717 header; 3718-3748 documents; 3749-3782 coverage; 3783-3809 provenance.
  - 3816-3907 declarations, with chunked parallel ID minting (`MINT_CHUNK`, `map_ordered`).
  - 3909-3993 four near-identical ordinal blocks (siblings, calls, references, control regions): group, sort by `(late, start, end)`, check ordinal, check duplicate range.
  - 3996-4027 occurrence-minting closures; 4028-4070 calls; 4071-4101 references; 4102-4131 gap candidates; 4132-4186 capability reports (nesting peak); 4187-4216 control regions; 4217-4231 parents; 4232-4245 call-to-region links.
- **Decomposition.** `struct StructureCheck<'a> { artifact, docs, proofs, declarations, contexts, expansions, workers }` with one method per section: `check_header`, `index_documents`, `check_coverage`, `index_proofs`, `check_declarations`, `check_calls`, `check_references`, `measured_candidates`, `check_capability_reports`, `check_control_regions`, `check_region_links`. One generic `check_ordinals<T>(groups, late, ordinal_of, range_of, mismatch_msg, duplicate_msg)` replaces 3919-3993 (about 75 lines become about 20 plus four call sites). The closures `late`, `occurrence_of`, `call_occurrence`, `region_occurrence` become methods.
- **Risk.** The comment at 3810-3815 says errors and their order are unchanged from the pre-parallel version; keep section order as a stability rule. `occurrence: IdentityRegistry` is stateful and admitted in the order calls, references, regions. Closures passed to `map_ordered` must stay `Sync`, so the context struct must be shareable by `&`. The `minted.next().flatten().unwrap_or_else(...)` fallback idiom repeats three times; keep its semantics.
- **Tests.** `tests/index/native_evidence.rs` (`native_validation_rejects_invalid_structure_against_immutable_capture` :623, `..._cross_document_proof_for_every_fact_family` :942, `native_build_is_deterministic` :1877; in 623-1050 only `is_err()` assertions were seen, not message matches). `tests/index/native_store.rs` :4563 and :5349 match substrings that look store-side (not verified). In-file `occurrence_identity_tests` and `parallel_extraction_tests` were not read.
- **Size M.** After: about 20-25 (`check_declarations` or `check_capability_reports`).

#### `ScipDecodeContinuation::advance` — `src/scip_raw.rs:1651-1941` (96/50/6/291/7)

- **Why.** Not an explicit state machine; it consumes one top-level unit per call with four implicit paths: end of input 1661-1688; unit header, budget, read, `wire` walk 1689-1735; metadata merge 1737-1741 and external symbol 1742-1756; document 1757-1937 (180 lines, nearly all the score). The document path decides the cap (1759-1765), then takes one of two parse paths (over-cap pre-parse 1766-1794, or normal parse plus `range_problem` 1795-1835), each repeating "text if read, `cap_document`, reparse"; then binding checks 1836-1867, occurrence loop 1868-1898, symbol-and-relationship loop 1899-1934. `from_utf8(&source.bytes)` under `unread` appears three times (1772, 1811, 1860); the `traversed`/1024 cancel tick five times, plus twice in `cap_document` (1507, 1527).
- **Decomposition.** `finish(&self, artifact) -> Result<ArtifactSummary>` (1661-1688); `read_unit(..) -> Result<(u64, UnitBuffer, WireBudget)>` (1689-1735); `external_symbol` (1742-1756); `admit_document(..) -> (types::Document, DocumentSource, Option<DocumentCap>)` (1759-1835) with a shared `exclude(..) -> Result<DocumentCap>`; `emit_document` (1836-1936) split into `emit_occurrences` and `emit_symbols`; free helpers `source_text(&DocumentSource)` and `tick(&mut usize, cancel)`.
- **Risk.** `UnitBuffer<'c>` holds `&mut BatchCounters` for the whole body and has a `Drop` impl (1206); helpers must take `&mut UnitBuffer`. Budget write-back happens before `walked?` (1733-1735) so counts persist on failure, while `self.relationships` is written only on success (1939); pinned by `continuation_tests::cursor_retains_global_wire_budget_and_cancellation_does_not_advance`. `sink.begin_document` (1840) precedes the source-binding error (1846). First-failure strings are asserted widely. The 1024-traversal cancel cadence must stay.
- **Tests.** In-file `tests` (52 tests in the file) and `continuation_tests` (4147-4300), `src/scip_raw/stream_tests.rs` (12), `tests/scip_decoding_properties.rs` (11), `tests/semantic/{semantic_import,semantic_batch_queue,semantic_batch_store,semantic_generated_import}.rs`.
- **Size M.** After: about 20-25.

#### `wire` — `src/scip_raw.rs:444-559` (88/45/5/116/5)

- **Why.** A hand-written recursive protobuf walker, table-driven by `field(node, n)` (:338), with special cases interleaved in one loop: per-field budget and cancel 458-464; per-document cap overrides 472-483; occurrence range-tag bitmask 488-501; relationship, candidate and occurrence counters 502-520; repeat counts 521-527; the packed inner loop 537-552, which duplicates the budget and cancel logic.
- **Decomposition: leaf extractions only.** `range_tags(n, &mut u8)` (488-501), `WireBudget::charge(&mut self, flag, name)` (458-464 and 541-551), `WireBudget::count_message(node, n, depth)` (502-520), `field_cap(node, n, max, limits)` (472-483), `packed(...)` (537-552). Keep the loop linear; do not split by node kind.
- **Risk.** It runs over every byte of every artifact, so helpers must inline. Error precedence is unknown field, wire type, range tags, limits. Limit names differ by site ("wire fields" versus "packed values") and are asserted in `stream_tests.rs` and `tests/semantic/semantic_batch_queue.rs`.
- **Size S.** After: about 40-45. It stays above any reasonable cap and belongs on an allowlist.

#### `CargoTargetsContinuation::advance` — `src/scip_raw/cargo_targets.rs:589-782` (99/46/6/194/1)

- **Why.** A real state machine: `CargoPhase` (:343) with 7 states in one match: `Discover` 591-630; `Start` 631-652; `Vertex` 653-739 with two hidden sub-states (edge in progress 654-720, itself with a `preflight` mode, and pop pending 721-738); `Associate` 740-746; `ClearSeen` 747-752; `Output` 753-778; `Done` 779. The other source is unnamed tuple cursors: `edge: Option<(String, bool, bool, usize, bool)>`, `pending: Vec<(String, bool, bool)>`, `check: Option<(String, usize, usize)>`.
- **Decomposition.** One method per phase: `step_discover`, `step_start`, `step_vertex` (dispatching to `step_edge(EdgeCursor)` and `step_pending`), `step_associate`, `step_clear_seen`, `step_output`. Named structs `EdgeCursor { file, mod_rs, included, index, preflight }`, `PendingFile`, `OwnerCheck`. A pure `module_candidates(file, mod_rs, &ModRef)` (671-703), possibly shareable with `reach` (:230; not verified). `TargetDiscovery::advance` (449-564) is a second, flag-encoded machine that could get the same treatment.
- **Risk.** Yield granularity is the contract: `tests::cargo_finish_yields_per_vertex_and_module_edge` (:1184) reads the private `cursor.seen` and `cursor.edge` and asserts `vertex_units == 3`. Results must equal `finish_oracle` (:161). `&self.manifests[self.package]` is held while `self.discovery` and `self.poisoned` are mutated, which works inline through disjoint fields but breaks under `&mut self` helpers; pass fields or the index.
- **Tests.** In-file `tests` (1149-1572).
- **Size S-M.** After: about 20-25 (`step_edge`).

#### `score` — `src/metrics.rs:633-957` (131/76/4/325/8)

- **Why.** An explicit-stack tree-sitter walk with one long body classifying node kinds by string: pruning 660-679; string-literal leaf 680-702; named nodes 703-826 (structural kinds 704-736, elif/else 737-746, case arms 747-770, exits 771-776, labelled jumps per language 777-795, child push with depth rules 796-826); leaf token line accounting 833-862; `else` token 863-873; Halstead operand or operator 874-894; boolean-operator runs 895-914; finalisation 916-956. The line-span `partition_point` pair appears three times (696-698, 839-841, 917-921).
- **Decomposition.** `struct Scorer<'a> { language, source, d, body, nested, newline_offsets, s, code_lines, comments, operators, operands }` removes the 8-parameter signature. Methods: `pruned(node)`, `string_literal(node)`, `named(node, depth, &mut stack)`, `leaf(node)`, `boolean_operator(node, token)`, `finish() -> Result<Score>`. Pure predicates: `is_structural`, `adds_cyclomatic`, `is_case_arm`, `counts_arm`, `labeled_jump`, `is_lambda`, `child_depth`, `is_operand`, `line_span`, `halstead`.
- **Risk.** Output is golden-pinned to the number. Traversal order (reverse child push) matters because `MAX_VISITS` breaks mid-walk (654) and yields a truncated score. `check()` runs per node. The `is_named()` branch falls through to leaf handling when a named node has no children (806-826); easy to lose.
- **Tests.** `tests/core/metrics.rs` (43), `tests/core/metrics_oracle.rs` (6, with `tests/fixtures/metrics/golden/*.rca.json` and `*.arborist.json`), `tests/core/metrics_store.rs`, `tests/http/metrics_http.rs`. No in-file tests.
- **Size M.** After: about 25-30 (`named`).

#### `evaluate` — `src/graph_query.rs:920-1209` (105/41/8/290/2)

- **Why.** Bounded best-first graph expansion in one function; nesting 8 from `loop` > `while head < nodes.len()` > `for call in local` > if / closure / if. Request preamble with five typed early returns 924-966; root setup 968-985; owner inspection 989-1147 (per call: disposition and classification 1026-1046, filter and hints 1047-1063, group key via `serde_json::to_string` of a 14-tuple 1064-1087, merge 1088-1093, rank 1099-1107, candidate build 1108-1135, sort and trim 1136-1145); admit the best candidate 1151-1196.
- **Decomposition.** `resolve_scope(request, source)` (924-966); `struct Expansion { nodes, admitted, declared_only, edges, frontier, filtered_hints, pending }` with `inspect_owner` (990-1146), `filtered`, `offer(&mut self, Candidate, max_calls)`, `admit` (1151-1196); `classify(call, ...) -> CallClass { selected, reason, exact_target, external, test, outside_app }`; `group_key(&CallClass)`; `rank(&CallClass, call, source)`.
- **Risk.** `node = &nodes[head]` is borrowed through the call loop, so method extraction needs a clone of role and document. `source` is `&mut dyn GraphSource`, with `check()` per owner and `check_work()` per call (whether tests pin the count is not verified). Output order depends on `pending.sort_by(key)` on every push and on which candidate is popped at the call limit. The group-key string is the grouping identity. The #778 and #557 rulings are embedded in rank and boundary logic.
- **Tests.** `tests/mcp/{graph_query (47), graph_query_store, graph_query_store_filters, sequence_columns, sequence_incidental, mcp_protocol}.rs`. No in-file tests.
- **Size M.** After: about 25.

#### `evaluate_prepared` — `src/rules.rs:2825-3102` (84/32/5/278/5)

- **Why.** A linear pipeline of independent passes over `serde_json::Value`: budget 2841-2849; module-to-layer assignment 2856-2872; external package assignment 2873-2895; edges into `$layers` and `$external` rows 2899-2972 (the two `json!` literals at 2957 and 2968 are nearly identical); unresolved references 2973-3030 with in-place merge 3005-3024; prepared-row filter 3032-3045; sort, package closure, limitations, per-app counts, payload 3046-3101.
- **Decomposition.** `assign_modules(...) -> ModuleAssignments<'a>`, `assign_external`, `edge_rows` with `policy_row(rule, source, target, layers, status, evidence, unresolved, edge) -> Value`, `unresolved_rows` with `merge_unresolved(existing, row, module, site)`, `filter_prepared`, `summarize(...) -> Value`.
- **Risk.** `Work::charge` order and the typed `bounded_work: ...` messages. Lifetime `'a` on `assignments: BTreeMap<&'a str, Option<&'a str>>`, which is returned in `Evaluation.layers`. Row order before the sort affects ties and the `unresolved_positions` index; the payload should stay byte-stable.
- **Tests.** `tests/core/rules.rs` (42), `tests/core/interfaces.rs`, `tests/index/references.rs`, in-file `app_scope_tests` (3633-4210) and `document_parallel_tests`, `src/store/architecture_apps_tests.rs`.
- **Size S-M.** After: about 15-20.

#### `parse` — `src/rules.rs:531-942` (73/47/4/412/1)

- **Why.** A flat validation table per TOML section; most of the 412 lines are 8-line `return Err(error(text, span, code, attribution, msg))` blocks. Size, TOML, version, collection limits 532-579; policy and `external_deny` 580-599; layers 602-695; roles 699-777; rules and constraints 780-857 (nesting peak); exceptions 860-928.
- **Decomposition.** Follow the precedent already in the file (`authored_apps` :1109, `interface_wrappers` :951): `parse_layers(text, raw.layers, &mut patterns)`, `parse_roles(text, raw.roles, &mut patterns)`, `parse_rules(text, raw.rules, &roles) -> (Vec<Rule>, BTreeSet<String>)` with `parse_constraint`, `parse_exceptions(text, raw.exceptions, &ids)`, plus a small local `schema(span, msg)` / `reference(span, msg)` helper to collapse the error blocks.
- **Risk.** `patterns` is one `&mut usize` budget (`PATTERN_LIMIT`) consumed in the order `external_deny`, layer paths, layer external, role paths, role name; keep the section order. First-error precedence across sections decides the reported span and line.
- **Tests.** `tests/core/rules.rs` asserts `line`, `attribution` and `message` (:232-248, :440-445, :925). Whether `src/rules_edit.rs` or `src/mcp/catalog.rs` depend on spans is not verified.
- **Size S.** After: about 15-20. Cheapest win in this group.

---

## 4. Families: real or superficial

### Reference catalogs (`references/{python,java,javascript,rust}.rs`)

**Superficial as a whole.** There is no shared trait. `references.rs:1216` `CatalogJob` is a plain enum; construction is `match language {0..3}` at :613-670 and site dispatch is a language-string match at :759-796. Signatures differ (Rust takes `Work` by value and resolves with `&mut self`; the others borrow `&mut Work` and resolve with `&self`).

| Phase | python `new` | java `new` | javascript `new` | rust `new` |
|---|---|---|---|---|
| Per-doc range-to-declaration map | 125-141, key `(start,end)` | 130-139, types only | 135-149, `(start,end,kind)` | 238-248, `(doc,start,end)`, global |
| Per-doc syntax inventory | `scan` into `Unit` (162-176) | top-level children only (152-191) | `inventory()` (150-157) | file scope (272), then recursive `scan` (283-293) |
| Cross-file structure | module layout, membership, child names (178-219) | duplicate-type marking (193-199) | none (lazy in `Resolver`) | module edges, parents, roots, externs, `ScopeIndex` (301-408) |
| Per selected doc | precompute star results (223-318) | build `Unit`, blockers, on-demand merge (200-296) | parallel hazards (165-187), precompute star results (192-301) | none; lazy at `resolve` with `export_memo` |
| `resolve` | table lookup | rule chain (300-365) | table lookup (304-334) | full algorithm (1602-1778) |

A `trait LanguageCatalog { fn build(..); fn resolve(..) }` would only rename the match; a shared `IndexBuilder` with language hooks would have nothing in the middle to share. **Recommendation:** split each `new` into per-pass private methods inside its own file; that gets about 90% of the benefit at low risk.

**Three real duplicates inside the family** (separate small PRs):
1. **Star-import precompute.** `python.rs:229-317` ≈ `javascript.rs:197-300`. The inner name loop (python 257-300, js 216-251) is almost line for line: unknown-set residual, `shadowed`, explicit-name check, `bucket.candidates.len() != 1`, then the `*_wildcard_fully_shadowed` / `*_wildcard_empty` fallback. Differences: residual string prefix, shadow source, dynamic flag, external fallback. Proposal: one `fn star_resolution(own, merged, env_unknown, is_shadowed: impl FnMut(&str) -> Result<Option<bool>>, residuals: &StarResiduals)`. Rust (`rust.rs:1668-1736`, `1766-1775`) does the same job with a different algorithm; leave it.
2. **Small value types and helpers.** `DeclarationId` is identical four times (python 7-12, java 6-11, js 7-12, rust 86-91). `charge` is identical three times (python 1101, java 551, js 856). `Candidate`/`Bucket`/`Names` nearly match (python 14-27, js 13-27). `add_bucket` python 1171 ≡ js 1008. `merge_bucket` python 1124 ≈ js 953, but js charges an extra `id.sid.len()`. **`merge_names` is not equivalent** (js 970-995 has unknown-set competition logic python 1141 lacks); do not unify it.
3. **Max-end interval index, three copies.** java `Blockers` 31-89, rust `ScopeIndex` 26-80, `references.rs` `RangeIndex` 1611-1686 → one generic.

`behavior_bindings::Bindings::new` is **not** in this family: one walk over one JS tree, no docs, `Work` or `check`. Its conceptual sibling is `javascript.rs` `inventory`/`declarations`/`pattern_names`/`enclosing_scope`/`body_hazards` (1231-1790); whether those could share code is not verified.

### `advance` steppers

**Shared convention, not a shared shape.** All follow "one bounded unit per call; `None` or `false` means call again", but signatures, context, error and completion types all differ:

| Stepper | Signature and states |
|---|---|
| `ScipDecodeContinuation::advance` (`scip_raw.rs:1651`) | 6 context params, `Result<Option<ArtifactSummary>, ScipCaptureError>`, no phase enum |
| `CargoTargetsContinuation::advance` (`cargo_targets.rs:589`) | `(&mut self) -> Option<CargoTargets>`, infallible, no cancel or budget, 7 phases |
| `TargetDiscovery::advance` (`cargo_targets.rs:449`) | `Option<bool>` |
| `RetainedStageContinuation::advance` / `advance_unit` (`retained_continuation.rs:235` / `256-800`) | 15-phase `Phase` (:66), anyhow `Result<Option<AdmittedBatch>>`; embeds the cargo stepper (:403) |
| `JoinContinuation::advance` (`staged/continuation.rs:102`) | 20 phases |
| `WitnessPreparation::advance` (`store/native_first/witness_preparation.rs:53`) | `Result<bool>` |
| `local_targets::advance` (`store/native_first/local_targets.rs:126`) | `Result<TargetAdvance>` |

No code is generic over steppers; the drive loops are three lines (`scip_raw.rs:1603`, `cargo_targets.rs:146`). A trait would need per-implementor context types and buys nothing. What transfers is the refactor pattern: phase enum, one `step_*` per phase, named cursor structs.

### `unit` versus `advance_unit`, and their one-shot twins

`unit` and `advance_unit` are **not duplicates**: `advance_unit` is retained admission (raw rows into `stage.db`, returns `AdmittedBatch`); `unit` is the join over that batch (returns `JoinedBatch`). They share only shape. At most share a `next_after(conn, sql, cursor)` helper.

The **real duplicates** are each engine's one-shot twin:
- `unit` ↔ `StagedJoin::run` (`staged.rs:2378`): same passes, whole-pass transactions. `JoinContinuation::advance` (`continuation.rs:136-223`) and `join_staged_using` (`staged.rs:2741-2829`) build `JoinCtx` and `StagedJoin` with about 85 near-verbatim lines. A shared `build_join(parts, readers, …)` is a cheap pure extraction.
- `advance_unit` ↔ `Passes::{source_pass, coverage_pass, artifact_pass}` (`admit.rs:164`, `:265`, `:344`, driven by `stage_batch` at `:655`). It already shares `stage_source` and `decode_advance`; further overlap not verified.

Both engines are live (`join_staged` in `src/http/worker/semantic.rs` and `src/index_coordinator.rs:2044`; the continuations in `src/store/native_first/continuation.rs`). No comment or doc marks either as legacy; `docs/` mentions #410 only for limits (`local-topology.md:241`).

### Long parameter lists in `semantic_import.rs`

Exactly five functions have 8 or more parameters, all in the oracle rejoin chain: `rejoin_inventory` (8), `rejoin_inventory_with_destination` (9), `rejoin_inventory_with_reuse` (10), `rejoin_with_stores` (9), `finish_rejoin` (18). The recurring group is `inventory, captured_native, destination_native, producer, basis, admitted_encoding` plus the two `*_sources`.
- **Wrappers: superficial.** A struct would only bundle pass-through; deleting the two dead wrappers is the fix. Keep `rejoin_inventory`'s positional signature (about 30 test call sites).
- **`finish_rejoin`: real.** `Eligibility` is one phase's output consumed by another, and `RejoinHeader` plus `RejoinStores` bring 18 parameters to about 6.

### Dispatchers

**Superficial as one shape.** `provisioned_core_answer` is a typed enum over heterogeneous async handlers (fix: function per route). `dispatch_cli` is a request builder plus per-command reporting (fix: split build from report). `dispatch_connection` is a connection lifecycle and retry machine with scattered string policy (fix: `Operation` policy enum plus phase functions). A common handler table would span sync and async, `Response` and `Value`, and would not touch the real complexity.

**Per-tool table in MCP: partly real.** These sites key on the same 13 names in `catalog::NAMES` (`catalog.rs:7`): `tools::fields` (126-293), `tools::check` (471), `tools::result` summary (927-1003), `run_call` `tool_read` (1323), `architecture::is_tool` (83), `operation` (102), `paged_fields` (367), plus scattered `name == "trellis_workspace_describe"` (mod.rs 1200, 2162; tools.rs 1094) and `== "trellis_search_symbols"` (2104; tools 572/580).
- Real: `fields`, `required`, the validator and the summary fit `ToolSpec { name, fields, required, validate: fn(&Map, &Reject) -> Result<(), Rejection>, summary: Summary }` with `Summary = Count(&[path], noun) | One(noun) | Custom(fn)`. That removes three matches and the `_ =>` defaults that silently mean `read_source`.
- Superficial: the reader (describe never goes through `evidence_read`; the four architecture tools share one reader; `PROMPT_NAME` uses that reader but is not in `NAMES`). Keep it as a match in the new readers module. The JSON schemas (`CATALOG`, `architecture::ENTRIES`, `show_entry.json`, `endpoints_entry.json`) are static fixture-pinned strings; do not generate them.
- Related, optional: validation runs up to three times per call (`tools::validate` at 2978, then `prepare` → `check` at 2982 or 1196) and arguments are re-parsed with `expect("validated")`. A typed `ToolArgs` enum returned by `check` would merge both; a larger step.

**Vertical duplication per operation: real.** Metrics options are validated in the CLI (`main.rs:1265`), daemon-side (`metrics_options_from_request`, `main.rs:3385`) and HTTP (`metrics_query`, `http.rs:4643`); the last two were not compared line by line. The "revision|directory → 300 s, else 30 s" budget appears three times (`main.rs:1061`, `main.rs:2567`, `http.rs:4869`). Share both on `MetricsOptions`.

### Evaluators

`score` (tree-sitter walk), `evaluate` (graph expansion over `GraphSource`) and `evaluate_prepared` (JSON row join) share only idioms (the `check()` cancel closure and ad hoc budgets). **Nothing to share.**

---

## 5. File splits

Suggested order everywhere: test moves first, function refactors in place, then production moves, as separate commits.

### 5.1 Pure-move test extractions (no logic change)

| File | Test lines | Share | Detail |
|---|---|---|---|
| `src/mcp/projection.rs` | ~4,520 of 6,138 | 74% | `tests` 263-702; `find_tests` 1093-1675; `search_tests` 1678-1933; `compact_symbol_tests` 1936-2556; `outgoing_tests` 3479-4043; `incoming_tests` 4073-4329; `source_tests` 4332-4382; `call_graph_tests` 4385-4704; `size_tests` 4710-6138 (1,429 lines, cross-tool, embeds Node/Ajv scripts). Hoist shared fixtures (`super::tests::{pending_tiers, rust_semantic}`, `super::find_tests::fixture`, `super::outgoing_tests::{owner, required_call, with_lines}`) into `projection/test_support.rs` first. |
| `src/scip_raw.rs` | ~2,265 of 4,300 | 53% | `tests` 2036-4145; `continuation_tests` 4147-4300. `tests::FULL` is `pub(super)`; its users were not checked. |
| `src/native_evidence.rs` | ~2,280 of 6,531 | 35% | Tests 4250-6531 in seven inline modules; `full_document_spool_tests` alone is 5604-6531. The file already uses `#[path = "native_rust_macros_tests.rs"]` (:3640). |
| `src/daemon/search_fill.rs` | ~1,580 of 3,054 | 52% | `mod tests` 1473-3054 → `search_fill_tests.rs` via `#[path]`, like its siblings. |
| `src/mcp/tools.rs` | ~1,177 of 2,308 | 51% | `mod tests` 1133-2308. |
| `src/rules.rs` | ~1,020 of 4,210 | 24% | Tests 3189-4210 in five modules; `app_scope_tests` is about 580 (3633-4210). |
| `src/semantic_evidence.rs` | ~830 of 3,431 | 24% | `witness_tests` 2601-2761; `native_index_tests` 2763-3350; `coverage_reason_tests` 3352-3431; plus about 110 lines of scattered `#[cfg(test)]` helpers. |
| `src/references/javascript.rs` | ~870 of 2,775 | 31% | From :1905, 21 tests. |
| `src/references.rs` | ~610 of 2,400 | 25% | From :1791, 14 tests in 4 modules → `references/tests.rs`. |
| `src/daemon/registry.rs` | ~540 of 4,444 | 12% | `selection_tests` 3907-4444. |
| `src/references/rust.rs` | ~330 of 2,302 | 14% | From :1969, 6 tests. |
| `src/semantic_import.rs` | 314 of 5,117 | 6% | 4804-5117: `walk_tests` and `typescript_tests` go with `role_sites.rs`, `language_reason_tests` with `report.rs`. |
| `src/mcp/mod.rs` | ~250 of 3,293 | 8% | `hang_up_tests`, `w1_error_tests`, `settlement_tests`, `selection_commit_tests` move with their modules. |
| `src/main.rs` | ~390 of 3,615 | 11% | About seven small modules; `daemon_shutdown_tests` is about 200. |
| `src/http.rs` | ~80 of 5,760 | 1% | Already mostly sibling files. |

The first ten rows take roughly 15,700 lines out of the largest files with no behavior change.

### 5.2 Production splits

**`src/mcp/mod.rs` (3,293)**

| New file | Lines moved | Kind |
|---|---|---|
| `mcp/workspace.rs` | 17-270: `OpenedWorkspace`, `TransportHooksForTests`, `IdentityError`, `mod tests` | Pure move (range not read in full) |
| `mcp/evidence_read.rs` | 355-751: `validated_expected_basis`, `mcp_read_error`, `call_graph_failure`, `producer_failure`, `app_files`, `evidence_failure_code`, `lookup_root`, `capture_evidence_error`, `evidence_read` | Pure move |
| `mcp/describe.rs` | 753-883 and 2250-2295: `DescribedSnapshot`, `read_describe`, `catching_up`, `DescribeResult`, `describe` | Pure move; later receives 1200-1304 |
| `mcp/call.rs` | 884-1128 (limits, `WorkerPermit`, `CallOutcome`, `Finals`, `InFlight`, `CallContext`, `commit_selection` and its test) plus `run_call` | Types are a pure move; `run_call` needs the refactor |
| `mcp/readers.rs`, `mcp/search.rs` | arms 1323-2053 | Refactor first |
| `mcp/coordinate.rs` | 937-995 (`Input`, `SendGate`, `FinalCheck`, `start_final_check`) and 2297-3293 (`FrameWindow`, `Inbox`, `run_transport`, `coordinate`, `send_call`, `live_calls`, `attempted_workspace`, `receive`, three test modules) | Pure move, about 1,060 lines; then the `Connection` refactor |

`mod.rs` keeps 272-353 (`run_stdio`, `run_socket`, `run_daemon_socket`) and `pub use` re-exports for `CALL_DEADLINE`, `CALL_DEFAULT_LIMIT`, `MAX_CONNECTION_CALLS`, `CallWorkersForTests` and the hook types. External users of those names were not checked.

**`src/mcp/projection.rs` (6,138; about 1,620 production)** — all pure moves with `pub(super)` visibility edits.

| New file | Production lines |
|---|---|
| `projection/mod.rs` | 1-160 (document table, tiers, language status) and `line_bytes` (253) |
| `projection/describe.rs` | 160-260 |
| `projection/symbols.rs` | 705-1090 (`declaration`, `find`, `find_lookup`, `lookup_candidates`, `search`, `symbols`) |
| `projection/calls.rs` | 2560-2838 (`call_evidence`, `intern_bases`, `label_projected_proofs`, `call_warnings`), `outgoing` 2842-3072, `incoming` 3282-3476 |
| `projection/call_graph.rs` | 3086-3277 |
| `projection/source.rs` | 4046-4070 |

Duplication between `outgoing` and `incoming` was not assessed.

**`src/http.rs` (5,760)**

| Target | Lines | Kind |
|---|---|---|
| `http/maintenance.rs` | 90-175, 877-1247 | pure move |
| `http/test_support.rs` (causal and watch accessors) | 557-870 | pure move |
| `http/worker/step.rs` | 1248-1330, 1456-1857 | pure move, then refactor |
| `http/search_glue.rs` | 1331-1455 | pure move |
| `http/dependencies.rs` | 1861-2010, 2667-2869 | pure move |
| `http/api_error.rs`, `http/guard.rs` | 2041-2287 | pure move |
| `http/assets.rs` | 31 `include_str!` routes in each router (2288-~2510, 3706-~3990) | refactor; the two lists have matching counts but were not diffed |
| `http/direct.rs`, `http/query.rs` | 2870-3582 | pure move |
| `http/provisioned.rs`, `http/git_routes.rs` | 3609-4158 | pure move |
| `http/browser_core/{mod,query}.rs` | 4160-4817 | pure move |
| `http/browser_core/` per-family files | 4819-5718 | refactor first |

**`src/main.rs` (3,615, the bin crate)**

| Target | Lines | Kind |
|---|---|---|
| `cli/args.rs` | 27-452 | pure move |
| `cli/producer.rs`, `cli/model.rs` | 579-864 | pure move |
| `cli/api.rs` | 865-966 | pure move |
| `cli/transport.rs` | 967-1143 | pure move |
| `cli/dispatch.rs` | 1147-1603 | refactor |
| `cli/serve.rs` | 1605-1790 | pure move |
| `daemon_main/run.rs` | 1792-2237 | pure move |
| `daemon_main/connection.rs` | 2238-3146 | refactor |
| `daemon_main/io.rs`, `metrics_request.rs` | 3147-3615 | pure move |

**`src/daemon/registry.rs` (4,444)** — all additional `impl` blocks, so pure moves; method bodies not read.

| Target | Lines |
|---|---|
| `registry/selection.rs` (plus the tests) | 32-445, 1699-1784, 3758-3905 |
| `registry.rs` keeps `CheckoutRegistry` core | 446-1025 (`activate` 621-~770 is itself about 150 lines) |
| `registry/registration.rs` | 1026-1540 |
| `registry/lifecycle.rs` | 1546-1698 |
| `registry/runtime.rs` (types) | 1785-2287 |
| `registry/runtime_cli.rs` | 2288-2651 |
| `registry/runtime_test_support.rs` (27 test-support items) | 2658-2998 |
| `registry/runtime_mcp.rs` | 2999-3072, 3378-3521 |
| `registry/runtime_architecture.rs` | 3073-3376, 3522-3741 |

**`src/daemon/search_fill.rs` (3,054)**

| Target | Lines | Kind |
|---|---|---|
| `search_fill/budget.rs` | 33-143 | pure move |
| `search_fill/backend.rs` (`FillSource`, `FillBackend`, `LocalBackend`, `prepare_windows`) | 144-741 | pure move |
| `Slot`, `Inbox`, `Core`, `SearchFill`, `FillRegistration`, `drive` stay together | 745-1466 | shared inbox invariants |

**`src/native_evidence.rs` (6,531)**

| Target | Lines | Kind |
|---|---|---|
| `native_evidence/model.rs` | 274-548 (`Producer` through `Artifact`) | pure move plus re-exports |
| `lookup.rs` | 604-715 | pure move |
| `build.rs` | 716-1160 (`from_capture`, `build_native_header`, `native_revision_digest`, `build_native*`) | pure move |
| `spool.rs` | 1162-1685 | pure move |
| `selective.rs` | 1686-1962 | pure move |
| `classify.rs` | 1964-2578 (`kind`, test markers, `name`, `java_signature`, `header_with_witnesses`, `callee`) | pure move |
| `extract.rs` | 2580-3638 (`extract_document` 2612-2859, `impl ParserState` 2949-3638) | pure move by line range; `ParserState` not read |
| `validate.rs` | 3643-4247 | move together with the `validate_structure` refactor |

Extraction-context and auth-fault hooks (39-232) stay in `mod.rs`.

**`src/scip_raw.rs` (4,300)** — pure moves once `invalid`, `limit`, `cancelled`, `sha` become `pub(super)`.

| Target | Lines |
|---|---|
| `scip_raw/inventory.rs` | 107-315 |
| `wire.rs` | 317-559 plus `next_field`, `document_identity`, `occurrence_roles` (1342-1402) |
| `positions.rs` | 596-794, 1273-1340, 1560-1584 (including the `#[cfg(test)]` thread-local hooks) |
| `claims.rs` | 798-971 |
| `sink.rs` | 973-1146 |
| `decode.rs` | 1148-1212, 1404-1557, 1586-2032 (the `advance` refactor lands here) |

Module-marker functions (1214-1271) fit beside `module_roots.rs`.

**`src/rules.rs` (4,210)** — all pure moves; the `parse` and `evaluate_prepared` refactors are independent.

| Target | Lines |
|---|---|
| `rules/config.rs` | 19-358 (errors, `Raw*`, `Config`) |
| `parse.rs` | 360-1191 |
| `apps.rs` | 1203-1850 (`effective_apps`, `LayerMatcher`, `AppScope`, `AppAssignment`, `validate_*`) |
| `bases.rs` | 2048-2262 |
| `declarations.rs` | 2263-2628 (`selection`, `DocumentEvaluation`, `evaluate_document`, `constraint`) |
| `evaluate.rs` | 1852-2046, 2630-3187 |

**`src/semantic_import.rs` (5,117)** — children import private items through `super::{…}` (`staged.rs:16-25`, `streaming.rs:3-6`), so keep `use x::*` re-exports in the root. Private struct fields read by `memory.rs` would need `pub(super)` (not verified). `src/semantic_import_complexity_tests.rs` (884 lines, attached by `#[path]`) uses private items and must stay a child of the root.

| Target | Lines | Content | Kind |
|---|---|---|---|
| `measured.rs` | 30-445, 520-580 | `MeasuredFacts`, `MeasuredAnchors`, `compose_join` | pure move |
| `coordinates.rs` | 581-869, 4046-4082 | endpoints, `CoordinateMaps`, `InventoryIndex`, `document_coordinates` | pure move |
| `work.rs` | 870-1056 | `ComplexityWork`, `PublicationWork`, `note_*` | pure move; re-export for external `crate::semantic_import::note_*` |
| `keys.rs` | 1057-1283 | scoped keys, `declaration_targets` | pure move |
| `dispatch.rs` | 1285-1559 | `TargetCallable`, `CallShape`, `*_dispatch` | pure move |
| `join.rs` | 1174-1222, 1560-2356 | join driver, `join_occurrence`, `settle_join_references` | move, then refactor |
| `relationships.rs` | 2357-2787 | method and relationship discovery, `emit_type_relationships` | move, then refactor |
| `role_sites.rs` | 2789-3450 | tree-sitter bases and role sites (bodies not read) | pure move |
| `rejoin.rs` | 3451-4802 | rejoin chain, `finish_rejoin`, `manifest_hash`; `DestinationMutations` (3971-4043) could be its own small file | delete dead reuse first, then move and refactor |
| `report.rs` | 446-519 | `LanguageReasons` | pure move |

**`src/semantic_evidence.rs` (3,431)** — from the outline only, bodies not read; all pure moves apart from `pub(super)` on helpers.

| Target | Lines | Content |
|---|---|---|
| `model.rs` | 1-214 | `closed!`/`record!` wire types, `CoverageReasons`, `Target`, `CapturedComponents` |
| `primitives.rs` | 215-388 | `text`, `hash`, `path`, `sid`, `occ`, `key`, `target(s)`, `resolution`, `declaration_name_owner` |
| `native_index.rs` | 389-742 | `NativeIndex`, `DocumentIndex`, `SupportIndex`; with its tests about 940 lines, the biggest single win |
| `rows.rs` | 743-867, 1101-1679 | `validate_join` and the per-row validators |
| `publication.rs` | 915-1036, 1680-2242 | `validate_evidence*`, `validate_evidence_inner` (250 lines; likely its own complexity item), provenance and fact row passes |
| `emission.rs` | 2243-2600, 868-914, 1037-1100 | `EmissionValidator`, `WitnessFact`, `DocumentFacts` |

**References files**

| File | Lines | Split |
|---|---|---|
| `src/references.rs` | 2,400 | `resolve` + `prepare_file` + `SiteContext` (497-924, 1216-1482) → `references/resolve.rs`; `RangeIndex`/`owner`/`walk` (1582-1790) → `references/range_index.rs` |
| `src/references/javascript.rs` | 2,775 | `inventory`..`body_hazards` (1128-1904) → `javascript/inventory.rs`; `Resolver` (336-855) → `javascript/resolver.rs` |
| `src/references/rust.rs` | 2,302 | types + `ScopeIndex`/`Imports` (1-199) stay; build (`new`, `keep_lost_owners_unknown`, `add_scope`, `scan`, `bind_parameters`, `scan_bodies`, 200-838) → `rust/build.rs`; lookup (839-1779) → `rust/lookup.rs` |
| `src/references/python.rs` | 2,098 | optional: syntax inventory (1309-1873: `import_nodes`, `record`, `scan`, `block_pattern`, `literal_all`) → `python/inventory.rs` |
| `src/references/java.rs` (940), `src/dependency_rust.rs` (885), `src/behavior_bindings.rs` (453) | — | no split needed; optionally discovery (169-593) versus source indexing (596-819) in `dependency_rust.rs` |

---

## 6. Cost-benefit ranking

Ordered by (complexity removed × churn × fix history) ÷ risk.

| # | Refactoring | Size | Risk | Why here |
|---|---|---|---|---|
| 1 | Move in-file test modules out of the files in 5.1 | S each | None (pure move) | About 15,700 lines out of the largest files; unblocks every later split. |
| 2 | Delete the dead rejoin reuse path (`rejoin_inventory_with_reuse`, `_with_destination`, `reuse_captured_source_associations`, branch at `semantic_import.rs:3878-3923`) | S | Low | About 150 lines and four `if reused_source` forks gone; confirmed unreachable. |
| 3 | `mcp/tools.rs` `check` → per-tool validators, then `ToolSpec` for fields/required/validate/summary | S-M | Low; error precedence is pinned by 20 in-file tests | 54 commits, 30 fixes; removes three parallel matches. |
| 4 | `rules.rs` `parse` → per-section parsers | S | Low; span and message tests pin it | Follows a precedent already in the file. |
| 5 | `dispatch_cli` → build/report split and shared validators | S | Low | `main.rs` has 117 commits. |
| 6 | `provisioned_core_answer` → function per route family, shared `RequestCancel` and `internal_error` | M | Medium; hook order and `RefreshDependencies` sequence | `http.rs` is the hottest file (184 commits, 88 fixes); 900 lines become a 35-line match. |
| 7 | `run_call` → `ReadCx`, `mcp/readers.rs`, `mcp/search.rs` | L | Medium; registry guard scoping, `resolved` write points | 91 commits; 1,117 lines; adds unit-testable per-tool readers where none exist. |
| 8 | `coordinate` → `Connection` struct with a method per phase | M-L | Medium-high; Decision 0009 §7 phase order, hook positions | Worst function in the repo; mechanical once the struct exists. Do after 7 so `mcp/mod.rs` is already split. |
| 9 | `dispatch_connection` → `enum Operation` policy, `ConnCtx`, phase functions; move test hooks under test-support | L | Medium-high; error routing through retry predicates, coordination territory | Policy scattered over about ten string sets is the bug-prone part; true complexity is well above the measured 37. |
| 10 | Reference catalogs: per-pass split of the four `new`s, `scan`s and `resolve`s; hoist `DeclarationId`/`charge`/`add_bucket`; shared `star_resolution`; generic interval index | M each | Medium; step counts must stay identical | Six functions above cog 75 in one family; all added in the last three weeks with a high fix share. |
| 11 | `validate_structure` → `StructureCheck` sections and `check_ordinals<T>` | M | Low-medium; section order | Second-worst function; `native_evidence.rs` has 97 commits. |
| 12 | `JoinContinuation::unit` and `CargoTargetsContinuation::advance` → `step_*` per phase, named cursors; shared `build_join` | M, S-M | Low-medium; yield granularity and hook positions | Mechanical; no borrow obstacle in `unit`. |
| 13 | `rules.rs` `evaluate_prepared`; `behavior_bindings::new` | S-M, S | Low | Cheap. |
| 14 | `graph_query::evaluate` → `Expansion`; `metrics::score` → `Scorer` | M each | Medium; output order and golden values, but well pinned (47 tests; golden oracle) | Highest nesting and an 8-parameter walk. |
| 15 | `join_occurrence`, `emit_type_relationships` | M each | Medium; reservation order and output order | Production join path, 72 commits in the file. |
| 16 | `search_fill::drive` → `next_job` / `attempt` / `classify` / `settle` | S-M | Medium; owner loop, inbox critical sections | Restructure as decide-under-lock, act-unlocked. |

Also worth doing alongside, small: share metrics option validation and the 300 s / 30 s budget rule on `MetricsOptions`; `ScipDecodeContinuation::advance` helpers (M); `references.rs` `resolve` (L, after item 10).

### Leave alone, or limit

- **`wire` (`scip_raw.rs:444`).** Leaf extractions only. A hot-path parser that reads best linearly; it stays around cog 40-45. Allowlist it.
- **`dependency_rust.rs` `build` and `index_package`.** 2 commits ever, none in 30 days, and warning order is hashed into the catalog identity. High risk, no churn. Leave until the file is next changed for a feature.
- **`advance_unit` (`retained_continuation.rs:256`).** Needs the 50-field struct split first (L), has 2 commits, added 2026-11-04. Defer; revisit if it starts to churn.
- **`settle_document`.** Limit the change to the `Settlement` struct and the generic `publish<T>` for the five-times block. Do not move code across `Reservation` lifetimes without a high-water assertion in place.
- **`worker_step_inner`.** Limit to extracting P4, P5 and the watcher tail as functions called from the same script positions, and replace the two `Cell`s with a returned value. Review against the ratified order; do not reorder.
- **`finish_rejoin` beyond context structs and `rewrite_ids`.** It is the oracle; do not share kernels with `settle_document`.
- **Do not build:** a `LanguageCatalog` trait, a stepper trait, a cross-dispatcher handler table, generated MCP JSON schemas, or a unified `merge_names`.

### Expected effect

If items 1-16 land as estimated, the non-store functions still above cognitive 100 would be `dependency_rust::build`, `index_package`, `advance_unit` and `settle_document` (4, down from 15). These are estimates from the per-function "after" figures above, not measurements. I did not compute resulting per-file line counts.

---

## 7. Measurable targets

Counts are current production Rust declarations (all / non-store).

| Metric | Threshold | Caught today |
|---|---|---|
| cognitive | ≥ 100 | 18 / 15 |
| cognitive | ≥ 75 | 36 / 31 |
| cognitive | ≥ 60 | 48 / 40 |
| cognitive | ≥ 50 | 64 / 53 |
| cognitive | ≥ 40 | 91 / 72 |
| cognitive | ≥ 30 | 154 / 122 |
| cognitive | ≥ 25 | 213 / 169 |
| cognitive | ≥ 15 | 452 / 368 |
| cyclomatic | ≥ 50 / 30 / 20 | 19 / 73 / 176 all; 16 / 62 / 137 non-store |
| loc | ≥ 500 | 16 / 9 |
| loc | ≥ 400 | 23 / 14 |
| loc | ≥ 300 | 37 / 23 |
| loc | ≥ 200 | 83 / 51 |
| loc | ≥ 150 | 135 / 88 |
| loc | ≥ 100 | 265 / 175 |
| maxNesting | ≥ 10 | 15 / 15 |
| maxNesting | ≥ 8 | 30 / 29 |
| maxNesting | ≥ 6 | 97 / 84 |
| maxNesting | ≥ 5 | 159 / 136 |
| parameters | ≥ 10 | 9 / 6 |
| parameters | ≥ 9 | 27 / 17 |
| parameters | ≥ 8 | 73 / 53 |
| cog ≥ 75 or loc ≥ 400 | combined | 46 / 35 |
| cog ≥ 60 or loc ≥ 300 | combined | 66 / 48 |
| cog ≥ 50 or loc ≥ 300 | combined | 78 / 60 |
| cog ≥ 50 or loc ≥ 200 | combined | 107 / 72 |
| cog ≥ 40 or loc ≥ 200 | combined | 127 / 89 |
| cog ≥ 30 or loc ≥ 150 | combined | 206 / 153 |
| cog ≥ 25 or loc ≥ 150 | combined | 257 / 198 |

Non-store files by total lines (tests included), of 220 files: ≥ 5,000: 4; ≥ 4,000: 7; ≥ 3,000: 11; ≥ 2,500: 19; ≥ 2,000: 26; ≥ 1,500: 35; ≥ 1,000: 60.

Three ratchet options for the owner:

| Option | Rule for new and changed code | Existing offenders (non-store) |
|---|---|---|
| **A. Stop the bleeding** | No function at cog ≥ 50 or loc ≥ 300; nesting ≥ 8; params ≥ 10; no file ≥ 4,000 lines | 60 functions, 29 nesting, 6 params, 7 files go on an allowlist that may only shrink |
| **B. Recommended** | A, plus an end-state after the section 6 work: no function at cog ≥ 75 or loc ≥ 400 outside a named allowlist of at most 6 (`wire`, the four "leave alone" functions, one spare); no file ≥ 3,000 lines | 35 functions and 11 files to clear or allowlist |
| **C. Strict** | cog ≥ 25 or loc ≥ 150; nesting ≥ 6; params ≥ 8; no file ≥ 2,000 lines | 198 functions, 84 nesting, 53 params, 26 files |

Two details for whichever is chosen:
- Count a closure against its enclosing function, or cap closures at the same threshold (finding 5). Otherwise moving a body into a closure passes the gate.
- Measure file size on production lines, or do the test moves first; otherwise the file cap mostly measures test volume.

---

## 8. Tool gaps (Trellis MCP)

Observed by me and the five readers. Usage note: the readers called `find_symbols` and `incoming_calls`/`outgoing_calls` a handful of times each and used Read and grep for everything else; `trellis_declaration`, `trellis_call_graph` and `trellis_show` were not exercised.

1. **No resolved call targets for Rust.** rust-analyzer evidence is `possiblyStale` (`semantic.available: false`, 33 failed documents), so every `outgoing_calls` and `incoming_calls` row came back `disposition: staleTarget` with `targetId: null` and empty `nativeCandidates.items` (only `omitted` counts). The tools degraded to name hints; all readers fell back to Read and grep for call structure.
2. **`incoming_calls` is name-matched and produced false positives.** For `CargoTargetsContinuation::advance` it returned callers of all same-named `advance` methods (a test `FakeClock`, the glob NFA, `CheckoutRegistry`) and truncated at 30. For the free function `src/http.rs::router` it listed `browser.router()` (`ProvisionedBrowser::router`, `src/daemon/mod.rs:456`) as a production caller, so it could not answer "is the direct router production-reachable".
3. **Feature gating is invisible.** `oracle.rs::full_rejoin` is under `#![cfg(feature = "test-support")]` but is reported with `callerRole: "production"`. The key finding of this report (the rejoin chain is oracle-only) needed grep.
4. **`compact: true` is not compact.** `outgoing_calls` returned 25 KB or more for one function (about 600 bytes per row with byte ranges and occurrence IDs) and truncated at the 50-row limit before the end of long functions (at line 2158 of `run_call`, which ends at 2248). `incoming_calls` returned `scopeCoverage` for five languages, far larger than the caller rows (about 40 KB for 30 callers).
5. **`outgoing_calls` selection.** For `CargoTargetsContinuation::advance` with limit 50 it returned 13 rows, all generic names (`default`, `new`, `clone`, `parent`, `join`, one `advance`), with `externalOmitted: 88`. For `worker_step_inner`, 50 rows and `externalOmitted: 90`. Project calls were not what surfaced.
6. **`read_source` caps at 200 lines / 16 KiB.** A 1,117-line function needs six calls, so every reader used Read for bodies.
7. **Closures are not addressable in the navigation tools,** although the metrics payload lists them as declarations. The closure at `mcp/mod.rs:1407` cannot be looked up, and parent cognitive complexity excludes nested closures (finding 5).
8. **Metric disagreement.** `drive` is reported with nesting 13; reading the code gives about 7. The metric probably counts closures and `let … else`.
9. **Missing queries.** No file outline (items and `impl` methods with ranges); no "which functions have ≥ N parameters"; no "tests that reach this private function"; no "is this reachable from non-test code"; no per-region complexity breakdown inside a function; no body-similarity or duplication detection.
10. **What worked.** `find_symbols` was accurate for ranges and lookup keys everywhere it was used, found all the `advance` steppers in one query, and correctly surfaced same-named pairs such as the Python and JS `merge_names`. `incoming_calls` on `index_package` and `finish_rejoin` found the right callers (still flagged stale).

---

## 9. What was not verified

- All estimated "after" complexity figures; none were measured.
- Test coverage claims marked "by name only" in section 3.
- Whether bail or error message strings are asserted for `run_call` arms, `advance_unit`, and the `validate_structure` store-side tests.
- Unread bodies: `daemon/registry.rs` methods, `LocalBackend`, `ParserState`, all of `semantic_evidence.rs`, the `role_sites` code in `semantic_import.rs`, `mcp/mod.rs` 17-353 and 753-883, `StagedJoin::eligibility_document`, `javascript.rs` `inventory`/`body_hazards`, `python.rs` `check_followed_bodies`, in-file `occurrence_identity_tests` and `parallel_extraction_tests`, and the bodies of the external test files (matched by name and grep).
- That the two `include_str!` asset route lists in `http.rs` are identical, and that the daemon-side and HTTP metrics validators match.
- Which `provisioned_core_answer` arms are covered only through the fixture router.
- External users of names that `mcp/mod.rs` would re-export after a split, and users of `scip_raw::tests::FULL`.
- Whether `python.rs:1472` over-bound nodes are covered by `syntax_bounds::scan`.
- Whether moving the `max-file-bytes` check in `dispatch_cli` breaks a pinned error precedence.
- Overlap between `advance_unit` and the `Passes::*_pass` functions, and between `projection::outgoing` and `incoming`.
- Whether `module_candidates` in `cargo_targets.rs` can be shared with `reach` (:230).
