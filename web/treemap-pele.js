"use strict";
// Exact vendored Pele .2 owns layout. Indexed records own identity and evidence.
(() => {
  const NS = "http://www.w3.org/2000/svg";
  const THEME = "--_bg:var(--pele-bg,#fff);--_fg:var(--pele-fg,#1f1f1f);--_m:var(--pele-muted,#6e6e6e);--_l:var(--pele-line,#1f1f1f);--_s:var(--pele-surface,#f3f3f3);--_a:var(--pele-surface-alt,#e6e6e6);--_b:var(--pele-border,#8a8a8a);--_c:var(--pele-accent,#5b7bd5);color:var(--_fg)";
  const FONT = ";--_fm:var(--pele-font-mono,monospace);font:14px sans-serif;letter-spacing:normal;word-spacing:normal;text-transform:none;direction:ltr;font-family:var(--pele-font,sans-serif)";
  const colors = ["#343644","#FFC20A","#FFAA10","#FF7A00"];
  const classifications = {production:"#ABA5FF",mixed:"#FFAA10",test:"#85C7FF",none:"#343644"};
  const selections = new WeakMap();
  const identity = file => JSON.stringify([file.sourceSetId,file.language,file.path]);
  const directoryKey = directory => JSON.stringify([directory.sourceSetId,directory.path]);
  const bytes = value => new TextEncoder().encode(value).length;
  const compare = (a,b) => a < b ? -1 : a > b ? 1 : 0;
  function unsupported(message) { const error = new Error(message); error.code = "unsupported"; throw error; }
  function project(files, options = {}) {
    if (!Array.isArray(files)) unsupported("Invalid Treemap files");
    const nodes = new Map(), directories = new Map(), seen = new Set(), zeroArea = [];
    let serial = 0, total = 0;
    for (const file of files) if (!file || !["sourceSetId","language","path"].every(k => typeof file[k] === "string") || !file.path || !Number.isSafeInteger(file.physicalLines) || file.physicalLines < 0) unsupported("Invalid Treemap file");
    const ordered = [...files].sort((a,b) => compare(identity(a),identity(b)));
    function directory(sourceSetId,path) {
      const key = directoryKey({sourceSetId,path});
      if (directories.has(key)) return directories.get(key);
      const missing = []; let cursor = path;
      while (!directories.has(directoryKey({sourceSetId,path:cursor}))) {
        missing.push(cursor);
        if (!cursor) break;
        const slash = cursor.lastIndexOf("/"); cursor = slash < 0 ? "" : cursor.slice(0,slash);
      }
      for (let i=missing.length-1;i>=0;i--) {
        const value = missing[i], record = {sourceSetId,path:value}, itemKey = directoryKey(record);
        const item = {id:"D"+(serial++).toString(36),kind:"directory",record,key:itemKey,total:0,children:[]};
        directories.set(itemKey,item); nodes.set(item.id,item);
        if (nodes.size > 12000) unsupported("Treemap has too many nodes");
        if (value) {
          const slash = value.lastIndexOf("/"), parentPath = slash < 0 ? "" : value.slice(0,slash);
          directories.get(directoryKey({sourceSetId,path:parentPath})).children.push(item);
        }
      }
      return directories.get(key);
    }
    for (const file of ordered) {
      if (!file || !["sourceSetId","language","path"].every(k => typeof file[k] === "string") || !file.path || !Number.isSafeInteger(file.physicalLines) || file.physicalLines < 0 || seen.has(identity(file))) unsupported("Invalid or duplicate Treemap file");
      seen.add(identity(file));
      if (!file.physicalLines) { zeroArea.push(file); continue; }
      total += file.physicalLines;
      if (!Number.isSafeInteger(total)) unsupported("Treemap area exceeds integer precision");
      const parts = file.path.split("/"); parts.pop();
      const parent = directory(file.sourceSetId,parts.join("/"));
      const value = (options.fileValue || options.moduleValue)?.(file);
      const label = file.path.slice(file.path.lastIndexOf("/")+1) + (value == null ? "" : ` · ${value}`);
      const item = {id:"F"+(serial++).toString(36),kind:"file",record:file,key:identity(file),total:file.physicalLines,label,full:`${file.sourceSetId} · ${file.language} · ${file.path} · ${file.physicalLines} physical file lines`,band:options.fileBand?.(file)};
      nodes.set(item.id,item); parent.children.push(item);
      // Totals count each included file exactly once at each ancestor.
      let path = parts.join("/");
      for (;;) {
        directory(file.sourceSetId,path).total += file.physicalLines;
        if (!path) break;
        const slash = path.lastIndexOf("/"); path = slash < 0 ? "" : path.slice(0,slash);
      }
    }
    const source = ["treemap-beta"]; let sourceBytes = bytes("treemap-beta\n");
    // 100 px per sqrt(file count), at least 640 px and at most 4000 px.
    // Pele's nodeWidth/nodeHeight units are ten pixels. No viewport rescaling.
    const size = Math.min(4000,Math.max(640,Math.ceil(Math.sqrt(files.length-zeroArea.length)*100)));
    const roots = [...directories.values()].filter(item => !item.record.path).sort((a,b) => compare(a.key,b.key));
    const stack = roots.slice().reverse().map(item => ({item,depth:0}));
    while (stack.length) {
      const {item,depth} = stack.pop();
      if (item.kind === "directory") {
        item.label = `${item.record.path ? item.record.path.slice(item.record.path.lastIndexOf("/")+1) : item.record.sourceSetId} · ${item.total} lines`;
        item.full = `${item.record.sourceSetId} · ${item.record.path || "/"} · ${item.total} physical file lines`;
      }
      const line = `${"  ".repeat(depth)}"${item.id}"${item.kind === "file" ? `: ${item.total}` : ""}`;
      sourceBytes += bytes(line)+1;
      if (sourceBytes > 512*1024) unsupported("Treemap Mermaid source too large");
      source.push(line);
      if (item.children) {
        item.children.sort((a,b) => compare(a.key,b.key));
        for (let i=item.children.length-1;i>=0;i--) stack.push({item:item.children[i],depth:depth+1});
      }
      if (source.length > 12000) unsupported("Treemap has too many nodes");
    }
    const text = source.join("\n")+"\n";
    if (bytes(text) > 512*1024) unsupported("Treemap Mermaid source too large");
    return {source:text,nodes,files:ordered,zeroArea,total,config:{treemap:{nodeWidth:size/10,nodeHeight:size/10,showValues:false}},size};
  }
  function validatedSvg(raw, projection) {
    if (typeof raw !== "string" || bytes(raw) > 8*1024*1024) unsupported("Treemap SVG too large");
    if (!raw.startsWith('<svg xmlns="http://www.w3.org/2000/svg" class="pele pele-treemap"')) unsupported("Changed Pele Treemap root");
    const theme = raw.match(/ style="([^"]*)"/);
    const style = THEME+";--_r:var(--pele-radius,4px)";
    if (!theme || ![THEME+FONT,style+FONT,style+";--_hw:var(--pele-heading-weight,bold)"+FONT].includes(theme[1])) unsupported("Changed Pele Treemap theme");
    const safe = raw.replace(theme[0],"").replaceAll(' style="rx:var(--_r)"',' rx="var(--_r)"');
    if (/\sstyle\s*=|<\/?(?:script|style|foreignObject|image|a)\b|<!/i.test(safe)) unsupported("Unexpected Treemap SVG markup");
    const root = new DOMParser().parseFromString(safe,"image/svg+xml").documentElement;
    if (root.localName !== "svg" || root.namespaceURI !== NS || root.querySelector("parsererror")) unsupported("Invalid Treemap SVG");
    const tags = new Set(["svg","g","rect","text","title"]);
    const attrs = new Set(["xmlns","class","viewBox","width","height","fill","fill-opacity","role","aria-roledescription","stroke","font-size","data-id","x","y","rx","text-anchor","font-weight"]);
    let count = 0;
    for (const node of [root,...root.querySelectorAll("*")]) {
      if (++count > 12000 || node.namespaceURI !== NS || !tags.has(node.localName)) unsupported("Unexpected Treemap SVG element");
      for (const attr of node.attributes) if (!attrs.has(attr.name) || /url\s*\(|javascript:|<|>|[\u0000-\u001f]/i.test(attr.value)) unsupported("Unexpected Treemap SVG attribute");
    }
    const dimension = projection.size+16;
    if (root.getAttribute("class") !== "pele pele-treemap" || root.getAttribute("width") !== String(dimension) || root.getAttribute("height") !== String(dimension) || root.getAttribute("viewBox") !== `0 0 ${dimension} ${dimension}`) unsupported("Changed Treemap viewport");
    const controls = new Map();
    let sections = 0, leaves = 0;
    for (const group of root.children) {
      const kind = group.getAttribute("class");
      if (group.localName !== "g" || !["pele-sections","pele-leaves"].includes(kind) || group.hasAttribute("data-id")) unsupported("Changed Treemap root structure");
      if (kind === "pele-sections" ? ++sections > 1 : ++leaves > 1) unsupported("Duplicate Treemap group");
      if (kind === "pele-sections" && group.getAttribute("font-size") !== "12") unsupported("Changed Treemap header font");
      for (const node of group.children) {
        const id = node.getAttribute("data-id"), item = projection.nodes.get(id);
        if (!item || controls.has(id)) unsupported("Missing, extra or duplicate Treemap identity");
        const section = item.kind === "directory";
        if (node.localName !== "g" || node.getAttribute("class") !== `pele-node pele-${section ? "section" : "leaf"}` || kind !== (section ? "pele-sections" : "pele-leaves")) unsupported("Changed Treemap node type");
        const children = [...node.children], title = children[0]?.localName === "title" ? children.shift() : null, rect = children.shift(), label = children.shift();
        if (rect?.localName !== "rect" || children.length || (label && label.localName !== "text") || rect.children.length || title?.children.length || label?.children.length) unsupported("Changed Treemap node structure");
        if (title && title.textContent !== id) unsupported("Changed Treemap title placeholder");
        if (label && !(label.textContent === id || (label.textContent.endsWith("…") && label.textContent.length > 1 && id.startsWith(label.textContent.slice(0,-1))))) unsupported("Changed Treemap label placeholder");
        const geometry = Object.fromEntries(["x","y","width","height"].map(key => [key,Number(rect.getAttribute(key))]));
        if (!["x","y","width","height"].every(key => rect.hasAttribute(key) && Number.isFinite(geometry[key]) && geometry[key] >= 0) || geometry.x+geometry.width > dimension || geometry.y+geometry.height > dimension) unsupported("Invalid Treemap geometry");
        if (label) {
          const fontSize = Number(label.getAttribute("font-size") || (section ? 12 : 14));
          const x = Number(label.getAttribute("x")), y = Number(label.getAttribute("y"));
          if (!Number.isFinite(x) || !Number.isFinite(y) || x < geometry.x || x > geometry.x+geometry.width || y < geometry.y || y > geometry.y+geometry.height || (section ? fontSize !== 12 : !Number.isInteger(fontSize) || fontSize < 8 || fontSize > 14)) unsupported("Invalid Treemap label geometry");
        }
        controls.set(id,{node,rect,label,title,item,geometry});
      }
    }
    if (controls.size !== projection.nodes.size || root.querySelectorAll("[data-id]").length !== controls.size || root.querySelectorAll("rect").length !== controls.size || root.querySelectorAll("text").length !== [...controls.values()].filter(c => c.label).length || root.querySelectorAll("title").length !== [...controls.values()].filter(c => c.title).length) unsupported("Incomplete Treemap native map");
    // Full-path accessibility adds one SVG title to every native node that
    // lacks one. Admit that final detached DOM before decoration or handlers.
    const addedTitles = [...controls.values()].filter(control => !control.title).length;
    if (count+addedTitles > 12000) unsupported("Treemap decorated SVG has too many nodes");
    return {root,controls};
  }
  function fit(text,width,measure) {
    // Only label fitting: no new tile, line, font size or geometry is introduced.
    const clean = String(text).replace(/[\u0000-\u001f\u007f]/g," ");
    if (measure(clean) <= width) return clean;
    if (measure("…") > width) return "";
    const chars = Array.from(clean); let low = 0, high = chars.length;
    while (low < high) { const mid = Math.ceil((low+high)/2); if (measure(chars.slice(0,mid).join("")+"…") <= width) low=mid; else high=mid-1; }
    return chars.slice(0,low).join("")+"…";
  }
  function select(container,key) { selections.get(container)?.(key); }
  async function render(container,files,options = {}) {
    const current = () => !options.isCurrent || options.isCurrent();
    if (!current()) return {status:"stale"};
    try {
      if (document.fonts?.ready) await document.fonts.ready;
      if (!current()) return {status:"stale"};
      const ctx = document.createElement("canvas").getContext("2d");
      if (!ctx) unsupported("Treemap text measurement unavailable");
      const projection = project(files,options);
      if (typeof window.pele?.render !== "function") unsupported("Pele asset unavailable");
      const result = await window.pele.render(projection.source,{config:projection.config,fontSize:14,padding:8,responsive:false,links:false,images:false});
      if (!current()) return {status:"stale"};
      const {root,controls} = validatedSvg(result.svg,projection);
      root.classList.add("trellis-treemap-svg"); root.setAttribute("font-family","Inter, sans-serif"); root.setAttribute("font-size","14");
      for (const {node,rect,label,title,item,geometry} of controls.values()) {
        const section = item.kind === "directory";
        if (label) {
          const size = Number(label.getAttribute("font-size") || (section ? 12 : 14));
          ctx.font = `${section ? "bold " : ""}${size}px Inter, sans-serif`;
          const x = Number(label.getAttribute("x"));
          const width = section ? geometry.x+geometry.width-8-x : 2*Math.max(0,Math.min(x-geometry.x-4,geometry.x+geometry.width-4-x));
          label.textContent = fit(item.label,Math.max(0,width),text => ctx.measureText(text).width);
          if (section) label.setAttribute("font-weight","bold");
          label.setAttribute("fill",section || item.band === 0 || item.band == null || item.band === "none" ? "#F7F7FB" : "#0B0C10");
        }
        const tooltip = title || document.createElementNS(NS,"title");
        tooltip.textContent = item.full; if (!title) node.insertBefore(tooltip,rect);
        rect.setAttribute("rx","2");
        rect.setAttribute("fill",section ? "#1D1E2A" : (colors[item.band] || classifications[item.band] || "#343644"));
        if (section) { rect.setAttribute("fill-opacity","1"); rect.setAttribute("stroke","#777A8E"); }
        const testOnly = !section && (item.record.fileRole === "test" || (item.record.roleCounts?.production === 0 && item.record.roleCounts?.test > 0));
        if (testOnly) { rect.setAttribute("stroke-dasharray","4 3"); rect.setAttribute("stroke","#B8BAC8"); }
        node.setAttribute("role","button"); node.setAttribute("tabindex","0"); node.setAttribute("aria-label",item.full);
        node.setAttribute("aria-pressed",String(options.selectedKey === item.key));
        node.setAttribute(section ? "data-trellis-directory-id" : "data-trellis-file-id",item.key);
        const select = event => {
          event.stopPropagation(); if (!current()) return;
          for (const control of controls.values()) control.node.setAttribute("aria-pressed",String(control.node === node));
          (section ? options.onSelectDirectory : options.onSelectFile)?.(item.record);
        };
        node.addEventListener("click",select);
        node.addEventListener("keydown",event => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); select(event); } });
      }
      if (!current()) return {status:"stale"};
      container.replaceChildren(root);
      selections.set(container,key => { if (current()) for (const {node,item} of controls.values()) node.setAttribute("aria-pressed",String(item.key === key)); });
      return {status:"ready",projection};
    } catch (error) {
      if (!current()) return {status:"stale"};
      selections.delete(container);
      const message = document.createElement("p"); message.className = "treemap-render-error";
      message.textContent = `Treemap unavailable: ${error.message}`; container.replaceChildren(message);
      return {status:"error",error};
    }
  }
  window.TrellisTreemapPele = {project,validatedSvg,render,select,identity,directoryKey};
})();
