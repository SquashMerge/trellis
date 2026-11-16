// What the pane's files share: its intervals and bounds, and a few small helpers.
import { TrellisError } from "./client.js";

// Polls are gentler for another machine: each one is a command run there.
export const STATUS_MS = 5000, REMOTE_STATUS_MS = 10000, LIST_MS = 30000, BUILDING_MS = 250, MAX_BUILDING_MS = 5000;
export const MAX_STEP_ROWS = 300, MAX_HISTORY = 20;
// A drawing yields to the host between phases (the runtime ends a task whose one synchronous
// run is too long, without an error), and one that hasn't finished by then is a typed error.
export const DRAW_TIMEOUT_MS = 15000;
// Re-lay out only for a width change worth a new drawing (the browser's RESIZE_SLACK).
export const WIDTH_SLACK = 8, PAD = 12;
export const SELECTED_FILE = "trellis.selectedFile";
export const HEAT_NAMES = ["low", "moderate", "high", "very high"];
// The top tabs, in order (#939: Deviations is its own tab).
export const TABS = ["architecture", "sequence", "deviations"];
// The Deviations tree's read (web/architecture-deviations.js PARAMS), and its page bound.
export const DEVIATION_PARAMS = { kind: "instances", rule: "$layers", status: "deviates", role: "production", excepted: "false", limit: "500" };
export const MAX_DEVIATION_PAGES = 40;
// The brand mark the web app's header shows, copied beside main.js by build.mjs.
export const LOGO = "mods/trellis/trellis-mark.png";

export const yieldTo = (cx) => () => cx.sleep(0);
export const drawTimeout = (what) => new TrellisError("draw_timeout", `The ${what} drawing didn't finish within ${DRAW_TIMEOUT_MS / 1000} s, so the Herdr runtime may have ended it. It is tried again when the data, width or theme changes.`);
export const fail = (error) => (error instanceof TrellisError ? error : new TrellisError(error?.code || "unexpected", error?.message || String(error)));
export const basename = (path) => path.replace(/\/+$/, "").split("/").pop() || path;
// FNV-1a: a short name for one drawing's file.
export function hash(text) {
  let h = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) { h ^= text.charCodeAt(i); h = Math.imul(h, 0x01000193) >>> 0; }
  return h.toString(16).padStart(8, "0");
}

export function settingsMono() { return "JetBrains Mono"; }
