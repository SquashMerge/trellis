// The Architecture tab's native parts (#886), as the web app shows them: the Color by and
// Aggregation controls, the app switcher and source line, the All apps column headers and HTTP
// interface cards, the heat legend, the inspector ("Architecture review") and the Deviations
// tree. Every number and sentence comes from the browser's own modules (diagram.js bundles
// them); this file only lays them out as Herdr rows. `pane` is the Trellis view.
import { div, image } from "gpui-kit";
import { h_flex, v_flex } from "gpui-base";
import { CONTROLS, ERROR, WARNING, apps, deviationModel, heatFill, plural } from "./diagram.js";

const MAX_TREE_ROWS = 300, MAX_LISTED = 150;
const TAGS = { call: "call", type: "type use", import: "import", reference: "reference" };
const num = (value) => value.toLocaleString("en-US");
const basename = (path) => path.slice(path.lastIndexOf("/") + 1);
const tone = (t, name) => (name === "error" ? ERROR : name === "warning" ? WARNING : t.muted);

const button = (t, id, text, onClick, { pressed = false, color = null } = {}) =>
  div().id(id).px(8).h(24).flex().flex_none().items_center().rounded(6).border_1().cursor_pointer().text_size(12)
    .border_color(pressed ? t.accent : t.input || t.border).text_color(color ?? (pressed ? t.foreground : t.muted))
    .when(pressed, (el) => el.bg(t.selected)).hover((s) => s.bg(t.hover)).on_click(onClick).child(text);

/** Color by and Aggregation: the web toolbar's selects, as a choice that opens a row of options. */
export function controls(pane, t) {
  const pick = (kind, options, value, label) => {
    const current = options.find((option) => option.value === value) ?? options[0];
    return [div().text_size(12).text_color(t.dim).child(label),
      button(t, `control-${kind}`, `${current.label} ▾`, (_e, cx) => { pane.menu = pane.menu === kind ? null : kind; cx.notify(); }, { pressed: pane.menu === kind })];
  };
  const row = h_flex().flex_none().gap(6).items_center().flex_wrap()
    .children(pick("metric", CONTROLS.metrics, pane.metric, "Color by"))
    .children(pick("aggregate", CONTROLS.aggregates, pane.aggregate, "Aggregation"));
  if (!pane.menu) return [row];
  const options = pane.menu === "metric" ? CONTROLS.metrics : CONTROLS.aggregates, value = pane.menu === "metric" ? pane.metric : pane.aggregate;
  return [row, h_flex().id(`menu-${pane.menu}`).flex_none().gap(4).flex_wrap().children(options.map((option) =>
    button(t, `${pane.menu}-${option.value}`, option.label, (_e, cx) => pane.choose(pane.menu, option.value, cx), { pressed: option.value === value })))];
}

/** The switcher (All apps, then one per app) and its source line, shown with two or more apps. */
export function switcher(pane, t, summary, view) {
  if (!summary.choices.length) return [];
  const group = summary.group;
  const out = [h_flex().id("app-switcher").flex_none().gap(4).flex_wrap().children(summary.choices.map((choice, index) =>
    button(t, `app-${index}`, choice.text, (_e, cx) => pane.setScope(choice.name, cx), { pressed: summary.scope === choice.name })))];
  out.push(div().id("app-source-line").flex_none().text_size(12).text_color(t.dim).whitespace_normal().child(apps.sourceLine(group, view.apps.length)));
  if (group?.duplicate) out.push(div().flex_none().text_size(12).text_color(WARNING).whitespace_normal().child(apps.duplicateNotice(group)));
  return out;
}

// An All apps column header: name, languages · roots · files · max, and its deviations in the
// error tone. Pressing it inspects the app.
function columnHeader(pane, t, part, width) {
  const info = pane.drawn.summary.columnHeader(part);
  return v_flex().id(`app-header-${part.group.name}`).flex_none().w(width).gap(2).p(8).rounded(8).border_1().cursor_pointer()
    .border_color(pane.inspect?.kind === "app" && pane.inspect.name === part.group.name ? t.accent : t.border).bg(t.surface).hover((s) => s.bg(t.hover))
    .on_click((_e, cx) => { pane.inspect = { kind: "app", name: part.group.name }; cx.notify(); })
    .child(div().font_semibold().child(info.name))
    .child(h_flex().gap(0).flex_wrap().text_size(12)
      .child(div().text_color(t.muted).child(info.deviations ? `${info.detail} · ` : info.detail))
      .when(Boolean(info.deviations), (el) => el.child(div().id(`app-deviations-${part.group.name}`).text_color(ERROR).child(info.deviations))))
    .when(Boolean(info.manifests), (el) => el.child(div().text_size(12).text_color(t.dim).child(`Detected from ${info.manifests}`)));
}

