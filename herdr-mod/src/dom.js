// The small slice of the DOM that web/architecture-pele.js uses, for a runtime with no DOM.
//
// The adapter parses Pele's SVG, validates its structure, rewrites labels and colours, and
// routes arrows, all through ordinary DOM calls. This gives it those calls over a plain tree:
// an XML parser for Pele's own output, elements with ordered attributes, `children`,
// `textContent`, `classList`, and `querySelector(All)` for the selectors the adapter uses
// (compound tag/class/attribute selectors, comma lists, `*` and `:scope > ...`). Anything else
// fails loudly rather than answering wrongly. `serialize` turns the finished tree back into SVG.
// Repository text only ever reaches this tree as text content or attribute values.

const ENTITIES = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'" };

function decode(text) {
  return text.replace(/&(#x[0-9a-fA-F]+|#[0-9]+|[a-z]+);/g, (whole, name) => {
    if (name[0] === "#") {
      const code = name[1] === "x" ? parseInt(name.slice(2), 16) : parseInt(name.slice(1), 10);
      return Number.isFinite(code) && code >= 0 && code <= 0x10ffff ? String.fromCodePoint(code) : whole;
    }
    return ENTITIES[name] ?? whole;
  });
}
const escapeText = (text) => text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
const escapeAttr = (text) => escapeText(text).replace(/"/g, "&quot;");

class Text {
  constructor(data) { this.nodeType = 3; this.data = data; this.parentNode = null; }
  get textContent() { return this.data; }
  set textContent(value) { this.data = String(value); }
}

class ClassList {
  constructor(element) { this.element = element; }
  tokens() { return (this.element.getAttribute("class") || "").split(/\s+/).filter(Boolean); }
  contains(name) { return this.tokens().includes(name); }
  add(...names) {
    const tokens = this.tokens();
    for (const name of names) if (!tokens.includes(name)) tokens.push(name);
    this.element.setAttribute("class", tokens.join(" "));
  }
  remove(...names) { this.element.setAttribute("class", this.tokens().filter((t) => !names.includes(t)).join(" ")); }
}

export class Element {
  constructor(localName, namespaceURI) {
    this.nodeType = 1;
    this.localName = localName;
    this.tagName = localName;
    this.namespaceURI = namespaceURI;
    this.attrs = new Map();
    this.childNodes = [];
    this.parentNode = null;
    this.classList = new ClassList(this);
    this.listeners = [];
  }
  get children() { return this.childNodes.filter((node) => node.nodeType === 1); }
  get attributes() { return [...this.attrs].map(([name, value]) => ({ name, value })); }
  getAttribute(name) { return this.attrs.has(name) ? this.attrs.get(name) : null; }
  hasAttribute(name) { return this.attrs.has(name); }
  setAttribute(name, value) { this.attrs.set(name, String(value)); }
  removeAttribute(name) { this.attrs.delete(name); }
  get className() { return this.getAttribute("class") || ""; }
  set className(value) { this.setAttribute("class", value); }
  get textContent() {
    return this.childNodes.map((node) => node.textContent).join("");
  }
  set textContent(value) {
    for (const node of this.childNodes) node.parentNode = null;
    this.childNodes = value === "" || value === null || value === undefined ? [] : [new Text(String(value))];
    for (const node of this.childNodes) node.parentNode = this;
  }
  append(...nodes) {
    for (const node of nodes) {
      const child = typeof node === "string" ? new Text(node) : node;
      if (child.parentNode) child.parentNode.childNodes = child.parentNode.childNodes.filter((n) => n !== child);
      child.parentNode = this;
      this.childNodes.push(child);
    }
  }
  appendChild(node) { this.append(node); return node; }
  replaceChildren(...nodes) {
    for (const node of this.childNodes) node.parentNode = null;
    this.childNodes = [];
    this.append(...nodes);
  }
  addEventListener(type, listener) { this.listeners.push([type, listener]); }
  focus() {}
  contains(node) {
    for (let n = node; n; n = n.parentNode) if (n === this) return true;
    return false;
  }
  descendants(out = []) {
    for (const child of this.children) { out.push(child); child.descendants(out); }
    return out;
  }
  querySelectorAll(selector) {
    const groups = parseSelector(selector);
    const pool = groups.every((g) => g.scopeChild) ? this.children : this.descendants();
    return pool.filter((el) => groups.some((g) => (!g.scopeChild || el.parentNode === this) && matches(el, g.compound)));
  }
  querySelector(selector) { return this.querySelectorAll(selector)[0] ?? null; }
}

// Compound selectors only: tag or *, .class, [attr], [attr="value"]; comma lists; and
// ":scope > compound". Unsupported syntax throws, so a new adapter selector is noticed.
const selectorCache = new Map();
function parseSelector(selector) {
  if (selectorCache.has(selector)) return selectorCache.get(selector);
  const groups = selector.split(",").map((part) => {
    let text = part.trim(), scopeChild = false;
    const scoped = /^:scope\s*>\s*/.exec(text);
    if (scoped) { scopeChild = true; text = text.slice(scoped[0].length); }
    const compound = { tag: null, classes: [], attrs: [] };
    const re = /^(?:(\*|[a-zA-Z][\w-]*)|\.([\w-]+)|\[([\w:-]+)(?:="([^"]*)")?\])/;
    while (text.length) {
      const m = re.exec(text);
      if (!m) throw new Error(`Unsupported selector: ${selector}`);
      if (m[1]) compound.tag = m[1] === "*" ? null : m[1];
      else if (m[2]) compound.classes.push(m[2]);
      else compound.attrs.push([m[3], m[4] ?? null]);
      text = text.slice(m[0].length);
    }
    return { scopeChild, compound };
  });
  selectorCache.set(selector, groups);
  return groups;
}
function matches(el, { tag, classes, attrs }) {
  if (tag && el.localName !== tag) return false;
  if (classes.length) { const own = el.classList.tokens(); if (!classes.every((c) => own.includes(c))) return false; }
  return attrs.every(([name, value]) => el.hasAttribute(name) && (value === null || el.getAttribute(name) === value));
}

// A strict parser for well-formed XML with no DTD: what Pele emits. Malformed input gives a
// document whose root is <parsererror>, as a browser's DOMParser does.
function parseXml(text) {
  const root = new Element("#document", null);
  const stack = [root];
  let i = 0;
  const fail = (why) => { const e = new Element("parsererror", null); e.textContent = why; return e; };
  const at = (re) => { re.lastIndex = i; return re.exec(text); };
  const CLOSE = /<\/([\w:.-]+)\s*>/y, OPEN = /<([\w:.-]+)/y, SPACE = /\s*/y;
  const ATTR = /([\w:.-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')/y;
  while (i < text.length) {
    if (text.startsWith("<!--", i)) { const end = text.indexOf("-->", i + 4); if (end < 0) return fail("comment"); i = end + 3; continue; }
    if (text.startsWith("<?", i)) { const end = text.indexOf("?>", i + 2); if (end < 0) return fail("pi"); i = end + 2; continue; }
    if (text.startsWith("<![CDATA[", i)) {
      const end = text.indexOf("]]>", i + 9); if (end < 0) return fail("cdata");
      stack[stack.length - 1].append(new Text(text.slice(i + 9, end))); i = end + 3; continue;
    }
    if (text.startsWith("<!", i)) return fail("doctype");
    if (text.startsWith("</", i)) {
      const m = at(CLOSE);
      if (!m || stack.length < 2 || stack[stack.length - 1].localName !== m[1]) return fail("close");
      stack.pop(); i += m[0].length; continue;
    }
    if (text[i] === "<") {
      const tag = at(OPEN);
      if (!tag) return fail("tag");
      i += tag[0].length;
      const parent = stack[stack.length - 1];
      const el = new Element(tag[1], parent.namespaceURI);
      for (;;) {
        const space = at(SPACE)[0]; i += space.length;
        if (text.startsWith("/>", i)) { i += 2; break; }
        if (text[i] === ">") { i += 1; stack.push(el); break; }
        const attr = at(ATTR);
        if (!attr || !space) return fail("attribute");
        if (el.attrs.has(attr[1])) return fail("duplicate attribute");
        el.attrs.set(attr[1], decode(attr[2] ?? attr[3]));
        i += attr[0].length;
      }
      if (el.attrs.has("xmlns")) el.namespaceURI = el.getAttribute("xmlns");
      parent.append(el);
      continue;
    }
    const next = text.indexOf("<", i), end = next < 0 ? text.length : next;
    const chunk = text.slice(i, end);
    if (stack.length > 1) stack[stack.length - 1].append(new Text(decode(chunk)));
    else if (chunk.trim()) return fail("text outside root");
    i = end;
  }
  if (stack.length !== 1 || root.children.length !== 1) return fail("unclosed");
  const svg = root.children[0];
  svg.parentNode = null;
  return svg;
}

export class DOMParser {
  parseFromString(text, type) {
    if (type !== "image/svg+xml") throw new Error(`Unsupported document type: ${type}`);
    const documentElement = parseXml(String(text));
    return { documentElement, querySelector: (s) => documentElement.querySelector(s) };
  }
}

/** A document for the adapter: elements, plus a canvas whose 2D context measures with `measure`. */
export function createDocument(measure) {
  return {
    createElementNS: (ns, name) => new Element(name, ns),
    createElement(name) {
      if (name !== "canvas") return new Element(name, null);
      const ctx = { font: "14px sans-serif", measureText: (text) => ({ width: measure(String(text), fontSize(ctx.font)) }) };
      return { getContext: (kind) => (kind === "2d" ? ctx : null) };
    },
  };
}
const fontSize = (font) => Number(/(\d+(?:\.\d+)?)px/.exec(font)?.[1] ?? 14);

/** SVG text for an element tree. `override(el)` may return attributes to replace on output. */
export function serialize(el, override) {
  if (el.nodeType === 3) return escapeText(el.data);
  const extra = override?.(el);
  const attrs = new Map(el.attrs);
  if (extra) for (const [name, value] of Object.entries(extra)) attrs.set(name, String(value));
  const head = `<${el.localName}${[...attrs].map(([n, v]) => ` ${n}="${escapeAttr(v)}"`).join("")}`;
  if (!el.childNodes.length) return `${head}/>`;
  return `${head}>${el.childNodes.map((child) => serialize(child, override)).join("")}</${el.localName}>`;
}

/** UTF-8 byte counts for the adapter's size bounds (the runtime has no TextEncoder). */
export class TextEncoder {
  encode(value) {
    let bytes = 0;
    for (const ch of String(value)) {
      const code = ch.codePointAt(0);
      bytes += code < 0x80 ? 1 : code < 0x800 ? 2 : code < 0x10000 ? 3 : 4;
    }
    return { length: bytes };
  }
}
