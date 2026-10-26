"use strict";
// One owned code snapshot plus one owned rules digest. No live rules re-evaluation.
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
  const moduleKey = identity => JSON.stringify([identity.sourceSetId, identity.language, identity.path, identity.sid]);
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
    function reset(message = "Open Architecture to read the indexed snapshot and reviewed rules.") {
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
          (payload.state === "ready" && (typeof payload.rulesFile?.contentHash !== "string" || !payload.rulesFile.contentHash))) {
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
      paragraph(target, `${unassigned.length} unassigned native file modules remain unknown.`);
      const choose = (label, action) => { const button = make("button", label); button.type = "button"; button.addEventListener("click", action); target.append(button); };
      const c = basis;
      choose("Inspect same-layer and unassigned references", () => { if (current(c) && usable()) inspect("unknown", null); });
      choose("Inspect unresolved sites", () => { if (current(c) && usable()) inspect("unresolved", null); });
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
      return [layer.name, `${prefix} · ${modules.length} files`,
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
      if (!usable()) return;
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
    async function refresh() {
      reset();
      const c = capture();
      if (!c.rootKey || !c.pin) { state("Choose an indexed checkout, then open Architecture.", "empty"); return; }
      state("Reading indexed architecture and reviewed rules…", "loading"); get("architecture-diagram").setAttribute("aria-busy", "true");
      try {
        const query = new URLSearchParams({aggregate:c.aggregate, role:c.role, indexGeneration:c.pin.indexGeneration, indexRevision:String(c.pin.indexRevision)});
        const data = await request(`/api/architecture?${query}`, () => current(c));
        if (!current(c)) return;
        const payload = payloadOf(data, c);
        if (payload.state !== "ready") {
          state(`${payload.state === "missing_rules" ? "No reviewed rules file." : "No authored layers are configured."} Create or review .trellis/rules.toml. The trellis_propose_layers MCP prompt can help propose layers; this browser does not call it or write rules.`, "empty"); return;
        }
        view = payload; basis = {...c, rulesHash:payload.rulesFile.contentHash}; coverage(); await render();
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
        const details = make("details"); details.append(make("summary", `${pair.source.path} → ${pair.target.path} · ${pair.count} reference sites`));
        paragraph(details, `${pair.sites.length} sampled sites · ${pair.omittedSites} omitted sites. Counts remain exact; samples are not exhaustive.`);
        disclosure(details, "Exact module-pair identity", {source:pair.source, target:pair.target, mapping:pair.mapping});
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
      if (!usable()) return;
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
        pairs(target, record.pairs, active);
      } else if (kind === "layer") {
        title.textContent = record.name; paragraph(target, layerLabel(record));
        for (const module of payload.modules.filter(item => item.layer === record.name)) {
          const button = make("button", module.identity.path); button.type = "button"; button.addEventListener("click", () => { if (active()) inspect("module", module); }); target.append(button);
        }
        const member = identity => payload.modules.some(module => module.layer === record.name && same(module.identity, identity));
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
        const membership = new Map(payload.modules.map(module => [moduleKey(module.identity), module.layer]));
        pairs(target, [...references.production, ...references.tests].filter(pair => {
          const source = membership.get(moduleKey(pair.source)), destination = membership.get(moduleKey(pair.target));
          return source == null || destination == null || source === destination;
        }), active);
        disclosure(target, "Unassigned native file modules", payload.modules.filter(module => module.layer === null));
        disclosure(target, "Owned policy evaluation", payload.rules);
      }
      for (const limitation of references.limitations || []) paragraph(target, limitation, "detail");
    }
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
