// The Architecture tab: the payload read, the drawing and its choices, presses on the drawing, the
// followed file's card and the layer rows. These are methods of the pane (main.js installs them on
// its class unchanged); the tab's other native parts are in architecture-ui.js.
import { div } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { theme } from "herdr";
import * as fs from "fs/promises";
import { architecturePayload, pinLabel, samePin } from "./client.js";
import { CONTROLS, ERROR, HEAT_INK, apps, draw, heat, heatFill, hit, plural, display } from "./diagram.js";
import { BUILDING_MS, MAX_BUILDING_MS, DRAW_TIMEOUT_MS, WIDTH_SLACK, PAD, HEAT_NAMES, yieldTo, drawTimeout, fail, basename, settingsMono } from "./common.js";

export class ArchitectureView {
  loadArchitecture(cx) {
    const rootKey = this.rootKey, serial = this.serial, pin = this.status?.pin, client = this.client, aggregate = this.aggregate, run = {};
    if (!pin) { this.view = null; this.viewError = null; this.archRun = null; this.loading = false; return; }
    this.devs = null;
    this.archRun = run;
    this.loading = true; this.view = null; this.viewError = null; this.building = null;
    const started = Date.now();
    // The building retries belong to this request too.
    const live = () => run === this.archRun && serial === this.serial && samePin(pin, this.status?.pin) && aggregate === this.aggregate;
    const attempt = (cx) => cx.spawn(async (cx) => {
      if (!live()) return;
      try {
        const data = await client.architecture(rootKey, pin, aggregate);
        if (!live()) return;
        const payload = architecturePayload(data, pin, aggregate);
        if (payload.state === "building") {
          this.building = payload.progress || {};
          const wait = Number.isSafeInteger(payload.retryAfterMs) && payload.retryAfterMs > 0 ? payload.retryAfterMs : BUILDING_MS;
          cx.timer.after(Math.min(wait * (Date.now() - started > 30000 ? 4 : 1), MAX_BUILDING_MS), (cx) => attempt(cx));
          return;
        }
        this.view = payload; this.building = null; this.loading = false; this.archRun = null;
      } catch (error) {
        if (!live()) return;
        this.view = null; this.viewError = fail(error); this.loading = false; this.archRun = null;
        if (this.viewError.code === "revision_conflict") this.refreshStatus(cx);
      } finally { cx.notify(); }
    });
    attempt(cx);
  }

  // --- the drawing ---

  // Redraw when the payload, the width (beyond the slack), the open bands, the theme or the
  // followed file's chip change. One drawing at a time; the newest inputs win.
  scheduleDraw(cx, highlight) {
    if (!this.view || this.view.state !== "ready" || this.width <= 0) return;
    const t = theme();
    const width = Math.max(320, Math.floor(this.width - 2 * PAD));
    const themeKey = ["background", "foreground", "muted", "input", "surface", "raised", "border", "accent"].map((k) => t[k]).join();
    const choice = this.choice();
    const key = JSON.stringify([this.view.indexGeneration, this.view.indexRevision, this.view.rulesFile?.contentHash, this.view.aggregate, [...this.open], [...this.drilled],
      this.unassignedOpen, choice.metric, choice.scope, themeKey, highlight]);
    // The last attempt's inputs, drawn or failed: a failure is not retried for the same inputs.
    const tried = this.tried && this.tried.key === key && Math.abs(this.tried.width - width) <= WIDTH_SLACK;
    if (tried || this.drawing) return;
    this.drawing = true; this.tried = { key, width };
    const view = this.view, serial = this.serial, run = {};
    this.drawRun = run;
    // A task the runtime ends runs no `finally`: the watchdog frees the slot and says so.
    const watchdog = cx.timer.after(DRAW_TIMEOUT_MS, (cx) => {
      if (this.drawRun !== run) return;
      this.drawRun = null; this.drawing = false;
      if (serial === this.serial && view === this.view) { this.drawn = null; this.drawError = drawTimeout("Architecture"); }
      cx.notify();
    });
    const live = () => this.drawRun === run && serial === this.serial && view === this.view;
    cx.spawn(async (cx) => {
      try {
        // The task's first run is inside render(), which has the shortest cap: leave it first.
        const pause = yieldTo(cx);
        await pause();
        if (!live()) return;
        const out = await draw(view, { width, theme: t, highlight, pause, choice });
        if (!live()) return;
        // One file per image (All apps stacked draws one per app); the extra files of an earlier drawing go.
        const assets = [];
        for (const [index, one] of out.images.entries()) assets.push(one ? await this.saveDrawing(`architecture-${index}`, one.svg) : null);
        for (const kind of Object.keys(this.files)) if (kind.startsWith("architecture-") && Number(kind.slice(13)) >= out.images.length) {
          const file = this.files[kind]; delete this.files[kind]; await fs.unlink(file).catch(() => {});
        }
        if (!live()) return;
        this.drawn = { ...out, view, assets, choice };
        this.drawError = null;
      } catch (error) {
        if (!live()) return;
        this.drawn = null; this.drawError = fail(error);
      } finally {
        watchdog.cancel();
        if (this.drawRun === run) { this.drawRun = null; this.drawing = false; }
        cx.notify();
      }
    });
  }

