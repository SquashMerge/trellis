"use strict";
// The Sequence view controller: open, load, boundary highlight, fit, column header and start list. Declarations only; app.js wires them.
// One reversible detail setting controls non-call rows and incidental calls (#594).
// This visible shortcut and Options → Show all steps always flip the same setting. An enriched
// view returns to Interactions (#636); a syntax-only one to calls-first.
function renderIncidentalToggle(data, options) {
  const button = $("sequence-incidental");
  if (!button) return;
  const count = Number.isSafeInteger(data?.incidentalSteps) ? data.incidentalSteps : 0, showing = !!$("all-steps").checked;
  const interactions = !!window.TrellisSequence?.display(data, {...options, showDetails:false}).interactions;
  button.hidden = false;
  button.textContent = showing ? interactions ? "Show interactions" : "Show calls" : "Show all steps";
  button.title = `${interactions ? "Interactions draw calls between columns and fold the rest into badges." : "Calls-first keeps conditional and unknown context."} Show all restores admitted non-call steps and ${count} incidental ${count === 1 ? "call" : "calls"}; sequence limits still apply.`;
  button.setAttribute("aria-pressed", String(showing));
}
// The method Fit was asked for: a redraw of it keeps Fit, another opens at its own size (#558).
let sequenceFitted = null;
// Open in Sequence from Architecture (#940). Its read can take seconds, so the header says a
// sequence is opening until the Sequence tab shows it. An open that the index or another view
// supersedes, or that fails, is never dropped silently: the note says why and offers Retry,
// which opens the same caller and calls at the head shown then. A newer open replaces the note.
let sequenceOpen = null;
function showSequenceOpen(text, retry = null) {
  $("sequence-open-label").textContent = text;
  $("sequence-open-retry").hidden = !retry; $("sequence-open-close").hidden = !retry;
  $("sequence-open").dataset.state = retry ? "stopped" : "opening";
  $("sequence-open").hidden = false;
}
function clearSequenceOpen() {
  sequenceOpen = null;
  $("sequence-open").hidden = true; $("sequence-open-label").textContent = "";
}
async function openArchitectureSequence(caller, sites, pin, saved, active) {
  const open = {session:epoch, root:selectedRootKey, retry:null};
  sequenceOpen = open;
  const mine = () => sequenceOpen === open && open.session === epoch && open.root === selectedRootKey;
  const stop = text => {
    if (!mine()) return;
    // Retry owns the view as the original open did: a navigation, view, section or source
    // change made while its read is pending drops it with the note, never a takeover.
    open.retry = () => {
      const context = navigationContext(), marks = viewMarks();
      return openArchitectureSequence(caller, sites, context.revision, saved, () => navigationCurrent(context) && viewMarks() === marks);
    };
    showSequenceOpen(text, open.retry);
  };
  const cancelled = () => stop(pin && status?.revision && !IndexPin.equal(pin, status.revision)
    ? "The index moved to a newer revision; Open in Sequence was cancelled." : "The view changed; Open in Sequence was cancelled.");
  if (!active() || !saved || !pin || !IndexPin.equal(pin, status?.revision)) return cancelled();
  showSequenceOpen(`Opening sequence for ${caller.name || "the caller"}…`);
  let data;
  try {
    data = await api("/api/sequence", "POST", {seed:caller.syntaxId, expectedRevision:pin, showAll:false, interactions:true}, () => active() && mine());
  } catch (error) {
    if (error.name === "AbortError") return cancelled();
    stop(`Open in Sequence failed: ${error.message}`);
    // A revision conflict still refreshes status, as any pinned read's does.
    if (IndexPin.isConflict(error)) throw error;
    return;
  }
  if (!active() || !mine()) return cancelled();
  if (!IndexPin.equal(data?.revision, pin) || data?.seed?.id !== caller.syntaxId) {
    stop("Open in Sequence failed: Sequence provenance mismatch. Nothing displayed."); return;
  }
  const boundary = {symbolId:caller.syntaxId, callIds:sites.map(site => site.callId)};
  try { await navigateDiagram(data.seed, pin, "sequence", data, false, boundary, saved); }
  finally { if (mine()) clearSequenceOpen(); }
}
function highlightSequenceBoundary(data, options) {
  const notice = $("sequence-boundary-status"), focus = sequenceBoundary;
  notice.replaceChildren(); notice.hidden = !focus || focus.symbolId !== data.seed.id;
  if (notice.hidden || !options.isCurrent()) return;
  const ids = new Set(focus.callIds), diagram = $("sequence-diagram"), highlighted = new Set();
  let first = null;
  for (const node of diagram.querySelectorAll("[data-trellis-call-id]")) {
    const id = node.getAttribute("data-trellis-call-id");
    if (!ids.has(id)) continue;
    node.setAttribute("data-boundary-highlight", "true"); highlighted.add(id); first ||= node;
  }
  const missing = ids.size - highlighted.size;
  let sameColumn = false;
  const findInternal = step => {
    if (ids.has(step.callId)) {
      const target = window.TrellisSequence.target(data, step);
      sameColumn ||= !!target && window.TrellisSequence.column(target).id === window.TrellisSequence.stepColumn(data, step).id;
    }
    for (const child of [...(step.children || []), ...(step.alternate || [])]) findInternal(child);
  };
  for (const step of data.steps) findInternal(step);
  const reason = options.showDetails ? " in this admitted sequence." : sameColumn
    ? " in Interactions; calls within one participant are hidden." : " in Interactions; internal or folded steps may be hidden.";
  notice.append(element("span", `${highlighted.size} boundary call${highlighted.size === 1 ? "" : "s"} highlighted.${missing ? ` ${missing} boundary call${missing === 1 ? " is" : "s are"} not shown${reason}` : ""}`));
  if (missing && !options.showDetails) {
    const show = element("button", "Show all steps"); show.type = "button";
    show.addEventListener("click", () => { if (options.isCurrent()) { $("all-steps").checked = true; void loadSequence(); } }); notice.append(show);
  }
  if (focus.callIds.length > SEQUENCE_LINK_CALL_IDS) notice.append(element("span", `Links keep at most ${SEQUENCE_LINK_CALL_IDS} highlighted calls, so reloading this link shows the sequence without highlights.`));
  if (first) first.scrollIntoView({block:"nearest", inline:"nearest"});
}
async function loadSequence(admitted = null, preserveSource = false, callerCurrent = () => true) {
  viewerMetrics?.clearInspection();
  window.TrellisShell?.resetInspector();
  if ($("sequence-warning-count")) $("sequence-warning-count").textContent = "0";
  if (!selectedMethod || !status) return;
  const serial = ++diagramSerial, guard = browseGuard(), symbol = selectedMethod, revision = IndexPin.copy(status.revision);
  const surface = sectionSerial;
  let published = false;
  const current = () => (published || callerCurrent()) && guard() && section === "diagrams" && surface === sectionSerial && serial === diagramSerial && selectedMethod === symbol;
  if (!preserveSource) clearSource();
  if (sequenceFitted !== symbol.id) { sequenceFitted = null; $("sequence-diagram").removeAttribute?.("data-fit"); }
  $("sequence-diagram").replaceChildren(); $("sequence-warnings").replaceChildren();
  $("sequence-boundary-status").hidden = true;
  if ($("sequence-incidental")) $("sequence-incidental").hidden = true;
  $("sequence-state").textContent = `Loading ${symbol.name}…`;
  await browseRequest(async () => {
    // Interactions (#663) ask for their candidates before the visible step bound; Show all does not.
    const data = admitted || await api("/api/sequence", "POST", {seed:symbol.id, expectedRevision:revision, showAll:!!$("all-steps").checked, interactions:!$("all-steps").checked}, current);
    if (!current()) return;
    if (!IndexPin.equal(data.revision, revision)) { unexpectedPair(undefined, data.revision); throw new Error("Sequence provenance mismatch. Nothing displayed."); }
    if (data.seed.id !== symbol.id) throw new Error("Sequence provenance mismatch. Nothing displayed.");
    diagramSequence = data;
    const readSource = step => { if (current()) return perform(() => showSource(step, revision)); };
    // Interactions are the default for an enriched view (#636); Show all steps is unchanged.
    const options = {showDetails: !!$("all-steps").checked, interactions:true, isCurrent: () => current() && IndexPin.equal(status?.revision, revision)};
    options.onRendered = () => highlightSequenceBoundary(data, options);
    options.onOpenTarget = sequenceTargetAction(data, options.isCurrent);
    // A later group redraw can fail after the initial load succeeded. Say so in place of
    // the diagram (#1007) rather than leaving an inert, silently folded group.
    options.onUnsupported = reason => {
      if (!options.isCurrent()) return;
      window.TrellisSequence.renderError($("sequence-diagram"), reason);
      $("sequence-state").textContent = $("sequence-state").textContent.replace(/ · Pele rendered$/, ` · This Sequence could not be drawn: ${reason}`);
    };
    if (window.TrellisShell) options.onSelect = step => {
      if (!current()) return;
      sourceInspector = {symbol:step, sequence:data}; viewerMetrics?.clearInspection(); window.TrellisShell?.selectStep(step, data, () => readSource(step), options.onOpenTarget);
      const entry = window.TrellisSequence?.entry(step) || step, evidence = entry.callEvidence, pkg = evidence?.externalPackage;
      const open = evidence?.boundaryReason === "external" && pkg && libraryButton({ecosystem:pkg.manager, name:pkg.name, version:pkg.version, origin:`external call ${entry.label}`});
      if (open) $("inspector-detail")?.prepend(open);
    };
    const diagram = $("sequence-diagram");
    // Chains holding an HTTP interface hop open expanded (#853).
    const expanded = window.TrellisSequence.hopGroups(data);
    if (sequenceBoundary?.symbolId === symbol.id) {
      const ids = new Set(sequenceBoundary.callIds);
      const reveal = step => {
        let matched = ids.has(step.callId);
        for (const child of [...(step.children || []), ...(step.alternate || [])]) matched = reveal(child) || matched;
        if (matched && step.kind === "group") expanded.add(step.id);
        return matched;
      };
      for (const step of data.steps) reveal(step);
    }
    // No calls is not no behavior. Keep any unknown/control markers beside this guidance.
    const empty = window.TrellisSequence.emptySummary(data, options);
    const onlyEmpty = empty && !window.TrellisSequence.display(data, options).visible.size;
    const outcome = onlyEmpty ? {status:"empty"} : window.TrellisPeleSequence
      ? await window.TrellisPeleSequence.render(diagram, data, readSource, expanded, options)
      : {status:"unsupported", reason:"Pele asset unavailable"};
    if (!current() || !IndexPin.equal(status?.revision, revision)) return;
    if (outcome.status === "stale") return;
    if (outcome.status !== "rendered" && outcome.status !== "empty") {
      window.TrellisSequence.renderError(diagram, outcome.reason);
    }
    if (empty) window.TrellisSequence.renderEmpty(diagram, empty, () => { if (options.isCurrent()) { $("all-steps").checked = true; void loadSequence(); } }, !onlyEmpty);
    if ($("sequence-warning-count")) $("sequence-warning-count").textContent = String((data.warnings || []).length);
    renderIncidentalToggle(data, options);
    // Interactions (#636) draw no internal step; the one count of them is in the status line.
    const interactions = window.TrellisSequence.display(data, options);
    $("sequence-state").textContent = `${symbol.name} · ${symbol.path} · revision ${IndexPin.label(revision)} · ${window.TrellisSequence.evidenceSummary(data)} · ${data.hiddenSteps} incidental steps hidden${interactions.interactions ? ` · Interactions · ${interactions.hiddenSteps} internal steps hidden` : ""}${interactions.omittedInteractions ? ` · truncated: ${interactions.omittedInteractions} later ${interactions.omittedInteractions === 1 ? "interaction" : "interactions"} not shown` : ""}${data.truncated ? " · truncated by sequence limits" : ""}${window.TrellisSequence.interfaceSummary(data) ? ` · ${window.TrellisSequence.interfaceSummary(data)}` : ""}${!data.steps.length ? " · No visible behavior steps. Read method source for context." : ""}${outcome.status === "empty" ? "" : outcome.status !== "rendered" ? ` · This Sequence could not be drawn: ${outcome.reason}` : " · Pele rendered"}`;
    for (const warning of data.warnings || []) $("sequence-warnings").append(element("li", warning, "warning"));
    highlightSequenceBoundary(data, options);
    published = true;
  }, current, "sequence-state");
}
// Fit is offered only while the diagram is wider than the panel, or to undo itself (#558).
// Every drawing replaces the panel's children, whichever renderer drew it.
function renderSequenceFit() {
  const diagram = $("sequence-diagram"), button = $("sequence-fit");
  if (!diagram || !button) return;
  const fitted = diagram.hasAttribute("data-fit"), width = diagram.querySelector(":scope > svg")?.viewBox?.baseVal?.width || 0;
  button.hidden = !width || !fitted && width <= diagram.clientWidth;
  button.setAttribute("aria-pressed", String(fitted));
}
// A tall diagram scrolls its headers away; a copy of the column names stays over the panel
// once they have gone. It reads what the renderer drew, so it needs no renderer's help.
let sequenceHeaderEnd = 0;
function positionSequenceColumns() {
  const diagram = $("sequence-diagram"), strip = $("sequence-columns");
  if (!strip) return;
  strip.hidden = !sequenceHeaderEnd || diagram.scrollTop < sequenceHeaderEnd;
  strip.style.transform = `translateX(${-diagram.scrollLeft}px)`;
}
function renderSequenceColumns() {
  const diagram = $("sequence-diagram"), strip = $("sequence-columns");
  if (!strip || !diagram?.getBoundingClientRect) return;
  const box = diagram.getBoundingClientRect();
  const headers = [...diagram.querySelectorAll(":scope > svg .pele-actor:not(.pele-actor-footer)")];
  sequenceHeaderEnd = 0;
  strip.replaceChildren(...headers.map(header => {
    const rect = header.getBoundingClientRect();
    // A wrapped header (#716) is one line here: its kind, then its name.
    const lines = [...header.querySelectorAll("tspan")].map(line => line.textContent);
    const name = element("span", lines.length ? lines.join(" ") : header.textContent);
    name.style.left = `${rect.left + rect.width / 2 - box.left + diagram.scrollLeft}px`;
    sequenceHeaderEnd = Math.max(sequenceHeaderEnd, rect.bottom - box.top + diagram.scrollTop);
    return name;
  }));
  positionSequenceColumns();
}
function renderSequenceChrome() { renderSequenceFit(); renderSequenceColumns(); }
// With no method selected, Sequence offers the highest-cognitive named callables of the
// current metrics projection as starting points (#450): application layers only, since
// that population leaves out fixtures and hidden tooling (#524). Repository text stays text.
const SEQUENCE_STARTS = 5;
function renderSequenceStart() {
  const top = !selectedMethod && token && status ? viewerMetrics?.topCallables?.() : null;
  const rows = (top?.rows || []).filter(row => typeof row.name === "string" && row.name && typeof row.sid === "string").slice(0, SEQUENCE_STARTS);
  $("sequence-start-list")?.replaceChildren();
  if ($("sequence-start")) $("sequence-start").hidden = !rows.length;
  if (!rows.length) return;
  $("sequence-start-note").textContent = `Highest cognitive complexity · ${top.population}. Or select a method in the tree, the source margin or the inspector.`;
  for (const row of rows) {
    const item = element("li"), pick = element("button", row.name); pick.type = "button";
    pick.append(element("span", `${row.path} · ${row.kind} · cognitive ${row.metrics?.cognitive ?? "—"}`, "detail"));
    pick.addEventListener("click", () => perform(() => openCallableSequence(row, top.pin)));
    item.append(pick); $("sequence-start-list").append(item);
  }
}
// A metrics row names an exact native sid at a pin; the sequence API admits it or fails.
async function openCallableSequence(row, pin) {
  if (!token || !IndexPin.equal(pin, status?.revision)) throw new Error("Metrics snapshot changed. Refresh status.");
  const context = navigationContext(), ticket = ++navigationSerial;
  const current = () => ticket === navigationSerial && navigationCurrent(context);
  $("sequence-state").textContent = `Loading ${row.name}…`;
  const data = await api("/api/sequence", "POST", {seed:row.sid, expectedRevision:pin, showAll:false, interactions:true}, current);
  if (!current()) return;
  if (!IndexPin.equal(data?.revision, pin) || data?.seed?.id !== row.sid) throw new Error("Sequence provenance mismatch. Nothing displayed.");
  // The shared navigation owner records history and returns to Diagrams.
  await navigateDiagram(data.seed, pin, "sequence", data);
}
