// Trellis beside the coding agents (#707): the daemon's Architecture and Sequence views as a
// Herdr pane.
//
// Shell: the checkout the focused agent works in (or the Files selection's, whichever is more
// recent), with a pin that stops following; index status (revision, fresh or catching up) and
// a typed error state that never shows partial data. The daemon is the one on
// the machine the shown space lives on (src/client.js). Views, drawn by the browser's own
// adapters and Pele into SVGs under this mod's `.render/` folder and shown with image():
// - Architecture: the layer stack, with the layer list and counts as native rows;
// - Sequence: a method's static call sequence, with its steps as native rows.
// Both follow the file the `trellis-files` panel selects (shared "trellis.selectedFile").
// An agent's `trellis_show` (#818), carried by each status poll as `viewRequest`, takes an
// unpinned pane to that Sequence, with a "Shown by agent" note and Back.
//
// Props (all optional): { url: "http://127.0.0.1:8877", tokenFile: "~/.config/trellis/serve.token",
// checkout: "<root key>", machine: "<Herdr machine>", transport: "exec" }.
// Open beside a terminal: pane.split {direction: "right", mod: "trellis"}.
import { View, div, image } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { appHome, layout, machines, props, setShared, shared, spaces, theme } from "herdr";
import * as fs from "fs/promises";
import { TrellisError, createClient, indexLabel, jobCause } from "./client.js";
import { DEFAULT_AGGREGATE, DEFAULT_METRIC, ERROR } from "./diagram.js";
import * as ui from "./architecture-ui.js";
import { ensureFilesPanel } from "./panels.js";
import { STATUS_MS, LIST_MS, PAD, TABS, LOGO, fail, basename, hash, settingsMono } from "./common.js";
import { Checkout } from "./checkout.js";
import { AgentShow } from "./agent-show.js";
import { ArchitectureView } from "./architecture-view.js";
import { Deviations } from "./deviations.js";
import { SequenceView } from "./sequence-view.js";

export default class Trellis extends View {
  init(_props, cx) {
    const p = props() || {};
    this.copy = Math.random().toString(36).slice(2, 10);
    this.open = new Map();
    // The web toolbar's choices (#886): Color by and Aggregation keep across checkouts, as in the browser.
    this.metric = DEFAULT_METRIC; this.aggregate = DEFAULT_AGGREGATE; this.menu = null;
    this.width = 0;
    this.props = p;
    // Follow (#809): the last agent-or-terminal focus and the last Files selection, each with a
    // sequence number, so the more recent one picks the checkout. Pinned stops agent follow.
    this.events = 0; this.agent = null; this.selection = null; this.focusKey = null; this.fileKey = null;
    this.pinned = (() => { try { return localStorage.getItem("trellis.pinned") === "true"; } catch { return false; } })();
    this.files = {};
    this.tab = (() => { try { const tab = localStorage.getItem("trellis.tab"); return TABS.includes(tab) ? tab : "architecture"; } catch { return "architecture"; } })();
    // The layout's trellis-files is reached only through the tab bar's Files button.
    try { ensureFilesPanel({ shared, setShared, layout }); } catch { /* the pane works without it */ }
    this.observe();
    this.pinnedSeq = this.events; // a pin kept from last time outranks what was there at start
    this.useMachine(this.shownMachine(), cx);
    cx.timer.every(STATUS_MS, (cx) => { if (this.client?.transport === "fetch" || ++this.ticks % 2 === 0) this.refreshStatus(cx); });
    cx.timer.every(LIST_MS, (cx) => this.loadCheckouts(cx));
  }

  async close() {
    for (const file of Object.values(this.files)) await fs.unlink(file).catch(() => {});
  }

  // The machine whose daemon this pane shows: the props' choice, else the shown space's.
  shownMachine() {
    if (typeof this.props.machine === "string") return this.props.machine || null;
    return spaces().find((s) => s.active)?.machine ?? null;
  }

  useMachine(machine, cx) {
    this.machine = machine; this.ticks = 0;
    this.checkouts = null; this.listError = null; this.fatal = null; this.unknown = null;
    this.rootKey = undefined; this.select(null, cx);
    try { this.client = createClient({ url: this.props.url, tokenFile: this.props.tokenFile, machine, transport: this.props.transport }); }
    catch (error) { this.fatal = fail(error); this.client = null; return; }
    this.loadCheckouts(cx);
  }

  // Writes one drawing under the mod's hidden `.render/` (a hidden folder: no mod reload) and
  // deletes this copy's previous drawing of that kind. Returns the image() path.
  async saveDrawing(kind, svg) {
    const dir = `${appHome()}/mods/trellis/.render`;
    await fs.mkdir(dir, { recursive: true });
    const name = `${kind}-${this.copy}-${hash(svg)}.svg`, file = `${dir}/${name}`;
    await fs.writeFile(file, svg);
    const previous = this.files[kind];
    this.files[kind] = file;
    if (previous && previous !== file) await fs.unlink(previous).catch(() => {});
    return `mods/trellis/.render/${name}`;
  }

