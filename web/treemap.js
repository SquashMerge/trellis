"use strict";
// Revision-owned file metrics. Scope and visibility never change the area basis.
(() => {
  const get = id => document.getElementById(id);
  const make = (tag, text, cls) => {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (cls) node.className = cls;
    return node;
  };
  const identity = file => JSON.stringify([file.sourceSetId, file.language, file.path]);
  const finite = value => typeof value === "number" && Number.isFinite(value);
  const display = value => finite(value) ? String(value) : "—";
  const {metricValue, quartiles, relativeBand:band, createScale, exclusions} = window.TrellisMetricHeat;
  const classification = file => file.roleCounts.production > 0 ? (file.roleCounts.test > 0 ? "mixed" : "production") : file.roleCounts.test > 0 ? "test" : "none";
  const testFile = file => file.fileRole === "test" || classification(file) === "test";
  const count = value => Number.isSafeInteger(value) && value >= 0;
  const metrics = new Set(["role", "cognitive", "cyclomatic", "maxNesting", "maintainabilityIndex", "halstead.volume", "fanOut"]);
  function create({request, context, readSource, onConflict = () => {}, onInspect = () => {}}) {
    let serial = 0, renderSerial = 0, selectionSerial = 0, navigationSerial = 0, view = null, basis = null, selectedKey = null;
    let metric = "cognitive", aggregate = "max", role = "production", relative = false, hideTests = true, path = "", language = "", scope = null;
    let layoutSerial = 0, relayout = null, resizeTimer = null;
    const population = () => role === "all" ? "all indexed callables" : "production callables";
    const state = (message, kind = "ready") => { get("treemap-state").textContent = message; get("treemap-state").dataset.state = kind; };
    const paragraph = (target, message) => target.append(make("p", message));
    function capture() {
      const c = context();
      return {rootKey:c.rootKey, session:c.session, pin:c.pin && window.TrellisIndexPin.copy(c.pin), workspaceRoot:c.workspaceRoot, aggregate, role, serial};
    }
    function current(c) {
      const now = context();
      return !!c && c.serial === serial && c.aggregate === aggregate && c.role === role && c.rootKey === now.rootKey && c.session === now.session &&
        !!c.pin && window.TrellisIndexPin.equal(c.pin, now.pin);
    }
    const usable = () => !!view && current(basis);
    // The observed checkout directory names the root, and only when one source set owns every indexed file.
    function rootLabel(sourceSetId) {
      const name = typeof basis?.workspaceRoot === "string" ? basis.workspaceRoot.split("/").filter(Boolean).at(-1) : "";
      return name && view && view.files.every(file => file.sourceSetId === sourceSetId) ? name : sourceSetId;
    }
    // The diagram box's own content size, less Pele's 8 px padding on each side; unknown while hidden.
    function viewport() {
      const node = get("treemap-diagram");
      if (!node?.offsetWidth || !node.offsetHeight || typeof getComputedStyle !== "function") return undefined;
      const style = getComputedStyle(node), px = (...names) => names.reduce((sum, name) => sum + (parseFloat(style[name]) || 0), 0);
      return {width:node.offsetWidth - px("paddingLeft", "paddingRight", "borderLeftWidth", "borderRightWidth") - 16,
        height:node.offsetHeight - px("paddingTop", "paddingBottom", "borderTopWidth", "borderBottomWidth") - 16};
    }
    function markSelection() {
      window.TrellisTreemapPele?.select?.(get("treemap-diagram"), selectedKey);
      for (const node of get("treemap-files").querySelectorAll("[data-trellis-file-id]")) {
        node.setAttribute("aria-pressed", String(node.getAttribute("data-trellis-file-id") === selectedKey));
      }
    }
    function clearInspection() {
      selectionSerial++; selectedKey = null;
      window.TrellisShell?.clearTreemapInspector(); markSelection();
    }
    function reset(message = "Open Treemap to read the indexed snapshot.") {
      serial++; renderSerial++; relayout = null; clearInspection(); view = null; basis = null;
      for (const id of ["treemap-diagram", "treemap-files", "treemap-legend", "treemap-breadcrumbs"]) get(id).replaceChildren();
      get("treemap-diagram").setAttribute("aria-busy", "false"); state(message, "empty");
    }
    function payloadOf(data, c) {
      const payload = data?.payload;
      if (!payload || !window.TrellisIndexPin.equal(payload, c.pin)) {
        throw Object.assign(new Error("Indexed snapshot changed. Refresh status."), {code:"revision_conflict", status:409});
      }
      if (payload.scope !== "revision" || payload.workbench !== true || payload.metricsVersion !== "metrics:v1" || payload.aggregate !== c.aggregate || payload.role !== c.role ||
          !Array.isArray(payload.files) || !Array.isArray(payload.directories)) throw new Error("Invalid Treemap metrics response. Nothing displayed.");
      const keys = new Set();
      for (const file of payload.files) {
        if (!file || ![file.sourceSetId, file.language, file.path].every(value => typeof value === "string" && value.length) ||
            !count(file.physicalLines) || !["production", "test"].includes(file.fileRole) || !count(file.declarationCount) ||
            !count(file.roleCounts?.production) || !count(file.roleCounts?.test) || !file.metrics || typeof file.metrics !== "object" || keys.has(identity(file))) {
          throw new Error("Invalid Treemap file metadata. Nothing displayed.");
        }
        keys.add(identity(file));
      }
      return payload;
    }
    function included(file) {
      return (!hideTests || !testFile(file)) && (!language || file.language === language) && (!path || file.path.includes(path)) &&
        (!scope || (file.sourceSetId === scope.sourceSetId && (!scope.path || file.path.startsWith(scope.path + "/"))));
    }
    function visibleFiles() {
      return view.files.filter(included).sort((a, b) => identity(a) < identity(b) ? -1 : identity(a) > identity(b) ? 1 : 0);
    }
    function breadcrumbs(active) {
      const target = get("treemap-breadcrumbs"); target.replaceChildren();
      const crumb = (label, next) => {
        const button = make("button", label); button.type = "button";
        button.addEventListener("click", () => { if (!active()) return; navigationSerial++; scope = next; return render(); }); target.append(button);
      };
      crumb("All indexed files", null);
      if (!scope) return;
      crumb(rootLabel(scope.sourceSetId), {sourceSetId:scope.sourceSetId, path:""});
      const parts = scope.path ? scope.path.split("/") : [];
      parts.forEach((part, index) => crumb(part, {sourceSetId:scope.sourceSetId, path:parts.slice(0, index + 1).join("/")}));
    }
    function legend(scale) {
      const target = get("treemap-legend"); target.replaceChildren();
      if (metric === "role") { paragraph(target, "Prod/test · production-only / mixed / test-only / no measured callables. Callable populations, not test-runner discovery. Stored test-file roles are also hidden by Hide test files. Test-only files have dashed outlines."); return; }
      paragraph(target, `${scale.label} · ${metric} · ${aggregate} · visible files · ${population()}`);
      for (const {label, band} of scale.entries) {
        const entry = make("span", undefined, "metric-legend-entry");
        entry.append(make("span", undefined, `metric-swatch${band === null ? " neutral" : ` heat-${band}`}`), make("span", label)); target.append(entry);
      }
      if (relative) target.append(make("span", "Low heat → high heat · ties stay together · — unavailable"));
    }
    function inspect(file, active) {
      if (!active()) return;
      clearInspection(); selectedKey = identity(file); const ticket = selectionSerial, c = basis;
      const selectionCurrent = () => active() && usable() && ticket === selectionSerial && selectedKey === identity(file) && !get("treemap-inspector").hidden;
      onInspect(); window.TrellisShell?.showTreemapInspector(); markSelection();
      get("treemap-inspector-title").textContent = file.path;
      const target = get("treemap-inspector-content"); target.replaceChildren();
      paragraph(target, `${file.sourceSetId} · ${file.language} · indexed snapshot ${window.TrellisIndexPin.label(c.pin)}`);
      paragraph(target, `Area: ${file.physicalLines} physical file lines · includes inline test lines. Callable-range LOC may overlap; it is not physical file lines.`);
      const labels = {production:"production-only", mixed:"mixed", test:"test-only", none:"no measured callables"};
      paragraph(target, `${labels[classification(file)]} callable population · stored file role: ${file.fileRole} · roleCounts: ${file.roleCounts.production} production / ${file.roleCounts.test} test.`);
      paragraph(target, `${file.declarationCount} selected ${population()} · ${aggregate} · metrics:v1. Classification is syntax-based, not test-runner discovery or complete conditional-compilation analysis.`);
      if (file.excludedReason) paragraph(target, `Excluded from heat: ${file.excludedReason} · ${exclusions([file])}`);
      else if (!file.declarationCount) paragraph(target, `No ${population()}. Numeric aggregates are unavailable, not zero.`);
      const values = make("dl", undefined, "metric-inspector-values");
      function visit(value, name) {
        if (value && typeof value === "object") {
          if (Object.hasOwn(value, "unavailable")) values.append(make("dt", name), make("dd", `${display(value.value)} · ${value.unavailable}`));
          else for (const [key, child] of Object.entries(value)) visit(child, name ? `${name} · ${key}` : key);
        } else values.append(make("dt", name), make("dd", display(value)));
      }
      visit(file.metrics, ""); target.append(values);
      const exact = make("details"); exact.append(make("summary", "Exact indexed file identity"), make("pre", JSON.stringify({sourceSetId:file.sourceSetId, language:file.language, path:file.path}, null, 2))); target.append(exact);
      if (file.physicalLines > 0) {
        const button = make("button", "Read indexed file source"); button.type = "button";
        button.addEventListener("click", () => {
          if (selectionCurrent()) return readSource({sourceSetId:file.sourceSetId, language:file.language, path:file.path, range:{startLine:1, endLine:file.physicalLines}}, c.pin, selectionCurrent);
        }); target.append(button);
      } else paragraph(target, "Empty indexed file · zero area; no source lines to read.");
    }
    async function render(select = null) {
      if (!usable()) return;
      const payload = view, c = basis, ticket = ++renderSerial;
      let ready = false; relayout = null;
      const active = () => current(c) && view === payload && ticket === renderSerial;
      const interactive = () => active() && ready;
      clearInspection(); get("treemap-diagram").replaceChildren(); get("treemap-files").replaceChildren();
      get("treemap-diagram").setAttribute("aria-busy", "true"); state("Rendering indexed files…", "loading");
      const files = visibleFiles(), values = files.map(file => metricValue(file.metrics, metric));
      const scale = createScale(metric, aggregate, values, relative);
      breadcrumbs(active); legend(scale);
      try {
        if (files.some(file => file.physicalLines > 0)) {
          if (!window.TrellisTreemapPele) throw new Error("Treemap renderer unavailable. Nothing displayed.");
          const paint = (selectedKey, isCurrent) => window.TrellisTreemapPele.render(get("treemap-diagram"), files, {
            metric, aggregate, selectedKey, isCurrent, viewport:viewport(), rootLabel,
            fileValue:file => metric === "role" ? undefined : metricValue(file.metrics, metric),
            fileBand:file => metric === "role" ? classification(file) : scale.band(metricValue(file.metrics, metric)),
            onSelectFile:file => { if (interactive() && files.includes(file)) inspect(file, interactive); },
            onSelectDirectory:directory => {
              if (!interactive() || !directory || !files.some(file => file.sourceSetId === directory.sourceSetId && (!directory.path || file.path.startsWith(directory.path + "/")))) return;
              navigationSerial++; scope = {sourceSetId:directory.sourceSetId, path:directory.path}; return render();
            },
          });
          const outcome = await paint(select, active);
          if (!active()) return;
          if (outcome?.status !== "ready") throw new Error(outcome?.reason || outcome?.error?.message || "Treemap mapping failed. Nothing displayed.");
          // Resizes relayout the same admitted files; only a changed layout size renders again.
          let laidOut = outcome.projection;
          const area = files.filter(file => file.physicalLines > 0).length;
          relayout = async () => {
            const next = interactive() && laidOut ? viewport() : undefined;
            if (!next) return;
            const size = window.TrellisTreemapPele.layoutSize(area, next);
            if (size.width === laidOut.width && size.height === laidOut.height) return;
            const layout = ++layoutSerial, latest = () => interactive() && layout === layoutSerial;
            const result = await paint(selectedKey, latest);
            if (!latest()) return;
            if (result?.status === "ready") { laidOut = result.projection; markSelection(); return; }
            ready = false; relayout = null; clearInspection();
            for (const id of ["treemap-files", "treemap-legend"]) get(id).replaceChildren();
            state(result?.error?.message || "Treemap rendering failed. Nothing displayed.", "error");
          };
        }
        if (!active()) return;
        ready = true;
        for (const file of files) {
          const button = make("button", `${file.path} · ${file.sourceSetId} · ${file.language} · ${file.physicalLines} physical lines${file.physicalLines === 0 ? " · zero area" : ""}`);
          button.type = "button"; button.setAttribute("data-trellis-file-id", identity(file)); button.setAttribute("aria-pressed", "false");
          button.addEventListener("click", () => inspect(file, interactive)); get("treemap-files").append(button);
        }
        const total = files.reduce((sum, file) => sum + file.physicalLines, 0);
        const message = files.length ? `${files.length} of ${payload.files.length} indexed files · ${total} physical file lines · ${aggregate} · ${population()} · test files ${hideTests ? "hidden" : "shown"} · ${exclusions(payload.files)}.` : "No indexed files match this scope and filters. Change the filters or use All indexed files.";
        state(`${message}${payload.layerExclusionsWarning ? ` · ${payload.layerExclusionsWarning}` : ""}`, files.length ? "ready" : "empty");
        if (select) { const file = files.find(item => identity(item) === select); if (file) inspect(file, interactive); }
      } catch (error) {
        if (!active()) return;
        ready = false; clearInspection();
        for (const id of ["treemap-diagram", "treemap-files", "treemap-legend"]) get(id).replaceChildren();
        state(error.message || "Treemap rendering failed. Nothing displayed.", "error");
      } finally { if (active()) get("treemap-diagram").setAttribute("aria-busy", "false"); }
    }
    async function refresh() {
      reset(); const c = capture();
      if (!c.rootKey || !c.pin) { state("Choose an indexed checkout, then open Treemap.", "empty"); return; }
      state("Reading indexed file metrics…", "loading"); get("treemap-diagram").setAttribute("aria-busy", "true");
      try {
        const query = new URLSearchParams({scope:"revision", aggregate:c.aggregate, role:c.role, workbench:"true", indexGeneration:c.pin.indexGeneration, indexRevision:String(c.pin.indexRevision)});
        const data = await request(`/api/metrics?${query}`, () => current(c));
        if (!current(c)) return;
        view = payloadOf(data, c); basis = c;
        const choices = get("treemap-language"); choices.replaceChildren();
        const option = make("option", "All languages"); option.value = ""; choices.append(option);
        for (const value of [...new Set(view.files.map(file => file.language))].sort()) { const option = make("option", value); option.value = value; choices.append(option); }
        choices.value = language;
        await render();
      } catch (error) {
        if (!current(c) || error.name === "AbortError") return;
        state(error.message || "Treemap unavailable. Refresh to try again.", "error");
        if (error.status === 409 && ["revision_conflict", "pin_expired", "root_changed"].includes(error.code)) onConflict(error);
      } finally { if (current(c) && !view) get("treemap-diagram").setAttribute("aria-busy", "false"); }
    }
    async function openFile(file, pin) {
      if (!file || ![file.sourceSetId, file.language, file.path].every(value => typeof value === "string" && value.length) || !window.TrellisIndexPin.equal(pin, context().pin)) return false;
      const navigation = ++navigationSerial, loading = refresh(), c = capture(); await loading;
      if (navigation !== navigationSerial || !current(c) || !usable() || !window.TrellisIndexPin.equal(pin, basis.pin)) return false;
      const selected = view.files.find(item => identity(item) === identity(file));
      if (!selected) { state("Requested indexed file identity was not returned. Nothing selected.", "error"); return false; }
      path = ""; language = ""; get("treemap-path").value = ""; get("treemap-language").value = "";
      scope = {sourceSetId:selected.sourceSetId, path:selected.path.includes("/") ? selected.path.slice(0, selected.path.lastIndexOf("/")) : ""};
      if (testFile(selected)) { hideTests = false; get("treemap-hide-tests").checked = false; }
      await render(identity(selected)); return navigation === navigationSerial && current(c) && selectedKey === identity(selected);
    }
    if (typeof ResizeObserver === "function") {
      // One trailing relayout per burst of resizes; a newer one supersedes an older one.
      new ResizeObserver(() => { clearTimeout(resizeTimer); resizeTimer = setTimeout(() => relayout?.(), 120); }).observe(get("treemap-diagram"));
    }
    get("treemap-relative").checked = false;
    get("treemap-relative").addEventListener("change", event => { relative = event.target.checked; return render(); });
    get("treemap-color").value = metric; get("treemap-aggregate").value = aggregate;
    get("treemap-metric-tests").checked = false; get("treemap-hide-tests").checked = true;
    get("treemap-path").value = ""; get("treemap-language").value = "";
    get("treemap-refresh").addEventListener("click", refresh);
    get("treemap-color").addEventListener("change", event => { if (!metrics.has(event.target.value)) return; navigationSerial++; metric = event.target.value; return render(); });
    get("treemap-hide-tests").addEventListener("change", event => { navigationSerial++; hideTests = event.target.checked; return render(); });
    get("treemap-path").addEventListener("input", event => { navigationSerial++; path = event.target.value; return render(); });
    get("treemap-language").addEventListener("change", event => { navigationSerial++; language = event.target.value; return render(); });
    get("treemap-aggregate").addEventListener("change", event => {
      if (!["max", "sum", "mean"].includes(event.target.value) || event.target.value === aggregate) return;
      aggregate = event.target.value; return refresh();
    });
    get("treemap-metric-tests").addEventListener("change", event => {
      const next = event.target.checked ? "all" : "production";
      if (next === role) return; role = next; return refresh();
    });
    return {refresh, reset, clearInspection, openFile};
  }
  window.TrellisTreemap = {create, identity, quartiles, band, metricValue, classification, testFile};
})();
