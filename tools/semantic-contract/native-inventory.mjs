// Decision 0003: trusted, versioned native extraction-input inventories.
//
// An artifact's own `extractionInputs` is a claim, never the authority. Normalization and
// checking compare it with the inventory declared here for the producer's exact
// (id, version) and the document's language, and fail closed when that producer version or
// language has no declaration or the claim differs. A producer that starts reading another
// input must ship under a new version with a new entry; it can never re-declare its inventory
// under an existing version.
const INVENTORIES = new Map([
  // Current production extractor. Its compiled grammars and normalization tables are
  // versioned by native-v12; no external capture affects per-document measurement.
  [
    "trellis.native.syntax\u0000native-v12",
    { java: [], rust: [], python: [], javascript: [] },
  ],
  // Conformance producer used by the example fixture and the toolkit tests. Like the Rust
  // native producer (`EXTRACTION_INPUTS` in src/native_evidence.rs), it measures a document
  // from only its own bytes, path and language, so every language declares `[]`.
  ["native\u00001", { java: [], rust: [], python: [], javascript: [] }],
  // Conformance-only producer version that reads its config capture, used to exercise a
  // legitimate inventory change (a new version) and the omitted-input rejection.
  [
    "native\u00001+config",
    {
      java: ["config"],
      rust: ["config"],
      python: ["config"],
      javascript: ["config"],
    },
  ],
]);

/** Every production declaration, excluding conformance-only producers. */
export function productionInventoryEntries() {
  return [...INVENTORIES].flatMap(([key, languages]) => {
    const [id, version] = key.split("\u0000");
    return id === "trellis.native.syntax"
      ? [[id, version, Object.entries(languages)]]
      : [];
  });
}

/** The trusted inventory for this producer version and language, or undefined if none. */
export function trustedInventory(producerId, producerVersion, language) {
  const entry = INVENTORIES.get(`${producerId}\u0000${producerVersion}`);
  return entry && Object.hasOwn(entry, language) ? entry[language] : undefined;
}

/** Null when `declared` equals the trusted inventory; otherwise the failure reason. */
export function inventoryMismatch(
  producerId,
  producerVersion,
  language,
  declared,
) {
  const trusted = trustedInventory(producerId, producerVersion, language);
  if (trusted === undefined)
    return `no trusted inventory for native producer ${producerId}@${producerVersion} (${language})`;
  if (
    !Array.isArray(declared) ||
    declared.length !== trusted.length ||
    declared.some((name, i) => name !== trusted[i])
  )
    return `declared inventory differs from the trusted inventory of ${producerId}@${producerVersion} (${language})`;
  return null;
}
