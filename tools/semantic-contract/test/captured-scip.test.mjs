import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, rm, realpath, symlink, unlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { createHash } from "node:crypto";
import { checkCapturedScip } from "../captured-scip.mjs";
import { parseJson } from "../json.mjs";
import { sourceManifestHash } from "../identity.mjs";
import { parseArgs, main } from "../cli.mjs";
import { confinedFileCapped } from "../load.mjs";

const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");
const pin = "0".repeat(64);
const executableHash = "78082ea1a96a008751767bd35f43738b5d4565cbef06ee5b31ad742a316133b5";
const source = Buffer.from("function f(){g();}\n");
// Independent #57 canonical bytes and full digests, not derived expected values.
const vectors = [
  ['{"formatVersion":1,"positionEncoding":1,"raw":{"enclosingRange":null,"range":{"encoding":"singleLine","endCharacter":14,"line":0,"startCharacter":13},"symbol":"B#","symbolRoles":0,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}', "138cf48a43d4bedcee1dc8236317c40d691ef5fda117d1e30c15526c8bda21dc"],
  ['{"formatVersion":1,"positionEncoding":1,"raw":{"enclosingRange":null,"range":{"encoding":"multiLine","endCharacter":14,"endLine":0,"startCharacter":13,"startLine":0},"symbol":"B#","symbolRoles":0,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}', "e49f19eab6e6c590aa18b0e4257c9627bbc8ee998cc31c2567d8de168467c54f"],
  ['{"formatVersion":1,"positionEncoding":1,"raw":{"enclosingRange":null,"range":{"encoding":"legacy","values":[0,9,10]},"symbol":"A#","symbolRoles":1,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}', "40a8a56aa6ed4e8011c75c28ab01cb9f23a0bdc504fb4ec97025142e00f1f7a6"],
  ['{"formatVersion":1,"positionEncoding":1,"raw":{"displayName":"f","enclosingSymbol":"","kind":17,"symbol":"A#"},"recordKind":"symbolInformation","relativePath":"src/A.js"}', "39d9337353c729180a48d4489882e6740625199bd54fe9a98b59ad3eab98ec9f"],
  ['{"formatVersion":1,"positionEncoding":1,"raw":{"isDefinition":false,"isImplementation":true,"isReference":true,"isTypeDefinition":false,"sourceSymbol":"A#","targetSymbol":"B#"},"recordKind":"relationship","relativePath":"src/A.js"}', "e38fa5f77ba73cee8bbe54eeec279ff98c4b250366f3b59dc8288015bd538fb0"],
  ['{"formatVersion":1,"positionEncoding":1,"raw":{"enclosingRange":null,"range":{"encoding":"legacy","values":[0,10,14]},"symbol":"local 0","symbolRoles":1,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}', "6d600757051c743f49830194c6d0cf5d78b27c666fbf60e3ce95100384d45670"],
  ['{"formatVersion":1,"positionEncoding":1,"raw":{"enclosingRange":{"encoding":"singleLine","endCharacter":16,"line":0,"startCharacter":12},"range":{"encoding":"legacy","values":[0,13,0,14]},"symbol":"B#","symbolRoles":0,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}', "93671bccac30c539d7d133849995f8798f3e4ba7a56804e4577ab108870e30c4"],
  ['{"formatVersion":1,"positionEncoding":1,"raw":{"enclosingRange":{"encoding":"multiLine","endCharacter":16,"endLine":0,"startCharacter":12,"startLine":0},"range":{"encoding":"singleLine","endCharacter":14,"line":0,"startCharacter":13},"symbol":"B#","symbolRoles":0,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}', "c7cbbbd7c3e447e4f3d1e5438b361d94b755627dc66f4c9601a0f6449e6b18bd"],
  ['{"formatVersion":1,"positionEncoding":0,"raw":{"enclosingRange":null,"range":{"encoding":"legacy","values":[0,9,10]},"symbol":"A#","symbolRoles":1,"syntaxKind":0},"recordKind":"occurrence","relativePath":"src/A.js"}', "48eecbae6ce056236cc3decb2fb3c25c46bcd2fe54cc9badb5fb9a12ae318dc7"],
  ['{"formatVersion":1,"positionEncoding":0,"raw":{"displayName":"f","enclosingSymbol":"","kind":17,"symbol":"A#"},"recordKind":"symbolInformation","relativePath":"src/A.js"}', "68953b47a29e361cf99726fe506d7449dfc9a4c2c1a250aa0a6f52d0a96cc561"],
];
async function fixture(envelopes = vectors.slice(0, 5).map(([bytes]) => JSON.parse(bytes))) {
  const root = await realpath(await mkdtemp(join(tmpdir(), "trellis-scip-")));
  await mkdir(join(root, "sources/src"), { recursive: true });
  await mkdir(join(root, "captured-scip"));
  await writeFile(join(root, "sources/src/A.js"), source);
  const sourceRow = { language: "rust", relativePath: "src/A.js", contentHash: hash(source) };
  const artifact = { formatVersion: 1, mappingVersion: "CapturedScipJsonFixtureV1/1", documents: [{ language: "rust", relativePath: "src/A.js", positionEncoding: envelopes[0]?.positionEncoding ?? 1, facts: envelopes.map((e) => ({ recordKind: e.recordKind, raw: e.raw })) }] };
  const artifactBytes = Buffer.from(JSON.stringify(artifact));
  const manifest = { formatVersion: 1, trustOrigin: "referenceProfile", completion: "complete", decoderVersion: "CapturedScipJsonFixtureV1/1", profileId: "mac-mini-m5-pro-v1", producer: { name: "rust-analyzer", version: "1.98.1 (48a229ce 2026-09-01)", executableHash, positionEncoding: 1 }, sourceSet: { id: "source-set:v1:root", rootId: "root", languages: ["java", "rust", "python", "javascript"], dependencies: [] }, sourceRevision: `revision:v2:${pin}`, sourceManifestHash: sourceManifestHash([{ document: { sourceSetId: "source-set:v1:root", language: "rust", path: "src/A.js" }, contentHash: hash(source) }]), components: { toolchainHash: pin, configHash: pin, dependencyHash: pin }, metadata: { protocolVersion: 0, projectRoot: pathToFileURL(root).href, textDocumentEncoding: 1 }, sourceDocuments: [sourceRow], documents: [sourceRow], exclusions: [], artifacts: [{ kind: "scipJson", path: "captured-scip/artifact.json", hash: hash(artifactBytes), language: "rust", documents: ["src/A.js"] }] };
  async function save() { const bytes = Buffer.from(JSON.stringify(manifest)); await writeFile(join(root, "captured-scip/manifest.json"), bytes); return hash(bytes); }
  await writeFile(join(root, "captured-scip/artifact.json"), artifactBytes);
  return { root, manifest, artifact, save, expected: await save() };
}
const withFixture = async (fn, envelopes) => { const f = await fixture(envelopes); try { await fn(f); } finally { await rm(f.root, { recursive: true, force: true }); } };
test("pinned binary-vector envelope bytes and full digests, JSON-specific artifact/proof", async () => {
  await withFixture(async ({ root, expected }) => {
    const result = await checkCapturedScip(root, expected);
    assert.equal(result.claims.length, 5);
    for (const [bytes, digest] of vectors.slice(0, 5)) {
      const claim = result.claims.find((x) => x.canonicalBytes.toString() === bytes);
      assert.ok(claim, bytes);
      assert.equal(claim.factDigest, digest);
      assert.equal(result.lookup(claim.capturedProofId, claim.envelope.recordKind, digest), claim);
      assert.equal(result.lookup(claim.capturedProofId, claim.envelope.recordKind, digest), claim);
    }
    assert.equal(result.artifactHash, "a92d285f8aa5de9a9d0628dd99aabe6b4da74b13cce57332ab07ff9ba7a5a9d3");
    assert.equal(result.claims.find((x) => x.factDigest === vectors[0][1]).capturedProofId, "1f39d2c728ccbe6db4add313fd4cc95e91c83eba19d65459042635b01a3372db");
    assert.notEqual(result.artifactHash, "8e2f72f58e2d6a1cc0f1177522693069a57c36d67b8ca405965de42b3d7afc5b");
    assert.throws(() => result.lookup(pin, "occurrence", pin), { code: "MissingProof" });
    const selected = result.claims.find((x) => x.factDigest === vectors[0][1]);
    assert.throws(() => result.lookup(selected.capturedProofId, "relationship", selected.factDigest), { code: "MissingProof" });
    assert.throws(() => result.lookup(selected.capturedProofId, "occurrence", pin), { code: "MissingProof" });
    result.claims.push({ ...selected, envelope: { ...selected.envelope, raw: { ...selected.envelope.raw, symbol: "forged" } } });
    assert.throws(() => result.lookup(selected.capturedProofId, "occurrence", selected.factDigest), { code: "NonUniqueProof" });
  });
});
test("representative range branches and raw zero retain exact pinned bytes and digests", async () => {
  for (const [bytes, digest] of vectors.slice(6)) {
    await withFixture(async ({ root, expected }) => {
      const result = await checkCapturedScip(root, expected);
      assert.equal(result.claims[0].canonicalBytes.toString(), bytes);
      assert.equal(result.claims[0].factDigest, digest);
    }, [JSON.parse(bytes)]);
  }
});
test("external digest, profile, sources and artifact remain independently bound", async () => {
  await withFixture(async (f) => {
    await assert.rejects(checkCapturedScip(f.root, pin), { code: "InvalidAdmission" });
    f.manifest.producer.version = "0.0.0";
    await assert.rejects(checkCapturedScip(f.root, await f.save()), { code: "InvalidAdmission" });
    f.manifest.producer.version = "1.98.1 (48a229ce 2026-09-01)";
    f.expected = await f.save();
    await writeFile(join(f.root, "sources/src/A.js"), "different");
    await assert.rejects(checkCapturedScip(f.root, f.expected), { code: "InvalidAdmission" });
    await writeFile(join(f.root, "sources/src/A.js"), source);
    await writeFile(join(f.root, "captured-scip/artifact.json"), "{}");
    await assert.rejects(checkCapturedScip(f.root, f.expected), { code: "InvalidAdmission" });
  });
});
test("unknown raw keys, invalid bounds and deep input reject atomically", async () => {
  await withFixture(async (f) => {
    f.artifact.documents[0].facts[0].raw.unexpected = 1;
    const bytes = Buffer.from(JSON.stringify(f.artifact));
    await writeFile(join(f.root, "captured-scip/artifact.json"), bytes);
    f.manifest.artifacts[0].hash = hash(bytes);
    await assert.rejects(checkCapturedScip(f.root, await f.save()), { code: "InvalidArtifact" });
  });
  assert.throws(() => parseJson("[".repeat(65) + "]".repeat(65)), /nesting limit/);
  assert.equal(parseJson('"[[[["'), "[[[[");
  assert.throws(() => parseArgs(["check-captured-scip", "--fixture-root", ".", "--expected-manifest-hash", "bad"]));
});

