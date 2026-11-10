// The Architecture diagram, drawn by the browser's own code: web/architecture-pele.js (the
// Pele adapter: projection, validation, labels, routed arrows) and the vendored Pele run here
// headless over src/dom.js. What the browser derives from a payload for its summary display
// (band labels and stat chips, module chips and their budget, the per-app parts of All apps,
// the heat scale) is web/architecture.js's own `createDisplay`, and the app switcher, column
// headers, interface cards, cross-app lines and inspector facts are web/architecture-apps.js's.
// All are bundled unchanged (#886), so the two views can't drift. The colour tokens and the
// Color by and Aggregation choices are read from web/style.css and web/index.html at build time.
import makeMetricHeat from "trellis-web:metric-heat";
import makeApps from "trellis-web:architecture-apps";
import makeArchitecture from "trellis-web:architecture";
import makeArchitecturePele from "trellis-web:architecture-pele";
import makeDeviations from "trellis-web:architecture-deviations";
import TOKENS from "trellis-web:tokens";
import CONTROLS from "trellis-web:controls";
import { DOMParser, TextEncoder, serialize } from "./dom.js";
import { FONT_FAMILY, document, pele } from "./runtime.js";

export { measure } from "./runtime.js";

export const heat = makeMetricHeat({});
export const apps = makeApps({});
const helpers = makeArchitecture({ TrellisMetricHeat: heat, TrellisArchitectureApps: apps }, document);
const adapter = makeArchitecturePele({ pele }, document, DOMParser, TextEncoder);
/** The Deviations tree's pure model (web/architecture-deviations.js). */
export const deviationModel = makeDeviations({}, document).model;
export { TOKENS, CONTROLS };

// The browser's defaults are the selects' own: the `selected` option, else the first.
const initial = (options) => (options.find((option) => option.selected) ?? options[0]).value;
export const DEFAULT_METRIC = initial(CONTROLS.metrics), DEFAULT_AGGREGATE = initial(CONTROLS.aggregates);

const finite = (value) => typeof value === "number" && Number.isFinite(value);
export const display = (value) => (finite(value) ? String(value) : "—");
export const plural = (count, word) => `${count} ${word}${count === 1 ? "" : "s"}`;

/**
 * Everything the browser derives from one payload for the current choices: `metric`,
 * `aggregate`, `scope` (an app name, or null for All apps), and the display state `open`
 * (band → open), `drilled` (open directory modules) and `unassigned` (the unassigned row open).
 */
export function summarize(view, { open = new Map(), drilled = new Set(), unassigned = false, metric = DEFAULT_METRIC, aggregate = DEFAULT_AGGREGATE, scope = null } = {}) {
  const shown = { open, drilled, unassigned, hidden: false };
  let drawn = null;
  // web/architecture.js scopeGroup: with fewer than two apps there is no switcher, so no scope.
  const scopeGroup = (payload = view) => (scope === null || payload?.state !== "ready" || !(payload.apps?.length >= 2) ? null : apps.group(payload, scope));
  const shared = helpers.createDisplay({ metric: () => metric, aggregate: () => aggregate, role: () => "production", relative: () => false,
    showTests: () => false, view: () => view, model: () => drawn ?? view, shown: () => shown, endMark: () => null, scopeGroup });
  const group = scopeGroup();
  const plan = shared.plan(view, group?.name ?? null);
  drawn = plan.payload;
  return {
    ...shared, plan, group, scope: group?.name ?? null, metric, aggregate, scale: plan.scale, layers: plan.layout.layers, tests: plan.layout.tests,
    hidden: plan.layout.hidden, choices: apps.choices(apps.assignment(view)),
    // An app with no ordered layer draws nothing: its files are listed as unassigned (AC13).
    noLayers: group !== null && !plan.payload.layers.some((layer) => !layer.shared),
    reviewCounts: shared.reviewCounts(view, group),
  };
}

// The browser's dark palette, as the adapter writes it, to the Herdr theme where a theme
// colour has the same role: background, text, surfaces and lines. The heat ramp, the error
// tone and ink on heat stay the web app's own tokens (web/style.css), so chips read the same.
function palette(t) {
  return {
    [TOKENS["sm-ink"]]: t.background, [TOKENS["sm-white"]]: t.foreground, [TOKENS["sm-muted"]]: t.muted, [TOKENS["sm-control"]]: t.input || t.dim,
    [TOKENS["sm-surface"]]: t.surface, [TOKENS["sm-raised"]]: t.raised, [TOKENS["sm-line"]]: t.border,
  };
}
/** The fill of heat band 0–3, as the browser draws it (band 0 is the theme's line colour). */
export const heatFill = (t, band) => (band > 0 ? [TOKENS["sm-gold"], TOKENS["sm-amber"], TOKENS["sm-orange"]][band - 1] : t.border);
export const HEAT_INK = TOKENS["sm-ink"], ERROR = TOKENS["sm-error"], WARNING = TOKENS["sm-gold"];

