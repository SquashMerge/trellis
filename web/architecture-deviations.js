"use strict";
// The Deviations tree: production $layers deviations grouped by file, function and line (#786).
// `model` is pure and DOM-free; `create` is the view. Repository text only ever reaches the page
// through textContent.
(() => {
  const get = id => document.getElementById(id);
  const make = (tag, text, cls) => {
    const node = document.createElement(tag);
    if (text !== undefined) node.textContent = text;
    if (cls) node.className = cls;
    return node;
  };
  const KINDS = ["call", "type", "import", "reference"];
  const TAGS = {call:"call", type:"type use", import:"import", reference:"reference"};
  const NOUNS = {call:["call", "calls"], type:["type use", "type uses"], import:["import", "imports"], reference:["reference", "references"]};
  const REASONS = {structural_only:"no semantic evidence for this file", unresolved:"no semantic binding for this site",
    external:"bound outside the checkout", ambiguous:"ambiguous binding"};
  const PARAMS = {kind:"instances", rule:"$layers", status:"deviates", role:"production", excepted:"false", limit:"500"};
  const CONFLICTS = ["revision_conflict", "pin_expired", "root_changed"];
  const INVALID = "Invalid Architecture detail response. Nothing displayed.";
  // The tree replaces the drawing and its toolbar (app switcher, metric controls, ⓘ); its own header leads back (#845).
  const HIDDEN = ["#architecture-diagram", "#architecture-legend", ".architecture-edge-legend", "#architecture-panel .architecture-toolbar"];
  const MODULE = "(module)";
  // Unnamed owners (closures) are named after their nearest named callable from the Files outline,
  // read for at most this many files, and pages of 200 symbols per file, per tree (#845).
  const CLOSURE_FILES = 50, OUTLINE_PAGES = 50;
  const unnamed = name => typeof name !== "string" || !name || name.startsWith("<");
  // A closure belongs to the nearest enclosing *callable*: a named function, method or constructor, or a binding
  // whose whole initializer is a function (the outline's `callableInitializer`, #526). Variables, fields and
  // types are lexical parents, not owners (#860).
  const CALLABLE = new Set(["function", "method", "constructor"]);
  const callable = item => !unnamed(item.symbol.name) && (CALLABLE.has(item.symbol.kind) || CALLABLE.has(item.nativeKind) || item.callableInitializer === true);
  // A closure with no enclosing callable, read to the root of its file's outline, is module-level code.
  const MODULE_LEVEL = Object.freeze({syntaxId:"(module)", moduleLevel:true});
  const num = value => value.toLocaleString("en-US");
  const plural = (value, one, many = `${one}s`) => `${num(value)} ${value === 1 ? one : many}`;
  const basename = path => path.slice(path.lastIndexOf("/") + 1);
  const record = value => value !== null && typeof value === "object" && !Array.isArray(value);
  const counted = value => Number.isSafeInteger(value) && value >= 0;
  const zeros = () => ({call:0, type:0, import:0, reference:0});
  const edgeKey = (source, destination) => `${source}→${destination}`;
  const validLines = lines => !!lines && Number.isSafeInteger(lines.start) && lines.start > 0 && Number.isSafeInteger(lines.end) && lines.end >= lines.start;
  const sortBy = (items, ...keys) => items.sort((a, b) => {
    for (const key of keys) { const x = key(a), y = key(b); if (x !== y) return x < y ? -1 : 1; }
    return 0;
  });

  const model = (() => {
    // The tag is the classified relation kind (#785), the same key Calls only and the kind counts use. A require or
    // dynamic import keeps its `import` tag even when it carries a caller (#816).
    const kindOf = evidence => evidence.relationKind;
    const destinationKey = destination => JSON.stringify([destination.sourceSetId, destination.language, destination.path ?? destination.package]);
    const lineKey = (instance, evidence) => JSON.stringify([instance.target.sourceSetId, instance.target.path, destinationKey(instance.destination),
      evidence.range.start, evidence.range.end, evidence.relationKind, evidence.callId]);
    // A shape and type check only. Any failing row invalidates the whole read.
    const valid = row => {
      const i = row?.instance;
      return record(i) && record(i.target) && record(i.destination) && typeof i.sourceLayer === "string" && typeof i.destinationLayer === "string" &&
        counted(i.referenceCount) && record(i.kindCounts) && KINDS.every(kind => counted(i.kindCounts[kind])) &&
        Array.isArray(i.evidence) && i.evidence.every(item => record(item) && KINDS.includes(item.relationKind) && record(item.range));
    };
    const directionOf = layers => {
      const order = new Map((layers || []).map(layer => [layer.name, layer.order]));
      return (source, destination) => Number.isFinite(order.get(source)) && Number.isFinite(order.get(destination)) &&
        order.get(destination) < order.get(source) ? "up" : "cross";
    };
    // A function row sits under its file row, so its label drops the `path::` prefix of the stable key.
    const labelOf = (owner, instance) => {
      if (!owner || owner.moduleLevel || owner.syntaxId === instance.target.sid) return "(module level)";
      const key = owner.stableLookupKey, prefix = `${instance.target.path}::`;
      return (typeof key === "string" && key.startsWith(prefix) ? key.slice(prefix.length) : key) ?? owner.name ?? "(anonymous)";
    };
    // `names` maps an unnamed owner's syntax ID to its nearest named enclosing callable (from `closureNames`).
    function assemble(rows, layers, {edge = null, callsOnly = false} = {}, names = null) {
      const direction = directionOf(layers), files = new Map();
      for (const {instance:i} of rows) {
        const pair = edgeKey(i.sourceLayer, i.destinationLayer);
        if (edge !== null && pair !== edge) continue;
        const key = JSON.stringify([i.target.sourceSetId, i.target.language, i.target.path]);
        if (!files.has(key)) files.set(key, {key, target:i.target, path:i.target.path, edges:new Map(), references:0, calls:0, kinds:zeros(), listed:zeros(),
          functions:new Map()});
        const file = files.get(key);
        if (!file.edges.has(pair)) file.edges.set(pair, {key:pair, sourceLayer:i.sourceLayer, destinationLayer:i.destinationLayer,
          direction:direction(i.sourceLayer, i.destinationLayer), count:0, calls:0});
        const entry = file.edges.get(pair);
        entry.count += i.referenceCount; entry.calls += i.kindCounts.call;
        file.references += i.referenceCount; file.calls += i.kindCounts.call;
        for (const kind of KINDS) file.kinds[kind] += i.kindCounts[kind];
        // A sampled instance (more sites than listed) makes every function it lists lines for a lower bound;
        // a function fed only by fully listed instances has exact counts (#845).
        const callsExact = i.evidence.filter(evidence => kindOf(evidence) === "call").length === i.kindCounts.call;
        const refsExact = i.evidence.length === i.referenceCount;
        for (const evidence of i.evidence) {
          const kind = kindOf(evidence);
          file.listed[kind]++;
          if (callsOnly && kind !== "call") continue;
          const enclosing = evidence.owner && names?.get(evidence.owner.syntaxId), owner = enclosing || evidence.owner;
          const id = owner && !owner.moduleLevel ? owner.syntaxId : MODULE;
          if (!file.functions.has(id)) file.functions.set(id, {key:id, id:JSON.stringify(["fn", key, id]), label:labelOf(owner, i), lines:[], callsExact:true, refsExact:true});
          const fn = file.functions.get(id);
          fn.callsExact &&= callsExact; fn.refsExact &&= refsExact;
          fn.lines.push({key:lineKey(i, evidence), evidence, instance:i, kind, fn:fn.label, closure:!!enclosing, direction:direction(i.sourceLayer, i.destinationLayer),
            start:evidence.lines?.start ?? 0});
        }
      }
      const done = [...files.values()].filter(file => !callsOnly || file.calls > 0).map(file => {
        const unlisted = zeros();
        for (const kind of KINDS) unlisted[kind] = Math.max(0, file.kinds[kind] - file.listed[kind]);
        const functions = [...file.functions.values()].map(fn => {
          sortBy(fn.lines, line => line.start, line => line.evidence.range.start);
          const calls = fn.lines.filter(line => line.kind === "call").length;
          return {...fn, calls, references:fn.lines.length};
        }).filter(fn => !callsOnly || fn.calls > 0);
        sortBy(functions, fn => fn.calls > 0 ? 0 : 1, fn => -fn.references, fn => fn.label);
        const edges = sortBy([...file.edges.values()].filter(item => !callsOnly || item.calls > 0), item => -(callsOnly ? item.calls : item.count), item => item.key);
        const {listed, functions:_functions, edges:_edges, ...rest} = file;
        return {...rest, edges, functions, unlisted, shown:callsOnly ? file.calls : file.references};
      });
      sortBy(done, file => -file.shown, file => file.path);
      return {files:done, totals:{files:done.length, references:done.reduce((sum, file) => sum + file.references, 0), calls:done.reduce((sum, file) => sum + file.calls, 0)}};
    }
    const build = (rows, layers, names = null) => ({...assemble(rows, layers, {}, names), rows, layers, names});
    const filter = (tree, options) => ({...assemble(tree.rows, tree.layers, options, tree.names), rows:tree.rows, layers:tree.layers, names:tree.names});
    // The unnamed owners to name, by file: an owner with neither a name nor a stable key that is not the file module.
    function unnamedOwners(rows) {
      const files = new Map();
      for (const {instance:i} of rows) for (const evidence of i.evidence) {
        const owner = evidence.owner;
        if (!owner || owner.syntaxId === i.target.sid || owner.stableLookupKey || owner.name) continue;
        if (!files.has(i.target.path)) files.set(i.target.path, new Set());
        files.get(i.target.path).add(owner.syntaxId);
      }
      return files;
    }
    // Walks each unnamed owner's `parent` chain in one file's outline items to the first callable. A named
    // owner seen elsewhere in the rows supplies its stable key, so the row joins that function's row;
    // otherwise a method reads `Type::name` from its Rust impl block, as stable keys do. A chain that reaches
    // the root without a callable is module level; one that breaks (a missing parent, the hop limit) stays unnamed.
    function closureNames(rows, ids, outline, names = new Map()) {
      const known = new Map();
      for (const {instance:i} of rows) for (const evidence of i.evidence) if (evidence.owner && (evidence.owner.stableLookupKey || evidence.owner.name)) known.set(evidence.owner.syntaxId, evidence.owner);
      const byId = new Map();
      for (const item of Array.isArray(outline) ? outline : []) if (record(item?.symbol) && typeof item.symbol.id === "string") byId.set(item.symbol.id, item);
      for (const id of ids) {
        let item = byId.get(id), hops = 0;
        if (!item) continue;
        for (item = byId.get(item.symbol.parent); item && !callable(item) && hops < 64; hops++) item = byId.get(item.symbol.parent);
        if (!item) {
          // The walk left the outline: at the root (`parent: null`) it is module level, otherwise unknown.
          let last = byId.get(id), steps = 0;
          while (last && last.symbol.parent != null && byId.has(last.symbol.parent) && steps++ < 64) last = byId.get(last.symbol.parent);
          if (last && last.symbol.parent == null) names.set(id, MODULE_LEVEL);
          continue;
        }
        if (!callable(item)) continue;
        const parent = byId.get(item.symbol.parent), type = typeof parent?.implTarget === "string" && parent.implTarget ? parent.implTarget : null;
        names.set(id, known.get(item.symbol.id) ?? {syntaxId:item.symbol.id, name:type ? `${type}::${item.symbol.name}` : item.symbol.name, stableLookupKey:null});
      }
      return names;
    }
    // One chip per layer pair, from the summary edges; `bound` is the same rule as the diagram arrows.
    function chips(view, edgeText) {
      const direction = directionOf(view.layers), groups = new Map();
      for (const edge of view.edges || []) {
        if (edge.role !== "production" || edge.status !== "deviates" || edge.excepted) continue;
        const key = edgeKey(edge.sourceLayer, edge.destinationLayer);
        if (!groups.has(key)) groups.set(key, {key, sourceLayer:edge.sourceLayer, destinationLayer:edge.destinationLayer,
          direction:direction(edge.sourceLayer, edge.destinationLayer), count:0, bound:false});
        const group = groups.get(key);
        group.count += edge.count; group.bound ||= !!edgeText(edge)?.bound;
      }
      const edges = sortBy([...groups.values()], chip => -chip.count, chip => chip.key);
      return {all:{count:edges.reduce((sum, chip) => sum + chip.count, 0), bound:edges.some(chip => chip.bound)}, edges};
    }
    const exceptionNote = (n, a, b) => `Approves all ${n} reference${n === 1 ? "" : "s"} from ${a} to ${b}, including future ones on this pair.`;
    function keyAction(rows, index, key) {
      const row = rows[index];
      if (!row) return null;
      const at = {focus:index}, parent = rows.findIndex(item => item.key === row.parent);
      switch (key) {
        case "ArrowDown": return {focus:Math.min(index + 1, rows.length - 1)};
        case "ArrowUp": return {focus:Math.max(index - 1, 0)};
        case "Home": return {focus:0};
        case "End": return {focus:rows.length - 1};
        case "ArrowRight":
          if (!row.expandable) return at;
          if (!row.expanded) return {...at, expand:row.key};
          return rows[index + 1]?.parent === row.key ? {focus:index + 1} : at;
        case "ArrowLeft":
          if (row.expandable && row.expanded) return {...at, collapse:row.key};
          return parent >= 0 ? {focus:parent} : at;
        case "Enter": case " ":
          if (row.expandable) return {...at, toggle:row.key};
          return key === "Enter" && row.level === 3 ? {...at, select:row.key} : at;
        default: return null;
      }
    };
    const canOpenSequence = evidence => !!(evidence.caller?.syntaxId && evidence.caller.stableLookupKey && evidence.callId);
    return Object.freeze({lineKey, valid, build, filter, chips, exceptionNote, keyAction, canOpenSequence, kindOf, unnamedOwners, closureNames});
  })();

  function create(deps) {
    const states = new Map(), nodes = new Map();
    let opened = false, trigger = null, ticket = 0, chrome = null, base = null, tree = null, phase = "idle", progress = "", errorInfo = null,
      hiddenNotice = false, feedback = null, pending = null, lineActive = null, activeKey = null, visible = [], hiddenNodes = [], loadedKey = null;
    const basisKey = () => {
      const basis = deps.basis();
      return basis ? JSON.stringify([basis.pin.indexGeneration, basis.pin.indexRevision, basis.rulesHash]) : "none";
    };
    const state = () => {
      const key = basisKey();
      if (!states.has(key)) states.set(key, {chip:null, callsOnly:false, expanded:new Set(), selected:null});
      return states.get(key);
    };
    const pinLabel = () => window.TrellisIndexPin.label(deps.view());
    const fallbackActive = () => opened && deps.current() && basisKey() === loadedKey;
    const isActive = () => (lineActive || fallbackActive)();

    function hide() {
      if (hiddenNodes.length) return;
      for (const selector of HIDDEN) {
        const node = document.querySelector(selector);
        if (node) { hiddenNodes.push([node, node.hidden]); node.hidden = true; }
      }
    }
    function showHidden() {
      for (const [node, was] of hiddenNodes) node.hidden = was;
      hiddenNodes = [];
    }

    function ensureChrome() {
      if (chrome) return;
      const root = get("architecture-deviations");
      const back = make("button", deps.backLabel?.() ?? "← Architecture", "deviations-back"); back.type = "button"; back.id = "architecture-deviations-back";
      back.addEventListener("click", () => close());
      const title = make("h2", "Deviations"); title.id = "architecture-deviations-title";
      const summary = make("p", "", "detail"); summary.id = "architecture-deviations-summary";
      const help = make("p", "Expand a file to see its functions and lines. Select a line for its actions, or open its menu with ⋯, right-click or Shift+F10.", "detail");
      const header = make("div", undefined, "deviations-header"); header.append(back, title, summary, help);
      const edges = make("div", undefined, "deviations-edges"); edges.id = "architecture-deviations-edges";
      edges.setAttribute("role", "group"); edges.setAttribute("aria-label", "Deviation edge");
      const calls = make("input"); calls.type = "checkbox"; calls.id = "architecture-deviations-calls";
      calls.addEventListener("change", () => changeFilter({callsOnly:calls.checked}, () => calls.focus()));
      const callsLabel = make("label"); callsLabel.append(calls, make("span", " Calls only"));
      const filters = make("div", undefined, "deviations-filters"); filters.append(edges, callsLabel, make("span", "Sorted by references · production only", "detail"));
      // Why an edge total may be low: the one place the tree says so, instead of a ≥ on every chip (#845).
      const coverage = make("p", "", "detail deviations-coverage"); coverage.id = "architecture-deviations-coverage";
      const notices = make("div", undefined, "deviations-notices"); notices.id = "architecture-deviations-notices";
      notices.setAttribute("role", "status"); notices.setAttribute("aria-live", "polite");
      const slots = {hidden:make("div"), unknown:make("div"), error:make("div"), feedback:make("div")};
      notices.append(slots.hidden, slots.unknown, slots.error, slots.feedback);
      const line = make("p", "", "detail"); line.id = "architecture-deviations-state"; line.setAttribute("role", "status"); line.setAttribute("aria-live", "polite");
      const head = make("div", undefined, "deviations-head"); head.setAttribute("aria-hidden", "true");
      head.append(make("span", "File, function, line"), make("span", "Reaches up"), make("span", "Calls"), make("span", "References"), make("span", ""));
      const box = make("div", undefined, "deviations-tree"); box.id = "architecture-deviations-tree";
      box.setAttribute("role", "tree"); box.setAttribute("aria-label", "Files with deviations");
      box.addEventListener("keydown", treeKey);
      const card = make("div", undefined, "deviations-card");
      card.append(head, box, make("p", "Lines are listed up to a sample per file pair, calls first. File totals are exact; ≥ marks a function whose file pair has more lines than listed.", "detail"));
      root.replaceChildren(header, filters, coverage, notices, line, card);
      chrome = {root, back, summary, edges, calls, coverage, slots, line, box, card};
    }

    // Reads every page in sequence; nothing read so far survives a failure.
    async function load(extra = null, restoring = false) {
      const mine = ++ticket, key = basisKey(), view = deps.view();
      loadedKey = key;
      const active = () => ticket === mine && opened && deps.current() && basisKey() === key && (!extra || extra());
      phase = "loading"; progress = ""; errorInfo = null; base = tree = null; render();
      try {
        const read = [];
        for (let offset = 0; offset !== null;) {
          const page = await deps.details({...PARAMS}, active, offset);
          if (!active()) return;
          read.push(...page.items); offset = page.nextOffset;
          if (offset !== null) { progress = `Reading deviations… ${num(read.length)} of ${num(page.total)} instances`; renderState(); }
        }
        if (!read.every(model.valid)) throw new Error(INVALID);
        const names = await nameClosures(read, active);
        if (!active()) return;
        base = model.build(read, view.layers, names); phase = "ready";
        refilter(); settle(restoring);
      } catch (error) {
        if (!active() || error?.name === "AbortError") return;
        base = tree = null; phase = "error"; errorInfo = describe(error); render();
        if (error?.status === 409 && CONFLICTS.includes(error.code)) deps.onConflict();
      }
    }
    // Best effort: a file whose outline cannot be read keeps "(anonymous)"; only a lost owner stops the load.
    // The outline lists a parent before its children, so paging stops once every wanted owner is seen.
    async function nameClosures(rows, active) {
      const names = new Map();
      if (typeof deps.outline !== "function") return names;
      for (const [path, ids] of [...model.unnamedOwners(rows)].slice(0, CLOSURE_FILES)) {
        progress = `Naming closures in ${path}…`; renderState();
        const items = [], wanted = new Set(ids);
        try {
          for (let offset = 0, pages = 0; offset !== null && wanted.size && pages < OUTLINE_PAGES; pages++) {
            const page = await deps.outline(path, offset, active);
            if (!active()) return names;
            for (const item of page.items) { items.push(item); wanted.delete(item?.symbol?.id); }
            offset = page.nextOffset;
          }
        } catch (error) {
          // A typed stale answer means the pin is gone: it is the tree's conflict, never an unnamed tree (#860).
          if (!active() || error?.name === "AbortError" || (error?.status === 409 && CONFLICTS.includes(error.code))) throw error;
        }
        model.closureNames(rows, ids, items, names);
      }
      return names;
    }
    function describe(error) {
      if (error?.status === 409 && CONFLICTS.includes(error.code)) return "The index moved to a newer revision. Reload deviations.";
      if (error?.code === "rules_conflict") return "Rules changed since these deviations were loaded. Reload deviations.";
      if (error?.status === 503 || error?.code === "architecture_building") return "Architecture is still building for this revision.";
      return error?.message || "Deviations unavailable.";
    }
    function refilter() {
      const s = state();
      if (s.chip && !model.chips(deps.view(), deps.edgeText).edges.some(chip => chip.key === s.chip)) s.chip = null;
      tree = base && model.filter(base, {edge:s.chip, callsOnly:s.callsOnly});
    }
    function findLine(key) {
      for (const file of tree?.files || []) for (const fn of file.functions) {
        const line = fn.lines.find(item => item.key === key);
        if (line) return {line, file, fn};
      }
      return null;
    }
    // Once the rows are in: reselect a remembered line. Nothing is ever selected on its own.
    function settle(restoring) {
      const s = state(), found = s.selected && findLine(s.selected);
      activeKey = null;
      if (found) {
        if (restoring) { s.expanded.add(found.file.key); s.expanded.add(found.fn.id); }
        render(); select(found.line, restoring);
        if (restoring) nodes.get(found.line.key)?.el.scrollIntoView?.({block:"nearest"});
        return;
      }
      if (s.selected) {
        s.selected = null;
        if (restoring) { feedback = "The selected line is no longer listed."; deps.overview(); }
      }
      render();
    }

    function close() {
      if (!opened) return;
      opened = false; ticket++; showHidden();
      if (chrome) chrome.root.hidden = true;
      deps.overview();
      // Back returns to the arrow or badge that opened the tree, else to the review chip or the drawing.
      const target = (trigger && trigger.isConnected !== false && !trigger.closest?.("[hidden]") ? trigger : null) || get("architecture-review") || get("architecture-diagram");
      target?.focus?.();
    }
    function open(next) {
      if (!deps.current()) return;
      if (next) trigger = next;
      if (opened && chrome && loadedKey === basisKey() && phase !== "error") { (nodes.get(activeKey)?.el || chrome.box).focus(); return; }
      opened = true; hide(); ensureChrome(); chrome.root.hidden = false;
      const reading = load();
      chrome.back.focus();
      return reading;
    }
    // Opens on one layer pair's chip with nothing selected and Calls only off. An already open tree
    // just switches filter. A new tree reads under the state set here, so the chip applies once loaded.
    function openEdge(sourceLayer, destinationLayer, next) {
      if (!deps.current()) return;
      Object.assign(state(), {chip:edgeKey(sourceLayer, destinationLayer), callsOnly:false, selected:null});
      if (!opened || !base) return open(next);
      hiddenNotice = false; refilter(); deps.overview(); render();
    }
    function reset(keepReopen = false) {
      if (!keepReopen) pending = null;
      ticket++; opened = false; showHidden(); lineActive = null;
      base = tree = null; phase = "idle"; progress = ""; errorInfo = null; hiddenNotice = false; feedback = null; activeKey = null; visible = []; nodes.clear();
      state().selected = null;
      if (chrome) { chrome.root.replaceChildren(); chrome.root.hidden = true; chrome = null; }
    }
    function reopenAfterRefresh() {
      const s = state();
      pending = {chip:s.chip, callsOnly:s.callsOnly, trigger};
    }
    function afterRefresh() {
      if (!pending) return;
      const {chip, callsOnly, trigger:was} = pending;
      pending = null;
      if (!deps.current()) return;
      Object.assign(state(), {chip, callsOnly, expanded:new Set(), selected:null});
      return open(was);
    }
    const snapshot = () => {
      if (!opened) return null;
      const s = state();
      return {chip:s.chip, callsOnly:s.callsOnly, expanded:[...s.expanded], selected:s.selected};
    };
    async function restore(saved, active) {
      if (!saved || !deps.current() || (active && !active())) return;
      Object.assign(state(), {chip:saved.chip ?? null, callsOnly:!!saved.callsOnly, expanded:new Set(saved.expanded || []), selected:saved.selected ?? null});
      opened = true; hide(); ensureChrome(); chrome.root.hidden = false;
      return load(active, true);
    }

    function notice(text) { feedback = text; renderNotices(); }
    function changeFilter(change, focus) {
      const s = state();
      Object.assign(s, change); refilter();
      if (s.selected && !findLine(s.selected)) { s.selected = null; hiddenNotice = true; deps.overview(); }
      render(); focus();
    }
    function clearFilter() {
      const s = state();
      Object.assign(s, {chip:null, callsOnly:false, selected:null}); hiddenNotice = false; refilter(); render();
      chrome.edges.children[0]?.focus();
    }

    function render() {
      if (!chrome) return;
      renderSummary(); renderChips(); renderNotices(); renderState(); renderTree();
    }
    function renderSummary() {
      const s = state(), totals = tree?.totals;
      chrome.summary.textContent = !totals ? "" : s.callsOnly ? `${plural(totals.files, "file")} · ${plural(totals.calls, "deviating call")}`
        : `${plural(totals.files, "file")} · ${plural(totals.references, "deviating reference")} · ${plural(totals.calls, "call")}`;
    }
    // Chips count reference sites, the design's unit: "storage → indexing · 445". A peer or shared edge keeps
    // its ⇄. A count that may be low (unresolved sites in the source layer) is said once, below the chips.
    function renderChips() {
      const s = state(), chips = model.chips(deps.view(), deps.edgeText), buttons = [];
      const upward = chips.edges.every(entry => entry.direction === "up");
      const chip = (key, label, cross, entry) => {
        const button = make("button", `${cross ? "⇄ " : ""}${label} · ${num(entry.count)}`, "deviations-chip"); button.type = "button";
        button.dataset.key = key ?? ""; button.setAttribute("aria-pressed", String(s.chip === key));
        if (s.chip === key) button.className += " selected";
        button.setAttribute("aria-label", `${key === null ? "" : cross ? "Peer or shared: " : "Upward: "}${label}, ${plural(entry.count, "reference")}`);
        button.addEventListener("click", () => changeFilter({chip:key}, () => Array.from(chrome.edges.children).find(item => item.dataset.key === (key ?? ""))?.focus()));
        buttons.push(button);
      };
      chip(null, upward ? "All upward" : "All deviations", false, chips.all);
      for (const entry of chips.edges) chip(entry.key, `${entry.sourceLayer} → ${entry.destinationLayer}`, entry.direction !== "up", entry);
      chrome.edges.replaceChildren(...buttons);
      chrome.calls.checked = s.callsOnly;
      const low = [...new Set(chips.edges.filter(entry => entry.bound && (s.chip === null || s.chip === entry.key)).map(entry => entry.sourceLayer))];
      chrome.coverage.textContent = low.length ? `Counts are references mapped to a file. Some sites in ${low.join(", ")} stayed unresolved, so ${low.length === 1 ? "its" : "their"} edges may have more.` : "";
      chrome.coverage.hidden = !low.length;
    }
    function renderNotices() {
      const {slots} = chrome, unknown = deps.view()?.rules?.counts?.unknown;
      slots.hidden.replaceChildren(); slots.unknown.replaceChildren(); slots.error.replaceChildren(); slots.feedback.replaceChildren();
      if (hiddenNotice) {
        const clear = make("button", "Clear filter"); clear.type = "button"; clear.id = "architecture-deviations-clear";
        clear.addEventListener("click", clearFilter);
        slots.hidden.append(make("span", "Selected line is hidden by this filter."), clear);
      }
      if (Number.isSafeInteger(unknown) && unknown > 0) {
        const inspect = make("button", "Inspect same-layer and unassigned references"); inspect.type = "button"; inspect.id = "architecture-deviations-inspect";
        inspect.addEventListener("click", () => { close(); deps.inspectUnknown(); });
        slots.unknown.append(make("span", unknown === 1 ? "1 unknown rule instance is not a deviation." : `${num(unknown)} unknown rule instances are not deviations.`), inspect);
      }
      if (errorInfo) {
        const reload = make("button", "Reload deviations"); reload.type = "button"; reload.id = "architecture-deviations-reload";
        reload.addEventListener("click", () => { reopenAfterRefresh(); return deps.refresh(); });
        slots.error.append(make("span", errorInfo), reload);
      }
      if (feedback) slots.feedback.append(make("span", feedback));
    }
    function renderState() {
      const s = state(), at = pinLabel();
      chrome.line.textContent = phase === "loading" ? progress || `Reading deviations at ${at}…`
        : phase !== "ready" ? ""
        : !base.rows.length ? `No production deviations at ${at}.`
        : tree.files.length ? ""
        : s.callsOnly ? `No calls on this edge at ${at}. Turn off Calls only to see type uses, imports and references.`
        : `No deviations on this edge at ${at}.`;
    }

    function visibleRows() {
      const s = state(), out = [];
      for (const file of tree?.files || []) {
        const open = s.expanded.has(file.key);
        out.push({key:file.key, level:1, expandable:true, expanded:open, parent:null, kind:"file", file});
        if (!open) continue;
        for (const fn of file.functions) {
          const fnOpen = s.expanded.has(fn.id);
          out.push({key:fn.id, level:2, expandable:true, expanded:fnOpen, parent:file.key, kind:"function", file, fn});
          if (fnOpen) for (const line of fn.lines) out.push({key:line.key, level:3, expandable:false, expanded:false, parent:fn.id, kind:"line", file, fn, line});
        }
        const more = s.callsOnly ? file.unlisted.call : KINDS.reduce((sum, kind) => sum + file.unlisted[kind], 0);
        if (more > 0) out.push({key:JSON.stringify(["more", file.key]), level:2, expandable:false, expanded:false, parent:file.key, kind:"more", file});
      }
      return out;
    }
    function renderTree() {
      const s = state();
      chrome.box.setAttribute("aria-busy", String(phase === "loading"));
      nodes.clear();
      if (!tree?.files.length) { visible = []; chrome.box.replaceChildren(); chrome.card.hidden = true; return; }
      chrome.card.hidden = false;
      visible = visibleRows();
      if (!visible.some(item => item.key === activeKey)) activeKey = visible.some(item => item.key === s.selected) ? s.selected : visible[0]?.key ?? null;
      chrome.box.replaceChildren(...visible.map(rowNode));
      syncRows();
    }
    function syncRows() {
      const s = state();
      for (const [key, node] of nodes) {
        node.el.setAttribute("tabindex", key === activeKey ? "0" : "-1");
        if (node.item.kind !== "line") continue;
        node.el.setAttribute("aria-selected", String(key === s.selected));
        node.menu.setAttribute("tabindex", key === activeKey ? "0" : "-1");
      }
    }
    function focusRow(key) {
      activeKey = key; syncRows(); nodes.get(key)?.el.focus();
    }
    const cell = (text, cls, tone) => {
      const node = make("span", text, cls);
      if (tone) node.dataset.tone = tone;
      return node;
    };
    const caret = item => {
      const node = cell(item.expanded ? "▾" : "▸", "deviation-caret");
      node.setAttribute("aria-hidden", "true");
      return node;
    };
    const countCell = (value, bounded, cls) => value > 0 ? cell(`${bounded ? "≥" : ""}${num(value)}`, cls, cls === "deviation-calls" ? "error" : undefined) : cell("—", `${cls} deviation-muted`);
    const spoken = (value, bounded, noun) => bounded ? `${noun}: at least ${num(value)}, sampled` : `${noun}: ${num(value)}`;

    function rowNode(item) {
      const s = state(), el = make("div", undefined, `deviation-row deviation-${item.kind}`);
      el.setAttribute("role", "treeitem"); el.setAttribute("aria-level", String(item.level)); el.dataset.level = String(item.level); el.dataset.key = item.key;
      const name = make("span", undefined, "deviation-name"), reach = make("span", undefined, "deviation-reach");
      let calls = cell("", "deviation-calls"), refs = cell("", "deviation-refs"), menu = null;
      if (item.kind === "file") {
        const f = item.file;
        el.setAttribute("aria-expanded", String(item.expanded));
        name.append(caret(item), cell(f.path, "deviation-path"));
        for (const edge of f.edges) {
          const n = s.callsOnly ? edge.calls : edge.count;
          reach.append(cell(`${edge.direction === "up" ? "▲" : "⇄"} ${edge.destinationLayer} ${num(n)}`, "deviation-edge", "error"));
        }
        calls = countCell(f.calls, false, "deviation-calls"); refs = countCell(f.shown, false, "deviation-refs");
        const part = edge => `${edge.destinationLayer} ${s.callsOnly ? plural(edge.calls, "call") : plural(edge.count, "reference")}`;
        const up = f.edges.filter(edge => edge.direction === "up").map(part), cross = f.edges.filter(edge => edge.direction !== "up").map(part);
        el.setAttribute("aria-label", [`${f.path}`, plural(f.calls, "call"), plural(f.shown, "reference"),
          up.length ? `upward: ${up.join(", ")}` : null, cross.length ? `peer or shared: ${cross.join(", ")}` : null].filter(Boolean).join(", "));
      } else if (item.kind === "function") {
        const fn = item.fn;
        el.setAttribute("aria-expanded", String(item.expanded));
        name.append(caret(item), cell(fn.label, "deviation-label"));
        calls = countCell(fn.calls, !fn.callsExact, "deviation-calls"); refs = countCell(s.callsOnly ? fn.calls : fn.references, s.callsOnly ? !fn.callsExact : !fn.refsExact, "deviation-refs");
        el.setAttribute("aria-label", `${fn.label}, ${spoken(fn.calls, !fn.callsExact, "calls")}, ${s.callsOnly ? spoken(fn.calls, !fn.callsExact, "references") : spoken(fn.references, !fn.refsExact, "references")}`);
      } else if (item.kind === "more") {
        const u = item.file.unlisted;
        const text = s.callsOnly ? `+ ${num(u.call)} more ${u.call === 1 ? "call" : "calls"} not listed`
          : `+ ${num(KINDS.reduce((sum, kind) => sum + u[kind], 0))} more not listed: ${KINDS.filter(kind => u[kind] > 0).map(kind => plural(u[kind], ...NOUNS[kind])).join(" · ")}`;
        name.append(cell(text, "deviation-label deviation-muted"));
        el.setAttribute("aria-label", text);
      } else {
        const {line} = item, e = line.evidence, t = e.target, lineNo = e.lines?.start;
        const symbol = t?.displayKey ?? e.spelling ?? "";
        const tag = cell(TAGS[line.kind], "badge deviation-kind"); tag.dataset.kind = line.kind;
        name.append(cell(`L${lineNo ?? "—"}`, "deviation-line deviation-muted"), cell(symbol, "deviation-symbol"), tag);
        let where;
        // One line, as the design draws it: the file name and line; the full path is the tooltip.
        let target;
        if (t) {
          target = cell(`→ ${basename(t.document.path)}:${t.lines.start}`, "deviation-target");
          target.setAttribute("title", `${t.document.path}:${t.lines.start}`);
          where = `defined at ${t.document.path}:${t.lines.start}`;
        } else {
          const reason = REASONS[e.targetReason];
          const destination = (deps.targetLabel || (identity => identity.path))(line.instance.destination);
          target = cell(`→ ${basename(destination)} · file only`, "deviation-target");
          target.setAttribute("title", `${destination} · file only${reason ? `: ${reason}` : ""}`);
          where = `file only: ${reason ?? "no binding"}`;
        }
        reach.append(target);
        el.setAttribute("aria-selected", String(item.key === s.selected));
        el.setAttribute("aria-label", `Line ${lineNo ?? "unknown"}, ${symbol}, ${TAGS[line.kind]}, ${where}`);
        menu = make("button", "⋯", "deviation-actions"); menu.type = "button";
        menu.setAttribute("aria-haspopup", "menu");
        menu.setAttribute("aria-label", `Actions for line ${lineNo ?? "unknown"} in ${line.instance.target.path}`);
        menu.addEventListener("click", event => openMenu(event, line));
        el.addEventListener("contextmenu", event => openMenu(event, line));
        el.addEventListener("keydown", event => { if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) openMenu(event, line); });
      }
      el.append(name, reach, calls, refs);
      el.append(menu ?? cell("", "deviation-menu"));
      el.addEventListener("click", () => { activeKey = item.key; return activate(item); });
      nodes.set(item.key, {el, item, menu});
      return el;
    }
    function activate(item) {
      if (item.kind === "line") return select(item.line, true);
      if (item.expandable) return setOpen(item.key, !state().expanded.has(item.key), item.key);
      focusRow(item.key);
    }
    function setOpen(key, open, focus) {
      const s = state();
      if (open) s.expanded.add(key); else s.expanded.delete(key);
      renderTree(); focusRow(focus);
    }
    function treeKey(event) {
      if (event.target?.closest?.(".deviation-actions")) return;
      const index = visible.findIndex(item => item.key === activeKey);
      const result = model.keyAction(visible, index, event.key);
      if (!result) return;
      event.preventDefault();
      const s = state(), key = visible[result.focus]?.key;
      if (result.expand) return setOpen(result.expand, true, key);
      if (result.collapse) return setOpen(result.collapse, false, key);
      if (result.toggle) return setOpen(result.toggle, !s.expanded.has(result.toggle), key);
      focusRow(key);
      if (result.select) select(visible[result.focus].line, true);
    }

    function select(line, focus = false) {
      const s = state();
      s.selected = line.key; hiddenNotice = false; feedback = null; activeKey = line.key;
      renderNotices(); syncRows();
      if (focus) nodes.get(line.key)?.el.focus();
      lineActive = deps.inspectLine(line, (target, title, active) => panel(target, title, line, active)) || fallbackActive;
    }
    function openMenu(event, line) {
      if (!window.TrellisClasses?.showContextMenu) return;
      select(line);
      window.TrellisClasses.showContextMenu(event, actions(line), {label:"Line actions"});
    }

    const location = line => {
      const path = line.evidence.document?.path ?? line.instance.target.path, lines = line.evidence.lines;
      return lines ? `${path}:${lines.start}` : path;
    };
    function actions(line, active = lineActive || fallbackActive) {
      const e = line.evidence, i = line.instance, t = e.target, pin = deps.basis().pin, items = [];
      const guarded = run => async () => {
        try { await run(); } catch (error) { if (isActive() && error?.name !== "AbortError") notice(error?.message || "Action unavailable."); }
      };
      const add = (id, label, run) => items.push({id, label, run:guarded(run)});
      if (model.canOpenSequence(e)) add("sequence", "Open in Sequence", () => deps.openSequence(e.caller, [e], pin, deps.snapshot(), active));
      if (validLines(e.lines)) add("call-site", "Go to call site", () => deps.readSource({path:e.document?.path ?? i.target.path,
        range:{startLine:e.lines.start, endLine:e.lines.end, startByte:e.range.start, endByte:e.range.end}}, pin, active));
      if (t && validLines(t.lines)) add("definition", `Go to definition · ${t.document.path}:${t.lines.start}`, () => deps.readSource({path:t.document.path,
        range:{startLine:t.lines.start, endLine:t.lines.end}}, pin, active));
      add("copy", "Copy location", async () => {
        const where = location(line), clipboard = typeof navigator === "undefined" ? undefined : navigator.clipboard;
        try {
          if (!clipboard?.writeText) throw new Error("Clipboard unavailable.");
          await clipboard.writeText(where);
          if (isActive()) notice(`Copied ${where}`);
        } catch {
          if (isActive()) notice(`Copy failed. Location: ${where}`);
        }
      });
      if (deps.eligible({instance:i, role:"production"})) add("exception", "Add exception for this file pair…", async () => {
        await deps.beginDraft({type:"addExceptions", pairs:[{source:deps.portable(i.target), destination:deps.portable(i.destination)}], reason:""},
          model.exceptionNote(i.referenceCount, i.target.path, i.destination.path));
        get("architecture-reason")?.focus();
      });
      return items;
    }

    function panel(target, title, line, active) {
      const e = line.evidence, i = line.instance, t = e.target, up = line.direction === "up", where = location(line);
      if (title && typeof title === "object") title.textContent = where;
      const higher = up ? "a higher layer" : "another layer";
      const labels = {call:`Call · runs code in ${up ? "a higher layer" : "another layer"}`, type:`Type use · names a type from ${higher}`,
        import:`Import · brings in a module from ${higher}`, reference:`Reference · names an item from ${higher}`};
      target.append(make("p", labels[line.kind], "detail"), make("p", `in ${line.fn}${line.closure ? " (in a closure)" : ""}`));
      const list = make("dl");
      const row = (term, value, tone) => { const dd = make("dd", value); if (tone) dd.dataset.tone = tone; list.append(make("dt", term), dd); };
      row("Refers to", t ? t.displayKey : `${e.spelling ?? ""} (spelling, not a resolved symbol)`);
      row("Defined at", t ? `${t.document.path}:${t.lines.start}` : REASONS[e.targetReason] ?? "no binding");
      row("Layers", `${up ? "↑" : "⇄"} ${i.sourceLayer} → ${i.destinationLayer} ${up ? "upward " : ""}deviation`, "error");
      row("File pair", `${i.target.path} → ${i.destination.path}`);
      let evidence = e.kind === "semanticBinding" || t ? "Semantic binding" : "Structural · syntax evidence";
      if (e.caller) evidence += ` · run by ${e.caller.name}`;
      else if (line.kind === "call") evidence += ` · no measured caller: ${e.callerReason ?? "unavailable"}`;
      row("Evidence", evidence);
      target.append(list);
      const items = actions(line, active), exception = items.find(item => item.id === "exception");
      const primary = items.find(item => item.id === "sequence") || items.find(item => item.id === "call-site");
      const button = item => {
        const node = make("button", item.label, item === primary ? "deviation-primary" : "deviation-secondary"); node.type = "button"; node.dataset.action = item.id;
        node.addEventListener("click", () => { if (active()) return item.run(); });
        return node;
      };
      if (primary) target.append(button(primary));
      for (const item of items) if (item !== primary && item !== exception) target.append(button(item));
      if (exception) target.append(button(exception), make("p", model.exceptionNote(i.referenceCount, i.target.path, i.destination.path), "detail"));
    }

    return {open, openEdge, close, reset, reopenAfterRefresh, afterRefresh, snapshot, restore, isOpen:() => opened};
  }

  window.TrellisDeviations = Object.freeze({create, model});
})();