test("production checker and CLI pin real producer and original bytes", async () => {
  await withFixture(async (f) => {
    assert.equal((await checkCapturedScip(f.root, f.expected)).claims.length, 5);
    assert.equal((await main(["check-captured-scip", "--fixture-root", f.root, "--expected-manifest-hash", f.expected])).claims.length, 5);
    f.manifest.producer.executableHash = pin;
    const wrongHash = await f.save();
    await assert.rejects(checkCapturedScip(f.root, wrongHash), { code: "InvalidAdmission" });
    await assert.rejects(main(["check-captured-scip", "--fixture-root", f.root, "--expected-manifest-hash", wrongHash]), { code: "InvalidAdmission" });
    assert.throws(() => parseArgs(["check-captured-scip", "--fixture-root", f.root, "--expected-manifest-hash", wrongHash, "--profile", "x"]));
  });
});
test("closed coverage, artifact and source path rejection", async () => {
  for (const mutate of [
    (f) => { f.manifest.documents[0].language = "python"; },
    (f) => { f.manifest.artifacts[0].kind = "scipBinary"; },
    (f) => { f.manifest.artifacts[0].documents = []; },
    (f) => { f.manifest.sourceDocuments[0].relativePath = "../outside"; },
    (f) => { f.manifest.metadata.projectRoot += "/"; },
    (f) => { f.manifest.completion = "partial"; },
  ]) await withFixture(async (f) => {
    mutate(f);
    await assert.rejects(checkCapturedScip(f.root, await f.save()), { code: /InvalidAdmission|InvalidArtifact/ });
  });
});
test("raw bounds, enum and count reject even with newly attested bytes", async () => {
  for (const mutate of [
    (raw) => { raw.range.endCharacter = 5000; },
    (raw) => { raw.range.startCharacter = 15; raw.range.endCharacter = 14; },
    (raw) => { raw.syntaxKind = 37; },
    (raw) => { raw.symbolRoles = 128; },
  ]) await withFixture(async (f) => {
    mutate(f.artifact.documents[0].facts[0].raw);
    const bytes = Buffer.from(JSON.stringify(f.artifact));
    await writeFile(join(f.root, "captured-scip/artifact.json"), bytes);
    f.manifest.artifacts[0].hash = hash(bytes);
    await assert.rejects(checkCapturedScip(f.root, await f.save()), { code: "InvalidArtifact" });
  });
});

