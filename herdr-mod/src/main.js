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
import { appHome, focused, layout, machines, props, setShared, shared, spaces, theme } from "herdr";
import * as fs from "fs/promises";
import { TrellisError, architecturePayload, createClient, detailPage, indexLabel, jobCause, pinLabel, pinOf, samePin, validRootKey } from "./client.js";
import { CONTROLS, DEFAULT_AGGREGATE, DEFAULT_METRIC, ERROR, HEAT_INK, apps, deviationModel, draw, heat, heatFill, hit, plural, display } from "./diagram.js";
import * as ui from "./architecture-ui.js";
import { drawSequence, helpers as sequenceHelpers, targetOf } from "./sequence.js";
import { ensureFilesPanel } from "./panels.js";

// Polls are gentler for another machine: each one is a command run there.
const STATUS_MS = 5000, REMOTE_STATUS_MS = 10000, LIST_MS = 30000, BUILDING_MS = 250, MAX_BUILDING_MS = 5000;
const MAX_STEP_ROWS = 300, MAX_HISTORY = 20;
// A drawing yields to the host between phases (the runtime ends a task whose one synchronous
// run is too long, without an error), and one that hasn't finished by then is a typed error.
const DRAW_TIMEOUT_MS = 15000;
// Re-lay out only for a width change worth a new drawing (the browser's RESIZE_SLACK).
const WIDTH_SLACK = 8, PAD = 12;
const SELECTED_FILE = "trellis.selectedFile";
const HEAT_NAMES = ["low", "moderate", "high", "very high"];
// The Deviations tree's read (web/architecture-deviations.js PARAMS), and its page bound.
const DEVIATION_PARAMS = { kind: "instances", rule: "$layers", status: "deviates", role: "production", excepted: "false", limit: "500" };
const MAX_DEVIATION_PAGES = 40;
// The brand mark the web app's header shows, copied beside main.js by build.mjs.
const LOGO = "mods/trellis/trellis-mark.png";

