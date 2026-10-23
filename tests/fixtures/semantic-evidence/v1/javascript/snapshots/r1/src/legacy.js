import { clean } from "./text.js";

// An old source adapter uses prototypes, not ES class inheritance.
export function LegacySource(prefix) {
  this.prefix = prefix;
  this.buffer = [];
}
LegacySource.prototype.append = function (line) {
  this.buffer.push(this.prefix + clean(line));
};
LegacySource.prototype.drain = function () {
  return this.buffer.splice(0);
};

export function TaggedSource(prefix, tag) {
  LegacySource.call(this, prefix);
  this.tag = tag;
}
TaggedSource.prototype = Object.create(LegacySource.prototype);
TaggedSource.prototype.constructor = TaggedSource;
TaggedSource.prototype.append = function (line) {
  LegacySource.prototype.append.call(this, `[${this.tag}] ${line}`);
};

export function readLegacy(lines) {
  const source = new TaggedSource("legacy:", "import");
  lines.forEach((line) => source.append(line));
  return source.drain();
}

export function adapterMethods(source) {
  return { append: source.append, drain: source.drain };
}
