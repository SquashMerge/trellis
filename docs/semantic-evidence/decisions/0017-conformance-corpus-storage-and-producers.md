# Decision 0017: conformance corpus storage and producers

- **Status:** owner-approved 2026-10-23.
- **Scope:** the Stage 1 conformance corpus package ([#9](https://github.com/SquashMerge/trellis/issues/9)): the [#26](https://github.com/SquashMerge/trellis/issues/26) checker's capture rules, and the language corpora [#27](https://github.com/SquashMerge/trellis/issues/27)–[#30](https://github.com/SquashMerge/trellis/issues/30) and their ratification in [#31](https://github.com/SquashMerge/trellis/issues/31). It amends the captured-executable wording of [contract v1](../contract-v1.md) (`Producer.executableHash`, `SemanticBasis.producerHash`). Identifier derivations, record shapes, freshness rules and the production index are unchanged.

## Decision

1. **Per-language outcome floors are amended by [Amendment 1](#amendment-1-2026-10-23-outcome-floors).** The corpus floors in the [toolkit README](../../../tools/semantic-contract/README.md) apply to every `profile:"corpus"` fixture. The producer feasibility trial (evidence kept outside the repository, at `trellis-evidence/corpus-trial/README.md`) informed the amendment below. Later floor changes are recorded as further amendments to this decision, not by editing the floors in place.
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

## Amendment 1 (2026-10-23): outcome floors

- **Status:** owner-approved 2026-10-23.
- **Evidence:** the Stage 1 producer feasibility trial, `trellis-evidence/corpus-trial/README.md` (kept outside the repository). It ran scip-typescript 0.4.0, scip-python 0.6.6, scip-java 0.12.3 and rust-analyzer 1.98.1, digest-verified, on small snippet projects and classified their raw SCIP output by hand.

1. **ambiguous.** The floor stays 20 for JavaScript and becomes 0 for Python, Java and Rust. Java and Rust resolve overloads statically, and scip-python always picks one target. Reaching 20 would need fabricated or deliberately invalid code. scip-python reports a genuinely ambiguous call as a single resolved target. That is a precision risk, handled in Stage 9 ([#15](https://github.com/SquashMerge/trellis/issues/15)) and the Python corpus ([#30](https://github.com/SquashMerge/trellis/issues/30)), not by a floor.
2. **unresolved.** In a document whose semantic coverage is `complete`, a measured call or reference with no producer occurrence at its exact join counts as an `unresolved` outcome. SCIP has no positive "unresolved" record, so this is how the outcome is authored and counted: a `callBinding` or `reference` fact with `resolution:"unresolved"`, a null declared target and no candidates, on that exact join. The checker rejects an `unresolved` disposition whose producer tuple is not `complete`. The floor stays 20 in all four languages.
3. **Rust type relationships.** The floor becomes 0 for Rust. rust-analyzer emits no relationship records, and parsing impl information out of symbol names is not evidence. Stage 8 ([#14](https://github.com/SquashMerge/trellis/issues/14)) may add Rust relationships later.
4. **unsupported.** It is no longer 20 per-anchor outcomes. It is a native-capability requirement: each language's corpus has at least one document that declares a native measurement family unavailable and carries producer facts in that document. `unsupported` is a per-document native property, so 20 anchors from one file would be padding by construction. The producer facts need not join in the unavailable family, and no per-anchor `unsupported` assertion is required. The checker counts `unsupported` by distinct document and revision, with a floor of one.
5. **alias.** A reference at the alias name that resolves to the original declaration counts toward the alias role. It is authored at the measured alias name with roles `definition` and `alias` and the original declaration as its target. Python alias is not applicable to the floor, because `from m import a as b` gives no exact alias anchor. Java alias stays exempt, as in contract v1.
6. **Position encoding.** Each fixture pins its producer's position encoding in `producers[].positionEncoding`, because SCIP indexes leave it unset. The trial observed UTF-16 code units for scip-typescript, scip-python and scip-java (`utf16`), and UTF-8 bytes for rust-analyzer (`utf8`). The checker already rejects a semantic anchor whose encoding differs from its producer's.

Everything else is unchanged: resolved, proven-external, callable-value negatives, the other reference roles, measured calls, scenarios, and type relationships for Java, JavaScript and Python.

| Floor | JavaScript | Java | Python | Rust |
|---|---:|---:|---:|---:|
| ambiguous outcomes | 20 | 0 | 0 | 0 |
| unresolved outcomes | 20 | 20 | 20 | 20 |
| type relationships | 20 | 20 | 20 | 0 |
| unsupported documents | 1 | 1 | 1 | 1 |
| alias role required | yes | no | no | yes |

The [toolkit README](../../../tools/semantic-contract/README.md) lists the complete floors, and `tools/semantic-contract/counts.mjs` holds them as per-language data.
