import test from "node:test";
import assert from "node:assert/strict";
import {
  cp,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  rename,
  writeFile,
} from "node:fs/promises";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { discoverFixtures } from "../load.mjs";
import { checkPublication, generateFixture } from "../publish.mjs";
import { runFixtures, parseRunnerArgs } from "./run.mjs";

const defaultRoot = resolve(
  fileURLToPath(
    new URL("../../../tests/fixtures/semantic-evidence/v1/", import.meta.url),
  ),
);
const fixturesRoot = process.env.SEMANTIC_FIXTURES_ROOT
  ? resolve(process.env.SEMANTIC_FIXTURES_ROOT)
  : defaultRoot;
async function copyExample() {
  const root = await mkdtemp(join(tmpdir(), "trellis-fixture-discovery-"));
  await cp(join(defaultRoot, "example"), join(root, "example"), {
    recursive: true,
  });
  return { root, cleanup: () => rm(root, { recursive: true, force: true }) };
}
async function descriptor(root, folder, change) {
  const path = join(root, folder, "fixture.json"),
    value = JSON.parse(await readFile(path, "utf8"));
  change(value);
  await writeFile(path, JSON.stringify(value));
}
const error = (assertion, field) => (value) => {
  assert.equal(value.assertion, assertion);
  assert.equal(value.code, "invalidRecord");
  assert.equal(value.field, field);
  assert.ok(value.message.includes(assertion));
  return true;
};

test("discovered fixtures are validated from the caller-selected root without a language registry", async () => {
  const paths = await discoverFixtures(fixturesRoot);
  assert.ok(paths.length > 0, "at least one fixture is required");
  for (const path of paths) assert.ok(path.startsWith(fixturesRoot));
  assert.equal(
    parseRunnerArgs(["--fixtures-root", fixturesRoot]),
    fixturesRoot,
  );
  assert.throws(() => parseRunnerArgs(["--fixtures-root"]), /RUNNER.ARGS/);
});

test("runner verifies a valid copied example and does not silently skip it", async (t) => {
  const { root, cleanup } = await copyExample();
  t.after(cleanup);
  const outcome = await runFixtures(root).then(
    (count) => count,
    (error) => error.assertion,
  );
  assert.equal(outcome, 1);
});

test("the committed example publication passes no-write checks", async () => {
  if (fixturesRoot !== defaultRoot) return; // Temporary roots may intentionally omit publication.
  await checkPublication(join(fixturesRoot, "example"));
});

// Source, measurements, semantic capture and expected evidence are all authored under
// this temporary language directory. The runner must derive every published count.
const sha = (value) => createHash("sha256").update(value).digest("hex");
const canonical = (value) =>
  value === null || typeof value !== "object"
    ? JSON.stringify(value)
    : Array.isArray(value)
      ? "[" + value.map(canonical).join(",") + "]"
      : "{" +
        Object.keys(value)
          .sort((a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b)))
          .map((k) => canonical(k) + ":" + canonical(value[k]))
          .join(",") +
        "}";
