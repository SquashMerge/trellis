/* Revision-bound navigation. Source reads require an explicit selected source action. */
(function () {
  "use strict";
  const MAX_TARGETS = 64, MAX_NOTICES = 8, MAX_TEXT = 700;
  let api, serial = 0, bound = false, ownedMenu = null;
  const sources = new Set(), menuScroll = new Map();
  const text = value => String(value == null ? "" : value).slice(0, MAX_TEXT);
  const positive = value => Number.isSafeInteger(value) && value > 0;
  const natural = value => Number.isSafeInteger(value) && value >= 0;
  function el(tag, content, className) {
    const node = document.createElement(tag);
    if (content !== undefined) node.textContent = content;
    if (className) node.className = className;
    return node;
  }
  function closeOwnedMenu() {
    const handle = ownedMenu; ownedMenu = null;
    handle?.close?.();
  }
  function reset() {
    closeOwnedMenu(); serial++; menuScroll.clear();
    for (const source of [...sources]) source.dispose();
  }
  function init(options) {
    reset(); api = options;
    if (!bound) {
      bound = true;
      // The shared menu owns focus and dismissal. Observe dismissal intent before
      // it stops propagation, so a pending lookup cannot reopen an escaped menu.
      document.addEventListener("keydown", event => {
        if (event.key === "Escape" || event.key === "Tab") serial++;
      }, true);
      document.addEventListener("pointerdown", event => {
        if (!event.target?.closest?.(".classes-context-menu")) serial++;
      }, true);
      document.addEventListener("scroll", event => {
        if (event.target?.closest?.(".classes-context-menu")) return;
        const previous = menuScroll.get(event.target);
        if (!previous || (previous[0] === event.target.scrollTop && previous[1] === event.target.scrollLeft)) return;
        serial++;
      }, true);
      window.addEventListener("resize", () => { serial++; });
    }
  }
  function capture(event) {
    event?.preventDefault?.(); event?.stopPropagation?.();
    const anchor = event?.currentTarget || event?.target || document.activeElement || document.body;
    const rect = anchor.getBoundingClientRect();
    const keyboard = event?.type === "keydown" || (!event?.clientX && !event?.clientY);
    // Native currentTarget is cleared after dispatch. Keep the real focus anchor and
    // coordinates now, not after the request completes.
    return {type: "contextmenu", currentTarget: anchor, target: anchor,
      clientX: keyboard ? rect.left : event.clientX,
      clientY: keyboard ? rect.bottom : event.clientY,
      preventDefault() {}, stopPropagation() {}};
  }
  function selectorBody(selector, revision) {
    if (!selector || typeof selector !== "object" || Array.isArray(selector)) return null;
    const source = Object.prototype.hasOwnProperty.call(selector, "path");
    const allowed = source ? ["expectedRevision", "path", "line"] : ["expectedRevision", "classId", "memberName", "startByte", "endByte"];
    if (Object.keys(selector).some(key => !allowed.includes(key))) return null;
    if (selector.expectedRevision !== undefined && !window.TrellisIndexPin.equal(selector.expectedRevision, revision)) return null;
    if (source) {
      if (typeof selector.path !== "string" || !selector.path || selector.path.length > 4096 || !positive(selector.line)) return null;
      return {expectedRevision: revision, path: selector.path, line: selector.line};
    }
    if (typeof selector.classId !== "string" || !selector.classId || selector.classId.length > 4096 ||
        typeof selector.memberName !== "string" || !selector.memberName || selector.memberName.length > 4096 ||
        !natural(selector.startByte) || !natural(selector.endByte) || selector.endByte < selector.startByte) return null;
    return {expectedRevision: revision, classId: selector.classId, memberName: selector.memberName,
      startByte: selector.startByte, endByte: selector.endByte};
  }
  function sourceWitness(item) {
    const r = item?.range;
    return typeof item?.path === "string" && !!item.path.trim() && !!r &&
      Number.isSafeInteger(r.startLine) && r.startLine > 0 &&
      Number.isSafeInteger(r.endLine) && r.endLine >= r.startLine &&
      (r.startColumn == null || (Number.isSafeInteger(r.startColumn) && r.startColumn > 0)) &&
      (r.endColumn == null || (Number.isSafeInteger(r.endColumn) && r.endColumn > 0)) &&
      (r.startLine !== r.endLine || r.startColumn == null || r.endColumn == null || r.endColumn >= r.startColumn) &&
      (r.startByte == null || (Number.isSafeInteger(r.startByte) && r.startByte >= 0)) &&
      (r.endByte == null || (Number.isSafeInteger(r.endByte) && r.endByte > (r.startByte ?? -1)));
  }
  async function open(event, selector, {isCurrent} = {}) {
    const anchor = capture(event), owner = api, ticket = ++serial;
    menuScroll.clear();
    for (let node = anchor.currentTarget, depth = 0; node && depth < 64; node = node.parentNode, depth++) {
      if (typeof node.scrollTop === "number" && typeof node.scrollLeft === "number") menuScroll.set(node, [node.scrollTop, node.scrollLeft]);
    }
    if (!owner) return;
    const revision = window.TrellisIndexPin.copy(owner.currentRevision()), session = owner.currentSession();
    const current = () => {
      try { return owner === api && ticket === serial && window.TrellisIndexPin.equal(revision, owner.currentRevision()) &&
        session === owner.currentSession() && (!isCurrent || isCurrent()); }
      catch (_) { return false; }
    };
    if (!current()) return;
    let replacing = false;
    const display = actions => {
      if (!current()) return;
      replacing = true;
      try {
        ownedMenu = owner.showMenu(anchor, actions, {onClose(reason) {
          // Our own loading-to-results replacement keeps this lookup alive.
          // Another caller's menu, dismissal, or programmatic focus does not.
          if (ticket === serial && reason !== "action" && !replacing) serial++;
        }});
      } finally { replacing = false; }
    };
    const close = {label: "Close navigation", run: () => { if (current()) serial++; }};
    const notice = label => ({label: text(label), disabled: true, run() {}});
    const body = selectorBody(selector, revision);
    const retry = {label: "Try navigation again", run: () => { if (current()) return open(anchor, body, {isCurrent}); }};
    if (!body) { display([notice("Navigation unavailable: select a current indexed line or member."), close]); return; }
    display([notice("Finding cached navigation targets…"), close]);
    // Focus dismissal and another non-navigation menu do not necessarily produce
    // pointer/keyboard events. A reply may replace only the loading menu it owns.
    // Do not use connectivity in action guards: the shared helper intentionally
    // closes its menu and restores focus before calling an action.
    const loadingMenu = document.querySelector(".classes-context-menu");
    const mayPublish = () => current() && (!loadingMenu ||
      (loadingMenu.isConnected && document.querySelector(".classes-context-menu") === loadingMenu));
    try {
      const data = await owner.request("/api/navigation", {method: "POST", body});
      if (!mayPublish()) return;
      if (!data || !window.TrellisIndexPin.equal(data.revision, revision)) {
        display([notice("Navigation is stale. Refresh the indexed view and try again."), close]);
        owner.onStale?.("Navigation is stale. Refresh the indexed view and try again."); return;
      }
      if (!Array.isArray(data.targets)) throw new Error("Invalid navigation response.");
      const actions = [];
      // Old call/type matches are lexical candidates, even when labelled "measured".
      // Only a declaration on the selected source line witnesses a source-open action.
      const declarations = data.targets.slice(0, MAX_TARGETS).filter(target =>
        target?.reason === "declaration" && target.matchKind === "measured" &&
        body.path && target.symbol?.path === body.path && target.symbol.range?.startLine === body.line &&
        sourceWitness(target.symbol) &&
        typeof owner.openSource === "function");
      const labels = declarations.map(target => `Read declaration source · ${text(target.symbol.name)} · ${text(target.symbol.path)}:${target.symbol.range.startLine}`);
      for (const [index, target] of declarations.entries()) {
        const r = target.symbol.range;
        const suffix = labels.indexOf(labels[index]) !== labels.lastIndexOf(labels[index])
          ? ` · bytes ${r.startByte ?? "?"}–${r.endByte ?? "?"} · ${text(target.symbol.id)}` : "";
        actions.push({label: labels[index] + suffix,
          run: () => { if (current()) { serial++; return owner.openSource(target.symbol, revision); } }});
      }
      if (!actions.length) actions.push(notice("No measured declaration on this line. Candidate type and call targets cannot be opened as navigation."));
      if (data.requireIndex) actions.push(notice("Index the workspace to populate cached class declarations."));
      for (const warning of (Array.isArray(data.warnings) ? data.warnings : []).slice(0, MAX_NOTICES)) actions.push(notice(warning));
      if (data.truncated || data.targets.length > MAX_TARGETS || data.warnings?.length > MAX_NOTICES) actions.push(notice("Partial index or navigation results; see notices."));
      actions.push(close); display(actions);
    } catch (error) {
      if (!mayPublish()) return;
      display([notice(`Navigation failed: ${text(error?.message || "Request unavailable.")}`), retry, close]);
      if (window.TrellisIndexPin.isConflict(error)) owner.onStale?.("Navigation is stale. Refresh the indexed view and try again.");
    }
  }
  function attachSource(container, {path, revision, startLine = 1, isCurrent} = {}) {
    // Only annotations/listeners are added. Source spans, selection, and highlighting
    // remain owned by the source renderer.
    for (const source of [...sources]) if (source.container === container) source.dispose();
    const rows = Array.from(container.querySelectorAll(".source-line"));
    const lines = rows.map(row => {
      const number = row.querySelector(".line-number")?.textContent?.trim() || "";
      return /^[1-9]\d*$/.test(number) && positive(Number(number)) ? Number(number) : null;
    });
    const initial = lines.indexOf(startLine);
    let active = initial < 0 ? 0 : initial, disposed = false, generation = 0;
    const toolbar = el("div", undefined, "source-navigation");
    const button = el("button", "Navigate", "source-navigation-action"); button.type = "button";
    button.setAttribute("aria-haspopup", "menu");
    const status = el("span", undefined, "source-navigation-status");
    status.setAttribute("role", "status"); status.setAttribute("aria-live", "polite");
    toolbar.append(button, status); container.before(toolbar);
    const attributes = ["tabindex", "aria-label", "aria-haspopup"];
    const previous = attributes.map(name => container.getAttribute(name));
    container.setAttribute("tabindex", "0"); container.setAttribute("aria-haspopup", "menu");
    const owner = api, session = owner?.currentSession();
    const usable = () => !disposed && api === owner && window.TrellisIndexPin.equal(owner?.currentRevision(), revision) &&
      owner.currentSession() === session && (!isCurrent || isCurrent());
    function update() {
      const line = lines[active];
      button.disabled = !line;
      button.textContent = line ? `Navigate line ${line}` : "Navigate";
      status.textContent = line ? "Line-based targets · ↑ ↓ to select · Shift F10 to navigate" : "No source lines available for navigation.";
      container.setAttribute("aria-label", line ? `Source code. Active line ${line}. Use Up and Down to select a line; Shift F10 to navigate indexed targets.` : "Source code. No navigable lines.");
      rows[active]?.classList.add("navigation-active-line");
    }
    function activate(index, scroll = false) {
      if (!usable() || index < 0 || index >= rows.length || index === active) return;
      rows[active]?.classList.remove("navigation-active-line");
      active = index; generation++; serial++; update();
      if (scroll) rows[active].scrollIntoView?.({block: "nearest", inline: "nearest"});
    }
    function rowIndex(target) {
      const row = target?.closest?.(".source-line");
      return row && container.contains(row) ? rows.indexOf(row) : -1;
    }
    function navigate(event) {
      if (!usable() || !lines[active]) return;
      const scope = generation;
      return open(event, {expectedRevision: revision, path, line: lines[active]},
        {isCurrent: () => usable() && scope === generation});
    }
    function click(event) { const index = rowIndex(event.target); if (index >= 0) activate(index); }
    function contextmenu(event) {
      const index = rowIndex(event.target);
      if (index < 0) return;
      activate(index); return navigate(event);
    }
    function keydown(event) {
      if (!usable() || event.altKey || event.ctrlKey || event.metaKey) return;
      if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) return navigate(event);
      // Shift-arrow remains native text selection. Copy and page scrolling remain native.
      if (event.shiftKey || event.target !== container) return;
      const next = {ArrowDown: Math.min(rows.length - 1, active + 1), ArrowUp: Math.max(0, active - 1), Home: 0, End: rows.length - 1}[event.key];
      if (next !== undefined && rows.length) { event.preventDefault(); activate(next, true); }
    }
    container.addEventListener("click", click);
    container.addEventListener("contextmenu", contextmenu);
    container.addEventListener("keydown", keydown);
    button.addEventListener("click", navigate);
    const binding = {container, reset() { closeOwnedMenu(); generation++; serial++; }, dispose() {
      if (disposed) return;
      closeOwnedMenu(); disposed = true; generation++; serial++;
      container.removeEventListener("click", click);
      container.removeEventListener("contextmenu", contextmenu);
      container.removeEventListener("keydown", keydown);
      button.removeEventListener("click", navigate);
      rows[active]?.classList.remove("navigation-active-line"); toolbar.remove();
      attributes.forEach((name, index) => previous[index] == null ? container.removeAttribute(name) : container.setAttribute(name, previous[index]));
      sources.delete(binding);
    }};
    sources.add(binding); update(); return binding;
  }
  window.TrellisNavigation = Object.freeze({init, open, attachSource, reset, dispose: reset});
})();
