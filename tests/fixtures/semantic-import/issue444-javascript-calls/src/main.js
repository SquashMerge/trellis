import { helper, Widget } from "./lib.js";
function plain() { return 1; }
function register(fn) { return fn; }
export function caller(obj, a) {
  plain();
  helper(2);
  const w = new Widget(3);
  w.run();
  obj.method();
  a?.b();
  register(plain);
  const s = "😀é"; plain();
  return [w, s];
}
