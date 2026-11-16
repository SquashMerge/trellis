// The Deviations tab (#939): the tree's read at the summary's pin and rules hash, its filter, and a
// line's action. These are methods of the pane (main.js installs them on its class unchanged); the
// tree's rows are in architecture-ui.js.
import { TrellisError, detailPage } from "./client.js";
import { apps, deviationModel, summarize } from "./diagram.js";
import { DEVIATION_PARAMS, MAX_DEVIATION_PAGES, fail } from "./common.js";

export class Deviations {
  // The Deviations tree sees the chosen app's slice and that app's counts (web/architecture.js deviationsView).
  deviationView() {
    const view = this.view, group = this.scope === null || !(view?.apps?.length >= 2) ? null : apps.group(view, this.scope);
    return group ? { ...apps.scoped(view, group.name), rules: { ...view.rules, counts: group.counts } } : view;
  }

  // The Deviations tab on one edge ("source→destination") or all of them (#939): the review
  // chip, an arrow on the diagram and an app's deviation count open it with their filter.
  openDeviations(edge, cx) {
    this.setTab("deviations", cx);
    this.loadDeviations(edge, cx);
  }

  // An app's deviation count: that app's slice, every edge.
  openAppDeviations(name, cx) {
    this.setScope(name, cx);
    this.openDeviations(null, cx);
  }

  // A deviation line, as the browser's main line action: the caller's sequence when the line
  // has one (Open in Sequence); otherwise its location, since this pane has no source view.
  openLine(line, cx) {
    const e = line.evidence;
    if (deviationModel.canOpenSequence(e)) {
      this.setTab("sequence", cx);
      const seed = { id: e.caller.syntaxId, name: e.caller.name ?? null, path: e.caller.document?.path ?? null };
      // The Files selection's method list stays, as when the agent shows a sequence; reading it
      // for a newer selection doesn't replace the caller's sequence.
      const file = this.followed(), path = file?.checkout?.rootKey === this.rootKey ? file.path : null;
      if (path && path !== this.seq.file) { this.seq = { file: path, history: [], expanded: new Set() }; this.loadMethods(cx); }
      this.loadSequence(seed, true, cx, this.seq, "caller");
      return;
    }
    const path = e.document?.path ?? line.instance.target.path;
    if (this.devs) this.devs.notice = `${e.lines ? `${path}:${e.lines.start}` : path} · This pane has no source view; open it in the browser.`;
    cx.notify();
  }

  // The tree's summary (its app and edge words): the drawing's when it is current, else the
  // payload's own for the chosen app, without drawing.
  deviationSummary() {
    const drawn = this.current();
    if (drawn && drawn.choice?.scope === this.scope) return drawn.summary;
    if (this.devSummary?.view !== this.view || this.devSummary.scope !== this.scope) this.devSummary = { view: this.view, scope: this.scope, summary: summarize(this.view, { scope: this.scope }) };
    return this.devSummary.summary;
  }

  // Reads the Deviations tree: every page of production `$layers` deviations at the summary's
  // pin and rules hash. A tree already read for this payload and app is only filtered again.
  loadDeviations(edge, cx) {
    const view = this.view, pin = this.status?.pin, rootKey = this.rootKey, serial = this.serial, client = this.client;
    if (!view || view.state !== "ready" || !pin) return;
    const held = this.devs;
    if (held?.base && !held.error) {
      held.edge = edge; held.tree = deviationModel.filter(held.base, { edge, callsOnly: false }); held.notice = null;
      return cx.notify();
    }
    const group = this.scope === null || !(view.apps?.length >= 2) ? null : apps.group(view, this.scope);
    const params = group ? { ...DEVIATION_PARAMS, app: group.name } : { ...DEVIATION_PARAMS };
    const devs = { edge, expanded: new Set(), base: null, tree: null, error: null, progress: "", notice: null };
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
}