// An HTTP interface card: the DTO's counts, the ends no line shows, and a press inspects it.
function card(pane, t, item) {
  const selected = pane.inspect?.kind === "interface" && pane.inspect.key === item.key;
  return v_flex().id(`interface-card-${item.entry.client}-${item.entry.server}`).flex_none().max_w(360).gap(2).p(8).rounded(8).border_1().cursor_pointer()
    .border_color(selected ? t.accent : t.border).bg(t.surface).hover((s) => s.bg(t.hover))
    .on_click((_e, cx) => { pane.inspect = { kind: "interface", key: item.key, client: item.entry.client, server: item.entry.server }; cx.notify(); })
    // Listed under the drawing, a card names its interface: the browser's gutter position does that there.
    .child(div().id(`interface-card-name-${item.entry.client}-${item.entry.server}`).text_size(12).font_semibold().child(`${item.entry.client} → ${item.entry.server}`))
    .children(item.lines.map((line) => div().text_size(line.strong ? 13 : 12).when(Boolean(line.strong), (el) => el.font_semibold())
      .text_color(line.tone ? tone(t, line.tone) : t.foreground).child(line.text)))
    .children(item.ends.map((text) => div().text_size(12).text_color(t.dim).whitespace_normal().child(text)));
}

const unassignedBox = (t, id, name, count) => h_flex().id(id).flex_none().gap(6).px(8).py(4).rounded(8).border_1().border_color(WARNING)
  .child(div().text_size(12).font_semibold().child(name)).child(div().text_size(12).text_color(WARNING).child(`◇ ${count} unassigned · listed, never dropped`));

/** The drawing: one stack, All apps side by side (headers, image, cards) or stacked per app. */
export function drawing(pane, t, drawn) {
  const { summary, images } = drawn, plan = summary.plan, view = drawn.view, out = [];
  const picture = (index) => {
    const one = images[index];
    return div().id(index === 0 ? "diagram" : `diagram-${index}`).flex_none().w(one.width).h(one.height).cursor_pointer()
      .on_mouse_down("left", (event, cx) => pane.press(event, index, cx))
      .child(image(drawn.assets[index]).w(one.width).h(one.height));
  };
  if (!plan.allApps) { out.push(picture(0)); return out; }
  const parts = plan.parts, cards = drawn.cards.map((item) => card(pane, t, item));
  if (drawn.sideBySide) {
    // Never stack; scroll (#949/#950, owner ruling 2026-11-12): the pane has no drawer, so the columns stay side by
    // side and the headers and the drawing scroll sideways together when they are wider than the pane. Each gutter
    // has its own width (#949), so a spacer of that width sits between two headers.
    out.push(div().id("app-scroll").w_full().flex_none().overflow_x_scrollbar()
      .child(v_flex().flex_none().w(drawn.layout.width).gap(12)
        .child(h_flex().id("app-columns").flex_none().items_start()
          .children(parts.flatMap((part, index) => [...(index ? [div().flex_none().w(drawn.layout.gutters[index - 1])] : []), columnHeader(pane, t, part, drawn.layout.widths[index])])))
        .child(picture(0))));
    // The browser floats a card in the gutter between its two columns; the pane lists them under the drawing.
    if (cards.length) out.push(h_flex().id("interface-cards").flex_none().gap(8).flex_wrap().items_start().children(cards));
    parts.forEach((part, index) => { if (!part.slice.layers.some((l) => !l.shared)) out.push(unassignedBox(t, `app-unlayered-${index}`, part.group.name, part.group.unassignedFileCount)); });
  } else {
    parts.forEach((part, index) => {
      out.push(columnHeader(pane, t, part, Math.min(drawn.layout.width, Math.max(320, pane.width - 24))));
      if (images[index]) out.push(picture(index));
      else out.push(unassignedBox(t, `app-unlayered-${index}`, part.group.name, part.group.unassignedFileCount));
      // Stacked, a card follows its client's column, as in the browser.
      drawn.cards.forEach((item, at) => { if (item.client === index) out.push(cards[at]); });
    });
  }
  if (!drawn.cards.length) {
    const names = parts.map((part) => part.group.name), last = names.pop();
    out.push(div().flex_none().text_size(12).text_color(t.dim).child(`No HTTP interface measured between ${names.length ? `${names.join(", ")} and ` : ""}${last}. This does not prove absence.`));
  }
  const aside = [...parts.filter((part) => part.group.unassignedFileCount > 0 && part.slice.layers.some((l) => !l.shared))
    .map((part) => unassignedBox(t, `unassigned-${part.group.name}`, part.group.name, part.group.unassignedFileCount))];
  if (view.noApp?.unassignedFileCount > 0) aside.push(unassignedBox(t, "unassigned-no-app", "no app", view.noApp.unassignedFileCount));
  if (aside.length) out.push(h_flex().flex_none().gap(8).flex_wrap().children(aside));
  return out;
}

