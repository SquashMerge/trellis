"use strict";
// The app switcher and the All apps drawing of the Architecture canvas: one column per app, the interface cards and
// the overlay lines. Moved whole from architecture.js (the controller), which passes what these read and call. The
// pure layout, routes and wording are architecture-apps.js.
(() => {
  // The narrow-screen drawer width (web/shell.js): the only width at which All apps columns stack (#949/#950).
  const DRAWER_WIDTH = "(max-width: 900px)";
  function create({get, make, apps, shared, scopeName, scopeGroup, setScope, openTab, inspect, pressOnly, openReview, asideBoxes}) {
    // The switcher and source line (#789): shown only with two or more detected apps. Choosing a
    // button re-draws the held snapshot and never reads.
    function switcher(payload) {
      const target = get("architecture-apps"), line = get("architecture-source-line"), groups = apps.groups(payload);
      const show = payload.state === "ready" && payload.apps.length >= 2 && groups.length > 0;
      const refocus = show && !target.hidden && !!target.contains?.(document.activeElement);
      target.hidden = line.hidden = !show; target.replaceChildren(); line.replaceChildren();
      const footnote = get("architecture-footnote"); footnote.hidden = !show;
      if (show) footnote.textContent = apps.footnote(window.TrellisIndexPin.label(payload));
      if (!show) return;
      const choose = (name, text) => {
        const button = make("button", text); button.type = "button"; button.setAttribute("aria-pressed", String(scopeName(payload) === name));
        button.addEventListener("click", () => { void setScope(name); }); target.append(button);
        if (refocus && scopeName(payload) === name) button.focus?.();
      };
      choose(null, "All apps");
      for (const item of groups) choose(item.name, apps.label(item));
      const group = scopeGroup(payload);
      line.append(make("span", apps.sourceLine(group, payload.apps.length)));
      if (group?.duplicate) line.append(make("span", apps.duplicateNotice(group), "architecture-app-notice"));
    }
    // ---- All apps (#790). Every string below that comes from the repository reaches the page through
    // textContent (make) or setAttribute; the overlay is built with createElementNS. ----
    const SVG_NS = "http://www.w3.org/2000/svg";
    const cardEntries = new WeakMap();
    const css = (node, props) => { if (node.style) Object.assign(node.style, props); };
    // A card or header says Selected, in words and in its name, exactly while it is pressed; its overlay lines follow.
    function syncCards() {
      const root = get("architecture-diagram"), on = new Set();
      for (const card of root.querySelectorAll?.("[data-trellis-card]") || []) {
        const entry = cardEntries.get(card), pressedNow = card.getAttribute("aria-pressed") === "true";
        if (!entry) continue;
        card.setAttribute("aria-label", apps.cardName(entry, pressedNow));
        if (pressedNow) on.add(apps.key(entry));
      }
      for (const line of root.querySelectorAll?.("[data-interface]") || []) line.setAttribute("data-selected", String(on.has(line.getAttribute("data-interface"))));
    }
    async function drawApps({full, parts, canvas, width, stack, active, layout}) {
      // Each gutter is as wide as the lines it carries (#949), so the lanes are counted before any column is drawn.
      // Columns stack only at the narrow-screen drawer width (#949/#950, owner ruling 2026-11-12); else the canvas scrolls.
      const laneCounts = shared.lanes(full, parts), drawer = !!window.matchMedia?.(DRAWER_WIDTH)?.matches;
      let plan = apps.layout({width, count:parts.length, lanes:laneCounts, drawer});
      const pele = window.TrellisArchitecturePele, grid = make("div", undefined, "architecture-apps-grid"), columns = [], boxes = [], bandsOf = new Map();
      // Side by side, the grid's tracks alternate column and gutter, and column i takes track 2i + 1.
      const place = () => {
        grid.dataset.layout = plan.sideBySide ? "columns" : "stacked";
        css(grid, {width:`${plan.width}px`, gridTemplateColumns:plan.sideBySide ? plan.widths.flatMap((each, index) => index ? [`${plan.gutters[index - 1]}px`, `${each}px`] : [`${each}px`]).join(" ") : `${plan.width}px`});
      };
      place();
      // Each column's Shared bands sit on the side away from its interface partners (#850).
      const sides = shared.sides(full, parts, plan.sideBySide);
      for (const part of parts) {
        const group = part.group, layered = part.slice.layers.some(layer => !layer.shared);
        const column = make("div", undefined, "architecture-app-column"), box = make("div", undefined, "architecture-app-stack");
        column.setAttribute("role", "group"); column.setAttribute("aria-label", group.name);
        const info = shared.columnHeader(part);
        const head = make("button", undefined, "architecture-app-header"); head.type = "button"; head.setAttribute("aria-pressed", "false");
        head.setAttribute("data-trellis-action-id", JSON.stringify(["app", group.name]));
        const line = make("span", info.deviations ? `${info.detail} · ` : info.detail, "architecture-app-line");
        // The count opens the Deviations tab on this app (#965); the rest of the header inspects the app.
        if (info.deviations) {
          const tone = make("span", info.deviations, "architecture-app-deviations"); tone.dataset.tone = "error";
          tone.setAttribute("title", `Open ${info.deviations} in Deviations`);
          tone.addEventListener("click", event => { event?.stopPropagation?.(); if (active()) openTab({app:group.name, edge:null}); });
          line.append(tone);
        }
        head.append(make("span", info.name, "architecture-app-name"), line);
        if (info.manifests) head.append(make("span", `Detected from ${info.manifests}`, "architecture-app-line"));
        head.append(make("span", "Selected", "architecture-selected-tag"));
        head.addEventListener("click", () => { if (!active()) return; inspect("app", group); pressOnly(head); });
        column.append(head, box); grid.append(column); columns.push(column); boxes.push(box);
        // The re-layout below only widens columns, so side by side stays decided and the track is this one.
        if (plan.sideBySide) css(column, {gridColumn:String(2 * columns.length - 1)});
        if (!layered) {
          const files = group.unassignedFileCount, button = make("button", `◇ ${files} unassigned · listed, never dropped`, "architecture-aside-box");
          button.type = "button"; button.dataset.tone = "warning"; button.addEventListener("click", () => { if (active()) openReview("unassigned"); });
          box.append(button); continue;
        }
        const outcome = await window.TrellisArchitecturePele.render(box, part.slice, stack(part, {width:plan.widths[columns.length - 1], title:`${group.name} · ordered layers`, sharedSide:sides.get(group.name)}));
        if (!active()) return false;
        if (outcome?.status !== "ready") throw new Error(outcome?.reason || outcome?.error?.message || "Architecture mapping failed. Nothing displayed.");
        bandsOf.set(group.name, outcome.bands);
      }
      canvas.replaceChildren(grid);
      // A drawing wider than its column (an unbreakable name) widens the column; the diagram scrolls.
      plan = apps.layout({width, count:parts.length, lanes:laneCounts, drawer, natural:boxes.map(box => Math.ceil(box.scrollWidth || 0))}); place();
      // Frames are in each drawing's own coordinates; the overlay and the cards live in the grid's.
      const rect = node => node.getBoundingClientRect?.() || null, origin = rect(grid), places = new Map();
      parts.forEach((part, index) => {
        const at = origin && rect(boxes[index]), dx = at ? at.left - origin.left : plan.columns[index].x, dy = at ? at.top - origin.top : 0;
        const bands = new Map([...(bandsOf.get(part.group.name) || [])].filter(([, frame]) => frame).map(([name, frame]) => [name, {x:frame.x + dx, y:frame.y + dy, w:frame.w, h:frame.h}]));
        places.set(part.group.name, {index, bands, top:dy});
      });
      const entriesOf = full.interfaces.filter(entry => entry && places.has(entry.client) && places.has(entry.server) && entry.client !== entry.server);
      const routed = apps.routes({interfaces:entriesOf}, plan, places, pele.edgeWidth), floor = new Map(), rest = make("div", undefined, "architecture-apps-cards");
      let bottom = 0;
      for (const entry of entriesOf) {
        const words = apps.endsText(entry, routed.undrawn.get(entry) || []), button = make("button", undefined, "architecture-interface-card");
        button.type = "button"; button.setAttribute("aria-pressed", "false"); button.setAttribute("data-trellis-action-id", apps.key(entry)); button.setAttribute("data-trellis-card", "interface");
        const client = places.get(entry.client), server = places.get(entry.server), gutter = Math.min(client.index, server.index);
        // A gutter narrower than a card (#949) lists the card under the grid, as for columns that are not adjacent;
        // away from its gutter a card names its interface, as the Herdr pane's cards do.
        const floats = plan.sideBySide && Math.abs(client.index - server.index) === 1 && apps.cardFits(plan.gutters[gutter]);
        if (plan.sideBySide && !floats) button.append(make("span", `${entry.client} → ${entry.server}`, "architecture-card-line"));
        for (const row of apps.cardText(entry)) {
          const item = make("span", row.text, "architecture-card-line");
          if (row.strong) item.dataset.strong = "true";
          if (row.tone) item.dataset.tone = row.tone;
          button.append(item);
        }
        for (const text of words) button.append(make("span", text, "architecture-card-ends"));
        button.append(make("span", "Selected", "architecture-selected-tag"));
        button.setAttribute("aria-label", apps.cardName(entry)); cardEntries.set(button, entry);
        button.addEventListener("click", () => { if (!active()) return; inspect("interface", entry); pressOnly(button); });
        if (!plan.sideBySide) columns[client.index].after(button);
        else if (floats) {
          // Below its own lines, so the card never covers a run; later cards in the gutter stack under it.
          const ys = routed.lines.filter(item => item.entry === entry).map(item => item.y), start = ys.length ? Math.max(...ys) + 14 : Math.min(client.top, server.top);
          const top = Math.max(start, floor.get(gutter) || 0);
          button.dataset.gutter = "true"; css(button, {left:`${plan.columns[gutter].x + plan.columns[gutter].width}px`, top:`${top}px`, width:`${plan.gutters[gutter]}px`});
          columns[client.index].after(button);
          const height = button.offsetHeight || 96;
          floor.set(gutter, top + height + 8); bottom = Math.max(bottom, top + height);
        } else rest.append(button);
      }
      if (plan.sideBySide && bottom) css(grid, {minHeight:`${bottom + 8}px`});
      if (routed.lines.length) {
        const overlay = document.createElementNS(SVG_NS, "svg");
        overlay.setAttribute("class", "architecture-apps-overlay"); overlay.setAttribute("aria-hidden", "true"); overlay.setAttribute("focusable", "false");
        overlay.setAttribute("width", String(plan.width)); overlay.setAttribute("height", String(Math.max(grid.scrollHeight || 0, bottom + 8)));
        for (const item of routed.lines) {
          const path = document.createElementNS(SVG_NS, "path"), arrow = document.createElementNS(SVG_NS, "polygon");
          path.setAttribute("class", "architecture-apps-line"); path.setAttribute("d", item.path); path.setAttribute("stroke-width", String(item.width));
          arrow.setAttribute("class", "architecture-apps-arrow"); arrow.setAttribute("points", item.arrow);
          for (const node of [path, arrow]) { node.setAttribute("data-interface", apps.key(item.entry)); node.setAttribute("data-selected", "false"); overlay.append(node); }
        }
        grid.append(overlay);
      }
      if (rest.children.length) canvas.append(rest);
      if (!entriesOf.length) {
        const names = parts.map(part => part.group.name), last = names.pop();
        canvas.append(make("p", `No HTTP interface measured between ${names.length ? `${names.join(", ")} and ` : ""}${last}. This does not prove absence.`, "detail"));
      }
      canvas.append(asideBoxes(full, parts, layout, active));
      return true;
    }
    return {switcher, syncCards, drawApps};
  }
  window.TrellisArchitectureAppColumns = {create};
})();
