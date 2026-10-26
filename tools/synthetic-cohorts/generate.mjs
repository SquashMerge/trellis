import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const seed = "trellis-synthetic-cohorts-v1";
const here = dirname(fileURLToPath(import.meta.url));
const languages = [["java", "java"], ["rust", "rs"], ["python", "py"], ["javascript", "js"]];
const sizes = [["small", 25, 1677722], ["medium", 250, 16777216], ["large", 2500, 134217728]];
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const name = (size, index) => `C${size}${String(index).padStart(4, "0")}`;

function rustRoots(size, ordinary) {
  if (size === "small") return [{ path: `${size}/rust/lib.rs`, text: Array.from({ length: ordinary }, (_, n) => `pub mod ${name(size, n)};`).join("\n") + "\n" }];
  const groups = size === "medium" ? 4 : 39;
  const roots = [{ path: `${size}/rust/lib.rs`, text: Array.from({ length: groups }, (_, g) => `pub mod g${String(g).padStart(2, "0")};`).join("\n") + "\n" }];
  for (let g = 0; g < groups; g++) {
    const first = Math.floor(ordinary * g / groups);
    const last = Math.floor(ordinary * (g + 1) / groups);
    roots.push({ path: `${size}/rust/g${String(g).padStart(2, "0")}/mod.rs`, text: Array.from({ length: last - first }, (_, n) => `pub mod ${name(size, first + n)};`).join("\n") + "\n" });
  }
  return roots;
}

// LARGE spends its byte budget on longer call-bearing bodies, not more calls or declarations.
function fillLarge(body, target, language, index) {
  let remaining = target - Buffer.byteLength(body);
  if (remaining < 2) throw new Error(`Insufficient LARGE body budget: ${language}/${index}: ${remaining}`);
  const marker = language === "rust" ? "local+peer_value+revision+f0()" :
    language === "python" ? "return local+peer_value+revision+f0()" : "return local+peerValue+revision+f0()";
  const parts = body.split(marker);
  const workers = parts.length - 1;
  if (workers !== 36) throw new Error(`Expected 36 LARGE workers: ${language}/${index}: ${workers}`);
  const additions = Array(workers).fill("");
  const offset = parseInt(sha256(`${seed}\0large\0${language}\0${index}`).slice(0, 8), 16) % workers;
  for (let term = 0; remaining > 0; term++) {
    let width = Math.min(7, remaining);
    if (remaining - width === 1) width--;
    const digits = width - 1;
    const minimum = 10 ** (digits - 1);
    const number = minimum + parseInt(sha256(`${seed}\0large\0${language}\0${index}\0term\0${term}`).slice(0, 8), 16) % (9 * minimum);
    additions[(offset + term) % workers] += `+${number}`;
    remaining -= width;
  }
  body = parts.map((part, n) => n === workers ? part : part + marker + additions[n]).join("");
  if (Buffer.byteLength(body) !== target) throw new Error("Source length mismatch");
  return Buffer.from(body, "utf8");
}

// Preserve the original small/medium byte stream, including its seeded body calls.
function fill(body, target, language, size, index) {
  if (size === "large") return fillLarge(body, target, language, index);
  let remaining = target - Buffer.byteLength(body);
  if (remaining < 0) throw new Error(`Source body exceeds target: ${size}/${language}/${index}: ${-remaining}`);
  const marker = language === "rust" ? "local+peer_value+revision+f0()" :
    language === "python" ? "return local+peer_value+revision+f0()" : "return local+peerValue+revision+f0()";
  const parts = body.split(marker);
  if (parts.length < 70) throw new Error(`Missing call-bearing workers: ${size}/${language}/${index}`);
  const workers = parts.length - 1;
  const additions = Array(workers).fill(0);
  // +f0() has five bytes and remains an intra-file, real AST body call.
  const offset = parseInt(sha256(`${seed}\0${size}\0${language}\0${index}`).slice(0, 8), 16) % workers;
  for (let n = offset; remaining >= 5; n = (n + 1) % workers) {
    additions[n] += 5;
    remaining -= 5;
  }
  // Remaining 0..4 bytes become a genuine change to the f0 leaf's result.
  body = parts.map((part, n) => n === workers ? part : part + marker + "+f0()".repeat(additions[n] / 5)).join("");
  if (remaining) {
    const replacement = `${1 + parseInt(sha256(`${seed}\0${size}\0${language}\0${index}\0leaf`).slice(0, 2), 16) % 8}${"0".repeat(remaining)}`;
    const leaf = language === "java" ? "int f0(){return 1;}" :
      language === "rust" ? "fn f0()->i32{1}" :
      language === "python" ? "def f0(): return 1" : "function f0(){return 1;}";
    if (!body.includes(leaf)) throw new Error(`Missing f0 leaf: ${size}/${language}/${index}`);
    body = body.replace(leaf, leaf.replace(/1(?=[^1]*$)/, replacement));
  }
  if (Buffer.byteLength(body) !== target) throw new Error("Source length mismatch");
  return Buffer.from(body, "utf8");
}

