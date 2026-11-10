# Screens

Each section lists the route in `trellis-apps-deviations.html`, the screenshots and the source board in `reference/`, then what the screen does. "Needs" is the data the implementation has to get from the daemon.

Both screens sit under **Diagrams → Architecture** and share the app header: checkout `repo ⎇ branch · rev`, and index state as a word plus a shape ("Fresh · rev 669eb851:43").

---

## Apps — `#Apps` · `screens/01–03` · `Apps.dc.html`

One Architecture tab, two levels: all apps side by side, or one app's own layer stack. This tab replaces today's single stack, where `web/` is just one more layer.

### Toolbar

- **App switcher** (segmented): All apps / web app · JS / daemon · Rust / tooling · hidden. The selected item is a purple fill with ink text.
- **Source line** next to it, saying where the apps came from: "2 apps · tooling and fixtures grouped aside", "web/ · package root", or "Cargo.toml [[bin]] trellis · src/".
- **Color by** (metric · aggregation) with the 4-step legend, low → high. It works the same as the existing Architecture view, with fixed bands per #395.

### All apps (`01-apps-all.png`)

- **One column per app:** a card with the app name, then "language · root · N files · max metric", plus a deviation count for apps that have rules.
- **Collapsed layer bands** inside each column: number · name · file count · up/down counts, the band's max metric, and the top 3–4 module chips colored by metric.
- **Interface edge.** One thick line from the owning band of the client app (web `4 · api client`) to the owning band of the server app (daemon `1 · entry`). The line ends at bands, not at app boxes.
- **Label card on the edge:**
  - "HTTP · 27 routes"
  - "34 call sites"
  - "wrapper-inferred"
  - "4 server-only" in gold
  
  The card is a button, and selecting it highlights the edge and both bands in lavender.
