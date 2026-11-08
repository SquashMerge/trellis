// The Architecture diagram, drawn by the browser's own code: web/architecture-pele.js (the
// Pele adapter: projection, validation, labels, routed arrows) and the vendored Pele are
// bundled unchanged and run here headless over src/dom.js. The summary-display options the
// browser's web/architecture.js builds inside its DOM-bound `create()` closure are ported
// below, with its defaults (metric cognitive, aggregate max, production role, fixed bands,
// tests hidden); they are marked with the function names they mirror. Keep them in step.
import makeMetricHeat from "trellis-web:metric-heat";
import makeArchitecture from "trellis-web:architecture";
import makeArchitecturePele from "trellis-web:architecture-pele";
import { DOMParser, TextEncoder, serialize } from "./dom.js";
import { FONT_FAMILY, document, pele } from "./runtime.js";

export { measure } from "./runtime.js";

const heat = makeMetricHeat({});
const helpers = makeArchitecture({ TrellisMetricHeat: heat }, document);
const adapter = makeArchitecturePele({ pele }, document, DOMParser, TextEncoder);
const { metricValue, createScale } = heat;
const { moduleKey, commonDirectory, groupModules } = helpers;

// --- web/architecture.js summary display, ported (browser defaults) ---
const METRIC = "cognitive", AGGREGATE = "max", ROLE = "production";
const finite = (value) => typeof value === "number" && Number.isFinite(value);
const display = (value) => (finite(value) ? String(value) : "—");
const plural = (count, word) => `${count} ${word}${count === 1 ? "" : "s"}`;
const STATS = [["up", "↑", "upward deviation", true], ["cross", "⇄", "peer or shared deviation", true], ["down", "↓", "downward"],
  ["shared", "→shared ", "into shared"], ["allowed", "allowed ", "allowed by policy"], ["excepted", "excepted ", "excepted"], ["unknown", "unknown ", "unknown"]];
const ENTRY_CAP = 40, BOX_CAP = 150;
const callables = () => "production callables";
const same = (a, b) => moduleKey(a) === moduleKey(b);
const hint = (layer) => (["expanded", "collapsed", "hidden"].includes(layer.display) ? layer.display : null);

