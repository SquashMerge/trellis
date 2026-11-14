"use strict";
// Pele supplies layout only. Native DTO records remain the evidence owner.
(() => {
  const NS = "http://www.w3.org/2000/svg";
  const {unsupported, bytes, themeOnce, asButton, textMeasurer} = window.TrellisPeleSvg;
  const {plan:planRoutes, paths:routePaths, placement, countLanes, channelWidth, round} = window.TrellisArchitectureRouting;
  const ROOT_STYLE = window.TrellisPeleSvg.rootStyle(14);
  const tags = new Set(["svg","g","path","rect","text","tspan"]);
  const attrs = new Set(["xmlns","class","viewBox","width","height","fill","fill-opacity","role","aria-roledescription","transform","stroke","font-size","data-id","d","x","y","rx","text-anchor","font-weight","stroke-dasharray","stroke-linecap","stroke-width","xml:space"]);
  const ADMISSION = {kind:"block", theme:raw => themeOnce(raw, ROOT_STYLE), active:/<\/?(?:script|style|foreignObject|image|a)\b/i, tags, attrs, words:{
    large:"Architecture SVG too large", root:"Changed Pele block root/theme", theme:"Changed Pele block root/theme", style:"Unexpected inline Architecture style",
    active:"Unexpected Architecture SVG element", invalid:"Invalid Architecture SVG", element:"Unexpected Architecture SVG element", attribute:"Unexpected Architecture SVG attribute"}};
  const colors = ["#343644","#FFC20A","#FFAA10","#FF7A00"];
  const identity = module => JSON.stringify([module.identity?.sourceSetId,module.identity?.language,module.identity?.path,module.identity?.sid]);
  const edgeKey = edge => JSON.stringify([edge.sourceLayer,edge.destinationLayer,edge.role,edge.status,edge.excepted,edge.exceptionReason]);
  // Display chips: compact module/file chips, more per band row, and a narrow shared box.
  // CHIP_CELL is one drawn chip column (chip, padding and grid gap) at 14px Inter; the
  // summary display fits as many columns as the canvas width holds (#449). The chip text
  // width then grows or shrinks so the drawing fills the panel width (#525).
  const MAX_STACK_SPAN = 4, CHIP_WIDTH = 104, MIN_CHIP = 72, MAX_CHIP = 240, CHIP_CELL = 176, CHIP_COLUMNS = 5, MAX_COLUMNS = 14, SHARED_ROWS = 6, STAT_GAP = "   ";
  // Pele's drawn node padding around a label and the grid gap between siblings.
  const NODE_PAD = 32, GRID_GAP = 12;
  // A stat chip is its text plus 12 units of padding, 6 apart (see statChips).
  const STAT_PAD = 12, STAT_SPACING = 6;
  // Lines one wrapped label line may take before its last is clipped: a bound for
  // pathological repository names only; ordinary paths and layer names wrap whole.
  const MAX_LINES = 12;
  // Arrow stroke width by reference-site count (#637): log scale from WIDTH_MIN at one site to
  // WIDTH_MAX at WIDTH_SPAN or more sites, so one large edge never swamps the rest.
  const WIDTH_MIN = 1.5, WIDTH_MAX = 7, WIDTH_SPAN = 1000;
  function edgeWidth(count) {
    const t = Math.min(1, Math.max(0, Math.log10(Math.max(1, count)) / Math.log10(WIDTH_SPAN)));
    return Math.round((WIDTH_MIN + (WIDTH_MAX - WIDTH_MIN) * t) * 100) / 100;
  }
  // Chip columns for the ordered stack: the canvas width less one column per shared box.
  function chipColumns(width, shared) {
    if (!Number.isFinite(width) || width <= 0) return CHIP_COLUMNS;
    return Math.max(1, Math.min(MAX_COLUMNS, Math.floor(width / CHIP_CELL) - shared));
  }
  // Greedy word wrap at separators (space, ·, _, -, /, .); an unbreakable run is split
  // by characters. Labels wrap rather than clip, so the drawn text is the whole text.
  function wrap(line, width, measure, maxLines) {
    if (measure(line) <= width) return [line];
    const lines = [];
    let current = "";
    const fits = text => measure(text.trimEnd()) <= width;
    for (const token of line.split(/(?<=[ ·_\-/.])/)) {
      if (fits(current + token)) { current += token; continue; }
      if (current.trim()) lines.push(current.trimEnd());
      current = token.trimStart();
      while (!fits(current)) {
        // Split by code point, so a surrogate pair never straddles two lines.
        const chars = Array.from(current);
        let cut = 1;
        while (cut < chars.length && fits(chars.slice(0, cut + 1).join(""))) cut++;
        lines.push(chars.slice(0, cut).join("")); current = chars.slice(cut).join("");
      }
    }
    if (current.trim() || !lines.length) lines.push(current.trimEnd());
    if (lines.length <= maxLines) return lines;
    const kept = lines.slice(0, maxLines);
    let last = Array.from(kept[maxLines - 1]);
    while (last.length && measure(`${last.join("")}…`) > width) last.pop();
    kept[maxLines - 1] = `${last.join("")}…`;
    return kept;
  }
  // A chip's drawn width. A stat with a `tag` (the visible "Selected" word, shown by CSS while the
  // chip is pressed) always reserves room for it, so selecting never changes the layout.
  const statWidth = (stat, measure) => Math.ceil(measure(stat.text)) + STAT_PAD + (stat.tag ? Math.ceil(measure(stat.tag)) + STAT_PAD : 0);
  // Stat chips grouped into header lines no wider than `width` once drawn.
  function statLines(stats, width, measure) {
    const lines = [];
    let line = [], used = 0;
    for (const stat of stats) {
      const drawn = statWidth(stat, measure);
      if (line.length && used + STAT_SPACING + drawn > width) { lines.push(line); line = []; used = 0; }
      used += (line.length ? STAT_SPACING : 0) + drawn; line.push(stat);
    }
    if (line.length) lines.push(line);
    return lines;
  }
  const ARROWS = {downward:"↓", upward:"↑", peer:"↔", "into shared":"→", "shared outbound":"←"};
  const MIRRORED = {"into shared":"←", "shared outbound":"→"};
  function direction(source, destination) {
    if (destination.shared) return "into shared";
    if (source.shared) return "shared outbound";
    return destination.order > source.order ? "downward" : destination.order < source.order ? "upward" : "peer";
  }
  function project(view, options = {}) {
    // The summary display is the caller's: drawn layers, expansion, directory entries and chips.
    const display = options.display;
    if (!display) unsupported("Architecture display unavailable");
    if (view?.state !== "ready" || !Array.isArray(view.layers) || !Array.isArray(view.modules) || !Array.isArray(view.edges)) unsupported("Invalid Architecture projection");
    const nodes = new Map(), edges = new Map(), layers = new Map(), native = new Set();
    const measure = options.measureText || (text => Array.from(text).length * 14);
    const chip = Number.isFinite(options.chipWidth) ? Math.max(MIN_CHIP, Math.min(MAX_CHIP, Math.floor(options.chipWidth))) : CHIP_WIDTH;
    let serial = 0;
    const clean = text => String(text).replace(/[\u0000-\u0009\u000b-\u001f\u007f]/g, " ");
    // `shown` is the drawn text when it is shorter than the accessible `text`.
    function label(kind, record, text, shown = text, width = null) {
      const id = kind + (serial++).toString(36), value = clean(text);
      // Uniform safe placeholders reserve width before repository text is inserted.
      // A line wraps at separators to the reserved width (#525); the accessible name keeps the full label.
      const lineWidth = width ?? (kind === "M" ? 150 : 200);
      // Edge counts are one line, sized to their text, so they never wrap.
      const lines = clean(shown).split("\n");
      const display = (kind === "E" ? [lines.join(" ")] : lines.flatMap(line => wrap(line, lineWidth, measure, MAX_LINES))).map(line => line || " ");
      const reserve = "W".repeat(Math.ceil(lineWidth / measure("W")));
      const firstReserve = "W".repeat(Math.max(1,Math.ceil((lineWidth - measure(id + "_")) / measure("W"))));
      const placeholders = display.map((_, i) => i === 0 ? id + "_" + firstReserve : reserve);
      return {id,kind,record,display,lineWidth,full:value,placeholder:placeholders.join("<br/>"),expected:placeholders.join("")};
    }
    // `host` is the band (layer or unassigned row) a node is drawn in; routing moves it with that band.
    let host = null;
    function addNode(kind, record, text, shown, width) { const item = label(kind,record,text,shown,width); item.host = host; nodes.set(item.id,item); return item; }
    const source = ["block-beta","columns 1"];
    function open(item, columns, span = 1) { item.composite = true; source.push(`block:${item.id}["${item.placeholder}"]:${span}`, `columns ${columns}`); }
    const shownShared = view.layers.filter(l => l.shared && display.layers.has(l.name)).length;
    const COLUMNS = options.columns ?? chipColumns(options.width, shownShared);
    display.reset?.();
    // The stack title reserves no more than one chip, so it never sets the stack's width.
    const orderedStack = addNode("B",null,options.title ?? "Ordered layers",options.title ?? "Ordered layers",chip);
    const names = new Set();
    for (const layer of view.layers) {
      if (typeof layer.name !== "string" || names.has(layer.name) || typeof layer.shared !== "boolean" || (!layer.shared && !Number.isSafeInteger(layer.order))) unsupported("Invalid or duplicate Architecture layer");
      names.add(layer.name);
    }
    const drawn = view.layers.filter(l => display.layers.has(l.name));
    const ordered = drawn.filter(l => !l.shared).sort((a,b) => a.order-b.order || a.name.localeCompare(b.name));
    const shared = drawn.filter(l => l.shared).sort((a,b) => a.name.localeCompare(b.name));
    // Peers share one band row; a band header is clipped to its band's chip columns.
    const peerCount = order => ordered.filter(l => l.order === order).length;
    const width = Math.max(1, ...ordered.map(l => peerCount(l.order)));
    const bandColumns = layer => Math.max(1, Math.floor(COLUMNS * Math.floor(width / peerCount(layer.order)) / width));
    // A header wraps within its band's chip row, so it never widens or clips the band.
    const rowWidth = columns => columns * (chip + NODE_PAD) + (columns - 1) * GRID_GAP - NODE_PAD;
    const headerWidth = layer => layer.shared ? chip : Math.max(chip, rowWidth(bandColumns(layer)));
    for (const layer of drawn) {
      const header = options.layerLabel?.(layer) ?? `${layer.name}\n${layer.shared ? "Shared" : `Order ${options.layerNumber?.(layer) ?? layer.order}`}`;
      // Non-zero reference counts become compact chips on their own header lines.
      const stats = (options.layerStats?.(layer) || []).filter(stat => typeof stat?.text === "string" && stat.text);
      const widest = stats.reduce((most, stat) => Math.max(most, statWidth(stat, measure)), 0);
      const lineWidth = Math.max(headerWidth(layer), widest), groups = statLines(stats, lineWidth, measure);
      const item = addNode("L",layer,stats.length ? `${header}\n${stats.map(stat => stat.label || stat.text).join(" · ")}` : header,
        [header, ...groups.map(group => group.map(stat => stat.text).join(STAT_GAP))].join("\n"),lineWidth);
      // The stat lines are the last lines; each fits its width, so wrapping leaves them whole.
      item.statLines = groups.map((group, i) => ({stats:group, line:item.display.length - groups.length + i}));
      item.host = item.id; layers.set(layer.name,item);
    }
    const moduleRows = new Map(view.layers.map(l => [l.name,[]])); moduleRows.set(null,[]);
    for (const module of view.modules) {
      if (!module.identity || !["sourceSetId","language","path","sid"].every(k => typeof module.identity[k] === "string") || native.has(identity(module)) || !moduleRows.has(module.layer)) unsupported("Invalid or duplicate Architecture module");
      native.add(identity(module)); moduleRows.get(module.layer).push(module);
    }
    const heat = band => Number.isInteger(band) && band >= 0 && band < 4 ? band : null;
    function leaf(item, span = 1) { source.push(`${item.id}["${item.placeholder}"]${span > 1 ? `:${span}` : ""}`); }
    // A named chip draws its short module or file name; the accessible label keeps the path.
    function moduleNode(module, name) {
      const value = options.metric === "layer" ? undefined : options.moduleValue?.(module);
      const full = `${module.identity.path}${value === null ? "\nUnavailable" : value === undefined ? "" : `\n${value}`}`;
      const item = typeof name === "string" ? addNode("M",module,full,`${name}${value === null ? "\n—" : value === undefined ? "" : `\n${value}`}`,chip) : addNode("M",module,full);
      item.band = heat(options.moduleBand?.(module)); leaf(item);
    }
    function emitEntries(entries, span) {
      if (!entries.length) leaf(addNode("B",null,"No files shown","No files shown",chip));
      for (const entry of entries) {
        if (entry.type === "module") moduleNode(entry.module, entry.name);
        else if (entry.type === "directory") {
          const item = addNode("D",entry,entry.label,entry.open ? entry.label : entry.shown ?? entry.label,entry.open ? rowWidth(span) : chip);
          item.band = entry.open ? null : heat(entry.band);
          if (entry.open) { open(item,span,span); emitEntries(entry.entries,span); source.push("end"); } else leaf(item);
        } else if (entry.type === "note") leaf(addNode("N",entry,entry.label,entry.shown ?? entry.label,chip));
        else if (entry.type === "chip") { const item = addNode("C",entry,entry.label,entry.shown ?? entry.label,chip); item.band = heat(entry.band); leaf(item); }
        else unsupported("Invalid Architecture display entry");
      }
    }
    // Pele gives a leaf row the height of its tallest sibling, so a folded band is a composite
    // whose header is the one toggle, over a short non-interactive file count.
    function folded(item, record, text, span) {
      open(item,1,span);
      const summary = addNode("S",record,text,text,chip); summary.owner = item; leaf(summary);
      source.push("end");
    }
    function emitLayer(layer, span = 1) { host = layers.get(layer.name).id; emitBand(layer, span); host = null; }
    function emitBand(layer, span) {
      // Peers split the chip columns of one band row, so the stack keeps one width.
      const item = layers.get(layer.name), columns = layer.shared ? 1 : bandColumns(layer);
      const members = moduleRows.get(layer.name).sort((a,b) => identity(a).localeCompare(identity(b)));
      // A collapsed band shows one row of module chips coloured by metric (#449), not a file count.
      if (!item.expanded) {
        const chips = display.summary ? display.summary(layer.name,members,layer.shared ? SHARED_ROWS : columns) : [];
        if (!chips.length) { folded(item,layer,`${members.length} files folded`,span); return; }
        open(item,columns,span); emitEntries(chips,columns); source.push("end"); return;
      }
      open(item,columns,span); emitEntries(opened.get(layer.name),columns); source.push("end");
    }
    // Open bands and an open unassigned row charge the chip budget first, in drawing order,
    // so collapsed summary rows only use what remains and never fold a band the viewer opened.
    const opened = new Map(), unassigned = moduleRows.get(null);
    for (const layer of [...ordered, ...shared]) {
      const item = layers.get(layer.name); item.expanded = !!display.expanded(layer.name);
      if (item.expanded) opened.set(layer.name, display.entries(layer.name,moduleRows.get(layer.name).sort((a,b) => identity(a).localeCompare(identity(b)))));
    }
    if (display.unassigned && unassigned.length && display.unassigned.expanded) opened.set(null, display.entries(null,unassigned));
    // Pele stretches a composite's children to equal grid columns, so the stack spans
    // several columns and each shared box one, keeping the shared boxes narrow. At one chip
    // column the fit may ask the stack to span more grid columns than it has chip columns
    // (`stackSpan`), so the widest band header no longer sets every shared box's width (#860).
    const stackSpan = Math.max(COLUMNS, Number.isSafeInteger(options.stackSpan) ? options.stackSpan : 0), orders = [...new Set(ordered.map(l => l.order))];
    source[1] = `columns ${stackSpan + shared.length}`;
    // All apps (#850) may put the shared boxes left of the stack, nearest box first beside it. They are
    // still projected after the stack, so chip budgets charge in the same order; only their source moves.
    const sharedLeft = options.sharedSide === "left" && shared.length > 0, stackAt = source.length;
    open(orderedStack,width,stackSpan);
    if (!ordered.length) leaf(addNode("B",null,"No ordered layers shown"));
    for (const order of orders) {
      const peers = ordered.filter(l => l.order === order), span = Math.floor(width / peers.length);
      peers.forEach(layer => emitLayer(layer,span));
      if (width > span * peers.length) source.push(`space:${width - span * peers.length}`);
    }
    source.push("end");
    const boxes = shared.map(layer => { const at = source.length; emitLayer(layer); return source.slice(at); });
    if (sharedLeft) { const tail = source.splice(stackAt), lines = boxes.flat().length; source.push(...boxes.reverse().flat(), ...tail.slice(0, tail.length - lines)); }
    let unassignedId = null;
    if (display.unassigned && unassigned.length) {
      const item = addNode("U",display.unassigned,display.unassigned.label,display.unassigned.label,rowWidth(COLUMNS + shared.length)); item.expanded = opened.has(null);
      item.host = unassignedId = host = item.id;
      if (item.expanded) { open(item,COLUMNS + shared.length,stackSpan + shared.length); emitEntries(opened.get(null),COLUMNS + shared.length); source.push("end"); }
      else {
        const chips = display.summary ? display.summary(null,unassigned,COLUMNS + shared.length) : [];
        if (chips.length) { open(item,COLUMNS + shared.length,stackSpan + shared.length); emitEntries(chips,COLUMNS + shared.length); source.push("end"); }
        else folded(item,display.unassigned,`${unassigned.length} files folded`,stackSpan + shared.length);
      }
      host = null;
    }
    const byName = new Map(view.layers.map(layer => [layer.name,layer])), seenEdges = new Set();
    for (const edge of view.edges) {
      if (!names.has(edge.sourceLayer) || !names.has(edge.destinationLayer) || edge.sourceLayer === edge.destinationLayer || !["production","test"].includes(edge.role) || !["conforms","deviates","unknown"].includes(edge.status) || !Number.isSafeInteger(edge.count) || edge.count <= 0 || typeof edge.excepted !== "boolean" || seenEdges.has(edgeKey(edge))) unsupported("Invalid or duplicate Architecture edge");
      seenEdges.add(edgeKey(edge));
      if (edge.role === "test" && !options.showTests) continue;
      if (!layers.has(edge.sourceLayer) || !layers.has(edge.destinationLayer) || !display.edge(edge)) continue;
      const from = layers.get(edge.sourceLayer).id, to = layers.get(edge.destinationLayer).id;
      const state = edge.status === "deviates" ? "deviation" : edge.status, way = direction(byName.get(edge.sourceLayer),byName.get(edge.destinationLayer));
      // Summary arrows draw a compact count; the accessible label keeps the words. The caller
      // may qualify both: a lower bound and the source layer's mapped-site coverage (#449).
      const words = options.edgeText?.(edge, way) || {};
      const full = `${edge.count} ${edge.role} sites · ${way}\n${state}${edge.excepted ? " · exception" : ""}${words.label ? ` · ${words.label}` : ""}`;
      // A shared box left of the stack (#850) mirrors the two shared glyphs, so a tag points the way its arrow runs.
      const glyph = sharedLeft && MIRRORED[way] ? MIRRORED[way] : ARROWS[way];
      const shown = `${glyph}${words.bound ? "≥" : ""}${edge.count}${edge.excepted ? " excepted" : ""}${edge.role === "test" ? " test" : ""}${words.note ? ` · ${words.note}` : ""}`;
      const item = label("E",edge,full,shown,Math.ceil(measure(`${shown}…`)) + 2);
      Object.assign(item,{from,to,tone:options.edgeTone?.(edge) || null}); edges.set(item.id,item);
      // The adapter routes its own arrows (#637): Pele lays out the boxes only, so its
      // edge-label placement never widens the drawing. A tag is its 12px text plus padding.
      item.tagWidth = Math.ceil(measure(item.display[0]) * 12 / 14) + 12;
    }
    const text = source.join("\n") + "\n";
    if (bytes(text) > 512 * 1024) unsupported("Architecture Mermaid source too large");
    // The band rows (peers share a row), shared boxes in drawing order, the ordered stack and
    // the unassigned row: what arrow routing needs (#637).
    const layout = {rows:orders.map(order => ordered.filter(l => l.order === order).map(l => layers.get(l.name).id)),
      shared:shared.map(l => layers.get(l.name).id), stack:orderedStack.id, unassigned:unassignedId, sharedLeft};
    return {source:text,nodes,edges,view,layout};
  }
  function validatedSvg(raw, projection) {
    const {root} = window.TrellisPeleSvg.admit(raw, ADMISSION);
    const controls = new Map(), seen = new Set();
    for (const node of root.querySelectorAll(".pele-cluster, .pele-node")) {
      const id = node.getAttribute("data-id"), expected = projection.nodes.get(id);
      if (!expected || seen.has(id)) unsupported("Missing, duplicate or changed Architecture node identity");
      const expectedClass = expected.composite ? "pele-cluster" : "pele-node pele-shape-rect";
      if (node.getAttribute("class") !== expectedClass || node.children.length !== 2 || node.children[0].localName !== "rect" || node.children[1].localName !== "text") unsupported("Changed Architecture node structure");
      seen.add(id);
      const label = node.querySelector(expected.composite ? ".pele-cluster-label" : ".pele-label");
      if (!label || label.textContent !== expected.expected) unsupported("Changed Architecture node placeholder");
      controls.set(id,{node,label,item:expected});
    }
    if (seen.size !== projection.nodes.size) unsupported("Missing Architecture node");
    // Pele draws the boxes only: the arrows are the adapter's own (#637), so a connector or a
    // count box in Pele's output is refused, whatever its shape.
    if (root.querySelector(".pele-edge, .pele-edge-label") || root.querySelectorAll("[data-id]").length !== projection.nodes.size) unsupported("Missing or unexpected Architecture edge");
    if (root.querySelectorAll("text").length !== projection.nodes.size || root.querySelectorAll("rect").length !== projection.nodes.size || root.querySelectorAll("path").length !== 0) unsupported("Unexpected Architecture SVG decoration");
    arrowShapes(root, controls, projection);
    return {root,controls};
  }
  // The adapter's own connector and count tag for each arrow (#637), in Pele's shape:
  // a connector line and arrowhead, and a tag box with its text. Routing gives them geometry.
  function arrowShapes(root, controls, projection) {
    const make = (name, attributes) => { const node = document.createElementNS(NS, name); for (const [key, value] of Object.entries(attributes)) node.setAttribute(key, String(value)); return node; };
    const lines = make("g", {class:"pele-edges", fill:"none", stroke:"var(--_l)"}), tags = make("g", {class:"pele-edge-labels", "font-size":"12"});
    let serial = 0;
    for (const item of projection.edges.values()) {
      const id = `${++serial}-${item.from}-${item.to}`, path = make("g", {class:"pele-edge", "data-id":id}), line = make("path", {d:"M0,0"});
      if (item.record.role === "test") line.setAttribute("stroke-dasharray", "3 4");
      path.append(line, make("path", {class:"pele-marker", d:"M0,0", fill:"var(--_l)", stroke:"none"}));
      const node = make("g", {class:"pele-edge-label", "data-id":id}), label = make("text", {x:item.tagWidth / 2, y:13.2, "text-anchor":"middle"});
      label.textContent = item.expected;
      node.append(make("rect", {x:0, y:0, width:item.tagWidth, height:18, rx:3, fill:"var(--_bg)"}), label);
      lines.append(path); tags.append(node); controls.set(item.id, {node, label, item, path});
    }
    root.append(lines, tags);
  }
  function replaceLabel(label, item) {
    const spans = [...label.querySelectorAll("tspan")];
    if (spans.length) {
      if (spans.length !== item.display.length) unsupported("Changed Architecture line wrapping");
      spans.forEach((span,i) => { span.textContent = item.display[i]; });
    } else { if (item.display.length !== 1) unsupported("Changed Architecture label wrapping"); label.textContent = item.display[0]; }
  }
  // Replace each reserved header stat line with one compact chip per non-zero count.
  // Deviations use the error colour and keep their glyph, so colour is never the only cue.
  // A stat with an `action` that has a callback is its own focusable button, the next sibling
  // of its band; the returned entries let routing move it with the band.
  function statChips(node, label, item, measure, handlers = {}) {
    const actions = [];
    const spans = [...label.querySelectorAll("tspan")];
    for (const {stats, line:index} of item.statLines) {
      const line = spans[index];
      const x = Number(line?.getAttribute("x")), y = Number(line?.getAttribute("y"));
      if (!line || !Number.isFinite(x) || !Number.isFinite(y)) unsupported("Invalid Architecture header geometry");
      const widths = stats.map(stat => statWidth(stat, measure)), gap = STAT_SPACING;
      const total = widths.reduce((sum, width) => sum + width, 0) + gap * (widths.length - 1);
      const anchor = line.getAttribute("text-anchor") || label.getAttribute("text-anchor");
      let left = anchor === "middle" ? x - total / 2 : anchor === "end" ? x - total : x;
      line.textContent = "";
      stats.forEach((stat, i) => {
        const color = stat.tone === "error" ? "#FF8E9A" : "#B8BAC8", chip = document.createElementNS(NS,"rect"), text = document.createElementNS(NS,"text");
        for (const [name,value] of Object.entries({x:left,y:y-14,width:widths[i],height:18,fill:"#14151D",stroke:color,rx:3})) chip.setAttribute(name,String(value));
        for (const [name,value] of Object.entries({x:left + widths[i] / 2,y,fill:color,"text-anchor":"middle","font-size":"14"})) text.setAttribute(name,String(value));
        text.setAttribute("class","trellis-architecture-stat"); text.setAttribute("data-tone",stat.tone === "error" ? "error" : "neutral");
        text.textContent = stat.text; const origin = left; left += widths[i] + gap;
        if (stat.tag) text.setAttribute("x",String(origin + (Math.ceil(measure(stat.text)) + STAT_PAD) / 2));
        const callback = stat.action ? (stat.action.interface !== undefined ? handlers.onSelectInterface : handlers.onOpenDeviation) : null;
        if (!callback) { node.append(chip,text); return; }
        const button = document.createElementNS(NS,"g");
        button.setAttribute("class","trellis-architecture-stat-action");
        asButton(button,stat.label || stat.text,event => { event?.stopPropagation(); if (handlers.isCurrent?.() !== false) callback(stat.action); });
        button.setAttribute("data-trellis-layer-action",item.record.name);
        button.append(chip,text); actions.push({node:button,stat});
        if (typeof stat.id === "string") button.setAttribute("data-trellis-action-id",stat.id);
        if (stat.tag) {
          // Pressed chips show the word through CSS; the chip's name and aria-pressed carry the state for assistive technology.
          const tagLeft = origin + Math.ceil(measure(stat.text)) + STAT_PAD, tagWidth = widths[i] - (tagLeft - origin) - 2;
          const back = document.createElementNS(NS,"rect"), tag = document.createElementNS(NS,"text");
          for (const [name,value] of Object.entries({x:tagLeft,y:y-12,width:tagWidth,height:14,rx:3,"class":"architecture-selected-tag-back","aria-hidden":"true"})) back.setAttribute(name,String(value));
          for (const [name,value] of Object.entries({x:tagLeft + tagWidth / 2,y,"text-anchor":"middle","font-size":"14","aria-hidden":"true","class":"architecture-selected-tag"})) tag.setAttribute(name,String(value));
          tag.textContent = stat.tag;
          button.setAttribute("aria-pressed","false"); button.append(back,tag);
        }
      });
    }
    return actions;
  }
  // Arrow routing (#637) is architecture-routing.js's: it plans every arrow from the frames Pele
  // drew and says how far each band moves. The adapter reads those frames, moves the drawing
  // and draws the polylines. Layout only: no record, count or identity changes.
  const num = (node, name) => Number(node.getAttribute(name));
  function frame(control) {
    const rect = control?.node.querySelector(":scope > rect");
    return rect ? {x:num(rect,"x"), y:num(rect,"y"), w:num(rect,"width"), h:num(rect,"height")} : null;
  }
  // Move one drawn cluster or node, and the chips and text it owns.
  function shift(node, dx, dy) {
    if (!dx && !dy) return;
    if (node.classList.contains("pele-node")) {
      const at = /translate\(\s*(-?[\d.]+)[ ,]+(-?[\d.]+)\s*\)/.exec(node.getAttribute("transform") || "");
      if (!at) unsupported("Invalid Architecture node geometry");
      node.setAttribute("transform", `translate(${round(Number(at[1]) + dx)},${round(Number(at[2]) + dy)})`);
      return;
    }
    for (const part of node.querySelectorAll("rect, text, tspan")) {
      if (part.hasAttribute("x")) part.setAttribute("x", String(round(num(part,"x") + dx)));
      if (part.hasAttribute("y")) part.setAttribute("y", String(round(num(part,"y") + dy)));
    }
  }
  // The width the side channels will add to Pele's drawing, so the fit leaves room for them.
  // It counts lanes with routing's own relations, so routing never draws wider.
  function channelReserve(projection) {
    const layout = projection.layout;
    const lanes = countLanes(layout, projection.edges.values());
    // The channel between the stack and the shared boxes already has the grid gap (#850: on either side).
    const gap = layout.shared.length ? GRID_GAP : 0;
    return layout.sharedLeft ? Math.max(0, channelWidth(lanes.left) - gap) + channelWidth(lanes.right)
      : channelWidth(lanes.left) + Math.max(0, channelWidth(lanes.right) - gap);
  }
  // Move every band, chip and header with its row; widen the stack and shared boxes to match.
  function shiftBands(controls, routing) {
    const {layout, stack, total, inner} = routing;
    for (const control of controls.values()) {
      if (control.item.kind === "E") continue;
      const owner = control.item.host, id = control.item.id, at = placement(routing, owner);
      shift(control.node, at.dx, at.dy);
      // Only a part's own box grows: a shared box, the unassigned row or the stack.
      if (at.part === "row" || id !== (at.part === "stack" ? layout.stack : owner)) continue;
      const rect = control.node.querySelector(":scope > rect");
      // A shared box spans the whole grown stack, so every gutter's track meets its side.
      if (at.part === "shared") rect.setAttribute("height", String(round(Math.max(num(rect,"height") + total, stack.y + stack.h + total - num(rect,"y")))));
      else if (at.part === "unassigned") rect.setAttribute("width", String(round(num(rect,"width") + inner)));
      else rect.setAttribute("height", String(round(num(rect,"height") + total)));
    }
  }
  function route(root, controls, projection) {
    const layout = projection.layout;
    const stack = frame(controls.get(layout.stack));
    if (!stack) return;
    const edges = new Map([...controls.values()].filter(control => control.item.kind === "E" && control.path).map(control => [control.item, control]));
    const band = id => frame(controls.get(id));
    const tag = item => { const rect = edges.get(item).node.querySelector("rect"); return {w:num(rect,"width"), h:num(rect,"height")}; };
    const routing = planRoutes({layout, stack, edges:[...edges.keys()], band, tag});
    shiftBands(controls, routing);
    const width = num(root,"width") + routing.grow, height = num(root,"height") + routing.total + routing.stripH;
    root.setAttribute("width", String(round(width))); root.setAttribute("height", String(round(height))); root.setAttribute("viewBox", `0 0 ${round(width)} ${round(height)}`);
    routePaths(routing, band, (item, points, at) => draw(edges.get(item), points, at));
  }
  // One routed arrow: a polyline ending at an arrowhead sized to its stroke, and its tag.
  function draw(control, points, tag) {
    const width = edgeWidth(control.item.record.count), [line, marker] = control.path.querySelectorAll("path");
    const [tip, before] = [points[points.length - 1], points[points.length - 2]];
    const length = Math.hypot(tip[0] - before[0], tip[1] - before[1]) || 1, ux = (tip[0] - before[0]) / length, uy = (tip[1] - before[1]) / length;
    const head = 6 + 1.2 * width, half = 3 + 0.9 * width, base = [tip[0] - ux * head, tip[1] - uy * head];
    const drawn = [...points.slice(0, -1), base].map(([x, y]) => `${round(x)},${round(y)}`);
    line.setAttribute("d", `M${drawn.join("L")}`);
    line.setAttribute("stroke-width", String(width)); line.setAttribute("stroke-linecap", "butt"); line.setAttribute("stroke-linejoin", "round");
    marker.setAttribute("d", `M${round(tip[0])},${round(tip[1])}L${round(base[0] - uy * half)},${round(base[1] + ux * half)}L${round(base[0] + uy * half)},${round(base[1] - ux * half)}Z`);
    const rect = control.node.querySelector("rect"), text = control.node.querySelector("text");
    const x = round(tag.cx - num(rect,"width") / 2), y = round(tag.cy - num(rect,"height") / 2), moveX = x - num(rect,"x"), moveY = y - num(rect,"y");
    rect.setAttribute("x", String(x)); rect.setAttribute("y", String(y));
    text.setAttribute("x", String(round(num(text,"x") + moveX))); text.setAttribute("y", String(round(num(text,"y") + moveY)));
  }
  // Where a render may be superseded. A phase yields PAUSE where a host's optional async pause
  // runs first (hosts that cap one synchronous run: the Herdr mod, #707) and CHECK where only
  // currency is asked; `render` answers both at its one stale checkpoint. The phases are
  // generators, not async functions, so they add no await of their own: without a pause nothing
  // here awaits more than the browser did.
  const PAUSE = "pause", CHECK = "check";
  // One step of the width fit: from the last attempt's drawn width, the next chip width to try,
  // moving `s` (columns, chipWidth, stackSpan and the fitting `low` and overflowing `high` chip
  // widths). False ends the search.
  function nextFit(s, fits, drawnWidth, target, sharedCount) {
    const cells = s.columns + sharedCount;
    let next;
    if (fits) {
      s.low = s.chipWidth;
      const slack = target - drawnWidth;
      if (slack < 2 * cells || s.chipWidth >= MAX_CHIP) return false;
      next = s.high === null ? Math.min(MAX_CHIP, s.chipWidth + Math.max(2, Math.floor(slack / cells))) : Math.floor((s.low + s.high) / 2);
    } else {
      s.high = s.chipWidth;
      if (s.low !== null) next = Math.floor((s.low + s.high) / 2);
      else if (s.chipWidth > MIN_CHIP) next = Math.max(MIN_CHIP, s.chipWidth - Math.ceil((drawnWidth - target) / cells) - 1);
      else if (s.columns > 1) { s.columns--; s.high = null; next = CHIP_WIDTH; }
      else if (sharedCount && Math.max(s.columns, s.stackSpan) < MAX_STACK_SPAN) { s.stackSpan = Math.max(s.columns, s.stackSpan) + 1; s.high = null; next = MIN_CHIP; }
      else return false;
    }
    if (s.low !== null && s.high !== null && s.high - s.low <= 1) return false;
    s.chipWidth = next;
    return true;
  }
  // Fit the canvas width (#449, #525): start from the estimated chip columns, then size the
  // chips so Pele's natural layout fills the canvas without passing it. Once a fitting and
  // an overflowing chip width are known, bisect between them. Too wide with no fit shrinks
  // the chips, then drops a column. At one column with the narrowest chips, a stack beside
  // shared boxes then spans more grid columns (up to MAX_STACK_SPAN), which narrows the equal
  // grid cells the shared boxes take; the narrowest layout wins when none fits.
  function* fitLayout(view, options, measure) {
    const sharedCount = view.layers.filter(l => l.shared && options.display.layers.has(l.name)).length;
    const s = {columns:chipColumns(options.width, sharedCount), chipWidth:CHIP_WIDTH, stackSpan:0, low:null, high:null};
    let projection, raw, best = null;
    const fit = options.width > 0;
    // The side channels routing adds (#637) depend only on the arrows and their tags, never on
    // chip sizes: they are taken off the width before chip columns are chosen.
    let target = options.width;
    yield PAUSE;
    if (fit) { target -= channelReserve(project(view,{...options,columns:s.columns,chipWidth:s.chipWidth,measureText:measure})); s.columns = chipColumns(target, sharedCount); }
    for (let attempt = 0; ; attempt++) {
      yield PAUSE;
      projection = project(view,{...options,columns:s.columns,chipWidth:s.chipWidth,stackSpan:s.stackSpan,measureText:measure});
      yield PAUSE;
      raw = window.pele.render(projection.source,{fontSize:14,padding:8,responsive:false,links:false,images:false}).svg;
      if (!fit) break;
      const drawnWidth = Number(/^<svg\b[^>]*?\swidth="([0-9.]+)"/.exec(raw)?.[1]), fits = drawnWidth <= target;
      if (!best || (fits && (!best.fits || drawnWidth > best.drawnWidth)) || (!fits && !best.fits && drawnWidth < best.drawnWidth)) best = {fits,drawnWidth,projection,raw,columns:s.columns,chipWidth:s.chipWidth,stackSpan:s.stackSpan};
      if (attempt >= 12 || !nextFit(s, fits, drawnWidth, target, sharedCount)) break;
    }
    // Projecting charges the caller's chip budget, so the chosen layout is projected again
    // when it was not the last attempt; the same inputs give the same source and ids.
    if (best && best.projection !== projection) {
      yield PAUSE;
      projection = project(view,{...options,columns:best.columns,chipWidth:best.chipWidth,stackSpan:best.stackSpan,measureText:measure});
      raw = best.raw;
    }
    return {projection, raw};
  }
  const PALETTE = {"var(--_bg)":"#0B0C10","var(--_fg)":"#F7F7FB","var(--_m)":"#B8BAC8","var(--_l)":"#777A8E","var(--_s)":"#14151D","var(--_a)":"#1D1E2A","var(--_b)":"#777A8E"};
  function applyPalette(root) {
    root.classList.add("trellis-architecture-svg"); root.setAttribute("font-size","14"); root.setAttribute("font-family","Inter, sans-serif");
    for (const node of [root,...root.querySelectorAll("*")]) for (const attr of [...node.attributes]) {
      if (PALETTE[attr.value]) node.setAttribute(attr.name,PALETTE[attr.value]);
      if (attr.name === "rx" && attr.value === "var(--_r)") node.setAttribute("rx","4");
    }
  }
  // A note's stable identity across re-layouts: its layer (null when unassigned) and its
  // ordinal among that layer's notes in projection order.
  function noteIdentities(projection) {
    const noteIds = new Map(), noteCounts = new Map();
    for (const item of [...projection.nodes.values()].filter(node => node.kind === "N").sort((a,b) => parseInt(a.id.slice(1),36) - parseInt(b.id.slice(1),36))) {
      const layer = typeof item.record?.layerKey === "string" ? item.record.layerKey : null, n = noteCounts.get(layer) || 0;
      noteCounts.set(layer, n + 1); noteIds.set(item.id, JSON.stringify([layer, n]));
    }
    return noteIds;
  }
  // A module or closed directory chip: metric colour, and its own callback.
  function tile({node,label,item}, {options}) {
    const fill = item.band === null ? "#14151D" : colors[item.band];
    node.querySelector("rect")?.setAttribute("fill",fill); label.setAttribute("fill",item.band > 0 ? "#0B0C10" : "#F7F7FB");
    return item.kind === "M" ? {callback:options.onSelectModule, expanded:null} : {callback:options.onToggleDirectory, expanded:false};
  }
  // The decorator table: how each kind of drawn control is styled, and the callback and
  // disclosure state (`expanded`; null for a selectable control) that make it a button. A kind
  // with no row, or a row with no callback, stays plain drawing.
  const DECORATE = {
    __proto__:null,
    // A summary chip: metric colour and name only; its band header is the one toggle.
    C({node,label,item}) {
      node.querySelector("rect")?.setAttribute("fill",item.band === null ? "#14151D" : colors[item.band]);
      label.setAttribute("fill",item.band > 0 ? "#0B0C10" : "#F7F7FB");
      node.setAttribute("data-trellis-summary-chip",typeof item.record.layerKey === "string" ? item.record.layerKey : "");
    },
    M:tile,
    D:(control, context) => control.item.composite ? {callback:context.options.onToggleDirectory, expanded:true} : tile(control, context),
    U:({item}, {options}) => ({callback:options.onToggleUnassigned, expanded:item.expanded}),
    N:(control, {options}) => ({callback:options.onSelectNote, expanded:null}),
    L({node,label,item}, {options, measure, current, actions}) {
      const callback = options.onToggleLayer;
      if (item.statLines?.length) {
        const handlers = {onOpenDeviation:options.onOpenDeviation, onSelectInterface:options.onSelectInterface, isCurrent:current};
        statChips(node,label,item,measure,handlers).reduce((previous,{node:button}) => {
          previous.after(button); actions.push({node:button,label:null,item:{kind:"A",id:`${item.id}.a${actions.length}`,host:item.host}});
          return button;
        },node);
      }
      return {callback, expanded:item.expanded};
    },
    E({node,label,item,path}, {options}) {
      // Deviations are highlighted; allowed (conforming or excepted) arrows are dimmed, not dropped (#449).
      const danger = item.record.role === "production" && item.record.status === "deviates" && !item.record.excepted;
      const tone = item.tone || (danger ? "deviation" : "neutral");
      const color = tone === "deviation" ? "#FF8E9A" : tone === "allowed" ? "#777A8E" : "#B8BAC8";
      for (const p of path.querySelectorAll("path")) {
        p.setAttribute("stroke",color);
        if (p.classList.contains("pele-marker")) p.setAttribute("fill",color);
      }
      const rect = node.querySelector("rect"); rect?.setAttribute("stroke",color); rect?.setAttribute("fill","#14151D"); label.setAttribute("fill",color);
      if (tone === "allowed") { path.setAttribute("opacity","0.45"); node.setAttribute("opacity","0.75"); }
      node.setAttribute("data-trellis-edge-tone",tone);
      return {callback:options.onSelectEdge, expanded:null};
    },
  };
  // Make one decorated control a button, with the identity its kind is found by.
  function bindControl({node,item}, callback, expanded, {controls, current, noteIds}) {
    asButton(node,item.full,event => {
      event?.stopPropagation();
      if (!current()) return;
      if (expanded === null) for (const control of controls.values()) if (control.node.hasAttribute("aria-pressed")) control.node.setAttribute("aria-pressed",String(control.node === node));
      callback(item.record);
    });
    node.setAttribute("data-trellis-architecture-id",item.id);
    // Disclosure nodes report expansion; selectable evidence reports selection.
    if (expanded === null) node.setAttribute("aria-pressed","false"); else node.setAttribute("aria-expanded",String(expanded));
    if (item.kind === "M") node.setAttribute("data-trellis-module-id",identity(item.record));
    if (item.kind === "L") node.setAttribute("data-trellis-layer-id",item.record.name);
    if (item.kind === "D") node.setAttribute("data-trellis-directory-id",item.record.key);
    if (item.kind === "U") node.setAttribute("data-trellis-unassigned","true");
    if (item.kind === "N") node.setAttribute("data-trellis-note-id",noteIds.get(item.id));
    if (item.kind === "E") node.setAttribute("data-trellis-edge-id",edgeKey(item.record));
  }
  // Label, style and bind every control, in drawing order. Returns the stat-chip buttons made
  // on the way; they join `controls` only after the pause that follows.
  function decorate(controls, projection, options, measure, current) {
    const context = {options, measure, current, controls, noteIds:noteIdentities(projection), actions:[]};
    for (const control of controls.values()) {
      replaceLabel(control.label,control.item);
      const made = DECORATE[control.item.kind]?.(control, context);
      if (made?.callback) bindControl(control, made.callback, made.expanded, context);
    }
    return context.actions;
  }
  function bandFrames(controls) {
    const bands = new Map();
    for (const control of controls.values()) if (control.item.kind === "L") bands.set(control.item.record.name,frame(control));
    return bands;
  }
  // One render's phases, in order, up to the finished drawing. Nothing here touches the page.
  function* phases(view, options, current) {
    yield CHECK;
    const measure = textMeasurer("14px Inter, sans-serif");
    if (!measure) unsupported("Architecture text measurement unavailable");
    if (typeof window.pele?.render !== "function") unsupported("Pele asset unavailable");
    if (!options.display) unsupported("Architecture display unavailable");
    const {projection, raw} = yield* fitLayout(view, options, measure);
    yield PAUSE;
    yield CHECK;
    const {root,controls} = validatedSvg(raw,projection);
    yield PAUSE;
    applyPalette(root);
    const actions = decorate(controls, projection, options, measure, current);
    yield PAUSE;
    for (const control of actions) controls.set(control.item.id,control);
    route(root,controls,projection);
    yield CHECK;
    return {root, projection, bands:bandFrames(controls)};
  }
  async function render(container, view, options = {}) {
    const current = () => !options.isCurrent || options.isCurrent();
    if (!current()) return {status:"stale"};
    const pause = typeof options.pause === "function" ? options.pause : null;
    const breathe = async () => { await pause(); return current(); };
    try {
      if (document.fonts?.ready) await document.fonts.ready;
      const steps = phases(view, options, current);
      let step = steps.next();
      // The one stale checkpoint: a superseded render stops here, between two phases, and
      // leaves the page as it found it.
      for (; !step.done; step = steps.next()) if (step.value === PAUSE ? pause && !(await breathe()) : !current()) return {status:"stale"};
      container.replaceChildren(step.value.root);
      return {status:"ready",projection:step.value.projection,bands:step.value.bands};
    } catch (error) {
      if (!current()) return {status:"stale"};
      const message = document.createElement("p"); message.className = "architecture-render-error";
      message.textContent = `Architecture diagram unavailable: ${error.message}`; container.replaceChildren(message);
      return {status:"error",error};
    }
  }
  window.TrellisArchitecturePele = {project,validatedSvg,render,identity,edgeKey,edgeWidth};
})();