- **"Serves web/ · include_str!"**: a dashed neutral line from the daemon to the web app. This is a build relationship, not a call.
- **Grouped aside**, dashed boxes below the columns:
  - **tooling**: 119 files, hidden by default (#524)
  - **fixtures**: 116 files
  - **unassigned**: 186 files, gold border, "listed, never dropped"

### One app (`02-apps-web.png`, `03-apps-daemon.png`)

- **Title:** "<app> · ordered layers". Layer numbers restart for each app.
- **Full bands with metric tiles:** name, then "cognitive N". Each band shows its max on the right.
- **Badges on each band:**
  - `↓N` for downward references, neutral.
  - `↑N <layer>` for upward references, in the error color with a border. **Each upward badge links to Deviations filtered to that edge.**
- **Interface stub** on the owning band:
  - web `4 · api client`: "→ daemon · 27 routes · 34 call sites"
  - daemon `1 · entry`: "← web app · 27 routes"
  
  The stub selects the interface.
- **Shared** box (daemon only): its tiles, "any layer may use", and its outbound count.
- **Proposed layers.** When an app's layers aren't in `rules.toml` yet, a dashed badge says "proposed · not in rules.toml yet". The web app is drawn like that.
- **Function-level layer.** When one file straddles two layers (`app.js` is both shell state and API client), the client layer is drawn around the functions (`api()`, `selectedUrl()`), and the file's tile is dashed lavender.

### Inspector

**Interface selected** (the default in All apps):

- Title "web app → daemon", plus "HTTP · path and method match · structural, not semantic proof".
- Counts: matched routes 27, client call sites 34, server-only routes 4 (gold), client-only calls 0.
- **How a call travels**, as 5 numbered steps:
  1. A view calls the injected `request`.
  2. `app.js api()` → `selectedUrl()` adds `/api/checkouts/{root}/`.
  3. The daemon's catch-all route receives it.
  4. `BrowserCoreRoute::parse`.
  5. `provisioned_core_answer` handles it.
- **Server-only routes** list, each with its reason (`#516`, "no caller").
- **Wrappers** recognised: `api`, `request`, `api.request`, `owner.request`.
- **Open endpoints** (primary) and **Cross-app sequence**. The second is #686; hide it until then.

**App selected** (from its column or the switcher):

- Files, layers, calls to the other app, deviations (error color), unassigned (gold).
- One sentence on how this app's layers were derived.

The footnote, always shown: apps come from build manifests and can be overridden with `[[apps]]` in rules.toml. Counts are reference sites at the pin; "≥" means a lower bound.

### Interactions

- The switcher changes the level.
- The interface card or a stub selects the interface.
- An app column header selects that app.
- An upward badge opens Deviations filtered to that edge.
- The basis (pin + rules hash) is shared with Deviations, so going there and back doesn't re-read unless the basis changed.

### Needs

- **App inventory:** name, language, root, source (manifest or override), file count.
- **Per-app layer config:** order, paths, and whether the layers are proposed or configured.
- **Per-band data:** file count, max metric by the chosen metric and aggregation (`metrics:v1`), top modules.
- **Layer-pair reference counts** within an app, up and down, production only.
- **Cross-app interface DTO** (#686): matched routes, client call sites, server-only and client-only lists, and the recognised wrappers. Each entry is anchored to its client call site and its server route.
- **Grouped-aside counts:** tooling, fixtures, unassigned.

---

## Deviations — `#Findings` · `screens/04–07` · `Findings.dc.html`

Answers "which files break the layering, which lines do it, and take me there".

### Header

- "← daemon · Rust" back link, title **Deviations**, and a live summary: "31 files · 746 upward references · 16 calls listed".
- One line of help: "Expand a file to see the functions and the exact lines. Select a line for its actions, or right-click it."

### Filters

- **Edge chips** (segmented): All upward · 746 / storage → indexing · 445 / storage → query · 280 / indexing → entry · 12 / storage → entry · 6. These come from the layer-pair counts. Arriving from an Apps badge preselects that chip.
- **Calls only**: hides type uses, functions with no calls, and files with no calls.
- "Sorted by references · production only." Test-only references never appear here.

### Tree (`role="tree"`)

| Column | Content |
| --- | --- |
| File · function · line | caret, name, kind tag |
| Reaches up | file rows: `▲ <layer> N` per edge, error color. Line rows: `→ <definition location>` |
| Calls | count of `call` lines, error color; "—" when none |
| References | total for the current filter |
| (actions) | `⋯` button on line rows |

- **File row:** bold mono path.
  - Can show several edges at once ("▲ indexing 403 · ▲ query 134").
  - Carries a flag chip when it applies, e.g. "◆ test-support": the file is behind `cfg(feature = "test-support")`, which the rust-analyzer import enables.
  - Ends with "+ N more functions · M type uses" when truncated.
- **Function row:** the caller's name from `stableLookupKey`, its call count and reference count. Ends with "+ N more lines" when it has more than are listed.
- **Line row:** `L<line>  <referenced symbol>`, with a kind tag:
  - **call** (error border): evidence with a `callId`, meaning it runs code in the higher layer.
  - **type use** (neutral): a binding without a call, i.e. a signature, field or constructor.
  - **module import**: structural evidence with no caller (`callerReason: syntax_only`). It's listed under "(module)".
- **Order:** files by references, descending. Inside a file, functions with calls come first.
- **Expansion state** is kept per basis.

### Selected line panel (right)

- **Kind label:** "Call · runs code in a higher layer" or "Type use · names a type from a higher layer".
- **Location:** `<path>:<line>` and "in `<function>`".
- **The source line** from the cached source at the index revision, shown as text.
- **Details:**
  - Refers to (symbol)
  - Defined at (path:line)
  - Layers (▲ from → to)
  - File pair
  - Evidence ("rust-analyzer · exact binding", or "structural · no caller")
- **Flag box** (gold, dashed) when the file carries a flag, explaining why it counts as production.
- **Actions:**
  - **Open in Sequence** (primary, calls only): opens the caller function's sequence with this call marked.
  - **Go to call site**: Files view at that line, at the index revision. It's the primary action for a type use.
  - **Go to definition**.
  - **Add exception for this file pair…** (secondary), with the note "Exceptions are per file pair in rules.toml, so this covers N listed lines in A → B."

### Line menu (`06-deviations-line-menu.png`)

- **Opened by:** the `⋯` button, or right-click on a line row. The two give the same menu, and `⋯` keeps it reachable from the keyboard.
- **Items:** Open in Sequence (calls only), Go to call site, Go to definition, Copy location, Add exception for file pair…
- **Escape** closes it and returns focus to `⋯`.

### Keyboard

- Tree navigation follows the ARIA tree pattern: ↑/↓ moves between rows, → expands, ← collapses or moves to the parent.
- Enter on a line selects it.
- Shift+F10 or the context-menu key opens the line menu.

### States

- **Loading:** the existing pinned-basis loading pattern.
- **No deviations for this filter:** "No upward references on this edge at rev N."
- **Unknown instances** (`status: unknown`) aren't deviations. Show them as a count with a link to the existing "Inspect same-layer and unassigned references" view.
- **Basis changed** (409 on read): "The index moved to rev N. Reload deviations."

### Needs

- **Layer-pair counts** for the edge chips (the same source as the Apps badges).
- **`rules_check` instances** for `$layers` with status `deviates`, production only: target and destination identities, reference count, and `omittedEvidence`.
- **`rules_explain` per instance**, up to 50 evidence entries, paged by `nextOffset`. For each entry:
  - caller (`name`, `stableLookupKey`, `syntaxId`), or `null` with `callerReason`;
  - range and `revisionId`;
  - `callId` (which decides call vs type use);
  - evidence kind and freshness.
- **The referenced symbol and its declaration location** for each evidence entry. These aren't in `rules_explain` today; take them from the binding's target.
- **The cached source line** at the index revision (existing Files API).
- **A Sequence view entry point** that takes a caller symbol plus a `callId` to highlight (new; slice 5).
- **Exception write** through the existing `architecture/rules/preview` and `architecture/rules/apply` routes.

### Known gaps in the mockup

- A filter can hide the selected line while the panel still shows it (`07-deviations-filtered.png`). It should select the first visible line, or say "Selected line is hidden by this filter".
- "+ 1 more files" should read "+ 1 more file".
- On Apps, the "serves web/ · include_str!" label is clipped against the daemon column.
- Line numbers in "Defined at" and the code text are placeholders.
- The `⋯` menu items are links in the mockup. In the product they are buttons with `role="menuitem"`.