function source(size, language, extension, index, group, target) {
  const base = readFileSync(join(here, "templates", size, `${language}.${extension}`), "utf8");
  const peers = Array.from({ length: group.count }, (_, n) => group.first + n)
    .filter((candidate) => candidate !== index)
    .sort((a, b) => sha256(`${seed}\0${size}\0${language}\0${index}\0${a}`).localeCompare(sha256(`${seed}\0${size}\0${language}\0${index}\0${b}`)))
    .slice(0, 2);
  const replacements = [name(size, index), ...peers.map((peer) => name(size, peer))];
  let body = base.replace(new RegExp(`\\bC${size}000[012]\\b`, "g"), (matched) => replacements[Number(matched.at(-1))]);
  if (language === "rust" && group.module) body = body.replaceAll("crate::", `crate::${group.module}::`);
  if (language === "java") body = body.replaceAll("I0", `I${index}`);
  if (language === "java" && !body.includes(`public class ${name(size, index)}`)) throw new Error("Java class template mismatch");
  return fill(body, target, language, size, index);
}

function localGroups(first, last) {
  const groups = [];
  let length = last - first;
  while (length > 0) {
    // A tail of six or seven uses two groups of three or a three and a four.
    const count = length === 6 || length === 7 ? 3 : Math.min(length, 5);
    groups.push({ first, count });
    first += count;
    length -= count;
  }
  return groups;
}

function main(args) {
  if (args.length !== 2 || args[0] !== "--out" || !args[1])
    throw new Error("Usage: node tools/synthetic-cohorts/generate.mjs --out <nonexistent-directory>");
  const out = resolve(args[1]);
  if (existsSync(out)) throw new Error(`Output already exists: ${out}`);
  mkdirSync(out);
  try {
    const files = [];
    const totals = {};
    for (const [size, total, bytes] of sizes) {
      totals[size] = { files: total * languages.length, sourceBytes: bytes };
      for (const [language, extension] of languages) {
        const ordinal = languages.findIndex(([value]) => value === language);
        const languageBytes = Math.floor(bytes / languages.length) + (ordinal < bytes % languages.length ? 1 : 0);
        const count = language === "rust" ? total - (size === "small" ? 1 : size === "medium" ? 5 : 40) : total;
        const roots = language === "rust" ? rustRoots(size, count) : [];
        const rootBytes = roots.reduce((sum, root) => sum + Buffer.byteLength(root.text), 0);
        const ordinaryBytes = languageBytes - rootBytes;
        for (let index = 0; index < count; index++) {
          const rustGroups = size === "medium" ? 4 : 39;
          const groupNumber = language === "rust" && size !== "small" ? Math.floor(((index + 1) * rustGroups - 1) / count) : -1;
          const moduleFirst = groupNumber < 0 ? 0 : Math.floor(count * groupNumber / rustGroups);
          const moduleLast = groupNumber < 0 ? count : Math.floor(count * (groupNumber + 1) / rustGroups);
          const groups = language === "rust" && size === "small" ? Array.from({ length: 8 }, (_, n) => ({ first: n * 3, count: 3 })) :
            localGroups(moduleFirst, moduleLast);
          const selected = groups.find((item) => item.first <= index && index < item.first + item.count);
          if (!selected) throw new Error(`No local peer group: ${size}/${language}/${index}`);
          const group = { ...selected, module: groupNumber < 0 ? null : `g${String(groupNumber).padStart(2, "0")}` };
          const target = Math.floor(ordinaryBytes / count) + (index < ordinaryBytes % count ? 1 : 0);
          const path = `${size}/${language}/${group.module ? `${group.module}/` : ""}${name(size, index)}.${extension}`;
          files.push({ path, content: source(size, language, extension, index, group, target) });
        }
        for (const root of roots) files.push({ path: root.path, content: Buffer.from(root.text) });
      }
    }
    files.sort((a, b) => a.path < b.path ? -1 : a.path > b.path ? 1 : 0);
    const manifest = files.map(({ path, content }) => {
      const filename = join(out, path);
      mkdirSync(dirname(filename), { recursive: true });
      writeFileSync(filename, content, { flag: "wx" });
      return { path, sourceBytes: content.length, sha256: sha256(content) };
    });
    writeFileSync(join(out, "manifest.json"), JSON.stringify({ version: 2, seed, hashAlgorithm: "sha256", totals, files: manifest }, null, 2) + "\n", { flag: "wx" });
  } catch (error) {
    rmSync(out, { recursive: true, force: true });
    throw error;
  }
}
try { main(process.argv.slice(2)); }
catch (error) { console.error(error.message); process.exitCode = 1; }
