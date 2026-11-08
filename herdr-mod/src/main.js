// Trellis beside the coding agents (#707): the daemon's Architecture view as a Herdr pane.
//
// Shell: a checkout selector (the daemon's checkout list), index status (revision, fresh or
// catching up) and a typed error state that never shows partial data. View: the Architecture
// layer diagram, drawn by the browser's own adapter and Pele (src/diagram.js) into an SVG under
// this mod's `.render/` folder and shown with image(); the layer list and counts beside it as
// native rows. It follows the file the `trellis-files` panel selects (shared
// "trellis.selectedFile") to that file's layer and module chip.
//
// Props (all optional): { url: "http://127.0.0.1:8877", tokenFile: "~/.config/trellis/serve.token",
// checkout: "<root key>" }. Open beside a terminal: pane.split {direction: "right", mod: "trellis"}.
import { View, div, image } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { appHome, focused, props, shared, spaces, theme } from "herdr";
import * as fs from "fs/promises";
import { TrellisError, architecturePayload, createClient, indexLabel, pinOf, samePin, validRootKey } from "./client.js";
import { draw, heat, hit, plural, display } from "./diagram.js";

const STATUS_MS = 5000, LIST_MS = 30000, BUILDING_MS = 250, MAX_BUILDING_MS = 5000;
// Re-lay out only for a width change worth a new drawing (the browser's RESIZE_SLACK).
const WIDTH_SLACK = 8, PAD = 12;
const SELECTED_FILE = "trellis.selectedFile";
const HEAT_NAMES = ["low", "moderate", "high", "very high"];

const fail = (error) => (error instanceof TrellisError ? error : new TrellisError(error?.code || "unexpected", error?.message || String(error)));
const basename = (path) => path.replace(/\/+$/, "").split("/").pop() || path;
// FNV-1a: a short name for one drawing's file.
function hash(text) {
  let h = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) { h ^= text.charCodeAt(i); h = Math.imul(h, 0x01000193) >>> 0; }
  return h.toString(16).padStart(8, "0");
}

export default class Trellis extends View {
  init(_props, cx) {
    const p = props() || {};
    this.copy = Math.random().toString(36).slice(2, 10);
    this.open = new Map();
    this.width = 0;
    this.listOpen = false;
    try { this.client = createClient({ url: p.url, tokenFile: p.tokenFile }); }
    catch (error) { this.fatal = fail(error); return; }
    this.wanted = validRootKey(p.checkout) ? p.checkout : (() => { try { return localStorage.getItem("trellis.checkout"); } catch { return null; } })();
    this.loadCheckouts(cx);
    cx.timer.every(STATUS_MS, (cx) => this.refreshStatus(cx));
    cx.timer.every(LIST_MS, (cx) => this.loadCheckouts(cx));
  }

  async close() {
    if (this.drawn?.file) await fs.unlink(this.drawn.file).catch(() => {});
  }

  // --- data ---

  loadCheckouts(cx) {
    cx.spawn(async (cx) => {
      try {
        const data = await this.client.checkouts();
        if (!Array.isArray(data?.checkouts)) throw new TrellisError("invalid_response", "Checkout list is invalid.");
        this.checkouts = data.checkouts.filter((row) => row && validRootKey(row.rootKey) && typeof row.state === "string");
        this.listError = null;
        const usable = (row) => row?.state === "available" && typeof row.workspaceRoot === "string";
        const current = this.checkouts.find((row) => row.rootKey === this.rootKey);
        if (!this.rootKey || !usable(current)) {
          // Reading a checkout's status starts indexing it, so never pick one the person hasn't
          // pointed at: the one chosen before (or in props), else the one the focused terminal
          // is in. Otherwise wait for a choice (or a Files selection).
          const here = focused()?.cwd || spaces().find((s) => s.active)?.folder || "";
          const inside = this.checkouts.filter((row) => usable(row) && (here === row.workspaceRoot || here.startsWith(`${row.workspaceRoot}/`)))
            .sort((a, b) => b.workspaceRoot.length - a.workspaceRoot.length)[0];
          const pick = usable(this.checkouts.find((row) => row.rootKey === this.wanted)) ? this.wanted : inside?.rootKey ?? null;
          this.select(pick, cx);
        }
      } catch (error) {
        this.listError = fail(error); this.checkouts = null;
        this.select(null, cx);
      }
      cx.notify();
    });
  }