/** Everything the browser derives from one payload for its summary display (#391, #449). */
export function summarize(view, open = new Map()) {
  const expanded = (layer) => (open.has(layer.name) ? open.get(layer.name) : hint(layer) === "expanded");
  const layerOf = (identity) => (identity.kind !== "externalPackage"
    ? view.modules.find((module) => same(module.identity, identity))?.layer
    : view.references?.externalPackages?.find((item) => same(item.identity, identity))?.layer ?? null);
  const worst = (modules) => {
    const values = modules.map((module) => metricValue(module.metrics, METRIC)).filter(finite);
    return values.length ? Math.max(...values) : null;
  };
  // layerCounts: outgoing production reference sites by verdict.
  function layerCounts(layer) {
    const byName = new Map(view.layers.map((item) => [item.name, item]));
    const counts = { up: 0, cross: 0, down: 0, shared: 0, allowed: 0, excepted: 0, unknown: 0 };
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
  const coverageOf = (layerName, siteRole) => (view.references?.coverage || []).find((row) => row.layer === layerName && row.role === siteRole) || null;
  const lowerBound = (layerName, siteRole) => (coverageOf(layerName, siteRole)?.unresolvedSites ?? 1) > 0;
  function layerStats(layer) {
    const counts = layerCounts(layer), bound = lowerBound(layer.name, "production") ? "≥" : "";
    return STATS.filter(([key]) => counts[key] > 0).map(([key, glyph, words, error]) =>
      ({ key, count: counts[key], text: `${glyph}${bound}${counts[key]}`, label: `${bound ? "at least " : ""}${counts[key]} outgoing production reference sites · ${words}`, tone: error ? "error" : "neutral" }));
  }
  function layerLabel(layer) {
    const modules = view.modules.filter((module) => module.layer === layer.name);
    const values = modules.map((module) => metricValue(module.metrics, METRIC)).filter(finite);
    const packages = (view.references?.externalPackages || []).filter((item) => layerOf(item.identity) === layer.name);
    const max = values.length ? String(values.reduce((most, value) => Math.max(most, value), -Infinity)) : "—";
    const parts = [`${layer.shared ? "Shared" : layer.order} · ${layer.name}`, `${plural(modules.length, "file")}${packages.length ? ` · ${packages.length} external packages` : ""}`, `max ${max}`];
    return parts.join(layer.shared ? "\n" : " · ");
  }
  function testLayer(layer) {
    const members = view.modules.filter((module) => module.layer === layer.name);
    const test = (module) => module.excludedReason === "fixture" || (module.roleCounts?.production === 0 && module.roleCounts?.test > 0);
    const empty = (module) => !module.excludedReason && module.roleCounts?.production === 0 && module.roleCounts?.test === 0;
    return members.some(test) && members.every((module) => test(module) || empty(module));
  }
  const flagged = (edge) => (edge.status === "deviates" && !edge.excepted) || edge.status === "unknown";
  const edgeTone = (edge) => (edge.role === "test" ? "neutral" : edge.status === "unknown" ? "unknown" : flagged(edge) ? "deviation" : "allowed");
  const percent = (part, whole) => {
    if (!whole) return "—";
    const value = (part / whole) * 100;
    return part > 0 && value < 1 ? "<1%" : part < whole && value > 99 ? ">99%" : `${Math.round(value)}%`;
  };
  const edgeTier = (edge) => ({ semantic: "; semantic bindings", mixed: "; syntax and semantic bindings" })[edge.evidenceTier] || "";
  function edgeText(edge) {
    const covered = edge.coverage || coverageOf(edge.sourceLayer, edge.role);
    const bound = edge.coverage ? edge.coverage.complete !== true : lowerBound(edge.sourceLayer, edge.role);
    if (!covered) return { bound };
    const share = percent(covered.mappedSites, covered.sites);
    const label = `${bound ? "lower bound; " : ""}${share} of ${covered.sites} ${edge.role} reference sites from ${edge.sourceLayer} mapped to a module${edgeTier(edge)}`;
    return { bound, label, note: bound && ["deviation", "unknown"].includes(edgeTone(edge)) ? `${share} mapped` : "" };
  }
  // summary: unhinted test/fixture layers and authored hidden layers are not drawn.
  const tests = new Set(view.layers.filter((layer) => hint(layer) === null && testLayer(layer)).map((layer) => layer.name));
  const hidden = view.layers.filter((layer) => hint(layer) === "hidden" || tests.has(layer.name));
  const layers = new Set(view.layers.filter((layer) => !hidden.includes(layer)).map((layer) => layer.name));
  const visible = view.modules.filter((module) => (module.layer === null || layers.has(module.layer)) && module.declarationCount > 0);
  const scale = createScale(METRIC, AGGREGATE, visible.map((module) => metricValue(module.metrics, METRIC)), false);
  const budget = { left: BOX_CAP, truncated: 0 };
  function level(layerKey, base, members) {
    return groupModules(members, base).map(([name, files]) => {
      files.sort((a, b) => a.identity.path.localeCompare(b.identity.path));
      if (files.length === 1) return { type: "module", module: files[0], name };
      // The mod draws no drilled-in directories yet: a directory module stays one chip.
      const prefix = `${base}${name}`, key = JSON.stringify([layerKey, prefix]), value = worst(files);
      const amount = `max ${display(value)}`;
      return { type: "directory", key, open: false, size: files.length, band: scale.band(value), entries: [], children: null,
        label: `${name}\n${plural(files.length, "file")} · ${amount}\n${prefix}`, shown: `${name}\n${display(value)} · ${plural(files.length, "file")}` };
    });
  }
  function capped(layerKey, list) {
    const room = Math.min(ENTRY_CAP, budget.left), kept = list.length > room ? list.slice(0, Math.max(0, room - 1)) : list;
    budget.left -= kept.length;
    if (kept.length === list.length) return kept;
    const omitted = list.slice(kept.length).reduce((sum, entry) => sum + (entry.size ?? 1), 0);
    budget.truncated += omitted;
    if (budget.left < 1) return kept;
    budget.left -= 1;
    return [...kept, { type: "note", layerKey, label: `${plural(omitted, "file")} not drawn · diagram size cap\nselect to list ${layerKey === null ? "unassigned" : "this layer's"} files`,
      shown: `${plural(omitted, "file")}\nnot drawn (cap)` }];
  }
  function entries(layerKey, modules) {
    const measured = modules.filter((module) => module.declarationCount > 0);
    const result = capped(layerKey, level(layerKey, commonDirectory(modules.map((module) => module.identity.path)), measured));
    const unmeasured = modules.length - measured.length;
    if (unmeasured && budget.left > 0) {
      budget.left -= 1;
      result.push({ type: "note", layerKey, label: `${plural(unmeasured, "file")} · metrics unavailable\nno ${callables()}`, shown: `${plural(unmeasured, "file")}\nno metrics` });
    }
    return result;
  }
  // The groups a collapsed band summarizes, hottest first: the module chips.
  function moduleGroups(modules) {
    const measured = modules.filter((module) => module.declarationCount > 0);
    const groups = groupModules(measured, commonDirectory(modules.map((module) => module.identity.path))).map(([name, files]) => ({ name, files, value: worst(files) }));
    const severity = (value) => (!finite(value) ? -Infinity : value);
    return groups.sort((a, b) => severity(b.value) - severity(a.value) || a.name.localeCompare(b.name));
  }
  function summaryChips(layerKey, modules, columns) {
    const groups = moduleGroups(modules);
    const room = Math.min(columns, ENTRY_CAP, budget.left), kept = groups.length > room ? groups.slice(0, Math.max(Math.min(1, room), room - 1)) : groups;
    budget.left -= kept.length;
    const chips = kept.map(({ name, files, value }) => ({ type: "chip", layerKey, band: scale.band(value),
      label: `${name} · ${plural(files.length, "file")} · ${METRIC} ${display(value)}`, shown: `${name}\n${display(value)}`, group: name }));
    const rest = groups.length - kept.length;
    if (rest > 0 && budget.left > 0) {
      budget.left -= 1;
      chips.push({ type: "chip", layerKey, band: null, label: `${plural(rest, "more module")} · open the band to see them`, shown: `+${rest} more\nopen band` });
    }
    return chips;
  }
  const unassigned = view.modules.filter((module) => module.layer === null).length;
  const options = {
    showTests: false, metric: METRIC, aggregate: AGGREGATE,
    moduleValue: (module) => metricValue(module.metrics, METRIC),
    moduleBand: (module) => scale.band(metricValue(module.metrics, METRIC)),
    layerLabel, layerStats, edgeText, edgeTone,
    display: { layers, expanded: (name) => budget.left > 0 && expanded(view.layers.find((layer) => layer.name === name)),
      entries: (key, modules) => entries(key, modules), reset: () => { budget.left = BOX_CAP; budget.truncated = 0; },
      summary: (key, modules, columns) => summaryChips(key, modules, columns), edge: () => true,
      unassigned: { label: `Unassigned / unknown\n${plural(unassigned, "file")} · no layer`, expanded: false } },
    // Selection callbacks make the adapter mark nodes as controls; clicks are hit-tested (see DESIGN-NOTES).
    onSelectModule() {}, onSelectEdge() {}, onSelectLayer() {}, onToggleLayer() {}, onToggleDirectory() {}, onToggleUnassigned() {}, onSelectNote() {},
  };
  return { options, hidden, tests, layers, scale, budget, layerStats, layerCounts, moduleGroups, worst, expanded };
}

// The browser's dark brand palette, as the adapter writes it, to the theme colour names the
// mod draws with. Heat keeps its order (neutral, then three warmer steps).
function palette(t) {
  return {
    "#0B0C10": t.background, "#F7F7FB": t.foreground, "#B8BAC8": t.muted, "#777A8E": t.input || t.dim,
    "#14151D": t.surface, "#1D1E2A": t.raised, "#343644": t.border, "#FF8E9A": t.ansi1,
    "#FFC20A": t.ansi11 || t.ansi3, "#FFAA10": t.ansi3, "#FF7A00": t.ansi9 || t.ansi1,
  };
}

/**
 * Lays out and draws one payload at `width` (logical px). Returns the themed SVG text, its
 * size, and the drawn controls' boxes (for hit-testing clicks) and the module chips by layer.
 * Throws a typed Error (`code`) when the payload or the drawing is invalid; never a partial
 * drawing.
 */
export async function draw(view, { width, open, theme, highlight }) {
  const summary = summarize(view, open);
  const container = document.createElement("div");
  const outcome = await adapter.render(container, view, { ...summary.options, width });
  if (outcome?.status !== "ready") {
    const error = new Error(outcome?.error?.message || "Architecture mapping failed. Nothing displayed.");
    error.code = outcome?.error?.code || "render_failed";
    throw error;
  }
  const root = container.children[0];
  const colors = palette(theme);
  root.setAttribute("font-family", FONT_FAMILY);
  // Heat chips carry ink text on a warm fill; ink must stay dark in a light theme too.
  const ink = theme.ansi0 || "#000000";
  const chips = new Map();
  for (const item of outcome.projection.nodes.values()) if (item.kind === "C" && item.record.group) chips.set(JSON.stringify([item.record.layerKey, item.record.group]), item.id);
  const highlighted = highlight ? chips.get(JSON.stringify(highlight)) : null;
  const override = (el) => {
    const out = {};
    for (const [name, value] of el.attrs) {
      if (name !== "fill" && name !== "stroke") continue;
      const upper = value.toUpperCase();
      if (el.localName === "text" && upper === "#0B0C10") out[name] = ink;
      else if (colors[upper]) out[name] = colors[upper];
    }
    if (highlighted && el.localName === "rect" && el.parentNode?.getAttribute("data-id") === highlighted) {
      out.stroke = theme.accent; out["stroke-width"] = 3;
    }
    return out;
  };
  const svg = serialize(root, override);
  const size = { width: Number(root.getAttribute("width")), height: Number(root.getAttribute("height")) };
  return { svg, ...size, summary, projection: outcome.projection, boxes: boxes(root, outcome.projection) };
}

// Each drawn control's box in SVG units, which are the image's logical pixels: a click on the
// image is hit-tested against these (the smallest box under the pointer wins).
function boxes(root, projection) {
  const out = [];
  for (const node of root.querySelectorAll("[data-trellis-architecture-id]")) {
    const id = node.getAttribute("data-trellis-architecture-id"), item = projection.nodes.get(id) || projection.edges.get(id);
    const rect = node.querySelector(":scope > rect") || node.querySelector("rect");
    if (!item || !rect) continue;
    let x = Number(rect.getAttribute("x") || 0), y = Number(rect.getAttribute("y") || 0);
    const at = /translate\(\s*(-?[\d.]+)[ ,]+(-?[\d.]+)\s*\)/.exec(node.getAttribute("transform") || "");
    if (at) { x += Number(at[1]); y += Number(at[2]); }
    out.push({ id, kind: item.kind, x, y, w: Number(rect.getAttribute("width")), h: Number(rect.getAttribute("height")), record: item.record, label: item.full });
  }
  return out;
}

export function hit(boxes, x, y) {
  return boxes.filter((b) => x >= b.x && x <= b.x + b.w && y >= b.y && y <= b.y + b.h).sort((a, b) => a.w * a.h - b.w * b.h)[0] || null;
}

export { heat, plural, display };