const span = (start, end) => ({ start, end, encoding: "utf8" });
const witness = (field, start, end, text) => ({
  field,
  witness: { range: span(start, end), text },
});
async function writeJavaCorpus(
  root,
  {
    document,
    text,
    decls,
    calls,
    refs,
    facts,
    scenarios,
    assertions,
    negatives,
  },
) {
  const files = new Map(),
    put = (path, value) =>
      files.set(
        path,
        typeof value === "string" ? value : JSON.stringify(value),
      );
  const sourceHash = sha(text);
  for (const scenario of scenarios)
    for (const anchor of scenario.anchors) anchor.contentHash = sourceHash;
  for (const fact of facts)
    if (fact.anchor) fact.anchor.contentHash = sourceHash;
  const native = {
    formatVersion: 1,
    producerId: "native",
    extractionInputs: [],
    declarations: decls,
    calls,
    controls: [],
    references: refs,
  };
  put("snapshots/src/Corpus.java", text);
  put("captures/native.json", native);
  for (const [name, value] of [
    ["native", "native-executable"],
    ["semantic", "semantic-executable-0"],
    ["toolchain", "toolchain"],
    ["config", "config"],
    ["dependency", "dependency"],
  ])
    put(`captures/${name}.txt`, value);
  for (const fact of facts) {
    const id = `proof-${fact.ref}`;
    if (fact.kind === "typeRelationship") fact.provenanceRef = id;
    else fact.record.provenanceId = id;
  }
  put("captures/semantic.json", {
    formatVersion: 1,
    producerId: "semantic",
    facts,
  });
  const captures = [
    ["native", "executable", "captures/native.txt"],
    ["semantic", "executable", "captures/semantic.txt"],
    ["toolchain", "toolchain", "captures/toolchain.txt"],
    ["config", "config", "captures/config.txt"],
    ["dependency", "dependency", "captures/dependency.txt"],
    ["artifact", "semanticArtifact", "captures/semantic.json"],
  ].map(([ref, kind, file]) => ({
    ref,
    kind,
    file,
    hash: sha(files.get(file)),
  }));
  const revision = {
    id: "r1",
    sourceSetId: "main",
    documents: [
      {
        key: document,
        revisionId: "r1",
        sourceFile: "snapshots/src/Corpus.java",
      },
    ],
    toolchainHash: captures[2].hash,
    configHash: captures[3].hash,
    dependencyHash: captures[4].hash,
  };
  const nativeProducer = {
    id: "native",
    version: "1",
    executableHash: captures[0].hash,
    kind: "native",
    languages: ["java"],
    positionEncoding: "utf8",
  };
  const semanticProducer = {
    id: "semantic",
    version: "1",
    executableHash: captures[1].hash,
    kind: "semantic",
    languages: ["java"],
    positionEncoding: "utf8",
  };
  const producers = [nativeProducer, semanticProducer].sort((a, b) =>
    Buffer.compare(Buffer.from(canonical(a)), Buffer.from(canonical(b))),
  );
  const support = ["declarationName", "callee", "invocation", "reference"].map(
    (kind) => ({
      kind,
      available: kind !== "invocation",
      diagnostic: kind === "invocation" ? "invocation unavailable" : null,
    }),
  );
  const semanticRoles = [
    "definition",
    "read",
    "write",
    "call",
    "type",
    "import",
  ];
  const coverage = (producerId) => ({
    producerId,
    language: "java",
    sourceSetId: "main",
    documentPath: document.path,
    revisionId: "r1",
    requested: true,
    selected: true,
    state: producerId === "native" ? "partial" : "complete",
    supportedRoles:
      producerId === "native" ? ["read", "write", "type"] : semanticRoles,
    observedRoles: producerId === "native" ? ["read", "write"] : semanticRoles,
    diagnostic: producerId === "native" ? "type not observed" : null,
  });
  const basis = {
    producerId: "semantic",
    producerVersion: "1",
    producerHash: semanticProducer.executableHash,
    artifactHash: captures[5].hash,
    language: "java",
    sourceSetId: "main",
    revisionId: "r1",
    sourceManifestHash: sha(canonical([{ document, contentHash: sourceHash }])),
    toolchainHash: revision.toolchainHash,
    configHash: revision.configHash,
    dependencyHash: revision.dependencyHash,
    lookupDependencies: [],
  };
  const provenance = facts.map((fact) => ({
    kind: "provenance",
    ref: `proof-fact-${fact.ref}`,
    record: {
      id: `proof-${fact.ref}`,
      producerId: "semantic",
      document,
      revisionId: "r1",
      contentHash: sourceHash,
      evidenceKind:
        fact.kind === "typeRelationship"
          ? "typeRelationship"
          : fact.kind === "declarationBinding"
            ? "declarationBinding"
            : "semanticReference",
      basis,
      freshness: "possiblyStale",
    },
  }));
  put("snapshots/src/Corpus.java.annotations.json", {
    formatVersion: 1,
    document,
    revisionId: "r1",
    scenarios,
    facts: [
      ...producers.map((p) => ({
        kind: "coverage",
        ref: `coverage-${p.id}`,
        record: coverage(p.id),
      })),
      ...provenance,
      ...facts,
    ],
  });
  const fixture = {
    formatVersion: 1,
    profile: "corpus",
    language: "java",
    sourceSets: [
      { id: "main", rootId: "root", languages: ["java"], dependencies: [] },
    ],
    producers,
    revisions: [revision],
    comparison: {
      sourceSetId: "main",
      revisionId: "r1",
      producers: producers.map((producer) =>
        producer.id === "semantic" ? { ...producer, version: "2" } : producer,
      ),
    },
    coverageIntents: producers.map((producer) => ({
      producerId: producer.id,
      document,
      revisionId: "r1",
      requestedRoles:
        producer.id === "native" ? ["read", "write", "type"] : semanticRoles,
      measurementSupport: support,
    })),
    nativeArtifact: "captures/native.json",
    semanticArtifacts: ["captures/semantic.json"],
    annotationFiles: ["snapshots/src/Corpus.java.annotations.json"],
    answersFile: "expected/answers.json",
    dispositionsFile: "expected/dispositions.json",
    anchorCasesFile: "expected/anchors.json",
    captures,
  };
  put("fixture.json", fixture);
  put("expected/answers.json", { formatVersion: 1, answers: [] });
  put("expected/dispositions.json", {
    formatVersion: 1,
    assertions,
    callableValueNegatives: negatives,
  });
  put("expected/anchors.json", { formatVersion: 1, cases: [] });
  for (const [path, value] of files) {
    await mkdir(dirname(join(root, path)), { recursive: true });
    await writeFile(join(root, path), value);
  }
}

