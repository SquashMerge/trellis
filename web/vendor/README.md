# Pele browser asset

`pele-1.10.0-trellis.1.js` is Pele 1.10.0 plus the treemap fix from upstream PR [obsidianmd/pele#91](https://github.com/obsidianmd/pele/pull/91), built from source. MIT (see `pele-1.10.0-trellis.1.LICENSE`, the unchanged upstream `LICENSE`). Trellis vendors this patched build only until an upstream release includes #91 (owner decision 2026-10-25, Decision 0014 amendment); then it reverts to an unchanged upstream release.

- **Upstream base:** tag `1.10.0` of `https://github.com/obsidianmd/pele`, commit `4eff76f98c73b6817800a05773517f35832e4e92` ("Release: 1.10.0"), the base of #91. Building that commit unpatched with the commands below reproduces npm `pele` 1.10.0's `dist/pele.global.js` byte for byte (709352 bytes, SHA-256 `dd70e44152f7d70d62fedc622f588db3f5855859b444f4d8cf274925c12f2651`; npm tarball `https://registry.npmjs.org/pele/-/pele-1.10.0.tgz`, integrity `sha512-hDMEM+GQ01pZin8tst3PRXGeB/xFATvXBreHHtjx2glSNAQ7nHFq7EbMuNJGTbqkYgueWPboLjyUBsAudsb1qQ==`).
- **Patch:** exactly the five commits of obsidianmd/pele#91 (branch `treemap-realistic-size`, head `578877c1348cb6b22b9c69b3cf24b46437375b21`), on top of `4eff76f`:
  1. `e1aaa16` Treemap: Keep boxes too small for their gap or their section's header
  2. `db2f353` Treemap: Fit text at the font size a class sets
  3. `bbb00e4` Treemap: Name a box in a tooltip when its name is cut short or left out
  4. `376956c` Tests: Cover treemap boxes too small for their gap or header
  5. `578877c` Tests: Cover treemap text in the font size a class sets

  They change only `src/diagrams/treemap/{layout,render}.ts` and `tests/render/treemap.test.ts`; `package.json` and `package-lock.json` are unchanged from 1.10.0.
- **Toolchain:** Node 24.11.1, npm 11.6.2, from the 1.10.0 `package-lock.json` (esbuild 0.28.2, TypeScript 7.0.2), macOS arm64.
- **Build** (at `578877c`):

  ```sh
  npm ci --ignore-scripts
  npm run typecheck
  npm test        # 292 files passed, 1 skipped; 6882 tests passed
  npm run build   # node scripts/build.mjs
  ```

  `pele-1.10.0-trellis.1.js` is the resulting unchanged `dist/pele.global.js`: 709743 bytes, SHA-256 `3749dd9b9f97d6b239eedc0017a97a913c98dc270ad3ce059b3c96933effa072`. Verify with `shasum -a 256 web/vendor/pele-1.10.0-trellis.1.js`.

Frozen output fixtures for this exact build live in `tests/fixtures/pele-1.10.0-trellis.1/`. The bundle is served locally, never from a CDN.