  // The display choices one drawing is made for.
  choice() {
    return { open: this.open, drilled: this.drilled, unassigned: this.unassignedOpen, metric: this.metric, aggregate: this.aggregate, scope: this.scope };
  }

  // Color by redraws the held payload; Aggregation reads the payload again, as the browser does.
  choose(kind, value, cx) {
    const options = kind === "metric" ? CONTROLS.metrics : CONTROLS.aggregates;
    this.menu = null;
    if (!options.some((option) => option.value === value)) return cx.notify();
    // A captured label carries the old metric's value: drop it, as the browser's re-render clears its inspection.
    if (kind === "metric" ? value !== this.metric : value !== this.aggregate) { if (this.inspect?.kind === "label") this.inspect = null; }
    if (kind === "metric") this.metric = value;
    else if (value !== this.aggregate) { this.aggregate = value; this.view = null; this.loadArchitecture(cx); }
    cx.notify();
  }

  // The app switcher: redraws the held snapshot and never reads (#789).
  setScope(name, cx) {
    if (name !== null && !apps.group(this.view, name)) return;
    this.scope = name; this.inspect = null; this.devs = null;
    cx.notify();
  }

  pinText() { return this.view ? pinLabel({ indexGeneration: this.view.indexGeneration, indexRevision: this.view.indexRevision }) : "no index"; }

  // The followed file in this payload: its layer, module chip and counts, from the summary.
  placement(file) {
    const view = this.view, summary = this.current()?.summary;
    if (!file?.path || !view || view.state !== "ready" || !summary) return null;
    const module = view.modules.find((m) => m.identity?.path === file.path && m.identity?.kind !== "externalPackage");
    if (!module) return { missing: true };
    const layer = module.layer === null ? null : view.layers.find((l) => l.name === module.layer) || null;
    const members = view.modules.filter((m) => m.layer === module.layer);
    const groups = summary.moduleGroups(members);
    const group = groups.find((g) => g.files.includes(module)) || null;
    // In All apps a band's stat chips count its own app's slice.
    const part = summary.plan.parts.find((p) => p.slice.modules.includes(module)) ?? summary.plan.parts[0];
    return { module, layer, group, rank: group ? groups.indexOf(group) : -1, groups: groups.length, members: members.length,
      hidden: layer ? !summary.layers.has(layer.name) : false, stats: layer ? summary.layerStats(layer, part.group?.name, part.slice) : [] };
  }

