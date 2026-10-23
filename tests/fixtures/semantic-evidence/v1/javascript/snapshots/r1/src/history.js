import { canonicalKey, describe } from "./text.js";

// Event batches form a tree; internal nodes have children, leaves have events.
export function flatten(batch) {
  if (!batch.children) return batch.events ?? [];
  return batch.children.flatMap(flatten);
}

export function checksum(text, index = 0) {
  if (index >= text.length) return 0;
  return text.charCodeAt(index) + checksum(text, index + 1);
}

// A replay alternates event delivery and checkpoint transitions.
export function replayEvent(cursor) {
  if (!cursor) return [];
  return [describe(cursor.event), ...replayCheckpoint(cursor.next)];
}

export function replayCheckpoint(cursor) {
  if (!cursor) return [];
  return cursor.checkpoint ? replayEvent(cursor.next) : replayCheckpoint(cursor.next);
}

export function findBatch(root, key) {
  function visit(batch) {
    if (canonicalKey(batch.key) === key) return batch;
    for (const child of batch.children ?? []) {
      const found = visit(child);
      if (found) return found;
    }
    return undefined;
  }
  return visit(root);
}

export class History {
  constructor(parent = null) {
    this.parent = parent;
    this.entries = new Map();
  }
  record(event) {
    this.entries.set(canonicalKey(event.key), event);
  }
  lookup(key) {
    const local = this.entries.get(canonicalKey(key));
    return local ?? this.parent?.lookup(key);
  }
  describe() {
    return Array.from(this.entries.values()).map(describe).join("; ");
  }
}

export function historyActions(history) {
  return { flatten, checksum, replayEvent, replayCheckpoint, findBatch,
    record: history.record, lookup: history.lookup, describe: history.describe };
}
