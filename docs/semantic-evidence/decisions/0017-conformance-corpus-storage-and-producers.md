# Decision 0017: conformance corpus storage and producers

- **Status:** owner-approved 2026-10-23.
- **Scope:** the Stage 1 conformance corpus package ([#9](https://github.com/SquashMerge/trellis/issues/9)): the [#26](https://github.com/SquashMerge/trellis/issues/26) checker's capture rules, and the language corpora [#27](https://github.com/SquashMerge/trellis/issues/27)–[#30](https://github.com/SquashMerge/trellis/issues/30) and their ratification in [#31](https://github.com/SquashMerge/trellis/issues/31). It amends the captured-executable wording of [contract v1](../contract-v1.md) (`Producer.executableHash`, `SemanticBasis.producerHash`). Identifier derivations, record shapes, freshness rules and the production index are unchanged.

## Decision

1. **Per-language outcome floors are unchanged for now.** The corpus floors in the [toolkit README](../../../tools/semantic-contract/README.md) still apply to every `profile:"corpus"` fixture. A producer feasibility trial (evidence kept outside the repository, under `trellis-evidence/corpus-trial/`) will inform any change. A change will be recorded later as an amendment to this decision, not by editing the floors in place.
2. **Producer and toolchain binaries are not stored in fixtures.** A fixture capture of kind `executable` or `toolchain` may be attested by the reference hardware profile instead of stored bytes. Captured producer output, meaning SCIP indexes and semantic captures, stays checked in, as do the small config and dependency captures.
3. **The JavaScript corpus deferral is partly lifted.** A JavaScript corpus ([#28](https://github.com/SquashMerge/trellis/issues/28)) may start as `profile:"draft"` with source, native captures and annotations only. Semantic captures and outcome answers wait for [#87](https://github.com/SquashMerge/trellis/issues/87)–[#90](https://github.com/SquashMerge/trellis/issues/90) and [#62](https://github.com/SquashMerge/trellis/issues/62).
4. **The conformance corpus stays source-controlled, as ratified.** Pinned upstream repositories, downloaded at test time and cached by hash, belong to the `cohorts-v1.json` `realProjects` qualification and scale harness. That harness runs outside `./tools/verify`. Such downloads never become conformance fixtures.
5. **rust-analyzer is pinned on its own toolchain.** It runs as `rustup run 1.98.1 rust-analyzer`, independent of the repository's `rust-toolchain.toml`. A rust-analyzer upgrade, like any producer upgrade, is a deliberate re-pin: update the hardware profile entry and re-capture the affected fixtures.

## Rationale for decision 2

scip-java alone is 128 MB. Under the [T00 threat model](../../local-topology.md#t00--threat-model) nobody tampers with the repository from inside the user's account, so storing producer bytes adds clone and review cost without adding safety. The [reference hardware profile](../hardware/mac-mini-m5-pro-v1.json) already pins each semantic producer by version and artifact digest. It now also records each producer's contract-form `executableHash`.

## Mechanics

- **Profile digest.** `semanticProducers[].executableHash` uses the contract's empty-prefix rule: the lowercase-hex SHA-256 of the exact executable bytes, with no domain. For a single-file producer (scip-java's standalone launcher, rust-analyzer), the executable is the installed file, whose plain SHA-256 equals the recorded `artifactDigest`. For an npm-distributed producer (scip-typescript, scip-python), the executable bytes are the registry tarball whose `sha512-` integrity is the recorded `artifactDigest`.
- **Pinned capture.** A capture row carries exactly one of `file` (stored bytes) or `pinned`, in the form `"<profile path relative to the repository>#<entry name>"`. Only kinds `executable` and `toolchain` may be pinned. The profile must lie under `docs/semantic-evidence/hardware/`. An executable pin names a `semanticProducers[].name` and matches its `executableHash`. A toolchain pin names a `toolchains[].name` and matches its `toolchainHash`.
- **Checking.** The checker loads the named profile entry and requires its recorded digest to equal the capture's `hash`. It reads no producer bytes. These failures reject the fixture with the existing identity codes: a missing profile, a missing entry, a digest mismatch, or a pin outside the hardware directory. A verified pinned capture satisfies the captured-executable and captured-toolchain checks exactly as a stored one does.
- **Toolchain entries.** The profile records no toolchain digests yet. Each corpus PR that pins a toolchain adds its `toolchains[]` entry.

Example and existing fixtures that store their bytes are unaffected.
