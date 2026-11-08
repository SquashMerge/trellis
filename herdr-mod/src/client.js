// The daemon client: the same routes and bearer token the browser uses, on the machine the
// shown space lives on.
//
// Same machine: `fetch` to the loopback URL, with the token read from the serve token file
// (default ~/.config/trellis/serve.token) into this closure only.
// Another machine (a space on a Herdr machine): one `exec("sh", …, { on: machine })` per
// request runs curl *there*, against 127.0.0.1:<port>. The token file is read by that shell and
// handed to curl on stdin (`-H @-`), so the token never reaches this app, its logs or curl's
// argv; responses are bounded (curl --max-filesize, exec's 8 MiB) and timed out (--max-time).
// The token is never logged, put in state, written anywhere, or included in an error message.
import { exec, home, readTextFile } from "herdr";

const LOOPBACK = /^http:\/\/(?:127\.0\.0\.1|localhost|\[::1\]):([0-9]{1,5})$/;
const TOKEN = /^[A-Za-z0-9_-]{1,512}$/;
const MAX_SECONDS = 25, MAX_BYTES = 8000000;
export const DEFAULT_URL = "http://127.0.0.1:8877";
const DEFAULT_TOKEN_FILE = "~/.config/trellis/serve.token";
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

// Runs on the daemon's machine: $1 token file, $2 port, $3 path, $4 JSON body (POST) or "".
// Prints the body, then a status line; or one error line. The token goes to curl on stdin.
const REMOTE = `
tf=$1 port=$2 path=$3 body=$4
case $tf in "~/"*) tf=$HOME/\${tf#"~/"};; esac
[ -e "$tf" ] || { echo "TRELLIS-ERROR token_missing"; exit 0; }
[ -r "$tf" ] || { echo "TRELLIS-ERROR token_unreadable"; exit 0; }
tok=$(head -c 513 "$tf" | tr -d ' \t\r\n')
case $tok in ""|*[!A-Za-z0-9_-]*) echo "TRELLIS-ERROR token_invalid"; exit 0;; esac
command -v curl >/dev/null 2>&1 || { echo "TRELLIS-ERROR curl_missing"; exit 0; }
if [ -n "$body" ]; then set -- -X POST -H "Content-Type: application/json" --data-binary "$body"; else set --; fi
printf 'Authorization: Bearer %s\n' "$tok" | curl -sS --max-time ${MAX_SECONDS} --max-filesize ${MAX_BYTES} -H @- "$@" -w '\nTRELLIS-STATUS %{http_code}\n' "http://127.0.0.1:$port$path" 2>/dev/null
rc=$?
[ $rc -eq 0 ] || echo "TRELLIS-CURL $rc"
`;
const CURL_ERRORS = { 7: "daemon_unreachable", 28: "daemon_timeout", 63: "response_too_large" };

/**
 * `machine`: a Herdr machine name (a space's `machine`), or null for this computer.
 * `transport`: "fetch" (default here) or "exec" (always used for another machine).
 */
export function createClient({ url = DEFAULT_URL, tokenFile, machine = null, transport } = {}) {
  const base = String(url).replace(/\/+$/, "");
  const port = Number(LOOPBACK.exec(base)?.[1]);
  if (!(port > 0 && port < 65536)) throw new TrellisError("invalid_url", `Not a loopback daemon address: ${base}. Use http://127.0.0.1:<port>.`);
  const viaExec = machine !== null || transport === "exec";
  const file = tokenFile || (viaExec ? DEFAULT_TOKEN_FILE : `${home()}/.config/trellis/serve.token`);
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

  async function viaShell(path, body) {
    let result;
    try { result = await exec("sh", ["-c", REMOTE, "trellis", file, String(port), path, body === undefined ? "" : JSON.stringify(body)], { on: machine }); }
    catch { throw new TrellisError("machine_unreachable", `Cannot run a command on ${machine ?? "this computer"}.`); }
    const out = result.stdout || "";
    const error = /^TRELLIS-ERROR (\w+)\s*$/.exec(out);
    if (error) throw new TrellisError(error[1], tokenMessages(file)[error[1]] || "curl is not installed on the daemon's machine.");
    const curl = /\nTRELLIS-CURL (\d+)\s*$/.exec(out) || /^TRELLIS-CURL (\d+)\s*$/.exec(out);
    if (curl) {
      const code = CURL_ERRORS[curl[1]] || "transport_failed";
      throw new TrellisError(code, code === "daemon_unreachable" ? `No Trellis daemon answers at ${where}. Start \`trellis serve\`.`
        : code === "daemon_timeout" ? `The daemon at ${where} did not answer within ${MAX_SECONDS} s.`
        : code === "response_too_large" ? "The daemon's answer is larger than the mod reads." : `curl failed (${curl[1]}) on ${machine ?? "this computer"}.`);
    }
    const at = out.lastIndexOf("\nTRELLIS-STATUS ");
    const status = at >= 0 ? Number(/^\d{3}/.exec(out.slice(at + 16))?.[0]) : NaN;
    if (!Number.isInteger(status) || status === 0) throw new TrellisError("invalid_response", "The daemon's answer was cut short or unreadable.");
    const text = out.slice(0, at);
    let data = null;
    if (status !== 204) {
      try { data = JSON.parse(text); }
      catch { throw new TrellisError("invalid_response", `The daemon sent an unreadable answer (${status}).`, status); }
    }
    return answer(status, data);
  }

  const request = viaExec ? viaShell : viaFetch;
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
