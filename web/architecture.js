"use strict";
// One owned code snapshot plus a separate live rules digest; edits never publish an index.
(() => {
  const get = id => document.getElementById(id);
  const make = (tag, text, cls) => {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (cls) node.className = cls;
    return node;
  };
  // The pure display and helpers (architecture-display.js, loaded first).
  const {createDisplay, moduleKey, commonDirectory, groupModules, external, plural, BOX_CAP} = window.TrellisArchitectureDisplay;
  const targetLabel = identity => external(identity)
    ? `${identity.package} (${identity.language}${identity.standardLibrary ? " stdlib" : " package"})` : identity.path;
  const {metricValue, quartiles, relativeBand:band, exclusions} = window.TrellisMetricHeat;
  // Resize re-layout: the quiet period, and the width change that is worth a new drawing.
  const RESIZE_DEBOUNCE_MS = 150, RESIZE_SLACK = 8;
  const BUILD_PHASES = {reading:"reading indexed files", resolving:"resolving references", evaluating:"evaluating rules", measuring:"measuring code"};
  // The app choice when the page shares none: memory only, per checkout.
  function ownScope(context) {
    let root = null, name = null;
    return {get:() => root === context().rootKey ? name : null, set:next => { root = context().rootKey; name = next; }};
  }
  // `navigate` moves between the Architecture and Deviations tabs (#965): `deviations({app, edge})` shows the
  // Deviations tab on that filter, `diagram()` shows Architecture, and `filter({app, edge})` reports a filter
  // chosen inside the tab, so the page's link follows it. Without one, both tabs are this controller's own.
  function create({request, context, readSource, openTreemap, openSequence, libraryButton = () => null, onConflict = () => {}, onInspect = () => {}, inspector = {}, onRulesChanged = () => {}, appScope = null, navigate = null}) {
    let serial = 0, renderSerial = 0, selectionSerial = 0, view = null, basis = null, buildingTimer = null, buildingWake = null;
    let metric = "cognitive", aggregate = "max", role = "production", relative = false, showTests = false;
    // The app whose stack is drawn: null for all apps, else an app name (never compared with a literal). Memory only; a new checkout starts at "all".
    // The choice is the page's, shared with Treemap, Classes and the Files tree (#852): `scopes.get()` reads it at the
    // current checkout and `scopes.set` changes it for every view. `drawnScope` is the choice the last render drew.
    // `drawn` is the view the last render laid out (the scoped slice, or `view` itself).
    const scopes = appScope || ownScope(context);
    let drawn = null, drawnScope = null;
    // The tab this controller shows: "diagram" (Architecture), "tree" (Deviations, #965) or null (neither).
    // A read for the tree tab draws nothing; Architecture draws the held snapshot when it is shown again.
    // `afterDraw` is what Show on diagram asks of the next drawing: an edge to highlight, or an inspection.
    let tab = "diagram", afterDraw = null, routing = false;
    // The band that holds the selected stub's end, so its label can say `client end` / `server end` (AC25).
    let endMark = null;
    let reviewMode = "unassigned", draft = null, preview = null, editSerial = 0, editBusy = false, busyOwner = 0;
    const callables = () => role === "all" ? "indexed callables" : "production callables";
    const population = () => role === "all" ? "all indexed callables" : callables();
    // The hidden-layer note moves into the canvas after each drawing, so keep its nodes.
    const hiddenNote = get("architecture-hidden"), hiddenToggle = get("architecture-hidden-toggle"), hiddenNames = get("architecture-hidden-names");
    // The selected node's identity, re-pressed after a resize re-layout; the width last laid out.
    let pressed = null, laidOut = null, inspection = null;
    const apps = window.TrellisArchitectureApps;
    // The summary display, shared with the Herdr mod (#886): band labels, chips, budgets, parts and scale.
    const shared = createDisplay({metric:() => metric, aggregate:() => aggregate, role:() => role, relative:() => relative, showTests:() => showTests,
      view:() => view, model:() => model(), shown:() => shown, endMark:() => endMark, scopeGroup:payload => scopeGroup(payload)});
    const {layerLabel, expanded, edgeText, coverageSummary, ruleCounts} = shared;
    const deviations = window.TrellisDeviations.create({details:(params, active, offset) => details(appFilter(params), active, offset), current:() => usable(),
      basis:() => basis, view:() => deviationsView(), targetLabel, eligible, portable, beginDraft:(operation, scope) => beginDraft(operation, scope), reviewChips,
      edgeText, inspectLine:(record, render) => inspectLine(record, render), overview:() => overview(), inspectUnknown:() => { if (usable()) showDiagram({unknown:true}); },
      showDiagram:chip => { if (usable()) showDiagram({edge:chip ?? null}); }, onFilter:chip => navigate?.filter({app:scopeName(), edge:chip ?? null}),
      onConflict:() => onConflict(), refresh:() => refresh(), readSource:(site, pin, active) => readSource(site, pin, active),
      openSequence:(...args) => openSequence(...args), snapshot:() => snapshot(),
      // One page of the Files outline at the view's pin; it names a closure's enclosing callable (#845).
      outline:async (path, offset, active) => {
        const c = basis, query = new URLSearchParams({indexGeneration:c.pin.indexGeneration, indexRevision:String(c.pin.indexRevision), path, offset:String(offset), limit:"200"});
        const data = await request(`/api/symbols?${query}`, active);
        if (!window.TrellisIndexPin.equal(data?.revision, c.pin) || !Array.isArray(data?.items) || !(data.nextOffset === null || (Number.isSafeInteger(data.nextOffset) && data.nextOffset > offset))) {
          throw new Error("Invalid outline response.");
        }
        return data;
      }});
    // The Deviations tab shows the same read state while its snapshot is read (#965).
    const state = (message, kind = "ready") => {
      for (const id of ["architecture-state", "deviations-state"]) { const node = get(id); if (node) { node.textContent = message; node.dataset.state = kind; } }
    };
    function capture() {
      const c = context();
      return {rootKey:c.rootKey, session:c.session, pin:c.pin && window.TrellisIndexPin.copy(c.pin), aggregate, role, serial, rulesEpoch:c.rulesEpoch};
    }
    function current(c) {
      const now = context();
      return c && c.serial === serial && c.aggregate === aggregate && c.role === role && c.rootKey === now.rootKey && c.session === now.session &&
        !!c.pin && window.TrellisIndexPin.equal(c.pin, now.pin);
    }
    function usable() { return view && current(basis) && view.rulesFile?.contentHash === basis.rulesHash; }
    // The app group the view is scoped to; null for the whole checkout (and for a name the data no longer has).
    // With fewer than two detected apps there is no switcher, so no scope: the stack is checkout-wide.
    const scopeGroup = (payload = view) => scopes.get() === null || payload?.state !== "ready" || !(payload.apps?.length >= 2) ? null : apps.group(payload, scopes.get());
    const scopeName = (payload = view) => scopeGroup(payload)?.name ?? null;
    // A snapshot that cannot show the chosen app ends the choice, for every view: it never returns
    // with the apps. Settling draws nothing here; the caller renders.
    let settling = false;
    const settleScope = payload => {
      if (scopes.get() === null || scopeGroup(payload)) return;
      settling = true;
      try { scopes.set(null); } finally { settling = false; }
    };
    // The Deviations tree sees the app's slice: its layers and edges, and the group's counts.
    let treeView = null;
    function deviationsView() {
      const group = scopeGroup();
      if (!group) return view;
      if (treeView?.of !== view || treeView.scope !== group.name) treeView = {of:view, scope:group.name, value:{...apps.scoped(view, group.name), rules:{...view.rules, counts:group.counts}}};
      return treeView.value;
    }
    const model = () => drawn ?? view;
    const unassignedOf = (payload, group) => payload.modules.filter(module => module.layer === null && (!group || module.app === group.name));
    // A one-app scope narrows instance lists to that app; every other list is unchanged.
    const appFilter = params => { const group = scopeGroup(); return group && params.kind === "instances" ? {...params, app:group.name} : params; };
    function clearInspection() {
      selectionSerial++; pressed = null; inspection = null;
      inspector.clear?.();
      // Leaving a stub drops its band's word; the drawing is swapped in place, keeping what else is selected.
      if (endMark) { endMark = null; if (tab === "diagram" && usable() && view.state === "ready") void render({keepSelection:true}); }
    }
    function reset(message = "Open Architecture to read the indexed snapshot and reviewed rules.", keepDraft = false, keepReopen = false) {
      drawn = null; get("architecture-apps").hidden = true; get("architecture-source-line").hidden = true; get("architecture-key-interface").hidden = true; get("architecture-footnote").hidden = true;
      editSerial++; preview = null;
      if (!keepDraft) setBusy(false);
      if (!keepDraft) { draft = null; get("architecture-review-panel").hidden = true; get("architecture-editor").hidden = true; }
      get("architecture-preview-result").hidden = true;
      // The Deviations tab's open tree reopens on its chip at the next snapshot (#965).
      const reopen = tab === "tree" && deviations.isOpen();
      if (reopen) deviations.reopenAfterRefresh();
      serial++; renderSerial++; clearInspection(); deviations.reset(keepReopen || reopen); view = null; basis = null;
      clearTimeout(buildingTimer); buildingWake?.(); buildingTimer = buildingWake = null;
      get("architecture-diagram").replaceChildren(); get("architecture-diagram").setAttribute("aria-busy", "false");
      get("architecture-legend").replaceChildren(); get("architecture-notice-content").replaceChildren();
      get("architecture-notices").open = false;
      hiddenNote.hidden = true; get("architecture-summary").textContent = ""; get("architecture-coverage").textContent = ""; get("architecture-exclusions").textContent = "";
      state(message, "empty");
    }
    function payloadOf(data, c) {
      const payload = data?.payload;
      if (!payload || !window.TrellisIndexPin.equal(payload, c.pin)) {
        const error = new Error("Architecture snapshot changed. Refresh status."); error.code = "revision_conflict"; error.status = 409; throw error;
      }
      if (payload.state === "building") return payload;
      if (!window.TrellisDtoCore.architecturePayload(payload, c.aggregate, c.role)) throw new Error("Invalid Architecture response. Nothing displayed.");
      return payload;
    }
    function paragraph(target, text, cls) { target.append(make("p", text, cls)); }
    function disclosure(target, title, value) {
      const details = make("details"); details.append(make("summary", title), make("pre", JSON.stringify(value, null, 2))); target.append(details);
    }
    function coverage() {
      const target = get("architecture-notice-content"); target.replaceChildren();
      paragraph(target, `Code ${window.TrellisIndexPin.label(view)} · rules ${view.rulesFile.contentHash} · ${aggregate} · ${population()}.`);
      const references = view.references;
      // One app counts the sites from its own layers, as the arrow key's coverage does (#913).
      const {rows, group:scoped} = shared.coverageRows(view), sum = key => rows.reduce((total, row) => total + (row[key] ?? 0), 0);
      const counts = scoped ? {mappedSites:sum("mappedSites"), semanticSites:sum("semanticSites"), unresolvedSites:sum("unresolvedSites")} : references?.counts;
      paragraph(target, `${counts?.mappedSites ?? 0} mapped reference sites · ${counts?.semanticSites ?? 0} bound by semantic evidence · ${counts?.unresolvedSites ?? 0} unresolved${scoped ? ` from ${scoped.name}'s layers` : ""}. Missing sites do not prove absence.`);
      if (ruleCounts(view)) paragraph(target, ruleCounts(view));
      for (const limitation of references?.limitations || []) paragraph(target, limitation);
      const group = scopeGroup(), unassigned = unassignedOf(view, group);
      paragraph(target, view.state === "no_layers" ? "No layer graph or file assignment inventory is available until layers are configured." : `${unassigned.length} unassigned native file modules remain unknown${group ? ` in ${group.name}` : ""}.`);
      if (view.state === "ready" && Number.isSafeInteger(view.rules?.unassignedCount)) paragraph(target, `${view.rules.unassignedCount} unassigned identities (native files and external packages) in the rule-check catalog${group ? " (whole checkout)" : ""}.`);
      if (references?.externalPackages?.length) disclosure(target, "External packages and captured evidence", references.externalPackages);
      const choose = (label, action) => { const button = make("button", label); button.type = "button"; button.addEventListener("click", action); target.append(button); };
      const c = basis;
      choose("Inspect same-layer and unassigned references", () => { if (current(c) && usable()) inspect("unknown", null); });
      choose("Inspect unresolved sites", () => { if (current(c) && usable()) inspect("unresolved", null); });
      if (references?.externalPackages?.length) choose("Inspect external policy results", () => { if (current(c) && usable()) inspect("external", null); });
    }
    // Undrawn detail is read on demand, one bounded page at a time, at the view's exact
    // pin and rules hash (#448). A changed pin or rules file fails with a typed 409.
    async function details(params, active, offset = 0) {
      const c = basis;
      const query = new URLSearchParams({...params, indexGeneration:c.pin.indexGeneration, indexRevision:String(c.pin.indexRevision),
        rulesHash:c.rulesHash, offset:String(offset)});
      const data = await request(`/api/architecture/details?${query}`, active);
      const page = data?.payload;
      if (!page || !window.TrellisIndexPin.equal(page, c.pin) || !window.TrellisDtoCore.detailPage(page, c.rulesHash, params.kind, offset)) {
        throw new Error("Invalid Architecture detail response. Nothing displayed.");
      }
      return page;
    }
    // One paged detail list; "Show more" reads the next page of the same scope.
    function detailList(target, params, active, renderItems, empty) {
      const list = make("div", undefined, "architecture-detail-list"), items = make("div"), footer = make("div");
      list.dataset.detailKind = params.kind; list.append(items, footer); target.append(list);
      async function load(offset) {
        footer.replaceChildren(make("p", "Loading details…", "detail"));
        try {
          const page = await details(params, active, offset);
          if (!active()) return;
          footer.replaceChildren(); renderItems(items, page.items);
          if (!page.total && empty) paragraph(items, empty);
          if (page.nextOffset !== null) {
            const more = make("button", `Show more (${page.total - page.nextOffset} more of ${page.total})`); more.type = "button";
            more.addEventListener("click", () => { if (active()) return load(page.nextOffset); });
            footer.append(more);
          }
        } catch (error) {
          if (!active() || error.name === "AbortError") return;
          // The view re-reads on a new revision; a rules edit on disk needs this explicit reload.
          footer.replaceChildren(make("p", error.code === "rules_conflict" ? "Rules changed since this diagram was loaded." :
            error.message || "Details unavailable.", "detail"));
          const reload = make("button", "Reload Architecture"); reload.type = "button";
          reload.addEventListener("click", () => { if (!editBusy) return refresh(); }); footer.append(reload);
          if (error.status === 409 && ["revision_conflict", "pin_expired", "root_changed"].includes(error.code)) onConflict();
        }
      }
      void load(0);
    }
    const pairItems = active => (list, items) => pairs(list, items.map(item => item.pair), active, false);
    const instanceItems = active => (list, items) => renderInstances(list, items, active, false);
    // Summary-first display (#391): presentation state only, remembered per checkout.
    let shown = null, shownRoot = null;
    const strings = value => Array.isArray(value) ? value.filter(item => typeof item === "string") : [];
    const storageKey = rootKey => `trellis.architecture.display.v1:${rootKey}`;
    function loadDisplay(rootKey) {
      if (shown && rootKey === shownRoot) return;
      shownRoot = rootKey; shown = {open:new Map(), drilled:new Set(), unassigned:false, hidden:false};
      try {
        const saved = JSON.parse(window.localStorage.getItem(storageKey(rootKey)) || "null");
        if (saved && typeof saved === "object") shown = {
          open:new Map((Array.isArray(saved.open) ? saved.open : []).filter(pair => Array.isArray(pair) && typeof pair[0] === "string" && typeof pair[1] === "boolean")),
          drilled:new Set(strings(saved.drilled)), unassigned:saved.unassigned === true, hidden:saved.hidden === true};
      } catch { /* Browser storage is a convenience; the defaults stay usable. */ }
    }
    function saveDisplay() {
      try {
        window.localStorage.setItem(storageKey(shownRoot), JSON.stringify({open:[...shown.open], drilled:[...shown.drilled],
          unassigned:shown.unassigned, hidden:shown.hidden}));
      } catch { /* Not remembered; the current page still applies it. */ }
    }
    function displayState(payload, layout) {
      const files = layers => payload.modules.filter(module => layers.some(layer => layer.name === module.layer)).length;
      hiddenNote.hidden = !layout.hidden.length;
      const toggle = hiddenToggle;
      toggle.textContent = `${shown.hidden ? "▾" : "▸"} ${plural(layout.hidden.length, "hidden layer")} · ${plural(files(layout.hidden), "file")} · ${plural(layout.hiddenEdges, "production edge")}`;
      toggle.setAttribute("aria-expanded", String(shown.hidden));
      hiddenNames.textContent = layout.hidden.map(layer => layout.tests.has(layer.name) ? `${layer.name} (tests/fixtures; include them to show)` : layer.name).join(", ");
    }
    // Built from what they read: the drawn nodes' identities (architecture-drawing.js), the app switcher and the
    // All apps columns (architecture-app-columns.js), and the inspector kinds (architecture-inspector.js).
    const {nodeKey, findNode, pressedNode, refocus} = window.TrellisArchitectureDrawing.create({get, hiddenToggle});
    async function rerender(target, after) {
      const c = basis;
      if (!await render() || !usable() || !current(c)) return;
      after?.(); refocus(target);
    }
    const {switcher, syncCards, drawApps} = window.TrellisArchitectureAppColumns.create({get, make, apps, shared, scopeName, scopeGroup, setScope, openTab, inspect, pressOnly, openReview, asideBoxes});
    // A `↑` badge or a deviating arrow opens the Deviations tab on its edge, unselected and with
    // Calls only off (AC10, #845, #965).
    function openDeviations(action) {
      if (!usable() || view.state !== "ready" || editBusy) return;
      return openTab({app:scopeName(), edge:`${action.sourceLayer}→${action.destinationLayer}`});
    }
    // The Deviations tab on a filter: the review chip's (every edge), an arrow's (its edge) or an app's
    // deviation count (that app, every edge). The page moves the tab and its link; alone, this controller does.
    function openTab(filter) {
      if (!usable() || editBusy) return;
      return navigate ? navigate.deviations(filter) : showDeviations(filter);
    }
    // Show on diagram (#965): Architecture, with the tree's edge pressed and focused where it is drawn.
    function showDiagram(next) {
      afterDraw = next;
      return navigate ? navigate.diagram() : reopen();
    }
    // What Show on diagram asked of this drawing. An edge on the tree's chip is the production deviation
    // arrow between its layers; in All apps the first column that draws it.
    function settleDraw() {
      const next = afterDraw;
      afterDraw = null;
      if (next?.unknown) return inspect("unknown", null);
      if (typeof next?.edge !== "string") return;
      const node = [...(get("architecture-diagram").querySelectorAll?.("[data-trellis-edge-id]") || [])].find(item => {
        try {
          const [source, destination, role, status, excepted] = JSON.parse(item.getAttribute("data-trellis-edge-id"));
          return `${source}→${destination}` === next.edge && role === "production" && status === "deviates" && excepted === false;
        } catch { return false; }
      });
      if (!node) return;
      pressOnly(node); node.focus?.(); node.scrollIntoView?.({block:"nearest", inline:"nearest"});
    }
    // The review panel (exception drafts, unassigned and authored lists) sits above the shown tab's content.
    function placeReview() {
      const panel = get("architecture-review-panel"), tree = tab === "tree";
      const host = get(tree ? "deviations-panel" : "architecture-panel");
      const before = tree ? get("architecture-deviations") : host?.querySelector?.(".architecture-status");
      if (panel && host?.insertBefore && before && panel.parentNode !== host) host.insertBefore(panel, before);
    }
    // The Deviations tab (#965): the held snapshot when nothing it rests on changed, else a read that
    // draws nothing. `filter.app` chooses the app for every view (undefined keeps it); `filter.edge` is
    // the tree's chip (undefined keeps the tree's own). The tree reads once per snapshot and app.
    function showDeviations(filter = {}) {
      tab = "tree"; afterDraw = null; placeReview();
      // The caller's route already names this app and edge; the app change it makes must not rewrite it to every edge.
      if (filter.app !== undefined && filter.app !== scopes.get()) {
        routing = true;
        try { scopes.set(filter.app); } finally { routing = false; }
      }
      if (!held()) {
        if (filter.edge !== undefined) deviations.want(filter.edge);
        return refresh();
      }
      basis = {...basis, serial}; coverage(); state("", "ready"); overview();
      return deviations.show(filter.edge);
    }
    // The tree's filter, as the Deviations tab's link names it.
    const deviationsFilter = () => ({app:scopeName(), edge:deviations.filter() ?? null});
    // The arrows the tree lists: production deviations that no exception covers.
    const treeEdge = edge => edge?.role === "production" && edge.status === "deviates" && !edge.excepted;
    function setScope(next) {
      if (!usable() || view.state !== "ready" || editBusy || next === scopeName()) return;
      if (next !== null && !apps.group(view, next)) return;
      scopes.set(next);
      return scopeChanged();
    }
    // The page's choice changed, here or in another view: a drawn snapshot follows it at once and
    // never reads; a hidden or busy one draws it when it next renders.
    let following = null;
    function scopeChanged() {
      if (settling || !usable() || view.state !== "ready" || editBusy) return following;
      // The Deviations tab reads the new app's tree, on every edge, and its link follows (#965).
      if (tab === "tree") {
        deviations.reset(); coverage(); overview();
        if (!get("architecture-review-panel").hidden) renderReview();
        if (!routing) navigate?.filter({app:scopeName(), edge:null});
        return deviations.show(null);
      }
      if (scopeName() === drawnScope) return following;
      deviations.reset(); coverage();
      if (!get("architecture-review-panel").hidden) renderReview();
      return following = render();
    }
    // AC13: an app that no ordered layer names has nothing to draw. It gets the setup help and its
    // unassigned files, so none is dropped. The list is bounded like a drawing; the chip lists all.
    function noLayers(full) {
      const group = scopeGroup(full), files = unassignedOf(full, group), canvas = get("architecture-diagram"), ticket = renderSerial;
      get("architecture-legend").replaceChildren(); hiddenNote.hidden = true;
      get("architecture-coverage").textContent = coverageSummary(full);
      get("architecture-summary").textContent = `${plural(files.length, "unassigned file")} in ${group.name} · no layers yet.`;
      get("architecture-exclusions").textContent = "";
      const list = make("ul", undefined, "architecture-app-files");
      for (const module of files.slice(0, BOX_CAP)) {
        const item = make("li"), button = make("button", module.identity.path); button.type = "button";
        button.addEventListener("click", () => { if (ticket === renderSerial) inspect("module", module); });
        item.append(button); list.append(item);
      }
      if (files.length > BOX_CAP) list.append(make("li", `${plural(files.length - BOX_CAP, "file")} not listed here · open the unassigned files chip to list them all`));
      canvas.replaceChildren(list); laidOut = canvas.clientWidth;
      state(`${group.name} has no layers yet. Its files are listed as unassigned below. The trellis_propose_rules MCP prompt can propose layers per app; this browser does not call it. Review .trellis/rules.toml to add layers with app = "${group.name}".`, "empty");
      return true;
    }
    const flip = (set, key) => { if (set.has(key)) set.delete(key); else set.add(key); };
    function legend(scale) {
      const target = get("architecture-legend"); target.replaceChildren();
      paragraph(target, shared.legendText(scale));
      if (metric === "layer") return;
      for (const {label, band} of scale.entries) {
        const entry = make("span", undefined, "metric-legend-entry");
        entry.append(make("span", undefined, `metric-swatch${band === null ? " neutral" : ` heat-${band}`}`), make("span", label)); target.append(entry);
      }
      if (relative) target.append(make("span", "Low heat → high heat · ties stay together · — unavailable"));
    }
    // `keepSelection` re-lays out the same view for a new panel width (#515, #525): the
    // inspector and the selected node stay; the drawing is swapped in place.
    async function render({keepSelection = false} = {}) {
      if (!usable() || view.state !== "ready") return false;
      // `full` is the snapshot; `payload` is what this drawing shows: the whole stack, or one app's slice of it.
      // With no app chosen and two or more named apps, the drawing is one column per app (#790).
      const c = basis, full = view, ticket = ++renderSerial;
      if (!keepSelection) endMark = null;
      const scope = scopeName(full);
      // The parts, their layouts and budgets, and the one heat scale: shared with the Herdr mod (#886).
      const {allApps, payload, parts, layout, scale} = shared.plan(full, scope);
      drawnScope = scope;
      const active = () => current(c) && view === full && ticket === renderSerial && full.rulesFile.contentHash === c.rulesHash;
      drawn = payload; switcher(full);
      get("architecture-key-interface").hidden = !allApps;
      // The inspector's review overview opens before measuring, so the drawing fits the width it gets.
      if (!keepSelection) { clearInspection(); overview(); get("architecture-diagram").replaceChildren(); }
      if (scope !== null && !payload.layers.some(layer => !layer.shared)) return noLayers(full);
      legend(scale);
      displayState(payload, layout);
      get("architecture-coverage").textContent = coverageSummary(full);
      // The canvas's inner width; the adapter fits that many chip columns. A re-layout
      // returns keyboard focus to the same band, chip, arrow or toggle afterwards.
      const canvas = get("architecture-diagram"), drawnFor = canvas.clientWidth;
      const focused = keepSelection && canvas.contains?.(document.activeElement) ? nodeKey(document.activeElement) : null;
      const style = window.getComputedStyle?.(canvas), padding = style ? parseFloat(style.paddingLeft) + parseFloat(style.paddingRight) : 0;
      const stack = (part, extra) => {
        const slice = part.slice;
        return {
          ...shared.options(part, scale, extra), isCurrent:active,
          onOpenDeviation:action => {
            const named = name => slice.layers.some(layer => layer.name === name);
            if (active() && named(action?.sourceLayer) && named(action?.destinationLayer)) return openDeviations(action, document.activeElement);
          },
          onSelectInterface:action => {
            const entry = apps.link(slice, action?.interface?.client, action?.interface?.server);
            if (!active() || !entry) return;
            inspect("interface", entry);
            const stub = typeof action.id === "string" ? findNode(["data-trellis-action-id", action.id]) : null;
            for (const node of get("architecture-diagram").querySelectorAll?.("[aria-pressed]") || []) node.setAttribute("aria-pressed", String(node === stub));
            pressed = nodeKey(stub);
            endMark = {layer:action.layer, side:action.side}; void render({keepSelection:true});
          },
          onSelectModule:module => { if (active() && slice.modules.includes(module)) { inspect("module", module); pressed = nodeKey(pressedNode()); } },
          onSelectEdge:edge => {
            if (!active() || !slice.edges.includes(edge)) return;
            if (treeEdge(edge)) { const node = pressedNode(); node?.setAttribute("aria-pressed", "false"); return openDeviations(edge, node); }
            inspect("edge", edge); pressed = nodeKey(pressedNode());
          },
          onSelectLayer:layer => { if (active() && slice.layers.includes(layer)) inspect("layer", layer); },
          onToggleLayer:layer => {
            if (!active() || !slice.layers.includes(layer)) return;
            shown.open.set(layer.name, !expanded(layer)); saveDisplay();
            return rerender({layer:layer.name}, () => inspect("layer", layer));
          },
          onToggleDirectory:entry => {
            if (!active() || typeof entry?.key !== "string") return;
            flip(shown.drilled, entry.key); saveDisplay(); return rerender({directory:entry.key});
          },
          onToggleUnassigned:() => { if (!active()) return; shown.unassigned = !shown.unassigned; saveDisplay(); return rerender({unassigned:true}); },
          onSelectNote:entry => {
            if (!active()) return;
            const layer = slice.layers.find(item => item.name === entry?.layerKey);
            if (layer) inspect("layer", layer); else inspect("unknown", null);
            pressed = nodeKey(pressedNode());
          },
        };
      };
      try {
        if (!window.TrellisArchitecturePele) throw new Error("Architecture renderer unavailable. Nothing displayed.");
        const width = canvas.clientWidth - (padding || 0);
        if (allApps) {
          if (!await drawApps({full, parts, canvas, width, stack, active, layout})) return false;
        } else {
          const outcome = await window.TrellisArchitecturePele.render(canvas, payload, stack(parts[0], {width, ...(scope === null ? {} : {title:`${scope} · ordered layers`})}));
          if (!active()) return false;
          if (outcome?.status !== "ready") throw new Error(outcome?.reason || outcome?.error?.message || "Architecture mapping failed. Nothing displayed.");
        }
        laidOut = drawnFor; if (canvas.dataset) canvas.dataset.layoutWidth = String(drawnFor);
        // The hidden-layer note is part of the drawing's layout, right under it (#525).
        canvas.append(hiddenNote);
        if (keepSelection) {
          // Only a selectable node is re-pressed; a band that stands in for a removed note only takes focus.
          const again = findNode(pressed);
          if (again?.hasAttribute("aria-pressed")) again.setAttribute("aria-pressed", "true");
          syncCards();
          findNode(focused)?.focus();
        }
        const production = payload.edges.filter(edge => edge.role === "production").reduce((sum, edge) => sum + edge.count, 0);
        const tests = payload.edges.filter(edge => edge.role === "test").reduce((sum, edge) => sum + edge.count, 0);
        const truncated = parts.reduce((sum, part) => sum + part.budget.truncated, 0);
        // The counts live behind the ⓘ with the heat exclusions; nothing but the drawing sits above it (#449, #525).
        get("architecture-summary").textContent = `${payload.modules.length} file modules · ${production} production / ${tests} test-only cross-layer sites${showTests ? "" : " (tests hidden)"}${truncated ? ` · ${plural(truncated, "file")} not drawn (size cap ${BOX_CAP} chips)` : ""} · Metrics: ${aggregate}, ${population()}.`;
        state("", "ready");
        get("architecture-exclusions").textContent = `Heat: ${exclusions(payload.modules)}.`;
        if (!keepSelection) settleDraw();
        return true;
      } catch (error) {
        if (!active()) return false;
        get("architecture-diagram").replaceChildren(); get("architecture-legend").replaceChildren(); clearInspection();
        state(error.message || "Architecture rendering failed. Nothing displayed.", "error");
        return false;
      }
    }
    // Presses one drawn control and no other; the selection survives a re-layout through its action id.
    function pressOnly(node) {
      for (const other of get("architecture-diagram").querySelectorAll?.("[aria-pressed]") || []) other.setAttribute("aria-pressed", String(other === node));
      pressed = nodeKey(node); syncCards();
    }
    // Below the columns: a dashed box for each hidden layer, which is the toggle's face while the layers are hidden,
    // and the unassigned files per app plus those outside every app. Counts are the DTO's.
    function asideBoxes(full, parts, layout, active) {
      const aside = make("div", undefined, "architecture-apps-aside"), seen = new Set();
      const open = () => { if (!active()) return; openReview("unassigned"); };
      if (!shown.hidden) for (const part of parts) for (const layer of part.layout.hidden) {
        if (seen.has(layer.name)) continue;
        seen.add(layer.name);
        const files = full.modules.filter(module => module.layer === layer.name).length;
        const box = make("button", undefined, "architecture-aside-box"); box.type = "button"; box.dataset.kind = "hidden"; box.setAttribute("aria-expanded", "false");
        box.append(make("span", `${layer.name} · ${layer.shared ? "shared" : part.group.name}`, "architecture-card-line"), make("span", `${plural(files, "file")} · hidden`, "architecture-card-line"),
          make("span", layout.tests.has(layer.name) ? `${layer.name} (tests/fixtures; include them to show)` : layer.name, "architecture-card-ends"));
        box.addEventListener("click", () => { if (!active()) return; shown.hidden = !shown.hidden; saveDisplay(); void rerender({hidden:true}); });
        aside.append(box);
      }
      const unassigned = (name, count) => {
        if (!(count > 0)) return;
        const box = make("button", undefined, "architecture-aside-box"); box.type = "button"; box.dataset.kind = "unassigned"; box.dataset.tone = "warning";
        box.append(make("span", name, "architecture-card-line"), make("span", `◇ ${count} unassigned · listed, never dropped`, "architecture-card-line"));
        box.addEventListener("click", open); aside.append(box);
      };
      for (const part of parts) unassigned(part.group.name, part.group.unassignedFileCount);
      unassigned("no app", full.noApp?.unassignedFileCount);
      return aside;
    }
    // The daemon computes Architecture once per revision (#556). Until it has, a read answers
    // `building` with progress. `refresh` asks again and settles only on the outcome, which
    // the daemon answers as soon as it is done.
    function buildingMessage(progress) {
      const phase = BUILD_PHASES[progress?.phase] || "preparing";
      const count = progress?.completed > 0 && progress?.total > 0 ? ` ${progress.completed} of ${progress.total}` : "";
      return `Building Architecture for this revision: ${phase}${count}…`;
    }
    async function refresh(options = {}) {
      reset(undefined, options.keepDraft === true, true);
      const c = capture();
      if (!c.rootKey || !c.pin) { state("Choose an indexed checkout, then open Architecture.", "empty"); return; }
      loadDisplay(c.rootKey);
      state("Reading indexed architecture and reviewed rules…", "loading"); get("architecture-diagram").setAttribute("aria-busy", "true");
      return read(c, options);
    }
    // Opening the tab again draws the held snapshot when nothing it rests on changed: the checkout,
    // session, pin, aggregate, role and the rules epoch (a rules edit or conflict advances it).
    // Anything else reads again. A stale serial never revives: `suspend` bumped it, and the held
    // basis only adopts the current one here.
    function held() {
      const now = context();
      return !!view && view.state === "ready" && !!basis && basis.rootKey === now.rootKey && basis.session === now.session &&
        !!now.pin && window.TrellisIndexPin.equal(basis.pin, now.pin) && basis.aggregate === aggregate && basis.role === role &&
        basis.rulesEpoch === now.rulesEpoch;
    }
    function reopen() {
      tab = "diagram"; placeReview();
      if (!held()) return refresh();
      basis = {...basis, serial}; coverage();
      return render();
    }
    function suspend() {
      const kept = {view, basis};
      tab = null; afterDraw = null;
      reset(undefined, false, false);
      view = kept.view; basis = kept.basis;
    }
    function pause(delay) {
      return new Promise(resolve => { buildingWake = resolve; buildingTimer = setTimeout(resolve, delay); });
    }
    async function read(c, options) {
      let settled = false;
      try {
        const query = new URLSearchParams({aggregate:c.aggregate, role:c.role, workbench:"true", indexGeneration:c.pin.indexGeneration, indexRevision:String(c.pin.indexRevision)});
        let data, payload;
        for (;;) {
          data = await request(`/api/architecture?${query}`, () => current(c));
          if (!current(c)) return;
          payload = payloadOf(data, c);
          if (payload.state !== "building") break;
          state(buildingMessage(payload.progress), "loading");
          await pause(Number.isSafeInteger(payload.retryAfterMs) && payload.retryAfterMs > 0 ? payload.retryAfterMs : 250);
          if (!current(c)) return;
        }
        if (payload.state === "missing_rules") {
          state("No reviewed rules file. Create or review .trellis/rules.toml. The trellis_propose_rules MCP prompt can help propose apps and layers; this browser does not call it.", "empty"); return;
        }
        view = payload; basis = {...c, rulesHash:payload.rulesFile.contentHash}; settleScope(payload); coverage();
        if (payload.state === "no_layers") { state("No authored layers are configured. Existing authored exceptions can still be edited or removed. Review .trellis/rules.toml to configure layers.", "empty"); overview(); }
        // The Deviations tab draws nothing; Architecture draws this snapshot when it is shown (#965).
        else if (tab === "tree") { state("", "ready"); overview(); }
        else await render();
        if (options.keepDraft === true) { get("architecture-review-panel").hidden = false; renderReview(); }
        settled = true;
        // A history restore's own read reopens its saved tree itself; any other read opens the tab's tree.
        if (tab === "tree" && payload.state === "ready" && options.restoring !== true) void deviations.afterRefresh();
      } catch (error) {
        if (!current(c) || error.name === "AbortError") return;
        const diagnostic = error.diagnostic;
        state(diagnostic ? `${diagnostic.code || "invalid_rules"} · ${diagnostic.path}:${diagnostic.line}:${diagnostic.column} · ${diagnostic.kind} · ${diagnostic.message}` : error.message || "Architecture unavailable. Select the Architecture tab again to retry.", "error");
        if (diagnostic) disclosure(get("architecture-notice-content"), "Located rules diagnostic", diagnostic);
        if (error.status === 409 && ["revision_conflict", "pin_expired", "root_changed"].includes(error.code)) onConflict();
      } finally {
        if (current(c)) { get("architecture-diagram").setAttribute("aria-busy", "false"); if (!settled) deviations.reset(); }
      }
    }
    function same(a, b) { return moduleKey(a) === moduleKey(b); }
    function inspectorSource(target, identity, site, active, label = "Read indexed reference source") {
      const lines = site?.lines;
      const range = lines && Number.isSafeInteger(lines.start) && lines.start > 0 && Number.isSafeInteger(lines.end) && lines.end >= lines.start
        ? {startLine:lines.start, endLine:lines.end, startByte:site.range?.start, endByte:site.range?.end} : null;
      if (!range) return;
      const button = make("button", label); button.type = "button";
      button.addEventListener("click", () => { if (active()) return readSource({path:identity.path, range}, basis.pin, active); }); target.append(button);
    }
    const NO_PAIRS = "No mapped pairs in this scope. This does not prove absence.";
    function boundaryCallers(target, pairs, active) {
      const groups = new Map(), unavailable = [];
      for (const pair of pairs) for (const site of pair.sites) {
        const caller = site.caller;
        if (!caller?.syntaxId || !caller.stableLookupKey || !site.callId) { unavailable.push(site); continue; }
        const key = JSON.stringify([caller.document.sourceSetId, caller.document.language, caller.document.path, caller.syntaxId]);
        if (!groups.has(key)) groups.set(key, {caller, sites:new Map(), sampled:false});
        const group = groups.get(key);
        group.sites.set(site.callId, site); group.sampled ||= pair.omittedSites > 0;
      }
      for (const {caller, sites:byCall, sampled} of groups.values()) {
        const sites = [...byCall.values()];
        const group = make("div", undefined, "architecture-caller"); group.dataset.symbolId = caller.syntaxId;
        paragraph(group, `${caller.name} · ${caller.document.path} · ${sites.length} ${sampled ? "sampled " : ""}boundary site${sites.length === 1 ? "" : "s"}`);
        paragraph(group, "Measured calling method; destination evidence remains as captured.", "detail");
        if (typeof openSequence === "function") action(group, "Open sequence", () => openSequence(caller, sites, basis.pin, snapshot(), active), active);
        target.append(group);
      }
      for (const site of unavailable) paragraph(target,
        `${site.spelling} · ${site.callerReason || "No measured calling method is available for this site."}`,
        "architecture-caller-unavailable detail");
    }
    function pairs(target, values, active, emptyNote = true) {
      if (!values.length) { if (emptyNote) paragraph(target, NO_PAIRS); return; }
      boundaryCallers(target, values, active); pairDetails(target, values, active);
    }
    function pairDetails(target, values, active) {
      for (const pair of values) {
        const details = make("details"); details.append(make("summary", `${targetLabel(pair.source)} → ${targetLabel(pair.target)} · ${pair.count} reference sites`));
        paragraph(details, `${pair.sites.length} sampled sites · ${pair.omittedSites} omitted sites. Counts remain exact; samples are not exhaustive.`);
        disclosure(details, "Exact reference-pair identity", {from:pair.source, to:pair.target, evidenceTier:pair.evidenceTier, semanticSites:pair.semanticSites});
        if (external(pair.target)) {
          const descriptor = view.references.externalPackages.find(item => same(item.identity, pair.target));
          disclosure(details, "External package evidence", descriptor);
          // In-context library entry (#450); null unless the catalog indexes this package.
          const open = libraryButton(pair.target);
          if (open) details.append(open);
        }
        for (const site of pair.sites) {
          const sample = make("details"); sample.append(make("summary", `${site.role} · ${site.relationKind} · ${site.spelling}`));
          sample.append(make("pre", JSON.stringify(site, null, 2))); inspectorSource(sample, pair.source, site, active); details.append(sample);
        }
        target.append(details);
      }
    }
    const {values, evidenceSummary, ruleEvidence, inspectEdge, inspectLayer, inspectFound, inspectUnresolved, inspectExternal, inspectUnlayered, renderInstances} = window.TrellisArchitectureInspector.create({make, paragraph, disclosure, targetLabel, apps, layerLabel, appFilter, detailList, pairItems, instanceItems, boundaryCallers, pairDetails, inspectorSource, NO_PAIRS, inspect, overview, action, inspectRule, eligible});
    function openReview(mode) {
      if (!usable()) return;
      reviewMode = mode; get("architecture-review-panel").hidden = false; renderReview();
    }
    // Counted review chips head every Architecture inspection (#449); each opens its list.
    // File assignments count native modules; the rule-check catalog also includes external identities.
    function reviewChips(target) {
      const group = scopeGroup(), chips = make("div", undefined, "architecture-review-chips"), counts = shared.reviewCounts(view, group);
      chips.setAttribute("role", "group"); chips.setAttribute("aria-label", "Review counts");
      const chip = (id, count, word, open, tone) => {
        const button = make("button", `${count.toLocaleString("en-US")} ${word}`, "architecture-chip"); button.type = "button"; button.id = id;
        if (tone) button.dataset.tone = tone;
        button.addEventListener("click", () => { if (usable() && !editBusy) return open(button); }); chips.append(button);
      };
      if (counts.deviates !== null) chip("architecture-review", counts.deviates, counts.deviates === 1 ? "deviation" : "deviations", () => openTab({app:scopeName(), edge:null}), counts.deviates ? "error" : null);
      if (counts.unassigned !== null) chip("architecture-unassigned", counts.unassigned, counts.unassigned === 1 ? "unassigned file" : "unassigned files", () => openReview("unassigned"));
      if (counts.exceptions !== null) chip("architecture-authored", counts.exceptions, counts.exceptions === 1 ? "exception" : "exceptions", showAuthored);
      target.append(chips);
    }
    // The inspector's resting content once a snapshot is shown: review chips and coverage.
    // It opens the desktop column but never the narrow-screen drawer.
    function overview() {
      if (!usable() || !["ready", "no_layers"].includes(view.state)) return;
      clearInspection(); inspector.show?.({drawer:false});
      // Nothing is selected any more, so no drawn node may say it is (a stub shows the word Selected).
      for (const node of get("architecture-diagram").querySelectorAll?.('[aria-pressed="true"]') || []) node.setAttribute("aria-pressed", "false");
      syncCards();
      get("architecture-inspector-title").textContent = "Architecture review";
      const target = get("architecture-inspector-content"); target.replaceChildren();
      reviewChips(target);
      paragraph(target, view.state === "ready" ? "Select a band, module chip or arrow to inspect its evidence." : "Authored exceptions stay editable without layers.", "detail");
    }
    // One inspector per kind (`inspectors` below). Each fills the cleared inspector for its record.
    function inspectModule({target, title, record, c, active}) {
      title.textContent = record.identity.path;
      paragraph(target, `Layer: ${record.layer ?? "Unassigned / unknown"} · ${record.physicalLines} physical file lines · ${record.declarationCount} ${callables()}.`);
      paragraph(target, `${aggregate} · metrics:v1 · ${population()}. Callable-range LOC may overlap; it is not physical file lines.`);
      disclosure(target, "Native file-module identity and captured source", {identity:record.identity, revisionId:record.revisionId, contentHash:record.contentHash, provenanceId:record.provenanceId});
      if (record.excludedReason) paragraph(target, `Excluded from heat: ${record.excludedReason} · ${exclusions([record])}`);
      else if (record.declarationCount === 0) paragraph(target, `No ${callables()}. Numeric aggregates are unavailable, not zero.`);
      values(target, record.metrics);
      if (record.physicalLines > 0) {
        const button = make("button", "Read indexed file source"); button.type = "button";
        button.addEventListener("click", () => { if (active()) return readSource({path:record.identity.path, range:{startLine:1,endLine:record.physicalLines}}, c.pin, active); }); target.append(button);
      }
      if (typeof openTreemap === "function") {
        const button = make("button", "Open in Treemap"); button.type = "button";
        button.addEventListener("click", () => { if (active()) openTreemap(record.identity, c.pin); }); target.append(button);
      }
      paragraph(target, "Reference pairs from or to this file", "detail");
      detailList(target, {kind:"pairs", path:record.identity.path}, active, pairItems(active), NO_PAIRS);
      paragraph(target, "Rule evaluation for this file (including exceptions and test-only outcomes)", "detail");
      detailList(target, {kind:"instances", path:record.identity.path}, active, instanceItems(active), "No rule instances name this file.");
    }
    // Any other kind (the callers pass "unknown") is the same-layer and unassigned inspector.
    const inspectors = {module:inspectModule, edge:inspectEdge, layer:inspectLayer, interface:inspectFound, app:inspectFound, unresolved:inspectUnresolved, external:inspectExternal};
    function inspect(kind, record) {
      if (!usable() || view.state !== "ready") return;
      clearInspection(); inspection = {kind, record}; const ticket = selectionSerial, payload = view, c = basis;
      const active = () => usable() && current(c) && view === payload && ticket === selectionSerial && !get("architecture-inspector").hidden;
      onInspect(); inspector.show?.();
      const target = get("architecture-inspector-content"), title = get("architecture-inspector-title"); target.replaceChildren();
      reviewChips(target);
      paragraph(target, `${evidenceSummary(payload)} · code ${window.TrellisIndexPin.label(payload)} · rules ${c.rulesHash}.`);
      const references = payload.references;
      const draw = Object.hasOwn(inspectors, kind) ? inspectors[kind] : inspectUnlayered;
      if (draw({kind, record, target, title, payload, c, active, references}) === false) return;
      for (const limitation of references.limitations || []) paragraph(target, limitation, "detail");
    }
    function portable(identity) { return {language:identity.language, path:identity.path}; }
    function eligible({instance:r, role:rowRole}) {
      return rowRole === "production" && r.rule === "$layers" && r.status === "deviates" && !r.excepted &&
        !!r.target?.path && !!r.destination?.path && !external(r.target) && !external(r.destination) && r.referenceCount > 0;
    }
    function action(target, label, fn, active = () => usable()) {
      const button = make("button", label); button.type = "button";
      button.addEventListener("click", () => { if (active() && !editBusy) return fn(); }); target.append(button); return button;
    }
    // One selected Deviations line: the module renders the panel body into the shared inspector.
    function inspectLine(record, render) {
      clearInspection(); inspection = {kind:"deviation", record};
      const ticket = selectionSerial, payload = view, c = basis;
      const active = () => usable() && view === payload && current(c) && ticket === selectionSerial && !get("architecture-inspector").hidden;
      onInspect(); inspector.show?.();
      const target = get("architecture-inspector-content"); target.replaceChildren(); reviewChips(target);
      render(target, get("architecture-inspector-title"), active);
      return active;
    }
    function inspectRule(row) {
      if (!usable()) return;
      clearInspection(); inspection = {kind:"rule", record:row}; const ticket = selectionSerial, payload = view, c = basis;
      const active = () => usable() && view === payload && current(c) && ticket === selectionSerial && !get("architecture-inspector").hidden;
      onInspect(); inspector.show?.();
      const r = row.instance, target = get("architecture-inspector-content"); target.replaceChildren();
      get("architecture-inspector-title").textContent = `${r.rule} · ${targetLabel(r.target)}${r.destination ? ` → ${targetLabel(r.destination)}` : ""}`;
      reviewChips(target);
      paragraph(target, `${row.role} · ${r.status} · ${r.excepted ? `excepted: ${r.exceptionReason}` : "not excepted"} · ${r.referenceCount ?? 0} exact reference sites.`);
      paragraph(target, `Code ${window.TrellisIndexPin.label(payload)} · rules ${c.rulesHash}. ${ruleEvidence(r)}`);
      disclosure(target, "Captured rule evidence and constraints", r);
      if (r.destination && r.target?.path) {
        const params = {kind:"pairs", role:row.role, source:JSON.stringify(r.target), target:JSON.stringify(r.destination)};
        detailList(target, params, active, pairItems(active), NO_PAIRS);
      } else paragraph(target, NO_PAIRS);
      if (eligible(row)) action(target, "Add exception for this pair", () => beginDraft({type:"addExceptions", pairs:[{source:portable(r.target), destination:portable(r.destination)}], reason:""}), active);
    }
    function renderReview() {
      if (!usable()) return;
      const target = get("architecture-review-list"); target.replaceChildren();
      const inApp = scopeGroup() ? ` in ${scopeGroup().name}` : "";
      get("architecture-review-scope").textContent = reviewMode === "unassigned" ? `Unassigned files${inApp} (including files without edges)` :
        "Authored exceptions";
      get("architecture-exceptions").hidden = reviewMode !== "authored";
      const c = basis, active = () => usable() && current(c);
      if (reviewMode === "unassigned" && view.state === "no_layers") {
        paragraph(target, "No layers configured. File assignment inventory is unavailable until layers are configured; authored exceptions remain editable.");
      } else if (reviewMode === "unassigned") {
        for (const module of unassignedOf(view, scopeGroup())) {
          const row = make("div", undefined, "architecture-rule-row"); row.dataset.source = module.identity.path;
          action(row, module.identity.path, () => inspect("module", module), active);
          const label = make("label", "Assign to layer "), select = make("select");
          select.setAttribute("aria-label", `Layer for ${module.identity.path}`);
          for (const layer of view.layers) { const option = make("option", layer.name); option.value = layer.name; select.append(option); }
          label.append(select); row.append(label);
          action(row, "Assign file", () => beginDraft({type:"assignFile", path:module.identity.path, layer:select.value || view.layers[0]?.name}), active).disabled = !view.layers.length; target.append(row);
        }
        if (!target.children.length) paragraph(target, "All indexed native files have an assignment.");
      }
    }
    function editState(message, kind = "ready") { const node = get("architecture-edit-state"); node.textContent = message; node.dataset.state = kind; }
    function setBusy(value, owner) {
      if (!value && owner !== undefined && owner !== busyOwner) return;
      busyOwner++; editBusy = value;
      for (const id of ["architecture-preview", "architecture-confirm", "architecture-rules-reload", "architecture-cancel", "architecture-reason"]) get(id).disabled = value;
      get("architecture-editor").setAttribute("aria-busy", String(value));
      return busyOwner;
    }
    function invalidatePreview() { editSerial++; preview = null; get("architecture-preview-result").hidden = true; }
    function editError(error) {
      if (error.code === "rules_conflict" && draft) draft.needsReload = true;
      const d = error.diagnostic;
      editState(error.code === "rules_conflict" ? "Rules changed on disk. Draft kept. Reload rules, review the new base, then generate a new preview. Nothing was automatically retried." :
        d ? `${d.path}:${d.line}:${d.column} · ${d.kind} · ${d.message}` : error.message || "Rules change failed. No automatic retry.", "error");
      if (error.status === 409 && ["revision_conflict", "pin_expired", "root_changed"].includes(error.code)) onConflict();
    }
    async function loadDescriptor() {
      const c = basis, query = new URLSearchParams(c.pin), data = await request(`/api/architecture/rules?${query}`, () => usable() && current(c));
      if (!usable() || !current(c)) return null;
      const payload = data?.payload;
      if (!payload || !window.TrellisIndexPin.equal(payload, c.pin) || payload.rulesFile?.contentHash !== c.rulesHash || !Array.isArray(payload.exceptions) || !Array.isArray(payload.layers)) {
        const error = new Error("Rules changed since this diagram was loaded. Reload rules before authoring."); error.code = "rules_conflict"; throw error;
      }
      return payload;
    }
    async function showAuthored() {
      if (!usable()) return;
      reviewMode = "authored"; get("architecture-review-panel").hidden = false; renderReview();
      const target = get("architecture-exceptions"); target.replaceChildren(); paragraph(target, "Reading authored exceptions…");
      const c = basis;
      try {
        const data = await loadDescriptor(); if (!data || !current(c) || reviewMode !== "authored") return;
        target.replaceChildren();
        for (const entry of data.exceptions) {
          const row = make("section", undefined, "architecture-rule-row"); row.dataset.exceptionIndex = String(entry.index);
          paragraph(row, `${entry.rule} · ${JSON.stringify(entry.target)}${entry.destination ? ` → ${JSON.stringify(entry.destination)}` : ""} · ${entry.reason}`);
          if (entry.rule === "$layers" && entry.destination) action(row, "Inspect boundary calls", () => inspectAuthored(entry), () => current(c) && usable());
          action(row, "Edit reason", () => beginDraft({type:"editException", index:entry.index, reason:entry.reason}), () => current(c) && usable());
          action(row, "Remove", () => beginDraft({type:"removeException", index:entry.index}), () => current(c) && usable()); target.append(row);
        }
        if (!data.exceptions.length) paragraph(target, "No authored exceptions. Entries remain editable here even when no current reference matches.");
      } catch (error) { if (current(c)) { target.replaceChildren(); paragraph(target, error.message); } }
    }
    function inspectAuthored(entry) {
      if (!usable()) return;
      clearInspection(); inspection = {kind:"authored", record:entry};
      const c = basis, ticket = selectionSerial;
      const active = () => usable() && current(c) && ticket === selectionSerial && !get("architecture-inspector").hidden;
      onInspect(); inspector.show?.();
      get("architecture-inspector-title").textContent = `${entry.target.path} → ${entry.destination.path}`;
      const target = get("architecture-inspector-content"); target.replaceChildren(); reviewChips(target);
      paragraph(target, `Authored exception · ${entry.reason}`);
      detailList(target, {kind:"instances", rule:entry.rule, path:entry.target.path, excepted:"true", exceptionReason:entry.reason}, active, (list, rows) => {
        const matches = (identity, authored) => !!identity && Object.entries(authored).every(([key, value]) =>
          identity[key === "source_set" ? "sourceSetId" : key] === value);
        const matching = rows.filter(({instance:r}) => matches(r.target, entry.target) && matches(r.destination, entry.destination));
        for (const row of matching) {
          paragraph(list, `${row.role} · ${row.instance.referenceCount} exact reference sites`);
          detailList(list, {kind:"pairs", role:row.role, source:JSON.stringify(row.instance.target), target:JSON.stringify(row.instance.destination)}, active, pairItems(active), NO_PAIRS);
        }
      }, "No current reference matches this authored exception. This does not prove absence.");
    }
    function snapshot() {
      return usable() && inspection ? {inspection, pressed:nodeKey(pressedNode()), rulesHash:basis.rulesHash,
        aggregate, role, scope:scopeName(), reviewMode, reviewOpen:!get("architecture-review-panel").hidden, deviations:deviations.snapshot()} : null;
    }
    async function restore(saved, active) {
      // A line selected in the Deviations tab returns to that tab (#965); anything else to Architecture.
      tab = saved.inspection?.kind === "deviation" ? "tree" : "diagram"; afterDraw = null; placeReview();
      aggregate = saved.aggregate; role = saved.role;
      get("architecture-aggregate").value = aggregate;
      // The saved scope applies to this checkout, and so to every view that shares the choice.
      scopes.set(saved.scope ?? null);
      // A restored tree reads once, with its saved selection, not first as a fresh tree. The mark belongs to
      // this restore's read only, so a superseded restore never holds back a newer visit's tree.
      await refresh({restoring:true});
      if (!active() || !usable()) return;
      settleScope(view);
      if (basis.rulesHash !== saved.rulesHash) throw new Error("Architecture rules changed. Select the boundary again.");
      if (saved.reviewOpen) {
        // The authored list reads on its own guard; the inspection does not wait for it.
        if (saved.reviewMode === "authored") void showAuthored();
        else if (saved.reviewMode === "unassigned") openReview("unassigned");
      }
      if (!active() || !usable()) return;
      const {kind, record} = saved.inspection;
      if (kind === "deviation") { await deviations.restore(saved.deviations, active); return; }
      if (kind === "authored") inspectAuthored(record);
      else if (kind === "rule") inspectRule(record);
      else inspect(kind, record);
      pressed = saved.pressed;
      const node = findNode(pressed); if (node) { node.setAttribute("aria-pressed", "true"); node.focus({preventScroll:true}); syncCards(); }
    }
    function beginDraft(operation, scope = null) {
      if (!usable() || editBusy) return;
      if (draft?.needsReselect && operation.type === "editException") operation.reason = get("architecture-reason").value;
      invalidatePreview(); draft = {operation, baseHash:basis.rulesHash, basis, needsReselect:false};
      get("architecture-review-panel").hidden = false; get("architecture-editor").hidden = false;
      const reason = get("architecture-reason"); reason.value = operation.reason || ""; reason.hidden = !["addExceptions", "editException"].includes(operation.type);
      get("architecture-reason-label").hidden = reason.hidden;
      get("architecture-edit-title").textContent = {addExceptions:"Add finite pair exceptions",editException:"Edit exception reason",removeException:"Remove authored exception",assignFile:"Assign file to layer"}[operation.type];
      get("architecture-edit-scope").textContent = typeof scope === "string" ? scope : operation.type === "addExceptions" ? `${operation.pairs.length} concrete witnessed pairs. One reason, one preview and one apply.\n${operation.pairs.map(pair => `${pair.source.path} → ${pair.destination.path}`).join("\n")}` : JSON.stringify(operation);
      editState(`Base rules ${draft.baseHash}. Preview before confirming.`);
    }
    async function previewDraft() {
      if (!draft || !usable() || editBusy) return;
      invalidatePreview();
      if (draft.needsReload) { editState("Rules changed. Reload rules before generating a new preview. Draft kept.", "error"); return; }
      if (draft.needsReselect) { editState("Draft reason kept. Select the authored entry again on the reloaded base before previewing.", "error"); return; }
      const operation = {...draft.operation};
      if (["addExceptions", "editException"].includes(operation.type)) {
        operation.reason = get("architecture-reason").value;
        if (!operation.reason.trim()) { editState("A nonblank exception reason is required.", "error"); return; }
      }
      const c = basis, owned = draft, ticket = editSerial, body = {...c.pin, baseHash:owned.baseHash, operations:[operation]};
      const active = () => usable() && current(c) && draft === owned && ticket === editSerial;
      const busy = setBusy(true); editState("Validating complete rules and preparing exact diff…", "loading");
      try {
        const data = await request("/api/architecture/rules/preview", active, {method:"POST",body}); if (!active()) return;
        const p = data?.payload;
        if (!p || !window.TrellisIndexPin.equal(p,c.pin) || p.baseHash !== body.baseHash || typeof p.proposedHash !== "string" || typeof p.text !== "string" || typeof p.diff !== "string" || !Array.isArray(p.pairs)) throw new Error("Invalid rules preview. Nothing can be applied.");
        preview = {body, payload:p, basis:c};
        get("architecture-diff").textContent = p.diff; get("architecture-proposed-text").textContent = p.text;
        get("architecture-preview-pairs").textContent = JSON.stringify(p.pairs,null,2); get("architecture-preview-result").hidden = false;
        editState(`Preview ready · ${p.pairs.length} resolved pairs · proposed rules ${p.proposedHash}. Confirm writes these exact bytes.`);
      } catch (error) { if (active()) editError(error); }
      finally { setBusy(false, busy); }
    }
    async function applyDraft() {
      if (!preview || !draft || !usable() || editBusy || !current(preview.basis)) return;
      const owned = preview, c = basis, ticket = editSerial, active = () => usable() && current(c) && preview === owned && ticket === editSerial;
      const busy = setBusy(true); editState("Saving exact preview…", "loading");
      try {
        const data = await request("/api/architecture/rules/apply", active, {method:"POST", body:{...owned.body, proposedHash:owned.payload.proposedHash}}); if (!active()) return;
        const p = data?.payload;
        if (!p || !window.TrellisIndexPin.equal(p,c.pin) || p.rulesFile?.contentHash !== owned.payload.proposedHash || !["committed","durability_uncertain"].includes(p.mutationOutcome)) throw new Error("Unrecognized write outcome. Reload rules to inspect disk before trying again.");
        const message = p.mutationOutcome === "durability_uncertain" ? "Rules were written, but crash durability is uncertain. " : "Rules committed. ";
        invalidatePreview(); draft = null; get("architecture-editor").hidden = true;
        // Written rules can hide or show layers at the same code pin; other views re-read their population.
        onRulesChanged();
        if (tab === "tree") deviations.reopenAfterRefresh();
        await refresh();
        const now = context();
        if (now.rootKey !== c.rootKey || now.session !== c.session || !window.TrellisIndexPin.equal(now.pin, c.pin)) return;
        if (usable() && basis.rulesHash === p.rulesFile.contentHash) { state(message + "Rechecked at the same code snapshot."); }
        else { state(message + "Recheck failed or rules changed again. Select the Architecture tab again before reviewing; no old graph is current.", "stale"); }
      } catch (error) {
        if (active()) {
          invalidatePreview();
          if (Number.isInteger(error.status)) editError(error);
          else {
            draft.needsReload = true;
            const detail = `Write outcome unknown. The apply response was not received or could not be verified: ${error.message || "unknown error"}. Reload rules and inspect the current rules before any new preview. No retry was made.`;
            renderSerial++; clearInspection(); view = null;
            get("architecture-diagram").replaceChildren(); get("architecture-legend").replaceChildren();
            editState(detail, "error"); state(detail, "stale");
          }
        }
      }
      finally { setBusy(false, busy); }
    }
    async function reloadRules() {
      if (!draft || editBusy) return;
      const owned = draft, prior = owned.basis, reason = get("architecture-reason").value;
      if (!current(prior)) return;
      invalidatePreview(); const busy = setBusy(true);
      try {
        await refresh({keepDraft:true});
        if (!usable() || draft !== owned) { editState("Draft kept, but rules recheck failed. Reload rules before authoring.", "error"); return; }
        await loadDescriptor();
        if (!usable() || draft !== owned) return;
        const changed = owned.baseHash !== basis.rulesHash;
        owned.baseHash = basis.rulesHash; owned.basis = basis; owned.needsReload = false;
        owned.needsReselect = changed && ["editException","removeException"].includes(owned.operation.type);
        get("architecture-reason").value = reason;
        editState(owned.needsReselect ? "Rules reloaded. Draft reason kept; choose the authored entry again because entry indexes belong to the old rules hash." : "Rules reloaded. Draft kept. Review the new base and generate a new preview.");
      } catch (error) { editError(error); }
      finally { setBusy(false, busy); }
    }
    get("architecture-review-close").addEventListener("click", () => { get("architecture-review-panel").hidden = true; });
    get("architecture-reason").addEventListener("input", invalidatePreview);
    get("architecture-preview").addEventListener("click", previewDraft);
    get("architecture-confirm").addEventListener("click", applyDraft);
    get("architecture-rules-reload").addEventListener("click", reloadRules);
    get("architecture-cancel").addEventListener("click", () => { if (!editBusy) { invalidatePreview(); draft = null; get("architecture-editor").hidden = true; } });
    get("architecture-relative").checked = false;
    get("architecture-relative").addEventListener("change", event => { relative = event.target.checked; return render(); });
    get("architecture-color").value = metric; get("architecture-aggregate").value = aggregate;
    get("architecture-tests").checked = showTests;
    get("architecture-metric-tests").checked = false;
    get("architecture-metric-tests").addEventListener("change", event => {
      const next = event.target.checked ? "all" : "production";
      if (next === role) return;
      role = next; return refresh();
    });
    hiddenToggle.addEventListener("click", () => {
      if (!usable()) return;
      shown.hidden = !shown.hidden; saveDisplay(); return rerender({hidden:true});
    });
    // A changed panel width (window resize, inspector toggle) re-lays out the drawing,
    // debounced, keeping the size cap, the inspector and the selection (#515, #525).
    // A hidden panel has no width and waits until it is shown again.
    let resizeTimer = null, resizeSerial = 0;
    if (typeof window.ResizeObserver === "function") new window.ResizeObserver(() => {
      const canvas = get("architecture-diagram"), ticket = ++resizeSerial;
      window.clearTimeout(resizeTimer);
      // Tests and tools can tell a pending re-layout from a settled drawing.
      canvas.dataset.layout = "pending";
      resizeTimer = window.setTimeout(async () => {
        const width = canvas.clientWidth;
        if (width && laidOut !== null && Math.abs(width - laidOut) >= RESIZE_SLACK && usable() && view.state === "ready") await render({keepSelection:true});
        // Settled unless a newer width change scheduled another re-layout meanwhile.
        if (ticket === resizeSerial) delete canvas.dataset.layout;
      }, RESIZE_DEBOUNCE_MS);
    }).observe(get("architecture-diagram"));
    get("architecture-color").addEventListener("change", event => { metric = event.target.value; return render(); });
    get("architecture-tests").addEventListener("change", event => { showTests = event.target.checked; return render(); });
    get("architecture-aggregate").addEventListener("change", event => {
      if (!["max", "sum", "mean"].includes(event.target.value) || event.target.value === aggregate) return;
      aggregate = event.target.value; return refresh();
    });
    return {refresh, reset, clearInspection, restore, reopen, suspend, scopeChanged, showDeviations, deviationsFilter};
  }
  window.TrellisArchitecture = {create, createDisplay, moduleKey, quartiles, band, metricValue, commonDirectory, groupModules};
})();
