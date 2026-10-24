import { readFile, readdir, realpath, lstat } from "node:fs/promises";
import { basename, resolve, relative, join } from "node:path";
import { fileURLToPath } from "node:url";
import { validate } from "./formats.mjs";
import { parseJson } from "./json.mjs";
import { contentHash, sourceManifestHash } from "./identity.mjs";
import { toByteRange, verifyWitness } from "./coordinates.mjs";

function reject(id, field, message) {
  const error = new Error(`${id} ${field}: ${message}`);
  error.assertion = id;
  error.code = "invalidRecord";
  error.field = field;
  throw error;
}
const key = (value) => JSON.stringify(value);
const documentKey = (x) => key([x.sourceSetId, x.language, x.path]);
const roles = [
  "definition",
  "read",
  "write",
  "call",
  "type",
  "import",
  "alias",
];
const applicable = (language) =>
  language === "java" ? roles.filter((x) => x !== "alias") : roles;
const order = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));
const languages = ["java", "rust", "python", "javascript"];
const documentOrder = (a, b) =>
  languages.indexOf(a.key.language) - languages.indexOf(b.key.language) ||
  order(a.key.path, b.key.path);

// Resolve every byte path from the physical fixture root. This also rejects symlinked
// parent directories and files, including links whose target happens to remain inside.
export async function confinedFile(root, path) {
  validate("Path", path);
  const physical = await realpath(root),
    target = resolve(physical, path);
  if (relative(physical, target).startsWith("..") || target === physical)
    reject("IDENTITY.PATH", path, "outside fixture");
  let cursor = physical;
  for (const part of path.split("/")) {
    cursor = join(cursor, part);
    let info;
    try {
      info = await lstat(cursor);
    } catch (error) {
      if (error.code === "ENOENT")
        reject("IDENTITY.INVENTORY", path, "declared input missing");
      throw error;
    }
    if (info.isSymbolicLink() || (cursor === target && !info.isFile()))
      reject("IDENTITY.PATH", path, "symlink or non-file");
  }
  if ((await realpath(target)) !== target)
    reject("IDENTITY.PATH", path, "escaped fixture");
  return readFile(target);
}
async function jsonFile(root, path, type) {
  const bytes = await confinedFile(root, path);
  const value = parseJson(bytes);
  validate(type, value);
  return { bytes, value };
}
function unique(rows, selector, field) {
  const seen = new Set();
  for (const row of rows) {
    const id = selector(row);
    if (seen.has(id)) reject("IDENTITY.DUPLICATE", field, `duplicate ${id}`);
    seen.add(id);
  }
}
async function inventory(root, paths) {
  unique(paths, (x) => x, "files");
  const listed = new Set(paths);
  // Generated publication is not an input. All other fixture directories are closed.
  async function walk(folder = "") {
    for (const entry of await readdir(resolve(root, folder), {
      withFileTypes: true,
    })) {
      const path = folder ? `${folder}/${entry.name}` : entry.name;
      if (
        !folder &&
        (path === "fixture.json" ||
          path === ".DS_Store" ||
          path === "README.md")
      )
        continue;
      if (!folder && path === "generated" && entry.isDirectory()) continue;
      if (entry.isSymbolicLink()) reject("IDENTITY.INVENTORY", path, "symlink");
      if (entry.isDirectory()) await walk(path);
      else if (!entry.isFile() || !listed.has(path))
        reject("IDENTITY.INVENTORY", path, "unlisted fixture input");
    }
  }
  await walk();
  for (const path of listed) await confinedFile(root, path);
}

