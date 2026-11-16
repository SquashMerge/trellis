"use strict";
// The identities of the Architecture canvas's drawn nodes: how a new drawing finds a node again, and where focus
// returns after one. Moved whole from architecture.js (the controller), which passes the two things these read.
(() => {
  function create({get, hiddenToggle}) {
    // A drawn node's stable identity (its data attribute), so a new drawing can find it again.
    const NODE_KEYS = ["data-trellis-module-id", "data-trellis-edge-id", "data-trellis-layer-id", "data-trellis-directory-id", "data-trellis-unassigned", "data-trellis-note-id", "data-trellis-action-id"];
    function nodeKey(node) {
      if (!node) return null;
      if (node === hiddenToggle) return ["id", hiddenToggle.id];
      const name = NODE_KEYS.find(key => node.hasAttribute?.(key));
      return name ? [name, node.getAttribute(name)] : null;
    }
    function findNode(key) {
      if (!key) return null;
      if (key[0] === "id") return hiddenToggle;
      const find = (name, value) => [...(get("architecture-diagram").querySelectorAll?.(`[${name}]`) || [])].find(node => node.getAttribute(name) === value) || null;
      const found = find(key[0], key[1]);
      if (found || key[0] !== "data-trellis-note-id") return found;
      // A note the new drawing no longer has falls back to its owning band (or the unassigned row).
      let layer = null;
      try { [layer] = JSON.parse(key[1]); } catch { return null; }
      return typeof layer === "string" ? find("data-trellis-layer-id", layer) : find("data-trellis-unassigned", "true");
    }
    const pressedNode = () => [...(get("architecture-diagram").querySelectorAll?.('[aria-pressed="true"]') || [])][0] || null;
    function refocus(target) {
      if (target.hidden) { hiddenToggle.focus?.(); return; }
      for (const node of get("architecture-diagram").querySelectorAll?.("[role=button]") || []) {
        if ((target.layer !== undefined && node.getAttribute("data-trellis-layer-id") === target.layer) ||
            (target.directory !== undefined && node.getAttribute("data-trellis-directory-id") === target.directory) ||
            (target.unassigned && node.hasAttribute("data-trellis-unassigned"))) { node.focus(); return; }
      }
    }
    return {nodeKey, findNode, pressedNode, refocus};
  }
  window.TrellisArchitectureDrawing = {create};
})();