test("confined capped reads reject symlink, traversal, absolute and oversized bytes", async () => {
  await withFixture(async (f) => {
    const actual = "sources/src/A.js";
    assert.deepEqual(await confinedFileCapped(f.root, actual, source.length), source);
    await assert.rejects(confinedFileCapped(f.root, actual, source.length - 1), { code: "LimitExceeded" });
    for (const path of ["../outside", "/etc/passwd", "sources/../src/A.js"])
      await assert.rejects(confinedFileCapped(f.root, path, 10));
    await symlink("A.js", join(f.root, "sources/src/link.js"));
    await assert.rejects(confinedFileCapped(f.root, "sources/src/link.js", 100));
  });
});
test("duplicate byte-identical facts deduplicate; count precedes dedup; distinct raw persists", async () => {
  await withFixture(async (f) => {
    f.artifact.documents[0].facts.push({ ...f.artifact.documents[0].facts[0] });
    f.artifact.documents[0].facts.push({ recordKind: "symbolInformation", raw: { ...f.artifact.documents[0].facts[3].raw, kind: 18 } });
    const bytes = Buffer.from(JSON.stringify(f.artifact));
    await writeFile(join(f.root, "captured-scip/artifact.json"), bytes);
    f.manifest.artifacts[0].hash = hash(bytes);
    const result = await checkCapturedScip(f.root, await f.save());
    assert.equal(result.claims.length, 6);
  });
});
test("production UTF-8 scalar boundary, CRLF and raw zero preserve digest value", async () => {
  for (const [encoding, column, valid] of [[1, 1, false], [1, 4, true], [1, 5, false]]) {
    await withFixture(async (f) => {
      const bytes = Buffer.from("😀\r\nx");
      await writeFile(join(f.root, "sources/src/A.js"), bytes);
      f.manifest.sourceDocuments[0].contentHash = hash(bytes);
      f.manifest.documents[0].contentHash = hash(bytes);
      f.manifest.sourceManifestHash = sourceManifestHash([{ document: { sourceSetId: f.manifest.sourceSet.id, language: "rust", path: "src/A.js" }, contentHash: hash(bytes) }]);
      f.manifest.producer.positionEncoding = encoding;
      f.artifact.documents[0].positionEncoding = 0;
      f.artifact.documents[0].facts = [{ recordKind: "occurrence", raw: { range: { encoding: "singleLine", line: 0, startCharacter: 0, endCharacter: column }, enclosingRange: null, symbol: "A#", symbolRoles: 0, syntaxKind: 0 } }];
      const artifactBytes = Buffer.from(JSON.stringify(f.artifact));
      await writeFile(join(f.root, "captured-scip/artifact.json"), artifactBytes);
      f.manifest.artifacts[0].hash = hash(artifactBytes);
      const expected = await f.save();
      if (valid) {
        const result = await checkCapturedScip(f.root, expected);
        assert.equal(result.claims[0].envelope.positionEncoding, 0);
      } else await assert.rejects(checkCapturedScip(f.root, expected), { code: "InvalidArtifact" });
    });
  }
});