// The fixture directory name fixes its profile and language on every path
// (discovery, CLI generate/check): example/ is the JavaScript example, and a
// language directory holds that language's corpus, final or draft.
function admitDescriptor(root, fixture) {
  const name = basename(root);
  // Only these names are reachable from discovery and the CLI; other directory
  // names are unit-test roots, which count floors treat as final corpora.
  if (name !== "example" && !languages.includes(name)) return;
  const profiles = name === "example" ? ["example"] : ["corpus", "draft"];
  const language = name === "example" ? "javascript" : name;
  if (!profiles.includes(fixture.profile) || fixture.language !== language)
    reject("DISCOVERY.PROFILE", root, "fixture descriptor mismatch");
}
// Decision 0017: an executable or toolchain capture may be attested by the
// reference hardware profile instead of stored bytes. Profiles are trusted
// repository files (T00), so the pin is checked against the recorded digest only.
const repositoryRoot = fileURLToPath(new URL("../..", import.meta.url));
const pinSections = {
  executable: ["semanticProducers", "executableHash"],
  toolchain: ["toolchains", "toolchainHash"],
};
async function verifyPin(repository, capture) {
  const [path, name, extra] = capture.pinned.split("#");
  if (
    !name ||
    extra !== undefined ||
    !path.startsWith("docs/semantic-evidence/hardware/")
  )
    reject("IDENTITY.PATH", capture.pinned, "pin outside hardware profiles");
  const [section, field] = pinSections[capture.kind];
  const entries = parseJson(await confinedFile(repository, path))[section];
  const entry = Array.isArray(entries)
    ? entries.find((x) => x?.name === name)
    : undefined;
  if (!entry) reject("IDENTITY.DIGEST", capture.pinned, `no ${section} entry`);
  if (entry[field] !== capture.hash)
    reject(
      "IDENTITY.DIGEST",
      capture.pinned,
      `pinned digest differs: expected ${capture.hash}, profile ${entry[field]}`,
    );
}
export async function loadFixture(root, { repository = repositoryRoot } = {}) {
  root = await realpath(root);
  const { value: fixture } = await jsonFile(root, "fixture.json", "FixtureV1");
  admitDescriptor(root, fixture);
  unique(fixture.sourceSets, (x) => x.id, "sourceSets");
  unique(fixture.producers, (x) => x.id, "producers");
  unique(fixture.comparison.producers, (x) => x.id, "comparison.producers");
  unique(fixture.revisions, (x) => key([x.sourceSetId, x.id]), "revisions");
  unique(fixture.captures, (x) => x.ref, "captures.ref");
  const stored = fixture.captures.filter((x) => x.file !== undefined);
  unique(stored, (x) => x.file, "captures.file");
  const sources = new Map(),
    sourceFiles = [],
    revisions = new Map(),
    revisionChronology = new Map(),
    captureBytes = new Map(),
    captures = new Map();
  for (const set of fixture.sourceSets) {
    if (!set.languages.length || !set.languages.includes(fixture.language))
      reject(
        "IDENTITY.SOURCE_SET",
        "languages",
        "fixture language not admitted",
      );
    unique(set.languages, (x) => x, "sourceSets.languages");
    unique(set.dependencies, (x) => x, "sourceSets.dependencies");
    for (const dependency of set.dependencies)
      if (!fixture.sourceSets.some((x) => x.id === dependency))
        reject("IDENTITY.SOURCE_SET", "dependencies", "unknown source set");
  }
  for (const producer of [
    ...fixture.producers,
    ...fixture.comparison.producers,
  ]) {
    if (!producer.languages.length)
      reject("IDENTITY.PRODUCER", "languages", "empty producer languages");
    unique(producer.languages, (x) => x, "producers.languages");
    if (
      fixture.producers.includes(producer) &&
      !producer.languages.includes(fixture.language)
    )
      reject("IDENTITY.PRODUCER", "languages", "fixture language unsupported");
  }
  unique(fixture.semanticArtifacts, (x) => x, "semanticArtifacts");
  unique(fixture.annotationFiles, (x) => x, "annotationFiles");
  unique(
    [
      fixture.nativeArtifact,
      fixture.answersFile,
      fixture.dispositionsFile,
      fixture.anchorCasesFile,
      ...fixture.annotationFiles,
      ...fixture.semanticArtifacts,
    ],
    (x) => x,
    "inputFiles",
  );
  for (const path of [
    ...fixture.revisions.flatMap((x) => x.documents.map((d) => d.sourceFile)),
    ...stored.map((x) => x.file),
    fixture.nativeArtifact,
    ...fixture.annotationFiles,
    fixture.answersFile,
    fixture.dispositionsFile,
    fixture.anchorCasesFile,
  ])
    if (
      path === "fixture.json" ||
      path === "generated" ||
      path.startsWith("generated/")
    )
      reject(
        "IDENTITY.INVENTORY",
        path,
        "publication and descriptor bytes cannot be declared as inputs",
      );
  const intents = new Set();
  for (const intent of fixture.coverageIntents) {
    const id = key([
      intent.producerId,
      documentKey(intent.document),
      intent.revisionId,
    ]);
    if (intents.has(id))
      reject("IDENTITY.COVERAGE", "coverageIntents", "duplicate tuple");
    intents.add(id);
    const producer = fixture.producers.find((x) => x.id === intent.producerId);
    const revision = fixture.revisions.find(
      (x) =>
        x.id === intent.revisionId &&
        x.sourceSetId === intent.document.sourceSetId,
    );
    if (
      !producer ||
      !producer.languages.includes(intent.document.language) ||
      intent.document.language !== fixture.language ||
      !revision?.documents.some(
        (x) => documentKey(x.key) === documentKey(intent.document),
      )
    )
      reject(
        "IDENTITY.COVERAGE",
        "coverageIntents",
        "unadmitted producer/document/revision",
      );
    unique(intent.requestedRoles, (x) => x, "requestedRoles");
    if (
      intent.requestedRoles.some(
        (x) => !applicable(intent.document.language).includes(x),
      )
    )
      reject("IDENTITY.COVERAGE", "requestedRoles", "inapplicable role");
    if (intent.measurementSupport.length !== 4)
      reject(
        "IDENTITY.COVERAGE",
        "measurementSupport",
        "four families required",
      );
    unique(intent.measurementSupport, (x) => x.kind, "measurementSupport");
    for (const entry of intent.measurementSupport)
      if (entry.available !== (entry.diagnostic === null))
        reject(
          "IDENTITY.COVERAGE",
          "measurementSupport",
          "availability/diagnostic mismatch",
        );
  }
  for (const revision of fixture.revisions) {
    const set = fixture.sourceSets.find((x) => x.id === revision.sourceSetId);
    if (!set || !set.languages.includes(fixture.language))
      reject(
        "IDENTITY.REVISION",
        "revisions",
        "unadmitted source set/language",
      );
    if (!revision.documents.length)
      reject("IDENTITY.REVISION", "documents", "empty snapshot");
    unique(revision.documents, (x) => documentKey(x.key), "documents");
    for (let i = 1; i < revision.documents.length; i++)
      if (documentOrder(revision.documents[i - 1], revision.documents[i]) >= 0)
        reject("IDENTITY.MANIFEST", "documents", "unsorted snapshot");
    const documents = [];
    for (const item of revision.documents) {
      if (
        item.key.sourceSetId !== revision.sourceSetId ||
        item.revisionId !== revision.id ||
        item.key.language !== fixture.language ||
        !item.key.path
      )
        reject(
          "IDENTITY.DOCUMENT",
          "documents",
          "document tuple/source path mismatch",
        );
      const bytes = await confinedFile(root, item.sourceFile);
      try {
        new TextDecoder("utf-8", { fatal: true }).decode(bytes);
      } catch {
        reject("IDENTITY.UTF8", item.sourceFile, "invalid UTF-8 source bytes");
      }
      sourceFiles.push(item.sourceFile);
      const document = {
        key: item.key,
        revisionId: item.revisionId,
        contentHash: contentHash(bytes),
        byteLength: bytes.length,
      };
      documents.push(document);
      sources.set(
        key([revision.sourceSetId, revision.id, item.key.path]),
        bytes,
      );
    }
    const complete = { ...revision, documents };
    revisions.set(key([revision.sourceSetId, revision.id]), complete);
    if (!revisionChronology.has(revision.sourceSetId))
      revisionChronology.set(revision.sourceSetId, []);
    revisionChronology.get(revision.sourceSetId).push(complete);
  }
  const selected = revisions.get(
    key([fixture.comparison.sourceSetId, fixture.comparison.revisionId]),
  );
  if (!selected)
    reject("IDENTITY.COMPARISON", "comparison", "snapshot not admitted");
  // Fixture order is authored chronology within each source set. IDs are opaque.
  const selectedIndex = revisionChronology
    .get(fixture.comparison.sourceSetId)
    .indexOf(selected);
  for (const capture of fixture.captures) {
    captures.set(capture.ref, capture);
    if (capture.pinned !== undefined) {
      await verifyPin(repository, capture);
      continue;
    }
    const bytes = await confinedFile(root, capture.file);
    const actualHash = contentHash(bytes);
    if (actualHash !== capture.hash)
      reject(
        "IDENTITY.DIGEST",
        capture.file,
        `captured bytes differ: expected ${capture.hash}, actual ${actualHash}`,
      );
    captureBytes.set(capture.ref, bytes);
  }
  const matching = (kind, hash) =>
    fixture.captures.some((x) => x.kind === kind && x.hash === hash);
  for (const producer of [
    ...fixture.producers,
    ...fixture.comparison.producers,
  ]) {
    if (
      fixture.producers.includes(producer) &&
      !matching("executable", producer.executableHash)
    )
      reject(
        "IDENTITY.PRODUCER",
        producer.id,
        "executable capture unavailable",
      );
  }
  for (const revision of revisions.values())
    for (const [field, kind] of [
      ["toolchainHash", "toolchain"],
      ["configHash", "config"],
      ["dependencyHash", "dependency"],
    ])
      if (!matching(kind, revision[field]))
        reject("IDENTITY.DIGEST", field, "revision capture unavailable");
  const semanticBytes = [];
  for (const path of fixture.semanticArtifacts) {
    const capture = fixture.captures.find(
      (x) => x.file === path && x.kind === "semanticArtifact",
    );
    if (!capture)
      reject("IDENTITY.INVENTORY", path, "semantic artifact lacks capture");
    const artifact = await jsonFile(root, path, "SemanticCapture");
    if (
      !fixture.producers.some(
        (x) => x.id === artifact.value.producerId && x.kind === "semantic",
      )
    )
      reject("IDENTITY.PRODUCER", path, "unknown semantic producer");
    semanticBytes.push(artifact);
  }
  if (
    fixture.captures.filter((x) => x.kind === "semanticArtifact").length !==
    semanticBytes.length
  )
    reject(
      "IDENTITY.INVENTORY",
      "semanticArtifacts",
      "unlisted semantic capture",
    );
  const native = (
    await jsonFile(root, fixture.nativeArtifact, "NativeArtifact")
  ).value;
  unique(
    [
      ...native.declarations,
      ...native.calls,
      ...native.controls,
      ...native.references,
    ],
    (x) => x.ref,
    "native.ref",
  );
  for (const row of [
    ...native.declarations,
    ...native.calls,
    ...native.controls,
    ...native.references,
  ]) {
    const snapshot = revisions.get(
      key([row.document.sourceSetId, row.revisionId]),
    );
    if (
      !snapshot?.documents.some(
        (x) => documentKey(x.key) === documentKey(row.document),
      )
    )
      reject(
        "IDENTITY.NATIVE",
        row.ref,
        "native row outside admitted snapshot",
      );
  }
  if (
    !fixture.producers.some(
      (x) => x.id === native.producerId && x.kind === "native",
    )
  )
    reject("IDENTITY.PRODUCER", "nativeArtifact", "unknown native producer");
  const annotations = [];
  for (const path of fixture.annotationFiles) {
    const annotation = (await jsonFile(root, path, "AnnotationFile")).value;
    if (
      !sources.has(
        key([
          annotation.document.sourceSetId,
          annotation.revisionId,
          annotation.document.path,
        ]),
      ) ||
      !fixture.revisions.some(
        (r) =>
          r.id === annotation.revisionId &&
          r.sourceSetId === annotation.document.sourceSetId &&
          r.documents.some(
            (d) =>
              documentKey(d.key) === documentKey(annotation.document) &&
              path === `${d.sourceFile}.annotations.json`,
          ),
      )
    )
      reject("IDENTITY.ANNOTATION", path, "not adjacent to admitted source");
    annotations.push(annotation);
  }
  const semanticFacts = new Map();
  for (const { bytes, value } of semanticBytes)
    for (const fact of value.facts) {
      // A normalized provenance basis cannot be embedded in the bytes it hashes.
      if (fact.kind === "provenance")
        reject(
          "IDENTITY.SEMANTIC",
          fact.ref,
          "raw capture cannot contain normalized provenance",
        );
      const id = key([value.producerId, fact.ref]);
      if (semanticFacts.has(id))
        reject("IDENTITY.SEMANTIC", "facts", "duplicate raw fact");
      semanticFacts.set(id, { fact, hash: contentHash(bytes) });
    }
  const semanticProofs = new Map();
  for (const annotation of annotations) {
    const proofById = new Map(
      annotation.facts
        .filter((x) => x.kind === "provenance")
        .map((x) => [x.record.id, x.record]),
    );
    for (const fact of annotation.facts) {
      if (fact.kind === "coverage" || fact.kind === "provenance") continue;
      const proofId =
        fact.kind === "typeRelationship"
          ? fact.provenanceRef
          : fact.record.provenanceId;
      const provenance = proofById.get(proofId);
      const evidenceKind = {
        declarationBinding: "declarationBinding",
        symbol: "declarationBinding",
        reference: "semanticReference",
        callBinding: "semanticReference",
        typeRelationship: "typeRelationship",
      }[fact.kind];
      const documentHash = revisions
        .get(key([annotation.document.sourceSetId, annotation.revisionId]))
        ?.documents.find(
          (x) => documentKey(x.key) === documentKey(annotation.document),
        )?.contentHash;
      if (
        !provenance ||
        provenance.evidenceKind !== evidenceKind ||
        provenance.producerId !== provenance.basis?.producerId ||
        provenance.contentHash !== documentHash
      )
        reject(
          "IDENTITY.SEMANTIC",
          fact.ref,
          `missing, wrong-kind, or mismatched semantic provenance${provenance && provenance.contentHash !== documentHash ? `: expected contentHash ${documentHash}, actual ${provenance.contentHash}` : ""}`,
        );
      if (fact.kind === "typeRelationship" && fact.source.kind === "internal") {
        const declaration = native.declarations.find(
          (x) =>
            x.ref === fact.source.declarationRef &&
            x.revisionId === fact.source.revisionId &&
            documentKey(x.document) === documentKey(annotation.document),
        );
        const source = sources.get(
          key([
            annotation.document.sourceSetId,
            fact.source.revisionId,
            annotation.document.path,
          ]),
        );
        const name = declaration?.witnesses.find(
          (x) => x.field === "name",
        )?.witness;
        const encoding = fixture.producers.find(
          (x) => x.id === native.producerId,
        )?.positionEncoding;
        if (
          !declaration ||
          !source ||
          !name ||
          !encoding ||
          declaration.range.encoding !== encoding ||
          declaration.nameRange?.encoding !== encoding ||
          name.range.encoding !== encoding
        )
          reject(
            "IDENTITY.SEMANTIC",
            "fact.ref",
            "relationship source declaration identity or encoding mismatch",
          );
        let declarationRange, nameRange, witnessRange;
        try {
          declarationRange = toByteRange(source, declaration.range);
          nameRange = toByteRange(source, declaration.nameRange);
          witnessRange = verifyWitness(source, name, {
            within: declarationRange,
          });
        } catch (error) {
          const coordinateError = error.message?.startsWith(
            "COORD.INVALID_RANGE",
          );
          if (!coordinateError && !error.message?.startsWith("WITNESS."))
            throw error;
          const failure = new Error(
            `IDENTITY.SEMANTIC fact.ref: ${error.message}`,
          );
          failure.assertion = "IDENTITY.SEMANTIC";
          failure.code = coordinateError ? "invalidRange" : "invalidRecord";
          failure.field = "fact.ref";
          throw failure;
        }
        if (
          witnessRange.start !== nameRange.start ||
          witnessRange.end !== nameRange.end ||
          name.text !== declaration.name
        )
          reject(
            "IDENTITY.SEMANTIC",
            "fact.ref",
            "relationship source declaration name range or text mismatch",
          );
      }
      const raw = semanticFacts.get(key([provenance.producerId, fact.ref]));
      if (!raw || key(raw.fact) !== key(fact))
        reject(
          "IDENTITY.SEMANTIC",
          fact.ref,
          "fact absent or contradicts raw capture",
        );
      if (
        provenance.basis?.artifactHash !== raw.hash ||
        documentKey(provenance.document) !== documentKey(annotation.document) ||
        provenance.revisionId !== annotation.revisionId ||
        (fact.anchor &&
          (documentKey(fact.anchor.document) !==
            documentKey(annotation.document) ||
            fact.anchor.revisionId !== annotation.revisionId))
      )
        reject(
          "IDENTITY.SEMANTIC",
          fact.ref,
          `capture basis/document mismatch${provenance.basis?.artifactHash !== raw.hash ? `: expected artifactHash ${raw.hash}, actual ${provenance.basis?.artifactHash}` : ""}`,
        );
      const existing = semanticProofs.get(provenance.id);
      if (
        existing &&
        (existing.hash !== raw.hash ||
          existing.factRef !== fact.ref ||
          existing.factKind !== fact.kind ||
          key(existing.wrapper) !==
            key({ ...provenance, freshness: undefined }))
      )
        reject(
          "IDENTITY.SEMANTIC",
          fact.ref,
          "proof spans different captured facts or wrappers",
        );
      semanticProofs.set(provenance.id, {
        ...raw,
        factRef: fact.ref,
        factKind: fact.kind,
        wrapper: structuredClone({ ...provenance, freshness: undefined }),
      });
    }
  }
  const answers = (await jsonFile(root, fixture.answersFile, "AnswersInputV1"))
    .value;
  const dispositions = (
    await jsonFile(root, fixture.dispositionsFile, "DispositionsV1")
  ).value;
  const anchors = (
    await jsonFile(root, fixture.anchorCasesFile, "AnchorCasesV1")
  ).value;
  await inventory(root, [
    ...new Set(sourceFiles),
    ...stored.map((x) => x.file),
    fixture.nativeArtifact,
    ...fixture.annotationFiles,
    fixture.answersFile,
    fixture.dispositionsFile,
    fixture.anchorCasesFile,
  ]);
  return {
    root,
    fixture,
    comparison: fixture.comparison,
    selected,
    selectedIndex,
    revisions,
    revisionChronology,
    sources,
    captures,
    captureBytes,
    semanticBytes,
    semanticProofs,
    native,
    annotations,
    answers,
    dispositions,
    anchors,
    sourceManifestHash: (revision) =>
      sourceManifestHash(
        revision.documents.map((x) => ({
          document: x.key,
          contentHash: x.contentHash,
        })),
      ),
  };
}

export async function discoverFixtures(root) {
  const fixtures = [];
  for (const entry of (await readdir(root, { withFileTypes: true }))
    .filter((x) => x.isDirectory())
    .sort((a, b) => order(a.name, b.name))) {
    if (entry.name !== "example" && !languages.includes(entry.name))
      reject("DISCOVERY.LANGUAGE", entry.name, "unknown language directory");
    const path = join(root, entry.name);
    const children = await readdir(path, { withFileTypes: true });
    if (
      !children.some((x) => x.name === "fixture.json" && x.isFile()) ||
      children.some(
        (x) =>
          x.isDirectory() && (x.name === "corpus" || x.name === "javascript"),
      )
    )
      reject(
        "DISCOVERY.PROFILE",
        entry.name,
        "expected immediate fixture.json",
      );
    fixtures.push(path);
  }
  const seen = new Set();
  for (const path of fixtures) {
    const { value } = await jsonFile(path, "fixture.json", "FixtureV1");
    const name = relative(root, path);
    admitDescriptor(path, value);
    const pair = key([name]);
    if (seen.has(pair))
      reject("DISCOVERY.DUPLICATE", path, "duplicate fixture");
    seen.add(pair);
  }
  return fixtures;
}
