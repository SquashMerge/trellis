# Rust adapter slice

Implement .rs indexing and sequence browsing in the existing language-neutral graph/SequenceView.
Use tree-sitter-rust and cached source. Indexing must never execute cargo, rustc, build.rs, or
macros. Trusted Trellis development builds/tests remain allowed. No provider calls or schema migration.

Indexer owns src/indexer.rs, src/indexer_rust.rs, tests/rust_indexer.rs.
Behavior owns src/behavior_rust.rs, tests/rust_behavior.rs.
Root owns Cargo dependency/lock, lib registration, behavior dispatch, HTTP support reasons,
integration tests, docs and deployment. Preserve JS functionality.

language=rust; callable Symbol ranges are whole AST function_item/closure_expression nodes.
Impl methods are Method; free functions Function; containers use existing Module/Class kinds.
Trait signatures without bodies must not acquire fabricated behavior. Closures have independent
ownership; their bodies do not run at definition. Macro token trees are opaque boundaries,
not ordinary calls. (Native measurement, and the graph calls derived from it, measure the
arguments of allowlisted expression macros such as `assert!` and `format!`: Decision 0019.) Attributes/cfg/module ambiguity must remain explicit. Async function body
models possible execution when polled, not future construction. No compiler/type/trait-resolution
claims. Prefer unresolved calls to unsafe name matching. IDs, ranges, owners, provenance retained.
Call ordinal is source order, NOT execution order.

Behavior: receiver/argument evaluation before calls, if/iflet/match with guards, loops without
unrolling, short-circuit, return, ? early exit, await, mutations. Unsupported transfers, labels,
patterns, unsafe constructs, macros, async blocks and closure bodies may be explicit source-linked
boundaries. Never flatten alternatives or render a suffix unconditionally after possible exits.
Names/receivers are visual hints, NOT object identity or inferred method dispatch.
Bounds:200steps/20participants/depth24, warnings/truncation/provenance. No Rust importance filter.

Tests: mixed JS/Rust discovery, functions/impl/closures/macros/UTF8/recovery, cached source;
behavior nested order, branches/match/loops/?/await/return/boundaries. Validate real src/auth.rs
function valid_token and a larger method. No external source commands during indexing.
