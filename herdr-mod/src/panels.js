// Makes the tab bar's Files button reachable in a layout that puts `trellis-files` in the
// side panel (#707). The shipped `tabs` mod draws a Files or Changes button only for the
// names in shared "workspace.panels" (both when unset; the welcome narrows it). A Files button
// is the only way to the panel's `files` child, so a layout whose `files` child is
// `trellis-files` needs "files" there. `setShared` from a mod is the supported way to set it.
const PANELS = "workspace.panels";

function find(node, id) {
  if (node?.id === id) return node;
  for (const child of node?.children ?? []) {
    const found = find(child, id);
    if (found) return found;
  }
  return null;
}

// The new "workspace.panels" value, or null when it needs no change: the layout has no
// `panel-body` stack whose `files` child is `trellis-files`, or "files" is already enabled.
export function panelsWithFiles(current, root) {
  const files = find(root, "panel-body")?.children?.find((child) => child?.id === "files");
  if (files?.content?.mod !== "trellis-files") return null;
  if (current == null) return null; // unset: the tab bar shows both buttons
  const list = Array.isArray(current) ? current.filter((name) => typeof name === "string") : [];
  return list.includes("files") ? null : ["files", ...list];
}

export function ensureFilesPanel({ shared, setShared, layout }) {
  const next = panelsWithFiles(shared(PANELS), layout());
  if (next) setShared(PANELS, next);
  return next;
}
