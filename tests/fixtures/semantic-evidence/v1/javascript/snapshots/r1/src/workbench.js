import summarize, { eventKey as keyOf, serialize, café, 𐐀label } from "./text.js";
import * as text from "./text.js";
import { Pipeline, toolPalette, schedulePreview, scopedPreview } from "./pipeline.js";
import { History, historyActions, checksum, flatten, findBatch, replayEvent } from "./history.js";
import { planRetry } from "./adapters.js";
import { convert, sendBatch } from "./public.js";
import { preview, explainIngress } from "./stages.js";
import { readLegacy } from "./legacy.js";
import { EventEditor } from "./editor.js";

export function prepare(lines) {
  const pipeline = new Pipeline();
  return lines.map((line) => pipeline.run(line)).filter(Boolean);
}

export function inspectBatch(batch, key) {
  const events = flatten(batch);
  const selected = findBatch(batch, key);
  return { selected, summary: summarize(events), checksum: checksum(key) };
}

export function captions(event) {
  const normalized = café(event);
  const heading = 𐐀label(event);
  const emoji = "📨"; return text.describe(event) + emoji + heading + normalized;
}

export function runWorkbench(json, quiet, lines) {
  const history = new History();
  const events = lines.map((line) => convert(json, line)).filter(Boolean).map((result) => result.event);
  for (const event of events) {
    history.record(event);
    const retry = planRetry(!quiet, event, 0);
    if (retry.accepted) preview(true, event);
  }
  const output = sendBatch(quiet, events);
  return { output, history: history.describe(), sample: explainIngress(json, lines[0]) };
}

export function previewTools(history) {
  const palette = toolPalette();
  const actions = historyActions(history);
  return { ...palette, ...actions, prepare, inspectBatch, captions, runWorkbench,
    schedulePreview, scopedPreview, readLegacy, replayEvent };
}

export function exportSelection(events) {
  return events.map((event) => ({ key: keyOf(event.key), data: serialize(event) }));
}

export function compareLabels(event) {
  function describe(value) { return `local:${value.key}`; }
  return [describe(event), text.describe(event)];
}

export function editSelection(event, changes) {
  const editor = new EventEditor(event);
  if ("key" in changes) editor.key = changes.key;
  if ("payload" in changes) editor.payload = changes.payload;
  if ("label" in changes) editor.label = changes.label;
  if ("tags" in changes) editor.tags = changes.tags;
  return editor.commit();
}

export function watchSelection(host, events) {
  const cancel = host.watch(events);
  host.render(exportSelection(events));
  return () => { cancel(); host.clear(); };
}