/** An app with no ordered layers: the setup help and its unassigned files (AC13). */
export function noLayers(t, summary, view) {
  const group = summary.group, files = view.modules.filter((m) => m.layer === null && m.app === group.name);
  return [div().flex_none().text_color(t.muted).whitespace_normal().child(`${group.name} has no layers yet. Its files are listed as unassigned below. Review .trellis/rules.toml to add layers with app = "${group.name}".`),
    v_flex().flex_none().gap(2).children(files.slice(0, MAX_LISTED).map((m) => div().text_size(12).font_family("JetBrains Mono").child(m.identity.path)))
      .when(files.length > MAX_LISTED, (el) => el.child(div().text_size(12).text_color(t.dim).child(`${plural(files.length - MAX_LISTED, "file")} not listed here`)))];
}

/** The heat legend: the scale's words and one swatch per band, then the coverage sentence. */
export function legend(t, summary, view) {
  const swatches = summary.metric === "layer" ? [] : summary.scale.entries.map(({ label, band }) => h_flex().gap(4).items_center()
    .child(div().w(12).h(12).rounded(2).bg(band === null ? t.raised : heatFill(t, band))).child(div().text_size(12).text_color(t.muted).child(label)));
  return [v_flex().id("legend").flex_none().gap(4)
    .child(div().text_size(12).text_color(t.muted).whitespace_normal().child(summary.legendText(summary.scale)))
    .when(swatches.length > 0, (el) => el.child(h_flex().gap(10).flex_wrap().children(swatches)))
    .child(div().text_size(12).text_color(t.dim).whitespace_normal().child(summary.coverageSummary(view)))];
}

const factsGrid = (t, rows) => v_flex().gap(2).children(rows.map(([name, value, rowTone]) => h_flex().gap(8)
  .child(div().w(160).flex_none().text_size(12).text_color(t.dim).child(name)).child(div().text_size(12).text_color(rowTone ? tone(t, rowTone) : t.foreground).child(value))));

/**
 * The inspector, below the drawing (the panel is too narrow for the browser's side column):
 * "Architecture review" with its counted review chips, or the selected app, interface or
 * control. Deviations open the Deviations tree.
 */
