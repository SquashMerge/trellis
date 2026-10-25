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
  function unsupported(message) { const e = new Error(message); e.code = "unsupported"; throw e; }
  function project(view, options = {}) {
    if (view?.state !== "ready" || !Array.isArray(view.layers) || !Array.isArray(view.modules) || !Array.isArray(view.edges)) unsupported("Invalid Architecture projection");
    const nodes = new Map(), edges = new Map(), layers = new Map(), native = new Set();
    const measure = options.measureText || (text => Array.from(text).length * 14);
    let serial = 0;
    function label(kind, record, text) {
      const id = kind + (serial++).toString(36), value = String(text).replace(/[\u0000-\u0009\u000b-\u001f\u007f]/g, " ");
      // Uniform safe placeholders reserve width before repository text is inserted.
      // A line is clipped, never shrunk; the accessible name keeps the full label.
      const lineWidth = kind === "L" ? 380 : 150;
      const full = value.split("\n"), display = full.map(line => {
        let result = "";
        for (const c of line) { if (measure(result + c + "…") > lineWidth) return result + "…"; result += c; }
        return result || " ";
      });
      const reserve = "W".repeat(Math.ceil(lineWidth / measure("W")));
      const firstReserve = "W".repeat(Math.max(1,Math.ceil((lineWidth - measure(id + "_")) / measure("W"))));
      const placeholders = display.map((_, i) => i === 0 ? id + "_" + firstReserve : reserve);
      return {id,kind,record,display,full:value,placeholder:placeholders.join("<br/>"),expected:placeholders.join("")};
    }
    function addNode(kind, record, text) { const item = label(kind,record,text); nodes.set(item.id,item); return item; }
    const source = ["block-beta","columns 1"];
    function open(item, columns, span = 1) { item.composite = true; source.push(`block:${item.id}["${item.placeholder}"]:${span}`, `columns ${columns}`); }
    const outer = addNode("B",null,"Architecture · structural references");
    const orderedStack = addNode("B",null,"Ordered layers");
    const ordered = view.layers.filter(l => !l.shared).sort((a,b) => a.order-b.order || a.name.localeCompare(b.name));
    const shared = view.layers.filter(l => l.shared).sort((a,b) => a.name.localeCompare(b.name));
    for (const layer of view.layers) {
      if (typeof layer.name !== "string" || layers.has(layer.name) || typeof layer.shared !== "boolean" || (!layer.shared && !Number.isSafeInteger(layer.order))) unsupported("Invalid or duplicate Architecture layer");
      layers.set(layer.name,addNode("L",layer,options.layerLabel?.(layer) ?? `${layer.name}\n${layer.shared ? "Shared" : `Order ${layer.order}`}`));
    }
    const moduleRows = new Map(view.layers.map(l => [l.name,[]])); moduleRows.set(null,[]);
    for (const module of view.modules) {
      if (!module.identity || !["sourceSetId","language","path","sid"].every(k => typeof module.identity[k] === "string") || native.has(identity(module)) || !moduleRows.has(module.layer)) unsupported("Invalid or duplicate Architecture module");
      native.add(identity(module));
      const value = options.metric === "layer" ? undefined : options.moduleValue?.(module);
      const item = addNode("M",module,`${module.identity.path}${value === null ? "\nUnavailable" : value === undefined ? "" : `\n${value}`}`);
      const band = options.moduleBand?.(module);
      item.band = Number.isInteger(band) && band >= 0 && band < 4 ? band : null;
      moduleRows.get(module.layer).push(item);
    }
    function emitLayer(layer) {
      const item = layers.get(layer.name); open(item,layer.shared ? 1 : 4);
      const members = moduleRows.get(layer.name).sort((a,b) => identity(a.record).localeCompare(identity(b.record)));
      if (!members.length) { const empty = addNode("B",null,"No indexed modules"); source.push(`${empty.id}["${empty.placeholder}"]`); }
      for (const m of members) source.push(`${m.id}["${m.placeholder}"]`);
      source.push("end");
    }
    open(outer,4 + shared.length); open(orderedStack,1,4);
    if (!ordered.length) { const empty = addNode("B",null,"No ordered layers"); source.push(`${empty.id}["${empty.placeholder}"]`); }
    for (const order of [...new Set(ordered.map(l => l.order))]) {
      const peers = ordered.filter(l => l.order === order);
      if (peers.length > 1) open(addNode("B",null,`Peer layers · order ${order}`),peers.length);
      peers.forEach(emitLayer); if (peers.length > 1) source.push("end");
    }
    source.push("end"); shared.forEach(emitLayer); source.push("end");
    if (moduleRows.get(null).length) {
      open(addNode("B",null,"Unassigned / unknown"),4);
      for (const m of moduleRows.get(null)) source.push(`${m.id}["${m.placeholder}"]`);
      source.push("end");
    }
    const seenEdges = new Set();
    for (const edge of view.edges) {
      if (!layers.has(edge.sourceLayer) || !layers.has(edge.destinationLayer) || edge.sourceLayer === edge.destinationLayer || !["production","test"].includes(edge.role) || !["conforms","deviates","unknown"].includes(edge.status) || !Number.isSafeInteger(edge.count) || edge.count <= 0 || typeof edge.excepted !== "boolean" || seenEdges.has(edgeKey(edge))) unsupported("Invalid or duplicate Architecture edge");
      seenEdges.add(edgeKey(edge));
      if (edge.role === "test" && !options.showTests) continue;
      const from = layers.get(edge.sourceLayer).id, to = layers.get(edge.destinationLayer).id;
      const state = edge.status === "deviates" ? "deviation" : edge.status;
      const item = label("E",edge,`${edge.count} ${edge.role} sites\n${state}${edge.excepted ? " · exception" : ""}`);
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
  async function render(container, view, options = {}) {
    const current = () => !options.isCurrent || options.isCurrent();
    if (!current()) return {status:"stale"};
    try {
      if (document.fonts?.ready) await document.fonts.ready;
      if (!current()) return {status:"stale"};
      const canvas = document.createElement("canvas"), ctx = canvas.getContext("2d");
      if (!ctx) unsupported("Architecture text measurement unavailable");
      ctx.font = "14px Inter, sans-serif";
      const projection = project(view,{...options,measureText:text => ctx.measureText(text).width});
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
        let callback;
        if (item.kind === "M") {
          const fill = item.band === null ? "#14151D" : colors[item.band];
          node.querySelector("rect")?.setAttribute("fill",fill); label.setAttribute("fill",item.band > 0 ? "#0B0C10" : "#F7F7FB");
          callback = options.onSelectModule;
        } else if (item.kind === "L") {
          callback = options.onSelectLayer;
          const headerLines = [...label.querySelectorAll("tspan")];
          for (const line of headerLines) {
            if (!/^Up deviation: [1-9]/.test(line.textContent)) continue;
            line.setAttribute("fill","#FF8E9A");
            const x = Number(line.getAttribute("x")), y = Number(line.getAttribute("y"));
            if (!Number.isFinite(x) || !Number.isFinite(y)) unsupported("Invalid Architecture header geometry");
            const badge = document.createElementNS(NS,"rect");
            for (const [name,value] of Object.entries({x:x-192,y:y-15,width:384,height:20,fill:"#14151D",stroke:"#FF8E9A",rx:3})) badge.setAttribute(name,String(value));
            node.insertBefore(badge,label);
          }
        } else if (item.kind === "E") {
          const danger = item.record.role === "production" && item.record.status === "deviates" && !item.record.excepted;
          const color = danger ? "#FF8E9A" : "#B8BAC8";
          for (const p of path.querySelectorAll("path")) { p.setAttribute("stroke",color); if (p.classList.contains("pele-marker")) p.setAttribute("fill",color); }
          const rect = node.querySelector("rect"); rect?.setAttribute("stroke",color); rect?.setAttribute("fill","#14151D"); label.setAttribute("fill",color);
          callback = options.onSelectEdge;
        }
        if (!callback) continue;
        node.setAttribute("role","button"); node.setAttribute("tabindex","0"); node.setAttribute("aria-label",item.full); node.setAttribute("aria-pressed","false"); node.setAttribute("data-trellis-architecture-id",item.id);
        if (item.kind === "M") node.setAttribute("data-trellis-module-id",identity(item.record));
        if (item.kind === "L") node.setAttribute("data-trellis-layer-id",item.record.name);
        if (item.kind === "E") node.setAttribute("data-trellis-edge-id",edgeKey(item.record));
        const select = event => {
          event?.stopPropagation();
          if (!current()) return;
          for (const control of controls.values()) if (control.node.hasAttribute("aria-pressed")) control.node.setAttribute("aria-pressed",String(control.node === node));
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
