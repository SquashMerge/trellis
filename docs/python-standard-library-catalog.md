# Python standard-library catalog

The [PR361 owner review](https://github.com/SquashMerge/trellis/pull/361#issuecomment-6431615903)
requires a reviewed, pinned CPython standard-library root catalog rather than a
three-root subset. `python-standard-library:v2` labels exactly the **300** names
in CPython **v3.12.0** `Python/stdlib_module_names.h`. The authoritative shipped
membership is the sorted `PYTHON_STDLIB_ROOTS` literal in
`src/references/external.rs`; lookup is exact and case-sensitive. The three
architecture output schemas admit the same roots. This replaces the earlier
three-entry evidence catalog; wire versions remain `references:v2` / `rules:v2`.

## Pin and source provenance

- Repository: [python/cpython](https://github.com/python/cpython).
- Release tag: `v3.12.0`.
- Annotated tag object: `0fb6e700c2f94ae0717ee1be7d4e50b2dd480d14`.
- Commit: `0fb18b02c8ad56299d6a2910be0bab8ad601ef24`.

All paths below are pinned to that commit. Hashes are source-review provenance,
not runtime state or an integrity check against the user's own account.

| Path | SHA-256 |
|---|---|
| [Python/stdlib_module_names.h](https://github.com/python/cpython/blob/0fb18b02c8ad56299d6a2910be0bab8ad601ef24/Python/stdlib_module_names.h) | `6403b75da74a35a8191cdb115237e4b86b2489b3dbd527eb2a288e370330fbf6` |
| [Tools/build/generate_stdlib_module_names.py](https://github.com/python/cpython/blob/0fb18b02c8ad56299d6a2910be0bab8ad601ef24/Tools/build/generate_stdlib_module_names.py) | `08f099c77a6a0bf604c29c1de0614c354e45b7f4abe48f4daba1a4adadf751b8` |
| [Python/sysmodule.c](https://github.com/python/cpython/blob/0fb18b02c8ad56299d6a2910be0bab8ad601ef24/Python/sysmodule.c) | `1c866fd0d93325e0382f0946b32aedc93bdd3f933bef5cc2c7b1bd6ee6959013` |
| [Doc/library/sys.rst](https://github.com/python/cpython/blob/0fb18b02c8ad56299d6a2910be0bab8ad601ef24/Doc/library/sys.rst) | `f81dffff03fcdef48fa58fc2e4bceb741b1473f6a1b2060f05912443fe040ef7` |

The generated header says it creates `sys.stdlib_module_names`.
`Python/sysmodule.c::list_stdlib_module_names` turns that array into the published
frozenset. The generator collects pure-Python packages/modules, built-in,
extension and frozen modules, removes its ignored test modules/packages and
rejects dotted submodule entries before writing sorted roots. Trellis copies
the pinned header; it does not execute the generator or inspect the host Python.

## Coverage and meaning

The pinned `Doc/library/sys.rst` defines `sys.stdlib_module_names` as the same
list on all platforms. It includes modules unavailable on some platforms and
modules disabled at Python build time. All kinds are represented: pure Python,
built-in, frozen and extension modules. Test modules are excluded. For packages,
only the main package is listed, not its subpackages or submodules: `email` is a
root; `email.mime` and `email.message` are not entries.

Trellis labels these exact roots at package granularity. For example, `os`,
`email`, `asyncio`, `tomllib`, `_winapi`, `posix` and `cProfile` are catalog labels
regardless of the machine running Trellis. A dotted import can identify a
catalog root only when the syntax resolver separately proves that root; catalog
membership does not prove any requested descendant or member exists.

No runtime/checkout version, installed library, platform availability, build
configuration, member inventory or semantic call binding is inferred. The
catalog is a fixed reference, not a host-version check or compatibility shim.
Unlisted roots remain unknown. Distribution declarations still do not prove
third-party import-name mappings. Local module precedence and ambiguity remain
the resolver's responsibility; an ambiguous or unproved local root must not fall
through to a standard-library label. Selector spelling validation is unchanged
and does not establish catalog membership.

Private source tests pin the count, strict sortedness/uniqueness, lookup of every
entry, representative platform-specific/new roots and rejection of dotted,
lookalike, third-party and excluded test-module names. They do not inspect a
runtime list.
