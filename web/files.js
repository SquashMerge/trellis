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
      const symbol = item.symbol, row = make("li");
      const pick = make("button", `${item.nativeKind} · ${symbol.name} · L${symbol.range.startLine}`, "files-symbol-pick");
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
  window.TrellisFiles = {outline, headerText, matches, diff};
})();
