const languages = ["java", "rust", "python", "javascript", "typescript"];
const roles = [
  "definition",
  "read",
  "write",
  "call",
  "type",
  "import",
  "alias",
];
function decodeEscapes(spelling, language) {
  if (language === "python" || language === "rust") return spelling;
  if (language === "java")
    return spelling.replace(/\\u+([0-9a-fA-F]{4})/g, (_, digits) =>
      String.fromCharCode(parseInt(digits, 16)),
    );
  const decoded = spelling.replace(
    /\\u(?:([0-9a-fA-F]{4})|\{([0-9a-fA-F]{1,6})\})/g,
    (_, fixed, variable) => {
      const code = parseInt(fixed ?? variable, 16);
      if (code > 0x10ffff || (variable && code >= 0xd800 && code <= 0xdfff))
        throw new SyntaxError("LOOKUP.ESCAPE invalid scalar");
      return fixed ? String.fromCharCode(code) : String.fromCodePoint(code);
    },
  );
  return decoded;
}
export function lookupKey(language, spelling) {
  if (
    !languages.includes(language) ||
    typeof spelling !== "string" ||
    !spelling.length
  )
    throw new TypeError("LOOKUP.INPUT invalid language or spelling");
  if (language === "rust" && spelling.startsWith("r#")) {
    spelling = spelling.slice(2);
    if (!spelling || spelling.startsWith("#"))
      throw new SyntaxError("LOOKUP.ESCAPE invalid raw identifier");
  }
  if (!spelling.length) throw new SyntaxError("LOOKUP.ESCAPE empty identifier");
  const decoded = decodeEscapes(spelling, language);
  if (
    decoded.includes("\\") ||
    /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/u.test(
      decoded,
    )
  )
    throw new SyntaxError("LOOKUP.ESCAPE malformed identifier escape");
  return language === "rust"
    ? decoded.normalize("NFC")
    : language === "python"
      ? decoded.normalize("NFKC")
      : decoded;
}
export function applicableRoles(language) {
  if (!languages.includes(language))
    throw new TypeError("ROLE.LANGUAGE unknown language");
  return language === "java" ? roles.slice(0, -1) : [...roles];
}
export function validateRoles(
  language,
  selected,
  { site, callee = false } = {},
) {
  const applicable = applicableRoles(language);
  let last = -1;
  for (const role of selected) {
    const index = applicable.indexOf(role);
    if (index <= last)
      throw new Error("ROLE.ORDER inapplicable, duplicate or unordered role");
    last = index;
  }
  if (selected.includes("definition") && site !== "declaration")
    throw new Error("ROLE.DEFINITION requires declaration");
  if (
    selected.includes("alias") &&
    (site !== "declaration" || !selected.includes("definition"))
  )
    throw new Error("ROLE.ALIAS requires declaration and definition");
  if (selected.includes("call") && !callee)
    throw new Error("ROLE.CALL requires measured callee");
}
