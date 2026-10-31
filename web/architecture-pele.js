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
  // Count-tag search: the farthest a tag may sit from its connector, and the connector
  // points tried (#525). The far bound keeps a tag beside its own arrow in a crowded drawing.
  const TAG_REACH = 200, TAG_POINTS = 48;
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
    function addNode(kind, record, text, pad, shown, width) { const item = label(kind,record,text,pad,shown,width); nodes.set(item.id,item); return item; }
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
      layers.set(layer.name,item);
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
    function emitLayer(layer, span = 1) {
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
    if (display?.unassigned && unassigned.length) {
      const item = addNode("U",display.unassigned,display.unassigned.label,false,display.unassigned.label,rowWidth(COLUMNS + shared.length)); item.expanded = opened.has(null);
      if (item.expanded) { open(item,COLUMNS + shared.length,stackSpan + shared.length); emitEntries(opened.get(null),COLUMNS + shared.length); source.push("end"); }
      else {
        const chips = display.summary ? display.summary(null,unassigned,COLUMNS + shared.length) : [];
        if (chips.length) { open(item,COLUMNS + shared.length,stackSpan + shared.length); emitEntries(chips,COLUMNS + shared.length); source.push("end"); }
        else folded(item,display.unassigned,`${unassigned.length} files folded`,stackSpan + shared.length);
      }
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
      source.push(`${from} ${edge.role === "test" ? "-." : "--"} "${item.placeholder}" ${edge.role === "test" ? ".->" : "-->"} ${to}`);
    }
    const text = source.join("\n") + "\n";
    if (bytes(text) > 512 * 1024) unsupported("Architecture Mermaid source too large");
    return {source:text,nodes,edges,view};
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
    if (seenEdges.size !== projection.edges.size || paths.size !== projection.edges.size || root.querySelectorAll("[data-id]").length !== projection.nodes.size + 2 * projection.edges.size) unsupported("Missing or unexpected Architecture edge");
    if (root.querySelectorAll("text").length !== projection.nodes.size + projection.edges.size || root.querySelectorAll("rect").length !== projection.nodes.size + projection.edges.size || root.querySelectorAll("path").length !== 2 * projection.edges.size) unsupported("Unexpected Architecture SVG decoration");
    return {root,controls};
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
  // Text extents of one drawn line: 14px Inter, anchored at its x.
  function lineBox(x, y, width, anchor) {
    const left = anchor === "middle" ? x - width / 2 : anchor === "end" ? x - width : x;
    return {x:left, y:y - 15, w:width, h:20};
  }
  const overlaps = (a, b) => a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
  // Points every few units along a connector's absolute M/L/Q/C path.
  function pathPoints(d) {
    const points = [];
    let at = null;
    for (const [, command, args] of d.matchAll(/([MLQC])([^MLQC]*)/g)) {
      const values = (args.match(/-?\d*\.?\d+(?:e[-+]?\d+)?/gi) || []).map(Number);
      const pairs = []; for (let i = 0; i + 1 < values.length; i += 2) pairs.push([values[i], values[i + 1]]);
      if (!pairs.length || pairs.some(pair => !pair.every(Number.isFinite))) return [];
      const end = pairs[pairs.length - 1];
      if (command === "M" || !at) { at = end; points.push(at); continue; }
      const curve = t => {
        const [p0, ...rest] = [at, ...pairs], all = [p0, ...rest];
        // De Casteljau over the segment's control points (linear, quadratic or cubic).
        let level = all;
        while (level.length > 1) level = level.slice(1).map((point, i) => [level[i][0] + (point[0] - level[i][0]) * t, level[i][1] + (point[1] - level[i][1]) * t]);
        return level[0];
      };
      const length = Math.hypot(end[0] - at[0], end[1] - at[1]) + (pairs.length > 1 ? Math.hypot(pairs[0][0] - at[0], pairs[0][1] - at[1]) : 0);
      const steps = Math.max(1, Math.ceil(length / 3));
      for (let i = 1; i <= steps; i++) points.push(curve(i / steps));
      at = end;
    }
    return points;
  }
  // Count tags sit on or beside their own connector, clear of every header line, stat
  // chip, drawn chip and other tag (#525). Each tag takes the nearest free spot to its
  // connector, preferring the connector's middle; a tag with no free spot within reach
  // keeps Pele's place. Text and boxes are obstacles; connector strokes and band borders are not.
  function placeTags(root, controls, measure) {
    const width = Number(root.getAttribute("width")), height = Number(root.getAttribute("height"));
    const obstacles = [];
    for (const text of root.querySelectorAll(".pele-cluster-label")) {
      const anchor = text.getAttribute("text-anchor") || "start", spans = [...text.querySelectorAll("tspan")];
      for (const line of spans.length ? spans : [text]) {
        const content = line.textContent;
        if (!content.trim()) continue;
        const x = Number(line.getAttribute("x") ?? text.getAttribute("x")), y = Number(line.getAttribute("y") ?? text.getAttribute("y"));
        if (Number.isFinite(x) && Number.isFinite(y)) obstacles.push(lineBox(x, y, measure(content), line.getAttribute("text-anchor") || anchor));
      }
    }
    for (const text of root.querySelectorAll("text.trellis-architecture-stat")) {
      const chip = text.previousElementSibling;
      obstacles.push({x:Number(chip.getAttribute("x")), y:Number(chip.getAttribute("y")), w:Number(chip.getAttribute("width")), h:Number(chip.getAttribute("height"))});
    }
    for (const node of root.querySelectorAll(".pele-node")) {
      const shift = /translate\(\s*(-?[\d.]+)[ ,]+(-?[\d.]+)\s*\)/.exec(node.getAttribute("transform") || ""), rect = node.querySelector("rect");
      const dx = shift ? Number(shift[1]) : 0, dy = shift ? Number(shift[2]) : 0;
      obstacles.push({x:dx + Number(rect.getAttribute("x")), y:dy + Number(rect.getAttribute("y")), w:Number(rect.getAttribute("width")), h:Number(rect.getAttribute("height"))});
    }
    // A coarse grid keeps each test to the few obstacles near a candidate.
    const CELL = 48, grid = new Map(), cells = box => {
      const keys = [];
      for (let i = Math.floor(box.x / CELL); i <= Math.floor((box.x + box.w) / CELL); i++)
        for (let j = Math.floor(box.y / CELL); j <= Math.floor((box.y + box.h) / CELL); j++) keys.push(`${i},${j}`);
      return keys;
    };
    const add = box => { for (const key of cells(box)) { if (!grid.has(key)) grid.set(key, []); grid.get(key).push(box); } };
    // A tag keeps a small clearance from every obstacle, so its box never touches text.
    const CLEAR = 2, inflate = box => ({x:box.x - CLEAR, y:box.y - CLEAR, w:box.w + 2 * CLEAR, h:box.h + 2 * CLEAR});
    const free = box => {
      if (box.x < 1 || box.y < 1 || box.x + box.w > width - 1 || box.y + box.h > height - 1) return false;
      const wide = inflate(box);
      return !cells(wide).some(key => grid.get(key)?.some(other => overlaps(wide, other)));
    };
    obstacles.filter(box => [box.x, box.y, box.w, box.h].every(Number.isFinite)).forEach(add);
    // Offsets around a connector point, nearest first: on the connector, then beside it.
    // A finer near pass, then a coarser far pass for a crowded narrow drawing.
    const ring = (reach, stepX, stepY, skip) => {
      const list = [];
      for (let dx = -reach; dx <= reach; dx += stepX) for (let dy = -reach; dy <= reach; dy += stepY)
        if (!skip || Math.abs(dx) > skip || Math.abs(dy) > skip) list.push([dx, dy]);
      return list.sort((a, b) => Math.hypot(a[0], a[1]) - Math.hypot(b[0], b[1]) || a[1] - b[1] || a[0] - b[0]);
    };
    const offsets = [...ring(96, 4, 3, 0), ...ring(TAG_REACH, 8, 6, 96)];
    // Wider tags choose first, while there is most room; ties keep a stable order.
    const tagWidth = control => Number(control.node.querySelector("rect")?.getAttribute("width")) || 0;
    const tags = [...controls.values()].filter(control => control.item.kind === "E" && control.path)
      .sort((a, b) => tagWidth(b) - tagWidth(a) || a.item.id.localeCompare(b.item.id));
    for (const {node, path} of tags) {
      const rect = node.querySelector("rect"), text = node.querySelector("text");
      const box = {x:Number(rect.getAttribute("x")), y:Number(rect.getAttribute("y")), w:Number(rect.getAttribute("width")), h:Number(rect.getAttribute("height"))};
      const points = pathPoints(path.querySelector("path")?.getAttribute("d") || "");
      if (![box.x, box.y, box.w, box.h].every(Number.isFinite)) continue;
      if (!points.length) { add(box); continue; }
      // At most TAG_POINTS evenly spaced connector points bound the search on a long connector.
      const stride = Math.max(1, Math.ceil(points.length / TAG_POINTS)), sampled = points.filter((_, i) => i % stride === 0);
      const middle = (sampled.length - 1) / 2, order = sampled.map((point, i) => [point, Math.abs(i - middle)]).sort((a, b) => a[1] - b[1]).map(([point]) => point);
      let chosen = null;
      for (const [dx, dy] of offsets) {
        for (const [px, py] of order) {
          const candidate = {x:Math.round((px + dx - box.w / 2) * 100) / 100, y:Math.round((py + dy - box.h / 2) * 100) / 100, w:box.w, h:box.h};
          if (free(candidate)) { chosen = candidate; break; }
        }
        if (chosen) break;
      }
      if (chosen) {
        const moveX = chosen.x - box.x, moveY = chosen.y - box.y;
        rect.setAttribute("x", String(chosen.x)); rect.setAttribute("y", String(chosen.y));
        text.setAttribute("x", String(Math.round((Number(text.getAttribute("x")) + moveX) * 100) / 100));
        text.setAttribute("y", String(Math.round((Number(text.getAttribute("y")) + moveY) * 100) / 100));
      }
      add(chosen || box);
    }
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
      for (let attempt = 0; ; attempt++) {
        projection = project(view,{...options,columns,chipWidth,measureText:measure});
        raw = window.pele.render(projection.source,{fontSize:14,padding:8,responsive:false,links:false,images:false}).svg;
        if (!fit) break;
        const drawnWidth = Number(/^<svg\b[^>]*?\swidth="([0-9.]+)"/.exec(raw)?.[1]), cells = columns + sharedCount;
        const fits = drawnWidth <= options.width;
        if (!best || (fits && (!best.fits || drawnWidth > best.drawnWidth)) || (!fits && !best.fits && drawnWidth < best.drawnWidth)) best = {fits,drawnWidth,projection,raw,columns,chipWidth};
        if (attempt >= 12) break;
        let next;
        if (fits) {
          low = chipWidth;
          const slack = options.width - drawnWidth;
          if (slack < 2 * cells || chipWidth >= MAX_CHIP) break;
          next = high === null ? Math.min(MAX_CHIP, chipWidth + Math.max(2, Math.floor(slack / cells))) : Math.floor((low + high) / 2);
        } else {
          high = chipWidth;
          if (low !== null) next = Math.floor((low + high) / 2);
          else if (chipWidth > MIN_CHIP) next = Math.max(MIN_CHIP, chipWidth - Math.ceil((drawnWidth - options.width) / cells) - 1);
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
      if (options.display) placeTags(root,controls,measure);
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
  window.TrellisArchitecturePele = {project,validatedSvg,render,identity,edgeKey};
})();
