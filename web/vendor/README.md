# Pele browser asset

`pele-1.10.0-trellis.2.js` is Pele 1.10.0 plus the exact treemap commits from [obsidianmd/pele#91](https://github.com/obsidianmd/pele/pull/91) and the native block-edge label fix below. MIT (see `pele-1.10.0-trellis.2.LICENSE`, the unchanged upstream `LICENSE`). Trellis temporarily vendors these patches until an upstream release includes both fixes; then it returns to an unchanged upstream release (Decision 0014).

## Source provenance

- Repository: <https://github.com/obsidianmd/pele>
- Base/tag: [`4eff76f98c73b6817800a05773517f35832e4e92`](https://github.com/obsidianmd/pele/commit/4eff76f98c73b6817800a05773517f35832e4e92), `1.10.0`.
- The unpatched base reproduces npm `pele` 1.10.0's `dist/pele.global.js` byte for byte: 709352 bytes, SHA-256 `dd70e44152f7d70d62fedc622f588db3f5855859b444f4d8cf274925c12f2651`. [npm tarball](https://registry.npmjs.org/pele/-/pele-1.10.0.tgz), integrity `sha512-hDMEM+GQ01pZin8tst3PRXGeB/xFATvXBreHHtjx2glSNAQ7nHFq7EbMuNJGTbqkYgueWPboLjyUBsAudsb1qQ==`.
- Apply the following five #91 commits, in order, unchanged:
  1. [`e1aaa16249eb10e02e06930c5995078bcf30c598`](https://github.com/obsidianmd/pele/commit/e1aaa16249eb10e02e06930c5995078bcf30c598) — Treemap: Keep boxes too small for their gap or their section's header
  2. [`db2f353efacf63d16a74d96a274f43b6ecce85a8`](https://github.com/obsidianmd/pele/commit/db2f353efacf63d16a74d96a274f43b6ecce85a8) — Treemap: Fit text at the font size a class sets
  3. [`bbb00e45c006728ac233662a037803ea386af525`](https://github.com/obsidianmd/pele/commit/bbb00e45c006728ac233662a037803ea386af525) — Treemap: Name a box in a tooltip when its name is cut short or left out
  4. [`376956c0020ca0f53c8622283ee6c0cf9560a98e`](https://github.com/obsidianmd/pele/commit/376956c0020ca0f53c8622283ee6c0cf9560a98e) — Tests: Cover treemap boxes too small for their gap or header
  5. [`578877c1348cb6b22b9c69b3cf24b46437375b21`](https://github.com/obsidianmd/pele/commit/578877c1348cb6b22b9c69b3cf24b46437375b21) — Tests: Cover treemap text in the font size a class sets

- Then apply `e720e9cd17154c316d869136e15be2a385701656`, **Block: Keep labels on parallel edges separate**, from the owner-submitted upstream PR [obsidianmd/pele#92](https://github.com/obsidianmd/pele/pull/92). Its complete source patch is checked in at [`patches/0001-Block-Keep-labels-on-parallel-edges-separate.patch`](patches/0001-Block-Keep-labels-on-parallel-edges-separate.patch). It is based directly on 1.10.0 and applies unchanged after #91. The local combined assembly commit is `6586adf671c6aae1644ff30af1acb7baa9a5368f`. The owner submitted this fix from `jasoncarreira` as [obsidianmd/pele#92](https://github.com/obsidianmd/pele/pull/92).

The block patch changes only `src/diagrams/block/render.ts` and `tests/render/block.test.ts`. The #91 commits change only the treemap layout/render source and tests. `package.json` and `package-lock.json` remain unchanged from 1.10.0. The patch file carries author and commit provenance; no application-specific routing is added.

## Reproduce

Node 24.11.1, npm 11.6.2, macOS arm64; locked esbuild 0.28.2 and TypeScript 7.0.2.

```sh
git clone https://github.com/obsidianmd/pele pele-build
cd pele-build
git checkout --detach 578877c1348cb6b22b9c69b3cf24b46437375b21
git am /path/to/trellis/web/vendor/patches/0001-Block-Keep-labels-on-parallel-edges-separate.patch
npm ci --ignore-scripts
npm run typecheck
npm test -- --maxWorkers=2
npm run build
shasum -a 256 dist/pele.global.js
```

The vendored file is the unchanged resulting `dist/pele.global.js`: **711887 bytes**, SHA-256 **`bb4220ed66550152469e0e9a3afb6c532d5812ad62c45126234ed2ca42a0ccea`**. Two independent source checkouts with separate `npm ci --ignore-scripts` installs produced identical bytes. Verify with `shasum -a 256 web/vendor/pele-1.10.0-trellis.2.js`.

## Scope and validation

The native block patch separates overlapping measured labels on repeated/opposite edges sharing an unordered endpoint pair. It routes each affected edge through its final label center and avoids introducing neighboring-label collisions during pair packing. It preserves native IDs and deterministic rendering; route coordinates may change. It is not a general global label or node-obstacle solver. Four SVG units of lane clearance are new; existing eight-unit label background padding and 2,000,000-work allowance are unchanged. The conservative exhausted-budget placement can widen the SVG.

Failing-first upstream regressions cover vertical, horizontal multiline, narrow, repeated and composite cases. The isolated block suite passes 41 tests; the pure-base block patch full suite passes 6,884 tests (103 skipped, 2 todo). The assembled #91 plus block-patch full suite passes 6,888 tests (103 skipped, 2 todo), with typecheck and build passing. Five raw Chromium cases have zero final label overlaps and all 15 native label-center clicks select the intended edge. The unconstrained initial upstream full run hit two unrelated existing 3-second stress assertions; a serialized two-worker run passes without changing their deadlines.

Frozen fixtures for this exact build live in `tests/fixtures/pele-1.10.0-trellis.2/`. Existing Sequence and representative SVG output remains byte-identical to `.1`; the native block-label browser regression uses the exact vendored asset and real unforced clicks. Its isolated raw-SVG host is not a product CSP claim. Trellis's separate Sequence/browser adapter acceptance retains the real strict CSP. The asset is served locally, never from a CDN.