test("per-document occurrence count is charged before deduplication", async () => {
  await withFixture(async (f) => {
    f.artifact.documents[0].facts = Array.from({ length: 10001 }, () => f.artifact.documents[0].facts[0]);
    const bytes = Buffer.from(JSON.stringify(f.artifact));
    await writeFile(join(f.root, "captured-scip/artifact.json"), bytes);
    f.manifest.artifacts[0].hash = hash(bytes);
    await assert.rejects(checkCapturedScip(f.root, await f.save()), { code: "LimitExceeded" });
  });
});

test("partial coverage authenticates all source bytes and explicit exclusions", async () => {
  await withFixture(async (f) => {
    const omitted = Buffer.from("fn omitted() {}\n");
    const row = { language: "rust", relativePath: "src/B.rs", contentHash: hash(omitted) };
    await writeFile(join(f.root, "sources/src/B.rs"), omitted);
    f.manifest.sourceDocuments.push(row);
    f.manifest.sourceManifestHash = sourceManifestHash(f.manifest.sourceDocuments.map((r) => ({ document: { sourceSetId: f.manifest.sourceSet.id, language: r.language, path: r.relativePath }, contentHash: r.contentHash })));
    f.manifest.exclusions = [{ language: "rust", relativePath: "src/B.rs", reason: "notRequested" }];
    f.manifest.completion = "partial";
    const expected = await f.save();
    const result = await checkCapturedScip(f.root, expected);
    assert.equal(result.completion, "partial");
    assert.equal(JSON.stringify(result.exclusions), JSON.stringify(f.manifest.exclusions));
    assert.equal(result.claims.length, 5);
    await writeFile(join(f.root, "sources/src/B.rs"), "changed");
    await assert.rejects(checkCapturedScip(f.root, expected), { code: "InvalidAdmission" });
  });
});
test("cross-language duplicate source path rejects before extraction", async () => {
  await withFixture(async (f) => {
    f.manifest.sourceDocuments.unshift({ ...f.manifest.sourceDocuments[0], language: "java" });
    f.manifest.sourceManifestHash = sourceManifestHash(f.manifest.sourceDocuments.map((r) => ({ document: { sourceSetId: f.manifest.sourceSet.id, language: r.language, path: r.relativePath }, contentHash: r.contentHash })));
    await assert.rejects(checkCapturedScip(f.root, await f.save()), { code: "InvalidAdmission", message: /duplicate source path/ });
  });
});
test("checker rejects deeply nested original JSON with a typed artifact failure", async () => {
  await withFixture(async (f) => {
    const bytes = Buffer.from("[".repeat(65) + "]".repeat(65));
    await writeFile(join(f.root, "captured-scip/artifact.json"), bytes);
    f.manifest.artifacts[0].hash = hash(bytes);
    await assert.rejects(checkCapturedScip(f.root, await f.save()), { code: "InvalidArtifact", message: /nesting limit/ });
  });
});

