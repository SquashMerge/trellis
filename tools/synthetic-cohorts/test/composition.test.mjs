import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";

const here = dirname(fileURLToPath(import.meta.url));
// One helper target per checkout: concurrent runs in different checkouts must
// not rebuild each other's tree-sitter outputs mid-read. CI pins it for caching.
const compositionTarget = process.env.TRELLIS_COMPOSITION_TARGET ??
  join(tmpdir(), `trellis-synthetic-composition-cargo-target-${createHash("sha256").update(here).digest("hex").slice(0, 12)}`);
const generator = join(here, "..", "generate.mjs");
const helper = join(here, "composition", "Cargo.toml");
const policy = join(here, "..", "..", "..", "docs", "semantic-evidence", "cohorts-v1.json");
const pinned = {
  minimumCallableBodiesPerOrdinaryFile: 64,
  maximumCallableBodiesPerOrdinaryFile: 80,
  maximumJavaMethodsPerFile: 72,
  minimumNonLeafCallablePercentPerOrdinaryFile: 90,
  minimumBodyCallsPerNonLeafCallable: 2,
  minimumIntraFileTargetCallsPerNonLeafCallable: 1,
  minimumImportedPeerCallingNonLeafPercentPerOrdinaryFile: 40,
  minimumCrossFileCallingBodiesPerOrdinaryFile: 8,
  exactSameLanguageImportedPeerFilesPerOrdinaryFile: 2,
  minimumOrdinaryFilesPerLocalPeerGroup: 3,
  maximumOrdinaryFilesPerLocalPeerGroup: 5,
  minimumTypedOrdinaryFilePercentPerLanguageCohort: 10,
  minimumMethodBodiesPerTypedFile: 2,
  minimumRelationshipTypedFilePercentPerLanguageCohort: 10,
  minimumInheritanceInterfaceTraitCasesPerLanguageCohort: 1,
  maximumCallFreeBodyStatementBytesPercentPerOrdinaryFile: 15,
  maximumRepeatedCallFreeArithmeticAssignmentCopiesPerCallableBody: 0,
  maximumIdenticalNormalizedNonLeafBodyShapePercentPerOrdinaryFile: 80,
  maximumCommentPaddingPercentPerOrdinaryFile: 10,
  maximumCommentPaddingPercentPerCohort: 10,
  maximumRustModuleRootFiles: { small: 1, medium: 5, large: 40 },
  maximumLargeIndexedDeclarationNodes: 1200000,
  maximumClassDetailRecordsAtLargestIndexScope: 225000,
  largeOrdinaryFileOverrides: {
    minimumCallableBodiesPerOrdinaryFile: 38,
    maximumCallableBodiesPerOrdinaryFile: 40,
    maximumJavaMethodsPerFile: 40,
  },
};
const sample = "small/java/Csmall0000.java";
function checker(root, file) {
  const args = ["run", "--quiet", "--locked", "--manifest-path", helper, "--", root];
  if (file) args.push(file);
  return spawnSync("cargo", args, { encoding: "utf8", timeout: 900000, maxBuffer: 4 * 1024 * 1024, env: { ...process.env, CARGO_TARGET_DIR: compositionTarget } });
}
function outcome(result) { return result.stderr || result.stdout || result.error?.message || ""; }

