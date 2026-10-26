# Rust indexing and static sequences

> Historical validation/analysis. Saved views, notes and Jev/ACP providers described below were retired by the [owner ruling 2026-10-24, #305](https://github.com/SquashMerge/trellis/issues/305#issuecomment-6394175295). Their old files remain in place and ignored. Historical observations are not current feature or startup instructions.

Rust `.rs` files now share the same file → inline methods → sequence workflow as JavaScript.
Native tree-sitter parsing extracts functions, impl methods, containers, closures, calls and
lexical control regions. Function signatures without bodies do not acquire invented behavior.
Source coordinates use byte ranges and preserve UTF8. Rust call IDs include both byte endpoints
so chained calls sharing a start position remain distinct. JavaScript and Rust coexist in one
atomic cached snapshot. Cargo.toml and Cargo.lock join the root manifest hash inputs.

Indexing never invokes cargo, rustc, rust-analyzer, build.rs, package scripts or macro expansion.
Rust semantic/type/trait resolution is not implemented. Calls remain unresolved lexical evidence;
named participant lanes are visual path/receiver hints, not proven runtime objects or dispatch.
The adapter does not borrow JavaScript SCIP evidence or interpret cfg as compiled truth.

Sequence construction uses cached source and measured call evidence. It represents nested
receiver/argument order, branches, match guards, loops, `?` early exit, await, returns and writes.
Plain assignment evaluates the RHS before its assignee. Type-sensitive compound assignment,
macros, unsafe/const blocks, labels/transfers, let-else/chains and other unsupported forms remain
explicit source-linked boundaries. Async/closure bodies do not execute merely when constructed;
a selected async function describes possible execution when polled. Implicit Drop, coercion,
iterator protocol, panic and divergence paths are not expanded. Bounds remain200steps,
20participants and depth24, with explicit truncation. Rust methods/steps are retained, not ranked.

Validation includes mixed-language indexing, cached-source viewing after deleting the live Rust
file, exact measured call provenance, hostile/mismatched semantic evidence, an inert build.rs
sentinel, and existing JavaScript regressions. The real `src/auth.rs` methods `valid_token` and
`load_or_create_token` were selected through the browser and rendered with source highlights.
The larger example had69 steps. Desktop1440px and mobile390px had no page-level overflow.

[Desktop example](images/rust-sequence-desktop.png) · [Mobile example](images/rust-sequence-mobile.png)


## Deployed validation

Inspector port8877 now uses the existing Trellis workspace/state and token. Revision3 contains
74files (39Rust +35JavaScript),2158symbols and11176 lexical calls, with zero parse errors.
172 Rust tests and104 UI tests pass; formatting, clippy and build pass. Remote CI was not run.
Saved views/notes and provider status were preserved. Providers remain disabled for this
workspace; no inference was used. [Validation metadata](rust-validation.json).
