# Partial-call rust-analyzer fixture

Real rust-analyzer 1.98.1 (48a229ce 2026-09-01) SCIP output for `workspace`.
The source combines macro/attribute gaps with `run -> helper -> leaf` calls.

Generate with the pinned producer, not the application toolchain:

```sh
rustup run 1.98.1 rust-analyzer scip "$PWD/workspace" --output index.scip
```

`documents.scip` retains the original Index document units byte-for-byte. Only
Index.metadata (field 1), containing the producer workspace path, was removed,
as in the other rust-analyzer fixtures. The test supplies its temporary workspace
metadata. No document, occurrence or symbol was rewritten.

Producer binary SHA-256: `78082ea1a96a008751767bd35f43738b5d4565cbef06ee5b31ad742a316133b5`.
Original artifact SHA-256: `4a20bcc5287af38ac69974fb8a0aa4a355f2a2e26d37675880fb16a548d8a5ba`.
Document bytes SHA-256: `fd63b7b7289d977ba1ea48fbe7c5b11942f4d236d6f3c50c601433158265aadc`.