  // A press on the diagram (image `index`): a band opens or folds, as its header does in the
  // browser; a directory module opens one level; the unassigned row opens; an upward ↑ chip or a
  // deviating arrow (its label, line or head) opens the Deviations tab on its edge; an interface
  // stub inspects the interface; anything else is inspected by its full accessible label.
  press(event, index, cx) {
    const drawn = this.current(), at = event?.local_position;
    if (!drawn || !at || !drawn.images[index]) return;
    const box = hit(drawn.images[index].boxes, at.x, at.y);
    if (!box) return;
    const summary = drawn.summary, record = box.record;
    if (box.kind === "L") this.open.set(record.name, !summary.expanded(record));
    else if (box.kind === "D" && typeof record?.key === "string") { if (this.drilled.has(record.key)) this.drilled.delete(record.key); else this.drilled.add(record.key); }
    else if (box.kind === "U") this.unassignedOpen = !this.unassignedOpen;
    else if (box.kind === "A" && box.action[0] === "up") return this.openDeviations(`${box.action[1]}→${box.action[2]}`, cx);
    else if (box.kind === "A" && box.action[0] === "interface") this.inspect = { kind: "interface", key: apps.key({ client: box.action[1], server: box.action[2] }), client: box.action[1], server: box.action[2] };
    else if (box.kind === "E" && record?.role === "production" && record.status === "deviates" && !record.excepted) return this.openDeviations(`${record.sourceLayer}→${record.destinationLayer}`, cx);
    else this.inspect = { kind: "label", title: { E: "Arrow", M: "File", D: "Module", C: "Module", N: "Note", A: "Selected" }[box.kind] || "Selected", text: box.label || "" };
    cx.notify();
  }

  // The drawing of the payload shown now; a drawing of an earlier payload is never shown.
  current() { return this.drawn && this.drawn.view === this.view ? this.drawn : null; }

  fileCard(t, file, place) {
    const card = v_flex().flex_none().gap(6).p(10).rounded(8).border_1().border_color(t.accent).bg(t.surface)
      .child(h_flex().gap(8).items_center()
        .child(div().text_size(12).text_color(t.dim).child("Files selection"))
        .child(div().flex_1().min_w(0).truncate().font_family(settingsMono()).text_size(12).child(file.path ?? file.absolute)));
    if (!file.checkout) return card.child(this.note(t, "Not in a checkout this daemon serves."));
    if (file.checkout.rootKey !== this.rootKey) return card.child(this.note(t, `In ${basename(file.checkout.workspaceRoot)}, which is not available.`));
    if (!place) return card.child(this.note(t, "Waiting for the diagram…"));
    if (place.missing) return card.child(this.note(t, "Not an indexed source file at this revision."));
    const { module, layer, group } = place, summary = this.drawn.summary, metric = summary.metric;
    const value = metric === "layer" ? undefined : heat.metricValue(module.metrics, metric);
    const band = value === undefined ? null : summary.scale.band(value);
    card.child(h_flex().gap(6).items_center().flex_wrap()
      .child(div().text_color(t.dim).child("Layer"))
      .child(div().font_semibold().child(layer ? `${summary.layerTag(layer)} · ${layer.name}` : "Unassigned / unknown"))
      .when(Boolean(module.app), (el) => el.child(div().text_size(12).text_color(t.dim).child(`app ${module.app}`)))
      .when(place.hidden, (el) => el.child(div().text_size(12).text_color(t.dim).child(summary.tests.has(layer.name) ? "(tests/fixtures layer, not drawn)" : "(hidden layer, not drawn)")))
      .children(place.stats.map((stat) => this.stat(t, stat))));
    if (group) {
      const projections = this.drawn.images.flatMap((one) => (one ? [one.projection, ...(one.projections ?? [])] : [])).filter(Boolean);
      const drawnChip = place.rank >= 0 && projections.some((projection) => [...projection.nodes.values()].some((n) => n.kind === "C" && n.record.group === group.name && n.record.layerKey === (layer?.name ?? null)));
      card.child(h_flex().gap(6).items_center().flex_wrap()
        .child(div().text_color(t.dim).child("Module"))
        .child(this.chip(t, group.name, group.value === undefined ? null : summary.scale.band(group.value)))
        .child(div().text_color(t.muted).child(`${plural(group.files.length, "file")}${group.value === undefined ? "" : ` · ${metric === "maintainabilityIndex" ? "min" : "max"} ${metric} ${display(group.value)}`} · #${place.rank + 1} of ${place.groups} in the layer`))
        .child(div().text_size(12).text_color(t.dim).child(drawnChip ? "outlined in the diagram" : "not drawn as its own chip: in the band's “+N more” chip, or the band is open")));
    }
    card.child(h_flex().gap(6).items_center().flex_wrap()
      .child(div().text_color(t.dim).child("File"))
      .child(div().text_color(t.muted).child(module.declarationCount > 0
        ? `${metric === "layer" ? "layer membership" : `${metric} ${display(value)}`}${band === null ? "" : ` (${HEAT_NAMES[band]})`} · ${plural(module.declarationCount, "measured callable")}`
        : `no measured callables${module.excludedReason ? ` · ${module.excludedReason}` : ""}`)));
    return card;
  }