async function authorJavaCorpus(root) {
  const doc = {
    sourceSetId: "main",
    language: "java",
    path: "src/Corpus.java",
  };
  let text = "// é\npackage corpus;\n",
    decls = [],
    calls = [],
    refs = [],
    facts = [],
    scenarios = [];
  const add = (part) => {
    const start = Buffer.byteLength(text);
    text += part;
    return start;
  };
  const anchor = (kind, start, end, ownerRef) => ({
    document: doc,
    revisionId: "r1",
    contentHash: "0".repeat(64),
    kind,
    range: span(start, end),
    ownerRef,
  });
  const simple = (
    ref,
    kind,
    name,
    begin,
    end,
    nameAt,
    parentRef = null,
    params = [],
    bases = [],
  ) => {
    const header = {
      kind,
      name,
      modifiers: [],
      typeParameters: [],
      parameters: params.map(([type, value]) => ({
        type,
        name: value,
        variadic: false,
      })),
      resultType: null,
      bases,
    };
    const witnesses = [
      witness("name", nameAt, nameAt + name.length, name),
      witness("header.name", nameAt, nameAt + name.length, name),
    ];
    for (const [i, [type, value]] of params.entries()) {
      const at = Buffer.from(text).indexOf(type, begin);
      witnesses.push(
        witness(`header.parameters[${i}].type`, at, at + type.length, type),
      );
      witnesses.push(
        witness(
          `header.parameters[${i}].name`,
          at + type.length + 1,
          at + type.length + 1 + value.length,
          value,
        ),
      );
      witnesses.push(
        witness(`signature.parameterTypes[${i}]`, at, at + type.length, type),
      );
    }
    for (const [i, base] of bases.entries()) {
      const at = Buffer.from(text).indexOf(base, begin);
      witnesses.push(witness(`header.bases[${i}]`, at, at + base.length, base));
    }
    decls.push({
      ref,
      nativeId: null,
    role: "production",
      document: doc,
      revisionId: "r1",
      parentRef: parentRef ?? (kind === "type" ? "unit" : null),
      kind,
      name,
      range: span(begin, end),
      nameRange: span(nameAt, nameAt + name.length),
      header,
      signature: params.length
        ? {
            parameterTypes: params.map(([type]) => type),
            typeParameterCount: 0,
            variadic: false,
          }
        : null,
      witnesses,
    });
  };
  const makeRef = (name, start, owner = "drive") => {
    const ref = `ref-${refs.length}`;
    refs.push({
      ref,
      nativeId: null,
      document: doc,
      revisionId: "r1",
      ownerRef: owner,
      range: span(start, start + name.length),
      spelling: name,
      witnesses: [witness("spelling", start, start + name.length, name)],
    });
    return ref;
  };
  const addFact = (kind, selector, record, extra = {}) => {
    const ref = `fact-${facts.length}`,
      proof = `proof-${ref}`;
    facts.push({
      kind,
      ref,
      ...(selector ? { anchor: selector } : {}),
      ...(kind === "typeRelationship"
        ? { ...extra, provenanceRef: proof }
        : { record, ...extra }),
    });
    return ref;
  };
  // Java imports are actual imported type names, not call-site aliases.
  const importSelectors = [];
  for (const name of ["List", "Map", "Set", "Collection"]) {
    const start = add(`import java.util.${name};\n`) + 17;
    const ref = makeRef(name, start, "unit");
    importSelectors.push({
      selector: anchor("reference", start, start + name.length, "unit"),
      ref,
      name,
    });
  }
  const host = add("class Host {\n"),
    targetStart = add(" void target() {}\n");
  simple(
    "target",
    "method",
    "target",
    targetStart,
    Buffer.byteLength(text) - 1,
    targetStart + 6,
    "host",
  );
  const fieldStart = add(" Host field;\n");
  simple(
    "field",
    "field",
    "field",
    fieldStart,
    Buffer.byteLength(text) - 1,
    fieldStart + 6,
    "host",
  );
  const ambiguousStart = add(" void ambiguous(String value) {}\n");
  simple(
    "ambiguous-string",
    "method",
    "ambiguous",
    ambiguousStart,
    Buffer.byteLength(text) - 1,
    ambiguousStart + 6,
    "host",
    [["String", "value"]],
  );
  const ambiguousOther = add(" void ambiguous(Integer value) {}\n");
  simple(
    "ambiguous-integer",
    "method",
    "ambiguous",
    ambiguousOther,
    Buffer.byteLength(text) - 1,
    ambiguousOther + 6,
    "host",
    [["Integer", "value"]],
  );
  const driveStart = add(" void drive() {\n");
  // Calls 44-63 deliberately have an overload error; 64-83 have an unknown method.
  // Both are valid Java syntax, but javac correctly rejects those semantic cases.
  const callSelectors = [],
    unsupportedSelectors = [];
  for (let i = 0; i < 120; i++) {
    const expression =
      i < 4
        ? "drive()"
        : i < 24 || i >= 84
          ? "target()"
          : i < 44
            ? "java.util.Objects.requireNonNull(this)"
            : i < 64
              ? "ambiguous(null)"
              : "unknown()";
    const name =
      i < 4
        ? "drive"
        : i < 24 || i >= 84
          ? "target"
          : i < 44
            ? "requireNonNull"
            : i < 64
              ? "ambiguous"
              : "unknown";
    const line = `  ${expression};\n`,
      begin = add(line),
      start = begin + 2 + expression.indexOf(name);
    const invocationStart = begin + 2,
      invocationEnd = invocationStart + expression.length;
    calls.push({
      ref: `call-${i}`,
      nativeId: null,
      document: doc,
      revisionId: "r1",
      ownerRef: "drive",
      range: span(invocationStart, invocationEnd),
      calleeRange: span(start, start + name.length),
      spelling: name,
      regionRefs: [],
      witnesses: [witness("spelling", start, start + name.length, name)],
    });
    callSelectors.push(anchor("callee", start, start + name.length, "drive"));
    if (i >= 4 && i < 23) makeRef(name, start);
  }
  // Native callee capture is partial: these 20 valid Java invocations have no
  // native Call row, and the producer cannot supply invocation-family joins.
  for (let i = 0; i < 20; i++) {
    const expression = "this.target()",
      start = add(`  ${expression};\n`) + 2;
    unsupportedSelectors.push(
      anchor("invocation", start, start + expression.length, "drive"),
    );
  }
  const refSelectors = [];
  for (let i = 0; i < 20; i++) {
    const line = `  Runnable callback${i} = this::target;\n`;
    const start = add(line) + line.indexOf("target");
    makeRef("target", start);
    refSelectors.push(anchor("reference", start, start + 6, "drive"));
  }
  const writeLine = "  this.field = this;\n",
    writeStart = add(writeLine) + writeLine.indexOf("field");
  makeRef("field", writeStart);
  const writeSelector = anchor(
    "reference",
    writeStart,
    writeStart + 5,
    "drive",
  );
  const typeLine = "  List<String> items = null;\n",
    typeStart = add(typeLine) + 2;
  makeRef("List", typeStart);
  const typeSelector = anchor("reference", typeStart, typeStart + 4, "drive");
  add(" }\n");
  simple(
    "drive",
    "method",
    "drive",
    driveStart,
    Buffer.byteLength(text) - 1,
    driveStart + 6,
    "host",
  );
  for (let i = 0; i < 4; i++) {
    const name = `over${i}`;
    for (const [j, type] of ["int", "String"].entries()) {
      const start = add(` void ${name}(${type} value) {}\n`);
      simple(
        `over-${i}-${j}`,
        "method",
        name,
        start,
        Buffer.byteLength(text) - 1,
        start + 6,
        "host",
        [[type, "value"]],
      );
    }
  }
  add("}\n");
  simple("host", "type", "Host", host, Buffer.byteLength(text) - 1, host + 6);
  const other = add("class Other {\n"),
    otherMethod = add(" void over0(long value) {}\n");
  simple(
    "other-over0",
    "method",
    "over0",
    otherMethod,
    Buffer.byteLength(text) - 1,
    otherMethod + 6,
    "other",
    [["long", "value"]],
  );
  add("}\n");
  simple(
    "other",
    "type",
    "Other",
    other,
    Buffer.byteLength(text) - 1,
    other + 6,
  );
  decls.push({
    ref: "unit",
    nativeId: null,
    role: "production",
    document: doc,
    revisionId: "r1",
    parentRef: null,
    kind: "module",
    name: null,
    range: span(0, 0),
    nameRange: null,
    header: {
      kind: "module",
      name: null,
      modifiers: [],
      typeParameters: [],
      parameters: [],
      resultType: null,
      bases: [],
    },
    signature: null,
    witnesses: [],
  });
  const relationshipSelectors = [];
  for (let i = 0; i < 20; i++) {
    const name = `Child${i}`,
      base = `Base${i}`;
    const start = add(`class ${name} extends ${base} {}\n`);
    simple(
      `child-${i}`,
      "type",
      name,
      start,
      Buffer.byteLength(text) - 1,
      start + 6,
      null,
      [],
      [base],
    );
    relationshipSelectors.push(
      anchor("declarationName", start + 6, start + 6 + name.length, "unit"),
    );
    const baseStart = add(`class ${base} {}\n`);
    simple(
      `base-${i}`,
      "type",
      base,
      baseStart,
      Buffer.byteLength(text) - 1,
      baseStart + 6,
    );
    addFact("typeRelationship", null, null, {
      source: {
        kind: "internal",
        declarationRef: `child-${i}`,
        revisionId: "r1",
      },
      target: {
        kind: "internal",
        declarationRef: `base-${i}`,
        revisionId: "r1",
      },
      relationshipKind: "extends",
    });
  }
  decls.find((row) => row.ref === "unit").range = span(
    0,
    Buffer.byteLength(text),
  );
  const relationFacts = facts.map((x) => x.ref);
  const targetRef = {
      kind: "internal",
      declarationRef: "target",
      revisionId: "r1",
    },
    driveRef = { kind: "internal", declarationRef: "drive", revisionId: "r1" };
  const external = {
    kind: "external",
    symbol: {
      scheme: "scip",
      symbol: "java.util.Objects requireNonNull",
      scope: "global",
      document: null,
    },
  };
  const assertions = [];
  const refFacts = [];
  // Each role is witnessed by its own Java source construct: imported names,
  // assignment, generic type use, invocation callee, method-reference read and declaration.
  for (let i = 0; i < 39; i++) {
    const selector =
      i < 19
        ? { ...callSelectors[i + 4], kind: "reference" }
        : refSelectors[i - 19];
    const roles = i < 19 ? ["read", "call"] : ["read"];
    const record = {
      site: "use",
      roles,
      resolution: "resolved",
      declaredTarget: targetRef,
      candidates: [],
      provenanceId: "",
    };
    const fact = addFact("reference", selector, record);
    refFacts.push(fact);
    if (i >= 19)
      assertions.push({
        kind: "resolution",
        factRef: fact,
        disposition: "resolved",
      });
  }
  const importFacts = importSelectors.map(({ selector, name }) =>
    addFact("reference", selector, {
      site: "use",
      roles: ["import"],
      resolution: "external",
      declaredTarget: {
        kind: "external",
        symbol: {
          scheme: "scip",
          symbol: `java.util ${name}`,
          scope: "global",
          document: null,
        },
      },
      candidates: [],
      provenanceId: "",
    }),
  );
  addFact("reference", writeSelector, {
    site: "use",
    roles: ["write"],
    resolution: "resolved",
    declaredTarget: {
      kind: "internal",
      declarationRef: "field",
      revisionId: "r1",
    },
    candidates: [],
    provenanceId: "",
  });
  addFact("reference", typeSelector, {
    site: "use",
    roles: ["type"],
    resolution: "external",
    declaredTarget: {
      kind: "external",
      symbol: {
        scheme: "scip",
        symbol: "java.util List",
        scope: "global",
        document: null,
      },
    },
    candidates: [],
    provenanceId: "",
  });
  for (const [group, resolution] of [
    "external",
    "ambiguous",
    "unresolved",
  ].entries())
    for (let i = 0; i < 20; i++) {
      const selector = callSelectors[24 + group * 20 + i];
      const fact = addFact("callBinding", selector, {
        resolution,
        declaredTarget: resolution === "external" ? external : null,
        candidates:
          resolution === "ambiguous"
            ? [
                {
                  kind: "internal",
                  declarationRef: "ambiguous-integer",
                  revisionId: "r1",
                },
                {
                  kind: "internal",
                  declarationRef: "ambiguous-string",
                  revisionId: "r1",
                },
              ]
            : [],
        dispatch: "direct",
        possibleDispatch: [],
        possibleDispatchComplete: false,
        provenanceId: "",
      });
      assertions.push({
        kind: "resolution",
        factRef: fact,
        disposition: resolution === "external" ? "provenExternal" : resolution,
      });
    }
  makeRef("target", targetStart + 6, "host");
  addFact(
    "reference",
    anchor("reference", targetStart + 6, targetStart + 12, "host"),
    {
      site: "declaration",
      roles: ["definition"],
      resolution: "resolved",
      declaredTarget: targetRef,
      candidates: [],
      provenanceId: "",
    },
  );
  const callFacts = [];
  for (let i = 0; i < 4; i++) {
    const fact = addFact("callBinding", callSelectors[i], {
      resolution: "resolved",
      declaredTarget: driveRef,
      candidates: [],
      dispatch: "direct",
      possibleDispatch: [],
      possibleDispatchComplete: false,
      provenanceId: "",
    });
    callFacts.push(fact);
  }
  const unsupportedFacts = [];
  for (let i = 0; i < 20; i++) {
    const fact = addFact("callBinding", unsupportedSelectors[i], {
      resolution: "unresolved",
      declaredTarget: null,
      candidates: [],
      dispatch: "unknown",
      possibleDispatch: [],
      possibleDispatchComplete: false,
      provenanceId: "",
    });
    unsupportedFacts.push(fact);
    assertions.push({
      kind: "join",
      factRef: fact,
      disposition: "unsupported",
    });
  }
  const overloadFacts = [];
  for (let i = 0; i < 4; i++) {
    const row = decls.find((d) => d.ref === `over-${i}-1`);
    overloadFacts.push(
      addFact(
        "declarationBinding",
        anchor(
          "declarationName",
          row.nameRange.start,
          row.nameRange.end,
          "host",
        ),
        {
          symbols: [
            {
              scheme: "scip",
              symbol: `pkg ${row.name} String`,
              scope: "global",
              document: null,
            },
          ],
          provenanceId: "",
        },
      ),
    );
  }
  const overloadAnchors = [];
  for (let i = 0; i < 4; i++)
    overloadAnchors.push(
      [0, 1].map((j) => {
        const row = decls.find((d) => d.ref === `over-${i}-${j}`);
        return anchor(
          "declarationName",
          row.nameRange.start,
          row.nameRange.end,
          "host",
        );
      }),
    );
  const scenario = (category, anchors, factRefs) =>
    scenarios.push({
      id: `scenario-${scenarios.length}`,
      category,
      anchors,
      factRefs,
    });
  for (let i = 0; i < 4; i++) {
    scenario("sameNameOverload", overloadAnchors[i], [overloadFacts[i]]);
    scenario("importsAliases", [importSelectors[i].selector], [importFacts[i]]);
    scenario(
      "callableValues",
      Array.from({ length: 5 }, (_, j) => refSelectors[i * 5 + j]),
      Array.from({ length: 5 }, (_, j) => refFacts[19 + i * 5 + j]),
    );
    scenario("recursion", [callSelectors[i]], [callFacts[i]]);
    scenario(
      "relationshipsDispatch",
      [relationshipSelectors[i]],
      [relationFacts[i]],
    );
    scenario(
      "unicodeCoordinates",
      [{ ...callSelectors[i + 8], kind: "reference" }],
      [refFacts[i + 4]],
    );
    scenario(
      "coverageFreshness",
      [{ ...callSelectors[i + 12], kind: "reference" }],
      [refFacts[i + 8]],
    );
    scenario(
      "compatibilityControl",
      [{ ...callSelectors[i + 16], kind: "reference" }],
      [refFacts[i + 12]],
    );
  }
  const negatives = Array.from({ length: 20 }, (_, i) => ({
    scenarioId: scenarios[Math.floor(i / 5) * 8 + 2].id,
    referenceRef: refFacts[i + 19],
    ownerRef: "drive",
    range: refSelectors[i].range,
  }));
  await writeJavaCorpus(root, {
    document: doc,
    text,
    decls,
    calls,
    refs,
    facts,
    scenarios,
    assertions,
    negatives,
  });
}

