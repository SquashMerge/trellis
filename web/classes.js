/* Cached syntax only. No source or provider request occurs on selection. */
// Pele draws measured class declarations only. Positions bind each native class
// node and member line to the original DTO object; no relation is ever drawn.
(() => {
  "use strict";
  const SVG_NS = "http://www.w3.org/2000/svg";
  const ROOT_STYLE = "--_bg:var(--pele-bg,#fff);--_fg:var(--pele-fg,#1f1f1f);--_m:var(--pele-muted,#6e6e6e);--_l:var(--pele-line,#1f1f1f);--_s:var(--pele-surface,#f3f3f3);--_a:var(--pele-surface-alt,#e6e6e6);--_b:var(--pele-border,#8a8a8a);--_c:var(--pele-accent,#5b7bd5);color:var(--_fg);--_r:var(--pele-radius,4px);--_hw:var(--pele-heading-weight,bold);--_fm:var(--pele-font-mono,monospace);font:14px sans-serif;letter-spacing:normal;word-spacing:normal;text-transform:none;direction:ltr;font-family:var(--pele-font,sans-serif)";
  const tags = new Set(["svg", "g", "path", "rect", "text", "tspan"]);
  const attrs = new Set(["xmlns", "class", "viewBox", "width", "height", "fill", "role", "aria-roledescription", "transform", "stroke", "font-size", "data-id", "d", "x", "y", "rx", "text-anchor", "font-weight", "xml:space"]);
  // Bounds match the card view: 24 classes and 200 members per compartment.
  // Pele itself rejects sources over 50,000 characters.
  const MAX_CLASSES = 24, MAX_MEMBERS = 200, MAX_SOURCE = 50000, LABEL = 60, MEMBER = 96;
  function unsupported(reason) { const error = new Error(reason); error.code = "unsupported"; throw error; }
  // Pele decodes numeric entities into SVG text. Escape everything outside a
  // small literal alphabet so repository text cannot become Mermaid grammar.
  const encode = value => Array.from(String(value)).map(c => /[A-Za-z0-9 ._-]/.test(c) ? c : `#${c.codePointAt(0)};`).join("");
  // Display text only: control characters and whitespace runs become one space.
  const tidy = value => String(value).replace(/[\u0000-\u001f\u007f\s]+/g, " ").trim();
  const clip = (value, max) => { const chars = Array.from(value); return chars.length > max ? chars.slice(0, max - 1).join("") + "…" : value; };
  const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
  function witness(item) {
    const r = item?.range;
    return typeof item?.path === "string" && !!item.path.trim() && !!r &&
      Number.isSafeInteger(r.startLine) && r.startLine > 0 && Number.isSafeInteger(r.endLine) && r.endLine >= r.startLine;
  }
  function project(nodes, options = {}) {
    if (!Array.isArray(nodes) || nodes.length > MAX_CLASSES) unsupported("Too many classes for Pele");
    const open = options.membersOpen || new Set(), seen = new Set(), classes = [], lines = ["classDiagram"];
    for (const [index, node] of nodes.entries()) {
      const definition = node?.class, symbol = definition?.symbol;
      if (typeof node?.id !== "string" || !node.id || seen.has(node.id) || !symbol || symbol.id !== node.id ||
          typeof symbol.name !== "string" || !witness(symbol) || !Array.isArray(definition.fields) || !Array.isArray(definition.methods)) unsupported("Unsupported class declaration");
      seen.add(node.id);
      const alias = `C${index}`, label = clip(tidy(symbol.name), LABEL);
      if (!label) unsupported("Unsupported class name");
      const fields = definition.fields, methods = definition.methods;
      const annotation = tidy([definition.language, definition.declarationKind, node.id === options.seed ? "focus" : null,
        plural(fields.length, "field"), plural(methods.length, "method")].filter(Boolean).join(" · "));
      const members = [];
      if (open.has(node.id)) {
        for (const [kind, list] of [["field", fields], ["method", methods]]) {
          for (const member of list.slice(0, MAX_MEMBERS)) {
            const name = clip(tidy(member?.name ?? ""), MEMBER), type = clip(tidy(member?.typeHint ?? ""), MEMBER);
            if (!name) unsupported("Unsupported class member");
            // A literal "()" makes Pele draw a method; every other character is encoded.
            const raw = kind === "method" ? `${encode(name)}()${type ? " " + encode(type) : ""}` : encode(type ? `${name}: ${type}` : name);
            const text = kind === "method" ? `${name}()${type ? " : " + type : ""}` : type ? `${name}: ${type}` : name;
            members.push({kind, member, raw, text});
          }
        }
      }
      lines.push(`class ${alias}["${encode(label)}"] {`, `<<${encode(annotation)}>>`, ...members.map(item => item.raw), "}");
      const omitted = Math.max(0, fields.length - MAX_MEMBERS) + Math.max(0, methods.length - MAX_MEMBERS);
      classes.push({alias, node, label, annotation, members, open:open.has(node.id), omitted});
    }
    const source = lines.join("\n") + "\n";
    if (source.length > MAX_SOURCE) unsupported("Class Mermaid source too large");
    return {source, classes};
  }
  function validatedSvg(raw, projection) {
    if (typeof raw !== "string" || new TextEncoder().encode(raw).length > 8 * 1024 * 1024) unsupported("Class SVG output too large");
    const start = /^<svg xmlns="http:\/\/www\.w3\.org\/2000\/svg" class="pele pele-class"/;
    const signature = ` style="${ROOT_STYLE}"`;
    if (!start.test(raw) || raw.split(signature).length !== 2) unsupported("Changed Pele class root/theme");
    const safe = raw.replace(signature, "").replace(start, '<svg xmlns="http://www.w3.org/2000/svg" class="pele pele-class trellis-pele"')
      .replaceAll(' style="rx:var(--_r)"', ' rx="var(--_r)"');
    if (/\sstyle\s*=|<style\b/i.test(safe)) unsupported("Unexpected inline class style");
    // Reject active/resource markup before DOM parsing.
    if (/<\/?(?:script|foreignObject|image|a)\b/i.test(safe)) unsupported("Unexpected class SVG element");
    const root = new DOMParser().parseFromString(safe, "image/svg+xml").documentElement;
    if (root.localName !== "svg" || root.namespaceURI !== SVG_NS || root.querySelector("parsererror")) unsupported("Invalid class SVG");
    let count = 0;
    for (const node of [root, ...root.querySelectorAll("*")]) {
      if (++count > 12000 || node.namespaceURI !== SVG_NS || !tags.has(node.localName)) unsupported("Unexpected class SVG element");
      for (const a of node.attributes) if (!attrs.has(a.name) || /url\s*\(|javascript:|<|>|[\u0000-\u001f]/i.test(a.value)) unsupported("Unexpected class SVG attribute");
    }
    const groups = [...root.querySelectorAll(".pele-node")];
    if (groups.length !== projection.classes.length || root.querySelectorAll("[data-id]").length !== projection.classes.length ||
        root.querySelectorAll(".pele-edge, .pele-edge-label, .pele-cluster").length) unsupported("Missing, extra or related Pele class node");
    const controls = [];
    for (const [i, item] of projection.classes.entries()) {
      const group = groups[i];
      if (group.getAttribute("data-id") !== item.alias || group.getAttribute("class") !== "pele-node pele-class") unsupported("Changed Pele class identity or order");
      const annotations = group.querySelectorAll(":scope > .pele-annotation"), labels = group.querySelectorAll(":scope > .pele-label");
      if (annotations.length !== 1 || annotations[0].textContent !== `«${item.annotation}»`) unsupported("Changed Pele class annotation");
      if (labels.length !== 1 || labels[0].textContent.replace(/\s+/g, "") !== item.label.replace(/\s+/g, "")) unsupported("Changed Pele class label");
      const members = [...group.querySelectorAll(".pele-member")];
      if (members.length !== item.members.length || group.querySelectorAll("text").length !== 2 + members.length) unsupported("Changed Pele class members");
      for (const [j, expected] of item.members.entries()) {
        const kind = expected.kind === "method" ? "pele-method" : "pele-attribute";
        if (members[j].getAttribute("class") !== `pele-member ${kind}` || members[j].textContent !== expected.text) unsupported("Changed Pele member identity or text");
      }
      controls.push({group, item, members:members.map((node, j) => ({node, ...item.members[j]}))});
    }
    return {root, controls};
  }
  // Returns the validated, unattached SVG and its native-node → DTO mapping.
  // The caller owns actions, guards and installation.
  function render(nodes, options = {}) {
    try {
      const projection = project(nodes, options);
      if (typeof window.pele?.render !== "function") unsupported("Pele asset unavailable");
      const raw = window.pele.render(projection.source, {fontSize:14, responsive:false, links:false, images:false}).svg;
      const {root, controls} = validatedSvg(raw, projection);
      for (const {group, item, members} of controls) {
        group.setAttribute("data-trellis-class-id", item.node.id);
        if (item.node.id === options.seed) group.classList.add("trellis-class-seed");
        members.forEach(({node, kind}, j) => { node.setAttribute("data-trellis-member-kind", kind); node.setAttribute("data-trellis-member-index", String(j)); });
      }
      return {status:"rendered", root, controls, source:projection.source};
    } catch (error) {
      return {status:"unsupported", reason:String(error?.message || error)};
    }
  }
  window.TrellisPeleClasses = Object.freeze({project, validatedSvg, render});
})();
(function () {
  "use strict";
  const MAX_NODES = 24;
  const WIDTH = 300, HEIGHT = 116, OPEN_HEIGHT = 390, STEP_X = 520;
  let api, ui, serial = 0, searchSerial = 0, seed = null, expanded = [], diagram = null;
  let positions = new Map(), cards = new Map(), stage = null, menu = null, menuAnchor = null, warningBox = null;
  let menuContext = null, menuExpanded = null, menuOnClose = null, menuScroll = new Map();
  let bound = false, snapshot = null, diagramGeneration = 0, displayTicket = 0;
  let controls, changeButton, allButton, chooser = null, showAll = false, membersOpen = new Set();
  // "pele" or "cards". Cards remain the visible fallback when Pele cannot
  // represent the returned declarations faithfully (Decision 0014 §2).
  let renderer = null, rendererNote = "";
  const currentView = () => snapshot && valid(snapshot) && displayTicket === serial;
  function closeChooser() { if (chooser) chooser.remove(); chooser = null; }
  function selectedHierarchy(data) {
    return new Set([seed, ...expanded].filter(id => data.nodes.some(node => node.id === id && node.class)));
  }
  function visibleNodes(data) {
    const selected = selectedHierarchy(data);
    return data.nodes.slice(0, MAX_NODES).filter(node => node.class && (showAll || selected.has(node.id)));
  }
  function updateStatus() {
    const nodes = visibleNodes(diagram);
    const partial = diagram.truncated || diagram.nodes.length > MAX_NODES;
    state(`${nodes.length} measured declarations shown · no proven class relationships.${partial ? " Partial diagram; some index or diagram details are omitted. See notices." : ""}${diagram.requireIndex ? " Index workspace to populate class declarations." : ""}${rendererNote}`, diagram.requireIndex ? "unindexed" : partial ? "partial" : "ready");
    controls.hidden = false;
    allButton.textContent = showAll ? "Show selected classes" : "Show all returned classes";
    allButton.setAttribute("aria-pressed", String(showAll));
  }
  const el = (tag, text, cls) => {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (cls) node.className = cls;
    return node;
  };
  const svg = (tag, attrs, text) => {
    const node = document.createElementNS("http://www.w3.org/2000/svg", tag);
    for (const [key, value] of Object.entries(attrs || {})) node.setAttribute(key, value);
    if (text !== undefined) node.textContent = text;
    return node;
  };
  const context = () => ({session: api.currentSession(), revision: window.TrellisIndexPin.copy(api.currentRevision())});
  const valid = c => !!api && c.session === api.currentSession() && window.TrellisIndexPin.equal(c.revision, api.currentRevision());
  const state = (text, kind = "ready") => { ui.state.textContent = text; ui.state.dataset.state = kind; };
  function renderWarnings(warnings = []) {
    if (!warningBox) {
      warningBox = el("details", undefined, "classes-warnings");
      ui.diagram.before(warningBox);
    }
    warningBox.replaceChildren(); warningBox.open = false; warningBox.hidden = !warnings.length;
    if (!warnings.length) return;
    warningBox.append(el("summary", `${warnings.length} indexing / diagram ${warnings.length === 1 ? "notice" : "notices"}`));
    const list = el("ul");
    for (const warning of warnings.slice(0, 50)) {
      const message = String(warning);
      list.append(el("li", message.length > 2000 ? message.slice(0, 2000) + "… (notice shortened)" : message));
    }
    if (warnings.length > 50) list.append(el("li", `${warnings.length - 50} more notices omitted from this summary.`));
    warningBox.append(list);
  }
  function closeContextMenu(restore = true, reason = "dismiss") {
    const closing = menu, anchor = menuAnchor, expanded = menuExpanded, onClose = menuOnClose;
    // Clear ownership before DOM, callbacks or focus can re-enter dismissal.
    menu = menuAnchor = menuContext = menuOnClose = null; menuExpanded = null; menuScroll.clear();
    if (closing) closing.remove();
    if (anchor) anchor.setAttribute("aria-expanded", expanded ?? "false");
    if (onClose) onClose(reason);
    if (restore && !menu && anchor && anchor.isConnected) anchor.focus({preventScroll: true});
  }
  async function invoke(run, c) {
    if (!valid(c)) { closeContextMenu(false); return; }
    try { await run(); }
    catch (error) { if (valid(c) && error.name !== "AbortError") state(error.message || "Action unavailable.", "error"); }
  }
  // Public menu actions: [{label: string, run: () => void|Promise, disabled?: boolean}].
  // The event's currentTarget is the focus-return anchor. Root must reset on session/revision changes.
  // Optional onClose(reason): "dismiss", "replace" or "action"; called once.
  // The returned handle can dismiss only the menu created by this call.
  function showContextMenu(event, actions, options = {}) {
    event.preventDefault(); event.stopPropagation();
    closeContextMenu(false, "replace");
    menuAnchor = event.currentTarget || event.target || document.activeElement;
    menuExpanded = menuAnchor.getAttribute("aria-expanded");
    // A queued scroll event from bringing the trigger into view must not close
    // the newly opened menu. Actual subsequent scrolling still dismisses it.
    for (let ancestor = menuAnchor, depth = 0; ancestor && depth < 64; ancestor = ancestor.parentNode, depth++) {
      if (typeof ancestor.scrollTop === "number" && typeof ancestor.scrollLeft === "number") {
        menuScroll.set(ancestor, [ancestor.scrollTop, ancestor.scrollLeft]);
      }
    }
    menuContext = context();
    const c = menuContext;
    menu = el("div", undefined, "classes-context-menu");
    const createdMenu = menu;
    menuOnClose = typeof options.onClose === "function" ? options.onClose : null;
    menu.setAttribute("role", "menu"); menu.setAttribute("aria-label", "Class actions");
    menuAnchor.setAttribute("aria-expanded", "true");
    const buttons = [];
    for (const action of actions) {
      const item = el("button", action.label);
      item.type = "button"; item.setAttribute("role", "menuitem"); item.tabIndex = -1;
      item.disabled = !!action.disabled;
      item.addEventListener("click", () => {
        if (menu !== createdMenu) return;
        closeContextMenu(true, "action"); return invoke(action.run, c);
      });
      menu.append(item); if (!item.disabled) buttons.push(item);
    }
    menu.addEventListener("keydown", event => {
      if (menu !== createdMenu) return;
      if (!valid(c)) { closeContextMenu(false); return; }
      if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); closeContextMenu(); return; }
      if (event.key === "Tab") { closeContextMenu(); return; }
      const index = buttons.indexOf(document.activeElement);
      let next;
      if (event.key === "ArrowDown") next = (index + 1) % buttons.length;
      if (event.key === "ArrowUp") next = (index - 1 + buttons.length) % buttons.length;
      if (event.key === "Home") next = 0;
      if (event.key === "End") next = buttons.length - 1;
      if (next !== undefined && buttons[next]) { event.preventDefault(); event.stopPropagation(); buttons[next].focus(); }
    });
    document.body.append(menu);
    const rect = menuAnchor.getBoundingClientRect();
    const keyboard = event.type === "keydown" || (!event.clientX && !event.clientY);
    const x = keyboard ? rect.left : event.clientX, y = keyboard ? rect.bottom : event.clientY;
    const bounds = menu.getBoundingClientRect();
    menu.style.left = `${Math.max(8, Math.min(x, window.innerWidth - bounds.width - 8))}px`;
    menu.style.top = `${Math.max(8, Math.min(y, window.innerHeight - bounds.height - 8))}px`;
    if (buttons[0]) buttons[0].focus({preventScroll: true});
    return {close: () => { if (menu === createdMenu) closeContextMenu(); }};
  }
  function listenMenu(node, actions) {
    node.setAttribute("aria-haspopup", "menu");
    node.addEventListener("contextmenu", event => showContextMenu(event, actions()));
    node.addEventListener("keydown", event => {
      if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) showContextMenu(event, actions());
    });
  }
  function reset() {
    if (api && api.onChange) api.onChange();
    serial++; searchSerial++; diagramGeneration++; closeContextMenu(false);
    seed = null; expanded = []; diagram = null; snapshot = null; showAll = false; membersOpen.clear(); closeChooser();
    if (controls) controls.hidden = true;
    if (ui) ui.results.hidden = false;
    positions = new Map(); cards = new Map(); stage = null;
    if (!ui) return;
    ui.results.replaceChildren(); ui.diagram.replaceChildren(); renderWarnings();
    ui.diagram.setAttribute("aria-busy", "false");
    ui.query.value = ""; ui.unmatched.checked = false;
    state("Choose a Java or Python class, or search the indexed workspace.", "empty");
  }
  function init(options) {
    api = options;
    ui = Object.fromEntries(["panel", "state", "query", "search", "results", "diagram", "unmatched"].map(key => [key, document.getElementById(`classes-${key}`)]));
    ui.state.setAttribute("role", "status"); ui.state.setAttribute("aria-live", "polite");
    ui.diagram.setAttribute("tabindex", "0"); ui.diagram.setAttribute("aria-label", "Class diagram. Scroll to explore. Class actions are available with Shift F10.");
    if (!bound) {
      bound = true;
      controls = el("div", undefined, "classes-controls");
      changeButton = el("button", "Change class"); allButton = el("button", "Show all returned classes");
      changeButton.type = allButton.type = "button";
      changeButton.setAttribute("aria-controls", "classes-results");
      changeButton.addEventListener("click", () => {
        ui.results.hidden = !ui.results.hidden;
        changeButton.setAttribute("aria-expanded", String(!ui.results.hidden));
        if (!ui.results.hidden) ui.query.focus();
      });
      allButton.addEventListener("click", () => {
        if (!currentView()) return;
        closeChooser(); showAll = !showAll; render(diagram); updateStatus();
      });
      controls.append(changeButton, allButton); ui.diagram.before(controls);
      ui.search.addEventListener("click", () => lookup({q: ui.query.value.trim()}));
      ui.query.addEventListener("keydown", event => {
        if (event.key === "Enter") { event.preventDefault(); lookup({q: ui.query.value.trim()}); }
      });
      ui.unmatched.addEventListener("change", () => { if (seed) return loadDiagram(seed, expanded); });
      document.addEventListener("pointerdown", event => { if (menu && !menu.contains(event.target)) closeContextMenu(false); }, true);
      document.addEventListener("focusin", event => { if (menu && (!valid(menuContext) || !menu.contains(event.target))) closeContextMenu(false); });
      document.addEventListener("scroll", event => {
        if (!menu || menu.contains(event.target)) return;
        const previous = menuScroll.get(event.target);
        if (!previous || (previous[0] === event.target.scrollTop && previous[1] === event.target.scrollLeft)) return;
        closeContextMenu();
      }, true);
      window.addEventListener("resize", () => closeContextMenu());
    }
    reset();
  }
  function unsupported(path) { return path && !/\.(java|py|pyi)$/i.test(path); }
  async function open(options = {}) {
    if (!ui) return;
    closeContextMenu(false);
    if (unsupported(options.path)) {
      reset(); state("Class diagrams currently support Java and Python declarations. Sequence browsing is unchanged.", "unsupported"); return;
    }
    if (options.seed) {
      searchSerial++; ui.results.replaceChildren();
      return loadDiagram(options.seed, [], true);
    }
    return lookup({path: options.path || "", q: "", autoOpen: !!options.path});
  }
  async function lookup({path = "", q = "", offset = 0, autoOpen = false} = {}) {
    const c = context(), ticket = ++searchSerial;
    // Lookup supersedes a pending diagram request. Restore the visible diagram's
    // action ticket; the old response is still rejected by searchSerial.
    if (diagram && snapshot && valid(snapshot)) displayTicket = serial;
    if (api.onChange) api.onChange();
    ui.diagram.setAttribute("aria-busy", "false");
    // Keep the current diagram and its callbacks usable while lookup is pending.
    // A failed non-revision request must not discard the only loaded view.
    state("Looking for indexed classes…", "loading");
    try {
      const params = new URLSearchParams({q, offset: String(offset), limit: "100", indexGeneration: c.revision.indexGeneration, indexRevision: String(c.revision.indexRevision)});
      if (path) params.set("path", path);
      const data = await api.request(`/api/classes?${params}`);
      if (ticket !== searchSerial || !valid(c)) return;
      if (!window.TrellisIndexPin.equal(data.revision, c.revision)) {
        reset(); api.onStale?.("Workspace revision changed. Search again.");
        state("Workspace revision changed. Search again.", "stale"); return;
      }
      serial++; closeContextMenu(false); closeChooser(); ui.results.hidden = false;
      controls.hidden = true;
      if (!offset) {
        showAll = false; membersOpen.clear(); expanded = [];
        seed = null; diagram = null; snapshot = null; diagramGeneration++; ui.diagram.replaceChildren();
        stage = null; cards = new Map(); positions = new Map(); ui.results.replaceChildren();
      }
      ui.diagram.setAttribute("aria-busy", "false");
      const items = data.items || [];
      for (const definition of items) {
        const button = el("button", undefined, "classes-result"); button.type = "button";
        button.append(el("span", definition.qualifiedName || definition.symbol.name), el("span", definition.symbol.path, "classes-path"));
        button.addEventListener("click", () => { if (valid(c)) return loadDiagram(definition.symbol.id, [], true); });
        ui.results.append(button);
      }
      if (data.nextOffset != null) {
        const more = el("button", "More classes", "classes-more"); more.type = "button";
        more.addEventListener("click", () => { if (!valid(c)) return; more.remove(); return lookup({path, q, offset: data.nextOffset}); });
        ui.results.append(more);
      }
      const warnings = data.warnings || [];
      const unindexed = data.requireIndex || warnings.some(warning => /index workspace/i.test(warning));
      renderWarnings(warnings);
      state(`${unindexed ? "Index workspace to populate class declarations." : items.length ? "Choose a class to show its declared relationships." : "No matching classes. Try another name or index Java / Python files."}${data.truncated ? " Results are partial." : ""}`, unindexed ? "unindexed" : data.truncated ? "partial" : items.length ? "ready" : "empty");
      if (autoOpen && items.length) await loadDiagram(items[0].symbol.id, [], true);
    } catch (error) {
      if (ticket === searchSerial && valid(c) && error.name !== "AbortError") {
        const conflict = window.TrellisIndexPin.isConflict(error);
        if (conflict) { reset(); api.onStale?.("Workspace revision changed. Search again."); }
        state(error.message || "Class lookup failed. Try Search again.", conflict ? "stale" : "error");
      }
    }
  }
  async function loadDiagram(nextSeed, nextExpanded, fresh = false, focusId = null) {
    // A chosen diagram owns status/notices; late catalog pages must not replace them.
    const searchTicket = ++searchSerial;
    const c = context(), ticket = ++serial, focusBefore = document.activeElement;
    const restoreFocus = () => {
      if ((!fresh && !focusId) || ticket !== serial || !valid(c) || displayTicket !== ticket) return;
      // Do not pull focus back after the user has moved to another control while loading.
      if (document.activeElement !== document.body && document.activeElement !== focusBefore) return;
      const anchor = cards.get(focusId || seed);
      if (anchor && anchor.isConnected) anchor.focus({preventScroll: true});
    };
    if (api.onChange) api.onChange();
    closeContextMenu(false); closeChooser(); ui.diagram.setAttribute("aria-busy", "true");
    state("Loading measured class declarations…", "loading");
    const expansion = [...nextExpanded];
    try {
      const data = await api.request("/api/class-diagram", {method: "POST", body: {seed: nextSeed, expectedRevision: c.revision, expanded: expansion, includeHierarchy: false, includeUnmatched: false}});
      if (ticket !== serial || searchTicket !== searchSerial || !valid(c)) return;
      if (!window.TrellisIndexPin.equal(data.revision, c.revision)) {
        reset(); api.onStale?.("Workspace revision changed. Open the class again.");
        state("Workspace revision changed. Open the class again.", "stale"); return;
      }
      if (fresh) { positions = new Map(); cards = new Map(); stage = null; showAll = false; membersOpen.clear(); ui.diagram.replaceChildren(); }
      ui.results.hidden = true; changeButton.setAttribute("aria-expanded", "false");
      seed = data.seed; expanded = expansion; diagram = data; snapshot = c; diagramGeneration++; displayTicket = ticket;
      render(data); renderWarnings(data.warnings || []);
      updateStatus(); restoreFocus();
    } catch (error) {
      if (ticket === serial && searchTicket === searchSerial && valid(c) && error.name !== "AbortError") {
        const conflict = window.TrellisIndexPin.isConflict(error);
        const recoverable = diagram && snapshot && valid(snapshot) && ![401, 403].includes(error.status) && !conflict;
        if (recoverable) {
          // This request never published a new view. Restore only the still-current cached view.
          displayTicket = ticket;
          state(`${error.message || "Class diagram unavailable."} Previous diagram retained. Its actions are available; retry the class action.`, "error");
          restoreFocus();
        } else {
          diagramGeneration++; diagram = null; snapshot = null; seed = null; expanded = [];
          stage = null; cards = new Map(); positions = new Map(); controls.hidden = true; ui.diagram.replaceChildren();
          if (conflict) api.onStale?.("Workspace revision changed. Refresh status and open the class again.");
          state(`${error.message || "Class diagram unavailable."} ${conflict ? "Workspace revision changed. Refresh status and open the class again." : "Try opening the class again."}`, conflict ? "stale" : "error");
        }
      }
    } finally { if (ticket === serial && searchTicket === searchSerial && valid(c)) ui.diagram.setAttribute("aria-busy", "false"); }
  }
  function classActions(node) {
    const c = snapshot, generation = diagramGeneration;
    const guarded = action => () => { if (valid(c) && generation === diagramGeneration && displayTicket === serial) return action(); };
    return [
      {label: "Read class source", run: guarded(() => api.readSource({path: node.class.symbol.path, range: {...node.class.symbol.range}}, c.revision))},
      {label: "Focus this class", run: guarded(() => loadDiagram(node.id, [], true))}
    ];
  }
  // Member intents shared by both renderers. `anchor` is the rendered member
  // row; its connection proves the action is still on screen.
  function memberIntents(definition, member, method, c, generation, anchor) {
    const label = `${member.name}${method ? "()" : ""}${member.typeHint ? ": " + member.typeHint : ""}`;
    const select = method && member.symbolId ? () => {
      if (!valid(c) || generation !== diagramGeneration || displayTicket !== serial || ui.panel.hidden) return;
      return invoke(() => api.selectMethod({id: member.symbolId, name: member.name, kind: "method", path: member.path, range: {...member.range}, parent: definition.symbol.id, accessor: false, provenance: definition.symbol.provenance}), c);
    } : null;
    const canNavigate = typeof api.navigateMember === "function";
    const navigate = event => {
      // Stop here synchronously: a member intent must not also open its class menu.
      event.preventDefault(); event.stopPropagation();
      const ticket = displayTicket;
      const isCurrent = () => snapshot === c && valid(c) && generation === diagramGeneration &&
        ticket === displayTicket && ticket === serial && !ui.panel.hidden && anchor.isConnected &&
        membersOpen.has(definition.symbol.id);
      if (!canNavigate || !isCurrent()) return;
      // Names and type hints are display text, never a global search selector.
      return api.navigateMember(event, {classId: definition.symbol.id, memberName: member.name,
        startByte: member.range.startByte, endByte: member.range.endByte}, {isCurrent});
    };
    return {label, select, navigate, canNavigate};
  }
  function compartment(definition, name, members, c, generation) {
    const section = el("section", undefined, "classes-compartment");
    section.append(el("h4", `${name} (${members.length})`));
    const list = el("div", undefined, "classes-members");
    list.tabIndex = 0; list.setAttribute("aria-label", `${name} of ${definition.symbol.name}`);
    for (const member of members.slice(0, 200)) {
      const row = el("div", undefined, "classes-member");
      const {label, select, navigate, canNavigate} = memberIntents(definition, member, name === "Methods", c, generation, row);
      row.title = label;
      const memberName = el(select ? "button" : "span", `${member.name}${name === "Methods" ? "()" : ""}`, "classes-member-name");
      if (select) {
        memberName.type = "button"; memberName.setAttribute("aria-label", `Show sequence for ${label}`);
        memberName.addEventListener("click", select);
      }
      row.append(memberName);
      row.addEventListener("contextmenu", navigate);
      row.addEventListener("keydown", event => {
        if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) return navigate(event);
      });
      if (member.typeHint) {
        row.append(el("span", ": ", "classes-member-separator"));
        const type = el(canNavigate ? "button" : "span", member.typeHint, "classes-member-type");
        if (canNavigate) {
          type.type = "button"; type.setAttribute("aria-haspopup", "menu");
          type.setAttribute("aria-label", `Navigate declared type ${member.typeHint} for ${member.name}`);
          type.addEventListener("click", navigate);
        }
        row.append(type);
      }
      if (canNavigate) {
        row.tabIndex = -1; row.setAttribute("aria-haspopup", "menu");
        const actions = el("button", "⋯", "classes-member-menu"); actions.type = "button";
        actions.setAttribute("aria-label", `Navigate member ${label}`); actions.setAttribute("aria-haspopup", "menu");
        actions.addEventListener("click", navigate); row.append(actions);
      }
      list.append(row);
    }
    if (!members.length) list.append(el("span", "None declared", "classes-empty-member"));
    if (members.length > 200) list.append(el("span", "First 200 members shown. Read source for more.", "classes-empty-member"));
    section.append(list); return section;
  }
  const dataIndex = id => diagram.nodes.findIndex(node => node.id === id);
  function card(node) {
    const article = el("article", undefined, `classes-node${node.class ? "" : " classes-hint"}${node.id === seed ? " classes-seed" : ""}`);
    article.dataset.classId = node.id; article.tabIndex = 0;
    article.setAttribute("aria-label", `${node.label}${node.class ? ", class actions available" : ", terminal " + node.kind + " type hint"}`);
    const header = el("header", undefined, "classes-node-header");
    const title = el("div", undefined, "classes-node-title");
    const fullName = node.class ? node.class.qualifiedName || node.label : node.label;
    title.append(el("span", node.class ? `${node.class.language} · ${node.class.declarationKind}${node.id === seed ? " · focus" : ""}` : `${node.kind} · terminal hint`, "classes-kind"), el("h3", node.class ? node.class.symbol.name : node.label));
    if (node.class) {
      title.title = `${fullName} · ${node.class.symbol.path}`;
      article.setAttribute("aria-label", `${fullName}, class actions available`);
    }
    header.append(title); article.append(header);
    if (node.class) {
      const menuButton = el("button", "⋯", "classes-menu-button"); menuButton.type = "button";
      menuButton.setAttribute("aria-label", `Actions for ${node.label}`); menuButton.setAttribute("aria-haspopup", "menu");
      menuButton.addEventListener("click", event => showContextMenu(event, classActions(node)));
      header.append(menuButton); listenMenu(article, () => classActions(node));
      const fields = node.class.fields || [], methods = node.class.methods || [];
      const reveal = el("button", `Members · ${fields.length} ${fields.length === 1 ? "field" : "fields"} · ${methods.length} ${methods.length === 1 ? "method" : "methods"}`, "classes-members-toggle");
      reveal.type = "button"; reveal.setAttribute("aria-expanded", String(membersOpen.has(node.id)));
      const content = el("div", undefined, "classes-member-detail");
      content.hidden = !membersOpen.has(node.id);
      const detailId = `classes-members-${dataIndex(node.id)}`;
      content.id = detailId; reveal.setAttribute("aria-controls", detailId);
      const qualified = el("span", fullName, "classes-qualified"); qualified.title = fullName;
      const path = el("div", node.class.symbol.path, "classes-node-path"); path.title = node.class.symbol.path;
      content.append(qualified, path, compartment(node.class, "Fields", fields, snapshot, diagramGeneration), compartment(node.class, "Methods", methods, snapshot, diagramGeneration));
      const c = snapshot, generation = diagramGeneration;
      reveal.addEventListener("click", () => {
        if (!valid(c) || generation !== diagramGeneration || displayTicket !== serial) return;
        // Collapsing can make the class representable by Pele again.
        toggleMembers(node.id);
      });
      article.append(reveal, content);
      if (node.class.truncated) article.append(el("div", "Partial declaration · read source for details", "classes-node-notice"));
    } else article.append(el("p", "No unique indexed class match. This hint cannot expand.", "classes-hint-note"));
    return article;
  }
  function relationshipLedger() {
    const ledger = el("details", undefined, "classes-relationships");
    ledger.append(el("summary", "No proven class relationships"), el("p", "Class declarations are shown without inferred hierarchy or type targets."));
    return ledger;
  }
  function toggleMembers(id) {
    if (!currentView()) return;
    const top = ui.diagram.scrollTop, left = ui.diagram.scrollLeft;
    if (membersOpen.has(id)) membersOpen.delete(id); else membersOpen.add(id);
    render(diagram); updateStatus();
    ui.diagram.scrollTop = top; ui.diagram.scrollLeft = left;
    const anchor = cards.get(id);
    (renderer === "pele" ? anchor : anchor?.querySelector(".classes-members-toggle"))?.focus({preventScroll: true});
  }
  // Pele owns layout only. Each native node and member line was validated
  // against its original DTO record; actions use those records, never SVG text.
  function renderPele(outcome) {
    const c = snapshot, generation = diagramGeneration;
    const guarded = action => () => { if (valid(c) && generation === diagramGeneration && displayTicket === serial) return action(); };
    renderer = "pele"; stage = null; positions = new Map(); cards = new Map();
    const notices = el("ul", undefined, "classes-pele-notices");
    for (const {group, item, members} of outcome.controls) {
      const node = item.node, definition = node.class, fullName = definition.qualifiedName || node.label;
      const counts = `${definition.fields.length} ${definition.fields.length === 1 ? "field" : "fields"} · ${definition.methods.length} ${definition.methods.length === 1 ? "method" : "methods"}`;
      group.setAttribute("role", "button"); group.setAttribute("tabindex", "0");
      group.setAttribute("aria-haspopup", "menu"); group.setAttribute("aria-expanded", "false");
      group.setAttribute("aria-label", `${fullName} · ${definition.symbol.path} · ${item.annotation} · members ${item.open ? "shown" : "hidden"}. Class actions available.`);
      const actions = () => [{label: item.open ? "Hide members" : `Show members · ${counts}`, run: guarded(() => toggleMembers(node.id))}, ...classActions(node)];
      const open = event => showContextMenu(event, actions());
      group.addEventListener("click", open);
      group.addEventListener("contextmenu", open);
      group.addEventListener("keydown", event => {
        if (event.target !== group) return;
        if (["Enter", " ", "ContextMenu"].includes(event.key) || (event.shiftKey && event.key === "F10")) open(event);
      });
      cards.set(node.id, group);
      for (const {node: line, kind, member} of members) {
        const {label, select, navigate, canNavigate} = memberIntents(definition, member, kind === "method", c, generation, line);
        if (!select && !canNavigate) continue;
        line.setAttribute("role", "button"); line.setAttribute("tabindex", "0");
        if (member.symbolId) line.setAttribute("data-trellis-symbol-id", member.symbolId);
        if (canNavigate) line.setAttribute("aria-haspopup", "menu");
        line.setAttribute("aria-label", select ? `Show sequence for ${label}` : `Navigate member ${label}`);
        const activate = event => { event.preventDefault(); event.stopPropagation(); return select ? select() : navigate(event); };
        line.addEventListener("click", activate);
        line.addEventListener("contextmenu", navigate);
        line.addEventListener("keydown", event => {
          if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) return navigate(event);
          if (event.key === "Enter" || event.key === " ") return activate(event);
        });
      }
      if (item.omitted) notices.append(el("li", `${fullName}: first 200 fields and first 200 methods shown; ${item.omitted} more omitted. Read class source for more.`));
      if (definition.truncated) notices.append(el("li", `${fullName}: partial declaration · read source for details.`));
    }
    ui.diagram.replaceChildren(outcome.root, ...(notices.children.length ? [notices] : []), relationshipLedger());
  }
  function render(data) {
    const nodes = visibleNodes(data), ids = new Set(nodes.map(node => node.id));
    const outcome = window.TrellisPeleClasses ? window.TrellisPeleClasses.render(nodes, {seed, membersOpen}) : {status: "unsupported", reason: "Pele asset unavailable"};
    if (outcome.status === "rendered") { rendererNote = " Pele rendered."; renderPele(outcome); return; }
    rendererNote = ` Pele fell back: ${outcome.reason}.`;
    if (renderer !== "cards") { ui.diagram.replaceChildren(); stage = null; positions = new Map(); cards = new Map(); }
    renderer = "cards";
    if (!stage) { stage = el("div", undefined, "classes-stage"); ui.diagram.append(stage); }
    // Reserve stable grid slots across expansions. No force simulation or DTO mutation.
    for (const id of positions.keys()) if (!ids.has(id)) positions.delete(id);
    const occupied = new Set(positions.values());
    for (const node of nodes) if (!positions.has(node.id)) {
      let slot = 0; while (occupied.has(slot)) slot++;
      positions.set(node.id, slot); occupied.add(slot);
    }
    for (const [id, item] of cards) if (!ids.has(id)) { item.remove(); cards.delete(id); }
    const maxSlot = Math.max(0, ...nodes.map(node => positions.get(node.id)));
    const rowTops = [24], rowHeights = [];
    for (let row = 0; row <= Math.floor(maxSlot / 2); row++) {
      rowHeights[row] = nodes.some(node => Math.floor(positions.get(node.id) / 2) === row && membersOpen.has(node.id)) ? OPEN_HEIGHT : HEIGHT;
      rowTops[row + 1] = rowTops[row] + rowHeights[row] + 100;
    }
    const width = nodes.length > 1 ? STEP_X + WIDTH + 100 : WIDTH + 48;
    const height = rowTops[rowTops.length - 1];
    stage.style.width = `${width}px`; stage.style.height = `${height}px`;
    const coords = id => { const slot = positions.get(id), row = Math.floor(slot / 2); return {x: 24 + (slot % 2) * STEP_X, y: rowTops[row], rowBottom: rowTops[row] + rowHeights[row], height: membersOpen.has(id) ? OPEN_HEIGHT : HEIGHT}; };
    const priorEdges = stage.querySelector(".classes-edges"); if (priorEdges) priorEdges.remove();
    const existingLedger = ui.diagram.querySelector(".classes-relationships"); if (existingLedger) existingLedger.remove();
    const ledger = relationshipLedger();
    for (const node of nodes) {
      const old = cards.get(node.id), fresh = card(node), pos = coords(node.id);
      // Preserve the actual DOM node identity and position, replacing only its contents.
      let item = fresh;
      if (old) { old.className = fresh.className; old.replaceChildren(...Array.from(fresh.children)); item = old; }
      else { cards.set(node.id, item); stage.append(item); }
      item.style.left = `${pos.x}px`; item.style.top = `${pos.y}px`; item.style.height = `${pos.height}px`;
    }
    ui.diagram.append(ledger);
  }
  window.TrellisClasses = Object.freeze({init, open, reset, showContextMenu, closeContextMenu});
})();
