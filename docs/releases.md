# Release builds

Release targets under #338 are ARM64 macOS with Metal support, and x86-64/ARM64
Linux GNU with CPU support and a glibc 2.31 baseline. Intel macOS, musl, signing
and notarization are not included. Model weights are never bundled.

The owner-directed release default is Metal enabled on macOS and CPU-only on Linux.
`tools/release/release.py build --target <target>` selects these features explicitly;
`Cargo.toml` defaults and ordinary `cargo build` / `cargo build --release` remain CPU-only.
This release policy does not change runtime backend selection: `auto` may fall back to
CPU, while an explicit `TRELLIS_SEARCH_BACKEND=metal` request never silently falls back.

A compiled `metal` capability is not proof that a model selected Metal. The
runtime's automatic backend selection and selected-backend status belong to
#211. Explicit Metal parity runs must fail if Metal is unavailable; CPU fallback
cannot satisfy that gate.

## Tagged releases and candidates

The `release-candidates.yml` workflow uses the same three native rows for tag,
manual and release-path pull-request builds. It runs the native suite, packages
the binaries, smokes the actual archived bytes and records checksums/provenance.
Linux archives also pass ELF import/dependency checks and smoke in glibc 2.31
userspace. The ARM64 macOS build sets deployment target 14.0 and includes Metal with
automatic CPU fallback. Hosted native smoke runs on the workflow's macOS runner;
it is not a claim of a separate macOS 14 execution test.

Only a pushed `v<Cargo package version>` tag can publish to GitHub Releases on
`SquashMerge/trellis`. The tag must match the checked-out package version exactly.
Publication waits for all three target rows to succeed and attaches their
archives, per-archive SHA-256 checksums and build provenance. Manual and
pull-request runs remain CI candidates; they cannot publish. See the
[owner ruling](https://github.com/SquashMerge/trellis/issues/338#issuecomment-6415629491).

Publication validates all three targets' archives, checksums and provenance from
that workflow run before creating a draft release. It uploads all nine required
assets before making the release public. An existing release, including a draft,
causes failure: published assets are never overwritten. If publication fails
partway, inspect the unpublished draft, delete that draft explicitly, then rerun.
Do not delete or replace a public release as a retry step. A Cargo prerelease
version (for example `0.1.0-rc.1`) is published as a GitHub prerelease.

## Build and inspect a Linux candidate

Use a native Linux runner of the target architecture. The baseline build image
is pinned to Ubuntu 20.04 (glibc 2.31); it installs the Rust toolchain from the
checkout's `rust-toolchain.toml`. It does not use a host glibc SDK.

```sh
docker build -f tools/release-linux.Dockerfile -t trellis-release-glibc231 .
docker run --rm -v "$PWD:/work" trellis-release-glibc231 \
  python3 tools/release/release.py build --target aarch64-unknown-linux-gnu
```

Use `x86_64-unknown-linux-gnu` on the x86-64 runner. Set `TRELLIS_BUILD_COMMIT`
to the tagged commit in CI. Ordinary builds use the fixed commit label `dev`;
Git commits and branch switches do not refresh build identity. Do not enable
`test-support` in a shipping binary.
Run the native suite separately on each supported platform. The browser suite
can use a current native host; this is distinct from testing the packaged
binary inside the baseline userspace.

Inspect ELF architecture, interpreter, dynamic dependencies and imported GLIBC
versions, then run the exact archived binary inside clean glibc 2.31 userspace.
A maximum imported GLIBC version does not prove execution compatibility by
itself. Build-image provenance does not establish a Linux kernel baseline.

## Mac candidate

Build on an ARM64 Mac, using the pinned toolchain:

```sh
MACOSX_DEPLOYMENT_TARGET=14.0 python3 tools/release/release.py build \
  --target aarch64-apple-darwin
```

Run Metal feature tests on the native Mac. Full-model CPU/Metal parity is a
separate change-triggered check: re-run it when the model, Candle version or
embedding code changes, not on every release. A parity run needs a working Metal
device; hosted macOS labels do not guarantee one, and CPU fallback is not Metal
parity evidence.

Unsigned browser downloads may be quarantined. After checking the published
SHA-256 checksum, remove quarantine with:

```sh
xattr -d com.apple.quarantine trellis
```

`curl`, `gh release download` and source builds are not normally quarantined.

## Qualification before publication

Every target must pass the native suite and archived-binary
indexing/status/browser smoke; Linux also needs the baseline userspace smoke and
ELF audit. There is **no per-release parity gate**. When the model, Candle version
or embedding code changes, re-run the full form-c CPU/Metal parity check in
[semantic-search-candle.md](semantic-search-candle.md). The seven-row fixture is
not a substitute for all 6,722 cards and 50 queries, and a missing model or
reference corpus is not a successful parity run.

Release archives, SHA-256 checksums and provenance belong on GitHub Releases.
Provenance records the tagged commit, target, compiled features, toolchain,
locked inputs and archived bytes. Rebuildable source is not a claim of proven
bit-for-bit binary reproducibility. No tagged release has been qualified by
this document alone.

The workflow does not bundle or download model weights. Per-release native
builds and archived-byte smoke do not claim fresh full-model parity or actual
Metal inference. No tag creation or release publication is implied by a local
workflow edit; a real tagged CI run and its assets must be observed separately.
