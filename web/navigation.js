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
    const allowed = source ? ["expectedRevision", "path", "line"] : ["expectedRevision", "classId", "memberName", "startByte", "endByte", "memberPath"];
    if (Object.keys(selector).some(key => !allowed.includes(key))) return null;
    if (selector.expectedRevision !== undefined && !window.TrellisIndexPin.equal(selector.expectedRevision, revision)) return null;
    if (source) {
      if (typeof selector.path !== "string" || !selector.path || selector.path.length > 4096 || !positive(selector.line)) return null;
      return {expectedRevision: revision, path: selector.path, line: selector.line, semanticProducerPolicy: "solePublished"};
    }
    if (typeof selector.classId !== "string" || !selector.classId || selector.classId.length > 4096 ||
        typeof selector.memberName !== "string" || !selector.memberName || selector.memberName.length > 4096 ||
        !natural(selector.startByte) || !natural(selector.endByte) || selector.endByte < selector.startByte ||
        (selector.memberPath !== undefined && (typeof selector.memberPath !== "string" || !selector.memberPath || selector.memberPath.length > 4096))) return null;
    // The member's own document: Rust classes record methods from impl blocks in other files (#452).
    return {expectedRevision: revision, classId: selector.classId, memberName: selector.memberName,
      startByte: selector.startByte, endByte: selector.endByte, ...(selector.memberPath ? {memberPath: selector.memberPath} : {})};
  }
  function callable(symbol) { return ["function", "method"].includes(symbol?.kind); }
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
    if (!owner?.currentRevision?.()) return;
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
    const retry = {label: "Try navigation again", run: () => {
      if (current()) return open(anchor, body?.path ? {expectedRevision: revision, path: body.path, line: body.line} : body, {isCurrent});
    }};
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
      const candidates = data.targets.slice(0, MAX_TARGETS).filter(target => {
        if (!sourceWitness(target?.symbol) || typeof target.symbol.id !== "string" || !target.symbol.id) return false;
        if (target.reason === "call") return target.matchKind === "resolved" && target.action === "sequence" && callable(target.symbol);
        if (target.matchKind !== "measured" || !["declaration", "enclosing"].includes(target.reason)) return false;
        if (!body.path) return target.reason === "declaration";
        if (target.symbol.path !== body.path) return false;
        const range = target.symbol.range;
        return target.reason === "declaration" ? range.startLine === body.line :
          range.startLine <= body.line && (range.endLine > body.line ||
            (range.endLine === body.line && range.endColumn > 1));
      });
      const labels = candidates.map(target => {
        const range = target.range;
        const callsite = target.reason === "call" && natural(range?.start) && natural(range?.end) && range.end > range.start
          ? ` · call bytes ${range.start}–${range.end}` : "";
        return `${text(target.symbol.name)} · ${text(target.symbol.path)}:${target.symbol.range.startLine}${callsite}`;
      });
      for (const [index, target] of candidates.entries()) {
        const symbol = target.symbol, r = symbol.range;
        const suffix = labels.indexOf(labels[index]) !== labels.lastIndexOf(labels[index])
          ? target.reason === "call" && typeof target.callId === "string" && target.callId
            ? ` · call ${text(target.callId)}`
            : ` · bytes ${r.startByte ?? "?"}–${r.endByte ?? "?"} · ${text(symbol.id)}` : "";
        const label = labels[index] + suffix;
        const run = callback => () => { if (current()) { serial++; return callback(symbol, revision); } };
        if (target.reason === "declaration" && typeof owner.openSource === "function")
          actions.push({label: `Read declaration source · syntax · ${label}`, run: run(owner.openSource)});
        if (callable(symbol) && typeof owner.selectMethod === "function") {
          const prefix = target.reason === "enclosing" ? "Sequence · enclosing · syntax" : target.reason === "call" ? "Sequence of callee · resolved" : "Sequence · syntax";
          actions.push({label: `${prefix} · ${label}`, run: run(owner.selectMethod)});
        }
        if (symbol.kind === "class" && target.action === "class" && /\.(?:java|py|js|mjs|cjs|rs)$/i.test(symbol.path) && typeof owner.openClass === "function")
          actions.push({label: `Class diagram · syntax · ${label}`, run: run(owner.openClass)});
        if (["declaration", "enclosing"].includes(target.reason) && typeof owner.inspectSymbol === "function")
          actions.push({label: `Inspect${target.reason === "enclosing" ? " enclosing declaration" : ""} · syntax · ${label}`, run: run(owner.inspectSymbol)});
      }
      if (!actions.length) actions.push(notice("No measured declaration or resolved callee on this line. Candidate type and call targets cannot be opened as navigation."));
      actions.push(notice("Call graph · not available yet"));
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
  function attachSource(container, {path, revision, startLine = 1, isCurrent, metadataSnapshot} = {}) {
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
    const more = el("button", "More source actions", "source-navigation-action"); more.type = "button"; more.hidden = true;
    const coverage = el("span", undefined, "source-navigation-status"); coverage.setAttribute("role", "status");
    toolbar.append(button, status, more, coverage); container.before(toolbar);
    const attributes = ["tabindex", "aria-label", "aria-haspopup"];
    const previous = attributes.map(name => container.getAttribute(name));
    container.setAttribute("tabindex", "0"); container.setAttribute("aria-haspopup", "menu");
    const owner = api, session = owner?.currentSession?.();
    const usable = () => !disposed && api === owner && window.TrellisIndexPin.equal(owner?.currentRevision?.(), revision) &&
      owner?.currentSession?.() === session && (!isCurrent || isCurrent());
    const annotations = [], seen = new Set();
    let activeAction = null;
    const lineActions = () => annotations.filter(node => node.parentNode === rows[active]);
    function updateActions(preferred = activeAction) {
      const actions = lineActions();
      activeAction = actions.includes(preferred) ? preferred : actions[0] || null;
      for (const action of annotations) action.setAttribute("tabindex", action === activeAction ? "0" : "-1");
    }
    let nextOffset = 0, metadataLoading = false, metadataCount = 0, metadataPartial = false, admitted = null;
    function clearAnnotations() {
      for (const node of annotations) node.remove();
      annotations.length = 0; activeAction = null;
    }
    function renderSymbols(symbols) {
      for (const symbol of symbols) {
        if (seen.has(symbol.id)) continue;
        seen.add(symbol.id); metadataCount++; admitted.push(symbol);
        if (!callable(symbol) || typeof owner.selectMethod !== "function") continue;
        const row = rows[lines.indexOf(symbol.range.startLine)];
        if (!row) continue;
        const action = el("button", undefined, "source-diagram-action"); action.type = "button";
        action.setAttribute("tabindex", "-1");
        action.setAttribute("data-symbol-id", symbol.id);
        action.setAttribute("data-source-action", "sequence");
        // A CSS label keeps controls out of source text and native copy/selection.
        action.setAttribute("data-label", "Sequence");
        action.setAttribute("aria-label", `Sequence for ${text(symbol.name)} · syntax · ${text(symbol.id)}`);
        action.setAttribute("title", `Sequence · syntax · ${text(symbol.name)} · ${text(symbol.id)}`);
        action.addEventListener("click", event => {
          event.stopPropagation();
          if (usable()) { activate(lines.indexOf(symbol.range.startLine)); serial++; return owner.selectMethod(symbol, revision); }
        });
        row.append(action); annotations.push(action);
      }
      update();
    }
    function updateCoverage() {
      more.hidden = nextOffset === null; more.textContent = "More source actions";
      coverage.textContent = nextOffset !== null || metadataPartial ?
        `Source actions cover ${metadataCount} declarations · ${nextOffset !== null ? "more available" : "partial metadata"}. Line navigation remains available.` : "";
    }
    async function loadDeclarations() {
      if (!usable() || metadataLoading || nextOffset === null || typeof owner.request !== "function") return;
      const offset = nextOffset;
      const current = usable;
      metadataLoading = true; more.disabled = true;
      try {
        const data = await owner.request(`/api/symbols?path=${encodeURIComponent(path)}&${window.TrellisIndexPin.query(revision)}&offset=${offset}&limit=200`);
        if (!current()) return;
        if (!window.TrellisIndexPin.equal(data?.revision, revision)) {
          clearAnnotations(); admitted = null; nextOffset = null; more.hidden = true;
          coverage.textContent = "Source actions are stale. Refresh the indexed view.";
          owner.onStale?.("Source actions are stale. Refresh the indexed view."); return;
        }
        if (data.path !== path || !Array.isArray(data.items) || data.items.length > 200 ||
            data.items.some(item => item?.symbol?.path !== path || !sourceWitness(item.symbol) ||
              typeof item.symbol.id !== "string" || !item.symbol.id) ||
            !(data.nextOffset === null || (natural(data.nextOffset) && data.nextOffset > offset)))
          throw new Error("Invalid source declaration metadata.");
        admitted ||= [];
        renderSymbols(data.items.map(item => item.symbol));
        nextOffset = data.nextOffset;
        metadataPartial ||= (data.nextOffset === null && !!data.truncated) || !!data.warnings?.length;
        updateCoverage();
      } catch (error) {
        if (!current()) return;
        coverage.textContent = `Source actions unavailable: ${text(error?.message || "Request failed.")} Line navigation remains available.`;
        more.hidden = false; more.textContent = "Retry source actions";
        if (window.TrellisIndexPin.isConflict(error)) {
          clearAnnotations(); admitted = null; owner.onStale?.("Source actions are stale. Refresh the indexed view.");
        }
      } finally {
        metadataLoading = false;
        if (current()) more.disabled = false;
      }
    }
    function update() {
      const line = lines[active];
      if (line) container.dataset.navigationLine = String(line);
      else delete container.dataset.navigationLine;
      button.disabled = !line;
      button.textContent = line ? `Navigate line ${line}` : "Navigate";
      updateActions();
      const actionHint = activeAction ? ` · Tab for Sequence${lineActions().length > 1 ? " · ← → to choose declaration" : ""}` : "";
      status.textContent = line ? `Line-based targets · ↑ ↓ to select · Shift F10 to navigate${actionHint}` : "No source lines available for navigation.";
      container.setAttribute("aria-label", line ? `Source code. Active line ${line}. Use Up and Down to select a line; Shift F10 to navigate indexed targets.${activeAction ? " Tab for Sequence. Left and Right choose declarations on this line." : ""}` : "Source code. No navigable lines.");
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
    function focusin(event) {
      if (!usable() || !annotations.includes(event.target)) return;
      activate(rowIndex(event.target)); updateActions(event.target);
    }
    function contextmenu(event) {
      const index = rowIndex(event.target);
      if (index < 0) return;
      activate(index); return navigate(event);
    }
    function keydown(event) {
      if (!usable() || event.altKey || event.ctrlKey || event.metaKey) return;
      if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) return navigate(event);
      // Shift-arrow remains native text selection. Copy and page scrolling remain native.
      const onAction = annotations.includes(event.target);
      if (event.shiftKey || (event.target !== container && !onAction)) return;
      if (onAction && ["ArrowLeft", "ArrowRight"].includes(event.key)) {
        event.preventDefault();
        const actions = lineActions(), step = event.key === "ArrowRight" ? 1 : -1;
        const nextAction = actions[(actions.indexOf(event.target) + step + actions.length) % actions.length];
        updateActions(nextAction); nextAction?.focus(); return;
      }
      const next = {ArrowDown: Math.min(rows.length - 1, active + 1), ArrowUp: Math.max(0, active - 1), Home: 0, End: rows.length - 1}[event.key];
      if (next !== undefined && rows.length) {
        event.preventDefault();
        if (onAction) container.focus({preventScroll:true});
        activate(next, true);
      }
    }
    container.addEventListener("click", click);
    container.addEventListener("contextmenu", contextmenu);
    container.addEventListener("keydown", keydown);
    container.addEventListener("focusin", focusin);
    button.addEventListener("click", navigate);
    more.addEventListener("click", loadDeclarations);
    const binding = {container, snapshot() {
      return admitted !== null ? {items:admitted.slice(), nextOffset, metadataPartial, metadataCount} : undefined;
    }, reset() { closeOwnedMenu(); generation++; serial++; }, dispose() {
      if (disposed) return;
      closeOwnedMenu(); disposed = true; generation++; serial++;
      container.removeEventListener("click", click);
      container.removeEventListener("contextmenu", contextmenu);
      container.removeEventListener("keydown", keydown);
      container.removeEventListener("focusin", focusin);
      button.removeEventListener("click", navigate);
      more.removeEventListener("click", loadDeclarations); clearAnnotations();
      delete container.dataset.navigationLine;
      rows[active]?.classList.remove("navigation-active-line"); toolbar.remove();
      attributes.forEach((name, index) => previous[index] == null ? container.removeAttribute(name) : container.setAttribute(name, previous[index]));
      sources.delete(binding);
    }};
    sources.add(binding); update();
    if (metadataSnapshot && usable()) {
      admitted = [];
      renderSymbols(metadataSnapshot.items);
      nextOffset = metadataSnapshot.nextOffset; metadataPartial = metadataSnapshot.metadataPartial; metadataCount = metadataSnapshot.metadataCount;
      updateCoverage();
      binding.ready = Promise.resolve();
    } else binding.ready = loadDeclarations();
    return binding;
  }
  window.TrellisNavigation = Object.freeze({init, open, attachSource, reset, dispose: reset});
})();
