// The daemon client: the same routes the browser uses, on the machine the shown space lives on.
//
// Same machine: `fetch` to the loopback URL, with the bearer token read from the serve token
// file (default ~/.config/trellis/serve.token) into this closure only.
// Another machine (a space on a Herdr machine), or `transport: "exec"`: one
// `exec("trellis", ["api", …], { on: machine })` per request (#817). The CLI reaches that
// machine's daemon over its private socket, as every Trellis command does, so no token is read
// by the mod or a shell there. It serves only the routes this client uses, with the browser's
// bounds, and prints the daemon's JSON. Responses are bounded by exec's 8 MiB and timed out.
// The token is never logged, put in state, written anywhere, or included in an error message.
import { exec, home, readTextFile } from "herdr";

const LOOPBACK = /^http:\/\/(?:127\.0\.0\.1|localhost|\[::1\]):([0-9]{1,5})$/;
const TOKEN = /^[A-Za-z0-9_-]{1,512}$/;
// `trellis api` gives up on a silent daemon after 25 s; exec's own backstop is a little later.
const EXEC_TIMEOUT_MS = 30000;
export const DEFAULT_URL = "http://127.0.0.1:8877";
export const validRootKey = (key) => typeof key === "string" && /^[0-9a-f]{64}$/.test(key);

/** A typed failure: `code` names it (the daemon's own code where it gave one). */
export class TrellisError extends Error {
  constructor(code, message, status = null) {
    super(message);
    this.code = code;
    this.status = status;
  }
}

const tokenMessages = (file) => ({
  token_missing: `No daemon token at ${file}. Start \`trellis serve --token-file ${file}\`.`,
  token_unreadable: `Cannot read the daemon token file ${file}.`,
  token_invalid: `The daemon token file ${file} does not hold a token.`,
});

/**
 * `machine`: a Herdr machine name (a space's `machine`), or null for this computer.
 * `transport`: "fetch" (default here) or "exec" (`trellis api`; always used for another machine).
 */
export function createClient({ url = DEFAULT_URL, tokenFile, machine = null, transport } = {}) {
  const base = String(url).replace(/\/+$/, "");
  const port = Number(LOOPBACK.exec(base)?.[1]);
  if (!(port > 0 && port < 65536)) throw new TrellisError("invalid_url", `Not a loopback daemon address: ${base}. Use http://127.0.0.1:<port>.`);
  const viaExec = machine !== null || transport === "exec";
  const file = tokenFile || `${home()}/.config/trellis/serve.token`;
  const where = machine ? `${base} on ${machine}` : base;
  let token = null;

  async function loadToken() {
    let text;
    try { text = await readTextFile(file); }
    catch { throw new TrellisError("token_unreadable", tokenMessages(file).token_unreadable); }
    if (text === null) throw new TrellisError("token_missing", tokenMessages(file).token_missing);
    const value = text.trim();
    if (!TOKEN.test(value)) throw new TrellisError("token_invalid", tokenMessages(file).token_invalid);
    token = value;
  }

  // The parsed answer, or the daemon's typed error.
  function answer(status, data) {
    if (status >= 200 && status < 300) return data;
    const code = typeof data?.error?.code === "string" ? data.error.code : `http_${status}`;
    const message = typeof data?.error?.message === "string" ? data.error.message : `Request failed (${status}).`;
    throw new TrellisError(code, message, status);
  }

  async function viaFetch(path, body) {
    if (!token) await loadToken();
    let response, data;
    try {
      response = await fetch(`${base}${path}`, body === undefined ? { headers: { Authorization: `Bearer ${token}` } }
        : { method: "POST", headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" }, body: JSON.stringify(body) });
    } catch {
      throw new TrellisError("daemon_unreachable", `No Trellis daemon answers at ${where}. Start \`trellis serve\`.`);
    }
    // A rejected token is re-read next time: \`serve\` may have been restarted with another file.
    if (response.status === 401 || response.status === 403) token = null;
    try { data = response.status === 204 ? null : await response.json(); }
    catch { throw new TrellisError("invalid_response", `The daemon sent an unreadable answer (${response.status}).`, response.status); }
    return answer(response.status, data);
  }

  // `trellis api` on the daemon's machine. Exit 0: the daemon's 2xx JSON. Exit 1: the daemon's
  // own `{error}` answer. Exit 2: a typed failure with no answer (refused route, daemon down).
  async function viaCli(path, body) {
    const args = body === undefined ? ["api", "GET", path] : ["api", "POST", path, "--body", JSON.stringify(body)];
    const on = machine ?? "this computer";
    let result;
    try { result = await exec("trellis", args, { on: machine, timeout: EXEC_TIMEOUT_MS }); }
    catch (error) {
      if (/time(d)? ?out/i.test(String(error?.message ?? error))) throw new TrellisError("daemon_timeout", `trellis api on ${on} did not answer within ${EXEC_TIMEOUT_MS / 1000} s.`);
      throw new TrellisError("machine_unreachable", `Cannot run trellis on ${on}: the machine is unreachable or trellis isn't on its PATH.`);
    }
    if (result.code === 127) throw new TrellisError("trellis_missing", `trellis isn't on the PATH on ${on}.`);
    let data;
    try { data = JSON.parse(result.stdout); }
    catch {
      if (/unrecognized subcommand/.test(result.stderr || "")) throw new TrellisError("trellis_outdated", `The trellis on ${on} has no \`api\` command. Install a newer Trellis there.`);
      throw new TrellisError("invalid_response", `trellis api on ${on} printed no JSON (exit ${result.code}).`);
    }
    if (result.code === 0) return data;
    const code = typeof data?.error?.code === "string" ? data.error.code : `exit_${result.code}`;
    const message = typeof data?.error?.message === "string" ? data.error.message : `trellis api failed (exit ${result.code}).`;
    throw new TrellisError(code, message);
  }

  const request = viaExec ? viaCli : viaFetch;
  const checkout = (rootKey, suffix, body) => {
    if (!validRootKey(rootKey)) throw new TrellisError("invalid_checkout", "Choose an available checkout.");
    return request(`/api/checkouts/${rootKey}/${suffix}`, body);
  };
  const pinQuery = (pin) => `indexGeneration=${encodeURIComponent(pin.indexGeneration)}&indexRevision=${pin.indexRevision}`;
  return {
    base, machine, transport: viaExec ? "exec" : "fetch",
    checkouts: () => request("/api/checkouts"),
    status: (rootKey) => checkout(rootKey, "status"),
    architecture: (rootKey, pin) => checkout(rootKey, `architecture?aggregate=max&role=production&workbench=true&${pinQuery(pin)}`),
    methods: (rootKey, pin, path) => checkout(rootKey, `methods?${pinQuery(pin)}&path=${encodeURIComponent(path)}`),
    sequence: (rootKey, pin, seed) => checkout(rootKey, "sequence", { seed, expectedRevision: pin, showAll: false, interactions: true }),
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
