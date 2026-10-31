# Automatic library catalog (Rust first)

Trellis discovers included Rust/Cargo libraries from the selected workspace and indexes local
declarations automatically. There are no downloads, Cargo invocations, build scripts, proc macros,
or provider calls during library indexing. The shared catalog/API is ecosystem-neutral, but the
first discovery adapter supports Rust/Cargo on the existing Unix secure filesystem reader.
This is not support for every language or Windows yet.

## Current interface (revision 7)

The charcoal/orange workbench has Sequence, Classes and Libraries tabs. Select a call to inspect
its target and evidence, then choose **Open call site** to read the cached workspace source.
Library definitions open a separate source-dock tab. Selecting either source does not expand
third-party behavior. Compact diagram labels retain full evidence in the inspector and Show all.
See [the workbench contract](design-refresh-contract.md) and
[validation](design-refresh-validation.json).

## Use it

Libraries is not a tab (owner decision 2026-10-29, #450). Choose **Open in library** on an
Architecture reference into an external package, or in the step inspector of an external call
whose SCIP symbol names a catalogued package. The source dock's **Library** tab shows that
package's indexed definitions without entering library paths or knowing source filenames.
Filter them and select a definition to open its source. Source reads are explicit, hash-checked and separate from your workspace source
pane. The source window shows at most 600 lines; Previous/Next uses the cached text.

The catalog builds in the background at startup and after Index, and the browser re-reads its
status with the workspace status (header ⋯ → Refresh). **Open in library** appears only once the
catalog is ready and indexes the package. This does not call a model.

`src/acp.rs → check_file_with_unlinked` is the primary example: its fluent OpenOptions chain can be
collapsed or expanded. A collapsed chain retains an arrow for its first measured call and labels
additional calls as collapsed. It does not assign every call in the chain to the first receiver.
Unused and hidden-only lanes are omitted. Unresolved calls show source receiver/callee names where
available (for example `file` and `check_file_metadata`), explicitly not resolved types.
A matching external type is a **candidate**, not a resolved receiver or runtime object. Receiver calls such as `.open()` and `file.metadata()` stay unresolved without
semantic evidence. Show all restores the original measured steps.

## Boundaries

- The declaration catalog is separate from workspace behavior and provider packets.
- Third-party symbols are not valid sequence roots. Library implementations are not expanded.
- Candidate annotations do not change measured call IDs, ranges or resolution.
- Rust cfg, exports, traits, receiver inference and dispatch are not resolved. A future semantic
  adapter is still required for reliable receiver/type binding.
- Catalog configuration is captured at refresh; it is not compiler evidence attested to cached
  workspace source. Use Index after changing manifests or toolchains.
- Local registry entries are name/version source candidates, not checksum-verified package origins.
- Missing, blocked, ambiguous and partial packages remain visible. Workspace inheritance,
  custom source replacement, unsupported git sources, outside-workspace path dependencies,
  custom library roots and generated code are not silently guessed.
- Memory catalog snapshots rebuild on startup; there is no disk catalog cache or file watcher yet.
- The manual Rust source-root endpoints remain a fallback API; the browser no longer offers them (#450).

## Trusted toolchain configuration

Cargo sources use `--cargo-home`, otherwise `CARGO_HOME` or the normal user Cargo home.
Standard sources use `--rust-library` or `RUST_SRC_PATH`. Alternatively configure an explicitly
trusted absolute compiler with `--trusted-rustc /absolute/path/to/rustc`.

That optional startup probe runs only `--print sysroot`, outside the workspace with a cleared
environment, a three-second deadline and bounded output. It does not run during indexing or
execute project commands. Do not provide a project-supplied wrapper. This is not an OS sandbox.
Missing standard-library sources are not downloaded. No filesystem layout is hard-coded.

## APIs

- `GET /api/dependencies`: catalog state, package availability, counts and warnings.
- `GET /api/dependencies/symbols?catalogId=…&packageId=…&q=…`: paged declarations.
- `GET /api/dependencies/source?catalogId=…&sourceRef=…`: hash-checked explicit source view.
- `POST /api/dependencies/refresh {}`: rebuild local catalog; does not index workspace or call a model.

All routes use the existing authentication/origin checks. Catalog and workspace revision checks
reject obsolete requests. Retained directory handles prevent source roots from being retargeted
through pathname replacement. Symlinks below those roots are not followed.

## Resource limits

2 MiB per file; 2,000 files and 64 MiB source per catalog; 500 files and 16 MiB per package;
50,000 declaration records globally, 2,500 per package, and 64 MiB of declaration text. Additional metadata, traversal,
namespace, signature and per-package record bounds apply. Limits produce explicit partial status.
Only one catalog builder runs per daemon; refreshes coalesce and superseded work is canceled.
Parsing is byte-bounded, not protected by a hard tree-sitter parsing deadline.

The initial catalog release was revision 5. The collapsed-call/source-hint display fix is deployed
at revision 6: 261 Rust tests and 143 UI tests passed; formatting, Clippy with warnings denied and
build passed. Remote CI was not run. See [display validation](collapsed-call-preview-validation.json).

The real catalog discovers 244 packages. 80 currently have indexed declarations; the remaining
164 have no declarations under the current bounds. The catalog contains 50,000 records across
1,742 captured source identities. Package status is 73 complete within the supported syntax scan
and 171 partial. These are not claims of complete compiled APIs or active dependencies.
See [validation details](automatic-library-validation.json).
