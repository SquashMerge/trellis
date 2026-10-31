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
  const finite = value => typeof value === "number" && Number.isFinite(value);
  const display = value => finite(value) ? String(value) : "—";
  const external = identity => identity.kind === "externalPackage";
  const moduleKey = identity => external(identity)
    ? JSON.stringify([identity.kind, identity.sourceSetId, identity.language, identity.package, identity.standardLibrary])
    : JSON.stringify([identity.sourceSetId, identity.language, identity.path, identity.sid]);
  const targetLabel = identity => external(identity)
    ? `${identity.package} (${identity.language}${identity.standardLibrary ? " stdlib" : " package"})` : identity.path;
  const {metricValue, quartiles, relativeBand:band, createScale, exclusions} = window.TrellisMetricHeat;
  const labels = {loc:"LOC (callable ranges)", sloc:"SLOC (callable ranges)", commentLines:"Comment lines (callable ranges)",
    cognitive:"Cognitive", cyclomatic:"Cyclomatic", maxNesting:"Max nesting", maintainabilityIndex:"Maintainability (higher is better)",
    fanOut:"Fan-out (call sites)", fanIn:"Fan-in", halstead:"Halstead", parameters:"Parameters", exits:"Exits"};
  const STATS = [["up", "↑", "upward deviation", true], ["cross", "⇄", "peer or shared deviation", true], ["down", "↓", "downward"],
    ["shared", "→shared ", "into shared"], ["allowed", "allowed ", "allowed by policy"], ["excepted", "excepted ", "excepted"], ["unknown", "unknown ", "unknown"]];
  // SPEC §9 size cap: chips per band or open module, and in the whole diagram.
  const ENTRY_CAP = 40, BOX_CAP = 150;
  const stem = name => { const dot = name.lastIndexOf("."); return dot > 0 ? name.slice(0, dot) : name; };
  // A directory module's own root file: shown first when it opens, never as a "mod" chip.
  const ROOT_FILES = new Set(["mod.rs", "__init__.py"]);
  // Longest common directory ("" or ending in "/") of a band's file paths.
  function commonDirectory(paths) {
    let shared = null;
    for (const path of paths) {
      const parts = path.split("/").slice(0, -1);
      if (shared === null) { shared = parts; continue; }
      let i = 0;
      while (i < shared.length && i < parts.length && shared[i] === parts[i]) i++;
      shared = shared.slice(0, i);
    }
    return shared?.length ? `${shared.join("/")}/` : "";
  }
  // A module is the first path component below `base`; a file and its same-named
  // directory (Rust's `store.rs` and `store/`) form one module. Paths stay labels.
  function groupModules(modules, base) {
    const groups = new Map();
    for (const module of modules) {
      const rest = module.identity.path.slice(base.length), slash = rest.indexOf("/");
      const name = slash >= 0 ? rest.slice(0, slash) : ROOT_FILES.has(rest) ? rest : stem(rest);
      if (!groups.has(name)) groups.set(name, []);
      groups.get(name).push(module);
    }
    return [...groups].sort(([a], [b]) => a.localeCompare(b));
  }
  function create({request, context, readSource, openTreemap, libraryButton = () => null, onConflict = () => {}, onInspect = () => {}}) {
    let serial = 0, renderSerial = 0, selectionSerial = 0, reviewSerial = 0, view = null, basis = null;
    let metric = "cognitive", aggregate = "max", role = "production", relative = false, showTests = false;
    let reviewScope = null, reviewMode = "rules", selected = new Map(), draft = null, preview = null, editSerial = 0, editBusy = false, busyOwner = 0;
    const callables = () => role === "all" ? "indexed callables" : "production callables";
    const population = () => role === "all" ? "all indexed callables" : callables();
    const state = (message, kind = "ready") => { get("architecture-state").textContent = message; get("architecture-state").dataset.state = kind; };
    function capture() {
      const c = context();
      return {rootKey:c.rootKey, session:c.session, pin:c.pin && window.TrellisIndexPin.copy(c.pin), aggregate, role, serial};
    }
    function current(c) {
      const now = context();
      return c && c.serial === serial && c.aggregate === aggregate && c.role === role && c.rootKey === now.rootKey && c.session === now.session &&
        !!c.pin && window.TrellisIndexPin.equal(c.pin, now.pin);
    }
    function usable() { return view && current(basis) && view.rulesFile?.contentHash === basis.rulesHash; }
    function clearInspection() {
      selectionSerial++;
      window.TrellisShell?.clearArchitectureInspector();
    }
    function reset(message = "Open Architecture to read the indexed snapshot and reviewed rules.", keepDraft = false) {
      editSerial++; preview = null; selected.clear();
      if (!keepDraft) setBusy(false);
      if (!keepDraft) { draft = null; get("architecture-review-panel").hidden = true; get("architecture-editor").hidden = true; }
      get("architecture-preview-result").hidden = true;
      serial++; renderSerial++; clearInspection(); view = null; basis = null;
      get("architecture-diagram").replaceChildren(); get("architecture-diagram").setAttribute("aria-busy", "false");
      get("architecture-legend").replaceChildren(); get("architecture-notice-content").replaceChildren();
      get("architecture-notices").open = false;
      get("architecture-hidden").hidden = true; get("architecture-coverage").textContent = ""; get("architecture-exclusions").textContent = "";
      state(message, "empty");
    }
    function payloadOf(data, c) {
      const payload = data?.payload;
      if (!payload || !window.TrellisIndexPin.equal(payload, c.pin)) {
        const error = new Error("Architecture snapshot changed. Refresh status."); error.code = "revision_conflict"; error.status = 409; throw error;
      }
      if (!["ready", "missing_rules", "no_layers"].includes(payload.state) || payload.aggregate !== c.aggregate || payload.role !== c.role ||
          payload.workbench !== true || payload.metricsVersion !== "metrics:v1" ||
          payload.evidenceTier !== "syntax" || payload.semanticComplete !== false ||
          !Array.isArray(payload.layers) || !Array.isArray(payload.modules) || !Array.isArray(payload.edges) ||
          (["ready", "no_layers"].includes(payload.state) && (typeof payload.rulesFile?.contentHash !== "string" || !payload.rulesFile.contentHash))) {
        throw new Error("Invalid Architecture response. Nothing displayed.");
      }
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
      paragraph(target, `${references?.counts?.mappedSites ?? 0} structurally mapped reference sites · ${references?.counts?.unresolvedSites ?? 0} unresolved. Missing sites do not prove absence.`);
      for (const limitation of references?.limitations || []) paragraph(target, limitation);
      const unassigned = view.modules.filter(module => module.layer === null);
      paragraph(target, view.state === "no_layers" ? "No layer graph or file assignment inventory is available until layers are configured." : `${unassigned.length} unassigned native file modules remain unknown.`);
      if (references?.externalPackages?.length) disclosure(target, "External packages and captured evidence", references.externalPackages);
      const choose = (label, action) => { const button = make("button", label); button.type = "button"; button.addEventListener("click", action); target.append(button); };
      const c = basis;
      choose("Inspect same-layer and unassigned references", () => { if (current(c) && usable()) inspect("unknown", null); });
      choose("Inspect unresolved sites", () => { if (current(c) && usable()) inspect("unresolved", null); });
      if (references?.externalPackages?.length) choose("Inspect external policy results", () => { if (current(c) && usable()) inspect("external", null); });
    }
    function layerOf(identity) {
      if (!external(identity)) return view.modules.find(module => same(module.identity, identity))?.layer;
      return view.references?.externalPackages?.find(item => same(item.identity, identity))?.layer ?? null;
    }
    // Undrawn detail is read on demand, one bounded page at a time, at the view's exact
    // pin and rules hash (#448). A changed pin or rules file fails with a typed 409.
    async function details(params, active, offset = 0) {
      const c = basis;
      const query = new URLSearchParams({...params, indexGeneration:c.pin.indexGeneration, indexRevision:String(c.pin.indexRevision),
        rulesHash:c.rulesHash, offset:String(offset)});
      const data = await request(`/api/architecture/details?${query}`, active);
      const page = data?.payload;
      if (!page || !window.TrellisIndexPin.equal(page, c.pin) || page.rulesFile?.contentHash !== c.rulesHash || page.kind !== params.kind ||
          page.offset !== offset || !Array.isArray(page.items) || !Number.isSafeInteger(page.total) ||
          !(page.nextOffset === null || (Number.isSafeInteger(page.nextOffset) && page.nextOffset > offset))) {
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
    // The exact $layers filter of one summary edge (its verdict and exception group).
    function edgeFilters(edge) {
      return {rule:"$layers", role:edge.role, sourceLayer:edge.sourceLayer, destinationLayer:edge.destinationLayer, status:edge.status,
        excepted:String(!!edge.excepted), ...(edge.exceptionReason ? {exceptionReason:edge.exceptionReason} : {})};
    }
    function layerLabel(layer) {
      const modules = view.modules.filter(module => module.layer === layer.name);
      const values = modules.map(module => metricValue(module.metrics, metric)).filter(finite);
      const packages = (view.references?.externalPackages || []).filter(item => layerOf(item.identity) === layer.name);
      const max = values.length ? String(values.reduce((most, value) => Math.max(most, value), -Infinity)) : "—";
      // The narrow shared box stacks its header; ordered bands keep one line.
      const parts = [`${layer.shared ? "Shared" : layer.order} · ${layer.name}`, `${plural(modules.length, "file")}${packages.length ? ` · ${packages.length} external packages` : ""}`];
      if (metric !== "layer") parts.push(`max ${max}`);
      return parts.join(layer.shared ? "\n" : " · ");
    }
    // Outgoing production reference sites by verdict: the same summary edges the arrows draw.
    function layerCounts(layer) {
      const byName = new Map(view.layers.map(item => [item.name, item]));
      const counts = {up:0, cross:0, down:0, shared:0, allowed:0, excepted:0, unknown:0};
      for (const edge of view.edges) {
        if (edge.sourceLayer !== layer.name || edge.role !== "production") continue;
        const destination = byName.get(edge.destinationLayer), ordered = !layer.shared && destination && !destination.shared;
        if (edge.excepted) counts.excepted += edge.count;
        else if (edge.status === "deviates") counts[ordered && destination.order < layer.order ? "up" : "cross"] += edge.count;
        else if (edge.status !== "conforms") counts.unknown += edge.count;
        else if (destination?.shared) counts.shared += edge.count;
        else counts[ordered && destination.order > layer.order ? "down" : "allowed"] += edge.count;
      }
      return counts;
    }
    // Only non-zero counts become chips; deviations carry the error tone and a glyph.
    function layerStats(layer) {
      const counts = layerCounts(layer), bound = lowerBound(layer.name, "production") ? "≥" : "";
      return STATS.filter(([key]) => counts[key] > 0).map(([key, glyph, words, error]) =>
        ({key, count:counts[key], text:`${glyph}${bound}${counts[key]}`, label:`${bound ? "at least " : ""}${counts[key]} outgoing production reference sites · ${words}`, tone:error ? "error" : "neutral"}));
    }
    // Summary-first display (#391): presentation state only, remembered per checkout.
    let shown = null, shownRoot = null;
    const plural = (count, word) => `${count} ${word}${count === 1 ? "" : "s"}`;
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
    // Authored rules.toml presentation hint (#391 ruling); absent leaves the choice to the browser.
    const hint = layer => ["expanded", "collapsed", "hidden"].includes(layer.display) ? layer.display : null;
    // Bands open on module chips unless the authored hint asks for a collapsed summary.
    // #391 ruling: an absent hint starts collapsed like "collapsed"; "expanded" opens on module chips.
    const expanded = layer => shown.open.has(layer.name) ? shown.open.get(layer.name) : hint(layer) === "expanded";
    // A test or fixture layer: every file is a fixture input or holds only test callables.
    function testLayer(layer, modules) {
      const members = modules.filter(module => module.layer === layer.name);
      const test = module => module.excludedReason === "fixture" || (module.roleCounts?.production === 0 && module.roleCounts?.test > 0);
      const empty = module => !module.excludedReason && module.roleCounts?.production === 0 && module.roleCounts?.test === 0;
      return members.some(test) && members.every(module => test(module) || empty(module));
    }
    // Deviations are highlighted and allowed references dimmed; no layer is dropped for it (#449).
    const flagged = edge => (edge.status === "deviates" && !edge.excepted) || edge.status === "unknown";
    const edgeTone = edge => edge.role === "test" ? "neutral" : edge.status === "unknown" ? "unknown" : flagged(edge) ? "deviation" : "allowed";
    // Exact mapped-site coverage of one source layer and role, from the summary (#449).
    function coverageOf(layerName, siteRole) {
      return (view?.references?.coverage || []).find(row => row.layer === layerName && row.role === siteRole) || null;
    }
    const percent = (part, whole) => {
      if (!whole) return "—";
      const value = part / whole * 100;
      return part > 0 && value < 1 ? "<1%" : part < whole && value > 99 ? ">99%" : `${Math.round(value)}%`;
    };
    // A count is a lower bound when any site of its source layer and role stayed unresolved.
    const lowerBound = (layerName, siteRole) => (coverageOf(layerName, siteRole)?.unresolvedSites ?? 1) > 0;
    function edgeText(edge) {
      const covered = coverageOf(edge.sourceLayer, edge.role), bound = lowerBound(edge.sourceLayer, edge.role);
      if (!covered) return {bound};
      const share = percent(covered.mappedSites, covered.sites);
      const label = `${bound ? "lower bound; " : ""}${share} of ${covered.sites} ${edge.role} reference sites from ${edge.sourceLayer} mapped to a module`;
      return {bound, label, note:bound && ["deviation", "unknown"].includes(edgeTone(edge)) ? `${share} mapped` : ""};
    }
    function summary(payload) {
      const production = payload.edges.filter(edge => edge.role === "production");
      // Unhinted test and fixture layers join the hidden footer until tests and fixtures are included;
      // an authored display hint always wins. Every other layer is drawn: the full stack (#449).
      const tests = new Set(role === "production" ? payload.layers.filter(layer => hint(layer) === null && testLayer(layer, payload.modules)).map(layer => layer.name) : []);
      const hidden = payload.layers.filter(layer => hint(layer) === "hidden" || tests.has(layer.name));
      const layers = new Set(payload.layers.filter(layer => !hidden.includes(layer) || shown.hidden).map(layer => layer.name));
      const hiddenEdges = production.filter(edge => hidden.some(layer => layer.name === edge.sourceLayer || layer.name === edge.destinationLayer)).length;
      return {hidden, tests, hiddenEdges, layers};
    }
    function worst(modules) {
      if (metric === "layer") return undefined;
      const values = modules.map(module => metricValue(module.metrics, metric)).filter(finite);
      if (!values.length) return null;
      return metric === "maintainabilityIndex" ? Math.min(...values) : Math.max(...values);
    }
    // One chip per module; a click opens it one level, down to file chips.
    function level(layerKey, base, members, scale) {
      return groupModules(members, base).map(([name, files]) => {
        files.sort((a, b) => a.identity.path.localeCompare(b.identity.path));
        if (files.length === 1) return {type:"module", module:files[0], name};
        const prefix = `${base}${name}`, key = JSON.stringify([layerKey, prefix]), open = shown.drilled.has(key), value = worst(files);
        const amount = value === undefined ? "" : `${metric === "maintainabilityIndex" ? "min" : "max"} ${display(value)}`;
        // An open module lists its own root file (`store.rs`, `store/mod.rs`, `pkg/__init__.py`) before its nested modules.
        const inner = `${prefix}/`, root = file => !file.identity.path.startsWith(inner) || ROOT_FILES.has(file.identity.path.slice(inner.length));
        const children = () => [...files.filter(root).map(module => ({type:"module", module, name:module.identity.path.slice(base.length)})),
          ...level(layerKey, inner, files.filter(file => !root(file)), scale)];
        return {type:"directory", key, open, size:files.length, band:scale.band(value), entries:[], children:open ? children : null,
          label:`${name}\n${plural(files.length, "file")}${amount ? ` · ${amount}` : ""}\n${prefix}`,
          shown:`${name}\n${value === undefined ? "" : `${display(value)} · `}${plural(files.length, "file")}`};
      });
    }
    // Every chip and note costs one unit of the diagram budget, so a render never draws more
    // than BOX_CAP. Past the cap, the rest of a list becomes one note that opens the full file list.
    function capped(layerKey, list, budget) {
      const room = Math.min(ENTRY_CAP, budget.left), kept = list.length > room ? list.slice(0, Math.max(0, room - 1)) : list;
      budget.left -= kept.length;
      for (const entry of kept) {
        if (!entry.children) continue;
        // With the budget spent, an open module stays folded rather than drawing an empty box.
        if (budget.left > 0) entry.entries = capped(layerKey, entry.children(), budget); else entry.open = false;
      }
      if (kept.length === list.length) return kept;
      const omitted = list.slice(kept.length).reduce((sum, entry) => sum + (entry.size ?? 1), 0);
      budget.truncated += omitted;
      if (budget.left < 1) return kept;
      budget.left -= 1;
      return [...kept, {type:"note", layerKey, label:`${plural(omitted, "file")} not drawn · diagram size cap\nselect to list ${layerKey === null ? "unassigned" : "this layer's"} files`,
        shown:`${plural(omitted, "file")}\nnot drawn (cap)`}];
    }
    function entries(layerKey, modules, scale, budget) {
      const measured = modules.filter(module => module.declarationCount > 0);
      const result = capped(layerKey, level(layerKey, commonDirectory(modules.map(module => module.identity.path)), measured, scale), budget);
      // Notes are chip-sized: a short drawn label, the full sentence as the accessible name.
      const unmeasured = modules.length - measured.length;
      if (unmeasured && budget.left > 0) {
        budget.left -= 1;
        result.push({type:"note", layerKey, label:`${plural(unmeasured, "file")} · metrics unavailable\nno ${callables()}`, shown:`${plural(unmeasured, "file")}\nno metrics`});
      }
      return result;
    }
    // A collapsed band's one row of module chips, hottest first (#449). Display only: the
    // band header opens it. The last chip says how many modules the row leaves out.
    function summaryChips(layerKey, modules, columns, scale, budget) {
      const measured = modules.filter(module => module.declarationCount > 0);
      const groups = groupModules(measured, commonDirectory(modules.map(module => module.identity.path))).map(([name, files]) => ({name, files, value:worst(files)}));
      const severity = value => !finite(value) ? -Infinity : metric === "maintainabilityIndex" ? -value : value;
      if (metric !== "layer") groups.sort((a, b) => severity(b.value) - severity(a.value) || a.name.localeCompare(b.name));
      const room = Math.min(columns, ENTRY_CAP, budget.left), kept = groups.length > room ? groups.slice(0, Math.max(Math.min(1, room), room - 1)) : groups;
      budget.left -= kept.length;
      const chips = kept.map(({name, files, value}) => {
        const amount = value === undefined ? "" : `${display(value)}`;
        return {type:"chip", layerKey, band:value === undefined ? null : scale.band(value),
          label:`${name} · ${plural(files.length, "file")}${amount ? ` · ${metric} ${amount}` : ""}`, shown:`${name}${amount ? `\n${amount}` : ""}`};
      });
      const rest = groups.length - kept.length;
      if (rest > 0 && budget.left > 0) {
        budget.left -= 1;
        chips.push({type:"chip", layerKey, band:null, label:`${plural(rest, "more module")} · open the band to see them`, shown:`+${rest} more\nopen band`});
      }
      return chips;
    }
    function displayState(payload, layout) {
      const files = layers => payload.modules.filter(module => layers.some(layer => layer.name === module.layer)).length;
      get("architecture-hidden").hidden = !layout.hidden.length;
      const toggle = get("architecture-hidden-toggle");
      toggle.textContent = `${shown.hidden ? "▾" : "▸"} ${plural(layout.hidden.length, "hidden layer")} · ${plural(files(layout.hidden), "file")} · ${plural(layout.hiddenEdges, "production edge")}`;
      toggle.setAttribute("aria-expanded", String(shown.hidden));
      get("architecture-hidden-names").textContent = layout.hidden.map(layer => layout.tests.has(layer.name) ? `${layer.name} (tests/fixtures; include them to show)` : layer.name).join(", ");
    }
    function refocus(target) {
      for (const node of get("architecture-diagram").querySelectorAll?.("[role=button]") || []) {
        if ((target.layer !== undefined && node.getAttribute("data-trellis-layer-id") === target.layer) ||
            (target.directory !== undefined && node.getAttribute("data-trellis-directory-id") === target.directory) ||
            (target.unassigned && node.hasAttribute("data-trellis-unassigned"))) { node.focus(); return; }
      }
    }
    async function rerender(target, after) {
      const c = basis;
      if (!await render() || !usable() || !current(c)) return;
      after?.(); refocus(target);
    }
    const flip = (set, key) => { if (set.has(key)) set.delete(key); else set.add(key); };
    function legend(scale) {
      const target = get("architecture-legend"); target.replaceChildren();
      if (metric === "layer") { paragraph(target, "Layer membership · neutral chips; no metric heat."); return; }
      paragraph(target, `${scale.label} · ${metric} · ${aggregate} · ${population()}`);
      for (const {label, band} of scale.entries) {
        const entry = make("span", undefined, "metric-legend-entry");
        entry.append(make("span", undefined, `metric-swatch${band === null ? " neutral" : ` heat-${band}`}`), make("span", label)); target.append(entry);
      }
      if (relative) target.append(make("span", "Low heat → high heat · ties stay together · — unavailable"));
    }
    // Measured coverage beside the arrow key: drawn counts are lower bounds (#449).
    function coverageSummary(payload) {
      const rows = payload.references?.coverage || [];
      const sites = rows.reduce((sum, row) => sum + row.sites, 0), mapped = rows.reduce((sum, row) => sum + row.mappedSites, 0);
      const counts = payload.rules?.counts;
      const parts = [`${mapped.toLocaleString("en-US")} of ${sites.toLocaleString("en-US")} reference sites mapped to a module (${percent(mapped, sites)})`];
      if (rows.some(row => row.unresolvedSites > 0)) parts.push("≥ counts are lower bounds");
      if (counts && Number.isSafeInteger(counts.unknown)) parts.push(`${counts.unknown.toLocaleString("en-US")} of ${counts.comparable.toLocaleString("en-US")} production rule instances unknown`);
      return parts.join(" · ");
    }
    async function render() {
      if (!usable() || view.state !== "ready") return false;
      const c = basis, payload = view, ticket = ++renderSerial;
      const active = () => current(c) && view === payload && ticket === renderSerial && payload.rulesFile.contentHash === c.rulesHash;
      // The inspector's review overview opens before measuring, so the drawing fits the width it gets.
      clearInspection(); overview(); get("architecture-diagram").replaceChildren();
      const layout = summary(payload);
      // Collapsing layers or directories changes representation, not their module population. The
      // unassigned row is always drawn (as metric chips when collapsed, #449), so it is always in the scale.
      const visible = payload.modules.filter(module =>
        (module.layer === null || layout.layers.has(module.layer)) && module.declarationCount > 0);
      const values = visible.map(module => metricValue(module.metrics, metric));
      const scale = createScale(metric, aggregate, values, relative);
      legend(scale);
      displayState(payload, layout);
      get("architecture-coverage").textContent = coverageSummary(payload);
      const unassigned = payload.modules.filter(module => module.layer === null).length, budget = {left:BOX_CAP, truncated:0};
      // The canvas's inner width; the adapter fits that many chip columns.
      const canvas = get("architecture-diagram"), style = window.getComputedStyle?.(canvas), padding = style ? parseFloat(style.paddingLeft) + parseFloat(style.paddingRight) : 0;
      try {
        if (!window.TrellisArchitecturePele) throw new Error("Architecture renderer unavailable. Nothing displayed.");
        const outcome = await window.TrellisArchitecturePele.render(canvas, payload, {
          showTests, metric, aggregate, isCurrent:active, width:canvas.clientWidth - (padding || 0),
          moduleValue:module => metric === "layer" ? undefined : metricValue(module.metrics, metric),
          moduleBand:module => metric === "layer" ? null : scale.band(metricValue(module.metrics, metric)),
          layerLabel, layerStats, edgeText, edgeTone,
          display:{layers:layout.layers, expanded:name => budget.left > 0 && expanded(payload.layers.find(layer => layer.name === name)), entries:(key, modules) => entries(key, modules, scale, budget), reset:() => { budget.left = BOX_CAP; budget.truncated = 0; },
            summary:(key, modules, columns) => summaryChips(key, modules, columns, scale, budget),
            edge:() => true,
            unassigned:{label:`Unassigned / unknown\n${plural(unassigned, "file")} · no layer`, get expanded() { return shown.unassigned && budget.left > 0; }}},
          onSelectModule:module => { if (active() && payload.modules.includes(module)) inspect("module", module); },
          onSelectEdge:edge => { if (active() && payload.edges.includes(edge)) inspect("edge", edge); },
          onSelectLayer:layer => { if (active() && payload.layers.includes(layer)) inspect("layer", layer); },
          onToggleLayer:layer => {
            if (!active() || !payload.layers.includes(layer)) return;
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
            const layer = payload.layers.find(item => item.name === entry?.layerKey);
            if (layer) inspect("layer", layer); else inspect("unknown", null);
          },
        });
        if (!active()) return false;
        if (outcome?.status !== "ready") throw new Error(outcome?.reason || outcome?.error?.message || "Architecture mapping failed. Nothing displayed.");
        const production = payload.edges.filter(edge => edge.role === "production").reduce((sum, edge) => sum + edge.count, 0);
        const tests = payload.edges.filter(edge => edge.role === "test").reduce((sum, edge) => sum + edge.count, 0);
        // One status line; heat exclusions move to the coverage notices (#449).
        state(`${payload.modules.length} file modules · ${production} production / ${tests} test-only cross-layer sites${showTests ? "" : " (tests hidden)"}${budget.truncated ? ` · ${plural(budget.truncated, "file")} not drawn (size cap ${BOX_CAP} chips)` : ""} · Metrics: ${aggregate}, ${population()}`, "ready");
        get("architecture-exclusions").textContent = `Heat: ${exclusions(payload.modules)}.`;
        return true;
      } catch (error) {
        if (!active()) return false;
        get("architecture-diagram").replaceChildren(); get("architecture-legend").replaceChildren(); clearInspection();
        state(error.message || "Architecture rendering failed. Nothing displayed.", "error");
        return false;
      }
    }
    async function refresh(options = {}) {
      reset(undefined, options.keepDraft === true);
      const c = capture();
      if (!c.rootKey || !c.pin) { state("Choose an indexed checkout, then open Architecture.", "empty"); return; }
      loadDisplay(c.rootKey);
      state("Reading indexed architecture and reviewed rules…", "loading"); get("architecture-diagram").setAttribute("aria-busy", "true");
      try {
        const query = new URLSearchParams({aggregate:c.aggregate, role:c.role, workbench:"true", indexGeneration:c.pin.indexGeneration, indexRevision:String(c.pin.indexRevision)});
        const data = await request(`/api/architecture?${query}`, () => current(c));
        if (!current(c)) return;
        const payload = payloadOf(data, c);
        if (payload.state === "missing_rules") {
          state("No reviewed rules file. Create or review .trellis/rules.toml. The trellis_propose_layers MCP prompt can help propose layers; this browser does not call it.", "empty"); return;
        }
        view = payload; basis = {...c, rulesHash:payload.rulesFile.contentHash}; coverage();
        if (payload.state === "no_layers") { state("No authored layers are configured. Existing authored exceptions can still be edited or removed. Review .trellis/rules.toml to configure layers.", "empty"); overview(); }
        else await render();
        if (options.keepDraft === true) { get("architecture-review-panel").hidden = false; renderReview(); }
      } catch (error) {
        if (!current(c) || error.name === "AbortError") return;
        const diagnostic = error.diagnostic;
        state(diagnostic ? `${diagnostic.code || "invalid_rules"} · ${diagnostic.path}:${diagnostic.line}:${diagnostic.column} · ${diagnostic.kind} · ${diagnostic.message}` : error.message || "Architecture unavailable. Select the Architecture tab again to retry.", "error");
        if (diagnostic) disclosure(get("architecture-notice-content"), "Located rules diagnostic", diagnostic);
        if (error.status === 409 && ["revision_conflict", "pin_expired", "root_changed"].includes(error.code)) onConflict();
      } finally { if (current(c)) get("architecture-diagram").setAttribute("aria-busy", "false"); }
    }
    function same(a, b) { return moduleKey(a) === moduleKey(b); }
    function inspectorSource(target, identity, site, active) {
      const lines = site?.lines;
      const range = lines && Number.isSafeInteger(lines.start) && lines.start > 0 && Number.isSafeInteger(lines.end) && lines.end >= lines.start
        ? {startLine:lines.start, endLine:lines.end, startByte:site.range?.start, endByte:site.range?.end} : null;
      if (!range) return;
      const button = make("button", "Read indexed reference source"); button.type = "button";
      button.addEventListener("click", () => { if (active()) return readSource({path:identity.path, range}, basis.pin, active); }); target.append(button);
    }
    const NO_PAIRS = "No mapped pairs in this scope. This does not prove absence.";
    function pairs(target, values, active, emptyNote = true) {
      if (!values.length) { if (emptyNote) paragraph(target, NO_PAIRS); return; }
      for (const pair of values) {
        const details = make("details"); details.append(make("summary", `${targetLabel(pair.source)} → ${targetLabel(pair.target)} · ${pair.count} reference sites`));
        paragraph(details, `${pair.sites.length} sampled sites · ${pair.omittedSites} omitted sites. Counts remain exact; samples are not exhaustive.`);
        disclosure(details, "Exact reference-pair identity", {source:pair.source, target:pair.target, mapping:pair.mapping});
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
    function values(target, metrics) {
      const list = make("dl", undefined, "metric-inspector-values");
      function visit(value, path) {
        if (value && typeof value === "object") {
          if (Object.hasOwn(value, "unavailable")) { list.append(make("dt", path), make("dd", `${display(value.value)} · ${value.unavailable}`)); }
          else for (const [key, child] of Object.entries(value)) visit(child, path ? `${path} · ${labels[key] || key}` : labels[key] || key);
        } else list.append(make("dt", path), make("dd", display(value)));
      }
      visit(metrics, ""); target.append(list);
    }
    function openReview(mode) {
      if (!usable()) return;
      reviewScope = null; reviewMode = mode;
      // The deviations chip counts unexcepted production deviations; its list shows exactly those.
      if (mode === "rules") { get("architecture-review-role").value = "production"; get("architecture-review-state").value = "deviates"; }
      get("architecture-review-panel").hidden = false; renderReview();
    }
    // Counted review chips head every Architecture inspection (#449); each opens its list.
    // Counts come from the summary: rule-check counts, unassigned files, authored exceptions.
    function reviewChips(target) {
      const chips = make("div", undefined, "architecture-review-chips"), counts = view.rules?.counts;
      chips.setAttribute("role", "group"); chips.setAttribute("aria-label", "Review counts");
      const chip = (id, count, word, open, tone) => {
        const button = make("button", `${count.toLocaleString("en-US")} ${word}`, "architecture-chip"); button.type = "button"; button.id = id;
        if (tone) button.dataset.tone = tone;
        button.addEventListener("click", () => { if (usable() && !editBusy) return open(); }); chips.append(button);
      };
      if (view.state === "ready" && Number.isSafeInteger(counts?.deviates)) chip("architecture-review", counts.deviates, counts.deviates === 1 ? "deviation" : "deviations", () => openReview("rules"), counts.deviates ? "error" : null);
      if (view.state === "ready" && Number.isSafeInteger(view.rules?.unassignedCount)) chip("architecture-unassigned", view.rules.unassignedCount, "unassigned", () => openReview("unassigned"));
      if (Number.isSafeInteger(view.exceptionCount)) chip("architecture-authored", view.exceptionCount, view.exceptionCount === 1 ? "exception" : "exceptions", showAuthored);
      target.append(chips);
    }
    // The inspector's resting content once a snapshot is shown: review chips and coverage.
    // It opens the desktop column but never the narrow-screen drawer.
    function overview() {
      if (!usable() || !["ready", "no_layers"].includes(view.state)) return;
      clearInspection(); window.TrellisShell?.showArchitectureInspector({drawer:false});
      get("architecture-inspector-title").textContent = "Architecture review";
      const target = get("architecture-inspector-content"); target.replaceChildren();
      reviewChips(target);
      if (view.state === "ready") paragraph(target, coverageSummary(view), "detail");
      paragraph(target, view.state === "ready" ? "Select a band, module chip or arrow to inspect its evidence." : "Authored exceptions stay editable without layers.", "detail");
    }
    function inspect(kind, record) {
      if (!usable() || view.state !== "ready") return;
      clearInspection(); const ticket = selectionSerial, payload = view, c = basis;
      const active = () => usable() && current(c) && view === payload && ticket === selectionSerial && !get("architecture-inspector").hidden;
      onInspect(); window.TrellisShell?.showArchitectureInspector();
      const target = get("architecture-inspector-content"), title = get("architecture-inspector-title"); target.replaceChildren();
      reviewChips(target);
      paragraph(target, `Syntax evidence · not compiler-resolved calls · code ${window.TrellisIndexPin.label(payload)} · rules ${c.rulesHash}.`);
      const references = payload.references;
      if (kind === "module") {
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
      } else if (kind === "edge") {
        title.textContent = `${record.sourceLayer} → ${record.destinationLayer}`;
        paragraph(target, `${record.count} exact ${record.role} structural reference sites · ${record.status}${record.excepted ? ` · excepted: ${record.exceptionReason}` : " · not excepted"}${record.role === "test" ? " · hypothetical policy only; never a production violation" : ""}.`);
        reviewScope = record; reviewMode = "rules"; get("architecture-review-state").value = "all"; get("architecture-review-panel").hidden = false; renderReview();
        detailList(target, {kind:"instances", ...edgeFilters(record)}, active, instanceItems(active), "No rule instances match this review scope.");
        pairs(target, record.pairs, active);
      } else if (kind === "layer") {
        title.textContent = record.name; paragraph(target, layerLabel(record));
        for (const module of payload.modules.filter(item => item.layer === record.name)) {
          const button = make("button", module.identity.path); button.type = "button"; button.addEventListener("click", () => { if (active()) inspect("module", module); }); target.append(button);
        }
        const packages = (references.externalPackages || []).filter(item => item.layer === record.name);
        if (packages.length) disclosure(target, "External packages (no native source or metrics)", packages);
        paragraph(target, "Reference pairs from or to this layer", "detail");
        detailList(target, {kind:"pairs", layer:record.name}, active, pairItems(active), NO_PAIRS);
        paragraph(target, "Layer policy and exceptions", "detail");
        detailList(target, {kind:"instances", layer:record.name}, active, instanceItems(active), "No rule instances name this layer.");
      } else if (kind === "unresolved") {
        title.textContent = "Unresolved structural references";
        paragraph(target, `${references.counts?.unresolvedSites ?? 0} unresolved sites.`);
        detailList(target, {kind:"unresolved"}, active, (list, items) => {
          for (const item of items) {
            const entry = make("details"); entry.append(make("summary", `${item.source.path} · ${item.reason}`), make("pre", JSON.stringify(item, null, 2)));
            inspectorSource(entry, item.source, item.site, active); list.append(entry);
          }
        }, "No unresolved sites.");
      } else if (kind === "external") {
        title.textContent = "External policy results";
        paragraph(target, "Production and test-only $external results.", "detail");
        detailList(target, {kind:"instances", rule:"$external"}, active, instanceItems(active), "No external policy results.");
      } else {
        title.textContent = "Same-layer and unassigned references";
        detailList(target, {kind:"pairs", unlayered:"true"}, active, pairItems(active), NO_PAIRS);
        disclosure(target, "Unassigned native file modules", payload.modules.filter(module => module.layer === null));
      }
      for (const limitation of references.limitations || []) paragraph(target, limitation, "detail");
    }
    // Review filters as one exact detail scope; null when the role/verdict selects
    // contradict the chosen edge, so nothing can match.
    function reviewFilters(roleFilter, verdict) {
      const params = {kind:"instances", ...(reviewScope ? edgeFilters(reviewScope) : {})};
      const set = (key, value) => { if (key in params && params[key] !== value) return false; params[key] = value; return true; };
      if (roleFilter !== "all" && !set("role", roleFilter)) return null;
      if (verdict === "excepted" && !set("excepted", "true")) return null;
      if (!["all", "excepted"].includes(verdict) && !(set("status", verdict) && set("excepted", "false"))) return null;
      return params;
    }
    function portable(identity) { return {language:identity.language, path:identity.path}; }
    function pairKey(row) { return JSON.stringify([portable(row.target), portable(row.destination)]); }
    function eligible({instance:r, role:rowRole}) {
      return rowRole === "production" && r.rule === "$layers" && r.status === "deviates" && !r.excepted &&
        !!r.target?.path && !!r.destination?.path && !external(r.target) && !external(r.destination) && r.referenceCount > 0;
    }
    function action(target, label, fn, active = () => usable()) {
      const button = make("button", label); button.type = "button";
      button.addEventListener("click", () => { if (active() && !editBusy) return fn(); }); target.append(button); return button;
    }
    function renderInstances(target, rows, active, emptyNote = true) {
      for (const row of rows) {
        const r = row.instance, entry = make("div", undefined, "architecture-rule-row");
        entry.dataset.rule = r.rule; entry.dataset.source = r.target?.path || ""; entry.dataset.destination = r.destination?.path || "";
        const label = `${targetLabel(r.target)} → ${r.destination ? targetLabel(r.destination) : "Unknown destination"}`;
        {
          const choose = make("input"); choose.type = "checkbox"; choose.disabled = !eligible(row); choose.checked = eligible(row) && selected.has(pairKey(r));
          choose.setAttribute("aria-label", `Select ${label}`);
          choose.addEventListener("change", () => { if (!active() || editBusy || !eligible(row)) return;
            if (choose.checked) selected.set(pairKey(r), {source:portable(r.target), destination:portable(r.destination)}); else selected.delete(pairKey(r));
            get("architecture-add-selected").textContent = `Add selected exceptions (${selected.size})`;
          }); entry.append(choose);
        }
        action(entry, `${r.rule} · ${label}`, () => inspectRule(row), active);
        paragraph(entry, `${row.role} · ${r.sourceLayer ?? "Unassigned"} → ${r.destinationLayer ?? "Unknown"} · ${r.status} · ${r.excepted ? `excepted: ${r.exceptionReason}` : "not excepted"} · ${r.referenceCount ?? 0} exact reference sites`);
        if (!eligible(row)) paragraph(entry, r.excepted ? "Already excepted. Edit or remove the authored entry." :
          "New exceptions require a positively witnessed production $layers deviation between native files; unknown, external and test-only rows cannot be waived.", "detail");
        target.append(entry);
      }
      if (!rows.length && emptyNote) paragraph(target, "No rule instances match this review scope.");
    }
    function inspectRule(row) {
      if (!usable()) return;
      clearInspection(); const ticket = selectionSerial, payload = view, c = basis;
      const active = () => usable() && view === payload && current(c) && ticket === selectionSerial && !get("architecture-inspector").hidden;
      onInspect(); window.TrellisShell?.showArchitectureInspector();
      const r = row.instance, target = get("architecture-inspector-content"); target.replaceChildren();
      get("architecture-inspector-title").textContent = `${r.rule} · ${targetLabel(r.target)}`;
      reviewChips(target);
      paragraph(target, `${row.role} · ${r.status} · ${r.excepted ? `excepted: ${r.exceptionReason}` : "not excepted"} · ${r.referenceCount ?? 0} exact reference sites.`);
      paragraph(target, `Code ${window.TrellisIndexPin.label(payload)} · rules ${c.rulesHash}. Captured spelling and source-owner identities are syntax evidence, not destination symbol bindings.`);
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
      get("architecture-review-scope").textContent = reviewMode === "unassigned" ? "Unassigned files (including files without edges)" :
        reviewMode === "authored" ? "Authored exceptions" : reviewScope ? `${reviewScope.sourceLayer} → ${reviewScope.destinationLayer} · ${reviewScope.role}` : "All rule instances";
      get("architecture-exceptions").hidden = reviewMode !== "authored";
      const c = basis, ticket = ++reviewSerial, active = () => usable() && current(c);
      const listed = () => active() && ticket === reviewSerial;
      if (reviewMode === "unassigned" && view.state === "no_layers") {
        paragraph(target, "No layers configured. File assignment inventory is unavailable until layers are configured; authored exceptions remain editable.");
      } else if (reviewMode === "unassigned") {
        for (const module of view.modules.filter(item => item.layer === null)) {
          const row = make("div", undefined, "architecture-rule-row"); row.dataset.source = module.identity.path;
          action(row, module.identity.path, () => inspect("module", module), active);
          const label = make("label", "Assign to layer "), select = make("select");
          select.setAttribute("aria-label", `Layer for ${module.identity.path}`);
          for (const layer of view.layers) { const option = make("option", layer.name); option.value = layer.name; select.append(option); }
          label.append(select); row.append(label);
          action(row, "Assign file", () => beginDraft({type:"assignFile", path:module.identity.path, layer:select.value || view.layers[0]?.name}), active).disabled = !view.layers.length; target.append(row);
        }
        if (!target.children.length) paragraph(target, "All indexed native files have an assignment.");
      } else if (reviewMode === "rules") {
        const roleFilter = get("architecture-review-role").value, verdict = get("architecture-review-state").value;
        const params = reviewFilters(roleFilter, verdict), empty = "No rule instances match this review scope.";
        if (params) detailList(target, params, listed, instanceItems(active), empty); else paragraph(target, empty);
      }
      const select = get("architecture-destination-layer"), value = select.value; select.replaceChildren();
      const all = make("option", "All layers"); all.value = ""; select.append(all);
      for (const layer of view.layers) { const option = make("option", layer.name); option.value = layer.name; select.append(option); } select.value = value;
      get("architecture-add-selected").textContent = `Add selected exceptions (${selected.size})`;
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
      reviewMode = "authored"; reviewScope = null; get("architecture-review-panel").hidden = false; renderReview();
      const target = get("architecture-exceptions"); target.replaceChildren(); paragraph(target, "Reading authored exceptions…");
      const c = basis;
      try {
        const data = await loadDescriptor(); if (!data || !current(c) || reviewMode !== "authored") return;
        target.replaceChildren();
        for (const entry of data.exceptions) {
          const row = make("section", undefined, "architecture-rule-row"); row.dataset.exceptionIndex = String(entry.index);
          paragraph(row, `${entry.rule} · ${JSON.stringify(entry.target)}${entry.destination ? ` → ${JSON.stringify(entry.destination)}` : ""} · ${entry.reason}`);
          action(row, "Edit reason", () => beginDraft({type:"editException", index:entry.index, reason:entry.reason}), () => current(c) && usable());
          action(row, "Remove", () => beginDraft({type:"removeException", index:entry.index}), () => current(c) && usable()); target.append(row);
        }
        if (!data.exceptions.length) paragraph(target, "No authored exceptions. Entries remain editable here even when no current reference matches.");
      } catch (error) { if (current(c)) { target.replaceChildren(); paragraph(target, error.message); } }
    }
    function beginDraft(operation) {
      if (!usable() || editBusy) return;
      if (draft?.needsReselect && operation.type === "editException") operation.reason = get("architecture-reason").value;
      invalidatePreview(); draft = {operation, baseHash:basis.rulesHash, basis, needsReselect:false};
      get("architecture-review-panel").hidden = false; get("architecture-editor").hidden = false;
      const reason = get("architecture-reason"); reason.value = operation.reason || ""; reason.hidden = !["addExceptions", "editException"].includes(operation.type);
      get("architecture-reason-label").hidden = reason.hidden;
      get("architecture-edit-title").textContent = {addExceptions:"Add finite pair exceptions",editException:"Edit exception reason",removeException:"Remove authored exception",assignFile:"Assign file to layer"}[operation.type];
      get("architecture-edit-scope").textContent = operation.type === "addExceptions" ? `${operation.pairs.length} concrete witnessed pairs. One reason, one preview and one apply.\n${operation.pairs.map(pair => `${pair.source.path} → ${pair.destination.path}`).join("\n")}` : JSON.stringify(operation);
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
    function selectScope() {
      if (!usable() || editBusy) return;
      const glob = get("architecture-source-glob").value || "**", layer = get("architecture-destination-layer").value;
      // Selection only: the same whole-component **, * and ? grammar; never persisted.
      const components = glob.split("/");
      if (components.some(part => !part || part.includes("**") && part !== "**" || /[\[\]{}\\]/.test(part))) {
        get("architecture-review-selection-state").textContent = "Use literal path components, * or ?, and whole-component ** only."; return;
      }
      function componentMatches(pattern, text) {
        let reachable = new Set([0]);
        for (const char of pattern) {
          const next = new Set();
          if (char === "*" && reachable.size) {
            const first = Math.min(...reachable);
            for (let end = first; end <= text.length; end++) next.add(end);
          } else for (const index of reachable) {
            if (index < text.length && (char === "?" || char === text[index])) next.add(index + 1);
          }
          reachable = next;
        }
        return reachable.has(text.length);
      }
      function matches(path) {
        const parts = path.split("/"); let reachable = new Set([0]);
        for (const component of components) {
          const next = new Set();
          if (component === "**" && reachable.size) {
            const first = Math.min(...reachable);
            for (let end = first; end <= parts.length; end++) next.add(end);
          } else for (const index of reachable) {
            if (index < parts.length && componentMatches(component, parts[index])) next.add(index + 1);
          }
          reachable = next;
        }
        return reachable.has(parts.length);
      }
      // Only unexcepted production $layers deviations are eligible; read exactly those pages.
      const c = basis, owner = setBusy(true), active = () => usable() && current(c);
      const state = get("architecture-review-selection-state"); state.textContent = "Reading witnessed deviations…";
      return (async () => {
        try {
          const rows = [];
          for (let offset = 0; offset !== null;) {
            const page = await details({kind:"instances", role:"production", rule:"$layers", status:"deviates", excepted:"false", limit:"500"}, active, offset);
            if (!active()) return;
            rows.push(...page.items); offset = page.nextOffset;
          }
          selected.clear();
          for (const row of rows) if (eligible(row) && matches(row.instance.target.path) && (!layer || row.instance.destinationLayer === layer)) {
            const r = row.instance; selected.set(pairKey(r), {source:portable(r.target),destination:portable(r.destination)});
          }
          state.textContent = `${selected.size} finite witnessed pairs selected. No wildcard exception will be written.`;
          renderReview();
        } catch (error) {
          if (active() && error.name !== "AbortError") state.textContent = error.message || "Selection failed. Nothing was selected.";
        } finally { setBusy(false, owner); }
      })();
    }
    get("architecture-review-role").value = "all"; get("architecture-review-state").value = "deviates";
    get("architecture-review-close").addEventListener("click", () => { get("architecture-review-panel").hidden = true; });
    for (const id of ["architecture-review-role","architecture-review-state"]) get(id).addEventListener("change", renderReview);
    get("architecture-select-scope").addEventListener("click", selectScope);
    get("architecture-add-selected").addEventListener("click", () => { if (selected.size) beginDraft({type:"addExceptions",pairs:[...selected.values()],reason:""}); });
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
    get("architecture-hidden-toggle").addEventListener("click", () => {
      if (!usable()) return;
      shown.hidden = !shown.hidden; saveDisplay(); return rerender({});
    });
    get("architecture-color").addEventListener("change", event => { metric = event.target.value; return render(); });
    get("architecture-tests").addEventListener("change", event => { showTests = event.target.checked; return render(); });
    get("architecture-aggregate").addEventListener("change", event => {
      if (!["max", "sum", "mean"].includes(event.target.value) || event.target.value === aggregate) return;
      aggregate = event.target.value; return refresh();
    });
    return {refresh, reset, clearInspection};
  }
  window.TrellisArchitecture = {create, moduleKey, quartiles, band, metricValue, commonDirectory, groupModules};
})();
