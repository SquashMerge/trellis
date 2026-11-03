"use strict";
// #637: real-geometry checks of a drawn Architecture summary, run in the page with
// `page.evaluate(arrowGeometry, selector)`. Everything is measured in page pixels from the
// rendered SVG: band and chip boxes from their bounding rectangles, arrow segments from each
// connector's path data mapped through its screen matrix.
//
// - `crossings`: an arrow segment entering a band or chip box (borders may be touched);
// - `tagHits`: a count tag overlapping a band, a chip or another tag;
// - `upward`: each upward arrow that skips a band row, and whether it runs in the left channel
//   (a vertical run left of every band);
// - `widths`: each arrow's reference-site count and drawn stroke width.
function arrowGeometry(selector) {
  const svg = document.querySelector(selector);
  if (!svg) return {error:`no drawing at ${selector}`};
  const box = node => { const r = node.getBoundingClientRect(); return {x:r.x, y:r.y, w:r.width, h:r.height}; };
  const named = (node, kind) => ({kind, name:node.getAttribute("data-trellis-layer-id") ?? node.getAttribute("data-trellis-directory-id") ?? node.textContent.trim().slice(0, 60), ...box(node.querySelector(":scope > rect"))});
  // Bands: every layer box and the unassigned row. Chips: every drawn leaf and open directory.
  const bands = [...svg.querySelectorAll(".pele-cluster[data-trellis-layer-id], .pele-cluster[data-trellis-unassigned]")].map(node => named(node, "band"));
  const chips = [...svg.querySelectorAll(".pele-node"), ...svg.querySelectorAll(".pele-cluster[data-trellis-directory-id]")].map(node => named(node, "chip"));
  const obstacles = [...bands, ...chips];
  // A segment enters a box when it passes through the box shrunk by one pixel (Liang-Barsky).
  function enters(a, b, r) {
    const x0 = r.x + 1, x1 = r.x + r.w - 1, y0 = r.y + 1, y1 = r.y + r.h - 1;
    if (x1 <= x0 || y1 <= y0) return false;
    let t0 = 0, t1 = 1;
    const dx = b[0] - a[0], dy = b[1] - a[1];
    for (const [p, q] of [[-dx, a[0] - x0], [dx, x1 - a[0]], [-dy, a[1] - y0], [dy, y1 - a[1]]]) {
      if (p === 0) { if (q < 0) return false; continue; }
      const t = q / p;
      if (p < 0) { if (t > t1) return false; if (t > t0) t0 = t; } else { if (t < t0) return false; if (t < t1) t1 = t; }
    }
    return t1 - t0 > 1e-6;
  }
  // Connector path data to page-pixel points; curves are sampled finely.
  function points(path) {
    const matrix = path.getScreenCTM(), out = [];
    const length = path.getTotalLength();
    const d = path.getAttribute("d") || "";
    const map = p => [matrix.a * p.x + matrix.c * p.y + matrix.e, matrix.b * p.x + matrix.d * p.y + matrix.f];
    if (/^[ML0-9.,\s-]+$/.test(d)) {
      for (const [, x, y] of d.matchAll(/(-?[\d.]+),(-?[\d.]+)/g)) out.push(map({x:Number(x), y:Number(y)}));
      return out;
    }
    for (let at = 0; at < length; at += 2) out.push(map(path.getPointAtLength(at)));
    out.push(map(path.getPointAtLength(length)));
    return out;
  }
  const labels = new Map([...svg.querySelectorAll(".pele-edge-label")].map(node => [node.getAttribute("data-id"), node]));
  const crossings = [], widths = [], upward = [], tags = [];
  const layerRect = new Map(bands.filter(b => b.kind === "band").map(b => [b.name, b]));
  const left = Math.min(...[...layerRect.values()].map(b => b.x));
  for (const edge of svg.querySelectorAll(".pele-edge")) {
    const path = edge.querySelector("path:not(.pele-marker)"), label = labels.get(edge.getAttribute("data-id"));
    const id = label?.getAttribute("data-trellis-edge-id"), aria = label?.getAttribute("aria-label") || "";
    const name = `${label?.querySelector("text")?.textContent ?? "?"} ${id ?? ""}`;
    const pts = points(path);
    for (let i = 0; i + 1 < pts.length; i++) for (const r of obstacles) if (enters(pts[i], pts[i + 1], r)) crossings.push(`${name} × ${r.kind}:${r.name}`);
    widths.push({count:Number(/^(\d+)/.exec(aria)?.[1]), width:Number(path.getAttribute("stroke-width") || 1), name});
    if (id && /· upward/.test(aria)) {
      const [source, destination] = JSON.parse(id), s = layerRect.get(source), t = layerRect.get(destination);
      // A long upward arrow skips at least one band row between its source and target.
      const between = s && t ? [...layerRect.values()].filter(b => b.y + b.h <= s.y + 0.5 && b.y >= t.y + t.h - 0.5).length : 0;
      const channel = pts.some((p, i) => i + 1 < pts.length && Math.abs(p[0] - pts[i + 1][0]) < 0.5 && Math.abs(p[1] - pts[i + 1][1]) > 1 && p[0] < left - 0.5);
      upward.push({name, long:between > 0, channel});
    }
    if (label) tags.push({name, ...box(label.querySelector("rect"))});
  }
  const hit = (a, b) => a.x < b.x + b.w - 0.5 && b.x < a.x + a.w - 0.5 && a.y < b.y + b.h - 0.5 && b.y < a.y + a.h - 0.5;
  const tagHits = [];
  tags.forEach((tag, i) => {
    for (const r of obstacles) if (hit(tag, r)) tagHits.push(`${tag.name} × ${r.kind}:${r.name}`);
    for (const other of tags.slice(i + 1)) if (hit(tag, other)) tagHits.push(`${tag.name} × tag:${other.name}`);
  });
  return {bands:bands.length, chips:chips.length, arrows:widths.length, crossings, tagHits, upward, widths};
}
module.exports = {arrowGeometry};