// Pressing a control in the image is hit-tested (see DESIGN-NOTES); the adapter only needs
// the callbacks to exist to mark nodes and stat chips as controls.
const HANDLERS = { onSelectModule() {}, onSelectEdge() {}, onSelectLayer() {}, onToggleLayer() {}, onToggleDirectory() {}, onToggleUnassigned() {},
  onSelectNote() {}, onOpenDeviation() {}, onSelectInterface() {} };
// The "Selected" word on an interface stub is shown by the browser's CSS only while it is
// pressed; an image has no CSS, so it is left out.
const HIDDEN_CLASSES = ["architecture-selected-tag", "architecture-selected-tag-back"];
const SVG_NS = "http://www.w3.org/2000/svg";

async function renderPart(view, options) {
  const container = document.createElement("div");
  // `pause` yields to the host between heavy phases: the Herdr runtime silently ends a task
  // whose one synchronous run is too long (see DESIGN-NOTES, "The synchronous-run cap").
  const outcome = await adapter.render(container, view, { ...options, ...HANDLERS });
  if (outcome?.status !== "ready") {
    const error = new Error(outcome?.error?.message || outcome?.reason || "Architecture mapping failed. Nothing displayed.");
    error.code = outcome?.error?.code || "render_failed";
    throw error;
  }
  const root = container.children[0];
  return { root, projection: outcome.projection, bands: outcome.bands, width: Number(root.getAttribute("width")), height: Number(root.getAttribute("height")) };
}

/**
 * Lays out and draws one payload at `width` (logical px) for `choice` (see `summarize`).
 * Returns the summary and the images: one for a single stack, one composed image for All
 * apps side by side (with the cross-app lines), or one per app column when the columns stack
 * (null for an app with no layers). Each image has its themed SVG text, size and the drawn
 * controls' boxes (for hit-testing). Throws a typed Error (`code`) when the payload or the
 * drawing is invalid; never a partial drawing.
 */
export async function draw(view, { width, theme, highlight, pause, choice }) {
  const summary = summarize(view, choice);
  const { plan } = summary;
  if (summary.noLayers) return { summary, images: [], cards: [], sideBySide: false };
  const colors = palette(theme);
  const override = (highlighted) => (el) => {
    const cls = (el.getAttribute?.("class") || "").split(/\s+/);
    if (HIDDEN_CLASSES.some((name) => cls.includes(name))) return null;
    const out = {};
    for (const [name, value] of el.attrs) {
      if (name !== "fill" && name !== "stroke") continue;
      // Ink text sits on heat fills, which keep the web colours: it stays ink.
      if (el.localName === "text" && value.toUpperCase() === TOKENS["sm-ink"]) continue;
      if (colors[value.toUpperCase()]) out[name] = colors[value.toUpperCase()];
    }
    if (highlighted && el.localName === "rect" && el.parentNode?.getAttribute("data-id") === highlighted) {
      out.stroke = theme.accent; out["stroke-width"] = 3;
    }
    return out;
  };
  const options = (part, partWidth, sharedSide) => summary.options(part, plan.scale, { width: partWidth, pause,
    ...(summary.scope !== null ? { title: `${summary.scope} · ordered layers` } : part.group ? { title: `${part.group.name} · ordered layers`, sharedSide } : {}) });

  if (!plan.allApps) {
    const drawn = await renderPart(plan.payload, options(plan.parts[0], width));
    if (pause) await pause();
    const image = finish(drawn, override(chipId(drawn.projection, highlight)), 0);
    if (pause) await pause();
    return { summary, images: [{ ...image, parts: [0] }], cards: [], sideBySide: false };
  }

  // All apps (#790): web/architecture.js drawApps, with the grid composed into one SVG.
  let layout = apps.layout({ width, count: plan.parts.length });
  const sides = summary.sides(view, plan.parts, layout.sideBySide);
  const drawings = [];
  for (const [index, part] of plan.parts.entries()) {
    const layered = part.slice.layers.some((layer) => !layer.shared);
    drawings.push(layered ? await renderPart(part.slice, options(part, layout.widths[index], sides.get(part.group.name))) : null);
    if (pause) await pause();
  }
  // A drawing wider than its column (an unbreakable name) widens the column.
  layout = apps.layout({ width, count: plan.parts.length, natural: drawings.map((d) => d?.width ?? 0) });
  const places = new Map(plan.parts.map((part, index) => {
    const x = layout.sideBySide ? layout.columns[index].x : 0, frames = drawings[index]?.bands ?? new Map();
    return [part.group.name, { index, top: 0, bands: new Map([...frames].filter(([, f]) => f).map(([name, f]) => [name, { x: f.x + x, y: f.y, w: f.w, h: f.h }])) }];
  }));
  const entries = view.interfaces.filter((entry) => entry && places.has(entry.client) && places.has(entry.server) && entry.client !== entry.server);
  const routed = apps.routes({ interfaces: entries }, layout, places, adapter.edgeWidth);
  const cards = entries.map((entry) => ({ entry, key: apps.key(entry), lines: apps.cardText(entry), ends: apps.endsText(entry, routed.undrawn.get(entry) || []),
    name: apps.cardName(entry), client: places.get(entry.client).index, server: places.get(entry.server).index }));
  const highlighted = (index) => chipId(drawings[index]?.projection, highlight, plan.parts[index].group.name);

  if (!layout.sideBySide) {
    const images = drawings.map((drawn, index) => (drawn ? { ...finish(drawn, override(highlighted(index)), index), parts: [index] } : null));
    if (pause) await pause();
    return { summary, images, cards, sideBySide: false, layout };
  }
  // Side by side: each column's SVG nested at its x, and the interface lines over the gutters.
  const height = Math.max(1, ...drawings.map((d) => d?.height ?? 0)) + 8;
  const inner = [], all = [];
  for (const [index, drawn] of drawings.entries()) {
    if (!drawn) continue;
    const x = layout.columns[index].x;
    drawn.root.setAttribute("x", String(x)); drawn.root.setAttribute("y", "0");
    const one = finish(drawn, override(highlighted(index)), index);
    inner.push(one.svg);
    for (const box of one.boxes) all.push({ ...box, x: box.x + x });
  }
  const line = theme.input || theme.border;
  const lines = routed.lines.map((item) => `<path d="${item.path}" fill="none" stroke="${line}" stroke-width="${item.width}"/><polygon points="${item.arrow}" fill="${line}"/>`);
  const svg = `<svg xmlns="${SVG_NS}" width="${layout.width}" height="${height}" viewBox="0 0 ${layout.width} ${height}" font-family="${FONT_FAMILY}">${inner.join("")}${lines.join("")}</svg>`;
  if (pause) await pause();
  return { summary, images: [{ svg, width: layout.width, height, boxes: all, parts: plan.parts.map((_, i) => i) }], cards, sideBySide: true, layout };
}

