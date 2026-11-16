"use strict";
// What the browser and the Herdr mod check and word the same way in a daemon answer (the mod bundles
// this file): the shape of an Architecture payload and of one page of its detail, a checkout's index
// state as words and a shape, and a failed job's cause. Pure, and it throws nothing: each host
// compares the pin first, with its own comparison, and raises its own error.
(() => {
  window.TrellisDtoCore = Object.freeze({
    // A settled Architecture payload (not `building`) for the aggregate and role that were asked for.
    architecturePayload(payload, aggregate, role) {
      return ["ready", "missing_rules", "no_layers"].includes(payload.state) && Array.isArray(payload.apps) && Array.isArray(payload.interfaces) &&
        payload.aggregate === aggregate && payload.role === role &&
        payload.workbench === true && payload.metricsVersion === "metrics:v1" &&
        ["syntax", "mixed"].includes(payload.evidenceTier) && payload.semanticComplete === false &&
        Array.isArray(payload.layers) && Array.isArray(payload.modules) && Array.isArray(payload.edges) &&
        !(["ready", "no_layers"].includes(payload.state) && (typeof payload.rulesFile?.contentHash !== "string" || !payload.rulesFile.contentHash));
    },
    // One page of Architecture detail for the rules hash, kind and offset that were asked for.
    detailPage(page, rulesHash, kind, offset) {
      return page.rulesFile?.contentHash === rulesHash && page.kind === kind &&
        page.offset === offset && Array.isArray(page.items) && Number.isSafeInteger(page.total) &&
        (page.nextOffset === null || (Number.isSafeInteger(page.nextOffset) && page.nextOffset > offset));
    },
    // A checkout row's index state. `unavailable(row)` is the host's words for a checkout that is not
    // available (and not busy); `pinLabel(pin)` is the host's short revision label, and may throw.
    checkoutIndexLabel(row, unavailable, pinLabel) {
      if (!row || row.state !== "available") return {text: row?.state === "storage_busy" ? "Busy" : unavailable(row), state:"failed", shape:"□"};
      const pin = row.revision, catchingUp = row.catchingUp;
      if (row.indexError && row.indexError !== "index_not_ready") return {text:`Index unavailable · ${row.indexError}`, state:"failed", shape:"□"};
      if (!pin) return catchingUp === true ? {text:"Catching up", state:"catching-up", shape:"◐"}
        : {text:"Not indexed", state:"unknown", shape:"◇"};
      let label;
      try { label = pinLabel(pin); } catch (_) { return {text:"Index status unavailable", state:"unknown", shape:"◇"}; }
      return catchingUp === true ? {text:`Catching up · rev ${label}`, state:"catching-up", shape:"◐"}
        : catchingUp === false ? {text:`Fresh · rev ${label}`, state:"fresh", shape:"○"}
        : {text:`Indexed · idle · rev ${label}`, state:"unknown", shape:"◇"};
    },
    // The recorded failure code and reason, never the generic job message.
    jobCause(value) {
      const code = typeof value?.error?.code === "string" && value.error.code ? value.error.code : "cause not recorded";
      const reason = typeof value?.error?.reason === "string" && value.error.reason ? `: ${value.error.reason}` : "";
      return `${code}${reason}`;
    },
  });
})();
