// Trellis beside the coding agents (#707): the daemon's Architecture and Sequence views as a
// Herdr pane.
//
// Shell: a checkout selector (the daemon's checkout list), index status (revision, fresh or
// catching up) and a typed error state that never shows partial data. The daemon is the one on
// the machine the shown space lives on (src/client.js). Views, drawn by the browser's own
// adapters and Pele into SVGs under this mod's `.render/` folder and shown with image():
// - Architecture: the layer stack, with the layer list and counts as native rows;
// - Sequence: a method's static call sequence, with its steps as native rows.
// Both follow the file the `trellis-files` panel selects (shared "trellis.selectedFile").
//
// Props (all optional): { url: "http://127.0.0.1:8877", tokenFile: "~/.config/trellis/serve.token",
// checkout: "<root key>", machine: "<Herdr machine>", transport: "exec" }.
// Open beside a terminal: pane.split {direction: "right", mod: "trellis"}.
import { View, div, image } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { appHome, focused, machines, props, shared, spaces, theme } from "herdr";
import * as fs from "fs/promises";
import { TrellisError, architecturePayload, createClient, indexLabel, pinOf, samePin, validRootKey } from "./client.js";
import { draw, heat, hit, plural, display } from "./diagram.js";
import { drawSequence, helpers as sequenceHelpers, targetOf } from "./sequence.js";

