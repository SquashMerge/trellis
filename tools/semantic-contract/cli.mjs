#!/usr/bin/env node
import { resolve, join } from "node:path";
import { fileURLToPath } from "node:url";
import { generateFixture, checkPublication } from "./publish.mjs";
import { checkCapturedScip } from "./captured-scip.mjs";

export function parseArgs(args) {
  const [command, ...rest] = args;
  if (command === "check-captured-scip") {
    if (rest.length !== 4 || rest[0] !== "--fixture-root" || rest[2] !== "--expected-manifest-hash" || !rest[1] || !/^[a-f0-9]{64}$/.test(rest[3] ?? ""))
      throw new Error("CLI.FLAG expected --fixture-root <dir> --expected-manifest-hash <sha256>");
    return { command, root: resolve(rest[1]), expectedManifestHash: rest[3] };
  }
  if (!["generate", "check"].includes(command))
    throw new Error("CLI.COMMAND expected generate, check or check-captured-scip");
  let fixture = "example",
    root = fileURLToPath(
      new URL("../../tests/fixtures/semantic-evidence/v1/", import.meta.url),
    ),
    check = false;
  const seen = new Set();
  for (let i = 0; i < rest.length; i++) {
    const flag = rest[i];
    if (
      !["--check", "--fixture", "--fixtures-root"].includes(flag) ||
      seen.has(flag)
    )
      throw new Error(`CLI.FLAG unexpected or duplicate flag ${flag}`);
    seen.add(flag);
    if (flag === "--check") {
      if (command !== "generate")
        throw new Error("CLI.FLAG --check requires generate");
      check = true;
      continue;
    }
    if (++i >= rest.length || rest[i].startsWith("--"))
      throw new Error(`CLI.FLAG ${flag} requires a value`);
    if (flag === "--fixture") fixture = rest[i];
    else root = resolve(rest[i]);
  }
  if (!["example", "java", "rust", "python", "javascript"].includes(fixture))
    throw new Error(`CLI.FIXTURE unknown fixture ${fixture}`);
  return { command, fixture, root, check };
}
export async function main(args = process.argv.slice(2)) {
  const { command, fixture, root, check, expectedManifestHash } = parseArgs(args);
  if (command === "check-captured-scip") return checkCapturedScip(root, expectedManifestHash);
  const path = join(root, fixture);
  return command === "check"
    ? checkPublication(path)
    : generateFixture(path, { check });
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
