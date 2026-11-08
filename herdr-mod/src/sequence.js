// The Sequence diagram, drawn by the browser's own web/sequence-pele.js over the helpers in
// web/sequence.js (labels, lanes, the Interactions display), both bundled unchanged, with the
// shared headless Pele. Options are the browser's defaults: Interactions, details hidden.
import makeSequence from "trellis-web:sequence";
import makeSequencePele from "trellis-web:sequence-pele";
import { DOMParser, TextEncoder, serialize } from "./dom.js";
import { FONT_FAMILY, document, measure, pele } from "./runtime.js";

const env = { pele };
export const helpers = makeSequence(env, document);
const adapter = makeSequencePele(env, document, DOMParser, TextEncoder);
const OPTIONS = { showDetails: false, interactions: true };

/** The resolved declaration a call step reaches, or null (the browser's own proof rule). */
export const targetOf = (view, step) => helpers.target(view, step);

/** Pele's `var(--_x)` theme slots, as the browser's stylesheet fills them, in theme colours. */
function slots(t) {
  return { "var(--_bg)": t.background, "var(--_fg)": t.foreground, "var(--_m)": t.muted, "var(--_l)": t.input || t.dim,
    "var(--_s)": t.surface, "var(--_a)": t.raised, "var(--_b)": t.border, "var(--_c)": t.accent };
}

/**
 * Draws one sequence view. Returns {empty} when the view has nothing to draw, else the themed
 * SVG, its size and a box per step (for clicks). Throws a typed error when Pele can't draw it.
 */
export async function drawSequence(view, { theme, selectedId = null, expanded = new Set() }) {
  const empty = helpers.emptySummary(view, OPTIONS);
  if (empty && !helpers.display(view, OPTIONS).visible.size) return { empty };
  const container = document.createElement("div");
  const outcome = await adapter.render(container, view, null, expanded, { ...OPTIONS, selectedId, onSelect() {}, onOpenTarget() {} });
  if (outcome?.status !== "rendered") {
    const error = new Error(`Pele could not draw this sequence: ${outcome?.reason || outcome?.status}`);
    error.code = "unsupported";
    throw error;
  }
  const root = container.children[0];
  root.setAttribute("font-family", FONT_FAMILY);
  const colours = slots(theme);
  const override = (el) => {
    // The browser's expand/collapse buttons are HTML in a foreignObject: the steps list has them.
    if (el.localName === "foreignObject") return null;
    const out = {};
    for (const [name, value] of el.attrs) if (colours[value]) out[name] = colours[value];
    if (el.attrs.get("rx") === "var(--_r)") out.rx = 4;
    if (el.attrs.get("font-weight") === "var(--_hw)") out["font-weight"] = "bold";
    if (el.attrs.get("font-family") === "var(--_fm)") out["font-family"] = "JetBrains Mono, Menlo, monospace";
    if (el === root) { out.fill = theme.foreground; out.color = theme.foreground; }
    const pressed = el.parentNode?.getAttribute("aria-pressed") === "true";
    if (pressed && el.localName === "rect") { out.fill = theme.selected; out.stroke = theme.accent; out["stroke-width"] = 2; }
    return out;
  };
  const svg = serialize(root, override);
  const box = (root.getAttribute("viewBox") || "").split(/\s+/).map(Number);
  return { svg, width: Number(root.getAttribute("width")) || box[2], height: Number(root.getAttribute("height")) || box[3], boxes: boxes(root), empty };
}

// Each step's drawn box (its note, or its arrow's label) in SVG units.
function boxes(root) {
  const out = [];
  for (const node of root.querySelectorAll("[data-trellis-step-id]")) {
    if (node.classList.contains("pele-message")) continue;
    const rect = node.querySelector("rect"), text = node.querySelector("text");
    let b = null;
    if (rect) b = { x: Number(rect.getAttribute("x")), y: Number(rect.getAttribute("y")), w: Number(rect.getAttribute("width")), h: Number(rect.getAttribute("height")) };
    else if (text) {
      const w = measure(text.textContent, Number(text.getAttribute("font-size") || node.getAttribute("font-size") || 16));
      const x = Number(text.getAttribute("x")), anchor = text.getAttribute("text-anchor");
      b = { x: anchor === "middle" ? x - w / 2 : anchor === "end" ? x - w : x, y: Number(text.getAttribute("y")) - 16, w, h: 22 };
    }
    if (!b) continue;
    // Pele nests the drawing in translated groups: add every ancestor's offset.
    for (let n = node; n; n = n.parentNode) {
      const at = /translate\(\s*(-?[\d.]+)[ ,]+(-?[\d.]+)\s*\)/.exec(n.getAttribute?.("transform") || "");
      if (at) { b.x += Number(at[1]); b.y += Number(at[2]); }
    }
    if ([b.x, b.y, b.w, b.h].every(Number.isFinite)) out.push({ ...b, step: node.getAttribute("data-trellis-step-id") });
  }
  return out;
}
