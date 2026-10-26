# Release builds

Release targets under #338 are ARM64 macOS with Metal support, and x86-64/ARM64
Linux GNU with CPU support and a glibc 2.31 baseline. Intel macOS, musl, signing
and notarization are not included. Model weights are never bundled.

A compiled `metal` capability is not proof that a model selected Metal. The
runtime's automatic backend selection and selected-backend status belong to
#211. Explicit Metal parity runs must fail if Metal is unavailable; CPU fallback
cannot satisfy that gate.

## Candidate workflow

The manually dispatched `release-candidates.yml` workflow builds the three
native candidates, runs the native suite, packages them, smokes the archived
bytes and records checksums/provenance. Linux artifacts also run the smoke in
the glibc 2.31 build userspace and pass ELF import/dependency checks. These are
CI artifacts, not GitHub Releases. It has no tag trigger or publication job
until the complete parity/Metal gate is provisioned and approved.

## Build and inspect a Linux candidate

Use a native Linux runner of the target architecture. The baseline build image
is pinned to Ubuntu 20.04 (glibc 2.31); it installs the Rust toolchain from the
checkout's `rust-toolchain.toml`. It does not use a host glibc SDK.

```sh
docker build -f tools/release-linux.Dockerfile -t trellis-release-glibc231 .
docker run --rm -v "$PWD:/work" trellis-release-glibc231 \
  cargo build --locked --release --no-default-features --bin trellis --target aarch64-unknown-linux-gnu
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
cargo build --locked --release --no-default-features --bin trellis \
  --target aarch64-apple-darwin --features metal
```

Run Metal feature tests and actual full-model parity on a Mac with a working
Metal device. Hosted macOS labels do not guarantee device availability.

Unsigned browser downloads may be quarantined. After checking the published
SHA-256 checksum, remove quarantine with:

```sh
xattr -d com.apple.quarantine trellis
```

`curl`, `gh release download` and source builds are not normally quarantined.

## Qualification before publication

Every target must pass the native suite, archived-binary indexing/status/browser
smoke, and its backend's full form-c parity gate described in
[semantic-search-candle.md](semantic-search-candle.md). The seven-row fixture is
not a substitute for all 6,722 cards and 50 queries. Missing external model or
reference provisioning fails qualification, rather than skipping successfully.

Release archives, SHA-256 checksums and provenance belong on GitHub Releases.
Provenance records the tagged commit, target, compiled features, toolchain,
locked inputs and archived bytes. Rebuildable source is not a claim of proven
bit-for-bit binary reproducibility. No tagged release has been qualified by
this document alone.

CI parity provisioning and real-Metal execution are pending owner selection on
#338. Publication must not proceed without that gate. These instructions are
candidate-build groundwork, not a completed release workflow or a release
acceptance claim.