export function inspector(pane, t, summary, view) {
  const counts = summary.reviewCounts, chips = [];
  if (counts.deviates !== null) chips.push(button(t, "review-deviations", `${num(counts.deviates)} ${counts.deviates === 1 ? "deviation" : "deviations"}`,
    (_e, cx) => pane.openDeviations(null, cx), { color: counts.deviates ? ERROR : null }));
  if (counts.unassigned !== null) chips.push(div().id("review-unassigned").px(8).h(24).flex().items_center().rounded(6).border_1().border_color(t.border).text_size(12)
    .text_color(t.muted).child(`${num(counts.unassigned)} ${counts.unassigned === 1 ? "unassigned file" : "unassigned files"}`));
  if (counts.exceptions !== null) chips.push(div().id("review-exceptions").px(8).h(24).flex().items_center().rounded(6).border_1().border_color(t.border).text_size(12)
    .text_color(t.muted).child(`${num(counts.exceptions)} ${counts.exceptions === 1 ? "exception" : "exceptions"}`));
  const box = v_flex().id("inspector").flex_none().gap(6).p(10).rounded(8).border_1().border_color(t.border).bg(t.surface);
  const inspect = pane.inspect;
  const close = div().id("inspector-close").text_color(t.dim).cursor_pointer().child("×").on_click((_e, cx) => { pane.inspect = null; cx.notify(); });
  const title = (text) => h_flex().gap(8).items_center().child(div().flex_1().min_w(0).font_semibold().text_size(14).child(text)).when(Boolean(inspect), (el) => el.child(close));
  if (inspect?.kind === "app" && apps.group(view, inspect.name)) {
    const facts = apps.appFacts(apps.group(view, inspect.name), view);
    box.child(title(facts.title)).child(h_flex().gap(6).flex_wrap().children(chips))
      .child(div().text_size(12).text_color(t.dim).child(facts.detail)).child(factsGrid(t, facts.rows))
      .when(Boolean(facts.origin), (el) => el.child(div().text_size(12).text_color(t.dim).whitespace_normal().child(facts.origin)))
      .when(Boolean(facts.duplicate), (el) => el.child(div().text_size(12).text_color(WARNING).whitespace_normal().child(facts.duplicate)))
      .when(!facts.layered, (el) => el.child(div().text_size(12).whitespace_normal().child(`${facts.title} has no layers yet. Review .trellis/rules.toml to add layers with app = "${facts.title}".`)))
      .children(facts.interfaces.map((entry) => button(t, `inspect-interface-${entry.client}-${entry.server}`, `HTTP: ${entry.client} → ${entry.server}`,
        (_e, cx) => { pane.inspect = { kind: "interface", key: apps.key(entry), client: entry.client, server: entry.server }; cx.notify(); })));
  } else if (inspect?.kind === "interface" && apps.link(view, inspect.client, inspect.server)) {
    const facts = apps.interfaceFacts(apps.link(view, inspect.client, inspect.server));
    box.child(title(facts.title)).child(h_flex().gap(6).flex_wrap().children(chips))
      .child(div().text_size(12).text_color(t.dim).child(facts.basis))
      .children(facts.ends.map((text) => div().text_size(12).child(text))).child(factsGrid(t, facts.rows))
      .when(Boolean(facts.wrappers), (el) => el.child(div().text_size(12).text_color(t.dim).font_family("JetBrains Mono").whitespace_normal().child(facts.wrappers)))
      .child(div().text_size(12).text_color(t.dim).child("The routes and call sites are listed in the browser's inspector."));
  } else if (inspect?.kind === "label") {
    box.child(title(inspect.title)).child(h_flex().gap(6).flex_wrap().children(chips))
      .child(div().whitespace_normal().child(inspect.text.replace(/\n/g, " · ")));
  } else {
    box.child(title("Architecture review")).child(h_flex().gap(6).flex_wrap().children(chips))
      .child(div().text_size(12).text_color(t.dim).child("Select a band, module chip or arrow to inspect its evidence."));
  }
  if (summary.choices.length) box.child(div().id("apps-footnote").text_size(12).text_color(t.dim).whitespace_normal().child(apps.footnote(pane.pinText())));
  return [box];
}