  // The top tab; the choice is kept, as the browser keeps its view.
  setTab(tab, cx) {
    this.tab = tab;
    try { localStorage.setItem("trellis.tab", tab); } catch { /* a convenience only */ }
    cx.notify();
  }

  // --- view ---

  render(cx) {
    const t = theme();
    const machine = this.shownMachine();
    if (machine !== this.machine) this.useMachine(machine, cx);
    this.observe();
    this.follow(cx);
    const file = this.followed();
    const place = file?.checkout?.rootKey === this.rootKey ? this.placement(file) : null;
    const highlight = place?.group ? [place.layer?.name ?? null, place.group.name, place.module.app ?? null] : null;
    if (this.tab === "architecture") this.scheduleDraw(cx, highlight);
    // The Deviations tab reads its tree once the payload is here, and again for a new one or another app.
    else if (this.tab === "deviations") { if (!this.devs) this.loadDeviations(null, cx); }
    else { this.followSequence(file, cx); this.scheduleSequenceDraw(cx); }

    return v_flex().id("root").size_full().min_h(0).bg(t.background).text_color(t.foreground).text_size(13)
      .on_size_change((size, cx) => { if (Math.abs(size.width - this.width) >= 1) { this.width = size.width; cx.notify(); } })
      .child(this.header(t, cx))
      .child(v_flex().id("trellis-body").flex_1().min_h(0).overflow_y_scrollbar().p(PAD).gap(12)
        .children([...this.shownBar(t), ...this.body(t, file, place, cx)]));
  }

  // The web app's header: the Trellis by SquashMerge mark, the views, the followed checkout and
  // its pin; then the status line (web/app.js renderIndexStatus): the index state, and a failed
  // later re-index as a secondary note.
  header(t, cx) {
    const row = this.checkouts?.find((r) => r.rootKey === this.rootKey);
    const label = row ? indexLabel({ ...row, revision: this.status?.pin ?? row.revision, catchingUp: this.status?.catchingUp ?? row.catchingUp }) : null;
    const job = this.job, failed = job?.state === "failed";
    const state = failed && !this.status?.pin ? { text: `Index failed · ${jobCause(job)}`, state: "failed", shape: "□" } : label;
    const note = failed && this.status?.pin ? `Last re-index failed · ${jobCause(job)}` : "";
    const tone = state?.state === "fresh" ? t.ansi2 : state?.state === "catching-up" ? t.ansi3 : state?.state === "failed" ? ERROR : t.muted;
    return v_flex().flex_none().border_b(1).border_color(t.border)
      .child(h_flex().h(40).px(PAD).gap(10).items_center()
        .child(h_flex().id("brand").flex_none().gap(6).items_center()
          .child(image(LOGO).w(16).h(20))
          .child(div().font_semibold().text_size(14).child("Trellis"))
          .child(div().text_size(12).text_color(t.dim).child("by SquashMerge")))
        .children(TABS.map((tab) => div().id(`tab-${tab}`).px(8).h(24).flex().items_center().rounded(6).cursor_pointer()
          .text_color(this.tab === tab ? t.foreground : t.dim).when(this.tab === tab, (el) => el.bg(t.selected)).hover((s) => s.bg(t.hover))
          .on_click((_e, cx) => this.setTab(tab, cx))
          .child({ architecture: "Architecture", sequence: "Sequence", deviations: "Deviations" }[tab])))
        .child(div().flex_1())
        // The checkout is followed, not chosen: a label, and a pin that stops agent follow.
        .child(div().id("checkout-label").max_w(220).truncate().text_color(row ? t.foreground : t.dim)
          .child(row ? basename(row.workspaceRoot || row.rootKey) : this.unknown ? basename(this.unknown.folder) : this.checkouts ? "No checkout" : "Checkouts…"))
        .child(div().id("pin").px(8).h(24).flex().items_center().rounded(6).border_1().cursor_pointer().text_size(12)
          .border_color(this.pinned ? t.accent : t.input || t.border).text_color(this.pinned ? t.accent : t.dim).hover((s) => s.bg(t.hover))
          .on_click((_e, cx) => this.setPinned(!this.pinned, cx))
          .child(this.pinned ? "Pinned" : "Following")))
      .when(Boolean(state), (el) => el.child(h_flex().id("status-line").px(PAD).pb(8).gap(6).items_center().text_size(12)
        .child(div().text_color(tone).child(state.shape))
        .child(div().flex_none().text_color(tone).child(state.text))
        .when(Boolean(note), (el) => el.child(div().id("status-note").flex_none().text_color(ERROR).child(`· ${note}`)))
        .child(div().flex_1().min_w(0).truncate().text_color(t.dim).child(`${row?.workspaceRoot || ""}${this.machine ? ` · on ${this.machine}` : ""}`))));
  }

