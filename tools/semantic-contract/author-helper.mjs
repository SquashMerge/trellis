#!/usr/bin/env node
// Print source-backed authoring values; never publish or author expected answers.
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { confinedFile } from "./load.mjs";
import { canonicalBytes, parseJson } from "./json.mjs";
import { validate } from "./formats.mjs";
import { assignKeys, normalizeOccurrences } from "./normalize.mjs";
import {
  contentHash,
  sourceManifestHash,
  headerHash,
  siblingGroupHash,
} from "./identity.mjs";

const defaultRoot = fileURLToPath(
  new URL(
    "../../tests/fixtures/semantic-evidence/v1/example/",
    import.meta.url,
  ),
);
const tuple = (set, revision, path) => JSON.stringify([set, revision, path]);
const same = (a, b) => canonicalBytes(a).equals(canonicalBytes(b));

export function parseArgs(args) {
  if (
    args.length < 1 ||
    args.length > 2 ||
    !["ids", "hashes"].includes(args[0])
  ) {
    throw new Error("AUTHOR.ARGS expected ids|hashes [fixture-directory]");
  }
  return { command: args[0], root: resolve(args[1] ?? defaultRoot) };
}

export async function authorValues(root, command) {
  if (!["ids", "hashes"].includes(command))
    throw new Error("AUTHOR.ARGS expected ids|hashes");
  const fixture = parseJson(await confinedFile(root, "fixture.json"));
  validate("FixtureV1", fixture);
  const sources = new Map();
  const revisions = new Map();
  const snapshots = [];
  for (const revision of fixture.revisions) {
    const documents = [];
    for (const document of revision.documents) {
      const bytes = await confinedFile(root, document.sourceFile);
      const value = {
        key: document.key,
        revisionId: document.revisionId,
        contentHash: contentHash(bytes),
        byteLength: bytes.length,
      };
      documents.push(value);
      sources.set(
        tuple(revision.sourceSetId, revision.id, document.key.path),
        bytes,
      );
    }
    revisions.set(JSON.stringify([revision.sourceSetId, revision.id]), {
      ...revision,
      documents,
    });
    snapshots.push({
      sourceSetId: revision.sourceSetId,
      revisionId: revision.id,
      sourceManifestHash: sourceManifestHash(
        documents.map((x) => ({ document: x.key, contentHash: x.contentHash })),
      ),
      documents: revision.documents.map((x, i) => ({
        sourceFile: x.sourceFile,
        ...documents[i],
      })),
    });
  }
  if (command === "hashes") {
    const captures = [];
    for (const capture of fixture.captures) {
      if (capture.pinned !== undefined) continue; // attested by profile, no bytes
      const actualHash = contentHash(await confinedFile(root, capture.file));
      captures.push({
        ref: capture.ref,
        kind: capture.kind,
        file: capture.file,
        declaredHash: capture.hash,
        actualHash,
        matches: capture.hash === actualHash,
      });
    }
    return { snapshots, captures };
  }
  const native = parseJson(await confinedFile(root, fixture.nativeArtifact));
  validate("NativeArtifact", native);
  const loaded = { fixture, native, sources, revisions };
  const keys = assignKeys(loaded);
  const occurrences = normalizeOccurrences(loaded, keys);
  const declarations = fixture.revisions.flatMap((revision) =>
    native.declarations
      .filter(
        (x) =>
          x.document.sourceSetId === revision.sourceSetId &&
          x.revisionId === revision.id,
      )
      .map((x) => {
        const record = keys.records.find(
          (y) =>
            y.syntaxId === keys.ids.get(x.ref) &&
            y.revisionId === x.revisionId &&
            same(y.document, x.document),
        );
        const siblings = keys.records
          .filter(
            (y) =>
              y.revisionId === record.revisionId &&
              same(y.document, record.document) &&
              same(y.ancestors, record.ancestors) &&
              y.key.kind === record.key.kind &&
              y.key.name === record.key.name &&
              same(y.key.signature, record.key.signature),
          )
          .sort(
            (a, b) =>
              a.range.start - b.range.start || a.range.end - b.range.end,
          );
        const header = headerHash(record.header);
        const headers = siblings.map((y) => headerHash(y.header));
        return {
          ref: x.ref,
          revisionId: x.revisionId,
          document: x.document,
          syntaxId: keys.ids.get(x.ref),
          key: record.key,
          ancestors: record.ancestors,
          headerHash: header,
          siblingGroupHash: siblingGroupHash(headers),
          siblingCount: siblings.length,
          identicalHeaderCount: headers.filter((y) => y === header).length,
        };
      }),
  );
  const occurrencesByKind = Object.fromEntries(
    ["calls", "controls", "references"].map((kind) => [
      kind,
      native[kind].map((x) => ({
        ref: x.ref,
        revisionId: x.revisionId,
        ownerRef: x.ownerRef,
        ordinal:
          kind === "calls"
            ? occurrences.calls.find((y) => y.id === occurrences.ids.get(x.ref))
                ?.ordinal
            : kind === "controls"
              ? occurrences.controlRegions.find(
                  (y) => y.id === occurrences.ids.get(x.ref),
                )?.ordinal
              : occurrences.referenceRows.find(
                  (y) => y.id === occurrences.ids.get(x.ref),
                )?.ordinal,
        occurrenceId: occurrences.ids.get(x.ref),
      })),
    ]),
  );
  return { declarations, ...occurrencesByKind };
}

export async function main(
  args = process.argv.slice(2),
  output = process.stdout,
) {
  const { command, root } = parseArgs(args);
  const values = await authorValues(root, command);
  output.write(`${JSON.stringify(values, null, 2)}\n`);
  return values;
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  main().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
