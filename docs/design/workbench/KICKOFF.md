# Kickoff prompt for Claude Code

The packet lives in the repo at `docs/design/workbench/`. Start Claude Code in the trellis checkout and paste:

---

Read `docs/design/workbench/README.md` and `SCREENS.md`, and open `docs/design/workbench/trellis-workbench.html` in a browser. It's an interactive mockup of the next Trellis web app. The screenshots are in `docs/design/workbench/screens/`.

Then read `CLAUDE.md`, `AGENTS.md`, `DESIGN.md`, `docs/design-refresh-contract.md`, `docs/terminal-workbench-contract.md`, and `SPEC.md` §9 and §11.

Don't write code yet. Report back with:

1. Where the mockup conflicts with a ratified contract or decision, and which side you think should change. (The brand palette/fonts are already amended in `DESIGN.md` and `docs/design-refresh-contract.md`; the heat scale is fixed per-metric thresholds per #395.)
2. For each screen in `SCREENS.md`, which daemon endpoints and DTOs already exist, and what's missing. In particular:
   - the metrics:v1 API
   - prod/test reference counts between modules
   - the layer map
   - per-checkout index state
3. Your proposed issue breakdown, starting from the README's "Suggested slices". Give each issue acceptance criteria in the repo's usual style. Mark the gated ones (Herdr, terminals, design handoff) as not scheduled.

Constraints:

- Stack: vanilla JS in `web/`, no framework, no CDN. Fonts are served locally.
- Pele is vendored (`web/vendor/pele-1.10.0-trellis.2.js`). Label text from the repository is untrusted.
- Mockup values are illustrative; don't hard-code them.
- Don't build placeholder UI for capabilities that don't exist.
