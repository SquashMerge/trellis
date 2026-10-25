# Pele browser asset

`pele-1.6.0-trellis.1.js` is Pele 1.6.0 plus the treemap fix from upstream PR [obsidianmd/pele#91](https://github.com/obsidianmd/pele/pull/91), built from source. MIT (see `pele-1.6.0-trellis.1.LICENSE`, the unchanged upstream `LICENSE`). Trellis vendors this patched build only until an upstream release includes #91 (owner decision 2026-10-25, Decision 0014 amendment); then it reverts to an unchanged upstream release.

- **Upstream base:** tag `1.6.0` of `https://github.com/obsidianmd/pele`, commit `659c3593ce43794dce7ec7ab45d424ad2de182a5` ("Release: 1.6.0"). Building that tag unpatched with the commands below reproduces npm `pele` 1.6.0's `dist/pele.global.js` byte for byte (682005 bytes, SHA-256 `b82d1bcd435302779a945e1e0f4a54ea56fb26e010ce3f2122f4216fcfc095d7`; npm tarball `https://registry.npmjs.org/pele/-/pele-1.6.0.tgz`, integrity `sha512-VfAiVItsOGlGhvnBfphdT28VCG7mXMZxQNOwAGsCtxI2ztbUsT39MraEOYjsapLxVETkdUZscm0DdotYDQMLdw==`).
- **Patch:** the five commits of obsidianmd/pele#91 (upstream on `4eff76f`, 1.10.0), cherry-picked onto `1.6.0` (`git cherry-pick -x`):
  1. `e1aaa16` Treemap: Keep boxes too small for their gap or their section's header
  2. `db2f353` Treemap: Fit text at the font size a class sets
  3. `bbb00e4` Treemap: Name a box in a tooltip when its name is cut short or left out
  4. `376956c` Tests: Cover treemap boxes too small for their gap or header
  5. `578877c` Tests: Cover treemap text in the font size a class sets

  Commits 2 and 3 conflicted only on the treemap box `<rect>` lines, where 1.6.0 still writes `rx="${RADIUS}"` and main uses `rounded(...)`; the backport keeps 1.6.0's corner form and adds the patch's changes. Otherwise the backported diff equals the upstream diff. Only `src/diagrams/treemap/{layout,render}.ts` and `tests/render/treemap.test.ts` change.
- **Toolchain:** Node 24.11.1, npm 11.6.2, from the 1.6.0 `package-lock.json` (esbuild 0.28.2, TypeScript 7.0.2, Vitest 4.1.11), macOS arm64.
- **Build:**

  ```sh
  npm ci --ignore-scripts
  npm run typecheck
  npm test        # 287 files passed, 1 skipped; 6693 tests passed
  npm run build   # node scripts/build.mjs
  ```

  `pele-1.6.0-trellis.1.js` is the resulting unchanged `dist/pele.global.js`: 682396 bytes, SHA-256 `b3c9f79da624252e49522fb4da1e1ee398389526c22e0e406090540717523204`. Verify with `shasum -a 256 web/vendor/pele-1.6.0-trellis.1.js`.

Frozen non-treemap output fixtures live in `tests/fixtures/pele-1.6.0/`; the patch leaves them byte-identical. The bundle is served locally, never from a CDN.
