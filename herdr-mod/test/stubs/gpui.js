// Test stand-ins for the host's gpui-kit and gpui-base: every builder records its calls, so a
// test can read the text a view would draw and find elements by id.
class El {
  constructor(kind, arg) { this.kind = kind; this.arg = arg; this.kids = []; this.props = {}; }
  child(value) { if (value !== undefined && value !== null && value !== false) this.kids.push(value); return this; }
  children(list) { for (const value of list || []) this.child(value); return this; }
  when(cond, fn) { return cond ? fn(this) : this; }
  id(value) { this.props.id = value; return this; }
  on_click(fn) { this.props.click = fn; return this; }
  on_size_change(fn) { this.props.size = fn; return this; }
  on_mouse_down(button, fn) { this.props[`press-${button}`] = fn; return this; }
  hover(fn) { return this; }
  // Recorded so a test can tell a region that scrolls sideways.
  overflow_x_scrollbar() { this.props.scrollX = true; return this; }
  w(value) { this.props.w = value; return this; }
  text() { return [this.kind === "image" ? `[image ${this.arg}]` : "", ...this.kids.map((k) => (k instanceof El ? k.text() : String(k)))].filter(Boolean).join(" "); }
  find(id) { if (this.props.id === id) return this; for (const k of this.kids) if (k instanceof El) { const f = k.find(id); if (f) return f; } return null; }
  all(pred, out = []) { if (pred(this)) out.push(this); for (const k of this.kids) if (k instanceof El) k.all(pred, out); return out; }
}
// Unknown builder methods chain; `then` stays undefined so an element is never a thenable.
const chain = (el) => new Proxy(el, { get: (target, name) => (name in target || name === "then" ? target[name] : () => chain(target)) });
export const div = () => chain(new El("div"));
export const image = (path) => chain(new El("image", path));
export const svg = (path) => chain(new El("svg", path));
export const v_flex = div, h_flex = div;
export class View {}
export { El };