// Polls are gentler for another machine: each one is a command run there.
const STATUS_MS = 5000, REMOTE_STATUS_MS = 10000, LIST_MS = 30000, BUILDING_MS = 250, MAX_BUILDING_MS = 5000;
const MAX_STEP_ROWS = 300, MAX_HISTORY = 20;
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
    this.props = p;
    this.files = {};
    this.tab = (() => { try { return localStorage.getItem("trellis.tab") === "sequence" ? "sequence" : "architecture"; } catch { return "architecture"; } })();
    this.wanted = validRootKey(p.checkout) ? p.checkout : (() => { try { return localStorage.getItem("trellis.checkout"); } catch { return null; } })();
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
    this.checkouts = null; this.listError = null; this.fatal = null;
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

  // --- data ---

  loadCheckouts(cx) {
    const client = this.client;
    if (!client) return;
    cx.spawn(async (cx) => {
      try {
        const data = await client.checkouts();
        if (client !== this.client) return;
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
        if (client !== this.client) return;
        this.listError = fail(error); this.checkouts = null;
        this.select(null, cx);
      }
      cx.notify();
    });
  }

  select(rootKey, cx) {
    if (rootKey === this.rootKey) return;
    this.rootKey = rootKey; this.status = null; this.statusError = null; this.view = null; this.viewError = null; this.building = null;
    this.open = new Map(); this.tried = null; this.drawError = null; this.picked = null;
    this.seq = { history: [], expanded: new Set() };
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
        if (changed) this.reloadSequence(cx);
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
        const asset = await this.saveDrawing("architecture", out.svg);
        if (serial !== this.serial || view !== this.view) return;
        this.drawn = { ...out, view, asset };
        this.drawError = null;
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
    // Only a file on the machine whose daemon this pane shows (Files publishes its machine as `on`).
    if (!value || typeof value !== "object" || typeof value.root !== "string" || typeof value.path !== "string" || (value.on ?? null) !== this.machine) return null;
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

  // --- Sequence ---

  // Follows the Files selection to that file's methods; the first consequential named method
  // opens unless one is open already for this file.
  followSequence(file, cx) {
    const path = file?.checkout?.rootKey === this.rootKey ? file.path : null;
    if (!path || !this.status?.pin || this.seq.file === path) return;
    this.seq = { file: path, history: [], expanded: new Set() };
    this.loadMethods(cx);
  }

  reloadSequence(cx) {
    if (!this.seq?.file) return;
    const keep = this.seq.seed;
    this.seq = { ...this.seq, methods: null, methodsError: null, data: null, error: null, drawn: null, drawError: null, keep };
    this.seqTried = null;
    this.loadMethods(cx);
  }

  loadMethods(cx) {
    const { rootKey, serial } = this, pin = this.status?.pin, seq = this.seq;
    cx.spawn(async (cx) => {
      try {
        const data = await this.client.methods(rootKey, pin, seq.file);
        if (serial !== this.serial || seq !== this.seq) return;
        if (!Array.isArray(data?.items)) throw new TrellisError("invalid_response", "Invalid method list. Nothing displayed.");
        seq.methods = data.items.map((item) => item?.symbol).filter((symbol) => typeof symbol?.id === "string" && typeof symbol.name === "string");
        const named = seq.methods.filter((m) => !m.name.startsWith("<"));
        const first = seq.methods.find((m) => m.id === seq.keep?.id) || named.find((m) => data.items.find((i) => i.symbol === m)?.consequential) || named[0] || seq.methods[0];
        if (first) this.loadSequence(first, false, cx);
      } catch (error) {
        if (serial !== this.serial || seq !== this.seq) return;
        seq.methodsError = fail(error);
      } finally { cx.notify(); }
    });
  }

  // Opens one method's sequence; `push` keeps the current one for Back (a re-center).
  loadSequence(symbol, push, cx) {
    const { rootKey, serial } = this, pin = this.status?.pin, seq = this.seq;
    if (push && seq.seed) seq.history = [...seq.history, seq.seed].slice(-MAX_HISTORY);
    Object.assign(seq, { seed: { id: symbol.id, name: symbol.name, path: symbol.path }, data: null, error: null, selected: null, expanded: new Set(), loading: true });
    cx.spawn(async (cx) => {
      try {
        const data = await this.client.sequence(rootKey, pin, symbol.id);
        if (serial !== this.serial || seq !== this.seq || seq.seed.id !== symbol.id) return;
        // As the browser checks it: the answer is for this revision and this seed, whole.
        if (!samePin(data?.revision, pin) || data?.seed?.id !== symbol.id || !Array.isArray(data.steps) || !Array.isArray(data.participants)) {
          throw new TrellisError("revision_conflict", "Sequence provenance mismatch. Nothing displayed.");
        }
        seq.data = data; seq.seed = { id: data.seed.id, name: data.seed.name, path: data.seed.path };
      } catch (error) {
        if (serial !== this.serial || seq !== this.seq) return;
        seq.error = fail(error);
      } finally { if (seq === this.seq) seq.loading = false; cx.notify(); }
    });
  }

  scheduleSequenceDraw(cx) {
    const seq = this.seq, data = seq?.data;
    if (!data || this.seqDrawing) return;
    const t = theme();
    const key = JSON.stringify([data.revision, data.seed.id, seq.selected, [...seq.expanded], t.background, t.foreground, t.accent, t.selected]);
    if (this.seqTried === key) return;
    this.seqDrawing = true; this.seqTried = key;
    const serial = this.serial;
    cx.spawn(async (cx) => {
      try {
        const out = await drawSequence(data, { theme: t, selectedId: seq.selected, expanded: seq.expanded });
        if (serial !== this.serial || seq !== this.seq || data !== seq.data) return;
        seq.drawn = out.svg ? { ...out, data, asset: await this.saveDrawing("sequence", out.svg) } : { ...out, data };
        seq.drawError = null;
      } catch (error) {
        if (serial !== this.serial || seq !== this.seq) return;
        seq.drawn = null; seq.drawError = fail(error);
      } finally { this.seqDrawing = false; cx.notify(); }
    });
  }

  // A step: select it; a call with a proven target re-centers the view on that method, as the
  // browser's target action does; a collapsed call chain opens or closes.
  activateStep(step, cx) {
    const seq = this.seq, data = seq.data;
    if (!step || !data) return;
    const target = targetOf(data, step);
    if (target) { this.loadSequence({ id: target.declaration.id, name: target.declaration.name, path: target.declaration.path }, true, cx); cx.notify(); return; }
    if (step.kind === "group") { if (seq.expanded.has(step.id)) seq.expanded.delete(step.id); else seq.expanded.add(step.id); }
    seq.selected = step.id;
    cx.notify();
  }

  pressSequence(event, cx) {
    const drawn = this.seq?.drawn, at = event?.local_position;
    if (!drawn?.boxes || !at) return;
    const box = drawn.boxes.filter((b) => at.x >= b.x && at.x <= b.x + b.w && at.y >= b.y && at.y <= b.y + b.h).sort((a, b) => a.w * a.h - b.w * b.h)[0];
    if (box) this.activateStep(findStep(this.seq.data.steps, box.step), cx);
  }

  sequenceBody(t, file) {
    const seq = this.seq, out = [];
    if (!seq.file && !seq.seed) return [this.note(t, file && !file.checkout ? "The Files selection is not in a checkout this daemon serves." : "Select a file in Files to see its methods and their call sequences.")];
    if (seq.file) {
      const methods = seq.methods || [];
      out.push(v_flex().flex_none().gap(6)
        .child(h_flex().gap(8).items_center()
          .child(div().text_size(12).text_color(t.dim).child("Methods in"))
          .child(div().flex_1().min_w(0).truncate().font_family(settingsMono()).text_size(12).child(seq.file)))
        .when(Boolean(seq.methodsError), (el) => el.child(this.problem(t, seq.methodsError, "Methods")))
        .when(!seq.methods && !seq.methodsError, (el) => el.child(this.note(t, "Reading this file's methods…")))
        .when(Boolean(seq.methods) && !methods.length, (el) => el.child(this.note(t, "No indexed methods in this file.")))
        .child(h_flex().gap(4).flex_wrap().children(methods.filter((m) => !m.name.startsWith("<")).slice(0, 60).map((m) =>
          div().id(`method-${m.id}`).px(6).py(1).rounded(4).cursor_pointer().font_family(settingsMono()).text_size(12)
            .bg(seq.seed?.id === m.id ? t.selected : t.raised).hover((st) => st.bg(t.hover))
            .on_click((_e, cx) => { this.seq.history = []; this.loadSequence(m, false, cx); cx.notify(); })
            .child(m.name)))));
    }
    if (!seq.seed) return out;
    out.push(h_flex().flex_none().gap(8).items_center()
      .when(seq.history.length > 0, (el) => el.child(div().id("sequence-back").px(6).rounded(4).border_1().border_color(t.border).cursor_pointer()
        .hover((st) => st.bg(t.hover)).on_click((_e, cx) => { const back = this.seq.history.pop(); if (back) this.loadSequence(back, false, cx); cx.notify(); })
        .child(`← ${seq.history[seq.history.length - 1]?.name ?? ""}`)))
      .child(div().font_semibold().text_size(14).child(seq.seed.name || seq.seed.id))
      .child(div().flex_1().min_w(0).truncate().text_size(12).text_color(t.dim).child(seq.seed.path || "")));
    if (seq.error) return [...out, this.problem(t, seq.error, "Sequence")];
    if (!seq.data) return [...out, this.note(t, "Reading the static call sequence…")];
    const data = seq.data;
    out.push(div().flex_none().text_size(12).text_color(t.dim).child(`${sequenceHelpers.evidenceSummary(data)} · ${data.hiddenSteps ?? 0} incidental steps hidden${data.truncated ? " · truncated" : ""} · static possible paths, not a runtime trace`));
    const drawn = seq.drawn?.data === data ? seq.drawn : null;
    if (seq.drawError) out.push(this.problem(t, seq.drawError, "Diagram"));
    else if (!drawn) out.push(this.note(t, "Drawing the sequence…"));
    else {
      if (drawn.empty) out.push(this.note(t, drawn.empty));
      if (drawn.asset) out.push(div().id("sequence-diagram").flex_none().w(drawn.width).h(drawn.height).cursor_pointer()
        .on_mouse_down("left", (event, cx) => this.pressSequence(event, cx))
        .child(image(drawn.asset).w(drawn.width).h(drawn.height)));
    }
    out.push(this.stepList(t, data));
    for (const warning of (data.warnings || []).slice(0, 20)) out.push(div().flex_none().text_size(12).text_color(t.ansi3).child(`⚠ ${typeof warning === "string" ? warning : JSON.stringify(warning)}`));
    return out;
  }

  // The steps as rows, in source order: kind, label, line; a proven call target re-centers.
  stepList(t, data) {
    const rows = [], seq = this.seq;
    const walk = (steps, depth, alternate) => {
      for (const step of steps || []) {
        if (rows.length >= MAX_STEP_ROWS) return;
        const target = targetOf(data, step), line = step.range?.startLine;
        rows.push(h_flex().id(`step-${step.id}`).min_h(24).pl(6 + depth * 14).pr(6).gap(8).items_center().rounded(4).cursor_pointer()
          .when(seq.selected === step.id, (el) => el.bg(t.selected)).hover((st) => st.bg(t.hover))
          .on_click((_e, cx) => this.activateStep(step, cx))
          .child(div().w(64).flex_none().text_size(12).text_color(alternate ? t.ansi3 : t.dim).child(alternate ? `else ${step.kind}` : step.kind))
          .child(div().flex_1().min_w(0).truncate().font_family(settingsMono()).text_size(12).child(sequenceHelpers.compactLabel(step)))
          .when(Boolean(target), (el) => el.child(div().flex_none().text_size(12).text_color(t.accent).child(`→ ${target.declaration.name}`)))
          .when(step.kind === "group", (el) => el.child(div().flex_none().text_size(12).text_color(t.dim).child(seq.expanded.has(step.id) ? "collapse" : "expand")))
          .child(div().flex_none().text_size(12).text_color(t.dim).child(line ? `:${line}` : "")));
        walk(step.children, depth + 1, false);
        walk(step.alternate, depth + 1, true);
      }
    };
    walk(data.steps, 0, false);
    return v_flex().flex_none().gap(1)
      .child(div().font_semibold().text_size(14).pb(4).child("Steps"))
      .children(rows)
      .when(rows.length >= MAX_STEP_ROWS, (el) => el.child(this.note(t, `First ${MAX_STEP_ROWS} steps shown.`)));
  }

  // --- view ---

  render(cx) {
    const t = theme();
    const machine = this.shownMachine();
    if (machine !== this.machine) this.useMachine(machine, cx);
    const file = this.followed();
    if (file?.checkout && file.checkout.rootKey !== this.rootKey && file.checkout.state === "available" && file.checkout.rootKey !== this.followedSwitch) {
      // Follow the Files panel into the checkout that holds its file, once per checkout change.
      this.followedSwitch = file.checkout.rootKey;
      this.select(file.checkout.rootKey, cx);
    }
    const place = file?.checkout?.rootKey === this.rootKey ? this.placement(file) : null;
    const highlight = place?.group && place.layer ? [place.layer.name, place.group.name] : place?.group && !place.layer ? [null, place.group.name] : null;
    if (this.tab === "architecture") this.scheduleDraw(cx, highlight);
    else { this.followSequence(file, cx); this.scheduleSequenceDraw(cx); }

    return v_flex().id("root").size_full().min_h(0).bg(t.background).text_color(t.foreground).text_size(13)
      .on_size_change((size, cx) => { if (Math.abs(size.width - this.width) >= 1) { this.width = size.width; cx.notify(); } })
      .child(this.header(t, cx))
      .child(v_flex().id("trellis-body").flex_1().min_h(0).overflow_y_scrollbar().p(PAD).gap(12)
        .children(this.body(t, file, place, cx)));
  }

  header(t, cx) {
    const row = this.checkouts?.find((r) => r.rootKey === this.rootKey);
    const label = row ? indexLabel({ ...row, revision: this.status?.pin ?? row.revision, catchingUp: this.status?.catchingUp ?? row.catchingUp }) : null;
    const tone = label?.state === "fresh" ? t.ansi2 : label?.state === "catching-up" ? t.ansi3 : label?.state === "failed" ? t.ansi1 : t.muted;
    return v_flex().flex_none().border_b(1).border_color(t.border)
      .child(h_flex().h(40).px(PAD).gap(10).items_center()
        .child(div().font_semibold().text_size(14).child("Trellis"))
        .children(["architecture", "sequence"].map((tab) => div().id(`tab-${tab}`).px(8).h(24).flex().items_center().rounded(6).cursor_pointer()
          .text_color(this.tab === tab ? t.foreground : t.dim).when(this.tab === tab, (el) => el.bg(t.selected)).hover((s) => s.bg(t.hover))
          .on_click((_e, cx) => { this.tab = tab; try { localStorage.setItem("trellis.tab", tab); } catch { /* a convenience only */ } cx.notify(); })
          .child(tab === "architecture" ? "Architecture" : "Sequence")))
        .child(div().flex_1())
        .child(h_flex().id("checkout-button").gap(6).items_center().px(8).h(26).rounded(6).border_1().border_color(t.input || t.border)
          .cursor_pointer().hover((s) => s.bg(t.hover))
          .on_click((_e, cx) => { this.listOpen = !this.listOpen; cx.notify(); })
          .child(div().max_w(220).truncate().child(row ? basename(row.workspaceRoot || row.rootKey) : this.checkouts ? "Choose a checkout" : "Checkouts…"))
          .child(div().text_color(t.dim).child(this.listOpen ? "▴" : "▾"))))
      .when(Boolean(label), (el) => el.child(h_flex().px(PAD).pb(8).gap(6).items_center().text_size(12)
        .child(div().text_color(tone).child(label.shape))
        .child(div().text_color(tone).child(label.text))
        .child(div().flex_1().min_w(0).truncate().text_color(t.dim).child(`${row?.workspaceRoot || ""}${this.machine ? ` · on ${this.machine}` : ""}`))))
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
    const layout = this.machine ? machines().find((m) => m.name === this.machine)?.layout : "ready";
    if (layout === "unavailable") return [this.problem(t, new TrellisError("machine_unavailable", `Herdr can't reach ${this.machine} right now.`), "Machine")];
    if (this.listError) return [this.problem(t, this.listError, "Daemon")];
    if (!this.checkouts) return [this.note(t, "Reading the daemon's checkouts…")];
    if (!this.rootKey) return [this.note(t, !this.checkouts.length ? "No checkouts registered with this daemon."
      : this.checkouts.some((row) => row.state === "available") ? "Choose a checkout above, or select a file in Files." : "No available checkout. Choose one above when it is ready.")];
    if (this.statusError?.code === "index_not_ready") return [this.note(t, `Index not ready (index_not_ready: ${this.statusError.message}). Indexing starts when the browser, CLI or MCP first uses this checkout.`)];
    if (this.statusError) return [this.problem(t, this.statusError, "Index status")];
    if (!this.status) return [this.note(t, "Reading index status…")];
    if (!this.status.pin) return [this.note(t, this.status.catchingUp ? "Indexing this checkout. The diagram appears when the first revision is published." : "This checkout has no index yet.")];
    if (this.tab === "sequence") return this.sequenceBody(t, file);
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

function findStep(steps, id) {
  for (const step of steps || []) {
    if (step.id === id) return step;
    const found = findStep(step.children, id) || findStep(step.alternate, id);
    if (found) return found;
  }
  return null;
}