  // A heat chip in the web app's colours (web/style.css --heat-0..3), with ink text on heat.
  chip(t, text, band) {
    return div().px(6).py(1).rounded(4).bg(band > 0 ? heatFill(t, band) : t.raised).text_color(band > 0 ? HEAT_INK : t.foreground).child(text);
  }

  stat(t, stat) {
    const color = stat.tone === "error" ? ERROR : t.muted;
    return div().px(5).rounded(3).border_1().border_color(color).text_size(12).text_color(color).child(stat.text);
  }

  // The layer stack as rows, per app in All apps: order, name, files, max, outgoing counts. A row
  // opens or folds its band in the diagram, as clicking a band header does in the browser.
  layerList(t, place) {
    const view = this.view, summary = this.drawn.summary, out = [];
    for (const part of summary.plan.parts) {
      const slice = part.slice, name = part.group?.name;
      const ordered = slice.layers.filter((l) => !l.shared).sort((a, b) => a.order - b.order || a.name.localeCompare(b.name));
      const shared = slice.layers.filter((l) => l.shared).sort((a, b) => a.name.localeCompare(b.name));
      if (name) out.push(div().pt(4).font_semibold().text_size(13).child(name));
      out.push(...[...ordered, ...shared].map((layer) => {
        const members = slice.modules.filter((m) => m.layer === layer.name);
        const drawnLayer = part.layout.layers.has(layer.name), open = drawnLayer && summary.expanded(layer);
        const current = place?.layer?.name === layer.name, worst = summary.worst(members);
        return h_flex().id(`layer-${name ? `${name}-` : ""}${layer.name}`).min_h(30).px(8).py(4).gap(8).items_center().rounded(6).flex_wrap()
          .when(current, (el) => el.bg(t.selected))
          .when(drawnLayer, (el) => el.cursor_pointer().hover((s) => s.bg(t.hover)).on_click((_e, cx) => { this.open.set(layer.name, !open); cx.notify(); }))
          .child(div().w(14).text_color(t.dim).child(drawnLayer ? (open ? "▾" : "▸") : "·"))
          .child(div().w(52).text_color(t.dim).child(layer.shared ? "Shared" : summary.layerNumber(layer) === null ? "Hidden" : `Order ${summary.layerNumber(layer)}`))
          .child(div().font_semibold().text_color(drawnLayer ? t.foreground : t.dim).child(layer.name))
          .child(div().text_color(t.muted).child(`${plural(members.length, "file")}${worst === undefined ? "" : ` · ${summary.metric === "maintainabilityIndex" ? "min" : "max"} ${display(worst)}`}`))
          .when(!drawnLayer, (el) => el.child(div().text_size(12).text_color(t.dim).child(part.layout.tests.has(layer.name) ? "tests/fixtures · not drawn" : "hidden · not drawn")))
          .children(summary.layerStats(layer, name, slice).map((stat) => this.stat(t, stat)));
      }));
    }
    const unassigned = summary.plan.payload.modules.filter((m) => m.layer === null).length;
    return v_flex().id("layers").flex_none().gap(2)
      .child(h_flex().gap(8).items_center().pb(4)
        .child(div().font_semibold().text_size(14).child("Layers"))
        .child(div().text_size(12).text_color(t.dim).child(`${plural(summary.plan.payload.modules.length, "file module")} · rules ${String(view.rulesFile?.contentHash || "").slice(0, 12)}`)))
      .children(out)
      .child(h_flex().min_h(30).px(8).gap(8).items_center()
        .child(div().w(14)).child(div().w(52).text_color(t.dim).child("—"))
        .child(div().font_semibold().child("Unassigned")).child(div().text_color(t.muted).child(`${plural(unassigned, "file")} · no layer`)));
  }
}