  // A typed failure: its code and message, and nothing else from that read.
  problem(t, error, what) {
    return v_flex().gap(4).p(10).rounded(8).border_1().border_color(ERROR).bg(t.surface)
      .child(h_flex().gap(6).child(div().text_color(ERROR).child("⚠")).child(div().font_semibold().child(what)))
      .child(div().text_size(12).text_color(t.muted).font_family(settingsMono()).child(error.code))
      .child(div().text_color(t.foreground).child(error.message));
  }

  note(t, text) { return div().text_color(t.muted).whitespace_normal().child(text); }

  body(t, file, place) {
    if (this.fatal) return [this.problem(t, this.fatal, "Configuration")];
    const layout = this.machine ? machines().find((m) => m.name === this.machine)?.layout : "ready";
    if (layout === "unavailable") return [this.problem(t, new TrellisError("machine_unavailable", `Herdr can't reach ${this.machine} right now.`), "Machine")];
    if (this.listError) return [this.problem(t, this.listError, "Daemon")];
    if (!this.checkouts) return [this.note(t, "Reading the daemon's checkouts…")];
    if (!this.rootKey && this.unknown?.state) return [this.note(t, `${this.unknown.folder} is a checkout the daemon lists as ${this.unknown.state}, so it isn't shown.`)];
    if (!this.rootKey && this.unknown) return [this.note(t, `${this.unknown.folder} isn't in a checkout this daemon lists${this.machine ? ` on ${this.machine}` : ""}. A checkout is listed once the browser, CLI or MCP has used it.`)];
    if (!this.rootKey) return [this.note(t, !this.checkouts.length ? "No checkouts registered with this daemon."
      : this.pinned ? "Pinned with no checkout. Select a file in Files, or unpin to follow the focused agent." : "Focus an agent or terminal in a checkout, or select a file in Files.")];
    if (this.statusError?.code === "index_not_ready") return [this.note(t, `Index not ready (index_not_ready: ${this.statusError.message}). Indexing starts when the browser, CLI or MCP first uses this checkout.`)];
    if (this.statusError) return [this.problem(t, this.statusError, "Index status")];
    if (!this.status) return [this.note(t, "Reading index status…")];
    if (!this.status.pin) return [this.note(t, this.status.catchingUp ? "Indexing this checkout. The diagram appears when the first revision is published." : "This checkout has no index yet.")];
    if (this.tab === "sequence") return this.sequenceBody(t, file);
    // The Deviations tab is the tree alone: Color by and Aggregation change only the drawing.
    const tree = this.tab === "deviations";
    const controls = tree ? [] : ui.controls(this, t);
    if (this.viewError) return [...controls, this.problem(t, this.viewError, "Architecture")];
    if (this.building) {
      const phase = { reading: "reading indexed files", resolving: "resolving references", evaluating: "evaluating rules", measuring: "measuring code" }[this.building.phase] || "preparing";
      const count = this.building.completed > 0 && this.building.total > 0 ? ` ${this.building.completed} of ${this.building.total}` : "";
      return [...controls, this.note(t, `Building Architecture for this revision: ${phase}${count}…`)];
    }
    const view = this.view;
    if (!view) return [...controls, this.note(t, "Reading indexed architecture and reviewed rules…")];
    if (view.state === "missing_rules") return [this.note(t, "No reviewed rules file. Create or review .trellis/rules.toml to configure layers.")];
    if (view.state === "no_layers") return [this.note(t, "No authored layers are configured. Review .trellis/rules.toml to configure layers.")];
    if (tree) {
      const summary = this.deviationSummary();
      return [...ui.switcher(this, t, summary, view), ...(this.devs ? ui.deviations(this, t, summary, view) : [this.note(t, `Reading deviations at ${this.pinText()}…`)])];
    }
    const drawn = this.current();
    // The switcher and controls come from the drawing's summary, so they always match what is drawn.
    const out = [...controls];
    if (drawn) out.push(...ui.switcher(this, t, drawn.summary, view));
    if (file) out.push(this.fileCard(t, file, place));
    if (this.drawError) out.push(this.problem(t, this.drawError, "Diagram"));
    else if (drawn?.summary.noLayers) out.push(...ui.noLayers(t, drawn.summary, view));
    else if (drawn) out.push(...ui.drawing(this, t, drawn));
    else out.push(this.note(t, "Drawing the layer stack…"));
    if (drawn) out.push(...ui.legend(t, drawn.summary, view), ...ui.inspector(this, t, drawn.summary, view), this.layerList(t, place));
    return out;
  }
}

// The rest of the pane's methods, written in the files named here and installed unchanged: the
// class has the same methods as when they were all in this file.
for (const part of [Checkout, AgentShow, ArchitectureView, Deviations, SequenceView]) {
  const { constructor: _, ...methods } = Object.getOwnPropertyDescriptors(part.prototype);
  Object.defineProperties(Trellis.prototype, methods);
}
