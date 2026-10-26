// Shared event normalization. This module has no host integration.
export function clean(value) {
  return String(value ?? "").trim();
}

export function canonicalKey(value) {
  return clean(value).toLowerCase().replaceAll(" ", "-");
}

export function parseLine(line) {
  const event = JSON.parse(clean(line));
  return { ...event, key: canonicalKey(event.key), tags: event.tags ?? [] };
}

export function hasPayload(event) {
  return clean(event.payload).length > 0;
}

export function stamp(event) {
  return { ...event, observedAt: Date.now() };
}

export function redact(event) {
  const { secret, ...publicEvent } = event;
  return publicEvent;
}

export function formatLine(event) {
  return JSON.stringify(event);
}

export function describe(event) {
  return `${canonicalKey(event.key)}:${clean(event.payload)}`;
}

export function café(event) {
  return clean(event.label ?? event.key).normalize("NFC");
}

export function 𐐀label(event) {
  return café(event).toUpperCase();
}

export default function summarize(events) {
  return events.map(describe).join("\n");
}

export { canonicalKey as eventKey, formatLine as serialize };
