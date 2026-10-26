import { clean, canonicalKey } from "./text.js";

// Editing keeps display fields readable while normalizing committed values.
export class EventEditor {
  constructor(event) {
    this.event = { ...event };
  }
  get key() { return this.event.key; }
  set key(value) { this.event.key = canonicalKey(value); }
  get payload() { return this.event.payload; }
  set payload(value) { this.event.payload = clean(value); }
  get label() { return this.event.label ?? this.event.key; }
  set label(value) { this.event.label = clean(value).normalize("NFC"); }
  get tags() { return [...(this.event.tags ?? [])]; }
  set tags(values) { this.event.tags = [...new Set(values.map(canonicalKey))]; }
  commit() { return { ...this.event, tags: this.tags }; }
}
