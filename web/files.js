"use strict";
// Text-only presentation. The app owns selection, requests and snapshot fences.
(() => {
  const make = (tag, value, cls) => {
    const node = document.createElement(tag);
    if (value !== undefined) node.textContent = value;
    if (cls) node.className = cls;
    return node;
  };
  // The outline lists types and callables (#526). Fields and nested types or callables are a
  // container's members, shown when it is expanded; parameters, locals, aliases, closures and
  // implementation blocks are not rows. A Rust `impl` block lends its methods to the type it
  // implements in this file, or else becomes a row named by that type; the daemon names the type
  // from the parsed cached file. An impl it could not name lends its methods to the top level.
  const CONTAINERS = new Set(["type", "namespace"]);
  const CALLABLES = new Set(["function", "method", "constructor"]);
  const MEMBERS = new Set(["field"]);
  function outlineTree(items) {
    const byId = new Map(items.map(item => [item.symbol.id, item]));
    const nodes = new Map(), roots = [];
    const node = item => { let entry = nodes.get(item.symbol.id); if (!entry) { entry = {item, children:[]}; nodes.set(item.symbol.id, entry); } return entry; };
    // An impl names its type; prefer the type declared beside it (same parent), then any of that name.
    const typesByName = new Map();
    for (const item of items) if (item.nativeKind === "type") typesByName.set(item.symbol.name, [...(typesByName.get(item.symbol.name) || []), item]);
    const typeFor = (name, impl) => { const types = typesByName.get(name) || []; return types.find(type => type.symbol.parent === impl.symbol.parent) || types[0]; };
    // A binding whose whole initializer is a function (`const f = () => {}`, `const g = function () {}`,
    // `f = lambda: …`) is a callable row. The daemon classifies it from the parsed cached file;
    // a value that merely contains or calls a function is not, and without that evidence no binding is.
    const callable = item => CALLABLES.has(item.nativeKind) || (item.nativeKind === "variable" && item.callableInitializer === true);
    // A named impl container per target type that is not declared in this file.
    const implRows = new Map(), implOwner = new Map();
    for (const item of items) {
      if (item.nativeKind !== "implementation") continue;
      const target = typeof item.implTarget === "string" && item.implTarget ? item.implTarget : null;
      if (!target) continue;
      const type = typeFor(target, item);
      if (type) { implOwner.set(item.symbol.id, node(type)); continue; }
      let row = implRows.get(target);
      if (!row) { row = {item, label:target, children:[]}; implRows.set(target, row); nodes.set(item.symbol.id, row); }
      implOwner.set(item.symbol.id, row);
    }
    // The nearest shown ancestor owns an item; hidden kinds are walked through, except that
    // anything inside a callable belongs to that callable's body and is not listed.
    function owner(item) {
      const seen = new Set();
      for (let parent = byId.get(item.symbol.parent); parent && !seen.has(parent.symbol.id); parent = byId.get(parent.symbol.parent)) {
        seen.add(parent.symbol.id);
        if (callable(parent) || parent.nativeKind === "anonymousFunction") return false;
        if (CONTAINERS.has(parent.nativeKind)) return node(parent);
        if (parent.nativeKind === "implementation") return implOwner.get(parent.symbol.id) || null;
      }
      return null;
    }
    for (const row of implRows.values()) roots.push(row);
    for (const item of items) {
      const kind = item.nativeKind;
      if (!CONTAINERS.has(kind) && !callable(item) && !MEMBERS.has(kind)) continue;
      const parent = owner(item);
      if (parent === false) continue;
      if (!parent) { if (!MEMBERS.has(kind)) roots.push(node(item)); continue; }
      parent.children.push(node(item));
    }
    const order = (a, b) => (a.item.symbol.range.startByte ?? 0) - (b.item.symbol.range.startByte ?? 0) || a.item.symbol.range.startLine - b.item.symbol.range.startLine;
    const sort = list => { list.sort(order); for (const entry of list) sort(entry.children); return list; };
    return sort(roots);
  }
  // The outline rows that contain `id`, outermost first: what must open to show it.
  function containersOf(tree, id) {
    for (const entry of tree) {
      if (entry.item.symbol.id === id) return [];
      const inner = containersOf(entry.children, id);
      if (inner) return [entry.item.symbol.id, ...inner];
    }
    return null;
  }
  function outline(list, items, selected, choose, current, options = {}) {
    list.replaceChildren();
    const expanded = options.expanded || new Set(), toggle = options.toggle || (() => {});
    const tree = outlineTree(items);
    // A newly selected member opens its containers once; the reader may close them again.
    if (selected && options.reveal) for (const key of containersOf(tree, selected.symbol.id) || []) expanded.add(key);
    function rows(target, entries) {
      for (const entry of entries) {
        const {item} = entry, symbol = item.symbol, row = make("li"), line = `L${symbol.range.startLine}`;
        const label = entry.label || symbol.name, kind = entry.label ? "impl" : item.nativeKind;
        const key = symbol.id, open = entry.children.length > 0 && expanded.has(key);
        const head = make("div", undefined, "files-symbol-row");
        if (entry.children.length) {
          const twist = make("button", open ? "▾" : "▸", "files-symbol-toggle"); twist.type = "button";
          twist.setAttribute("aria-expanded", String(open));
          twist.setAttribute("aria-label", `${open ? "Collapse" : "Expand"} ${label} members (${entry.children.length})`);
          twist.addEventListener("click", () => { if (current()) toggle(key, !open); });
          head.append(twist);
        } else head.append(make("span", undefined, "files-symbol-toggle-space"));
        // One row: the icon marks a container or a callable and the name takes the rest. Kind and
        // line are in the tooltip and the accessible name, and selecting a row shows them in the inspector.
        const pick = make("button", undefined, "files-symbol-pick"), icon = make("span", CONTAINERS.has(kind) || kind === "impl" ? "◆" : "◇", "files-symbol-icon");
        icon.setAttribute("aria-hidden", "true");
        pick.append(icon, make("span", label, "files-symbol-name"));
        pick.title = `${label} · ${kind} · ${line}`; pick.setAttribute("aria-label", pick.title);
        pick.type = "button"; pick.setAttribute("data-symbol-id", symbol.id);
        pick.setAttribute("aria-pressed", String(selected?.symbol.id === symbol.id));
        pick.addEventListener("click", () => { if (current()) return choose(item); });
        head.append(pick); row.append(head); target.append(row);
        if (open) { const members = make("ul", undefined, "files-symbol-members"); rows(members, entry.children); row.append(members); }
      }
    }
    rows(list, tree);
    return tree.length;
  }
  function headerText(header) {
    if (!header) return "Measured header unavailable";
    const fields = [header.name && `Name · ${header.name}`, header.kind && `Kind · ${header.kind}`];
    if (header.modifiers?.length) fields.push(`Modifiers · ${header.modifiers.join(" ")}`);
    if (header.typeParameters?.length) fields.push(`Type parameters · ${header.typeParameters.join(", ")}`);
    for (const parameter of header.parameters || []) fields.push(`Parameter · ${parameter.name ?? "unnamed"}${parameter.type ? " · " + parameter.type : ""}${parameter.variadic ? " · variadic" : ""}`);
    if (header.bases?.length) fields.push(`Bases · ${header.bases.join(", ")}`);
    return fields.filter(Boolean).join("\n");
  }
  function matches(list, items, open, current) {
    for (const item of items) {
      const row = make("li"), pick = make("button", undefined, "files-match");
      pick.type = "button";
      pick.setAttribute("data-path", item.path); pick.setAttribute("data-start-byte", item.range.startByte); pick.setAttribute("data-end-byte", item.range.endByte);
      pick.append(make("span", `${item.path}:${item.range.startLine}:${item.range.startColumn}`, "files-match-location"), make("span", item.snippet, "files-match-snippet"));
      pick.addEventListener("click", () => { if (current()) return open(item); });
      row.append(pick); list.append(row);
    }
  }
  function diff(container, data) {
    container.replaceChildren();
    if (data?.git?.state !== "available") {
      container.append(make("p", `Working tree unavailable · ${data?.git?.reason || "status unavailable"}`)); return;
    }
    const file = data.file;
    if (!file) { container.append(make("p", "No uncommitted changes for this path. Refresh to measure Git again.")); return; }
    container.append(make("p", `${file.status} · ${file.previousPath ? file.previousPath + " → " : ""}${file.path}`, "files-diff-status"));
    if (file.binary) { container.append(make("p", "Binary file · no textual hunks.")); return; }
    if (!file.hunks.length) container.append(make("p", "No textual hunks are available for this change."));
    for (const hunk of file.hunks) {
      container.append(make("pre", hunk.header, "files-diff-hunk"));
      let oldLine = hunk.oldStart, newLine = hunk.newStart;
      for (const line of hunk.lines) {
        const row = make("div", undefined, "files-diff-line");
        row.setAttribute("data-kind", line.kind);
        row.append(make("span", line.kind === "added" ? "" : String(oldLine++), "files-diff-old"),
          make("span", line.kind === "deleted" ? "" : String(newLine++), "files-diff-new"),
          make("span", line.kind === "added" ? "+" : line.kind === "deleted" ? "−" : " ", "files-diff-mark"),
          make("span", line.text, "files-diff-text"));
        container.append(row);
      }
    }
  }
  // Subtle whole-line syntax colouring, as in the design: attributes and
  // annotations in gold, comments muted. It classifies a line by its prefix
  // and only adds a class, so text stays one text node and is never parsed as
  // HTML. Large files are left plain.
  const SYNTAX_LINE_LIMIT = 20000, PREFIX = 64;
  // Only languages known to use `//` and `/* * */` comments; others (Markdown, YAML…) stay plain.
  const C_COMMENT_LANGUAGES = ["rust", "java", "javascript", "typescript", "go", "c", "cpp", "csharp", "kotlin", "scala", "swift"];
  function lineClass(language, text, lineCount) {
    if (lineCount > SYNTAX_LINE_LIMIT) return "";
    const head = text.slice(0, PREFIX).trimStart();
    if (language === "rust" && /^#!?\[/.test(head)) return "syntax-attribute";
    if (["java", "python", "javascript", "typescript"].includes(language) && /^@[A-Za-z_]/.test(head)) return "syntax-attribute";
    if (language === "python") return head.startsWith("#") ? "syntax-comment" : "";
    if (C_COMMENT_LANGUAGES.includes(language) && /^(\/\/|\/\*|\*\/|\*(\s|$))/.test(head)) return "syntax-comment";
    return "";
  }
  window.TrellisFiles = {outline, outlineTree, headerText, matches, diff, lineClass, SYNTAX_LINE_LIMIT};
})();
