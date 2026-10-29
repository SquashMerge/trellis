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
  const CHIP_WIDTH = 120, CHIP_COLUMNS = 5, SHARED_WIDTH = 160, STAT_GAP = "   ";
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
    let serial = 0;
    const clean = text => String(text).replace(/[\u0000-\u0009\u000b-\u001f\u007f]/g, " ");
    // `shown` is the drawn text when it is shorter than the accessible `text`.
    function label(kind, record, text, pad = false, shown = text, width = null) {
      const id = kind + (serial++).toString(36), value = clean(text);
      // Uniform safe placeholders reserve width before repository text is inserted.
      // A line is clipped, never shrunk; the accessible name keeps the full label.
      const lineWidth = width ?? (kind === "L" || kind === "U" ? (pad ? 460 : 380) : kind === "M" ? 150 : kind === "E" ? (pad ? 240 : 150) : kind === "N" ? 260 : 200);
      // A padded summary band is blank at its top, middle and bottom, where
      // edge counts that meet or cross it are drawn; padded edge counts use one line.
      const lines = clean(shown).split("\n");
      // A shared side box adds trailing blanks so its text sits above the rows' edge counts.
      const full = !pad ? lines : kind === "E" ? [lines.join(" ")] : ["", lines[0], "", ...lines.slice(1), "", ...(record?.shared ? Array(6).fill("") : [])], display = full.map(line => {
        let result = "";
        for (const c of line) { if (measure(result + c + "…") > lineWidth) return result + "…"; result += c; }
        return result || " ";
      });
      const reserve = "W".repeat(Math.ceil(lineWidth / measure("W")));
      const firstReserve = "W".repeat(Math.max(1,Math.ceil((lineWidth - measure(id + "_")) / measure("W"))));
      const placeholders = display.map((_, i) => i === 0 ? id + "_" + firstReserve : reserve);
      return {id,kind,record,display,lineWidth,full:value,placeholder:placeholders.join("<br/>"),expected:placeholders.join("")};
    }
    function addNode(kind, record, text, pad, shown, width) { const item = label(kind,record,text,pad,shown,width); nodes.set(item.id,item); return item; }
    const source = ["block-beta","columns 1"];
    function open(item, columns, span = 1) { item.composite = true; source.push(`block:${item.id}["${item.placeholder}"]:${span}`, `columns ${columns}`); }
    const outer = options.display ? null : addNode("B",null,"Architecture · structural references");
    const orderedStack = addNode("B",null,"Ordered layers");
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
    for (const layer of drawn) {
      const header = options.layerLabel?.(layer) ?? `${layer.name}\n${layer.shared ? "Shared" : `Order ${layer.order}`}`;
      // Non-zero reference counts become compact chips on their own header line.
      const stats = display ? (options.layerStats?.(layer) || []).filter(stat => typeof stat?.text === "string" && stat.text) : [];
      const chips = stats.map(stat => stat.text).join(STAT_GAP);
      const item = addNode("L",layer,stats.length ? `${header}\n${stats.map(stat => stat.label || stat.text).join(" · ")}` : header,
        false,stats.length ? `${header}\n${chips}` : header,display && layer.shared ? SHARED_WIDTH : null);
      item.stats = stats; item.statsLine = stats.length ? item.display.lastIndexOf(chips) : -1;
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
      const item = typeof name === "string" ? addNode("M",module,full,false,`${name}${value === null ? "\n—" : value === undefined ? "" : `\n${value}`}`,CHIP_WIDTH) : addNode("M",module,full);
      item.band = heat(options.moduleBand?.(module)); leaf(item);
    }
    function emitEntries(entries, span) {
      if (!entries.length) leaf(addNode("B",null,"No files shown",false,"No files shown",CHIP_WIDTH));
      for (const entry of entries) {
        if (entry.type === "module") moduleNode(entry.module, entry.name);
        else if (entry.type === "directory") {
          const item = addNode("D",entry,entry.label,false,entry.open ? entry.label : entry.shown ?? entry.label,entry.open ? null : CHIP_WIDTH);
          item.band = entry.open ? null : heat(entry.band);
          if (entry.open) { open(item,span,span); emitEntries(entry.entries,span); source.push("end"); } else leaf(item);
        } else if (entry.type === "note") leaf(addNode("N",entry,entry.label,false,entry.shown ?? entry.label,CHIP_WIDTH));
        else unsupported("Invalid Architecture display entry");
      }
    }
    // Pele gives a leaf row the height of the tallest sibling, so a folded band is a
    // composite holding one short "show" node instead of a leaf as tall as the stack.
    function folded(item, record, text, span) {
      open(item,1,span);
      const toggle = addNode("S",record,text,false,text,CHIP_WIDTH * 2); toggle.owner = item; leaf(toggle);
      source.push("end");
    }
    function emitLayer(layer, span = 1) {
      // Peers split the chip columns of one band row, so the stack keeps one width.
      const item = layers.get(layer.name), columns = layer.shared ? 1 : display ? Math.max(2, Math.floor(CHIP_COLUMNS * span / width)) : 4;
      const members = moduleRows.get(layer.name).sort((a,b) => identity(a).localeCompare(identity(b)));
      if (display) {
        item.expanded = !!display.expanded(layer.name);
        if (!item.expanded) { folded(item,layer,`Show module chips\n${members.length} files`,span); return; }
        open(item,columns,span); emitEntries(display.entries(layer.name,members),columns); source.push("end"); return;
      }
      open(item,columns);
      if (!members.length) leaf(addNode("B",null,"No indexed modules"));
      for (const m of members) moduleNode(m);
      source.push("end");
    }
    // Pele stretches a composite's children to equal grid columns, so the stack spans
    // several columns and each shared box one, keeping the shared boxes narrow.
    const stackSpan = display ? 5 : 4, orders = [...new Set(ordered.map(l => l.order))];
    const width = display ? Math.max(1, ...orders.map(order => ordered.filter(l => l.order === order).length)) : 1;
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
    const unassigned = moduleRows.get(null);
    if (display?.unassigned && unassigned.length) {
      const item = addNode("U",display.unassigned,display.unassigned.label); item.expanded = !!display.unassigned.expanded;
      if (item.expanded) { open(item,CHIP_COLUMNS,stackSpan + shared.length); emitEntries(display.entries(null,unassigned),CHIP_COLUMNS); source.push("end"); }
      else folded(item,display.unassigned,`Show unassigned files\n${unassigned.length} files`,stackSpan + shared.length);
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
      // Summary arrows draw a compact count; the accessible label keeps the words.
      const full = `${edge.count} ${edge.role} sites · ${way}\n${state}${edge.excepted ? " · exception" : ""}`;
      const shown = `${ARROWS[way]}${edge.count}${edge.excepted ? " excepted" : ""}${edge.role === "test" ? " test" : ""}`;
      const item = display ? label("E",edge,full,true,shown,Math.ceil(measure(`${shown}…`)) + 2) : label("E",edge,full);
      Object.assign(item,{from,to}); edges.set(item.id,item);
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
  // Replace the reserved header line with one compact chip per non-zero count.
  // Deviations use the error colour and keep their glyph, so colour is never the only cue.
  function statChips(node, label, item, measure) {
    const line = [...label.querySelectorAll("tspan")][item.statsLine];
    const x = Number(line?.getAttribute("x")), y = Number(line?.getAttribute("y"));
    if (!line || !Number.isFinite(x) || !Number.isFinite(y)) unsupported("Invalid Architecture header geometry");
    const widths = item.stats.map(stat => Math.ceil(measure(stat.text)) + 12), gap = 6;
    const total = widths.reduce((sum, width) => sum + width, 0) + gap * (widths.length - 1);
    const anchor = line.getAttribute("text-anchor") || label.getAttribute("text-anchor");
    let left = anchor === "middle" ? x - total / 2 : anchor === "end" ? x - total : x;
    line.textContent = "";
    item.stats.forEach((stat, i) => {
      const color = stat.tone === "error" ? "#FF8E9A" : "#B8BAC8", chip = document.createElementNS(NS,"rect"), text = document.createElementNS(NS,"text");
      for (const [name,value] of Object.entries({x:left,y:y-15,width:widths[i],height:20,fill:"#14151D",stroke:color,rx:3})) chip.setAttribute(name,String(value));
      for (const [name,value] of Object.entries({x:left + widths[i] / 2,y,fill:color,"text-anchor":"middle","font-size":"14"})) text.setAttribute(name,String(value));
      text.setAttribute("class","trellis-architecture-stat"); text.setAttribute("data-tone",stat.tone === "error" ? "error" : "neutral");
      text.textContent = stat.text; node.append(chip,text); left += widths[i] + gap;
    });
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
      const projection = project(view,{...options,measureText:measure});
      if (typeof window.pele?.render !== "function") unsupported("Pele asset unavailable");
      const raw = window.pele.render(projection.source,{fontSize:14,padding:8,responsive:false,links:false,images:false}).svg;
      if (!current()) return {status:"stale"};
      const {root,controls} = validatedSvg(raw,projection);
      root.classList.add("trellis-architecture-svg"); root.setAttribute("font-size","14"); root.setAttribute("font-family","Inter, sans-serif");
      const palette = {"var(--_bg)":"#0B0C10","var(--_fg)":"#F7F7FB","var(--_m)":"#B8BAC8","var(--_l)":"#777A8E","var(--_s)":"#14151D","var(--_a)":"#1D1E2A","var(--_b)":"#777A8E"};
      for (const node of [root,...root.querySelectorAll("*")]) for (const attr of [...node.attributes]) {
        if (palette[attr.value]) node.setAttribute(attr.name,palette[attr.value]);
        if (attr.name === "rx" && attr.value === "var(--_r)") node.setAttribute("rx","4");
      }
      for (const {node,label,item,path} of controls.values()) {
        replaceLabel(label,item);
        let callback, expanded = null;
        if (item.kind === "M" || (item.kind === "D" && !item.composite)) {
          const fill = item.band === null ? "#14151D" : colors[item.band];
          node.querySelector("rect")?.setAttribute("fill",fill); label.setAttribute("fill",item.band > 0 ? "#0B0C10" : "#F7F7FB");
          callback = item.kind === "M" ? options.onSelectModule : options.onToggleDirectory;
          if (item.kind === "D") expanded = false;
        } else if (item.kind === "D") { callback = options.onToggleDirectory; expanded = true; }
        else if (item.kind === "U") { callback = options.onToggleUnassigned; expanded = item.expanded; }
        else if (item.kind === "S") { callback = item.owner.kind === "U" ? options.onToggleUnassigned : options.onToggleLayer; expanded = false; }
        else if (item.kind === "N") callback = options.onSelectNote;
        else if (item.kind === "L") {
          callback = options.display ? options.onToggleLayer : options.onSelectLayer;
          if (options.display) expanded = item.expanded;
          if (item.statsLine >= 0) statChips(node,label,item,measure);
        } else if (item.kind === "E") {
          const danger = item.record.role === "production" && item.record.status === "deviates" && !item.record.excepted;
          const color = danger ? "#FF8E9A" : "#B8BAC8";
          for (const p of path.querySelectorAll("path")) { p.setAttribute("stroke",color); if (p.classList.contains("pele-marker")) p.setAttribute("fill",color); }
          const rect = node.querySelector("rect"); rect?.setAttribute("stroke",color); rect?.setAttribute("fill","#14151D"); label.setAttribute("fill",color);
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
        if (item.kind === "E") node.setAttribute("data-trellis-edge-id",edgeKey(item.record));
        const select = event => {
          event?.stopPropagation();
          if (!current()) return;
          if (expanded === null) for (const control of controls.values()) if (control.node.hasAttribute("aria-pressed")) control.node.setAttribute("aria-pressed",String(control.node === node));
          callback(item.record);
        };
        node.addEventListener("click",select); node.addEventListener("keydown",event => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); select(event); } });
      }
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