const yieldTo = (cx) => () => cx.sleep(0);
const drawTimeout = (what) => new TrellisError("draw_timeout", `The ${what} drawing didn't finish within ${DRAW_TIMEOUT_MS / 1000} s, so the Herdr runtime may have ended it. It is tried again when the data, width or theme changes.`);
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
    // The web toolbar's choices (#886): Color by and Aggregation keep across checkouts, as in the browser.
    this.metric = DEFAULT_METRIC; this.aggregate = DEFAULT_AGGREGATE; this.menu = null;
    this.width = 0;
    this.props = p;
    // Follow (#809): the last agent-or-terminal focus and the last Files selection, each with a
    // sequence number, so the more recent one picks the checkout. Pinned stops agent follow.
    this.events = 0; this.agent = null; this.selection = null; this.focusKey = null; this.fileKey = null;
    this.pinned = (() => { try { return localStorage.getItem("trellis.pinned") === "true"; } catch { return false; } })();
    this.files = {};
    this.tab = (() => { try { return localStorage.getItem("trellis.tab") === "sequence" ? "sequence" : "architecture"; } catch { return "architecture"; } })();
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

  // --- data ---

  // Every async read below owns a token for its one request: a result or rejection that is no
  // longer the current request is dropped whole, and touches no successor's state or flags.
  loadCheckouts(cx) {
    const client = this.client, run = {};
    if (!client) return;
    this.listRun = run;
    const live = () => client === this.client && run === this.listRun;
    cx.spawn(async (cx) => {
      try {
        const data = await client.checkouts();
        if (!live()) return;
        if (!Array.isArray(data?.checkouts)) throw new TrellisError("invalid_response", "Checkout list is invalid.");
        this.checkouts = data.checkouts.filter((row) => row && validRootKey(row.rootKey) && typeof row.state === "string");
        this.listError = null;
        this.follow(cx);
      } catch (error) {
        if (!live()) return;
        this.listError = fail(error); this.checkouts = null;
        this.select(null, cx);
      }
      cx.notify();
    });
  }

  select(rootKey, cx) {
    if (rootKey === this.rootKey) return;
    this.rootKey = rootKey; this.status = null; this.statusError = null; this.view = null; this.viewError = null; this.building = null;
    // A new checkout starts a new baseline of agent shows (#818).
    this.showSeen = null; this.shown = null;
    this.open = new Map(); this.tried = null; this.drawError = null;
    // A new checkout starts at All apps, with nothing inspected and no Deviations tree (#789, #886).
    this.scope = null; this.drilled = new Set(); this.unassignedOpen = false; this.inspect = null; this.devs = null; this.job = null;
    this.seq = { history: [], expanded: new Set() };
    this.serial = (this.serial || 0) + 1;
    // A read still in flight for the old checkout is obsolete: the new one issues its own.
    this.statusRun = null; this.archRun = null; this.loading = false;
    if (rootKey) { try { localStorage.setItem("trellis.checkout", rootKey); } catch { /* a convenience only */ } }
    if (rootKey) this.refreshStatus(cx);
  }

  // --- following the agent and the Files selection (#809) ---

  // Records a new focus or Files selection as an event. A focused mod pane (this one included)
  // has no cwd, so it leaves the last agent-or-terminal focus in place.
  // At start the focus is recorded after the selection, so it wins over an older selection.
  observe() {
    const value = shared(SELECTED_FILE);
    const fileKey = value && typeof value === "object" ? JSON.stringify([value.root, value.path, value.on ?? null]) : null;
    if (fileKey !== this.fileKey) {
      this.fileKey = fileKey;
      if (fileKey) this.selection = { seq: ++this.events };
    }
    const f = focused();
    const key = f ? `${f.space}|${f.pane}|${f.terminal}|${f.cwd}` : null;
    if (key !== this.focusKey) {
      this.focusKey = key;
      if (typeof f?.cwd === "string" && f.cwd.startsWith("/")) this.agent = { cwd: f.cwd.replace(/\/+$/, "") || "/", seq: ++this.events };
    }
  }

  // The checkout the more recent event points at: the deepest daemon checkout that holds the
  // agent's folder, or the one that holds the Files selection. Reading a checkout's status starts
  // indexing it, so nothing else is ever opened, and a folder in no listed checkout is a note.
  follow(cx) {
    if (!this.checkouts || this.fatal) return;
    // Pinned: agent focus is ignored, and only a selection made after pinning switches.
    const agent = this.pinned ? null : this.agent;
    const floor = this.pinned ? this.pinnedSeq : agent?.seq ?? 0;
    const file = this.selection && this.selection.seq > floor ? this.followed() : null;
    let target, unknown = null;
    if (file) {
      if (!file.checkout) return; // the file card says it isn't in a served checkout
      target = file.checkout;
    } else if (agent) {
      target = containing(this.checkouts, agent.cwd);
      if (!target) unknown = { folder: agent.cwd };
    } else if (!this.rootKey) {
      // Nothing focused or selected yet: the checkout named in props or shown last time.
      const saved = validRootKey(this.props.checkout) ? this.props.checkout : (() => { try { return localStorage.getItem("trellis.checkout"); } catch { return null; } })();
      target = this.checkouts.find((row) => row.rootKey === saved) || null;
      if (!target) return;
    } else return;
    if (target && target.state !== "available") unknown = { folder: target.workspaceRoot, state: target.state };
    this.unknown = unknown;
    const key = unknown ? null : target.rootKey;
    if (key !== this.rootKey) this.select(key, cx);
  }

  setPinned(pinned, cx) {
    // Pinning while an agent's Sequence is still being read stops that show: the pane goes
    // back to what it showed before (#824).
    if (pinned && this.shown?.back && this.seq?.agent && this.seq.loading) this.backFromShow(cx);
    this.pinned = pinned; this.pinnedSeq = this.events;
    try { localStorage.setItem("trellis.pinned", String(pinned)); } catch { /* a convenience only */ }
    // Unpinning follows the agent focused now, as if it had just been focused.
    if (!pinned && this.agent) this.agent = { ...this.agent, seq: ++this.events };
    this.follow(cx);
    cx.notify();
  }

  // One status read at a time per checkout; `statusBusy` is whether the current one is in flight.
  get statusBusy() { return Boolean(this.statusRun); }

  refreshStatus(cx) {
    const rootKey = this.rootKey, serial = this.serial, client = this.client, run = {};
    if (!rootKey || this.statusRun) return;
    this.statusRun = run;
    const live = () => run === this.statusRun && serial === this.serial;
    cx.spawn(async (cx) => {
      try {
        const data = await client.status(rootKey);
        if (!live()) return;
        const pin = data?.revision == null ? null : pinOf(data.revision);
        const changed = !samePin(pin, this.status?.pin);
        this.status = { pin, catchingUp: data?.catchingUp ?? null, workspaceRoot: data?.workspaceRoot ?? null };
        this.statusError = null;
        if (changed || (!this.view && !this.viewError && !this.loading)) this.loadArchitecture(cx);
        if (changed) this.reloadSequence(cx);
        this.observeShow(data?.viewRequest, cx);
        // The status line's "Last re-index failed" note (web/app.js renderIndexStatus). Best effort:
        // a daemon or `trellis api` without the route leaves the note out.
        try { const job = await client.job(rootKey); if (live()) this.job = job && typeof job === "object" ? job : null; }
        catch { if (live()) this.job = null; }
      } catch (error) {
        if (!live()) return;
        // Never keep data from a checkout whose status can't be read.
        this.status = null; this.statusError = fail(error); this.view = null; this.viewError = null; this.archRun = null; this.loading = false;
      } finally {
        if (run === this.statusRun) this.statusRun = null;
        cx.notify();
      }
    });
  }

  // --- agent shows (#818) ---

  // The first request seen for a checkout is a baseline; a newer generation is a show, which a
  // pinned pane counts as seen and ignores. Only this pane's display changes, never focus.
  observeShow(value, cx) {
    const request = value == null ? null : viewRequestOf(value);
    if (value != null && !request) return;
    const generation = request?.generation ?? 0;
    if (!this.showSeen || this.showSeen.rootKey !== this.rootKey) { this.showSeen = { rootKey: this.rootKey, generation }; return; }
    if (generation <= this.showSeen.generation) return;
    this.showSeen.generation = generation;
    if (this.pinned) return;
    this.showRequest(request, cx);
  }

  showRequest(request, cx) {
    const back = this.shown?.back ?? null;
    const label = request.view === "source" ? `${request.path}:${request.line}` : request.view === "class" ? "Class diagram" : "Sequence";
    // The deep link's stale-pin rule: a target from another snapshot is not shown.
    if (!samePin(request.revision, this.status?.pin)) {
      this.shown = { text: `The agent showed a ${label} at index ${pinLabel(request.revision)}; this pane is at ${this.status?.pin ? pinLabel(this.status.pin) : "no index"}.`, back };
      return;
    }
    if (request.view !== "sequence") {
      this.shown = { text: `The agent showed ${label}. This pane has no ${request.view === "class" ? "Class" : "source"} view; open it in the browser.`, back };
      return;
    }
    const before = back ?? { tab: this.tab, seq: this.seq };
    const prev = this.seq;
    this.tab = "sequence";
    // The Files selection's method list stays; only the shown method changes.
    this.seq = { file: prev.file, methods: prev.methods, methodsError: prev.methodsError, history: [], expanded: new Set(), agent: true };
    this.loadSequence({ id: request.symbolId, name: null, path: null }, false, cx);
    this.shown = { text: "Shown by agent · Sequence", back: before };
  }

  // Back returns to the tab and sequence shown before the agent's first show.
  backFromShow(cx) {
    const back = this.shown?.back;
    this.shown = null;
    if (back) {
      this.tab = back.tab; this.seq = back.seq;
      // The earlier drawing's file was replaced, so it is drawn again.
      if (this.seq) this.seq.drawn = null;
      this.seqTried = null;
      // Never an older revision's payload under this header: read it again at this pin.
      const data = this.seq?.data;
      if (this.seq?.seed && !this.seq.loading && (!data || !samePin(data.revision, this.status?.pin))) this.reloadSequence(cx);
    }
    cx.notify();
  }

  shownBar(t) {
    if (!this.shown) return [];
    return [h_flex().id("agent-shown").flex_none().gap(8).items_center().px(8).py(4).rounded(6).bg(t.surface).border_1().border_color(t.border)
      .child(div().flex_1().min_w(0).whitespace_normal().text_size(12).text_color(t.muted).child(this.shown.text))
      .when(Boolean(this.shown.back), (el) => el.child(div().id("agent-shown-back").px(6).rounded(4).border_1().border_color(t.border).cursor_pointer()
        .hover((st) => st.bg(t.hover)).on_click((_e, cx) => this.backFromShow(cx)).child("Back")))
      .child(div().id("agent-shown-close").text_color(t.dim).cursor_pointer().child("×").on_click((_e, cx) => { this.shown = null; cx.notify(); }))];
  }

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

  // The Deviations tree sees the chosen app's slice and that app's counts (web/architecture.js deviationsView).
  deviationView() {
    const view = this.view, group = this.scope === null || !(view?.apps?.length >= 2) ? null : apps.group(view, this.scope);
    return group ? { ...apps.scoped(view, group.name), rules: { ...view.rules, counts: group.counts } } : view;
  }

  // Opens the Deviations tree, on one edge ("source→destination") or all of them, reading every
  // page of production `$layers` deviations at the summary's pin and rules hash.
  openDeviations(edge, cx) {
    const view = this.view, pin = this.status?.pin, rootKey = this.rootKey, serial = this.serial, client = this.client;
    if (!view || view.state !== "ready" || !pin) return;
    const group = this.scope === null || !(view.apps?.length >= 2) ? null : apps.group(view, this.scope);
    const params = group ? { ...DEVIATION_PARAMS, app: group.name } : { ...DEVIATION_PARAMS };
    const devs = { edge, expanded: new Set(), base: null, tree: null, error: null, progress: "" };
    this.devs = devs;
    const live = () => this.devs === devs && serial === this.serial && view === this.view;
    cx.spawn(async (cx) => {
      try {
        const rows = [];
        for (let offset = 0, pages = 0; offset !== null; pages++) {
          if (pages >= MAX_DEVIATION_PAGES) throw new TrellisError("too_many_deviations", "Too many deviation pages to list here. Open the Deviations tree in the browser.");
          const page = detailPage(await client.details(rootKey, pin, view.rulesFile.contentHash, params, offset), pin, view.rulesFile.contentHash, params.kind, offset);
          if (!live()) return;
          rows.push(...page.items); offset = page.nextOffset;
          if (offset !== null) { devs.progress = `Reading deviations… ${rows.length.toLocaleString("en-US")} of ${page.total.toLocaleString("en-US")} instances`; cx.notify(); }
        }
        if (!rows.every(deviationModel.valid)) throw new TrellisError("invalid_response", "Invalid Architecture detail response. Nothing displayed.");
        devs.base = deviationModel.build(rows, view.layers, null);
        devs.tree = deviationModel.filter(devs.base, { edge, callsOnly: false });
      } catch (error) {
        if (!live()) return;
        devs.error = fail(error);
      } finally { cx.notify(); }
    });
    cx.notify();
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
    // In All apps a band's stat chips count its own app's slice.
    const part = summary.plan.parts.find((p) => p.slice.modules.includes(module)) ?? summary.plan.parts[0];
    return { module, layer, group, rank: group ? groups.indexOf(group) : -1, groups: groups.length, members: members.length,
      hidden: layer ? !summary.layers.has(layer.name) : false, stats: layer ? summary.layerStats(layer, part.group?.name, part.slice) : [] };
  }

  // A press on the diagram (image `index`): a band opens or folds, as its header does in the
  // browser; a directory module opens one level; the unassigned row opens; an upward ↑ chip or a
  // deviating arrow opens the Deviations tree on its edge; an interface stub inspects the
  // interface; anything else is inspected by its full accessible label.
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

  // --- Sequence ---

  // Follows the Files selection to that file's methods; the first consequential named method
  // opens unless one is open already for this file.
  followSequence(file, cx) {
    const path = file?.checkout?.rootKey === this.rootKey ? file.path : null;
    if (!path || !this.status?.pin || this.seq.file === path) return;
    this.seq = { file: path, history: [], expanded: new Set() };
    this.loadMethods(cx);
  }

  // A new revision reloads what the pane shows and, during an agent show, what Back returns to:
  // a sequence is never shown under another revision's header (#824).
  reloadSequence(cx) {
    const back = this.shown?.back?.seq;
    if (back?.seed && back !== this.seq) this.loadSequence(back.seed, false, cx, back);
    if (!this.seq?.file) {
      if (this.seq?.seed) this.loadSequence(this.seq.seed, false, cx);
      return;
    }
    const keep = this.seq.seed;
    this.seq = { ...this.seq, methods: null, methodsError: null, data: null, error: null, drawn: null, drawError: null, keep };
    this.seqTried = null;
    this.loadMethods(cx);
  }

  loadMethods(cx) {
    const { rootKey, serial, client } = this, pin = this.status?.pin, seq = this.seq, run = {};
    seq.methodsRun = run;
    const live = () => serial === this.serial && seq === this.seq && run === seq.methodsRun;
    cx.spawn(async (cx) => {
      try {
        const data = await client.methods(rootKey, pin, seq.file);
        if (!live()) return;
        if (!Array.isArray(data?.items)) throw new TrellisError("invalid_response", "Invalid method list. Nothing displayed.");
        seq.methods = data.items.map((item) => item?.symbol).filter((symbol) => typeof symbol?.id === "string" && typeof symbol.name === "string");
        const named = seq.methods.filter((m) => !m.name.startsWith("<"));
        const first = seq.methods.find((m) => m.id === seq.keep?.id) || named.find((m) => data.items.find((i) => i.symbol === m)?.consequential) || named[0] || seq.methods[0];
        if (first) this.loadSequence(first, false, cx);
      } catch (error) {
        if (!live()) return;
        seq.methodsError = fail(error);
      } finally { cx.notify(); }
    });
  }

  // Opens one method's sequence; `push` keeps the current one for Back (a re-center).
  // `seq` is the shown sequence, or the one an agent show's Back returns to.
  loadSequence(symbol, push, cx, seq = this.seq) {
    const { rootKey, serial, client } = this, pin = this.status?.pin, run = {};
    if (push && seq.seed) seq.history = [...seq.history, seq.seed].slice(-MAX_HISTORY);
    Object.assign(seq, { seed: { id: symbol.id, name: symbol.name, path: symbol.path }, data: null, drawn: null, error: null, selected: null, expanded: new Set(), loading: true, run });
    // Only the newest sequence request of this view may set data, an error or loading.
    const live = () => serial === this.serial && (seq === this.seq || seq === this.shown?.back?.seq) && run === seq.run;
    cx.spawn(async (cx) => {
      try {
        const data = await client.sequence(rootKey, pin, symbol.id);
        if (!live()) return;
        // As the browser checks it: the answer is for this revision and this seed, whole.
        if (!samePin(data?.revision, pin) || data?.seed?.id !== symbol.id || !Array.isArray(data.steps) || !Array.isArray(data.participants)) {
          throw new TrellisError("revision_conflict", "Sequence provenance mismatch. Nothing displayed.");
        }
        seq.data = data; seq.seed = { id: data.seed.id, name: data.seed.name, path: data.seed.path };
      } catch (error) {
        if (!live()) return;
        seq.error = fail(error);
      } finally { if (live()) seq.loading = false; cx.notify(); }
    });
  }

  scheduleSequenceDraw(cx) {
    const seq = this.seq, data = seq?.data;
    if (!data || this.seqDrawing) return;
    const t = theme();
    const key = JSON.stringify([data.revision, data.seed.id, seq.selected, [...seq.expanded], t.background, t.foreground, t.accent, t.selected]);
    if (this.seqTried === key) return;
    this.seqDrawing = true; this.seqTried = key;
    const serial = this.serial, run = {};
    this.seqRun = run;
    const watchdog = cx.timer.after(DRAW_TIMEOUT_MS, (cx) => {
      if (this.seqRun !== run) return;
      this.seqRun = null; this.seqDrawing = false;
      if (serial === this.serial && seq === this.seq && data === seq.data) { seq.drawn = null; seq.drawError = drawTimeout("Sequence"); }
      cx.notify();
    });
    const live = () => this.seqRun === run && serial === this.serial && seq === this.seq && data === seq.data;
    cx.spawn(async (cx) => {
      try {
        const pause = yieldTo(cx);
        await pause();
        if (!live()) return;
        const out = await drawSequence(data, { theme: t, selectedId: seq.selected, expanded: seq.expanded, pause });
        if (!live()) return;
        const asset = out.svg ? await this.saveDrawing("sequence", out.svg) : null;
        if (!live()) return;
        seq.drawn = asset ? { ...out, data, asset } : { ...out, data };
        seq.drawError = null;
      } catch (error) {
        if (!live()) return;
        seq.drawn = null; seq.drawError = fail(error);
      } finally {
        watchdog.cancel();
        if (this.seqRun === run) { this.seqRun = null; this.seqDrawing = false; }
        cx.notify();
      }
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
    this.observe();
    this.follow(cx);
    const file = this.followed();
    const place = file?.checkout?.rootKey === this.rootKey ? this.placement(file) : null;
    const highlight = place?.group ? [place.layer?.name ?? null, place.group.name, place.module.app ?? null] : null;
    if (this.tab === "architecture") this.scheduleDraw(cx, highlight);
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
        .children(["architecture", "sequence"].map((tab) => div().id(`tab-${tab}`).px(8).h(24).flex().items_center().rounded(6).cursor_pointer()
          .text_color(this.tab === tab ? t.foreground : t.dim).when(this.tab === tab, (el) => el.bg(t.selected)).hover((s) => s.bg(t.hover))
          .on_click((_e, cx) => { this.tab = tab; try { localStorage.setItem("trellis.tab", tab); } catch { /* a convenience only */ } cx.notify(); })
          .child(tab === "architecture" ? "Architecture" : "Sequence")))
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
    const controls = ui.controls(this, t);
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
    const drawn = this.current();
    // The switcher and controls come from the drawing's summary, so they always match what is drawn.
    const out = [...controls];
    if (drawn) out.push(...ui.switcher(this, t, drawn.summary, view));
    if (drawn && this.devs) return [...out, ...ui.deviations(this, t, drawn.summary, view)];
    if (file) out.push(this.fileCard(t, file, place));
    if (this.drawError) out.push(this.problem(t, this.drawError, "Diagram"));
    else if (drawn?.summary.noLayers) out.push(...ui.noLayers(t, drawn.summary, view));
    else if (drawn) out.push(...ui.drawing(this, t, drawn));
    else out.push(this.note(t, "Drawing the layer stack…"));
    if (drawn) out.push(...ui.legend(t, drawn.summary, view), ...ui.inspector(this, t, drawn.summary, view), this.layerList(t, place));
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
    const { module, layer, group } = place, summary = this.drawn.summary, metric = summary.metric;
    const value = metric === "layer" ? undefined : heat.metricValue(module.metrics, metric);
    const band = value === undefined ? null : summary.scale.band(value);
    card.child(h_flex().gap(6).items_center().flex_wrap()
      .child(div().text_color(t.dim).child("Layer"))
      .child(div().font_semibold().child(layer ? `${layer.shared ? "Shared" : layer.order} · ${layer.name}` : "Unassigned / unknown"))
      .when(Boolean(module.app), (el) => el.child(div().text_size(12).text_color(t.dim).child(`app ${module.app}`)))
      .when(place.hidden, (el) => el.child(div().text_size(12).text_color(t.dim).child(summary.tests.has(layer.name) ? "(tests/fixtures layer, not drawn)" : "(hidden layer, not drawn)")))
      .children(place.stats.map((stat) => this.stat(t, stat))));
    if (group) {
      const drawnChip = place.rank >= 0 && this.drawn.images.some((one) => one && [...(one.projection?.nodes.values() ?? [])].some((n) => n.kind === "C" && n.record.group === group.name && n.record.layerKey === (layer?.name ?? null)));
      card.child(h_flex().gap(6).items_center().flex_wrap()
        .child(div().text_color(t.dim).child("Module"))
        .child(this.chip(t, group.name, group.value === undefined ? null : summary.scale.band(group.value)))
        .child(div().text_color(t.muted).child(`${plural(group.files.length, "file")}${group.value === undefined ? "" : ` · ${metric === "maintainabilityIndex" ? "min" : "max"} ${metric} ${display(group.value)}`} · #${place.rank + 1} of ${place.groups} in the layer`))
        .child(div().text_size(12).text_color(t.dim).child(drawnChip ? "outlined in the diagram" : "in the band's “more” chip")));
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
          .child(div().w(52).text_color(t.dim).child(layer.shared ? "Shared" : `Order ${layer.order}`))
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

// A status poll's `viewRequest` (#818), or null when it isn't one this pane understands.
function viewRequestOf(value) {
  if (!value || typeof value !== "object" || !Number.isSafeInteger(value.generation) || value.generation < 1) return null;
  let revision;
  try { revision = pinOf(value.revision); } catch { return null; }
  if ((value.view === "sequence" || value.view === "class") && typeof value.symbolId === "string" && value.symbolId)
    return { generation: value.generation, view: value.view, symbolId: value.symbolId, revision };
  if (value.view === "source" && typeof value.path === "string" && value.path && Number.isSafeInteger(value.line) && value.line >= 1)
    return { generation: value.generation, view: "source", path: value.path, line: value.line, revision };
  return null;
}

// The deepest listed checkout that holds `folder`.
function containing(checkouts, folder) {
  return checkouts.filter((row) => typeof row.workspaceRoot === "string" && (folder === row.workspaceRoot || folder.startsWith(`${row.workspaceRoot.replace(/\/+$/, "")}/`)))
    .sort((a, b) => b.workspaceRoot.length - a.workspaceRoot.length)[0] || null;
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
