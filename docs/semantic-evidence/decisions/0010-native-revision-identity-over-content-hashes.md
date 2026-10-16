# Decision 0010: native revision identity over document content hashes

- **Status:**
  - **Owner-approved 2026-10-16** (#141, option (c)).
  - **Pending:** Prime review, and ratification when the implementing PR merges.
- **Scope:** how the native producer computes a captured revision's `revisionId`. Nothing else in the native artifact, the contract, pins or attestation changes.
- **Compatibility:** none needed (pre-release). Indexes built with v1 IDs are recreated, not migrated.

## Background

The native revision ID was `revision:v1:` + `digest("trellis.native-revision.v1\0", canonical(input))`. In `input`, every captured document appeared with its **full bytes, hex-encoded**:

```
{config, dependencies, documents:[{byteLength, bytes:<hex>, language, path}], sourceSetId, toolchain}
```

The ID is recomputed on every publication, including a one-file edit, so hashing it cost time proportional to the **whole corpus's bytes**. At 10k files (128 MiB) that is 256 MiB of hex per edit: about 1.39 s, and about 0.49 s after #144 streamed it into the hasher. That was the largest single per-edit cost in #141's stack samples.

## Decision

The native revision ID is now `revision:v2:` + `digest("trellis.native-revision.v2\0", canonical(input))`, where each document enters by its **SHA-256 content hash** instead of its bytes:

```
{config, dependencies, documents:[{byteLength, contentHash, language, path}], sourceSetId, toolchain}
```

- **What `contentHash` is:** the document's existing captured content hash, the SHA-256 of its source bytes. That's the same value already stored as the document's content hash and attested by selected reads.
- **What it adds:** every other component is unchanged (config, dependencies, source set, toolchain, document order, canonical encoding). The new domain separator and `v2` prefix mean a v1 and a v2 ID can never be confused.
- **Cost:** O(number of documents), not O(corpus bytes). That's under 1 MB of canonical input at 10k files.

**Why this satisfies contract-v1.** The contract treats `revisionId` as an opaque *"captured semantic revision identity"* that resolves to the captured revision and self-matches its facts. It does not prescribe how the ID is computed. The v2 ID still commits to every document's exact bytes, through a collision-resistant hash of them, and to each document's path, language and length. Any change to any of those changes the ID.

## Consequences

- **Existing indexes:** an index built with v1 IDs fails the revision-header check. A read is refused with the typed `incompatible_index: reconciliation required after invalid current index`. The daemon's startup reconciliation then rebuilds the index as a **new generation**. That was checked live with only `status` calls: the first was refused, the second, 2 s later, served the new generation. Pins into the old generation conflict as for any recreated index. There is no migration, as AGENTS.md requires for disposable derived state.
- **Unaffected:** occurrence, syntax and stable IDs, test vectors (which treat revision IDs as opaque text), pins (`{indexGeneration, indexRevision}`), selected-document attestation, and the MCP and contract shapes.
- **Not a security change:** under T00 the ID is a consistency identity, not a MAC. Its strength rests on SHA-256 either way.
