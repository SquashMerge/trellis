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
  const metricValue = (metrics, name) => name.split(".").reduce((value, key) => value?.[key], metrics);
  const quartiles = values => {
    const sorted = values.filter(finite).sort((a, b) => a - b);
    return sorted.length ? [.25, .5, .75].map(p => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))]) : [];
  };
  const band = (value, cuts, metric) => {
    if (!finite(value) || cuts.length !== 3) return null;
    const rank = cuts.filter(cut => value > cut).length;
    return metric === "maintainabilityIndex" ? 3 - rank : rank;
  };
  const labels = {loc:"LOC (callable ranges)", sloc:"SLOC (callable ranges)", commentLines:"Comment lines (callable ranges)",
    cognitive:"Cognitive", cyclomatic:"Cyclomatic", maxNesting:"Max nesting", maintainabilityIndex:"Maintainability (higher is better)",
    fanOut:"Fan-out (call sites)", fanIn:"Fan-in", halstead:"Halstead", parameters:"Parameters", exits:"Exits"};
  function create({request, context, readSource, openTreemap, onConflict = () => {}, onInspect = () => {}}) {
    let serial = 0, renderSerial = 0, selectionSerial = 0, view = null, basis = null;
    let metric = "cognitive", aggregate = "max", role = "production", showTests = false;
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
      state(message, "empty");
    }
    function payloadOf(data, c) {
      const payload = data?.payload;
      if (!payload || !window.TrellisIndexPin.equal(payload, c.pin)) {
        const error = new Error("Architecture snapshot changed. Refresh status."); error.code = "revision_conflict"; error.status = 409; throw error;
      }
      if (!["ready", "missing_rules", "no_layers"].includes(payload.state) || payload.aggregate !== c.aggregate || payload.role !== c.role ||
          payload.metricsVersion !== "metrics:v1" ||
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
      const bans = [...(view.rules?.instances || []), ...(view.rules?.testOnly || [])].filter(instance => instance.rule === "$external");
      if (bans.length) disclosure(target, "External policy results (production and test-only)", bans);
      const choose = (label, action) => { const button = make("button", label); button.type = "button"; button.addEventListener("click", action); target.append(button); };
      const c = basis;
      choose("Inspect same-layer and unassigned references", () => { if (current(c) && usable()) inspect("unknown", null); });
      choose("Inspect unresolved sites", () => { if (current(c) && usable()) inspect("unresolved", null); });
    }
    function layerOf(identity) {
      if (!external(identity)) return view.modules.find(module => same(module.identity, identity))?.layer;
      return [...(view.rules?.instances || []), ...(view.rules?.testOnly || [])]
        .find(instance => instance.rule === "$layers" && instance.destination && same(instance.destination, identity))?.destinationLayer;
    }
    function layerLabel(layer) {
      const modules = view.modules.filter(module => module.layer === layer.name);
      const values = modules.map(module => metricValue(module.metrics, metric)).filter(finite);
      const prefix = layer.shared ? "Shared · any layer may use" : `Order ${layer.order}`;
      const outgoing = view.edges.filter(edge => edge.sourceLayer === layer.name && edge.role === "production");
      const byName = new Map(view.layers.map(item => [item.name, item]));
      let down = 0, up = 0, upDeviation = 0, other = 0, exceptions = 0;
      for (const edge of outgoing) {
        const destination = byName.get(edge.destinationLayer);
        if (edge.excepted) exceptions += edge.count;
        if (!layer.shared && destination && !destination.shared && destination.order > layer.order) down += edge.count;
        else if (!layer.shared && destination && !destination.shared && destination.order < layer.order) {
          up += edge.count;
          if (edge.status === "deviates" && !edge.excepted) upDeviation += edge.count;
        }
        else if (edge.status === "deviates" && !edge.excepted) other += edge.count;
      }
      const packages = (view.references?.externalPackages || []).filter(item => layerOf(item.identity) === layer.name);
      return [layer.name, `${prefix} · ${modules.length} files${packages.length ? ` · ${packages.length} external packages` : ""}`,
        ...(metric === "layer" ? [] : [`Max module ${metric} (${aggregate})`, values.length ? String(values.reduce((max, value) => Math.max(max, value), -Infinity)) : "—"]),
        `Prod refs down: ${down}`, `Prod refs up: ${up}`, `Up deviation: ${upDeviation}`, `Peer/shared deviation: ${other}`, `Excepted refs: ${exceptions}`].join("\n");
    }
    function legend(cuts) {
      const target = get("architecture-legend"); target.replaceChildren();
      if (metric === "layer") { paragraph(target, "Layer membership · neutral chips; no metric heat."); return; }
      paragraph(target, `${metric} · ${aggregate} · visible native file modules · ${population()}${metric === "maintainabilityIndex" ? " · higher is better; heat inverted" : ""}`);
      if (!cuts.length) { paragraph(target, `No measured values · ${population()} · — is unavailable, not zero.`); return; }
      const ranges = [`≤${cuts[0]}`, `>${cuts[0]}–${cuts[1]}`, `>${cuts[1]}–${cuts[2]}`, `>${cuts[2]}`];
      ranges.forEach((range, rank) => {
        const entry = make("span", undefined, "metric-legend-entry");
        entry.append(make("span", undefined, `metric-swatch heat-${metric === "maintainabilityIndex" ? 3 - rank : rank}`), make("span", range)); target.append(entry);
      });
      target.append(make("span", "Low heat → high heat · ties stay together · — unavailable"));
    }
    async function render() {
      if (!usable() || view.state !== "ready") return;
      const c = basis, payload = view, ticket = ++renderSerial;
      const active = () => current(c) && view === payload && ticket === renderSerial && payload.rulesFile.contentHash === c.rulesHash;
      clearInspection(); get("architecture-diagram").replaceChildren();
      const cuts = metric === "layer" ? [] : quartiles(payload.modules.map(module => metricValue(module.metrics, metric)));
      legend(cuts);
      try {
        if (!window.TrellisArchitecturePele) throw new Error("Architecture renderer unavailable. Nothing displayed.");
        const outcome = await window.TrellisArchitecturePele.render(get("architecture-diagram"), payload, {
          showTests, metric, aggregate, isCurrent:active,
          moduleValue:module => metric === "layer" ? undefined : metricValue(module.metrics, metric),
          moduleBand:module => metric === "layer" ? null : band(metricValue(module.metrics, metric), cuts, metric),
          layerLabel,
          onSelectModule:module => { if (active() && payload.modules.includes(module)) inspect("module", module); },
          onSelectEdge:edge => { if (active() && payload.edges.includes(edge)) inspect("edge", edge); },
          onSelectLayer:layer => { if (active() && payload.layers.includes(layer)) inspect("layer", layer); },
        });
        if (!active()) return;
        if (outcome?.status !== "ready") throw new Error(outcome?.reason || outcome?.error?.message || "Architecture mapping failed. Nothing displayed.");
        const production = payload.edges.filter(edge => edge.role === "production").reduce((sum, edge) => sum + edge.count, 0);
        const tests = payload.edges.filter(edge => edge.role === "test").reduce((sum, edge) => sum + edge.count, 0);
        state(`${payload.modules.length} native file modules · ${production} production / ${tests} test-only cross-layer reference sites${showTests ? " · tests shown" : " · tests hidden"}. Metrics: ${aggregate}, ${population()}.`, "ready");
      } catch (error) {
        if (!active()) return;
        get("architecture-diagram").replaceChildren(); get("architecture-legend").replaceChildren(); clearInspection();
        state(error.message || "Architecture rendering failed. Nothing displayed.", "error");
      }
    }
    async function refresh(options = {}) {
      reset(undefined, options.keepDraft === true);
      const c = capture();
      if (!c.rootKey || !c.pin) { state("Choose an indexed checkout, then open Architecture.", "empty"); return; }
      state("Reading indexed architecture and reviewed rules…", "loading"); get("architecture-diagram").setAttribute("aria-busy", "true");
      try {
        const query = new URLSearchParams({aggregate:c.aggregate, role:c.role, indexGeneration:c.pin.indexGeneration, indexRevision:String(c.pin.indexRevision)});
        const data = await request(`/api/architecture?${query}`, () => current(c));
        if (!current(c)) return;
        const payload = payloadOf(data, c);
        if (payload.state === "missing_rules") {
          state("No reviewed rules file. Create or review .trellis/rules.toml. The trellis_propose_layers MCP prompt can help propose layers; this browser does not call it.", "empty"); return;
        }
        view = payload; basis = {...c, rulesHash:payload.rulesFile.contentHash}; coverage();
        if (payload.state === "no_layers") state("No authored layers are configured. Existing authored exceptions can still be edited or removed. Review .trellis/rules.toml to configure layers.", "empty");
        else await render();
        if (options.keepDraft === true) { get("architecture-review-panel").hidden = false; renderReview(); }
      } catch (error) {
        if (!current(c) || error.name === "AbortError") return;
        const diagnostic = error.diagnostic;
        state(diagnostic ? `${diagnostic.code || "invalid_rules"} · ${diagnostic.path}:${diagnostic.line}:${diagnostic.column} · ${diagnostic.kind} · ${diagnostic.message}` : error.message || "Architecture unavailable. Refresh to try again.", "error");
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
    function pairs(target, values, active) {
      if (!values.length) { paragraph(target, "No mapped pairs in this scope. This does not prove absence."); return; }
      for (const pair of values) {
        const details = make("details"); details.append(make("summary", `${targetLabel(pair.source)} → ${targetLabel(pair.target)} · ${pair.count} reference sites`));
        paragraph(details, `${pair.sites.length} sampled sites · ${pair.omittedSites} omitted sites. Counts remain exact; samples are not exhaustive.`);
        disclosure(details, "Exact reference-pair identity", {source:pair.source, target:pair.target, mapping:pair.mapping});
        if (external(pair.target)) {
          const descriptor = view.references.externalPackages.find(item => same(item.identity, pair.target));
          disclosure(details, "External package evidence", descriptor);
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
    function inspect(kind, record) {
      if (!usable() || view.state !== "ready") return;
      clearInspection(); const ticket = selectionSerial, payload = view, c = basis;
      const active = () => usable() && current(c) && view === payload && ticket === selectionSerial && !get("architecture-inspector").hidden;
      onInspect(); window.TrellisShell?.showArchitectureInspector();
      const target = get("architecture-inspector-content"), title = get("architecture-inspector-title"); target.replaceChildren();
      paragraph(target, `Syntax evidence · not compiler-resolved calls · code ${window.TrellisIndexPin.label(payload)} · rules ${c.rulesHash}.`);
      const references = payload.references;
      if (kind === "module") {
        title.textContent = record.identity.path;
        paragraph(target, `Layer: ${record.layer ?? "Unassigned / unknown"} · ${record.physicalLines} physical file lines · ${record.declarationCount} ${callables()}.`);
        paragraph(target, `${aggregate} · metrics:v1 · ${population()}. Callable-range LOC may overlap; it is not physical file lines.`);
        disclosure(target, "Native file-module identity and captured source", {identity:record.identity, revisionId:record.revisionId, contentHash:record.contentHash, provenanceId:record.provenanceId});
        if (record.declarationCount === 0) paragraph(target, `No ${callables()}. Numeric aggregates are unavailable, not zero.`);
        values(target, record.metrics);
        if (record.physicalLines > 0) {
          const button = make("button", "Read indexed file source"); button.type = "button";
          button.addEventListener("click", () => { if (active()) return readSource({path:record.identity.path, range:{startLine:1,endLine:record.physicalLines}}, c.pin, active); }); target.append(button);
        }
        if (typeof openTreemap === "function") {
          const button = make("button", "Open in Treemap"); button.type = "button";
          button.addEventListener("click", () => { if (active()) openTreemap(record.identity, c.pin); }); target.append(button);
        }
        pairs(target, [...references.production, ...references.tests].filter(pair => same(pair.source, record.identity) || same(pair.target, record.identity)), active);
        disclosure(target, "Owned rule evaluation (including exceptions and test-only outcomes)", payload.rules);
      } else if (kind === "edge") {
        title.textContent = `${record.sourceLayer} → ${record.destinationLayer}`;
        paragraph(target, `${record.count} exact ${record.role} structural reference sites · ${record.status}${record.excepted ? ` · excepted: ${record.exceptionReason}` : " · not excepted"}${record.role === "test" ? " · hypothetical policy only; never a production violation" : ""}.`);
        reviewScope = record; reviewMode = "rules"; get("architecture-review-state").value = "all"; get("architecture-review-panel").hidden = false; renderReview();
        renderInstances(target, scopedRows(), active);
        pairs(target, record.pairs, active);
      } else if (kind === "layer") {
        title.textContent = record.name; paragraph(target, layerLabel(record));
        for (const module of payload.modules.filter(item => item.layer === record.name)) {
          const button = make("button", module.identity.path); button.type = "button"; button.addEventListener("click", () => { if (active()) inspect("module", module); }); target.append(button);
        }
        const packages = (references.externalPackages || []).filter(item => layerOf(item.identity) === record.name);
        if (packages.length) disclosure(target, "External packages (no native source or metrics)", packages);
        const member = identity => layerOf(identity) === record.name;
        pairs(target, [...references.production, ...references.tests].filter(pair => member(pair.source) || member(pair.target)), active);
        disclosure(target, "Owned layer policy and exceptions", payload.rules);
      } else if (kind === "unresolved") {
        title.textContent = "Unresolved structural references";
        for (const item of references.unresolved) {
          const entry = make("details"); entry.append(make("summary", `${item.source.path} · ${item.reason}`), make("pre", JSON.stringify(item, null, 2)));
          inspectorSource(entry, item.source, item.site, active); target.append(entry);
        }
      } else {
        title.textContent = "Same-layer and unassigned references";
        pairs(target, [...references.production, ...references.tests].filter(pair => {
          const source = layerOf(pair.source), destination = layerOf(pair.target);
          return source == null || destination == null || source === destination;
        }), active);
        disclosure(target, "Unassigned native file modules", payload.modules.filter(module => module.layer === null));
        disclosure(target, "Owned policy evaluation", payload.rules);
      }
      for (const limitation of references.limitations || []) paragraph(target, limitation, "detail");
    }
    function ruleRows() {
      return [...(view.rules?.instances || []).map(instance => ({instance, role:"production"})),
        ...(view.rules?.testOnly || []).map(instance => ({instance, role:"test"}))];
    }
    function scopedRows() {
      return ruleRows().filter(({instance:r, role:rowRole}) => !reviewScope ||
        rowRole === reviewScope.role && r.sourceLayer === reviewScope.sourceLayer && r.destinationLayer === reviewScope.destinationLayer &&
        r.status === reviewScope.status && !!r.excepted === !!reviewScope.excepted && (r.exceptionReason || null) === (reviewScope.exceptionReason || null));
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
    function renderInstances(target, rows, active) {
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
      if (!rows.length) paragraph(target, "No rule instances match this review scope.");
    }
    function inspectRule(row) {
      if (!usable()) return;
      clearInspection(); const ticket = selectionSerial, payload = view, c = basis;
      const active = () => usable() && view === payload && current(c) && ticket === selectionSerial && !get("architecture-inspector").hidden;
      onInspect(); window.TrellisShell?.showArchitectureInspector();
      const r = row.instance, target = get("architecture-inspector-content"); target.replaceChildren();
      get("architecture-inspector-title").textContent = `${r.rule} · ${targetLabel(r.target)}`;
      paragraph(target, `${row.role} · ${r.status} · ${r.excepted ? `excepted: ${r.exceptionReason}` : "not excepted"} · ${r.referenceCount ?? 0} exact reference sites.`);
      paragraph(target, `Code ${window.TrellisIndexPin.label(payload)} · rules ${c.rulesHash}. Captured spelling and source-owner identities are syntax evidence, not destination symbol bindings.`);
      disclosure(target, "Captured rule evidence and constraints", r);
      const refs = row.role === "test" ? payload.references.tests : payload.references.production;
      pairs(target, refs.filter(pair => same(pair.source, r.target) && r.destination && same(pair.target, r.destination)), active);
      if (eligible(row)) action(target, "Add exception for this pair", () => beginDraft({type:"addExceptions", pairs:[{source:portable(r.target), destination:portable(r.destination)}], reason:""}), active);
    }
    function renderReview() {
      if (!usable()) return;
      const target = get("architecture-review-list"); target.replaceChildren();
      get("architecture-review-scope").textContent = reviewMode === "unassigned" ? "Unassigned files (including files without edges)" :
        reviewMode === "authored" ? "Authored exceptions" : reviewScope ? `${reviewScope.sourceLayer} → ${reviewScope.destinationLayer} · ${reviewScope.role}` : "All rule instances";
      get("architecture-exceptions").hidden = reviewMode !== "authored";
      const c = basis, active = () => usable() && current(c);
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
        renderInstances(target, scopedRows().filter(({instance:r,role:rowRole}) => (roleFilter === "all" || roleFilter === rowRole) &&
          (verdict === "all" || verdict === "excepted" ? verdict === "all" || r.excepted : r.status === verdict && !r.excepted)), active);
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
      if (["committed", "durability_uncertain"].includes(error.mutationOutcome)) {
        if (draft) draft.needsReload = true;
        const message = error.mutationOutcome === "durability_uncertain" ? "Rules were written, but crash durability is uncertain." : "Rules were committed.";
        const detail = `${message} The response failed after the write: ${error.message || error.code || "unknown error"}. Inspect disk and reload rules before any new preview. No retry was made.`;
        renderSerial++; clearInspection(); view = null;
        get("architecture-diagram").replaceChildren(); get("architecture-legend").replaceChildren();
        editState(detail, "error"); state(detail, "stale");
        return;
      }
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
        else { state(message + "Recheck failed or rules changed again. Refresh before reviewing; no old graph is current.", "stale"); }
      } catch (error) { if (active()) { invalidatePreview(); editError(error); } }
      finally { setBusy(false, busy); }
    }
    async function reloadRules() {
      if (!draft || editBusy) return;
      const owned = draft, prior = owned.basis, reason = get("architecture-reason").value;
      if (!current(prior)) return;
      invalidatePreview(); const busy = setBusy(true);
      try {
        await refresh({keepDraft:true});
        if (!usable() || draft !== owned) { editState("Draft kept, but rules recheck failed. Refresh before authoring.", "error"); return; }
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
      selected.clear();
      for (const row of ruleRows()) if (eligible(row) && matches(row.instance.target.path) && (!layer || row.instance.destinationLayer === layer)) {
        const r = row.instance; selected.set(pairKey(r), {source:portable(r.target),destination:portable(r.destination)});
      }
      get("architecture-review-selection-state").textContent = `${selected.size} finite witnessed pairs selected. No wildcard exception will be written.`;
      renderReview();
    }
    get("architecture-review-role").value = "all"; get("architecture-review-state").value = "deviates";
    get("architecture-review").addEventListener("click", () => { if (usable()) { reviewScope = null; reviewMode = "rules"; get("architecture-review-panel").hidden = false; renderReview(); } });
    get("architecture-unassigned").addEventListener("click", () => { if (usable()) { reviewScope = null; reviewMode = "unassigned"; get("architecture-review-panel").hidden = false; renderReview(); } });
    get("architecture-authored").addEventListener("click", showAuthored);
    get("architecture-review-close").addEventListener("click", () => { get("architecture-review-panel").hidden = true; });
    for (const id of ["architecture-review-role","architecture-review-state"]) get(id).addEventListener("change", renderReview);
    get("architecture-select-scope").addEventListener("click", selectScope);
    get("architecture-add-selected").addEventListener("click", () => { if (selected.size) beginDraft({type:"addExceptions",pairs:[...selected.values()],reason:""}); });
    get("architecture-reason").addEventListener("input", invalidatePreview);
    get("architecture-preview").addEventListener("click", previewDraft);
    get("architecture-confirm").addEventListener("click", applyDraft);
    get("architecture-rules-reload").addEventListener("click", reloadRules);
    get("architecture-cancel").addEventListener("click", () => { if (!editBusy) { invalidatePreview(); draft = null; get("architecture-editor").hidden = true; } });
    get("architecture-color").value = metric; get("architecture-aggregate").value = aggregate;
    get("architecture-tests").checked = showTests;
    get("architecture-metric-tests").checked = false;
    get("architecture-metric-tests").addEventListener("change", event => {
      const next = event.target.checked ? "all" : "production";
      if (next === role) return;
      role = next; return refresh();
    });
    get("architecture-refresh").addEventListener("click", refresh);
    get("architecture-color").addEventListener("change", event => { metric = event.target.value; return render(); });
    get("architecture-tests").addEventListener("change", event => { showTests = event.target.checked; return render(); });
    get("architecture-aggregate").addEventListener("change", event => {
      if (!["max", "sum", "mean"].includes(event.target.value) || event.target.value === aggregate) return;
      aggregate = event.target.value; return refresh();
    });
    return {refresh, reset, clearInspection};
  }
  window.TrellisArchitecture = {create, moduleKey, quartiles, band, metricValue};
})();