test("a source-backed Java corpus is discovered and published through the common runner", async (t) => {
  const { root, cleanup } = await copyExample();
  t.after(cleanup);
  await authorJavaCorpus(join(root, "java"));
  const source = await readFile(
    join(root, "java", "snapshots/src/Corpus.java"),
    "utf8",
  );
  const native = JSON.parse(
    await readFile(join(root, "java", "captures/native.json"), "utf8"),
  );
  const semantic = JSON.parse(
    await readFile(join(root, "java", "captures/semantic.json"), "utf8"),
  );
  const captured = semantic.facts.filter((fact) => fact.kind === "reference");
  assert.equal(native.calls.length, 120);
  assert.equal((source.match(/this::target/g) ?? []).length, 20);
  assert.equal((source.match(/this\.target\(\)/g) ?? []).length, 20);
  assert.equal((source.match(/ambiguous\(null\)/g) ?? []).length, 20);
  assert.equal((source.match(/unknown\(\)/g) ?? []).length, 20);
  assert.deepEqual(
    captured
      .filter((fact) => fact.record.roles.includes("import"))
      .map((fact) =>
        Buffer.from(source)
          .subarray(fact.anchor.range.start, fact.anchor.range.end)
          .toString("utf8"),
      )
      .sort(),
    ["Collection", "List", "Map", "Set"],
  );
  assert.equal(
    captured.filter((fact) => fact.record.roles.includes("write")).length,
    1,
  );
  assert.equal(
    captured.filter((fact) => fact.record.roles.includes("type")).length,
    1,
  );
  assert.equal(
    semantic.facts.filter(
      (fact) =>
        fact.kind === "callBinding" && fact.anchor.kind === "invocation",
    ).length,
    20,
  );
  assert.deepEqual(await discoverFixtures(root), [
    join(root, "example"),
    join(root, "java"),
  ]);
  assert.equal(
    await runFixtures(parseRunnerArgs(["--fixtures-root", root])),
    2,
  );
});

