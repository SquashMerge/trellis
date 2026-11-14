"use strict";
// The index snapshot pair every pinned read carries: one validated, frozen copy and its comparisons.
// Loaded before every view and before app.js; they read it as window.TrellisIndexPin.
(() => {
  const IndexPin = Object.freeze({
    copy(value) {
      if (!value || typeof value !== "object" || Array.isArray(value) ||
          !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value.indexGeneration) ||
          !Number.isSafeInteger(value.indexRevision) || value.indexRevision < 0) throw new Error("Invalid index snapshot pair. Refresh status.");
      return Object.freeze({indexGeneration:value.indexGeneration, indexRevision:value.indexRevision});
    },
    equal(a, b) {
      return (a == null && b == null) || (!!a && !!b && typeof a.indexGeneration === "string" && typeof b.indexGeneration === "string" &&
        Number.isSafeInteger(a.indexRevision) && Number.isSafeInteger(b.indexRevision) &&
        a.indexGeneration === b.indexGeneration && a.indexRevision === b.indexRevision);
    },
    key(value) { const pin = this.copy(value); return `${pin.indexGeneration}:${pin.indexRevision}`; },
    query(value) { const pin = this.copy(value); return `indexGeneration=${encodeURIComponent(pin.indexGeneration)}&indexRevision=${pin.indexRevision}`; },
    label(value) { const pin = this.copy(value); return `${pin.indexGeneration.slice(0, 8)}:${pin.indexRevision}`; },
    isConflict(error) { return error?.status === 409 && error.code === "revision_conflict"; },
  });
  window.TrellisIndexPin = IndexPin;
})();
