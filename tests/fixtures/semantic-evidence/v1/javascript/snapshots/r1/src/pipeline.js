import { defaultStages } from "./stages.js";
import { clean, canonicalKey, parseLine, hasPayload, stamp, redact, formatLine, describe, café, 𐐀label } from "./text.js";

// A palette stores callable values. Opening a palette must not execute its tools.
export function toolPalette() {
  return {
    clean, canonicalKey, parseLine, hasPayload, stamp, redact, formatLine, describe, café, 𐐀label,
  };
}

export class Pipeline {
  constructor(stages = defaultStages()) {
    this.stages = stages;
    this.processed = 0;
  }

  run(input) {
    let value = input;
    for (const stage of this.stages) {
      if (value === undefined) break;
      value = stage.run(value);
    }
    this.processed += 1;
    return value;
  }

  inspect() {
    return this.stages.map((stage) => stage.describe());
  }

  consume(lines, observer) {
    const output = [];
    for (const line of lines) {
      try {
        const event = this.run(line);
        if (event !== undefined) {
          output.push(event);
          observer.accepted(event);
        } else {
          observer.skipped(line);
        }
      } catch (error) {
        observer.failed(error, line);
      }
    }
    observer.finished(this.processed);
    return output;
  }
}

export function schedulePreview(lines, publish) {
  const pipeline = new Pipeline();
  return Promise.resolve(lines).then((batch) => {
    const events = batch.map((line) => pipeline.run(line));
    publish(events);
    return events;
  });
}

export function scopedPreview(line) {
  function clean(value) { return value.replace(/^preview:/, ""); }
  return parseLine(clean(line));
}
