// The pane's checkout (#707, #809): the daemon's checkout list, the one the focused agent or the
// Files selection points at, the pin that stops following, and that checkout's index status.
// These are methods of the pane (main.js installs them on its class unchanged); `this` is the pane.
import { focused, shared } from "herdr";
import { TrellisError, pinOf, samePin, validRootKey } from "./client.js";
import { SELECTED_FILE, fail } from "./common.js";

export class Checkout {
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
}

// The deepest listed checkout that holds `folder`.
function containing(checkouts, folder) {
  return checkouts.filter((row) => typeof row.workspaceRoot === "string" && (folder === row.workspaceRoot || folder.startsWith(`${row.workspaceRoot.replace(/\/+$/, "")}/`)))
    .sort((a, b) => b.workspaceRoot.length - a.workspaceRoot.length)[0] || null;
}