  select(rootKey, cx) {
    if (rootKey === this.rootKey) return;
    this.rootKey = rootKey; this.status = null; this.statusError = null; this.view = null; this.viewError = null; this.building = null;
    this.open = new Map(); this.tried = null; this.drawError = null;
    this.serial = (this.serial || 0) + 1;
    if (rootKey) { try { localStorage.setItem("trellis.checkout", rootKey); } catch { /* a convenience only */ } }
    if (rootKey) this.refreshStatus(cx);
  }

  refreshStatus(cx) {
    const rootKey = this.rootKey, serial = this.serial;
    if (!rootKey || this.statusBusy) return;
    this.statusBusy = true;
    cx.spawn(async (cx) => {
      try {
        const data = await this.client.status(rootKey);
        if (serial !== this.serial) return;
        const pin = data?.revision == null ? null : pinOf(data.revision);
        const changed = !samePin(pin, this.status?.pin);
        this.status = { pin, catchingUp: data?.catchingUp ?? null, workspaceRoot: data?.workspaceRoot ?? null };
        this.statusError = null;
        if (changed || (!this.view && !this.viewError && !this.loading)) this.loadArchitecture(cx);
      } catch (error) {
        if (serial !== this.serial) return;
        // Never keep data from a checkout whose status can't be read.
        this.status = null; this.statusError = fail(error); this.view = null; this.viewError = null;
      } finally {
        if (serial === this.serial) this.statusBusy = false;
        cx.notify();
      }
    });
  }

