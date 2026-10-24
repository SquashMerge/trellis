// The parser checks token spellings before JSON.parse can discard duplicate keys or -0.
export function parseJson(input, { maxDepth = 64 } = {}) {
  const bytes = Buffer.isBuffer(input) || input instanceof Uint8Array;
  if (bytes && input[0] === 0xef && input[1] === 0xbb && input[2] === 0xbf)
    throw new SyntaxError("JSON.INTAKE leading BOM");
  const source = bytes
    ? new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(input)
    : input;
  if (typeof source !== "string")
    throw new TypeError("JSON input must be UTF-8 bytes or text");
  if (source.charCodeAt(0) === 0xfeff)
    throw new SyntaxError("JSON.INTAKE leading BOM");
  let depth = 0, quoted = false, escaped = false;
  for (const c of source) {
    if (quoted) {
      if (escaped) escaped = false;
      else if (c === "\\") escaped = true;
      else if (c === '"') quoted = false;
    } else if (c === '"') quoted = true;
    else if (c === "{" || c === "[") {
      if (++depth > maxDepth) throw new SyntaxError("JSON.INTAKE nesting limit exceeded");
    } else if (c === "}" || c === "]") depth--;
  }
  let i = 0;
  const error = (message) => {
    throw new SyntaxError(`JSON.INTAKE at offset ${i}: ${message}`);
  };
  const space = () => {
    while (i < source.length && /[ \t\n\r]/.test(source[i])) i++;
  };
  function string() {
    const begin = i;
    if (source[i++] !== '"') error("expected string");
    while (i < source.length) {
      const c = source[i++];
      if (c === '"') {
        const s = JSON.parse(source.slice(begin, i));
        if (!validString(s)) error("non-scalar string");
        return s;
      }
      if (c === "\\") {
        const e = source[i++];
        if (e === "u") {
          if (!/^[a-fA-F0-9]{4}$/.test(source.slice(i, i + 4)))
            error("invalid Unicode escape");
          i += 4;
        } else if (!'"\\/bfnrt'.includes(e ?? "")) error("invalid escape");
      } else if (c.charCodeAt(0) < 32) error("unescaped control");
    }
    error("unterminated string");
  }
  function value() {
    space();
    const c = source[i];
    if (c === '"') return string();
    if (c === "{") {
      i++;
      const out = Object.create(null),
        seen = new Set();
      space();
      if (source[i] === "}") {
        i++;
        return out;
      }
      while (true) {
        space();
        if (source[i] !== '"') error("expected object key");
        const key = string();
        if (seen.has(key)) error(`duplicate key ${key}`);
        seen.add(key);
        space();
        if (source[i++] !== ":") error("expected colon");
        out[key] = value();
        space();
        const separator = source[i++];
        if (separator === "}") return out;
        if (separator !== ",") error("expected comma");
      }
    }
    if (c === "[") {
      i++;
      const out = [];
      space();
      if (source[i] === "]") {
        i++;
        return out;
      }
      while (true) {
        out.push(value());
        space();
        const separator = source[i++];
        if (separator === "]") return out;
        if (separator !== ",") error("expected comma");
      }
    }
    for (const [word, v] of [
      ["true", true],
      ["false", false],
      ["null", null],
    ])
      if (source.startsWith(word, i)) {
        i += word.length;
        return v;
      }
    if (c === "-" || /[0-9]/.test(c ?? "")) {
      const token =
        /^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?/.exec(
          source.slice(i),
        )?.[0];
      if (!token) error("invalid number");
      i += token.length;
      if (
        !/^(?:0|[1-9][0-9]*)$/.test(token) ||
        !Number.isSafeInteger(Number(token))
      )
        error("expected safe unsigned integer");
      return Number(token);
    }
    error("invalid value");
  }
  const result = value();
  space();
  if (i !== source.length) error("trailing input");
  return result;
}
function validString(s) {
  return (
    typeof s === "string" &&
    !/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/u.test(
      s,
    )
  );
}
function encodeString(s) {
  if (!validString(s)) throw new TypeError("JSON.CANONICAL non-scalar string");
  return (
    '"' +
    s.replace(/["\\\u0000-\u001f]/g, (c) =>
      c === '"'
        ? '\\"'
        : c === "\\"
          ? "\\\\"
          : `\\u00${c.charCodeAt(0).toString(16).padStart(2, "0")}`,
    ) +
    '"'
  );
}
function encode(value) {
  if (value === null) return "null";
  if (typeof value === "string") return encodeString(value);
  if (typeof value === "boolean") return String(value);
  if (
    typeof value === "number" &&
    Number.isSafeInteger(value) &&
    value >= 0 &&
    !Object.is(value, -0)
  )
    return String(value);
  if (Array.isArray(value)) {
    const parts = [];
    for (let i = 0; i < value.length; i++) {
      if (!Object.hasOwn(value, i))
        throw new TypeError(`JSON.CANONICAL sparse array at [${i}]`);
      parts.push(encode(value[i]));
    }
    return `[${parts.join(",")}]`;
  }
  if (
    typeof value === "object" &&
    value &&
    (Object.getPrototypeOf(value) === Object.prototype ||
      Object.getPrototypeOf(value) === null)
  ) {
    const keys = Object.keys(value).sort((a, b) =>
      Buffer.compare(Buffer.from(a), Buffer.from(b)),
    );
    return `{${keys.map((key) => `${encodeString(key)}:${encode(value[key])}`).join(",")}}`;
  }
  throw new TypeError("JSON.CANONICAL unsupported value");
}
export function canonicalBytes(value) {
  return Buffer.from(encode(value), "utf8");
}