test("all generated source parses with pinned grammars and meets source-derived composition", { timeout: 1200000 }, () => {
  assert.deepEqual(JSON.parse(readFileSync(policy)).synthetic.minimumComposition, pinned);
  const temp = mkdtempSync(join(tmpdir(), "trellis-cohort-composition-"));
  const root = join(temp, "corpus");
  try {
    let result = spawnSync(process.execPath, [generator, "--out", root], { encoding: "utf8", timeout: 180000 });
    assert.equal(result.status, 0, outcome(result));
    result = checker(root);
    assert.equal(result.status, 0, outcome(result));
    const file = join(root, sample);
    const source = readFileSync(file, "utf8");
    const imported = [...source.matchAll(/^import small\.java\.(Csmall\d{4});$/gm)].map(m => m[1]);
    assert.equal(imported.length, 2);
    const calledPeer = [...source.matchAll(/Csmall\d{4}\.f0\(\)/g)][0]?.[0].split(".")[0];
    assert.ok(imported.includes(calledPeer));
    function rejects(rewrite, expected) {
      try {
        writeFileSync(file, rewrite(source));
        const bad = checker(root, sample);
        assert.notEqual(bad.status, 0, `mutation escaped AST check: ${expected}`);
        assert.match(outcome(bad), expected);
      } finally { writeFileSync(file, source); }
      const restored = checker(root, sample);
      assert.equal(restored.status, 0, outcome(restored));
    }
    // Source tokens inside comments must not count as executable call expressions.
    rejects(s => s.replaceAll(/Csmall\d{4}\.f0\(\)/g, "1/*Csmall0002.f0()*/"), /cross-file calling|non-leaf/);
    rejects(s => s.replaceAll(/Csmall\d{4}\.f0\(\)/g, "1").replace("public class Csmall0000 implements I0{", `public class Csmall0000 implements I0{String spoof="${calledPeer}.f0()";`), /cross-file calling|non-leaf/);
    rejects(s => s.replace(`import small.java.${imported[0]};\n`, ""), /exact two AST peer imports/);
    const unusedInGroup = [0, 1, 2, 3, 4].map(i => `Csmall${String(i).padStart(4, "0")}`).find(name => name !== "Csmall0000" && !imported.includes(name));
    assert.ok(unusedInGroup);
    rejects(s => s.replace(`import small.java.${imported[0]};`, `import small.java.${imported[0]};\nimport small.java.${unusedInGroup};`), /exact two AST peer imports/);
    rejects(s => s.replace(`import small.java.${imported[0]};`, "import small.java.Csmall9999;").replaceAll(`${imported[0]}.f0()`, "Csmall9999.f0()"), /peer file is missing/);
    rejects(s => s.replace(`import small.java.${imported[0]};`, "import small.java.Csmall0005;").replaceAll(`${imported[0]}.f0()`, "Csmall0005.f0()"), /connected peer group/);
    rejects(s => s.replace(`f0()+${calledPeer}.f0()`, `${calledPeer}.f0()+${calledPeer}.f0()`), /intra-file f0 call/);
    // A call token in a leaf changes the AST owner to non-leaf and cannot pass as a leaf exception.
    rejects(s => s.replace("f0(){return ", "f0(){return f0()+"), /non-leaf has fewer than 2 owned AST calls/);

    const workers = [...source.matchAll(/public static int f[1-9]\d*\([^)]*\)\{/g)];
    assert.ok(workers.length >= 60, "enough concrete worker bodies for quota boundary");
    // Java has one method m plus each worker as non-leaf; f0 is the only leaf.
    const nonleaf = workers.length + 1;
    const importedFloor = Math.ceil(nonleaf * 40 / 100);
    const remove = nonleaf - importedFloor;
    function dropPeerCalls(count) {
      let changed = source;
      // Walk complete method bodies. Reverse order keeps earlier source offsets stable.
      for (const match of workers.slice(0, count).reverse()) {
        const start = match.index + match[0].length - 1;
        let depth = 0;
        let end = start;
        for (; end < source.length; end++) {
          if (source[end] === "{") depth++;
          if (source[end] === "}" && --depth === 0) { end++; break; }
        }
        assert.ok(end <= source.length && depth === 0);
        changed = changed.slice(0, start) + changed.slice(start, end).replaceAll(/Csmall\d{4}\.f0\(\)/g, "f0()") + changed.slice(end);
      }
      return changed;
    }
    try {
      writeFileSync(file, dropPeerCalls(remove));
      const boundary = checker(root, sample);
      assert.equal(boundary.status, 0, `40% inclusive boundary: ${outcome(boundary)}`);
      writeFileSync(file, dropPeerCalls(remove + 1));
      const below = checker(root, sample);
      assert.notEqual(below.status, 0, "39%-side quota mutation must fail");
      assert.match(outcome(below), /cross-file calling owners/);
    } finally { writeFileSync(file, source); }

    // The import name alone is insufficient: its peer's parsed public leaf must exist.
    const peerFile = join(root, "small", "java", `${calledPeer}.java`);
    const peerSource = readFileSync(peerFile, "utf8");
    try {
      writeFileSync(peerFile, peerSource.replace("public static int f0()", "static int f0()"));
      const bad = checker(root, sample);
      assert.notEqual(bad.status, 0);
      assert.match(outcome(bad), /lacks exported leaf f0/);
    } finally { writeFileSync(peerFile, peerSource); }
    result = checker(root, sample);
    assert.equal(result.status, 0, outcome(result));

    // LARGE has a separate body-density contract; the other composition rules still apply.
    const largeSample = "large/java/Clarge0000.java";
    const largeFile = join(root, largeSample);
    const largeSource = readFileSync(largeFile, "utf8");
    const lastWorker = largeSource.match(/public static int f36\(int revision\)\{[\s\S]*?\n\}/)?.[0];
    assert.ok(lastWorker);
    try {
      writeFileSync(largeFile, largeSource.replace(lastWorker, ""));
      const below = checker(root, largeSample);
      assert.notEqual(below.status, 0, "LARGE callable floor must apply");
      assert.match(outcome(below), /callable bodies/);
      const extraWorkers = lastWorker.replace("f36(", "f37(") + "\n" + lastWorker.replace("f36(", "f38(");
      writeFileSync(largeFile, largeSource.slice(0, -2) + extraWorkers + "\n}\n");
      const above = checker(root, largeSample);
      assert.notEqual(above.status, 0, "LARGE Java signature ceiling must apply");
      assert.match(outcome(above), /Java method signatures/);
    } finally { writeFileSync(largeFile, largeSource); }
  } finally { rmSync(temp, { recursive: true, force: true }); }
});