test("a copied minimal example cannot evade corpus floors by changing its profile", async (t) => {
  const { root, cleanup } = await copyExample();
  t.after(cleanup);
  await rename(join(root, "example"), join(root, "javascript"));
  await descriptor(root, "javascript", (value) => {
    value.profile = "corpus";
  });
  assert.deepEqual(await discoverFixtures(root), [join(root, "javascript")]);
  await assert.rejects(runFixtures(root), error("COUNT.FLOOR", "counts"));
});

test("a draft language corpus passes every invariant without floors until it is marked corpus", async (t) => {
  const { root, cleanup } = await copyExample();
  t.after(cleanup);
  await rename(join(root, "example"), join(root, "javascript"));
  await descriptor(root, "javascript", (value) => {
    value.profile = "draft";
  });
  assert.deepEqual(await discoverFixtures(root), [join(root, "javascript")]);
  // An author republishes after changing the descriptor; the runner then checks it.
  const manifest = await generateFixture(join(root, "javascript"));
  assert.equal(manifest.profile, "draft");
  assert.equal(await runFixtures(root), 1);
  await descriptor(root, "javascript", (value) => {
    value.profile = "corpus";
  });
  await assert.rejects(runFixtures(root), error("COUNT.FLOOR", "counts"));
});

test("the literal example cannot claim the draft profile", async (t) => {
  const { root, cleanup } = await copyExample();
  t.after(cleanup);
  await descriptor(root, "example", (value) => {
    value.profile = "draft";
  });
  await assert.rejects(
    discoverFixtures(root),
    error("DISCOVERY.PROFILE", join(root, "example")),
  );
});

