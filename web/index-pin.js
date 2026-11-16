"use strict";
// The index snapshot pair every pinned read carries.
// TrellisIndexPinCore is what the browser and the Herdr mod share (the mod bundles this file): the
// pair's form, its comparison and its two spellings. It throws nothing and validates only in `read`;
// each host decides what else it rejects, what it throws and whether the copy is frozen.
// TrellisIndexPin is the browser's wrapper: one validated, frozen copy and its comparisons. It loads
// before every view and before app.js; they read it as window.TrellisIndexPin.
(() => {
  const Core = Object.freeze({
    // A new pair when both fields are well formed, else null.
    read(value) {
      if (!value || typeof value !== "object" ||
          !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value.indexGeneration) ||
          !Number.isSafeInteger(value.indexRevision) || value.indexRevision < 0) return null;
      return {indexGeneration:value.indexGeneration, indexRevision:value.indexRevision};
    },
    // Two pairs' fields are the same. The host decides what null and a malformed pair equal.
    same(a, b) { return a.indexGeneration === b.indexGeneration && a.indexRevision === b.indexRevision; },
    query(pin) { return `indexGeneration=${encodeURIComponent(pin.indexGeneration)}&indexRevision=${pin.indexRevision}`; },
    label(pin) { return `${pin.indexGeneration.slice(0, 8)}:${pin.indexRevision}`; },
  });
  const IndexPin = Object.freeze({
    copy(value) {
      const pin = Array.isArray(value) ? null : Core.read(value);
      if (!pin) throw new Error("Invalid index snapshot pair. Refresh status.");
      return Object.freeze(pin);
    },
    equal(a, b) {
      return (a == null && b == null) || (!!a && !!b && typeof a.indexGeneration === "string" && typeof b.indexGeneration === "string" &&
        Number.isSafeInteger(a.indexRevision) && Number.isSafeInteger(b.indexRevision) && Core.same(a, b));
    },
    key(value) { const pin = this.copy(value); return `${pin.indexGeneration}:${pin.indexRevision}`; },
    query(value) { return Core.query(this.copy(value)); },
    label(value) { return Core.label(this.copy(value)); },
    isConflict(error) { return error?.status === 409 && error.code === "revision_conflict"; },
  });
  window.TrellisIndexPinCore = Core;
  window.TrellisIndexPin = IndexPin;
})();
