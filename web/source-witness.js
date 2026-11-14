// One definition of "this item names a measured source range": a non-empty path and a
// well-formed line, column and byte range. Loaded before every script that reads it, as
// window.TrellisSourceWitness; the Herdr mod bundles it with the Sequence scripts.
(() => {
  "use strict";
  function sourceWitness(item) {
    const r = item?.range;
    return typeof item?.path === "string" && !!item.path.trim() && !!r &&
      Number.isSafeInteger(r.startLine) && r.startLine > 0 &&
      Number.isSafeInteger(r.endLine) && r.endLine >= r.startLine &&
      (r.startColumn == null || (Number.isSafeInteger(r.startColumn) && r.startColumn > 0)) &&
      (r.endColumn == null || (Number.isSafeInteger(r.endColumn) && r.endColumn > 0)) &&
      (r.startLine !== r.endLine || r.startColumn == null || r.endColumn == null || r.endColumn >= r.startColumn) &&
      (r.startByte == null || (Number.isSafeInteger(r.startByte) && r.startByte >= 0)) &&
      (r.endByte == null || (Number.isSafeInteger(r.endByte) && r.endByte > (r.startByte ?? -1)));
  }
  window.TrellisSourceWitness = sourceWitness;
})();
