# Current-directory file browser

> Historical validation/analysis. Saved views, notes and Jev/ACP providers described below were retired by the [owner ruling 2026-10-24, #305](https://github.com/SquashMerge/trellis/issues/305#issuecomment-6394175295). Their old files remain in place and ignored. Historical observations are not current feature or startup instructions.

The file tree and index share the selected workspace by default. The workspace defaults to
cwd; `--workspace PATH` changes both. `serve --browse-root PATH` explicitly selects a different
read-only browsing root and the UI then warns which workspace Index actually targets. Search is optional:
root entries appear immediately, folders expand lazily, and indexed methods open beneath files.
Contiguous directories with exactly one child directory and no other complete entries render as one
compact `/`-joined row. Opening that row follows the chain with bounded sequential metadata reads.
Branches, files, incomplete pages, truncation, loading and errors remain separate and visible.

The running inspector now browses and indexes Trellis itself. A file gets methods only when
it belongs to the cached index. Unsupported files remain visible with explicit reasons;
Rust parsing and static sequences are supported; see [Rust support](rust-support.md). The previous sample index, saved views, notes and provider
allowances remain preserved in their original state directories. They were not reset or
silently rebound to this different workspace. See [workspace correction](workspace-index-fix.md).

The tree endpoint returns filenames and metadata, not file content. It does not execute
repository code, follow symlinks, start indexing, or make model calls. The existing source
viewer continues to use cached indexed source. Directory pagination and scan limits are
explicit. A live file tree can differ from the older cached index until an explicit refresh.


## External participants

The JavaScript adapter identifies unshadowed built-in names such as `JSON` and static import
bindings from cached syntax. Imported bindings are grouped by module. Plain receiver names
remain labelled hints; they do not prove a class or object identity. Shadowed, written,
dynamic, and eval-sensitive cases downgrade conservatively. Buffer is a Node runtime hint,
not an ECMAScript built-in guarantee. Measured call resolutions remain unchanged: naming an
external participant does not mean its implementation has been indexed or expanded.


## Validation

Deployed on port8877. 146 Rust tests and77 UI tests pass, with formatting, clippy and build
also passing. Browser validation expanded `src` to reveal `main.rs` without any search text;
then navigated folders to the indexed sample and confirmed its JSON participant lane.
The390px mobile viewport has a338px tree and no page overflow. Current revision3, saved
views, notes, token and provider budgets were preserved. No provider calls were made.
Tree metadata uses Unix directory handles (macOS/Linux); Windows is not implemented.

[Validation metadata](cwd-tree-validation.json) · [Desktop](images/cwd-tree-desktop.png)
· [Mobile](images/cwd-tree-mobile.png)

Compact-directory follow-up: [deployed validation](compact-directory-validation.json).
