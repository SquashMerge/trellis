import { parseLine, hasPayload, stamp, redact, formatLine } from "./text.js";

export class Stage {
  accepts(value) { return value !== undefined; }
  transform(value) { return value; }
  describe() { return "identity"; }
  run(value) {
    return this.accepts(value) ? this.transform(value) : undefined;
  }
}

export class ParseStage extends Stage {
  accepts(value) { return typeof value === "string" && value.length > 0; }
  transform(value) { return parseLine(value); }
  describe() { return "parse incoming JSON lines"; }
}

export class FilterStage extends Stage {
  accepts(value) { return value !== null && typeof value === "object"; }
  transform(value) { return hasPayload(value) ? value : undefined; }
  describe() { return "remove empty events"; }
}

export class StampStage extends Stage {
  accepts(value) { return value !== null && typeof value === "object"; }
  transform(value) { return stamp(value); }
  describe() { return "record ingestion time"; }
}

export class RedactStage extends Stage {
  accepts(value) { return value !== null && typeof value === "object"; }
  transform(value) { return redact(value); }
  describe() { return "remove private fields"; }
}

export class FormatStage extends Stage {
  accepts(value) { return value !== undefined; }
  transform(value) { return formatLine(value); }
  describe() { return "encode outgoing JSON lines"; }
}

export function defaultStages() {
  return [new ParseStage(), new FilterStage(), new StampStage(), new RedactStage(), new FormatStage()];
}

// A replay job can receive either already-redacted objects or serialized lines.
export function preview(redactFirst, event) {
  const stage = redactFirst ? new RedactStage() : new FormatStage();
  if (!stage.accepts(event)) return undefined;
  const result = stage.transform(event);
  return { label: stage.describe(), result };
}

export function explainIngress(parse, value) {
  const stage = parse ? new ParseStage() : new FilterStage();
  return { accepted: stage.accepts(value), explanation: stage.describe() };
}
