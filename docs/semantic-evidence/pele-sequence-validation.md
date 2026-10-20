# Pele sequence browser validation — blocked evidence (run 232)

This is **not** a passed performance acceptance. The final 80/180-call indexed
normal/near-cap cohort must render through Pele without fallback; it currently
falls back. Do not use the exploratory timings below as acceptance measurements.

## Reproducibility and asset

- Exact local `pele` 1.6.0 browser asset: `web/vendor/pele-1.6.0.js`,
  682,005 raw bytes, 254,221 bytes with Node `zlib.gzipSync` (default level),
  SHA-256 `b82d1bcd435302779a945e1e0f4a54ea56fb26e010ce3f2122f4216fcfc095d7`.
  MIT license: `web/vendor/pele-1.6.0.LICENSE`; acquisition, npm tarball
  integrity and attribution: `web/vendor/README.md`.
- Cohort: macOS Darwin 27.0.0 arm64, Apple M5 Pro; pinned Playwright
  HeadlessChrome 140.0.7339.186 / Playwright 1.55.1. Loopback production
  browser, isolated temporary checkout indexed and read through Store/API.
- The fixture files `tests/fixtures/sequence-pele/cost-{80,180}.rs` produce
  exactly 80 and 180 recursive measured call steps, respectively. They include
  branch, loop, `?` early return, and unresolved call targets. The test asserts
  the producer count and no Pele fallback *before* measuring warm renders.
- Four producer-validated DTO browser parity cases and the real published
  smaller checkout E case pass. All recorded browser cases observe zero CSP
  violations. Exact first-slice adapter + vendor assets, not a CDN, are used.

## Current blocker and measurements

`./tools/verify` exits 1. Its Rust browser lane fails only at the required
80/180-call no-fallback assertion: `Pele fell back: Changed Pele control frames`.
The checks lane also fails because `tests/factory-config.test.cjs` freezes the
old `tools/verify` browser command list; that test needs a separately approved
path update. The other 1,418 Rust tests pass (six skipped). Full log:
`/tmp/trellis-232-verify-blocked.log`.

For each actual published 80/180 DTO, `TrellisPeleSequence.project` yields
frame source/preorder `[alt, loop, opt, opt, alt, opt]`; Pele's SVG frame DOM
order is `[alt, opt, loop, alt, opt, opt]` for nested control. The first-slice
adapter compares these orders literally and rejects the whole view. It must
validate nested frame identity without guessing geometry or dropping evidence.
Reproduction: `/tmp/trellis-232-shape-count.json` (projected Mermaid, frame
headers, SVG class/rect counts, producer call counts) and
`/tmp/trellis-232-cost-adjusted.log`. The required fix is outside this slice:
`web/sequence-pele.js`.

**Not yet measured on the accepted cohort:** cold app load/first sequence
selection, 20 warm old/Pele timings, generated SVG bytes/node counts and
final CSP results for the branch/loop/try 80/180 fixture. The cost test contains
these assertions and writes `/tmp/trellis-232-cost-{80,180}.json` on success;
its current run stops at the no-fallback assertion. Do not substitute a
reconstructed Store DTO or an empty-control fixture to fill this table.

For context only, an **earlier exploratory, superseded** 80/180-call indexed
fixture *without these control cases* rendered in both engines: 20 warm
old/Pele medians 1.3/2.9 ms (80) and 2.0/6.5 ms (180), p95 3.2/4.7 ms
and 5.7/19.3 ms, SVG bytes/nodes 38,708/254 and 86,105/554, zero CSP
violations. These are not accepted normal-cohort numbers and contain no cold
app timing. Original logs: `/tmp/trellis-232-cost-{80,180}.json`.

## Negative control

Passing `/tmp/trellis-232-P-pre-mutation.log` → temporarily hide the generated
SVG with an in-lane CSS production mutation → four-language browser test
fails `display 'none' !== 'block'` in `/tmp/trellis-232-mutation.log` → restore
CSS → passing `/tmp/trellis-232-P-restored.log`. The production mutation was
reverted. This confirms the class-wide P visibility assertion detects a real
integration regression.
