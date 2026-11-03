"use strict";
// Pele supplies layout only. Native DTO records remain the evidence owner.
(() => {
  const NS = "http://www.w3.org/2000/svg";
  const ROOT_STYLE = "--_bg:var(--pele-bg,#fff);--_fg:var(--pele-fg,#1f1f1f);--_m:var(--pele-muted,#6e6e6e);--_l:var(--pele-line,#1f1f1f);--_s:var(--pele-surface,#f3f3f3);--_a:var(--pele-surface-alt,#e6e6e6);--_b:var(--pele-border,#8a8a8a);--_c:var(--pele-accent,#5b7bd5);color:var(--_fg);--_r:var(--pele-radius,4px);--_fm:var(--pele-font-mono,monospace);font:14px sans-serif;letter-spacing:normal;word-spacing:normal;text-transform:none;direction:ltr;font-family:var(--pele-font,sans-serif)";
  const tags = new Set(["svg","g","path","rect","text","tspan"]);
  const attrs = new Set(["xmlns","class","viewBox","width","height","fill","fill-opacity","role","aria-roledescription","transform","stroke","font-size","data-id","d","x","y","rx","text-anchor","font-weight","stroke-dasharray","stroke-linecap","stroke-width","xml:space"]);
  const colors = ["#343644","#FFC20A","#FFAA10","#FF7A00"];
  const identity = module => JSON.stringify([module.identity?.sourceSetId,module.identity?.language,module.identity?.path,module.identity?.sid]);
  const edgeKey = edge => JSON.stringify([edge.sourceLayer,edge.destinationLayer,edge.role,edge.status,edge.excepted,edge.exceptionReason]);
  const bytes = value => new TextEncoder().encode(value).length;
  // Display chips: compact module/file chips, more per band row, and a narrow shared box.
  // CHIP_CELL is one drawn chip column (chip, padding and grid gap) at 14px Inter; the
  // summary display fits as many columns as the canvas width holds (#449). The chip text
  // width then grows or shrinks so the drawing fills the panel width (#525).
  const CHIP_WIDTH = 104, MIN_CHIP = 72, MAX_CHIP = 240, CHIP_CELL = 176, CHIP_COLUMNS = 5, MAX_COLUMNS = 14, SHARED_ROWS = 6, STAT_GAP = "   ";
  // Pele's drawn node padding around a label and the grid gap between siblings.
  const NODE_PAD = 32, GRID_GAP = 12;
  // A stat chip is its text plus 12 units of padding, 6 apart (see statChips).
  const STAT_PAD = 12, STAT_SPACING = 6;
  // Lines one wrapped label line may take before its last is clipped: a bound for
  // pathological repository names only; ordinary paths and layer names wrap whole.
  const MAX_LINES = 12;
  // Arrow routing (#637): a gutter under each band row holds count tags (rows of TAG_H plus
  // TAG_GAP), then one TRACK per horizontal run; DROP is the least vertical run that ends at a
  // band, so an arrowhead always fits. Long arrows run in side-channel lanes LANE apart.
  const GUTTER_PAD = 4, TAG_GAP = 4, TRACK = 10, LANE = 10, DROP = 18, MIN_GUTTER = 24, CHANNEL_PAD = 8, INSET = 10, SLOT_GAP = 6;
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
  // Stat chips grouped into header lines no wider than `width` once drawn.
  function statLines(stats, width, measure) {
    const lines = [];
    let line = [], used = 0;
    for (const stat of stats) {
      const drawn = Math.ceil(measure(stat.text)) + STAT_PAD;
      if (line.length && used + STAT_SPACING + drawn > width) { lines.push(line); line = []; used = 0; }
      used += (line.length ? STAT_SPACING : 0) + drawn; line.push(stat);
    }
    if (line.length) lines.push(line);
    return lines;
  }
  const ARROWS = {downward:"↓", upward:"↑", peer:"↔", "into shared":"→", "shared outbound":"←"};
  function direction(source, destination) {
    if (destination.shared) return "into shared";
    if (source.shared) return "shared outbound";
    return destination.order > source.order ? "downward" : destination.order < source.order ? "upward" : "peer";
  }
  function unsupported(message) { const e = new Error(message); e.code = "unsupported"; throw e; }
  function project(view, options = {}) {
    if (view?.state !== "ready" || !Array.isArray(view.layers) || !Array.isArray(view.modules) || !Array.isArray(view.edges)) unsupported("Invalid Architecture projection");
    const nodes = new Map(), edges = new Map(), layers = new Map(), native = new Set();
    const measure = options.measureText || (text => Array.from(text).length * 14);
    const chip = Number.isFinite(options.chipWidth) ? Math.max(MIN_CHIP, Math.min(MAX_CHIP, Math.floor(options.chipWidth))) : CHIP_WIDTH;
    let serial = 0;
    const clean = text => String(text).replace(/[\u0000-\u0009\u000b-\u001f\u007f]/g, " ");
    // `shown` is the drawn text when it is shorter than the accessible `text`.
    function label(kind, record, text, pad = false, shown = text, width = null) {
      const id = kind + (serial++).toString(36), value = clean(text);
      // Uniform safe placeholders reserve width before repository text is inserted.
      // A line wraps at separators to the reserved width (#525); the accessible name keeps the full label.
      const lineWidth = width ?? (kind === "L" || kind === "U" ? 380 : kind === "M" ? 150 : kind === "E" ? 150 : kind === "N" ? 260 : 200);
      // Edge counts are one line, sized to their text, so they never wrap.
      const lines = clean(shown).split("\n");
      const full = pad && kind === "E" ? [lines.join(" ")] : lines;
      const display = (pad && kind === "E" ? full : full.flatMap(line => wrap(line, lineWidth, measure, MAX_LINES))).map(line => line || " ");
      const reserve = "W".repeat(Math.ceil(lineWidth / measure("W")));
      const firstReserve = "W".repeat(Math.max(1,Math.ceil((lineWidth - measure(id + "_")) / measure("W"))));
      const placeholders = display.map((_, i) => i === 0 ? id + "_" + firstReserve : reserve);
      return {id,kind,record,display,lineWidth,full:value,placeholder:placeholders.join("<br/>"),expected:placeholders.join("")};
    }
    // `host` is the band (layer or unassigned row) a node is drawn in; routing moves it with that band.
    let host = null;
    function addNode(kind, record, text, pad, shown, width) { const item = label(kind,record,text,pad,shown,width); item.host = host; nodes.set(item.id,item); return item; }
    const source = ["block-beta","columns 1"];
    function open(item, columns, span = 1) { item.composite = true; source.push(`block:${item.id}["${item.placeholder}"]:${span}`, `columns ${columns}`); }
    const outer = options.display ? null : addNode("B",null,"Architecture · structural references");
    const shownShared = view.layers.filter(l => l.shared && (!options.display || options.display.layers.has(l.name))).length;
    const COLUMNS = options.display ? options.columns ?? chipColumns(options.width, shownShared) : CHIP_COLUMNS;
    options.display?.reset?.();
    // The stack title reserves no more than one chip, so it never sets the stack's width.
    const orderedStack = addNode("B",null,"Ordered layers",false,"Ordered layers",options.display ? chip : null);
    // Optional summary display: the caller chooses drawn layers, expansion and
    // directory entries. Without it every layer is expanded to file chips.
    const display = options.display || null;
    const names = new Set();
    for (const layer of view.layers) {
      if (typeof layer.name !== "string" || names.has(layer.name) || typeof layer.shared !== "boolean" || (!layer.shared && !Number.isSafeInteger(layer.order))) unsupported("Invalid or duplicate Architecture layer");
      names.add(layer.name);
    }
    const drawn = view.layers.filter(l => !display || display.layers.has(l.name));
    const ordered = drawn.filter(l => !l.shared).sort((a,b) => a.order-b.order || a.name.localeCompare(b.name));
    const shared = drawn.filter(l => l.shared).sort((a,b) => a.name.localeCompare(b.name));
    // Peers share one band row; a band header is clipped to its band's chip columns.
    const peerCount = order => ordered.filter(l => l.order === order).length;
    const width = display ? Math.max(1, ...ordered.map(l => peerCount(l.order))) : 1;
    const bandColumns = layer => Math.max(1, Math.floor(COLUMNS * Math.floor(width / peerCount(layer.order)) / width));
    // A header wraps within its band's chip row, so it never widens or clips the band.
    const rowWidth = columns => columns * (chip + NODE_PAD) + (columns - 1) * GRID_GAP - NODE_PAD;
    const headerWidth = layer => !display ? null : layer.shared ? chip : Math.max(chip, rowWidth(bandColumns(layer)));
    for (const layer of drawn) {
      const header = options.layerLabel?.(layer) ?? `${layer.name}\n${layer.shared ? "Shared" : `Order ${layer.order}`}`;
      // Non-zero reference counts become compact chips on their own header lines.
      const stats = display ? (options.layerStats?.(layer) || []).filter(stat => typeof stat?.text === "string" && stat.text) : [];
      const widest = stats.reduce((most, stat) => Math.max(most, Math.ceil(measure(stat.text)) + STAT_PAD), 0);
      const lineWidth = Math.max(headerWidth(layer) ?? 380, widest), groups = statLines(stats, lineWidth, measure);
      const item = addNode("L",layer,stats.length ? `${header}\n${stats.map(stat => stat.label || stat.text).join(" · ")}` : header,
        false,[header, ...groups.map(group => group.map(stat => stat.text).join(STAT_GAP))].join("\n"),lineWidth);
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
      const item = typeof name === "string" ? addNode("M",module,full,false,`${name}${value === null ? "\n—" : value === undefined ? "" : `\n${value}`}`,chip) : addNode("M",module,full);
      item.band = heat(options.moduleBand?.(module)); leaf(item);
    }
    function emitEntries(entries, span) {
      if (!entries.length) leaf(addNode("B",null,"No files shown",false,"No files shown",chip));
      for (const entry of entries) {
        if (entry.type === "module") moduleNode(entry.module, entry.name);
        else if (entry.type === "directory") {
          const item = addNode("D",entry,entry.label,false,entry.open ? entry.label : entry.shown ?? entry.label,entry.open ? rowWidth(span) : chip);
          item.band = entry.open ? null : heat(entry.band);
          if (entry.open) { open(item,span,span); emitEntries(entry.entries,span); source.push("end"); } else leaf(item);
        } else if (entry.type === "note") leaf(addNode("N",entry,entry.label,false,entry.shown ?? entry.label,chip));
        else if (entry.type === "chip") { const item = addNode("C",entry,entry.label,false,entry.shown ?? entry.label,chip); item.band = heat(entry.band); leaf(item); }
        else unsupported("Invalid Architecture display entry");
      }
    }
    // Pele gives a leaf row the height of its tallest sibling, so a folded band is a composite
    // whose header is the one toggle, over a short non-interactive file count.
    function folded(item, record, text, span) {
      open(item,1,span);
      const summary = addNode("S",record,text,false,text,chip); summary.owner = item; leaf(summary);
      source.push("end");
    }
    function emitLayer(layer, span = 1) { host = layers.get(layer.name).id; emitBand(layer, span); host = null; }
    function emitBand(layer, span) {
      // Peers split the chip columns of one band row, so the stack keeps one width.
      const item = layers.get(layer.name), columns = layer.shared ? 1 : display ? bandColumns(layer) : 4;
      const members = moduleRows.get(layer.name).sort((a,b) => identity(a).localeCompare(identity(b)));
      if (display) {
        // A collapsed band shows one row of module chips coloured by metric (#449), not a file count.
        if (!item.expanded) {
          const chips = display.summary ? display.summary(layer.name,members,layer.shared ? SHARED_ROWS : columns) : [];
          if (!chips.length) { folded(item,layer,`${members.length} files folded`,span); return; }
          open(item,columns,span); emitEntries(chips,columns); source.push("end"); return;
        }
        open(item,columns,span); emitEntries(opened.get(layer.name),columns); source.push("end"); return;
      }
      open(item,columns);
      if (!members.length) leaf(addNode("B",null,"No indexed modules"));
      for (const m of members) moduleNode(m);
      source.push("end");
    }
    // Open bands and an open unassigned row charge the chip budget first, in drawing order,
    // so collapsed summary rows only use what remains and never fold a band the viewer opened.
    const opened = new Map(), unassigned = moduleRows.get(null);
    if (display) {
      for (const layer of [...ordered, ...shared]) {
        const item = layers.get(layer.name); item.expanded = !!display.expanded(layer.name);
        if (item.expanded) opened.set(layer.name, display.entries(layer.name,moduleRows.get(layer.name).sort((a,b) => identity(a).localeCompare(identity(b)))));
      }
      if (display.unassigned && unassigned.length && display.unassigned.expanded) opened.set(null, display.entries(null,unassigned));
    }
    // Pele stretches a composite's children to equal grid columns, so the stack spans
    // several columns and each shared box one, keeping the shared boxes narrow.
    const stackSpan = display ? COLUMNS : 4, orders = [...new Set(ordered.map(l => l.order))];
    if (display) source[1] = `columns ${stackSpan + shared.length}`; else open(outer,stackSpan + shared.length);
    open(orderedStack,width,stackSpan);
    if (!ordered.length) leaf(addNode("B",null,display ? "No ordered layers shown" : "No ordered layers"));
    for (const order of orders) {
      const peers = ordered.filter(l => l.order === order);
      if (display) {
        const span = Math.floor(width / peers.length);
        peers.forEach(layer => emitLayer(layer,span));
        if (width > span * peers.length) source.push(`space:${width - span * peers.length}`);
        continue;
      }
      if (peers.length > 1) open(addNode("B",null,`Peer layers · order ${order}`),peers.length);
      peers.forEach(layer => emitLayer(layer)); if (peers.length > 1) source.push("end");
    }
    source.push("end"); shared.forEach(layer => emitLayer(layer)); if (!display) source.push("end");
    let unassignedId = null;
    if (display?.unassigned && unassigned.length) {
      const item = addNode("U",display.unassigned,display.unassigned.label,false,display.unassigned.label,rowWidth(COLUMNS + shared.length)); item.expanded = opened.has(null);
      item.host = unassignedId = host = item.id;
      if (item.expanded) { open(item,COLUMNS + shared.length,stackSpan + shared.length); emitEntries(opened.get(null),COLUMNS + shared.length); source.push("end"); }
      else {
        const chips = display.summary ? display.summary(null,unassigned,COLUMNS + shared.length) : [];
        if (chips.length) { open(item,COLUMNS + shared.length,stackSpan + shared.length); emitEntries(chips,COLUMNS + shared.length); source.push("end"); }
        else folded(item,display.unassigned,`${unassigned.length} files folded`,stackSpan + shared.length);
      }
      host = null;
    } else if (!display && unassigned.length) {
      open(addNode("B",null,"Unassigned / unknown"),4);
      for (const m of unassigned) moduleNode(m);
      source.push("end");
    }
    const byName = new Map(view.layers.map(layer => [layer.name,layer])), seenEdges = new Set();
    for (const edge of view.edges) {
      if (!names.has(edge.sourceLayer) || !names.has(edge.destinationLayer) || edge.sourceLayer === edge.destinationLayer || !["production","test"].includes(edge.role) || !["conforms","deviates","unknown"].includes(edge.status) || !Number.isSafeInteger(edge.count) || edge.count <= 0 || typeof edge.excepted !== "boolean" || seenEdges.has(edgeKey(edge))) unsupported("Invalid or duplicate Architecture edge");
      seenEdges.add(edgeKey(edge));
      if (edge.role === "test" && !options.showTests) continue;
      if (!layers.has(edge.sourceLayer) || !layers.has(edge.destinationLayer) || (display && !display.edge(edge))) continue;
      const from = layers.get(edge.sourceLayer).id, to = layers.get(edge.destinationLayer).id;
      const state = edge.status === "deviates" ? "deviation" : edge.status, way = direction(byName.get(edge.sourceLayer),byName.get(edge.destinationLayer));
      // Summary arrows draw a compact count; the accessible label keeps the words. The caller
      // may qualify both: a lower bound and the source layer's mapped-site coverage (#449).
      const words = options.edgeText?.(edge, way) || {};
      const full = `${edge.count} ${edge.role} sites · ${way}\n${state}${edge.excepted ? " · exception" : ""}${words.label ? ` · ${words.label}` : ""}`;
      const shown = `${ARROWS[way]}${words.bound ? "≥" : ""}${edge.count}${edge.excepted ? " excepted" : ""}${edge.role === "test" ? " test" : ""}${words.note ? ` · ${words.note}` : ""}`;
      const item = display ? label("E",edge,full,true,shown,Math.ceil(measure(`${shown}…`)) + 2) : label("E",edge,full);
      Object.assign(item,{from,to,tone:options.edgeTone?.(edge) || null}); edges.set(item.id,item);
      // The summary display routes its own arrows (#637): Pele lays out the boxes only, so its
      // edge-label placement never widens the drawing. A tag is its 12px text plus padding.
      if (display) { item.tagWidth = Math.ceil(measure(item.display[0]) * 12 / 14) + 12; continue; }
      source.push(`${from} ${edge.role === "test" ? "-." : "--"} "${item.placeholder}" ${edge.role === "test" ? ".->" : "-->"} ${to}`);
    }
    const text = source.join("\n") + "\n";
    if (bytes(text) > 512 * 1024) unsupported("Architecture Mermaid source too large");
    // The summary display's band rows (peers share a row), shared boxes in drawing order,
    // the ordered stack and the unassigned row: what arrow routing needs (#637).
    const layout = display ? {rows:orders.map(order => ordered.filter(l => l.order === order).map(l => layers.get(l.name).id)),
      shared:shared.map(l => layers.get(l.name).id), stack:orderedStack.id, unassigned:unassignedId} : null;
    return {source:text,nodes,edges,view,layout};
  }
  function validatedSvg(raw, projection) {
    if (typeof raw !== "string" || bytes(raw) > 8 * 1024 * 1024) unsupported("Architecture SVG too large");
    const signature = ` style="${ROOT_STYLE}"`;
    if (!raw.startsWith('<svg xmlns="http://www.w3.org/2000/svg" class="pele pele-block"') || raw.split(signature).length !== 2) unsupported("Changed Pele block root/theme");
    const safe = raw.replace(signature,"").replaceAll(' style="rx:var(--_r)"',' rx="var(--_r)"');
    if (/\sstyle\s*=|<style\b/i.test(safe)) unsupported("Unexpected inline Architecture style");
    // Reject active/resource markup before DOM parsing: even detached parsing
    // can report CSP violations for active elements in some browsers.
    if (/<\/?(?:script|style|foreignObject|image|a)\b/i.test(safe)) unsupported("Unexpected Architecture SVG element");
    const root = new DOMParser().parseFromString(safe,"image/svg+xml").documentElement;
    if (root.localName !== "svg" || root.namespaceURI !== NS || root.querySelector("parsererror")) unsupported("Invalid Architecture SVG");
    let count = 0;
    for (const node of [root,...root.querySelectorAll("*")]) {
      if (++count > 12000 || node.namespaceURI !== NS || !tags.has(node.localName)) unsupported("Unexpected Architecture SVG element");
      for (const attr of node.attributes) if (!attrs.has(attr.name) || /url\s*\(|javascript:|<|>|[\u0000-\u001f]/i.test(attr.value)) unsupported("Unexpected Architecture SVG attribute");
    }
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
    const paths = new Map();
    for (const path of root.querySelectorAll(".pele-edge")) {
      const id = path.getAttribute("data-id");
      if (path.getAttribute("class") !== "pele-edge" || path.children.length !== 2 || [...path.children].some(child => child.localName !== "path")) unsupported("Changed Architecture connector structure");
      if (!id || paths.has(id)) unsupported("Duplicate Architecture connector"); paths.set(id,path);
    }
    const seenEdges = new Set(), pairedPaths = new Set();
    for (const node of root.querySelectorAll(".pele-edge-label")) {
      if (node.getAttribute("class") !== "pele-edge-label" || node.children.length !== 2 || node.children[0].localName !== "rect" || node.children[1].localName !== "text") unsupported("Changed Architecture count box structure");
      const label = node.querySelector("text"), expected = [...projection.edges.values()].find(e => e.expected === label?.textContent);
      const nativeId = node.getAttribute("data-id"), path = paths.get(nativeId);
      // The occurrence prefix is not evidence identity. Exact app-owned label
      // tokens identify records; this association verifies declared endpoints.
      if (!expected || seenEdges.has(expected.id) || !path || pairedPaths.has(nativeId) || !new RegExp(`^[1-9][0-9]*-${expected.from}-${expected.to}$`).test(nativeId)) unsupported("Changed Architecture edge identity or endpoints");
      const segments = [...path.querySelectorAll("path")];
      if (segments.length !== 2 || segments[0].hasAttribute("class") || segments[1].getAttribute("class") !== "pele-marker") unsupported("Changed Architecture connector shape");
      const dashed = segments[0].getAttribute("stroke-dasharray");
      if ((expected.record.role === "test" ? dashed !== "3 4" : dashed !== null)) unsupported("Changed Architecture edge role stroke");
      seenEdges.add(expected.id); pairedPaths.add(nativeId); controls.set(expected.id,{node,label,item:expected,path});
    }
    // A summary display's arrows are drawn by the adapter, so Pele must draw none of them.
    const drawnEdges = projection.layout ? 0 : projection.edges.size;
    if (seenEdges.size !== drawnEdges || paths.size !== drawnEdges || root.querySelectorAll("[data-id]").length !== projection.nodes.size + 2 * drawnEdges) unsupported("Missing or unexpected Architecture edge");
    if (root.querySelectorAll("text").length !== projection.nodes.size + drawnEdges || root.querySelectorAll("rect").length !== projection.nodes.size + drawnEdges || root.querySelectorAll("path").length !== 2 * drawnEdges) unsupported("Unexpected Architecture SVG decoration");
    if (projection.layout) arrowShapes(root, controls, projection);
    return {root,controls};
  }
  // The adapter's own connector and count tag for each summary arrow (#637), in Pele's shape:
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
  function statChips(node, label, item, measure) {
    const spans = [...label.querySelectorAll("tspan")];
    for (const {stats, line:index} of item.statLines) {
      const line = spans[index];
      const x = Number(line?.getAttribute("x")), y = Number(line?.getAttribute("y"));
      if (!line || !Number.isFinite(x) || !Number.isFinite(y)) unsupported("Invalid Architecture header geometry");
      const widths = stats.map(stat => Math.ceil(measure(stat.text)) + STAT_PAD), gap = STAT_SPACING;
      const total = widths.reduce((sum, width) => sum + width, 0) + gap * (widths.length - 1);
      const anchor = line.getAttribute("text-anchor") || label.getAttribute("text-anchor");
      let left = anchor === "middle" ? x - total / 2 : anchor === "end" ? x - total : x;
      line.textContent = "";
      stats.forEach((stat, i) => {
        const color = stat.tone === "error" ? "#FF8E9A" : "#B8BAC8", chip = document.createElementNS(NS,"rect"), text = document.createElementNS(NS,"text");
        for (const [name,value] of Object.entries({x:left,y:y-14,width:widths[i],height:18,fill:"#14151D",stroke:color,rx:3})) chip.setAttribute(name,String(value));
        for (const [name,value] of Object.entries({x:left + widths[i] / 2,y,fill:color,"text-anchor":"middle","font-size":"14"})) text.setAttribute(name,String(value));
        text.setAttribute("class","trellis-architecture-stat"); text.setAttribute("data-tone",stat.tone === "error" ? "error" : "neutral");
        text.textContent = stat.text; node.append(chip,text); left += widths[i] + gap;
      });
    }
  }
  // Arrow routing (#637). Pele lays out the bands; the adapter then opens a gutter under each
  // band row and routes every arrow orthogonally through gutters, a left side channel (long
  // upward arrows), a right channel (long downward arrows, and runs to a farther shared box)
  // and, when needed, a strip under the drawing. No arrow segment enters a band or a chip:
  // arrows leave and reach bands only at their borders. Each count tag sits in a gutter's tag
  // rows, on its own arrow's vertical run. Layout only: no record, count or identity changes.
  const num = (node, name) => Number(node.getAttribute(name));
  function frame(control) {
    const rect = control?.node.querySelector(":scope > rect");
    return rect ? {x:num(rect,"x"), y:num(rect,"y"), w:num(rect,"width"), h:num(rect,"height")} : null;
  }
  const round = value => Math.round(value * 100) / 100;
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
  // Place vertical runs (and the tags on them) across one gutter edge, left to right.
  // Each run is a LANE-wide line through every tag row; a tagged run also holds its tag in one
  // row. Runs scan outward from their preferred x within their band's span.
  function slots(items) {
    const boxes = [], lines = [];
    const clear = (list, l, r) => !list.some(([a, b]) => l < b && a < r);
    const candidates = item => {
      const half = item.tag ? item.tag.w / 2 : LANE / 2, lo = item.lo + half, hi = item.hi - half;
      if (hi < lo) return [(item.lo + item.hi) / 2];
      const pref = Math.min(hi, Math.max(lo, item.pref)), list = [pref];
      for (let d = 2; pref - d >= lo || pref + d <= hi; d += 2) { if (pref - d >= lo) list.push(pref - d); if (pref + d <= hi) list.push(pref + d); }
      return list;
    };
    let rows = 0;
    for (const item of items) {
      const line = x => [x - LANE / 2 - SLOT_GAP / 2, x + LANE / 2 + SLOT_GAP / 2];
      const lineFree = (x, skip) => clear(lines, ...line(x)) && boxes.every(([row, l, r]) => row === skip || !(line(x)[0] < r && l < line(x)[1]));
      let placed = false;
      if (item.tag) {
        for (let row = 0; row < 8 && !placed; row++) for (const x of candidates(item)) {
          const l = x - item.tag.w / 2 - SLOT_GAP / 2, r = x + item.tag.w / 2 + SLOT_GAP / 2;
          if (lineFree(x, row) && clear(boxes.filter(box => box[0] === row).map(box => [box[1], box[2]]), l, r) && clear(lines, l, r)) {
            Object.assign(item, {x, row}); boxes.push([row, l, r]); rows = Math.max(rows, row + 1); placed = true; break;
          }
        }
        if (!placed) { item.x = candidates(item)[0]; item.row = rows; boxes.push([rows, item.x - item.tag.w / 2, item.x + item.tag.w / 2]); rows++; }
      } else {
        for (const x of candidates(item)) if (lineFree(x, -1)) { item.x = x; placed = true; break; }
        if (!placed) item.x = candidates(item)[0];
      }
      lines.push(line(item.x));
    }
    return rows;
  }
  // A side channel's width: its lanes and their padding. Tags stay in the gutters.
  const channelWidth = lanes => lanes ? lanes * LANE + 2 * CHANNEL_PAD : 0;
  // The width the side channels will add to Pele's drawing, so the fit leaves room for them.
  // It counts lanes exactly as routing does, so routing never draws wider.
  function channelReserve(projection) {
    const layout = projection.layout;
    if (!layout) return 0;
    const rowOf = new Map(), sharedOf = new Map();
    layout.rows.forEach((ids, row) => ids.forEach(id => rowOf.set(id, row)));
    layout.shared.forEach((id, index) => sharedOf.set(id, index));
    let lanesLeft = 0, lanesRight = 0;
    for (const edge of projection.edges.values()) {
      const rs = rowOf.get(edge.from), rd = rowOf.get(edge.to);
      if (rs !== undefined && rd !== undefined && Math.abs(rs - rd) > 1) { if (rd < rs) lanesLeft++; else lanesRight++; }
      else if ((rs === undefined) !== (rd === undefined) && sharedOf.get(rs === undefined ? edge.from : edge.to) > 0) lanesRight++;
    }
    return channelWidth(lanesLeft) + Math.max(0, channelWidth(lanesRight) - (layout.shared.length ? GRID_GAP : 0));
  }
  function route(root, controls, projection) {
    const layout = projection.layout;
    const stack = frame(controls.get(layout.stack));
    if (!stack) return;
    const edges = [...controls.values()].filter(control => control.item.kind === "E" && control.path);
    const rowOf = new Map(), sharedOf = new Map();
    layout.rows.forEach((ids, row) => ids.forEach(id => rowOf.set(id, row)));
    layout.shared.forEach((id, index) => sharedOf.set(id, index));
    const band = id => frame(controls.get(id));
    const rows = layout.rows.map(ids => { const frames = ids.map(band); return {top:Math.min(...frames.map(f => f.y)), bottom:Math.max(...frames.map(f => f.y + f.h))}; });
    // Gutter r lies under band row r; "strip" lies under the stack and shared boxes.
    const gutters = new Map(), gutter = key => { if (!gutters.has(key)) gutters.set(key, {tops:[], bottoms:[], tracks:[]}); return gutters.get(key); };
    const left = [], right = [], plans = [];
    const span = (id, inset = INSET) => { const f = band(id); return {lo:f.x + inset, hi:f.x + f.w - inset, mid:f.x + f.w / 2}; };
    for (const control of edges) {
      const {item} = control, tagRect = control.node.querySelector("rect");
      const tag = {w:num(tagRect,"width"), h:num(tagRect,"height")}, plan = {control, tag, width:edgeWidth(item.record.count)};
      const from = item.from, to = item.to, rs = rowOf.get(from), rd = rowOf.get(to), ks = sharedOf.get(from), kd = sharedOf.get(to);
      const top = (key, id, extra) => { const s = span(id), entry = {plan, id, lo:s.lo, hi:s.hi, pref:s.mid, ...extra}; gutter(key).tops.push(entry); return entry; };
      const bottom = (key, id, extra) => { const s = span(id), entry = {plan, id, lo:s.lo, hi:s.hi, pref:s.mid, ...extra}; gutter(key).bottoms.push(entry); return entry; };
      const track = key => { const list = gutter(key).tracks; list.push(plan); return {key, index:list.length - 1}; };
      plans.push(plan);
      if (rs !== undefined && rd !== undefined) {
        if (Math.abs(rs - rd) === 1) {
          // Adjacent rows share one gutter: straight down or up when the bands overlap, else one jog.
          const upper = rs < rd ? from : to, lower = rs < rd ? to : from, key = Math.min(rs, rd), u = span(upper), l = span(lower);
          const lo = Math.max(u.lo, l.lo), hi = Math.min(u.hi, l.hi);
          if (hi - lo >= tag.w) { plan.kind = "straight"; plan.lower = lower; plan.top = top(key, upper, {lo, hi, pref:(lo + hi) / 2, tag, straight:true}); gutter(key).bottoms.push(plan.top); }
          else { plan.kind = "jog"; plan.top = top(key, upper, {tag, pref:l.mid}); plan.bottom = bottom(key, lower, {pref:u.mid}); plan.track = track(key); }
          plan.down = rs < rd; plan.key = key;
        } else if (rs === rd) {
          // Peers: down into the gutter under their row, across, and back up.
          const s = span(from), d = span(to);
          plan.kind = "peer"; plan.key = rs;
          plan.top = top(rs, from, {tag, pref:s.mid < d.mid ? s.hi : s.lo}); plan.end = top(rs, to, {pref:s.mid < d.mid ? d.lo : d.hi}); plan.track = track(rs);
        } else if (rs < rd) {
          // Long downward: under the source row (its tag there), through the right channel, into the target from above.
          plan.kind = "right"; plan.channel = right; right.push(plan); plan.key = rs;
          plan.top = top(rs, from, {tag, pref:span(from).hi}); plan.first = track(rs);
          plan.bottom = bottom(rd - 1, to, {pref:span(to).hi}); plan.last = track(rd - 1);
        } else {
          // Long upward: above the source row, through the left side channel, into the target from
          // below; its tag sits in the gutter under the target, on the arrow's last run.
          plan.kind = "left"; plan.channel = left; left.push(plan); plan.key = rd;
          plan.bottom = bottom(rs - 1, from, {pref:span(from).lo}); plan.first = track(rs - 1);
          plan.top = top(rd, to, {tag, pref:span(to).lo}); plan.last = track(rd);
        }
      } else if (rs !== undefined || rd !== undefined) {
        // An ordered band and a shared box: through the gutter under the band's row, right to the
        // nearest shared box's side, or by the right channel and the strip to a farther one.
        const ordered = rs !== undefined ? from : to, box = rs !== undefined ? to : from, row = rowOf.get(ordered), k = sharedOf.get(box);
        plan.kind = k === 0 ? "shared" : "far"; plan.toShared = rs !== undefined; plan.box = box; plan.key = row;
        plan.top = top(row, ordered, {tag, pref:span(ordered).hi}); plan.first = track(row);
        if (k > 0) { right.push(plan); plan.channel = right; plan.end = top("strip", box, {}); plan.last = track("strip"); }
      } else if (ks !== undefined && kd !== undefined) {
        // Two shared boxes: down into the strip under them, across, and back up.
        plan.kind = "boxes"; plan.key = "strip";
        plan.top = top("strip", from, {tag}); plan.end = top("strip", to, {}); plan.track = track("strip");
      }
    }
    // Horizontal placement, then each gutter's height: tag rows, tracks and arrowhead room.
    for (const [key, g] of gutters) {
      const order = list => [...list].sort((a, b) => (b.tag ? 1 : 0) - (a.tag ? 1 : 0) || a.plan.control.item.id.localeCompare(b.plan.control.item.id));
      g.tagRows = slots(order(g.tops));
      const bottoms = g.bottoms.filter(entry => !entry.straight);
      // Bottom runs only meet the lower edge, under the tag rows: they avoid other runs, not tags.
      const lines = g.bottoms.filter(entry => entry.straight).map(entry => entry.x);
      for (const entry of order(bottoms)) {
        const options = [entry.pref]; for (let d = 2; entry.pref - d >= entry.lo || entry.pref + d <= entry.hi; d += 2) options.push(entry.pref - d, entry.pref + d);
        entry.x = options.find(x => x >= entry.lo && x <= entry.hi && lines.every(other => Math.abs(other - x) >= LANE + SLOT_GAP)) ?? Math.min(entry.hi, Math.max(entry.lo, entry.pref));
        lines.push(entry.x);
      }
      const tagH = Math.max(0, ...g.tops.filter(entry => entry.tag).map(entry => entry.tag.h));
      g.zone = g.tops.length ? Math.max(DROP, g.tagRows * (tagH + TAG_GAP)) : 0;
      g.tagH = tagH;
      g.height = GUTTER_PAD + g.zone + g.tracks.length * TRACK + (g.bottoms.length || g.tracks.length ? DROP : GUTTER_PAD);
      if (typeof key === "number" && key < rows.length - 1) g.height = Math.max(MIN_GUTTER, g.height);
    }
    // Space between rows that Pele already leaves, and what each gutter adds to it.
    const gap = r => (r + 1 < rows.length ? rows[r + 1].top : stack.y + stack.h) - rows[r].bottom;
    const extra = rows.map((_, r) => Math.max(0, (gutters.get(r)?.height ?? (r < rows.length - 1 ? MIN_GUTTER : 0)) - gap(r)));
    const below = r => extra.slice(0, r).reduce((sum, value) => sum + value, 0), total = below(rows.length);
    const LW = channelWidth(left.length), CW = channelWidth(right.length), sharedFrames = layout.shared.map(band);
    const firstShared = sharedFrames.length ? Math.min(...sharedFrames.map(f => f.x)) : null;
    const RW = Math.max(0, CW - (firstShared === null ? 0 : firstShared - (stack.x + stack.w)));
    const strip = gutters.get("strip");
    const rootBottom = Math.max(stack.y + stack.h, ...sharedFrames.map(f => f.y + f.h));
    const stripH = strip ? strip.height : 0;
    // Move every band, chip and header with its row; widen the stack and shared boxes to match.
    const rowOfHost = new Map(); layout.rows.forEach((ids, row) => ids.forEach(id => rowOfHost.set(id, row)));
    for (const control of controls.values()) {
      if (control.item.kind === "E") continue;
      const owner = control.item.host, id = control.item.id;
      if (rowOfHost.has(owner)) shift(control.node, LW, below(rowOfHost.get(owner)));
      else if (sharedOf.has(owner)) {
        shift(control.node, LW + RW, 0);
        if (id === owner) { const rect = control.node.querySelector(":scope > rect"); rect.setAttribute("height", String(round(num(rect,"height") + total))); }
      } else if (owner && owner === layout.unassigned) {
        shift(control.node, LW, total + stripH);
        if (id === owner) { const rect = control.node.querySelector(":scope > rect"); rect.setAttribute("width", String(round(num(rect,"width") + RW))); }
      } else {
        shift(control.node, LW, 0);
        if (id === layout.stack) { const rect = control.node.querySelector(":scope > rect"); rect.setAttribute("height", String(round(num(rect,"height") + total))); }
      }
    }
    const width = num(root,"width") + LW + RW, height = num(root,"height") + total + stripH;
    root.setAttribute("width", String(round(width))); root.setAttribute("height", String(round(height))); root.setAttribute("viewBox", `0 0 ${round(width)} ${round(height)}`);
    // Final coordinates: x moves with the left channel (and the right one for shared boxes).
    const dx = id => LW + (sharedOf.has(id) ? RW : 0), f = id => { const b = band(id); return {x:b.x, y:b.y, w:b.w, h:b.h, bottom:b.y + b.h}; };
    const X = entry => round(entry.x + dx(entry.id));
    const gutterTop = key => key === "strip" ? rootBottom + total : rows[key].bottom + below(key);
    const trackY = ({key, index}) => round(gutterTop(key) + GUTTER_PAD + gutters.get(key).zone + (index + 0.5) * TRACK);
    const tagAt = (entry, key) => { const g = gutters.get(key); return {cx:X(entry), cy:round(gutterTop(key) + GUTTER_PAD + entry.row * (g.tagH + TAG_GAP) + entry.tag.h / 2)}; };
    const stackLeft = stack.x + LW, stackRight = stack.x + stack.w + LW;
    const laneX = (list, plan) => {
      const sorted = [...list].sort((a, b) => a.reach - b.reach || a.control.item.id.localeCompare(b.control.item.id)), i = sorted.indexOf(plan);
      return list === left ? round(stackLeft - CHANNEL_PAD - (i + 0.5) * LANE) : round(stackRight + CHANNEL_PAD + (i + 0.5) * LANE);
    };
    for (const plan of [...left, ...right]) plan.reach = Math.abs(trackY(plan.first) - trackY(plan.last ?? plan.first));
    for (const plan of plans) {
      const {item} = plan.control;
      let points, tag;
      if (plan.kind === "straight") {
        const x = X(plan.top), upper = f(plan.top.id).bottom, lower = f(plan.lower).y;
        points = plan.down ? [[x, upper], [x, lower]] : [[x, lower], [x, upper]]; tag = tagAt(plan.top, plan.key);
      } else if (plan.kind === "jog") {
        const xt = X(plan.top), xb = X(plan.bottom), y = trackY(plan.track), upper = f(plan.top.id).bottom, lower = f(plan.bottom.id).y;
        points = plan.down ? [[xt, upper], [xt, y], [xb, y], [xb, lower]] : [[xb, lower], [xb, y], [xt, y], [xt, upper]]; tag = tagAt(plan.top, plan.key);
      } else if (plan.kind === "peer" || plan.kind === "boxes") {
        const y = trackY(plan.track), xs = X(plan.top), xd = X(plan.end);
        points = [[xs, f(plan.top.id).bottom], [xs, y], [xd, y], [xd, f(plan.end.id).bottom]]; tag = tagAt(plan.top, plan.key);
      } else if (plan.kind === "left" || plan.kind === "right") {
        const lane = laneX(plan.channel, plan), y1 = trackY(plan.first), y2 = trackY(plan.last), xs = X(plan.kind === "left" ? plan.bottom : plan.top), xd = X(plan.kind === "left" ? plan.top : plan.bottom);
        const start = plan.kind === "left" ? f(item.from).y : f(item.from).bottom, end = plan.kind === "left" ? f(item.to).bottom : f(item.to).y;
        points = [[xs, start], [xs, y1], [lane, y1], [lane, y2], [xd, y2], [xd, end]]; tag = tagAt(plan.top, plan.key);
      } else if (plan.kind === "shared" || plan.kind === "far") {
        const xs = X(plan.top), y1 = trackY(plan.first), band = f(plan.top.id).bottom;
        let run;
        if (plan.kind === "shared") run = [[xs, band], [xs, y1], [f(plan.box).x, y1]];
        else { const lane = laneX(right, plan), y2 = trackY(plan.last), xk = X(plan.end); run = [[xs, band], [xs, y1], [lane, y1], [lane, y2], [xk, y2], [xk, f(plan.box).bottom]]; }
        points = plan.toShared ? run : run.reverse(); tag = tagAt(plan.top, plan.key);
      } else continue;
      draw(plan, points, tag);
    }
  }
  // One routed arrow: a polyline ending at an arrowhead sized to its stroke, and its tag.
  function draw(plan, points, tag) {
    const {control, width} = plan, [line, marker] = control.path.querySelectorAll("path");
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
  async function render(container, view, options = {}) {
    const current = () => !options.isCurrent || options.isCurrent();
    if (!current()) return {status:"stale"};
    try {
      if (document.fonts?.ready) await document.fonts.ready;
      if (!current()) return {status:"stale"};
      const canvas = document.createElement("canvas"), ctx = canvas.getContext("2d");
      if (!ctx) unsupported("Architecture text measurement unavailable");
      ctx.font = "14px Inter, sans-serif";
      const measure = text => ctx.measureText(text).width;
      if (typeof window.pele?.render !== "function") unsupported("Pele asset unavailable");
      // Fit the canvas width (#449, #525): start from the estimated chip columns, then size the
      // chips so Pele's natural layout fills the canvas without passing it. Once a fitting and
      // an overflowing chip width are known, bisect between them. Too wide with no fit shrinks
      // the chips, then drops a column; the narrowest layout wins when none fits.
      const sharedCount = options.display ? view.layers.filter(l => l.shared && options.display.layers.has(l.name)).length : 0;
      let columns = options.display ? chipColumns(options.width, sharedCount) : undefined, chipWidth = CHIP_WIDTH;
      let projection, raw, best = null, low = null, high = null;
      const fit = options.display && options.width > 0;
      // The side channels routing adds (#637) depend only on the arrows and their tags, never on
      // chip sizes: they are taken off the width before chip columns are chosen.
      let target = options.width;
      if (fit) { target -= channelReserve(project(view,{...options,columns,chipWidth,measureText:measure})); columns = chipColumns(target, sharedCount); }
      for (let attempt = 0; ; attempt++) {
        projection = project(view,{...options,columns,chipWidth,measureText:measure});
        raw = window.pele.render(projection.source,{fontSize:14,padding:8,responsive:false,links:false,images:false}).svg;
        if (!fit) break;
        const drawnWidth = Number(/^<svg\b[^>]*?\swidth="([0-9.]+)"/.exec(raw)?.[1]), cells = columns + sharedCount;
        const fits = drawnWidth <= target;
        if (!best || (fits && (!best.fits || drawnWidth > best.drawnWidth)) || (!fits && !best.fits && drawnWidth < best.drawnWidth)) best = {fits,drawnWidth,projection,raw,columns,chipWidth};
        if (attempt >= 12) break;
        let next;
        if (fits) {
          low = chipWidth;
          const slack = target - drawnWidth;
          if (slack < 2 * cells || chipWidth >= MAX_CHIP) break;
          next = high === null ? Math.min(MAX_CHIP, chipWidth + Math.max(2, Math.floor(slack / cells))) : Math.floor((low + high) / 2);
        } else {
          high = chipWidth;
          if (low !== null) next = Math.floor((low + high) / 2);
          else if (chipWidth > MIN_CHIP) next = Math.max(MIN_CHIP, chipWidth - Math.ceil((drawnWidth - target) / cells) - 1);
          else if (columns > 1) { columns--; high = null; next = CHIP_WIDTH; }
          else break;
        }
        if (low !== null && high !== null && high - low <= 1) break;
        chipWidth = next;
      }
      // Projecting charges the caller's chip budget, so the chosen layout is projected again
      // when it was not the last attempt; the same inputs give the same source and ids.
      if (best && best.projection !== projection) {
        projection = project(view,{...options,columns:best.columns,chipWidth:best.chipWidth,measureText:measure});
        raw = best.raw;
      }
      if (!current()) return {status:"stale"};
      const {root,controls} = validatedSvg(raw,projection);
      root.classList.add("trellis-architecture-svg"); root.setAttribute("font-size","14"); root.setAttribute("font-family","Inter, sans-serif");
      const palette = {"var(--_bg)":"#0B0C10","var(--_fg)":"#F7F7FB","var(--_m)":"#B8BAC8","var(--_l)":"#777A8E","var(--_s)":"#14151D","var(--_a)":"#1D1E2A","var(--_b)":"#777A8E"};
      for (const node of [root,...root.querySelectorAll("*")]) for (const attr of [...node.attributes]) {
        if (palette[attr.value]) node.setAttribute(attr.name,palette[attr.value]);
        if (attr.name === "rx" && attr.value === "var(--_r)") node.setAttribute("rx","4");
      }
      // A note's stable identity across re-layouts: its layer (null when unassigned) and its
      // ordinal among that layer's notes in projection order.
      const noteIds = new Map(), noteCounts = new Map();
      for (const item of [...projection.nodes.values()].filter(node => node.kind === "N").sort((a,b) => parseInt(a.id.slice(1),36) - parseInt(b.id.slice(1),36))) {
        const layer = typeof item.record?.layerKey === "string" ? item.record.layerKey : null, n = noteCounts.get(layer) || 0;
        noteCounts.set(layer, n + 1); noteIds.set(item.id, JSON.stringify([layer, n]));
      }
      for (const {node,label,item,path} of controls.values()) {
        replaceLabel(label,item);
        let callback, expanded = null;
        if (item.kind === "C") {
          // A summary chip: metric colour and name only; its band header is the one toggle.
          node.querySelector("rect")?.setAttribute("fill",item.band === null ? "#14151D" : colors[item.band]);
          label.setAttribute("fill",item.band > 0 ? "#0B0C10" : "#F7F7FB");
          node.setAttribute("data-trellis-summary-chip",typeof item.record.layerKey === "string" ? item.record.layerKey : "");
        } else if (item.kind === "M" || (item.kind === "D" && !item.composite)) {
          const fill = item.band === null ? "#14151D" : colors[item.band];
          node.querySelector("rect")?.setAttribute("fill",fill); label.setAttribute("fill",item.band > 0 ? "#0B0C10" : "#F7F7FB");
          callback = item.kind === "M" ? options.onSelectModule : options.onToggleDirectory;
          if (item.kind === "D") expanded = false;
        } else if (item.kind === "D") { callback = options.onToggleDirectory; expanded = true; }
        else if (item.kind === "U") { callback = options.onToggleUnassigned; expanded = item.expanded; }
        else if (item.kind === "N") callback = options.onSelectNote;
        else if (item.kind === "L") {
          callback = options.display ? options.onToggleLayer : options.onSelectLayer;
          if (options.display) expanded = item.expanded;
          if (item.statLines?.length) statChips(node,label,item,measure);
        } else if (item.kind === "E") {
          // Deviations are highlighted; allowed (conforming or excepted) arrows are dimmed, not dropped (#449).
          const danger = item.record.role === "production" && item.record.status === "deviates" && !item.record.excepted;
          const tone = item.tone || (danger ? "deviation" : "neutral");
          const color = tone === "deviation" ? "#FF8E9A" : tone === "allowed" ? "#777A8E" : "#B8BAC8";
          for (const p of path.querySelectorAll("path")) {
            p.setAttribute("stroke",color);
            if (p.classList.contains("pele-marker")) p.setAttribute("fill",color); else if (tone === "deviation") p.setAttribute("stroke-width","2");
          }
          const rect = node.querySelector("rect"); rect?.setAttribute("stroke",color); rect?.setAttribute("fill","#14151D"); label.setAttribute("fill",color);
          if (tone === "allowed") { path.setAttribute("opacity","0.45"); node.setAttribute("opacity","0.75"); }
          node.setAttribute("data-trellis-edge-tone",tone);
          callback = options.onSelectEdge;
        }
        if (!callback) continue;
        node.setAttribute("role","button"); node.setAttribute("tabindex","0"); node.setAttribute("aria-label",item.full); node.setAttribute("data-trellis-architecture-id",item.id);
        // Disclosure nodes report expansion; selectable evidence reports selection.
        if (expanded === null) node.setAttribute("aria-pressed","false"); else node.setAttribute("aria-expanded",String(expanded));
        if (item.kind === "M") node.setAttribute("data-trellis-module-id",identity(item.record));
        if (item.kind === "L") node.setAttribute("data-trellis-layer-id",item.record.name);
        if (item.kind === "D") node.setAttribute("data-trellis-directory-id",item.record.key);
        if (item.kind === "U") node.setAttribute("data-trellis-unassigned","true");
        if (item.kind === "N") node.setAttribute("data-trellis-note-id",noteIds.get(item.id));
        if (item.kind === "E") node.setAttribute("data-trellis-edge-id",edgeKey(item.record));
        const select = event => {
          event?.stopPropagation();
          if (!current()) return;
          if (expanded === null) for (const control of controls.values()) if (control.node.hasAttribute("aria-pressed")) control.node.setAttribute("aria-pressed",String(control.node === node));
          callback(item.record);
        };
        node.addEventListener("click",select); node.addEventListener("keydown",event => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); select(event); } });
      }
      if (options.display) route(root,controls,projection);
      if (!current()) return {status:"stale"};
      container.replaceChildren(root);
      return {status:"ready",projection};
    } catch (error) {
      if (!current()) return {status:"stale"};
      const message = document.createElement("p"); message.className = "architecture-render-error";
      message.textContent = `Architecture diagram unavailable: ${error.message}`; container.replaceChildren(message);
      return {status:"error",error};
    }
  }
  window.TrellisArchitecturePele = {project,validatedSvg,render,identity,edgeKey,edgeWidth};
})();
