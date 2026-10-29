# Read-only Git status

Git status is a live, read-only observation of a selected checkout. It is not
semantic evidence and is not pinned by an index revision. Reads do not fetch,
stage, commit, refresh/write the Git index, run hooks, or execute repository-
configured filters, fsmonitor, credential helpers or diff programs.

## Reader

The reader uses exact-version-pinned `git2` 0.21.0 with vendored libgit2 and no
network features. `Cargo.lock` pins `libgit2-sys` 0.18.8+1.9.7 (libgit2 1.9.7). It uses libgit2's built-in content conversions only. External filter
registration and transport/credential callbacks are not enabled. For paths with
custom filters (for example Git LFS), the comparison is of stored/worktree bytes
with those external transformations omitted; it does not claim native Git
porcelain equivalence for transformations it deliberately cannot execute. Build environments
must not set `LIBGIT2_NO_VENDOR`, which would override the vendored choice.

A hidden Trellis child performs synchronous library reads. The parent enforces a
5-second deadline and a 2MiB complete JSON response budget, matching the existing
live-source response budget. It kills and reaps the child on timeout; dropping a
request must not leave Git work running. Checkout-list enrichment shares one
aggregate deadline rather than granting each row another full timeout. The complete
checkout-list response also has a 2MiB serialization cap; an oversized list fails
with HTTP 413 rather than dropping rows. Expected Git-read failures in selected
status/diff replies use HTTP 200 with tagged unavailable state. No database,
index writer, persistent cache or polling owner is involved.

## Browser API

- `GET /api/checkouts` includes a lightweight `git` summary in every row, with
  a measured changed-file count but no changed-path or hunk arrays.
- `GET /api/checkouts/{rootKey}/git` returns a summary and changed paths.
- `GET /api/checkouts/{rootKey}/git/diff?path=...` returns one changed file's hunks.

These routes retain the existing Bearer, Host, Origin and CSP boundary. The
selected root is checked before and after the live read. The repository workdir
must resolve to that selected root; `core.worktree` cannot redirect reads elsewhere.
Git reads do not require a ready semantic index and do not activate an index runtime
just to inspect Git.
Malformed paths are caller errors. A diff is not an arbitrary file-read endpoint:
the requested path must belong to the measured changed set.

A summary is tagged `available` or `unavailable`. Available summaries identify a
branch or detached HEAD, full and short commit, an optional upstream with
ahead/behind counts, and `workingTree`. Upstream counts use only existing
remote-tracking refs and are labelled **as of last fetch**. No upstream is not zero
ahead/behind. `workingTree` is independently tagged:

- `{ "state": "available", "changedFiles": N }` is the complete measured count,
  including an explicit zero for a clean checkout.
- `{ "state": "unavailable", "reason": GitReason }` means the change set could not
  be measured; it never substitutes zero. The checkout-list summary retains the
  independently measured HEAD/upstream, for example for a conflicted index.

HEAD/upstream or whole-reader failures use outer `unavailable` with a closed reason
code, never a guessed branch. Selected status/diff failures remain all-or-nothing:
outer `git` unavailable and no changes/file. Successful selected status/diff replies
carry the same complete `workingTree.changedFiles` count, not just the selected
file's count. Separate requests are fresh observations, so edits between them can
change the count.

## Change semantics

Changes are the net difference from HEAD to the working tree, including staged
and unstaged changes. Ordinary untracked files count as added; ignored files are
excluded. The summary count uses this same complete changed-set classification,
not staged plus unstaged totals: each changed path counts once, a rename counts
once, and a staged change undone in the working tree counts zero. Paths have
modified, added, deleted or renamed status; a rename carries its previous path. A submodule is reported as one nontext gitlink change, never as
nested changed paths. Libgit2 may inspect its dirtiness to classify that gitlink;
the same child deadline applies. Conflicts and unsupported or unreadable repository
state are typed unavailable, not silently clean.

Paths are lossless UTF-8 or the read fails explicitly; absolute/traversing paths
and path spellings unsupported by the checkout-bound reader are refused. Structured
hunks retain exact line text and line terminators. Binary files explicitly have no textual hunks.
Output over the response budget is rejected as a whole, never silently clipped.
A changed HEAD during a read is `git_changed`; later reads are fresh observations,
not a promise of an atomic working-tree snapshot across requests.

The checkout list and existing Viewer header show the measured branch or detached
commit, upstream counts and last-fetch label, or **Git unavailable**. Cold or broken
semantic indexes do not erase independently measured Git state. This slice does
not add a Files layout, stage/commit/fetch actions, or remote-machine access.

## Verification

Tests cover upstream counts, detached HEAD, no upstream, non-Git roots, linked
worktrees, renames, deleted files, response/deadline failures and configured program
sentinels. The sentinel fixture defines hooks, fsmonitor, credentials and filters;
none may execute. Index bytes and metadata must remain unchanged by the read.
Route tests retain selected-root admission and authentication. UI tests cover cold
indexes, checkout changes and unavailable state without retaining stale branches.
