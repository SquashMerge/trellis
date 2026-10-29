"use strict";
(() => {
  const finite = value => typeof value === "number" && Number.isFinite(value);
  const fixedCuts = {
    cognitive:[5, 15, 30],
    cyclomatic:[5, 10, 20],
    maxNesting:[2, 3, 4],
    sloc:[30, 60, 120],
    parameters:[3, 5, 7],
    fanOut:[7, 15, 30],
    "halstead.volume":[500, 1000, 2000],
  };
  const metricValue = (metrics, name) => name.split(".").reduce((value, key) => value?.[key], metrics);
  const quartiles = values => {
    const sorted = values.filter(finite).sort((a, b) => a - b);
    return sorted.length ? [.25, .5, .75].map(p => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))]) : [];
  };
  const relativeBand = (value, cuts, metric) => {
    if (!finite(value) || cuts.length !== 3) return null;
    const rank = cuts.filter(cut => value > cut).length;
    return metric === "maintainabilityIndex" ? 3 - rank : rank;
  };
  const ranges = cuts => [`≤${cuts[0]}`, `>${cuts[0]}–${cuts[1]}`, `>${cuts[1]}–${cuts[2]}`, `>${cuts[2]}`];
  function createScale(metric, aggregate, values, relative = false) {
    const inverse = metric === "maintainabilityIndex";
    let label, entries = [], classify = () => null;
    if (relative) {
      const cuts = quartiles(values);
      label = `Relative to visible set${inverse ? " · higher is better; heat inverted" : ""}`;
      if (cuts.length) {
        entries = ranges(cuts).map((label, rank) => ({label, band:inverse ? 3 - rank : rank}));
        classify = value => relativeBand(value, cuts, metric);
      }
    } else if (aggregate === "sum") {
      label = "Sum · relative totals; no callable risk bands";
    } else if (inverse) {
      label = "Fixed per-callable bands · higher is better; heat inverted";
      entries = ["≥40", "≥20–<40", "≥10–<20", "<10"].map((label, band) => ({label, band}));
      classify = value => [40, 20, 10].filter(cut => value < cut).length;
    } else if (Object.hasOwn(fixedCuts, metric)) {
      const cuts = fixedCuts[metric];
      label = "Fixed per-callable bands";
      entries = ranges(cuts).map((label, band) => ({label, band}));
      classify = value => cuts.filter(cut => value > cut).length;
    } else {
      label = "Informational · neutral; no fixed risk bands";
    }
    if (!values.some(finite)) return {band:() => null, label:`${label} · No measured values · — unavailable, not zero`, entries};
    return {band:value => finite(value) ? classify(value) : null, label, entries};
  }
  window.TrellisMetricHeat = {createScale, quartiles, metricValue, relativeBand};
})();
