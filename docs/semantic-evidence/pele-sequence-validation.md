# Pele sequence browser validation (run 232)

## Deployed asset and measured cohort

- Exact locally served `pele` 1.6.0 browser JS: 682,005 raw bytes;
  254,221 bytes with Node `zlib.gzipSync` at its default level; SHA-256
  `b82d1bcd435302779a945e1e0f4a54ea56fb26e010ce3f2122f4216fcfc095d7`.
  Its MIT license is in `web/vendor/pele-1.6.0.LICENSE`; pinned npm tarball,
  integrity, acquisition and attribution are in `web/vendor/README.md`.
- Host: Darwin 27.0.0 arm64, Apple M5 Pro; managed Playwright 1.55.1
  HeadlessChrome 140.0.7339.186, loopback daemon with exact embedded assets.
  Both cohorts use temporary indexed Rust checkouts and actual published
  `Store::sequence_at` JSON through the production API. No reconstructed DTO
  is used in either the production/browser E case or these costs.
- Each Rust `cost-{80,180}.rs` function has exactly 80/180 measured call
  steps (recursively counted before timing). Each includes a branch with
  alternate, loop with guard, `?` early-return distinction, measured flat
  three-call group, and an explicitly unresolved call. The browser asserts
  those structures and **Pele rendered** without fallback.

## Browser results

Warm comparison: 20 alternating old/Pele renders, each from the **same
published pinned DTO**, on one Chromium page per cohort. JS `performance.now()`
measures renderer calls including validated SVG and DOM installation. Medians
and p95 are milliseconds; SVG bytes use the installed element's UTF-8
`outerHTML`, including app-added control and witness attributes.

| Indexed cohort | Cold app/connect¹ | First sequence selection² | Old warm median / p95 | Pele warm median / p95 | Pele SVG bytes / nodes | CSP violations |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 80 measured calls | 453 ms | 674 ms | 1.4 / 2.3 ms | 3.3 / 5.2 ms | 45,527 / 308 | 0 |
| 180 measured calls | 433 ms | 530 ms | 2.3 / 4.5 ms | 6.5 / 8.0 ms | 92,948 / 608 | 0 |

¹ From navigation to the cold browser app through token connection, checkout
selection and visible workspace; excludes daemon launch/indexing and browser
process launch. ² From first click on the measured method in the already
opened file to visible Pele-rendered sequence; includes the Store/API read.
These are observed samples, not latency budgets or claims about other machines.
Raw records: `/tmp/trellis-232-cost-{80,180}.json`; passing browser output:
`/tmp/trellis-232-verify-final.log` (`./tools/verify` exit 0). The
verification lanes run concurrently; earlier focused measurements had lower
latencies. Cache and host effects can change these
measurements; the test checks functionality and records, not a speed threshold.

Four producer-validated JavaScript/Rust/Java/Python DTO cases render through
real Chromium, preserving original ordered note identity and measured
inspector actions. A published indexed checkout E case proves actual
note→inspector→explicit exact pinned source (click, Enter, Space, stale
rejection). The nested loop/guard P case checks Pele 1.6.0's frame close
order and rejection of changed frame shape. No CSP violations were observed
in the tested cases. The current CSP did not gain `unsafe-inline`.

## Negative control and former blocker

`/tmp/trellis-232-P-pre-mutation.log` passes; hiding the production generated
SVG via a temporary in-lane CSS mutation makes the class-wide Chromium test
fail `display 'none' !== 'block'` (`/tmp/trellis-232-mutation.log`); restoring
CSS passes (`/tmp/trellis-232-P-restored.log`). The mutation is reverted.

An earlier run failed with `Changed Pele control frames`: source/preorder
`[alt, loop, opt, opt, alt, opt]` differs from Pele's nested SVG DOM close
order `[alt, opt, loop, alt, opt, opt]`. Bounded actual Store reproduction:
`/tmp/trellis-232-shape-count.json`. The adapter now validates exact
kind/header in close order while retaining source note ordinals and separate
original DTO witnesses; an intentionally changed frame fails closed. An
initial simple 80/180-call fixture without controls was exploratory only,
not the accepted cohort; its timings are deliberately excluded here.