  loadArchitecture(cx) {
    const rootKey = this.rootKey, serial = this.serial, pin = this.status?.pin;
    if (!pin) { this.view = null; this.viewError = null; return; }
    this.loading = true; this.view = null; this.viewError = null; this.building = null;
    const started = Date.now();
    const attempt = (cx) => cx.spawn(async (cx) => {
      try {
        const data = await this.client.architecture(rootKey, pin);
        if (serial !== this.serial || !samePin(pin, this.status?.pin)) return;
        const payload = architecturePayload(data, pin);
        if (payload.state === "building") {
          this.building = payload.progress || {};
          const wait = Number.isSafeInteger(payload.retryAfterMs) && payload.retryAfterMs > 0 ? payload.retryAfterMs : BUILDING_MS;
          cx.timer.after(Math.min(wait * (Date.now() - started > 30000 ? 4 : 1), MAX_BUILDING_MS), (cx) => attempt(cx));
          return;
        }
        this.view = payload; this.building = null; this.loading = false;
      } catch (error) {
        if (serial !== this.serial) return;
        this.view = null; this.viewError = fail(error); this.loading = false;
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
    const themeKey = ["background", "foreground", "muted", "input", "surface", "raised", "border", "accent", "ansi0", "ansi1", "ansi3", "ansi9", "ansi11"].map((k) => t[k]).join();
    const key = JSON.stringify([this.view.indexGeneration, this.view.indexRevision, this.view.rulesFile?.contentHash, [...this.open], themeKey, highlight]);
    // The last attempt's inputs, drawn or failed: a failure is not retried for the same inputs.
    const tried = this.tried && this.tried.key === key && Math.abs(this.tried.width - width) <= WIDTH_SLACK;
    if (tried || this.drawing) return;
    this.drawing = true; this.tried = { key, width };
    const view = this.view, serial = this.serial;
    cx.spawn(async (cx) => {
      try {
        const out = await draw(view, { width, open: this.open, theme: t, highlight });
        if (serial !== this.serial || view !== this.view) return;
        const dir = `${appHome()}/mods/trellis/.render`;
        await fs.mkdir(dir, { recursive: true });
        const name = `architecture-${this.copy}-${hash(out.svg)}.svg`;
        await fs.writeFile(`${dir}/${name}`, out.svg);
        const previous = this.drawn?.file;
        this.drawn = { ...out, view, file: `${dir}/${name}`, asset: `mods/trellis/.render/${name}` };
        this.drawError = null;
        if (previous && previous !== this.drawn.file) await fs.unlink(previous).catch(() => {});
      } catch (error) {
        if (serial !== this.serial) return;
        this.drawn = null; this.drawError = fail(error);
      } finally {
        this.drawing = false;
        cx.notify();
      }
    });
  }

  // --- the followed file ---

  followed() {
    const value = shared(SELECTED_FILE);
    if (!value || typeof value !== "object" || typeof value.root !== "string" || typeof value.path !== "string" || value.on) return null;
    const absolute = `${value.root.replace(/\/+$/, "")}/${value.path.replace(/^\/+/, "")}`;
    const checkout = (this.checkouts || []).filter((row) => typeof row.workspaceRoot === "string" && absolute.startsWith(`${row.workspaceRoot}/`))
      .sort((a, b) => b.workspaceRoot.length - a.workspaceRoot.length)[0] || null;
    return { absolute, checkout, path: checkout ? absolute.slice(checkout.workspaceRoot.length + 1) : null };
  }

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
    return { module, layer, group, rank: group ? groups.indexOf(group) : -1, groups: groups.length, members: members.length,
      hidden: layer ? !summary.layers.has(layer.name) : false, stats: layer ? summary.layerStats(layer) : [] };
  }

  // A press on the diagram: a band (its header, or anywhere in it) opens or folds; an arrow's
  // count, a module or a note shows its full accessible label above the drawing.
  press(event, cx) {
    const drawn = this.current(), at = event?.local_position;
    if (!drawn || !at) return;
    const box = hit(drawn.boxes, at.x, at.y);
    if (!box) return;
    if (box.kind === "L") {
      const layer = box.record, summary = drawn.summary;
      this.open.set(layer.name, !summary.expanded(layer)); this.picked = null;
    } else {
      this.picked = { kind: { E: "Arrow", M: "File", D: "Module", N: "Note", U: "Unassigned" }[box.kind] || "Selected", text: box.label };
    }
    cx.notify();
  }

  // The drawing of the payload shown now; a drawing of an earlier payload is never shown.
  current() { return this.drawn && this.drawn.view === this.view ? this.drawn : null; }

  // --- view ---

  render(cx) {
    const t = theme();
    const file = this.followed();
    if (file?.checkout && file.checkout.rootKey !== this.rootKey && file.checkout.state === "available" && file.checkout.rootKey !== this.followedSwitch) {
      // Follow the Files panel into the checkout that holds its file, once per checkout change.
      this.followedSwitch = file.checkout.rootKey;
      this.select(file.checkout.rootKey, cx);
    }
    const place = file?.checkout?.rootKey === this.rootKey ? this.placement(file) : null;
    const highlight = place?.group && place.layer ? [place.layer.name, place.group.name] : place?.group && !place.layer ? [null, place.group.name] : null;
    this.scheduleDraw(cx, highlight);

    return v_flex().id("root").size_full().min_h(0).bg(t.background).text_color(t.foreground).text_size(13)
      .on_size_change((size, cx) => { if (Math.abs(size.width - this.width) >= 1) { this.width = size.width; cx.notify(); } })
      .child(this.header(t, cx))
      .child(v_flex().id("trellis-body").flex_1().min_h(0).overflow_y_scrollbar().p(PAD).gap(12)
        .children(this.body(t, file, place)));
  }

  header(t, cx) {
    const row = this.checkouts?.find((r) => r.rootKey === this.rootKey);
    const label = row ? indexLabel({ ...row, revision: this.status?.pin ?? row.revision, catchingUp: this.status?.catchingUp ?? row.catchingUp }) : null;
    const tone = label?.state === "fresh" ? t.ansi2 : label?.state === "catching-up" ? t.ansi3 : label?.state === "failed" ? t.ansi1 : t.muted;
    return v_flex().flex_none().border_b(1).border_color(t.border)
      .child(h_flex().h(40).px(PAD).gap(10).items_center()
        .child(div().font_semibold().text_size(14).child("Trellis"))
        .child(div().text_color(t.dim).child("Architecture"))
        .child(div().flex_1())
        .child(h_flex().id("checkout-button").gap(6).items_center().px(8).h(26).rounded(6).border_1().border_color(t.input || t.border)
          .cursor_pointer().hover((s) => s.bg(t.hover))
          .on_click((_e, cx) => { this.listOpen = !this.listOpen; cx.notify(); })
          .child(div().max_w(220).truncate().child(row ? basename(row.workspaceRoot || row.rootKey) : this.checkouts ? "Choose a checkout" : "Checkouts…"))
          .child(div().text_color(t.dim).child(this.listOpen ? "▴" : "▾"))))
      .when(Boolean(label), (el) => el.child(h_flex().px(PAD).pb(8).gap(6).items_center().text_size(12)
        .child(div().text_color(tone).child(label.shape))
        .child(div().text_color(tone).child(label.text))
        .child(div().flex_1().min_w(0).truncate().text_color(t.dim).child(row?.workspaceRoot || ""))))
      .when(this.listOpen, (el) => el.child(this.checkoutList(t)));
  }

  checkoutList(t) {
    if (!this.checkouts?.length) return div().px(PAD).pb(8).text_size(12).text_color(t.dim).child(this.listError ? "Checkout list unavailable." : "No checkouts registered with this daemon.");
    return v_flex().px(6).pb(6).gap(1).children(this.checkouts.map((row) => {
      const label = indexLabel(row), usable = row.state === "available";
      return h_flex().id(`checkout-${row.rootKey}`).h(28).px(8).gap(8).items_center().rounded(6)
        .when(usable, (el) => el.cursor_pointer().hover((s) => s.bg(t.hover)).on_click((_e, cx) => { this.listOpen = false; this.wanted = row.rootKey; this.select(row.rootKey, cx); cx.notify(); }))
        .when(row.rootKey === this.rootKey, (el) => el.bg(t.selected))
        .child(div().w(14).text_color(usable ? t.muted : t.dim).child(label.shape))
        .child(div().flex_none().text_color(usable ? t.foreground : t.dim).child(basename(row.workspaceRoot || row.rootKey)))
        .child(div().flex_1().min_w(0).truncate().text_size(12).text_color(t.dim).child(`${label.text} · ${row.workspaceRoot || ""}`));
    }));
  }

  // A typed failure: its code and message, and nothing else from that read.
  problem(t, error, what) {
    return v_flex().gap(4).p(10).rounded(8).border_1().border_color(t.ansi1).bg(t.surface)
      .child(h_flex().gap(6).child(div().text_color(t.ansi1).child("⚠")).child(div().font_semibold().child(what)))
      .child(div().text_size(12).text_color(t.muted).font_family(settingsMono()).child(error.code))
      .child(div().text_color(t.foreground).child(error.message));
  }

  note(t, text) { return div().text_color(t.muted).child(text); }

  body(t, file, place) {
    if (this.fatal) return [this.problem(t, this.fatal, "Configuration")];
    if (this.listError) return [this.problem(t, this.listError, "Daemon")];
    if (!this.checkouts) return [this.note(t, "Reading the daemon's checkouts…")];
    if (!this.rootKey) return [this.note(t, !this.checkouts.length ? "No checkouts registered with this daemon."
      : this.checkouts.some((row) => row.state === "available") ? "Choose a checkout above, or select a file in Files." : "No available checkout. Choose one above when it is ready.")];
    if (this.statusError?.code === "index_not_ready") return [this.note(t, `Index not ready (index_not_ready: ${this.statusError.message}). Indexing starts when the browser, CLI or MCP first uses this checkout.`)];
    if (this.statusError) return [this.problem(t, this.statusError, "Index status")];
    if (!this.status) return [this.note(t, "Reading index status…")];
    if (!this.status.pin) return [this.note(t, this.status.catchingUp ? "Indexing this checkout. The diagram appears when the first revision is published." : "This checkout has no index yet.")];
    if (this.viewError) return [this.problem(t, this.viewError, "Architecture")];
    if (this.building) {
      const phase = { reading: "reading indexed files", resolving: "resolving references", evaluating: "evaluating rules", measuring: "measuring code" }[this.building.phase] || "preparing";
      const count = this.building.completed > 0 && this.building.total > 0 ? ` ${this.building.completed} of ${this.building.total}` : "";
      return [this.note(t, `Building Architecture for this revision: ${phase}${count}…`)];
    }
    const view = this.view;
    if (!view) return [this.note(t, "Reading indexed architecture and reviewed rules…")];
    if (view.state === "missing_rules") return [this.note(t, "No reviewed rules file. Create or review .trellis/rules.toml to configure layers.")];
    if (view.state === "no_layers") return [this.note(t, "No authored layers are configured. Review .trellis/rules.toml to configure layers.")];
    const out = [];
    if (file) out.push(this.fileCard(t, file, place));
    const drawn = this.current();
    if (this.drawError) out.push(this.problem(t, this.drawError, "Diagram"));
    else if (drawn) {
      if (this.picked) out.push(h_flex().flex_none().gap(8).items_start().p(8).rounded(6).bg(t.surface).border_1().border_color(t.border)
        .child(div().text_color(t.dim).child(this.picked.kind))
        .child(div().flex_1().min_w(0).whitespace_normal().child(this.picked.text.replace(/\n/g, " · ")))
        .child(div().id("picked-close").text_color(t.dim).cursor_pointer().child("×").on_click((_e, cx) => { this.picked = null; cx.notify(); })));
      // The image is drawn at 1:1, so a press's element-relative position is in SVG units.
      out.push(div().id("diagram").flex_none().w(drawn.width).h(drawn.height).cursor_pointer()
        .on_mouse_down("left", (event, cx) => this.press(event, cx))
        .child(image(drawn.asset).w(drawn.width).h(drawn.height)));
    }
    else out.push(this.note(t, "Drawing the layer stack…"));
    if (drawn) out.push(this.layerList(t, place));
    return out;
  }

  fileCard(t, file, place) {
    const card = v_flex().flex_none().gap(6).p(10).rounded(8).border_1().border_color(t.accent).bg(t.surface)
      .child(h_flex().gap(8).items_center()
        .child(div().text_size(12).text_color(t.dim).child("Files selection"))
        .child(div().flex_1().min_w(0).truncate().font_family(settingsMono()).text_size(12).child(file.path ?? file.absolute)));
    if (!file.checkout) return card.child(this.note(t, "Not in a checkout this daemon serves."));
    if (file.checkout.rootKey !== this.rootKey) return card.child(this.note(t, `In ${basename(file.checkout.workspaceRoot)}, which is not available.`));
    if (!place) return card.child(this.note(t, "Waiting for the diagram…"));
    if (place.missing) return card.child(this.note(t, "Not an indexed source file at this revision."));
    const { module, layer, group } = place;
    const metric = heat.metricValue(module.metrics, "cognitive");
    const band = this.drawn.summary.scale.band(metric);
    card.child(h_flex().gap(6).items_center().flex_wrap()
      .child(div().text_color(t.dim).child("Layer"))
      .child(div().font_semibold().child(layer ? `${layer.shared ? "Shared" : layer.order} · ${layer.name}` : "Unassigned / unknown"))
      .when(place.hidden, (el) => el.child(div().text_size(12).text_color(t.dim).child(this.drawn.summary.tests.has(layer.name) ? "(tests/fixtures layer, not drawn)" : "(hidden layer, not drawn)")))
      .children(place.stats.map((stat) => this.stat(t, stat))));
    if (group) {
      const drawnChip = place.rank >= 0 && this.drawn.projection && [...this.drawn.projection.nodes.values()].some((n) => n.kind === "C" && n.record.group === group.name && n.record.layerKey === (layer?.name ?? null));
      card.child(h_flex().gap(6).items_center().flex_wrap()
        .child(div().text_color(t.dim).child("Module"))
        .child(this.chip(t, group.name, this.drawn.summary.scale.band(group.value)))
        .child(div().text_color(t.muted).child(`${plural(group.files.length, "file")} · max cognitive ${display(group.value)} · #${place.rank + 1} of ${place.groups} in the layer`))
        .child(div().text_size(12).text_color(t.dim).child(drawnChip ? "outlined in the diagram" : "in the band's “more” chip")));
    }
    card.child(h_flex().gap(6).items_center().flex_wrap()
      .child(div().text_color(t.dim).child("File"))
      .child(div().text_color(t.muted).child(module.declarationCount > 0
        ? `cognitive ${display(metric)}${band === null ? "" : ` (${HEAT_NAMES[band]})`} · ${plural(module.declarationCount, "measured callable")}`
        : `no measured callables${module.excludedReason ? ` · ${module.excludedReason}` : ""}`)));
    return card;
  }

  chip(t, text, band) {
    const fill = band === null || band === undefined || band === 0 ? t.raised : [null, t.ansi11 || t.ansi3, t.ansi3, t.ansi9 || t.ansi1][band];
    return div().px(6).py(1).rounded(4).bg(fill).text_color(band > 0 ? t.ansi0 || t.background : t.foreground).child(text);
  }

  stat(t, stat) {
    const color = stat.tone === "error" ? t.ansi1 : t.muted;
    return div().px(5).rounded(3).border_1().border_color(color).text_size(12).text_color(color).child(stat.text);
  }

  // The layer stack as rows: order, name, files, max cognitive, outgoing counts. A row opens or
  // folds its band in the diagram, as clicking a band header does in the browser.
  layerList(t, place) {
    const view = this.view, summary = this.drawn.summary;
    const ordered = view.layers.filter((l) => !l.shared).sort((a, b) => a.order - b.order || a.name.localeCompare(b.name));
    const shared = view.layers.filter((l) => l.shared).sort((a, b) => a.name.localeCompare(b.name));
    const unassigned = view.modules.filter((m) => m.layer === null).length;
    const rows = [...ordered, ...shared].map((layer) => {
      const members = view.modules.filter((m) => m.layer === layer.name);
      const drawnLayer = summary.layers.has(layer.name), open = drawnLayer && summary.expanded(layer);
      const current = place?.layer?.name === layer.name;
      return h_flex().id(`layer-${layer.name}`).min_h(30).px(8).py(4).gap(8).items_center().rounded(6).flex_wrap()
        .when(current, (el) => el.bg(t.selected))
        .when(drawnLayer, (el) => el.cursor_pointer().hover((s) => s.bg(t.hover)).on_click((_e, cx) => { this.open.set(layer.name, !open); cx.notify(); }))
        .child(div().w(14).text_color(t.dim).child(drawnLayer ? (open ? "▾" : "▸") : "·"))
        .child(div().w(52).text_color(t.dim).child(layer.shared ? "Shared" : `Order ${layer.order}`))
        .child(div().font_semibold().text_color(drawnLayer ? t.foreground : t.dim).child(layer.name))
        .child(div().text_color(t.muted).child(`${plural(members.length, "file")} · max ${display(summary.worst(members))}`))
        .when(!drawnLayer, (el) => el.child(div().text_size(12).text_color(t.dim).child(summary.tests.has(layer.name) ? "tests/fixtures · not drawn" : "hidden · not drawn")))
        .children(summary.layerStats(layer).map((stat) => this.stat(t, stat)));
    });
    const sites = (view.references?.coverage || []).reduce((s, r) => s + r.sites, 0), mapped = (view.references?.coverage || []).reduce((s, r) => s + r.mappedSites, 0);
    return v_flex().flex_none().gap(2)
      .child(h_flex().gap(8).items_center().pb(4)
        .child(div().font_semibold().text_size(14).child("Layers"))
        .child(div().text_size(12).text_color(t.dim).child(`${plural(view.modules.length, "file module")} · rules ${String(view.rulesFile?.contentHash || "").slice(0, 12)}`)))
      .children(rows)
      .child(h_flex().min_h(30).px(8).gap(8).items_center()
        .child(div().w(14)).child(div().w(52).text_color(t.dim).child("—"))
        .child(div().font_semibold().child("Unassigned")).child(div().text_color(t.muted).child(`${plural(unassigned, "file")} · no layer`)))
      .child(div().pt(6).text_size(12).text_color(t.dim).child(
        `${mapped.toLocaleString("en-US")} of ${sites.toLocaleString("en-US")} reference sites mapped to a module${(view.references?.coverage || []).some((r) => r.unresolvedSites > 0) ? " · ≥ counts are lower bounds" : ""} · heat: cognitive, max per callable, fixed bands`));
  }
}

function settingsMono() { return "JetBrains Mono"; }
