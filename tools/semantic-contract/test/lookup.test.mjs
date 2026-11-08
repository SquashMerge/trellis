import test from "node:test";
import assert from "node:assert/strict";
import { lookupKey, applicableRoles, validateRoles } from "../lookup.mjs";
import { syntaxId } from "../identity.mjs";
test("LOOKUP.LEXICAL decoding and language normalization preserve raw spelling identity", () => {
  assert.equal(lookupKey("rust", "r#é"), "é");
  assert.equal(lookupKey("python", "Ａ"), "A");
  assert.equal(lookupKey("java", "\\u0061"), "a");
  assert.equal(lookupKey("javascript", "\\u{61}"), "a");
  assert.equal(lookupKey("typescript", "\\u{61}"), "a");
  assert.equal(lookupKey("typescript", "\\u0061"), "a");
  for (const language of ["java", "javascript", "typescript"])
    assert.notEqual(lookupKey(language, "é"), lookupKey(language, "é"));
  assert.equal(lookupKey("rust", "é"), lookupKey("rust", "é"));
  const make = (name) =>
    syntaxId({
      sourceSet: "core",
      path: "a.rs",
      language: "rust",
      ancestors: [],
      declaration: { kind: "function", name, signature: null, ordinal: 0 },
    });
  assert.notEqual(make("é"), make("é"));
  assert.throws(() => lookupKey("javascript", "\\u{110000}"), /LOOKUP.ESCAPE/);
  assert.throws(() => lookupKey("typescript", "\\u{110000}"), /LOOKUP.ESCAPE/);
  assert.throws(() => lookupKey("java", "\\u00Q0"), /LOOKUP.ESCAPE/);
});
test("ROLE.APPLICABILITY alias is absent in Java and requires definition on declaration", () => {
  assert.equal(applicableRoles("java").includes("alias"), false);
  assert.equal(applicableRoles("rust").length, 7);
  assert.throws(
    () => validateRoles("java", ["alias"], { site: "declaration" }),
    /ROLE.ORDER/,
  );
  validateRoles("python", ["definition", "alias"], { site: "declaration" });
  assert.throws(
    () => validateRoles("python", ["alias"], { site: "declaration" }),
    /ROLE.ALIAS/,
  );
  assert.throws(
    () => validateRoles("javascript", ["call"], { callee: false }),
    /ROLE.CALL/,
  );
  validateRoles("javascript", ["read", "call"], { callee: true });
  assert.throws(
    () => validateRoles("typescript", ["call"], { callee: false }),
    /ROLE.CALL/,
  );
  validateRoles("typescript", ["definition", "alias"], { site: "declaration" });
});

test("LOOKUP and ROLE raw spelling, declaration sites and all language roles", () => {
  for (const language of ["java", "rust", "python", "javascript", "typescript"]) {
    assert.deepEqual(
      applicableRoles(language),
      language === "java"
        ? ["definition", "read", "write", "call", "type", "import"]
        : ["definition", "read", "write", "call", "type", "import", "alias"],
    );
    validateRoles(language, ["definition"], { site: "declaration" });
    validateRoles(language, ["read", "write", "type", "import"], {
      site: "use",
    });
    validateRoles(language, ["read", "call"], { site: "use", callee: true });
    validateRoles(language, ["read"], { site: "use", callee: false }); // callback value is not a call
    assert.throws(
      () => validateRoles(language, ["definition"], { site: "use" }),
      /ROLE.DEFINITION/,
    );
    assert.throws(
      () => validateRoles(language, ["call"], { site: "use", callee: false }),
      /ROLE.CALL/,
    );
    assert.throws(
      () => validateRoles(language, ["read", "read"], { site: "use" }),
      /ROLE.ORDER/,
    );
    assert.throws(
      () => validateRoles(language, ["write", "read"], { site: "use" }),
      /ROLE.ORDER/,
    );
    assert.throws(
      () => validateRoles(language, ["bogus"], { site: "use" }),
      /ROLE.ORDER/,
    );
    if (language === "java")
      assert.throws(
        () =>
          validateRoles(language, ["definition", "alias"], {
            site: "declaration",
          }),
        /ROLE.ORDER/,
      );
    else {
      validateRoles(language, ["definition", "alias"], { site: "declaration" });
      assert.throws(
        () => validateRoles(language, ["alias"], { site: "use" }),
        /ROLE.ALIAS/,
      );
    }
  }
  assert.equal(lookupKey("rust", "r#name"), lookupKey("rust", "name"));
  assert.throws(() => lookupKey("rust", "r##name"), /LOOKUP.ESCAPE/);
  assert.throws(() => lookupKey("rust", "r#"), /LOOKUP.ESCAPE/);
  const key = (name) =>
    syntaxId({
      sourceSet: "core",
      path: "a.rs",
      language: "rust",
      ancestors: [],
      declaration: { kind: "function", name, signature: null, ordinal: 0 },
    });
  assert.notEqual(key("r#name"), key("name"));
});
