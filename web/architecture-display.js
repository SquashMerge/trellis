"use strict";
// The Architecture summary display and the module helpers. Pure and DOM-free: the controller is
// architecture.js, which reads them from here, and the Herdr mod bundles this file alone.
(() => {
  const finite = value => typeof value === "number" && Number.isFinite(value);
  const display = value => finite(value) ? String(value) : "—";
  const external = identity => identity.kind === "externalPackage";
  const moduleKey = identity => external(identity)
    ? JSON.stringify([identity.kind, identity.sourceSetId, identity.language, identity.package, identity.standardLibrary])
    : JSON.stringify([identity.sourceSetId, identity.language, identity.path, identity.sid]);
  const {metricValue, relativeBand:band, createScale} = window.TrellisMetricHeat;
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
  const plural = (count, word) => `${count} ${word}${count === 1 ? "" : "s"}`;
  // ---- The summary display (#391, #449, #790), shared with the Herdr mod (#886) ----
  // Everything the drawing derives from a payload and the display choices: band labels and stat chips, the
  // module chips and their budget, the per-app parts of All apps and the heat scale. Pure and DOM-free;
  // `state` supplies the live choices as getters: metric, aggregate, role, relative, showTests, view (the
  // snapshot), model (the drawn slice), shown ({open, drilled, unassigned, hidden}), endMark and scopeGroup.
  function createDisplay(state) {
    const apps = () => window.TrellisArchitectureApps;
    const callables = () => state.role() === "all" ? "indexed callables" : "production callables";
    const same = (a, b) => moduleKey(a) === moduleKey(b);
    function layerOf(identity) {
      const view = state.view();
      if (!external(identity)) return view.modules.find(module => same(module.identity, identity))?.layer;
      return view.references?.externalPackages?.find(item => same(item.identity, identity))?.layer ?? null;
    }
    // Layer numbers count only the layers on show (#913): a hidden layer (an authored `display = "hidden"`,
    // or an unhinted tests/fixtures layer while tests are left out) takes none, so the first band shown is 1.
    // Peers share a number, and each app's stack counts from 1, as layer order is compared within an app.
    let numbering = null;
    function layerNumber(layer) {
      if (layer.shared) return null;
      const source = state.model(), role = state.role();
      if (numbering?.source !== source || numbering.role !== role) {
        const hidden = new Set(summary(source).hidden.map(item => item.name)), orders = new Map();
        for (const item of source.layers) {
          if (item.shared || hidden.has(item.name)) continue;
          const app = item.app ?? null;
          if (!orders.has(app)) orders.set(app, new Set());
          orders.get(app).add(item.order);
        }
        const sorted = new Map([...orders].map(([app, set]) => [app, [...set].sort((a, b) => a - b)]));
        numbering = {source, role, hidden, sorted};
      }
      if (numbering.hidden.has(layer.name)) return null;
      const index = numbering.sorted.get(layer.app ?? null)?.indexOf(layer.order) ?? -1;
      return index < 0 ? null : index + 1;
    }
    // A band's tag: its number, "Shared", or "Hidden" for a hidden layer the reader chose to show.
    const layerTag = layer => layer.shared ? "Shared" : layerNumber(layer) ?? "Hidden";
    function layerLabel(layer) {
      const metric = state.metric(), endMark = state.endMark();
      const modules = state.model().modules.filter(module => module.layer === layer.name);
      const values = modules.map(module => metricValue(module.metrics, metric)).filter(finite);
      const packages = (state.view().references?.externalPackages || []).filter(item => layerOf(item.identity) === layer.name);
      const max = values.length ? String(values.reduce((most, value) => Math.max(most, value), -Infinity)) : "—";
      // The narrow shared box stacks its header; ordered bands keep one line.
      const parts = [`${layerTag(layer)} · ${layer.name}`, `${plural(modules.length, "file")}${packages.length ? ` · ${packages.length} external packages` : ""}`];
      if (metric !== "layer") parts.push(`max ${max}`);
      if (endMark?.layer === layer.name) parts.push(`${endMark.side} end`);
      return parts.join(layer.shared ? "\n" : " · ");
    }
    // Outgoing production reference sites by verdict: the same summary edges the arrows draw.
    function layerCounts(layer, source = state.model()) {
      const byName = new Map(source.layers.map(item => [item.name, item]));
      const counts = {up:0, cross:0, down:0, shared:0, allowed:0, excepted:0, unknown:0}, upTo = new Map();
      for (const edge of source.edges) {
        if (edge.sourceLayer !== layer.name || edge.role !== "production") continue;
        const destination = byName.get(edge.destinationLayer), ordered = !layer.shared && destination && !destination.shared;
        if (edge.excepted) counts.excepted += edge.count;
        else if (edge.status === "deviates") {
          const up = ordered && destination.order < layer.order;
          counts[up ? "up" : "cross"] += edge.count;
          if (up) upTo.set(destination.name, (upTo.get(destination.name) || 0) + edge.count);
        }
        else if (edge.status !== "conforms") counts.unknown += edge.count;
        else if (destination?.shared) counts.shared += edge.count;
        else counts[ordered && destination.order > layer.order ? "down" : "allowed"] += edge.count;
      }
      return {...counts, upTo};
    }
    // Only non-zero counts become chips; deviations carry the error tone and a glyph. Upward deviations
    // get one chip per destination layer, each a control that opens the Deviations tree on that edge
    // (AC10), in every stack.
    // `source` is the slice the band is drawn from: a column's chips sum that app's own edges.
    function layerStats(layer, appName = state.scopeGroup()?.name, source = state.model()) {
      const counts = layerCounts(layer, source), bound = lowerBound(layer.name, "production") ? "≥" : "";
      const stats = STATS.filter(([key]) => key !== "up" && counts[key] > 0).map(([key, glyph, words, error]) =>
        ({key, count:counts[key], text:`${glyph}${bound}${counts[key]}`, label:`${bound ? "at least " : ""}${counts[key]} outgoing production reference sites · ${words}`, tone:error ? "error" : "neutral"}));
      // The count is reference sites, the tree's unit after #846: `↑12 references · entry` (owner ruling 2026-11-09).
      const up = [...counts.upTo].map(([destination, count]) => ({key:"up", count, text:`↑${bound}${count} ${count === 1 ? "reference" : "references"} · ${destination}`, tone:"error",
        label:`${bound ? "at least " : ""}${plural(count, "upward production reference")} from ${layer.name} to ${destination} · deviation · open in Deviations`,
        id:JSON.stringify(["up", layer.name, destination]), action:{sourceLayer:layer.name, destinationLayer:destination}}));
      return [...up, ...stats, ...interfaceStats(layer, appName)];
    }
    // The stubs of a one-app stack: this layer's end of an HTTP interface of the scoped app (AC15). The
    // action carries `interface`, which routes the control to the interface inspector.
    function interfaceStats(layer, appName) {
      return appName === undefined ? [] : apps().stubs(state.model(), appName, layer.name).map(stub => {
        const count = `${stub.atLeast ? "≥" : ""}${plural(stub.routes, "route")}`, client = stub.side === "client";
        const id = JSON.stringify(["interface", stub.client, stub.server, stub.side, layer.name]);
        return {key:"interface", tone:"neutral", tag:"Selected", id,
          text:client ? `→ ${stub.server} · ${count} · ${plural(stub.callSites, "call site")}` : `← ${stub.client} · ${count}`,
          label:client ? `HTTP interface to ${stub.server}: ${count}, ${plural(stub.callSites, "call site")}` : `HTTP interface from ${stub.client}: ${count}`,
          action:{id, interface:{client:stub.client, server:stub.server}, side:stub.side, layer:layer.name}};
      });
    }
    // Authored rules.toml presentation hint (#391 ruling); absent leaves the choice to the browser.
    const hint = layer => ["expanded", "collapsed", "hidden"].includes(layer.display) ? layer.display : null;
    // Bands open on module chips unless the authored hint asks for a collapsed summary.
    // #391 ruling: an absent hint starts collapsed like "collapsed"; "expanded" opens on module chips.
    const expanded = layer => state.shown().open.has(layer.name) ? state.shown().open.get(layer.name) : hint(layer) === "expanded";
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
      return (state.view()?.references?.coverage || []).find(row => row.layer === layerName && row.role === siteRole) || null;
    }
    const percent = (part, whole) => {
      if (!whole) return "—";
      const value = part / whole * 100;
      return part > 0 && value < 1 ? "<1%" : part < whole && value > 99 ? ">99%" : `${Math.round(value)}%`;
    };
    // A count is a lower bound when any site of its source layer and role stayed unresolved.
    const lowerBound = (layerName, siteRole) => (coverageOf(layerName, siteRole)?.unresolvedSites ?? 1) > 0;
    // An arrow's own coverage says whether its count is exact; it loses the "≥" when no site
    // of its source layer and role stayed unresolved (#523).
    const edgeTier = edge => ({semantic:"; semantic bindings", mixed:"; syntax and semantic bindings"})[edge.evidenceTier] || "";
    function edgeText(edge) {
      const covered = edge.coverage || coverageOf(edge.sourceLayer, edge.role);
      const bound = edge.coverage ? edge.coverage.complete !== true : lowerBound(edge.sourceLayer, edge.role);
      if (!covered) return {bound};
      const share = percent(covered.mappedSites, covered.sites);
      const label = `${bound ? "lower bound; " : ""}${share} of ${covered.sites} ${edge.role} reference sites from ${edge.sourceLayer} mapped to a module${edgeTier(edge)}`;
      return {bound, label, note:bound && ["deviation", "unknown"].includes(edgeTone(edge)) ? `${share} mapped` : ""};
    }
    function summary(payload) {
      const production = payload.edges.filter(edge => edge.role === "production");
      // Unhinted test and fixture layers join the hidden footer until tests and fixtures are included;
      // an authored display hint always wins. Every other layer is drawn: the full stack (#449).
      const tests = new Set(state.role() === "production" ? payload.layers.filter(layer => hint(layer) === null && testLayer(layer, payload.modules)).map(layer => layer.name) : []);
      const hidden = payload.layers.filter(layer => hint(layer) === "hidden" || tests.has(layer.name));
      const layers = new Set(payload.layers.filter(layer => !hidden.includes(layer) || state.shown().hidden).map(layer => layer.name));
      const hiddenEdges = production.filter(edge => hidden.some(layer => layer.name === edge.sourceLayer || layer.name === edge.destinationLayer)).length;
      return {hidden, tests, hiddenEdges, layers};
    }
    function worst(modules) {
      const metric = state.metric();
      if (metric === "layer") return undefined;
      const values = modules.map(module => metricValue(module.metrics, metric)).filter(finite);
      if (!values.length) return null;
      return metric === "maintainabilityIndex" ? Math.min(...values) : Math.max(...values);
    }
    // One chip per module; a click opens it one level, down to file chips.
    function level(layerKey, base, members, scale) {
      const metric = state.metric();
      return groupModules(members, base).map(([name, files]) => {
        files.sort((a, b) => a.identity.path.localeCompare(b.identity.path));
        if (files.length === 1) return {type:"module", module:files[0], name};
        const prefix = `${base}${name}`, key = JSON.stringify([layerKey, prefix]), open = state.shown().drilled.has(key), value = worst(files);
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
    // A band's module groups, hottest first: what a collapsed band's chips summarize.
    function moduleGroups(modules) {
      const metric = state.metric(), measured = modules.filter(module => module.declarationCount > 0);
      const groups = groupModules(measured, commonDirectory(modules.map(module => module.identity.path))).map(([name, files]) => ({name, files, value:worst(files)}));
      const severity = value => !finite(value) ? -Infinity : metric === "maintainabilityIndex" ? -value : value;
      if (metric !== "layer") groups.sort((a, b) => severity(b.value) - severity(a.value) || a.name.localeCompare(b.name));
      return groups;
    }
    // A collapsed band's one row of module chips, hottest first (#449). Display only: the
    // band header opens it. The last chip says how many modules the row leaves out.
    function summaryChips(layerKey, modules, columns, scale, budget) {
      const metric = state.metric(), groups = moduleGroups(modules);
      const room = Math.min(columns, ENTRY_CAP, budget.left), kept = groups.length > room ? groups.slice(0, Math.max(Math.min(1, room), room - 1)) : groups;
      budget.left -= kept.length;
      const chips = kept.map(({name, files, value}) => {
        const amount = value === undefined ? "" : `${display(value)}`;
        return {type:"chip", layerKey, group:name, band:value === undefined ? null : scale.band(value),
          label:`${name} · ${plural(files.length, "file")}${amount ? ` · ${metric} ${amount}` : ""}`, shown:`${name}${amount ? `\n${amount}` : ""}`};
      });
      const rest = groups.length - kept.length;
      if (rest > 0 && budget.left > 0) {
        budget.left -= 1;
        chips.push({type:"chip", layerKey, band:null, label:`${plural(rest, "more module")} · open the band to see them`, shown:`+${rest} more\nopen band`});
      }
      return chips;
    }
    // What one drawing shows: the whole stack, one app's slice, or with no app chosen and two or more
    // named apps one column per app (#790); each part has its own layout and chip budget, and one heat
    // scale spans them.
    function plan(full, scope) {
      const metric = state.metric();
      // All apps columns run in interface order (#949): each server between its clients.
      const found = scope === null && full.apps?.length >= 2 ? apps().groups(full) : [], allApps = found.length >= 2;
      const columns = allApps ? apps().columnOrder(full, found.map(group => group.name)).map(name => found.find(group => group.name === name)) : found;
      const payload = scope === null ? full : apps().scoped(full, scope);
      const parts = allApps ? columns.map(group => {
        const slice = apps().scoped(full, group.name);
        return {group, slice, layout:summary(slice), budget:{left:Math.floor(BOX_CAP / columns.length), truncated:0}};
      }) : [{group:null, slice:payload, layout:summary(payload), budget:{left:BOX_CAP, truncated:0}}];
      const layout = allApps ? summary(full) : parts[0].layout;
      // Collapsing layers or directories changes representation, not their module population. The
      // unassigned row is always drawn (as metric chips when collapsed, #449), so it is always in the scale.
      const visible = payload.modules.filter(module =>
        (module.layer === null || layout.layers.has(module.layer)) && module.declarationCount > 0);
      const scale = createScale(metric, state.aggregate(), visible.map(module => metricValue(module.metrics, metric)), state.relative());
      return {scope, allApps, payload, parts, layout, scale};
    }
    // The adapter's display options for one part; the caller adds its width, title and handlers.
    function options(part, scale, extra = {}) {
      const metric = state.metric(), slice = part.slice, budget = part.budget, cap = part.budget.left, name = part.group?.name;
      const unassigned = slice.modules.filter(module => module.layer === null).length;
      return {
        showTests:state.showTests(), metric, aggregate:state.aggregate(), ...extra,
        moduleValue:module => metric === "layer" ? undefined : metricValue(module.metrics, metric),
        moduleBand:module => metric === "layer" ? null : scale.band(metricValue(module.metrics, metric)),
        layerLabel, layerNumber, layerStats:name === undefined ? layerStats : layer => layerStats(layer, name, slice), edgeText, edgeTone,
        display:{layers:part.layout.layers, expanded:name => budget.left > 0 && expanded(slice.layers.find(layer => layer.name === name)), entries:(key, modules) => entries(key, modules, scale, budget), reset:() => { budget.left = cap; budget.truncated = 0; },
          summary:(key, modules, columns) => summaryChips(key, modules, columns, scale, budget),
          edge:() => true,
          unassigned:{label:`Unassigned / unknown\n${plural(unassigned, "file")} · no layer`, get expanded() { return state.shown().unassigned && budget.left > 0; }}},
      };
    }
    // Whether a layer of an All apps column is a drawn Shared band (true), a drawn ordered band (false) or none (null).
    function bandOf(parts) {
      const partOf = new Map(parts.map(part => [part.group.name, part]));
      return (name, layerName) => {
        const part = partOf.get(name), layer = part?.slice.layers.find(item => item.name === layerName);
        return layer && part.layout.layers.has(layerName) && part.slice.layers.some(item => !item.shared) ? layer.shared : null;
      };
    }
    // Each All apps column's Shared bands sit on the side away from its interface partners (#850).
    const sides = (full, parts, sideBySide) => apps().sharedSides(full, parts.map(part => part.group.name), bandOf(parts), sideBySide);
    // The lines each gutter between All apps columns carries, which size it (#949).
    const lanes = (full, parts) => apps().laneCounts(full, parts.map(part => part.group.name), bandOf(parts));
    // An All apps column's header lines.
    function columnHeader(part) {
      const metric = state.metric(), layered = part.slice.layers.some(layer => !layer.shared);
      const worstValue = worst(part.slice.modules.filter(module => module.layer !== null));
      return apps().header(part.group, {layered, metric, max:worstValue === undefined ? "" : `${metric === "maintainabilityIndex" ? "min" : "max"} ${display(worstValue)}`});
    }
    // Measured coverage beside the arrow key, the one place it is said (#449, #525):
    // drawn counts are lower bounds. Rule-instance counts live behind the ⓘ.
    // One app (#913): the rows of that app's own layers, as the drawing shows only that app's slice.
    function coverageRows(payload) {
      const rows = payload.references?.coverage || [], group = state.scopeGroup(payload);
      if (!group) return {rows, group};
      const names = new Set(apps().scoped(payload, group.name).layers.map(layer => layer.name));
      return {rows:rows.filter(row => names.has(row.layer)), group};
    }
    function coverageSummary(payload) {
      const {rows, group} = coverageRows(payload);
      const sites = rows.reduce((sum, row) => sum + row.sites, 0), mapped = rows.reduce((sum, row) => sum + row.mappedSites, 0);
      const parts = [`${mapped.toLocaleString("en-US")} of ${sites.toLocaleString("en-US")} reference sites${group ? ` from ${group.name}'s layers` : ""} mapped to a module (${percent(mapped, sites)})`];
      if (rows.some(row => row.unresolvedSites > 0)) parts.push("≥ counts are lower bounds");
      return parts.join(" · ");
    }
    // One app: the name's summed counts, as the CLI reports them per app; otherwise the checkout's.
    function ruleCounts(payload) {
      const group = state.scopeGroup(payload), counts = group ? group.counts : payload.rules?.counts;
      return counts && Number.isSafeInteger(counts.unknown) && Number.isSafeInteger(counts.comparable)
        ? `${counts.unknown.toLocaleString("en-US")} of ${counts.comparable.toLocaleString("en-US")} production rule instances unknown${group ? ` in ${group.name}` : ""}.` : "";
    }
    // The inspector's review chips (#449): deviations, unassigned files and authored exceptions, for the
    // chosen app or the whole checkout. `null` counts are not shown.
    function reviewCounts(payload, group) {
      const counts = group ? group.counts : payload.rules?.counts, ready = payload.state === "ready";
      return {deviates:ready && Number.isSafeInteger(counts?.deviates) ? counts.deviates : null,
        unassigned:ready ? payload.modules.filter(module => module.layer === null && (!group || module.app === group.name)).length : null,
        exceptions:Number.isSafeInteger(payload.exceptionCount) ? payload.exceptionCount : null};
    }
    // The heat legend's words for a scale.
    const legendText = scale => state.metric() === "layer" ? "Layer membership · neutral chips; no metric heat."
      : `${scale.label} · ${state.metric()} · ${state.aggregate()} · ${state.role() === "all" ? "all indexed callables" : callables()}`;
    return {layerOf, layerNumber, layerTag, layerLabel, layerCounts, layerStats, interfaceStats, hint, expanded, testLayer, flagged, edgeTone, coverageOf, percent,
      lowerBound, edgeText, summary, worst, level, capped, entries, moduleGroups, summaryChips, plan, options, sides, lanes, columnHeader,
      coverageRows, coverageSummary, ruleCounts, reviewCounts, legendText, callables};
  }
  window.TrellisArchitectureDisplay = {createDisplay, moduleKey, commonDirectory, groupModules, display, external, plural, BOX_CAP};
})();