// The followed file's module chip in one drawing: `highlight` is [layerKey, group, app]; in
// All apps only the file's own app's column outlines it.
function chipId(projection, highlight, app = null) {
  if (!projection || !highlight || (app !== null && highlight[2] != null && highlight[2] !== app)) return null;
  for (const item of projection.nodes.values()) if (item.kind === "C" && item.record.group && item.record.layerKey === highlight[0] && item.record.group === highlight[1]) return item.id;
  return null;
}

function finish(drawn, override, part) {
  drawn.root.setAttribute("font-family", FONT_FAMILY);
  const svg = serialize(drawn.root, override);
  return { svg, width: drawn.width, height: drawn.height, projection: drawn.projection, boxes: boxes(drawn.root, drawn.projection, part) };
}

// Each drawn control's box in SVG units, which are the image's logical pixels: a click on the
// image is hit-tested against these (the smallest box under the pointer wins). Stat chips that
// act (an upward deviation, an HTTP interface stub) are boxes of kind "A" with their action.
function boxes(root, projection, part) {
  const out = [];
  const place = (node, rect) => {
    let x = Number(rect.getAttribute("x") || 0), y = Number(rect.getAttribute("y") || 0);
    const at = /translate\(\s*(-?[\d.]+)[ ,]+(-?[\d.]+)\s*\)/.exec(node.getAttribute("transform") || "");
    if (at) { x += Number(at[1]); y += Number(at[2]); }
    return { x, y, w: Number(rect.getAttribute("width")), h: Number(rect.getAttribute("height")) };
  };
  for (const node of root.querySelectorAll("[data-trellis-architecture-id]")) {
    const id = node.getAttribute("data-trellis-architecture-id"), item = projection.nodes.get(id) || projection.edges.get(id);
    const rect = node.querySelector(":scope > rect") || node.querySelector("rect");
    if (!item || !rect) continue;
    out.push({ id, kind: item.kind, ...place(node, rect), record: item.record, label: item.full, part });
  }
  for (const node of root.querySelectorAll("[data-trellis-action-id]")) {
    const rect = node.querySelector("rect");
    let action = null;
    try { action = JSON.parse(node.getAttribute("data-trellis-action-id")); } catch { continue; }
    if (!rect || !Array.isArray(action)) continue;
    out.push({ id: node.getAttribute("data-trellis-action-id"), kind: "A", ...place(node, rect), action, label: node.getAttribute("aria-label"), part });
  }
  return out;
}

export function hit(boxes, x, y) {
  return boxes.filter((b) => x >= b.x && x <= b.x + b.w && y >= b.y && y <= b.y + b.h).sort((a, b) => a.w * a.h - b.w * b.h)[0] || null;
}
