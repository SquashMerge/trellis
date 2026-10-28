import { schemas } from "./schema.mjs";

export class FormatError extends Error {
  constructor(path, message) {
    super(`FORMAT.SHAPE ${path}: ${message}`);
    this.name = "FormatError";
    this.assertion = "FORMAT.SHAPE";
    this.code = "invalidRecord";
    this.field = path;
  }
}
const fail = (path, message) => {
  throw new FormatError(path, message);
};
const plain = (value) =>
  value !== null &&
  typeof value === "object" &&
  !Array.isArray(value) &&
  (Object.getPrototypeOf(value) === Object.prototype ||
    Object.getPrototypeOf(value) === null);
const scalar = (s) =>
  typeof s === "string" &&
  s.length > 0 &&
  !/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/u.test(
    s,
  );
function walk(spec, value, path) {
  if (typeof spec === "string" && Object.hasOwn(schemas, spec)) {
    walk(schemas[spec], value, path);
    if (spec === "ReferenceJoinDiagnostic" && value.join.status === "exact")
      fail(`${path}.join.status`, "non-exact diagnostic required");
    if (
      spec === "GraphRequest" ||
      (spec === "GraphRequestTemplate" && path.endsWith(".result.request"))
    ) {
      if (
        value.depth > 5 ||
        value.maxNodes < 1 ||
        value.maxNodes > 150 ||
        value.maxCalls > 500
      )
        fail(`${path}.request`, "request limits outside v1");
    }
    if ((spec === "GraphEdge" || spec === "GraphEdgeTemplate") && value.callSiteCount < 1)
      fail(`${path}.callSiteCount`, "positive site count required");
    return;
  }
  if (typeof spec === "string") {
    if (spec === "text" && scalar(value)) return;
    if (
      spec === "uint" &&
      Number.isSafeInteger(value) &&
      value >= 0 &&
      !Object.is(value, -0)
    )
      return;
    if (spec === "boolean" && typeof value === "boolean") return;
    if (
      spec === "hash" &&
      typeof value === "string" &&
      /^[a-f0-9]{64}$/.test(value)
    )
      return;
    if (
      spec === "syntaxId" &&
      typeof value === "string" &&
      /^sid:v1:[a-f0-9]{32}$/.test(value)
    )
      return;
    if (
      spec === "occurrenceId" &&
      typeof value === "string" &&
      /^occ:v2:[a-f0-9]{32}$/.test(value)
    )
      return;
    if (
      spec === "path" &&
      scalar(value) &&
      !/[\\\0]/.test(value) &&
      !value.startsWith("/") &&
      value.split("/").every((p) => p !== "" && p !== "." && p !== "..")
    )
      return;
    fail(path, `expected ${spec}`);
  }
  if (spec.nullable) {
    if (value === null) return;
    return walk(spec.nullable, value, path);
  }
  if (spec.either) {
    for (const variant of spec.either) {
      try {
        walk(variant, value, path);
        return;
      } catch (error) {
        if (!(error instanceof FormatError)) throw error;
      }
    }
    fail(path, "no union variant matches");
  }
  if (Object.hasOwn(spec, "literal")) {
    if (value !== spec.literal)
      fail(path, `expected ${JSON.stringify(spec.literal)}`);
    return;
  }
  if (spec.enum) {
    if (!spec.enum.includes(value))
      fail(path, `expected one of ${spec.enum.join(", ")}`);
    return;
  }
  if (spec.array) {
    if (!Array.isArray(value)) fail(path, "expected array");
    for (let i = 0; i < value.length; i++) {
      if (!Object.hasOwn(value, i))
        fail(`${path}[${i}]`, "sparse array element");
      walk(spec.array, value[i], `${path}[${i}]`);
    }
    return;
  }
  if (spec.union) {
    if (!plain(value)) fail(path, "expected object");
    const tag = spec.union.tag;
    if (!Object.hasOwn(value, tag))
      fail(`${path}.${tag}`, "missing discriminator");
    const variant = spec.union.variants[String(value[tag])];
    if (!variant) fail(`${path}.${tag}`, "unknown discriminator");
    return walk(variant, value, path);
  }
  if (spec.object) {
    if (!plain(value)) fail(path, "expected object");
    for (const key of Object.keys(spec.object))
      if (!Object.hasOwn(value, key))
        fail(`${path}.${key}`, "missing required field");
    for (const key of Object.keys(value))
      if (!Object.hasOwn(spec.object, key))
        fail(`${path}.${key}`, "unknown field");
    for (const [key, child] of Object.entries(spec.object))
      walk(child, value[key], `${path}.${key}`);
    if (
      spec === schemas.TypeRelationshipFact &&
      value.source.kind !== "internal"
    )
      fail(`${path}.source.kind`, "source must be internal");
    return;
  }
  fail(path, "invalid schema definition");
}
export function validate(typeName, value) {
  if (!Object.hasOwn(schemas, typeName))
    throw new TypeError(`Unknown format: ${typeName}`);
  walk(typeName, value, typeName);
}
