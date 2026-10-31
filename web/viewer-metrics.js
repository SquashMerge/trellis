"use strict";
// Indexed snapshot metrics only; the live filesystem tree owns navigation.
(() => {
  const get = id => document.getElementById(id);
  const make = (tag, text, className) => {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (className) node.className = className;
    return node;
  };
  const finite = value => typeof value === "number" && Number.isFinite(value);
  const display = value => finite(value) ? String(value) : "—";
  const {metricValue, quartiles, relativeBand:band, createScale, exclusions} = window.TrellisMetricHeat;
  const pair = value => value && typeof value.indexGeneration === "string" &&
    Number.isSafeInteger(value.indexRevision) && value.indexRevision >= 0
    ? {indexGeneration:value.indexGeneration, indexRevision:value.indexRevision} : null;
  const equalPair = (a, b) => !!a && !!b && a.indexGeneration === b.indexGeneration && a.indexRevision === b.indexRevision;
  const labels = {loc:"LOC (callable ranges)", sloc:"SLOC (callable ranges)", commentLines:"Comment lines (callable ranges)",
    cognitive:"Cognitive", cyclomatic:"Cyclomatic", maxNesting:"Max nesting", maintainabilityIndex:"Maintainability (higher is better)",
    halstead:"Halstead", fanIn:"Fan-in", fanOut:"Fan-out (call sites)", parameters:"Parameters", exits:"Exits"};
  const colorNames = {cognitive:"Cognitive", cyclomatic:"Cyclomatic", maxNesting:"Max nesting", maintainabilityIndex:"Maintainability",
    "halstead.volume":"Halstead volume", fanOut:"Fan-out"};
  function create({request, context, onConflict = () => {}, onChange = () => {}}) {
    let metric = "cognitive", aggregate = "max", role = "production", relative = false, epoch = 0, refreshSerial = 0, inspectionSerial = 0;
    let projection = null, projectionGuard = null, selected = null, loading = false;
    let decorations = [];
    const callables = () => role === "all" ? "indexed callables" : "production callables";
    const population = () => role === "all" ? "all indexed callables" : callables();
    const text = (id, value) => { if (get(id)) get(id).textContent = value; };
    function capture() {
      const now = context();
      return {rootKey:now.rootKey, session:now.session, pin:pair(now.pin), aggregate, role, epoch};
    }
    function current(guard) {
      const now = context();
      return guard.epoch === epoch && guard.rootKey === now.rootKey && guard.session === now.session &&
        guard.aggregate === aggregate && guard.role === role && equalPair(guard.pin, pair(now.pin));
    }
    const usable = () => projection && projectionGuard && current(projectionGuard);
    function clearDecorations() {
      for (const item of decorations) item.node.remove();
      decorations = [];
      get("metric-legend")?.replaceChildren();
    }
    function clearInspection() {
      inspectionSerial++;
      selected = null;
      window.TrellisShell?.clearMetricInspector();
    }
    function reset() {
      epoch++; refreshSerial++; inspectionSerial++;
      projection = null; projectionGuard = null; loading = false;
      clearDecorations(); clearInspection(); text("metrics-state", "");
      onChange();
    }
    function rows(selection, payload = projection) {
      const candidates = selection?.scope === "file" ? payload?.files : selection?.scope === "directory" ? payload?.directories : [];
      return (candidates || []).filter(row => row.path === selection.path &&
        (selection.language === undefined || row.language === selection.language));
    }
    function match(selection) {
      if (!usable()) return null;
      const candidates = rows(selection);
      // A tree path alone cannot choose a source set or language.
      return candidates.length === 1 ? candidates[0] : null;
    }
    function url(scope, guard, row) {
      const query = new URLSearchParams({scope, aggregate:guard.aggregate, role:guard.role, workbench:"true",
        indexGeneration:guard.pin.indexGeneration, indexRevision:String(guard.pin.indexRevision)});
      if (row) {
        query.set("path", row.path); query.set("sourceSetId", row.sourceSetId);
        if (scope === "file") query.set("language", row.language);
        query.set("topCognitive", "3");
      }
      return `/api/metrics?${query}`;
    }
    function payloadOf(data, guard, scope) {
      const payload = data?.payload;
      if (!pair(payload)) throw new Error("Invalid metrics response.");
      if (!equalPair(pair(payload), guard.pin)) {
        const error = new Error("Indexed snapshot changed. Refresh status.");
        error.code = "revision_conflict"; error.status = 409; throw error;
      }
      if (payload.workbench !== true || payload.metricsVersion !== "metrics:v1" || payload.aggregate !== guard.aggregate || payload.role !== guard.role || payload.scope !== scope ||
          !Array.isArray(payload.files) || !Array.isArray(payload.directories)) throw new Error("Invalid metrics response.");
      return payload;
    }
    function failure(error, id) {
      if (error?.name === "AbortError") return;
      text(id, error?.message || "Metrics unavailable. Try again.");
      if (error?.status === 409 && error.code === "revision_conflict") onConflict(error);
    }
    async function refresh() {
      const serial = ++refreshSerial, guard = capture();
      inspectionSerial++;
      projection = null; projectionGuard = null; clearDecorations();
      loading = !!guard.rootKey && !!guard.pin;
      text("metrics-state", loading ? "Loading indexed snapshot metrics…" : "Indexed snapshot metrics unavailable.");
      if (selected) inspectionState(selected, loading ? "Loading indexed snapshot metrics…" : "Metrics unavailable.");
      onChange();
      if (!loading) return;
      try {
        const data = await request(url("revision", guard));
        if (!current(guard) || serial !== refreshSerial) return;
        projection = payloadOf(data, guard, "revision"); projectionGuard = guard; loading = false;
        text("metrics-state", `Indexed snapshot · ${aggregate} · ${population()} · ${exclusions(projection.files)}${data.catchingUp ? " · index catching up" : ""}${projection.layerExclusionsWarning ? ` · ${projection.layerExclusionsWarning}` : ""}`);
        onChange();
        if (selected) await inspect(selected);
      } catch (error) {
        if (!current(guard) || serial !== refreshSerial) return;
        loading = false;
        if (selected) inspectionState(selected, error?.message || "Metrics unavailable.");
        failure(error, "metrics-state");
      }
    }
    function inspectionState(selection, message) {
      window.TrellisShell?.showMetricInspector();
      text("metric-inspector-title", selection.path || "Indexed workspace");
      text("metric-inspector-scope", `${selection.scope} · ${aggregate} · indexed snapshot · ${population()}`);
      text("metric-inspector-state", message);
      get("metric-inspector-values")?.replaceChildren(); get("metric-inspector-top")?.replaceChildren();
    }
    function renderValues(metrics) {
      const target = get("metric-inspector-values");
      function visit(value, keys) {
        if (value && typeof value === "object" && !Array.isArray(value)) {
          if (Object.hasOwn(value, "unavailable")) {
            target?.append(make("dt", keys.map(key => labels[key] || key).join(" · ")),
              make("dd", `${display(value.value)} — ${value.unavailable}`));
          } else for (const [key, child] of Object.entries(value)) visit(child, [...keys, key]);
        } else {
          target?.append(make("dt", keys.map(key => labels[key] || key).join(" · ")), make("dd", display(value)));
        }
      }
      for (const [key, value] of Object.entries(metrics || {})) visit(value, [key]);
    }
    function renderTop(top) {
      const target = get("metric-inspector-top");
      target?.append(make("li", `Highest cognitive complexity · ${population()} · raw callable scores`));
      if (!top?.length) { target?.append(make("li", `No ${callables()}.`)); return; }
      for (const row of top) {
        const item = make("li");
        item.append(make("strong", row.name ?? "Anonymous function"),
          make("span", ` · ${row.role} · ${row.kind} · ${row.sourceSetId} · ${row.language} · ${row.path} · UTF-8 bytes [${row.range?.start}, ${row.range?.end})`),
          make("span", ` · Cognitive ${display(row.metrics?.cognitive)} · Cyclomatic ${display(row.metrics?.cyclomatic)} · Max nesting ${display(row.metrics?.maxNesting)}`));
        target?.append(item);
      }
    }
    async function inspect(selection) {
      const serial = ++inspectionSerial;
      selected = selection && ["file", "directory"].includes(selection.scope) && typeof selection.path === "string"
        ? {scope:selection.scope, path:selection.path, ...(selection.language === undefined ? {} : {language:selection.language})} : null;
      if (!selected) { clearInspection(); return; }
      const choice = selected, guard = capture(), row = match(choice);
      inspectionState(choice, row ? "Loading indexed snapshot metrics…" : loading ? "Loading indexed snapshot metrics…" : "Metrics unavailable: no unique indexed source identity for this path.");
      if (!row) return;
      try {
        const data = await request(url(choice.scope, guard, row));
        if (!current(guard) || serial !== inspectionSerial) return;
        const payload = payloadOf(data, guard, choice.scope);
        if (!Array.isArray(payload.topCognitive)) throw new Error("Invalid highest-cognitive metrics response.");
        const candidates = rows(choice, payload).filter(candidate => candidate.sourceSetId === row.sourceSetId &&
          (choice.scope !== "file" || candidate.language === row.language));
        if (candidates.length !== 1) throw new Error("Metrics unavailable: selected indexed scope was not returned.");
        const result = candidates[0];
        text("metric-inspector-state", result.declarationCount === 0 ? `No ${callables()}. Numeric aggregates are unavailable, not zero.` :
          `${result.declarationCount} ${callables()}${data.catchingUp ? " · index catching up" : ""}`);
        if (result.excludedReason) text("metric-inspector-state", `Excluded from heat: ${result.excludedReason} · ${exclusions([result])}`);
        renderValues(result.metrics); renderTop(payload.topCognitive);
      } catch (error) {
        if (!current(guard) || serial !== inspectionSerial) return;
        failure(error, "metric-inspector-state");
      }
    }
    // true/false once the indexed snapshot is known; null while unknown.
    function indexedDirectory(path) {
      return usable() ? projection.directories.some(row => row.path === path) : null;
    }
    function decorate(pick, selection) {
      if (metric === "none") return;
      // Tree rendering replaces picks. Re-decoration of the same pick is idempotent.
      const previous = decorations.find(item => item.pick === pick);
      if (previous) { previous.node.remove(); decorations = decorations.filter(item => item !== previous); }
      const row = match(selection), value = metricValue(row?.metrics, metric);
      const node = make("span", undefined, "metric-value");
      const swatch = make("span", undefined, "metric-swatch"); swatch.setAttribute("aria-hidden", "true");
      node.append(swatch, make("span", display(value)));
      node.title = row ? `${metric} · ${aggregate} · ${population()} · indexed snapshot: ${display(value)}` : "Unavailable: no unique indexed metric for this path.";
      pick.append(node);
      const decoration = {pick, selection, node, swatch, value, guard:projectionGuard};
      decorations.push(decoration);
      return decoration;
    }
    // Explorer rows carry swatch + number; each row's tooltip names what it measures and its band.
    function explorerLegend(scale) {
      for (const item of decorations) {
        if (!match(item.selection)) continue;
        const color = scale && item.guard && current(item.guard) ? scale.band(item.value) : null;
        const band = scale?.entries.find(entry => entry.band === color && color !== null)?.label;
        item.node.title = `${colorNames[metric] || metric} · ${aggregate} · ${population()} · indexed snapshot: ${display(item.value)}`
          + (scale ? ` · ${scale.label}${band ? ` · band ${band}` : ""}` : " · No measured values · — unavailable, not zero");
      }
    }
    function renderLegend(rendered) {
      const target = get("metric-legend"); target?.replaceChildren();
      if (metric === "none") { clearDecorations(); return; }
      const visible = rendered.filter(Boolean);
      for (const item of decorations) if (!visible.includes(item)) item.node.remove();
      decorations = visible;
      const values = usable() ? visible.map(item => metricValue(match(item.selection)?.metrics, metric)).filter(finite) : [];
      if (!usable()) {
        for (const item of decorations) item.swatch.className = "metric-swatch neutral";
        target?.append(make("span", `No measured values · ${population()} · — unavailable, not zero`)); explorerLegend(null); return;
      }
      const scale = createScale(metric, aggregate, values, relative);
      for (const item of decorations) {
        const color = item.guard && current(item.guard) ? scale.band(item.value) : null;
        item.swatch.className = `metric-swatch${color === null ? " neutral" : ` heat-${color}`}`;
      }
      target?.append(make("span", `${scale.label}${values.length ? " · Displayed file/directory rows" : ""} · ${aggregate} · ${population()}`));
      for (const {label, band} of scale.entries) {
        const entry = make("span", undefined, "metric-legend-entry");
        entry.append(make("span", undefined, `metric-swatch${band === null ? " neutral" : ` heat-${band}`}`), make("span", label)); target?.append(entry);
      }
      if (relative) target?.append(make("span", "Ties stay together · — unavailable"));
      explorerLegend(scale);
    }
    if (get("metric-relative")) get("metric-relative").checked = false;
    get("metric-relative")?.addEventListener("change", event => {
      relative = event.target.checked; onChange();
    });
    if (get("metric-color")) get("metric-color").value = metric;
    if (get("metric-aggregate")) get("metric-aggregate").value = aggregate;
    if (get("metric-include-tests")) get("metric-include-tests").checked = false;
    get("metric-include-tests")?.addEventListener("change", event => {
      const next = event.target.checked ? "all" : "production";
      if (next === role) return;
      role = next; clearInspection(); void refresh();
    });
    get("metric-color")?.addEventListener("change", event => {
      metric = event.target.value; clearDecorations(); onChange();
    });
    get("metric-aggregate")?.addEventListener("change", event => {
      if (!["max", "sum", "mean"].includes(event.target.value) || aggregate === event.target.value) return;
      aggregate = event.target.value; void refresh();
    });
    return {reset, refresh, inspect, clearInspection, decorate, renderLegend, indexedDirectory};
  }
  window.TrellisMetrics = {create, quartiles, band, metricValue};
})();
