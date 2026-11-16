"use strict";
// What the four Pele adapters share (Architecture, Sequence, Treemap, Classes): admission of
// Pele's SVG output, the typed "unsupported" failure, Mermaid text escaping, Pele's root theme
// string, button semantics and the canvas text measurer. Each adapter keeps its own root,
// theme, allowlists, error words and diagram-specific checks.
(() => {
  const NS = "http://www.w3.org/2000/svg";
  const ROOT = '<svg xmlns="http://www.w3.org/2000/svg" class="pele pele-';
  const MAX_BYTES = 8 * 1024 * 1024, MAX_NODES = 12000;
  const THEME = "--_bg:var(--pele-bg,#fff);--_fg:var(--pele-fg,#1f1f1f);--_m:var(--pele-muted,#6e6e6e);--_l:var(--pele-line,#1f1f1f);--_s:var(--pele-surface,#f3f3f3);--_a:var(--pele-surface-alt,#e6e6e6);--_b:var(--pele-border,#8a8a8a);--_c:var(--pele-accent,#5b7bd5);color:var(--_fg)";
  function unsupported(reason) { const error = new Error(reason); error.code = "unsupported"; throw error; }
  const bytes = value => new TextEncoder().encode(value).length;
  // Pele decodes numeric entities into SVG text. Escape everything outside a
  // small literal alphabet so repository text cannot become Mermaid grammar.
  const encode = value => Array.from(String(value)).map(c => /[A-Za-z0-9 ._-]/.test(c) ? c : `#${c.codePointAt(0)};`).join("");
  // The root style Pele writes: its colour variables, the radius and heading-weight variables
  // (present only when the diagram uses them), then the font at `size` px.
  function rootStyle(size, {radius = true, heading = false, prefix = ""} = {}) {
    return `${prefix}${THEME}${radius ? ";--_r:var(--pele-radius,4px)" : ""}${heading ? ";--_hw:var(--pele-heading-weight,bold)" : ""};--_fm:var(--pele-font-mono,monospace);font:${size}px sans-serif;letter-spacing:normal;word-spacing:normal;text-transform:none;direction:ltr;font-family:var(--pele-font,sans-serif)`;
  }
  // The style attribute to strip when `style` is in the SVG exactly once, else null.
  function themeOnce(raw, style) {
    const signature = ` style="${style}"`;
    return raw.split(signature).length === 2 ? signature : null;
  }
  // Admit Pele's SVG string and return its detached root and node count. `spec` is the adapter's:
  //   kind     the root class suffix ("block", "sequence", "class", "treemap")
  //   mark     true to add the `trellis-pele` class to the root
  //   theme    raw => the root style attribute to strip, or null when the theme changed
  //   active   active/resource markup rejected before parsing
  //   tags, attrs   the element and attribute allowlists
  //   words    the adapter's error text for each check
  function admit(raw, spec) {
    const {words} = spec, root = `${ROOT}${spec.kind}"`;
    if (typeof raw !== "string" || bytes(raw) > MAX_BYTES) unsupported(words.large);
    if (!raw.startsWith(root)) unsupported(words.root);
    const signature = spec.theme(raw);
    if (!signature) unsupported(words.theme);
    let safe = raw.replace(signature, "").replaceAll(' style="rx:var(--_r)"', ' rx="var(--_r)"');
    if (spec.mark) safe = `${root.slice(0, -1)} trellis-pele"${safe.slice(root.length)}`;
    if (/\sstyle\s*=|<style\b/i.test(safe)) unsupported(words.style);
    // Reject active/resource markup before DOM parsing: even detached parsing
    // can report CSP violations for active elements in some browsers.
    if (spec.active.test(safe)) unsupported(words.active);
    const svg = new DOMParser().parseFromString(safe, "image/svg+xml").documentElement;
    if (svg.localName !== "svg" || svg.namespaceURI !== NS || svg.querySelector("parsererror")) unsupported(words.invalid);
    let count = 0;
    for (const node of [svg, ...svg.querySelectorAll("*")]) {
      if (++count > MAX_NODES || node.namespaceURI !== NS || !spec.tags.has(node.localName)) unsupported(words.element);
      for (const attr of node.attributes) if (!spec.attrs.has(attr.name) || /url\s*\(|javascript:|<|>|[\u0000-\u001f]/i.test(attr.value)) unsupported(words.attribute);
    }
    return {root:svg, count};
  }
  // Button semantics for a drawn node: role, focus, name, and activation by click, Enter or Space.
  function asButton(node, label, activate) {
    node.setAttribute("role", "button"); node.setAttribute("tabindex", "0"); node.setAttribute("aria-label", label);
    node.addEventListener("click", activate);
    node.addEventListener("keydown", event => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); activate(event); } });
  }
  // A text-width function over one 2D canvas context, or null where the host has none.
  // `measure.font(value)` sets the font the following widths are measured in.
  function textMeasurer(font) {
    const ctx = document.createElement("canvas").getContext("2d");
    if (!ctx) return null;
    const measure = text => ctx.measureText(text).width;
    measure.font = value => { ctx.font = value; };
    if (font) measure.font(font);
    return measure;
  }
  window.TrellisPeleSvg = Object.freeze({NS, unsupported, bytes, encode, rootStyle, themeOnce, admit, asButton, textMeasurer});
})();
