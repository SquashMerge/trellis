"use strict";
// The Architecture inspector's kinds that read no controller state: an edge, a layer, an interface, an app and the
// checkout-wide lists, with the rows and wording they share. Moved whole from architecture.js (the controller),
// which passes what these read and call and keeps `inspect`, the module inspector and the rule inspectors.
(() => {
  const {display, plural} = window.TrellisArchitectureDisplay;
  const labels = {loc:"LOC (callable ranges)", sloc:"SLOC (callable ranges)", commentLines:"Comment lines (callable ranges)",
    cognitive:"Cognitive", cyclomatic:"Cyclomatic", maxNesting:"Max nesting", maintainabilityIndex:"Maintainability (higher is better)",
    fanOut:"Fan-out (call sites)", fanIn:"Fan-in", halstead:"Halstead", parameters:"Parameters", exits:"Exits"};
  function create({make, paragraph, disclosure, targetLabel, apps, layerLabel, appFilter, detailList, pairItems, instanceItems, boundaryCallers, pairDetails, inspectorSource, NO_PAIRS, inspect, overview, action, inspectRule, eligible}) {
    // An edge's pairs and sampled sites are read on demand from the details route's `edge`
    // kind (#917). Boundary callers group every pair loaded so far, above the pair list.
    function edgePairs(target, record, active) {
      const callers = make("div"), loaded = []; target.append(callers);
      // The edge's bounded `key` names it; its reason text never travels in the request.
      detailList(target, {kind:"edge", key:record.key, limit:"500"}, active, (list, items) => {
        loaded.push(...items); callers.replaceChildren(); boundaryCallers(callers, loaded, active); pairDetails(list, items, active);
      }, NO_PAIRS);
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
    // Evidence wording follows what the selection rests on (#523); never semantically complete.
    function evidenceSummary(payload) {
      return payload.evidenceTier === "mixed"
        ? "Syntax evidence and fresh semantic bindings · not semantically complete"
        : "Syntax evidence · not compiler-resolved calls";
    }
    function edgeSites(edge) {
      if (edge.evidenceTier === "semantic") return "reference sites, all bound by semantic evidence";
      if (edge.evidenceTier === "mixed") return `reference sites, ${edge.semanticSites} bound by semantic evidence`;
      return "structural reference sites";
    }
    function ruleEvidence(row) {
      const semantic = (row.evidence || []).concat(...(row.constraints || []).map(c => c.evidence || []))
        .some(item => item.kind === "semanticBinding" || item.kind === "typeRelationship");
      return semantic
        ? "Semantic evidence names a fresh producer binding and its proof; structural spelling and source-owner identities remain syntax evidence."
        : "Captured spelling and source-owner identities are syntax evidence, not destination symbol bindings.";
    }
    const str = value => typeof value === "string" ? value : "";
    // One interface row of a detail list: the route, its confidence, and a source read for the handler and each call site.
    const endpointRows = active => (list, items) => {
      for (const item of items) {
        const entry = item?.entry, row = make("div", undefined, "architecture-rule-row");
        if (!entry || typeof entry !== "object") continue;
        const handler = entry.handler, sites = item.list === "clientOnly" ? [entry.callSite] : Array.isArray(entry.callSites) ? entry.callSites : [];
        const route = `${str(entry.method) || "unresolved method"} ${str(entry.pathTemplate) || "(unresolved path)"}`;
        row.dataset.list = str(item.list);
        const tail = item.list === "matched" ? ` · ${str(entry.confidence)} · ${plural(sites.length, "call site")}` : "";
        paragraph(row, `${route}${tail}`);
        if (handler?.path && handler.lines) {
          paragraph(row, `Handler ${handler.path}:${handler.lines.start}`, "detail");
          inspectorSource(row, {path:handler.path}, {lines:handler.lines}, active, "Read handler source");
        }
        for (const site of sites) if (site?.document?.path) {
          paragraph(row, `Call site ${site.document.path}:${site.lines?.start}`, "detail");
          inspectorSource(row, {path:site.document.path}, site, active, "Read call-site source");
        }
        list.append(row);
      }
    };
    // The interface inspector (AC15): its ends, counts and wrappers, then the matched routes and the one-sided routes and calls.
    function inspectInterface(target, title, entry, active) {
      const facts = apps.interfaceFacts(entry);
      title.textContent = facts.title;
      paragraph(target, facts.basis, "detail");
      for (const line of facts.ends) paragraph(target, line);
      const grid = make("dl", undefined, "metric-inspector-values");
      for (const [name, value, tone] of facts.rows) { const item = make("dd", value); if (tone) item.dataset.tone = tone; grid.append(make("dt", name), item); }
      target.append(grid);
      if (facts.wrappers) paragraph(target, facts.wrappers, "detail architecture-mono");
      const params = list => ({kind:"interfaces", client:entry.client, server:entry.server, list});
      // The matched routes load up front, under their heading, before the one-sided lists (#950; owner ruling 2026-11-12).
      paragraph(target, facts.matchedHeading, "detail");
      const matched = make("div", undefined, "architecture-endpoints"); target.append(matched);
      detailList(matched, params("matched"), active, endpointRows(active), "No matched routes at this pin.");
      paragraph(target, "◆ Server-only routes: no call matched them", "detail");
      detailList(target, params("serverOnly"), active, endpointRows(active), "No server-only routes.");
      paragraph(target, "◆ Client-only calls: no route matched them. This does not prove absence.", "detail");
      detailList(target, params("clientOnly"), active, endpointRows(active), "No client-only calls.");
    }
    // The app inspector: what was detected, how much of it has layers, and the interfaces it takes part in.
    function inspectApp(target, title, group, payload, active) {
      const facts = apps.appFacts(group, payload);
      title.textContent = facts.title;
      paragraph(target, facts.detail, "detail");
      const grid = make("dl", undefined, "metric-inspector-values");
      for (const [name, value, tone] of facts.rows) { const item = make("dd", value); if (tone) item.dataset.tone = tone; grid.append(make("dt", name), item); }
      target.append(grid);
      if (facts.origin) paragraph(target, facts.origin, "detail");
      if (facts.duplicate) paragraph(target, facts.duplicate, "architecture-app-notice");
      if (!facts.layered) paragraph(target, `${group.name} has no layers yet. The trellis_propose_rules MCP prompt can propose layers per app; this browser does not call it. Review .trellis/rules.toml to add layers with app = "${group.name}".`);
      for (const entry of facts.interfaces) {
        const button = make("button", `HTTP: ${entry.client} → ${entry.server}`); button.type = "button";
        button.addEventListener("click", () => { if (active()) inspect("interface", entry); }); target.append(button);
      }
    }
    function inspectEdge({target, title, record, active}) {
      title.textContent = `${record.sourceLayer} → ${record.destinationLayer}`;
      paragraph(target, `${record.count} exact ${record.role} ${edgeSites(record)} · ${record.status}${record.excepted ? ` · excepted: ${record.exceptionReason}` : " · not excepted"}${record.role === "test" ? " · hypothetical policy only; never a production violation" : ""}.`);
      edgePairs(target, record, active);
    }
    function inspectLayer({target, title, record, payload, references, active}) {
      title.textContent = record.name; paragraph(target, layerLabel(record));
      for (const module of payload.modules.filter(item => item.layer === record.name)) {
        const button = make("button", module.identity.path); button.type = "button"; button.addEventListener("click", () => { if (active()) inspect("module", module); }); target.append(button);
      }
      const packages = (references.externalPackages || []).filter(item => item.layer === record.name);
      if (packages.length) disclosure(target, "External packages (no native source or metrics)", packages);
      paragraph(target, "Reference pairs from or to this layer", "detail");
      detailList(target, {kind:"pairs", layer:record.name}, active, pairItems(active), NO_PAIRS);
      paragraph(target, "Layer policy and exceptions", "detail");
      detailList(target, appFilter({kind:"instances", layer:record.name}), active, instanceItems(active), "No rule instances name this layer.");
    }
    // An interface or an app that the payload no longer has returns to the overview; `false` tells `inspect` to stop there.
    function inspectFound({kind, target, title, record, payload, active}) {
      const found = kind === "interface" ? apps.link(payload, record?.client, record?.server) : apps.group(payload, record?.name);
      if (!found) { overview(); return false; }
      if (kind === "interface") inspectInterface(target, title, found, active); else inspectApp(target, title, found, payload, active);
    }
    function inspectUnresolved({target, title, references, active}) {
      title.textContent = "Unresolved structural references";
      paragraph(target, `${references.counts?.unresolvedSites ?? 0} unresolved sites.`);
      detailList(target, {kind:"unresolved"}, active, (list, items) => {
        for (const item of items) {
          const entry = make("details"); entry.append(make("summary", `${item.source.path} · ${item.reason}`), make("pre", JSON.stringify(item, null, 2)));
          inspectorSource(entry, item.source, item.site, active); list.append(entry);
        }
      }, "No unresolved sites.");
    }
    function inspectExternal({target, title, active}) {
      title.textContent = "External policy results";
      paragraph(target, "Production and test-only $external results.", "detail");
      detailList(target, {kind:"instances", rule:"$external"}, active, instanceItems(active), "No external policy results.");
    }
    function inspectUnlayered({target, title, payload, active}) {
      title.textContent = "Same-layer and unassigned references";
      detailList(target, {kind:"pairs", unlayered:"true"}, active, pairItems(active), NO_PAIRS);
      disclosure(target, "Unassigned native file modules", payload.modules.filter(module => module.layer === null));
    }
    function renderInstances(target, rows, active, emptyNote = true) {
      for (const row of rows) {
        const r = row.instance, entry = make("div", undefined, "architecture-rule-row");
        entry.dataset.rule = r.rule; entry.dataset.source = r.target?.path || ""; entry.dataset.destination = r.destination?.path || "";
        const label = `${targetLabel(r.target)} → ${r.destination ? targetLabel(r.destination) : "Unknown destination"}`;
        action(entry, `${r.rule} · ${label}`, () => inspectRule(row), active);
        paragraph(entry, `${row.role} · ${r.sourceLayer ?? "Unassigned"} → ${r.destinationLayer ?? "Unknown"} · ${r.status} · ${r.excepted ? `excepted: ${r.exceptionReason}` : "not excepted"} · ${r.referenceCount ?? 0} exact reference sites`);
        if (!eligible(row)) paragraph(entry, r.excepted ? "Already excepted. Edit or remove the authored entry." :
          "New exceptions require a positively witnessed production $layers deviation between native files; unknown, external and test-only rows cannot be waived.", "detail");
        target.append(entry);
      }
      if (!rows.length && emptyNote) paragraph(target, "No rule instances match this review scope.");
    }
    return {values, evidenceSummary, ruleEvidence, inspectEdge, inspectLayer, inspectFound, inspectUnresolved, inspectExternal, inspectUnlayered, renderInstances};
  }
  window.TrellisArchitectureInspector = {create};
})();