test("discovery rejects unknown directory name with stable assertion and path", async (t) => {
  const { root, cleanup } = await copyExample();
  t.after(cleanup);
  await rename(join(root, "example"), join(root, "kotlin"));
  await assert.rejects(
    discoverFixtures(root),
    error("DISCOVERY.LANGUAGE", "kotlin"),
  );
});

test("discovery rejects a corpus descriptor with the wrong language or profile", async (t) => {
  const { root, cleanup } = await copyExample();
  t.after(cleanup);
  await rename(join(root, "example"), join(root, "javascript"));
  await descriptor(root, "javascript", (value) => {
    value.profile = "corpus";
    value.language = "python";
  });
  await assert.rejects(discoverFixtures(root), (value) => {
    assert.equal(value.assertion, "DISCOVERY.PROFILE");
    assert.equal(value.field, join(root, "javascript"));
    assert.equal(value.code, "invalidRecord");
    assert.match(value.message, /fixture descriptor mismatch/);
    return true;
  });
  await descriptor(root, "javascript", (value) => {
    value.language = "javascript";
    value.profile = "example";
  });
  await assert.rejects(
    discoverFixtures(root),
    error("DISCOVERY.PROFILE", join(root, "javascript")),
  );
});

test("duplicate nested language fixture and absent descriptor are rejected", async (t) => {
  const { root, cleanup } = await copyExample();
  t.after(cleanup);
  await cp(join(defaultRoot, "example"), join(root, "example", "javascript"), {
    recursive: true,
  });
  await assert.rejects(
    discoverFixtures(root),
    error("DISCOVERY.PROFILE", "example"),
  );
  await rm(join(root, "example", "javascript"), { recursive: true });
  await rm(join(root, "example", "fixture.json"));
  await assert.rejects(
    discoverFixtures(root),
    error("DISCOVERY.PROFILE", "example"),
  );
});

test("absent declared input fails a concrete inventory assertion", async (t) => {
  const { root, cleanup } = await copyExample();
  t.after(cleanup);
  const value = JSON.parse(
    await readFile(join(root, "example", "fixture.json"), "utf8"),
  );
  const absent = value.answersFile;
  await rm(join(root, "example", absent));
  await assert.rejects(runFixtures(root), (value) => {
    assert.equal(value.assertion, "IDENTITY.INVENTORY");
    assert.equal(value.code, "invalidRecord");
    assert.equal(value.field, absent);
    assert.match(value.message, /Fixture .*example.*declared input missing/s);
    return true;
  });
});