test("symbol, relationship and document caps count candidates before dedup", async () => {
  for (const [facts, length] of [["symbolInformation", 10001], ["relationship", 20001]]) {
    await withFixture(async (f) => {
      const fact = f.artifact.documents[0].facts.find((x) => x.recordKind === facts);
      f.artifact.documents[0].facts = Array.from({ length }, () => fact);
      const bytes = Buffer.from(JSON.stringify(f.artifact));
      await writeFile(join(f.root, "captured-scip/artifact.json"), bytes);
      f.manifest.artifacts[0].hash = hash(bytes);
      await assert.rejects(checkCapturedScip(f.root, await f.save()), { code: "LimitExceeded" });
    });
  }
  await withFixture(async (f) => {
    f.artifact.documents = Array.from({ length: 1001 }, () => f.artifact.documents[0]);
    const bytes = Buffer.from(JSON.stringify(f.artifact));
    await writeFile(join(f.root, "captured-scip/artifact.json"), bytes);
    f.manifest.artifacts[0].hash = hash(bytes);
    await assert.rejects(checkCapturedScip(f.root, await f.save()), { code: "LimitExceeded" });
  });
});

test("production checker rejects source symlink and absolute artifact path", async () => {
  await withFixture(async (f) => {
    await writeFile(join(f.root, "sources/src/actual.js"), source);
    await unlink(join(f.root, "sources/src/A.js"));
    await symlink("actual.js", join(f.root, "sources/src/A.js"));
    await assert.rejects(checkCapturedScip(f.root, f.expected), { code: "InvalidAdmission" });
  });
  await withFixture(async (f) => {
    f.manifest.artifacts[0].path = "/tmp/escaped.json";
    await assert.rejects(checkCapturedScip(f.root, await f.save()), { code: "InvalidArtifact" });
  });
});
