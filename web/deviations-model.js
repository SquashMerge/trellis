"use strict";
// The Deviations tree's model: production $layers deviations grouped by file, function and line (#786).
// Pure and DOM-free: the view is architecture-deviations.js, which reads `model` from here, and the Herdr
// mod bundles this file alone.
(() => {
  const KINDS = ["call", "type", "import", "reference"];
  const MODULE = "(module)";
  const unnamed = name => typeof name !== "string" || !name || name.startsWith("<");
  // A closure belongs to the nearest enclosing *callable*: a named function, method or constructor, or a binding
  // whose whole initializer is a function (the outline's `callableInitializer`, #526). Variables, fields and
  // types are lexical parents, not owners (#860).
  const CALLABLE = new Set(["function", "method", "constructor"]);
  const callable = item => !unnamed(item.symbol.name) && (CALLABLE.has(item.symbol.kind) || CALLABLE.has(item.nativeKind) || item.callableInitializer === true);
  // A closure with no enclosing callable, read to the root of its file's outline, is module-level code.
  const MODULE_LEVEL = Object.freeze({syntaxId:"(module)", moduleLevel:true});
  const record = value => value !== null && typeof value === "object" && !Array.isArray(value);
  const counted = value => Number.isSafeInteger(value) && value >= 0;
  const zeros = () => ({call:0, type:0, import:0, reference:0});
  const edgeKey = (source, destination) => `${source}→${destination}`;
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
    // A line's symbol is its resolved target's written name without its path, qualified by its named ancestors
    // (`Type::method` in Rust, `Class.method` elsewhere). An unnamed Rust impl block reads its type from `impls`
    // (from `implNames`, the Files outline) and is otherwise left out. A site with no target, or whose target has no named
    // segment, reads its spelling (#911, #941).
    const ANONYMOUS = /^[A-Za-z_]+@\d+$/, IMPL = /^implementation@\d+$/;
    const segmentsOf = target => {
      const key = target.displayKey, path = target.document?.path;
      let rest = typeof path === "string" && key.startsWith(`${path}#`) ? key.slice(path.length + 1) : key.slice(key.indexOf("#") + 1);
      if (typeof path === "string" && rest.startsWith(`${path}.`)) rest = rest.slice(path.length + 1);
      return rest.split(".");
    };
    const symbolOf = (evidence, impls) => {
      const t = evidence.target;
      if (!record(t) || typeof t.displayKey !== "string") return typeof evidence.spelling === "string" ? evidence.spelling : "";
      const types = impls?.get(t.syntaxId) ?? [];
      let next = 0;
      const parts = segmentsOf(t).map(part => !ANONYMOUS.test(part) ? part : IMPL.test(part) ? types[next++] ?? null : null).filter(Boolean);
      return parts.join(t.document?.language === "rust" ? "::" : ".") || (typeof evidence.spelling === "string" && evidence.spelling) || t.displayKey;
    };
    // `names` maps an unnamed owner's syntax ID to its nearest named enclosing callable (from `closureNames`);
    // `impls` maps a target's syntax ID to the types of its unnamed impl blocks (from `implNames`).
    function assemble(rows, layers, {edge = null, callsOnly = false} = {}, names = null, impls = null) {
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
          // Every module-level line of a file shares one row: no owner, the file module itself, or a closure with no callable.
          const id = owner && !owner.moduleLevel && owner.syntaxId !== i.target.sid ? owner.syntaxId : MODULE;
          if (!file.functions.has(id)) file.functions.set(id, {key:id, id:JSON.stringify(["fn", key, id]), label:labelOf(owner, i), lines:[], callsExact:true, refsExact:true});
          const fn = file.functions.get(id);
          fn.callsExact &&= callsExact; fn.refsExact &&= refsExact;
          fn.lines.push({key:lineKey(i, evidence), evidence, instance:i, kind, fn:fn.label, closure:!!enclosing, direction:direction(i.sourceLayer, i.destinationLayer),
            start:evidence.lines?.start ?? 0, symbol:symbolOf(evidence, impls)});
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
    const build = (rows, layers, names = null, impls = null) => ({...assemble(rows, layers, {}, names, impls), rows, layers, names, impls});
    const filter = (tree, options) => ({...assemble(tree.rows, tree.layers, options, tree.names, tree.impls), rows:tree.rows, layers:tree.layers, names:tree.names, impls:tree.impls});
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
      const known = knownOwners(rows), byId = outlineById(outline);
      for (const id of ids) {
        if (!byId.get(id)) continue;
        const item = enclosingItem(byId, id);
        if (!item) { if (reachesRoot(byId, id)) names.set(id, MODULE_LEVEL); continue; }
        if (callable(item)) names.set(id, ownerRecord(item, byId, known));
      }
      return names;
    }
    // The owners the rows already name, by syntax ID.
    function knownOwners(rows) {
      const known = new Map();
      for (const {instance:i} of rows) for (const evidence of i.evidence) if (evidence.owner && (evidence.owner.stableLookupKey || evidence.owner.name)) known.set(evidence.owner.syntaxId, evidence.owner);
      return known;
    }
    function outlineById(outline) {
      const byId = new Map();
      for (const item of Array.isArray(outline) ? outline : []) if (record(item?.symbol) && typeof item.symbol.id === "string") byId.set(item.symbol.id, item);
      return byId;
    }
    // The first callable above `id`; undefined when the chain leaves the outline; a non-callable item at the hop limit.
    function enclosingItem(byId, id) {
      let item = byId.get(byId.get(id).symbol.parent), hops = 0;
      for (; item && !callable(item) && hops < 64; hops++) item = byId.get(item.symbol.parent);
      return item;
    }
    // The walk left the outline: at the root (`parent: null`) it is module level, otherwise unknown.
    function reachesRoot(byId, id) {
      let last = byId.get(id), steps = 0;
      while (last && last.symbol.parent != null && byId.has(last.symbol.parent) && steps++ < 64) last = byId.get(last.symbol.parent);
      return !!last && last.symbol.parent == null;
    }
    function ownerRecord(item, byId, known) {
      const parent = byId.get(item.symbol.parent), type = typeof parent?.implTarget === "string" && parent.implTarget ? parent.implTarget : null;
      return known.get(item.symbol.id) ?? {syntaxId:item.symbol.id, name:type ? `${type}::${item.symbol.name}` : item.symbol.name, stableLookupKey:null};
    }
    // The Rust targets whose written key passes through an unnamed impl block, by file: their type is in the outline.
    function implSites(rows) {
      const files = new Map();
      for (const {instance:i} of rows) for (const evidence of i.evidence) {
        const t = evidence.target;
        if (!record(t) || typeof t.displayKey !== "string" || typeof t.syntaxId !== "string" || t.document?.language !== "rust" || typeof t.document.path !== "string" ||
          !segmentsOf(t).some(part => IMPL.test(part))) continue;
        if (!files.has(t.document.path)) files.set(t.document.path, new Set());
        files.get(t.document.path).add(t.syntaxId);
      }
      return files;
    }
    // Each target's unnamed impl blocks, outermost first, as the types they implement (null when unknown).
    function implNames(ids, outline, impls = new Map()) {
      const byId = new Map();
      for (const item of Array.isArray(outline) ? outline : []) if (record(item?.symbol) && typeof item.symbol.id === "string") byId.set(item.symbol.id, item);
      for (const id of ids) {
        const item = byId.get(id), types = [];
        if (!item) continue;
        for (let parent = byId.get(item.symbol.parent), hops = 0; parent && hops < 64; parent = byId.get(parent.symbol.parent), hops++) {
          if (unnamed(parent.symbol.name) && (parent.nativeKind === "implementation" || "implTarget" in parent)) {
            types.unshift(typeof parent.implTarget === "string" && parent.implTarget ? parent.implTarget : null);
          }
        }
        impls.set(id, types);
      }
      return impls;
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
    return Object.freeze({lineKey, valid, build, filter, chips, exceptionNote, keyAction, canOpenSequence, kindOf, unnamedOwners, closureNames, implSites, implNames});
  })();
  window.TrellisDeviationsModel = Object.freeze({model, KINDS, edgeKey});
})();