/** The Deviations tree (#786, #845): the browser's own model, as native rows. */
export function deviations(pane, t, summary, view) {
  const devs = pane.devs, s = devs;
  const back = button(t, "deviations-back", summary.group ? `← ${summary.group.name}` : "← Architecture", (_e, cx) => { pane.devs = null; cx.notify(); });
  const head = h_flex().flex_none().gap(8).items_center().child(back).child(div().font_semibold().text_size(14).child("Deviations"))
    .child(div().flex_1().min_w(0).truncate().text_size(12).text_color(t.dim).child(devs.tree ? `${plural(devs.tree.totals.files, "file")} · ${plural(devs.tree.totals.references, "deviating reference")} · ${plural(devs.tree.totals.calls, "call")}` : ""));
  const out = [head];
  if (devs.error) return [...out, pane.problem(t, devs.error, "Deviations")];
  if (!devs.base) return [...out, pane.note(t, devs.progress || `Reading deviations at ${pane.pinText()}…`)];
  const tree = pane.deviationView();
  const chips = deviationModel.chips(tree, summary.edgeText);
  const upward = chips.edges.every((entry) => entry.direction === "up");
  const chip = (key, label, cross, entry) => button(t, `deviation-chip-${key ?? "all"}`, `${cross ? "⇄ " : ""}${label} · ${num(entry.count)}`,
    (_e, cx) => { s.edge = key; s.tree = deviationModel.filter(s.base, { edge: key, callsOnly: false }); cx.notify(); }, { pressed: s.edge === key });
  out.push(h_flex().id("deviation-chips").flex_none().gap(4).flex_wrap()
    .child(chip(null, upward ? "All upward" : "All deviations", false, chips.all))
    .children(chips.edges.map((entry) => chip(entry.key, `${entry.sourceLayer} → ${entry.destinationLayer}`, entry.direction !== "up", entry))));
  const unknown = tree?.rules?.counts?.unknown;
  if (Number.isSafeInteger(unknown) && unknown > 0) out.push(div().flex_none().text_size(12).text_color(t.dim).child(unknown === 1 ? "1 unknown rule instance is not a deviation." : `${num(unknown)} unknown rule instances are not deviations.`));
  if (!devs.base.rows.length) return [...out, pane.note(t, `No production deviations at ${pane.pinText()}.`)];
  if (!devs.tree.files.length) return [...out, pane.note(t, `No deviations on this edge at ${pane.pinText()}.`)];
  const rows = [];
  const toggle = (key) => (_e, cx) => { if (s.expanded.has(key)) s.expanded.delete(key); else s.expanded.add(key); cx.notify(); };
  const count = (value, bounded) => (value > 0 ? `${bounded ? "≥" : ""}${num(value)}` : "—");
  const row = (id, depth, onClick) => h_flex().id(id).min_h(24).pl(6 + depth * 14).pr(6).gap(8).items_center().rounded(4).when(Boolean(onClick), (el) => el.cursor_pointer().hover((st) => st.bg(t.hover)).on_click(onClick));
  for (const file of devs.tree.files) {
    if (rows.length >= MAX_TREE_ROWS) break;
    const open = s.expanded.has(file.key);
    rows.push(row(`deviation-file-${rows.length}`, 0, toggle(file.key))
      .child(div().w(12).text_color(t.dim).child(open ? "▾" : "▸"))
      .child(div().flex_1().min_w(0).truncate().font_family("JetBrains Mono").text_size(12).child(file.path))
      .children(file.edges.map((edge) => div().flex_none().text_size(12).text_color(ERROR).child(`${edge.direction === "up" ? "▲" : "⇄"} ${edge.destinationLayer} ${num(edge.count)}`)))
      .child(div().w(40).flex_none().text_size(12).text_color(ERROR).child(count(file.calls, false)))
      .child(div().w(40).flex_none().text_size(12).text_color(t.muted).child(count(file.shown, false))));
    if (!open) continue;
    for (const fn of file.functions) {
      if (rows.length >= MAX_TREE_ROWS) break;
      const fnOpen = s.expanded.has(fn.id);
      rows.push(row(`deviation-fn-${rows.length}`, 1, toggle(fn.id))
        .child(div().w(12).text_color(t.dim).child(fnOpen ? "▾" : "▸"))
        .child(div().flex_1().min_w(0).truncate().font_family("JetBrains Mono").text_size(12).child(fn.label))
        .child(div().w(40).flex_none().text_size(12).text_color(ERROR).child(count(fn.calls, !fn.callsExact)))
        .child(div().w(40).flex_none().text_size(12).text_color(t.muted).child(count(fn.references, !fn.refsExact))));
      if (fnOpen) for (const line of fn.lines) {
        if (rows.length >= MAX_TREE_ROWS) break;
        const e = line.evidence, target = e.target;
        rows.push(row(`deviation-line-${rows.length}`, 2, null)
          .child(div().w(44).flex_none().text_size(12).text_color(t.dim).child(`L${e.lines?.start ?? "—"}`))
          .child(div().flex_1().min_w(0).truncate().font_family("JetBrains Mono").text_size(12).child(line.symbol))
          .child(div().flex_none().px(4).rounded(3).border_1().border_color(t.border).text_size(11).text_color(t.muted).child(TAGS[line.kind] ?? line.kind))
          .child(div().flex_none().text_size(12).text_color(t.dim).child(target ? `→ ${basename(target.document.path)}:${target.lines.start}` : `→ ${basename(line.instance.destination.path ?? line.instance.destination.package ?? "")} · file only`)));
      }
    }
    const u = file.unlisted, more = Object.values(u).reduce((sum, value) => sum + value, 0);
    if (open && more > 0) rows.push(row(`deviation-more-${rows.length}`, 1, null).child(div().text_size(12).text_color(t.dim).child(`+ ${num(more)} more not listed`)));
  }
  out.push(v_flex().id("deviation-tree").flex_none().gap(1)
    .child(h_flex().px(6).gap(8).text_size(12).text_color(t.dim).child(div().flex_1().child("File · function · line")).child(div().w(40).child("calls")).child(div().w(40).child("refs")))
    .children(rows)
    .when(rows.length >= MAX_TREE_ROWS, (el) => el.child(pane.note(t, `First ${MAX_TREE_ROWS} rows shown.`))));
  out.push(div().flex_none().text_size(12).text_color(t.dim).whitespace_normal().child("Lines are listed up to a sample per file pair, calls first. File totals are exact; ≥ marks a function whose file pair has more lines than listed."));
  return out;
}

