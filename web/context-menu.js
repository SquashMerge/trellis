// The one context menu of the page, shared by Classes, Deviations and navigation. It was part of
// the Classes controller, which is still its host: the host says what "current" means and runs
// the chosen action.
(() => {
  "use strict";
  let host = null, menu = null, menuAnchor = null;
  let menuContext = null, menuExpanded = null, menuOnClose = null, menuScroll = new Map();
  // host: {context(): c, valid(c): boolean, invoke(run, c): Promise}. A menu belongs to the
  // context it opened in; an action runs through the host, which drops it once that is stale.
  // Call once: it also installs the page-wide dismissal listeners.
  function bind(given) {
    host = given;
    document.addEventListener("pointerdown", event => { if (menu && !menu.contains(event.target)) closeContextMenu(false); }, true);
    document.addEventListener("focusin", event => { if (menu && (!host.valid(menuContext) || !menu.contains(event.target))) closeContextMenu(false); });
    document.addEventListener("scroll", event => {
      if (!menu || menu.contains(event.target)) return;
      const previous = menuScroll.get(event.target);
      if (!previous || (previous[0] === event.target.scrollTop && previous[1] === event.target.scrollLeft)) return;
      closeContextMenu();
    }, true);
    window.addEventListener("resize", () => closeContextMenu());
  }
  function closeContextMenu(restore = true, reason = "dismiss") {
    const closing = menu, anchor = menuAnchor, expanded = menuExpanded, onClose = menuOnClose;
    // Clear ownership before DOM, callbacks or focus can re-enter dismissal.
    menu = menuAnchor = menuContext = menuOnClose = null; menuExpanded = null; menuScroll.clear();
    if (closing) closing.remove();
    if (anchor) { if (expanded === null) anchor.removeAttribute("aria-expanded"); else anchor.setAttribute("aria-expanded", expanded); }
    if (onClose) onClose(reason);
    if (restore && !menu && anchor && anchor.isConnected) anchor.focus({preventScroll: true});
  }
  // Public menu actions: [{label: string, run: () => void|Promise, disabled?: boolean}].
  // The event's currentTarget is the focus-return anchor. Root must reset on session/revision changes.
  // Optional onClose(reason): "dismiss", "replace" or "action"; called once.
  // The returned handle can dismiss only the menu created by this call.
  function showContextMenu(event, actions, options = {}) {
    event.preventDefault(); event.stopPropagation();
    closeContextMenu(false, "replace");
    menuAnchor = event.currentTarget || event.target || document.activeElement;
    menuExpanded = menuAnchor.getAttribute("aria-expanded");
    // A queued scroll event from bringing the trigger into view must not close
    // the newly opened menu. Actual subsequent scrolling still dismisses it.
    for (let ancestor = menuAnchor, depth = 0; ancestor && depth < 64; ancestor = ancestor.parentNode, depth++) {
      if (typeof ancestor.scrollTop === "number" && typeof ancestor.scrollLeft === "number") {
        menuScroll.set(ancestor, [ancestor.scrollTop, ancestor.scrollLeft]);
      }
    }
    menuContext = host.context();
    const c = menuContext;
    menu = document.createElement("div"); menu.className = "classes-context-menu";
    const createdMenu = menu;
    menuOnClose = typeof options.onClose === "function" ? options.onClose : null;
    menu.setAttribute("role", "menu"); menu.setAttribute("aria-label", options.label || "Class actions");
    menuAnchor.setAttribute("aria-expanded", "true");
    const buttons = [];
    for (const action of actions) {
      const item = document.createElement("button");
      if (action.label !== undefined) item.textContent = action.label;
      item.type = "button"; item.setAttribute("role", "menuitem"); item.tabIndex = -1;
      item.disabled = !!action.disabled;
      item.addEventListener("click", () => {
        if (menu !== createdMenu) return;
        closeContextMenu(true, "action"); return host.invoke(action.run, c);
      });
      menu.append(item); if (!item.disabled) buttons.push(item);
    }
    menu.addEventListener("keydown", event => {
      if (menu !== createdMenu) return;
      if (!host.valid(c)) { closeContextMenu(false); return; }
      if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); closeContextMenu(); return; }
      if (event.key === "Tab") { closeContextMenu(); return; }
      const index = buttons.indexOf(document.activeElement);
      let next;
      if (event.key === "ArrowDown") next = (index + 1) % buttons.length;
      if (event.key === "ArrowUp") next = (index - 1 + buttons.length) % buttons.length;
      if (event.key === "Home") next = 0;
      if (event.key === "End") next = buttons.length - 1;
      if (next !== undefined && buttons[next]) { event.preventDefault(); event.stopPropagation(); buttons[next].focus(); }
    });
    document.body.append(menu);
    const rect = menuAnchor.getBoundingClientRect();
    const keyboard = event.type === "keydown" || (!event.clientX && !event.clientY);
    const x = keyboard ? rect.left : event.clientX, y = keyboard ? rect.bottom : event.clientY;
    const bounds = menu.getBoundingClientRect();
    menu.style.left = `${Math.max(8, Math.min(x, window.innerWidth - bounds.width - 8))}px`;
    menu.style.top = `${Math.max(8, Math.min(y, window.innerHeight - bounds.height - 8))}px`;
    if (buttons[0]) buttons[0].focus({preventScroll: true});
    return {close: () => { if (menu === createdMenu) closeContextMenu(); }};
  }
  window.TrellisContextMenu = Object.freeze({bind, showContextMenu, closeContextMenu});
})();
