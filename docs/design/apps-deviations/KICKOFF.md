# Kickoff prompt for Claude Code

Unzip the packet into the repo as `docs/design/apps-deviations/`, start Claude Code in the trellis checkout, and paste:

---

Read `docs/design/apps-deviations/README.md` and `SCREENS.md`, and open `docs/design/apps-deviations/trellis-apps-deviations.html` in a browser. It's an interactive mockup of two Architecture-tab screens: Apps (#685) and the Deviations review. The screenshots are in `docs/design/apps-deviations/screens/`.

Then read `CLAUDE.md`, `AGENTS.md`, `DESIGN.md`, `docs/rules.md`, `docs/architecture-view.md`, and issues #685, #686, #725 and #769.

Don't write code yet. Report back with:

1. Where the mockup conflicts with a ratified contract or decision, and which side you think should change. Include the tooling chip and the per-file-pair exception note.
2. For each screen, which daemon routes, MCP tools and DTOs already exist and what's missing. In particular:
   - the app inventory and `[[apps]]` override;
   - per-app layer config;
   - the cross-app interface DTO (#686);
   - the referenced symbol and its declaration location on `rules_explain` evidence;
   - a Sequence entry point that highlights one `callId`.
3. Your proposed issue breakdown, starting from the README's "Suggested slices". Give each issue acceptance criteria in the repo's usual style. Mark the ones that depend on #686 or #769.

Constraints:

- Stack: vanilla JS in `web/`, no framework, no CDN. Fonts are served locally.
- Label and source text from the repository is untrusted; render it as text.
- Mockup values are illustrative; don't hard-code them.
- Don't build placeholder UI for capabilities that don't exist yet. That covers Open in Sequence and Cross-app sequence until their slices land.
