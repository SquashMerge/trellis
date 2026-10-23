import { clean, parseLine, formatLine, describe } from "./text.js";

export class JsonCodec {
  decode(line) { return parseLine(line); }
  encode(event) { return formatLine(event); }
  label() { return "json"; }
  accepts(line) { return clean(line).startsWith("{"); }
  extension() { return ".jsonl"; }
}

export class TextCodec {
  decode(line) { return { key: "message", payload: clean(line) }; }
  encode(event) { return describe(event); }
  label() { return "text"; }
  accepts(line) { return clean(line).length > 0; }
  extension() { return ".txt"; }
}

export class MemorySink {
  constructor() { this.events = []; }
  open() { this.events.length = 0; }
  write(event) { this.events.push(event); }
  flush() { return this.events.slice(); }
  close() { return this.events.length; }
  describe() { return "memory preview"; }
}

export class ConsoleSink {
  open() { console.info("begin stream"); }
  write(event) { console.log(formatLine(event)); }
  flush() { console.info("stream checkpoint"); return []; }
  close() { console.info("end stream"); return 0; }
  describe() { return "console output"; }
}

export class ImmediatePolicy {
  accepts(event) { return event !== undefined; }
  delay(attempt) { return 0; }
  limit() { return 1; }
  key(event) { return clean(event.key); }
  describe() { return "single attempt"; }
}

export class RetryPolicy {
  accepts(event) { return event !== undefined && !event.permanent; }
  delay(attempt) { return Math.min(1000, 25 * 2 ** attempt); }
  limit() { return 3; }
  key(event) { return clean(event.key) + ":retry"; }
  describe() { return "bounded retry"; }
}

// Each operation has two concrete candidate declarations, not an invented overload.
export function roundTrip(json, input) {
  const codec = json ? new JsonCodec() : new TextCodec();
  if (!codec.accepts(input)) return undefined;
  const event = codec.decode(input);
  return { event, body: codec.encode(event), format: codec.label(), suffix: codec.extension() };
}

export function deliver(quiet, events) {
  const sink = quiet ? new MemorySink() : new ConsoleSink();
  sink.open();
  for (const event of events) sink.write(event);
  const checkpoint = sink.flush();
  const count = sink.close();
  return { checkpoint, count, destination: sink.describe() };
}

export function planRetry(retry, event, attempt) {
  const policy = retry ? new RetryPolicy() : new ImmediatePolicy();
  return {
    accepted: policy.accepts(event),
    wait: policy.delay(attempt),
    attempts: policy.limit(),
    key: policy.key(event),
    description: policy.describe(),
  };
}
