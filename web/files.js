"use strict";
// Text-only presentation. The app owns selection, requests and snapshot fences.
(() => {
  const make = (tag, value, cls) => {
    const node = document.createElement(tag);
    if (value !== undefined) node.textContent = value;
    if (cls) node.className = cls;
    return node;
  };
  function outline(list, items, selected, choose, current) {
    list.replaceChildren();
    for (const item of items) {
      const symbol = item.symbol, row = make("li"), line = `L${symbol.range.startLine}`;
      // One row: icon and name, then kind and line right-aligned. Names truncate, with the full text as a tooltip.
      const pick = make("button", undefined, "files-symbol-pick"), icon = make("span", "◇", "files-symbol-icon");
      icon.setAttribute("aria-hidden", "true");
      const name = make("span", symbol.name, "files-symbol-name"); name.title = symbol.name;
      pick.append(icon, name, make("span", item.nativeKind, "files-symbol-kind"), make("span", line, "files-symbol-line"));
      pick.setAttribute("aria-label", `${symbol.name} · ${item.nativeKind} · ${line}`);
      pick.type = "button"; pick.setAttribute("data-symbol-id", symbol.id);
      pick.setAttribute("aria-pressed", String(selected?.symbol.id === symbol.id));
      pick.addEventListener("click", () => { if (current()) return choose(item); });
      row.append(pick); list.append(row);
    }
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
  window.TrellisFiles = {outline, headerText, matches, diff, lineClass, SYNTAX_LINE_LIMIT};
})();
