// The loopback daemon client: the same routes and bearer token the browser uses.
//
// The token is read from the serve token file (default ~/.config/trellis/serve.token, the file
// `trellis serve --token-file` was given) into this closure only. It is never logged, put in
// state, written anywhere, or included in an error message. Only loopback URLs are accepted.
import { home, readTextFile } from "herdr";

const LOOPBACK = /^http:\/\/(?:127\.0\.0\.1|localhost|\[::1\]):([0-9]{1,5})$/;
const TOKEN = /^[A-Za-z0-9_-]{1,512}$/;
export const DEFAULT_URL = "http://127.0.0.1:8877";
export const defaultTokenFile = () => `${home()}/.config/trellis/serve.token`;
export const validRootKey = (key) => typeof key === "string" && /^[0-9a-f]{64}$/.test(key);

/** A typed failure: `code` names it (the daemon's own code where it gave one). */
export class TrellisError extends Error {
  constructor(code, message, status = null) {
    super(message);
    this.code = code;
    this.status = status;
  }
}

export function createClient({ url = DEFAULT_URL, tokenFile = defaultTokenFile() } = {}) {
  const base = String(url).replace(/\/+$/, "");
  const port = Number(LOOPBACK.exec(base)?.[1]);
  if (!(port > 0 && port < 65536)) throw new TrellisError("invalid_url", `Not a loopback daemon address: ${base}. Use http://127.0.0.1:<port>.`);
  let token = null;

  async function loadToken() {
    let text;
    try { text = await readTextFile(tokenFile); }
    catch { throw new TrellisError("token_unreadable", `Cannot read the daemon token file ${tokenFile}.`); }
    if (text === null) throw new TrellisError("token_missing", `No daemon token at ${tokenFile}. Start \`trellis serve --token-file ${tokenFile}\`.`);
    const value = text.trim();
    if (!TOKEN.test(value)) throw new TrellisError("token_invalid", `The daemon token file ${tokenFile} does not hold a token.`);
    token = value;
  }

  async function request(path) {
    if (!token) await loadToken();
    let response, data;
    try {
      response = await fetch(`${base}${path}`, { headers: { Authorization: `Bearer ${token}` } });
    } catch {
      throw new TrellisError("daemon_unreachable", `No Trellis daemon answers at ${base}. Start \`trellis serve\`.`);
    }
    try { data = response.status === 204 ? null : await response.json(); }
    catch { throw new TrellisError("invalid_response", `The daemon sent an unreadable answer (${response.status}).`, response.status); }
    if (!response.ok) {
      // A rejected token is re-read next time: `serve` may have been restarted with another file.
      if (response.status === 401 || response.status === 403) token = null;
      const code = typeof data?.error?.code === "string" ? data.error.code : `http_${response.status}`;
      const message = typeof data?.error?.message === "string" ? data.error.message : `Request failed (${response.status}).`;
      throw new TrellisError(code, message, response.status);
    }
    return data;
  }

  const checkout = (rootKey, suffix) => {
    if (!validRootKey(rootKey)) throw new TrellisError("invalid_checkout", "Choose an available checkout.");
    return request(`/api/checkouts/${rootKey}/${suffix}`);
  };
  return {
    base,
    checkouts: () => request("/api/checkouts"),
    status: (rootKey) => checkout(rootKey, "status"),
    architecture: (rootKey, pin) => checkout(rootKey, `architecture?${new URLSearchParams({
      aggregate: "max", role: "production", workbench: "true", indexGeneration: pin.indexGeneration, indexRevision: String(pin.indexRevision),
    })}`),
  };
}

/** web/app.js IndexPin.copy: a validated index snapshot pair, or a typed error. */
export function pinOf(value) {
  if (!value || typeof value !== "object" ||
      !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value.indexGeneration) ||
      !Number.isSafeInteger(value.indexRevision) || value.indexRevision < 0) throw new TrellisError("invalid_revision", "Invalid index snapshot pair.");
  return { indexGeneration: value.indexGeneration, indexRevision: value.indexRevision };
}
export const samePin = (a, b) => (!a && !b) || (!!a && !!b && a.indexGeneration === b.indexGeneration && a.indexRevision === b.indexRevision);
export const pinLabel = (pin) => `${pin.indexGeneration.slice(0, 8)}:${pin.indexRevision}`;

/** web/app.js payloadOf: the Architecture states the browser accepts, or a typed error. */
export function architecturePayload(data, pin) {
  const payload = data?.payload;
  if (!payload || !samePin({ indexGeneration: payload.indexGeneration, indexRevision: payload.indexRevision }, pin)) {
    throw new TrellisError("revision_conflict", "Architecture snapshot changed. Refreshing.");
  }
  if (payload.state === "building") return payload;
  if (!["ready", "missing_rules", "no_layers"].includes(payload.state) || payload.aggregate !== "max" || payload.role !== "production" ||
      payload.workbench !== true || payload.metricsVersion !== "metrics:v1" ||
      !["syntax", "mixed"].includes(payload.evidenceTier) || payload.semanticComplete !== false ||
      !Array.isArray(payload.layers) || !Array.isArray(payload.modules) || !Array.isArray(payload.edges) ||
      (["ready", "no_layers"].includes(payload.state) && (typeof payload.rulesFile?.contentHash !== "string" || !payload.rulesFile.contentHash))) {
    throw new TrellisError("invalid_response", "Invalid Architecture response. Nothing displayed.");
  }
  return payload;
}

/** web/app.js checkoutIndexLabel: a checkout's index state as words and a shape. */
export function indexLabel(row) {
  if (!row || row.state !== "available") return { text: row?.state === "storage_busy" ? "Busy" : `Unavailable${row?.indexError ? ` · ${row.indexError}` : ""}`, state: "failed", shape: "□" };
  if (row.indexError && row.indexError !== "index_not_ready") return { text: `Index unavailable · ${row.indexError}`, state: "failed", shape: "□" };
  const pin = row.revision;
  if (!pin) return row.catchingUp === true ? { text: "Catching up", state: "catching-up", shape: "◐" } : { text: "Not indexed", state: "unknown", shape: "◇" };
  let label;
  try { label = pinLabel(pinOf(pin)); } catch { return { text: "Index status unavailable", state: "unknown", shape: "◇" }; }
  return row.catchingUp === true ? { text: `Catching up · rev ${label}`, state: "catching-up", shape: "◐" }
    : row.catchingUp === false ? { text: `Fresh · rev ${label}`, state: "fresh", shape: "○" }
    : { text: `Indexed · idle · rev ${label}`, state: "unknown", shape: "◇" };
}
