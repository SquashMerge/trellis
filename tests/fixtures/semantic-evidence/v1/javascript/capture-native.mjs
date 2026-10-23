#!/usr/bin/env node
// Fixture-local syntax capture. No compiler program, config, resolution or type checker.
import { readFile, readdir, writeFile } from "node:fs/promises";
import { resolve, relative, join, isAbsolute, sep } from "node:path";
import { createRequire } from "node:module";
import { TextDecoder } from "node:util";

function options(args) {
  const allowed = new Set([
    "--typescript", "--snapshot-root", "--source-set", "--revision", "--output",
  ]);
  const result = {};
  for (let i = 0; i < args.length; i += 2) {
    const key = args[i];
    if (!allowed.has(key) || !args[i + 1] || Object.hasOwn(result, key))
      throw new Error("Expected unique --typescript, --snapshot-root, --source-set, --revision and --output arguments");
    result[key] = args[i + 1];
  }
  if (Object.keys(result).length !== allowed.size)
    throw new Error("Missing required capture argument");
  if (!isAbsolute(result["--typescript"]))
    throw new Error("--typescript must be an explicit absolute parser path");
  return result;
}

async function sourcePaths(directory) {
  const paths = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) paths.push(...await sourcePaths(path));
    else if (entry.isFile() && entry.name.endsWith(".js")) paths.push(path);
    else if (entry.isSymbolicLink()) throw new Error(`Unsupported snapshot symlink: ${path}`);
  }
  return paths.sort((a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b)));
}

function captureDocument(ts, text, path, sourceSetId, revisionId, artifact) {
  const source = ts.createSourceFile(path, text, ts.ScriptTarget.Latest, true, ts.ScriptKind.JS);
  if (source.parseDiagnostics.length) {
    const diagnostic = source.parseDiagnostics[0];
    throw new Error(`${path}:${diagnostic.start}: ${ts.flattenDiagnosticMessageText(diagnostic.messageText, " ")}`);
  }
  const document = { sourceSetId, language: "javascript", path };
  const start = (node) => node.getStart(source);
  const range = (node) => ({ encoding: "utf16", start: start(node), end: node.end });
  const spelling = (node) => text.slice(start(node), node.end);
  const ref = (category, node) => `${revisionId}:${path}:${category}:${ts.SyntaxKind[node.kind]}:${start(node)}:${node.end}`;
  const witness = (field, node) => ({ field, witness: { range: range(node), text: spelling(node) } });
  const common = (category, node) => ({ ref: ref(category, node), nativeId: null, document, revisionId });
  const unsupported = (node, reason) => {
    throw new Error(`${path}:${start(node)}: unsupported fixture syntax: ${reason}`);
  };
  const callableKinds = new Set(["function", "method", "constructor", "anonymousFunction"]);
  const declarationNodes = new Map();
  const emptyHeader = (kind, name) => ({ kind, name, modifiers: [], typeParameters: [], parameters: [], resultType: null, bases: [] });
  const module = {
    ...common("declaration", source), role: "production", parentRef: null, kind: "module", name: null,
    range: { encoding: "utf16", start: 0, end: text.length }, nameRange: null,
    header: emptyHeader("module", null), signature: null, witnesses: [],
  };
  artifact.declarations.push(module);
  declarationNodes.set(source, module);

  function declarationKind(node) {
    if (ts.isFunctionDeclaration(node) || ts.isFunctionExpression(node)) return node.name ? "function" : "anonymousFunction";
    if (ts.isArrowFunction(node)) return "anonymousFunction";
    if (ts.isClassDeclaration(node) || ts.isClassExpression(node)) return "type";
    if (ts.isMethodDeclaration(node) || ts.isGetAccessorDeclaration(node) || ts.isSetAccessorDeclaration(node)) return "method";
    if (ts.isConstructorDeclaration(node)) return "constructor";
    if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name)) return "variable";
    if (ts.isImportSpecifier(node) || ts.isNamespaceImport(node) || ts.isExportSpecifier(node)) return "alias";
    if (ts.isImportClause(node) && node.name) return "alias";
    return null;
  }
  function declarations(node, parent) {
    if (node.type || node.typeParameters?.length || node.typeArguments?.length ||
        (ts.isTypeNode(node) && !ts.isExpressionWithTypeArguments(node)))
      unsupported(node, "TypeScript type syntax");
    if (ts.canHaveDecorators(node) && ts.getDecorators(node)?.length) unsupported(node, "decorators");
    const kind = declarationKind(node);
    let owner = parent;
    if (kind) {
      if (kind === "type" && !node.name) unsupported(node, "anonymous classes");
      const constructorName = kind === "constructor" ? node.getFirstToken(source) : null;
      const nameNode = constructorName ?? node.name ?? null;
      if (nameNode && !ts.isIdentifier(nameNode) && kind !== "constructor")
        unsupported(node, "non-identifier declaration names");
      const name = nameNode ? spelling(nameNode) : null;
      const header = emptyHeader(kind, name);
      const witnesses = nameNode ? [witness("name", nameNode), witness("header.name", nameNode)] : [];
      for (const modifier of node.modifiers ?? []) {
        witnesses.push(witness(`header.modifiers[${header.modifiers.length}]`, modifier));
        header.modifiers.push(spelling(modifier));
      }
      for (const parameter of node.parameters ?? []) {
        if (!ts.isIdentifier(parameter.name)) unsupported(parameter, "destructured parameters");
        witnesses.push(witness(`header.parameters[${header.parameters.length}].name`, parameter.name));
        header.parameters.push({ name: spelling(parameter.name), type: null, variadic: Boolean(parameter.dotDotDotToken) });
      }
      for (const clause of node.heritageClauses ?? []) {
        if (clause.token !== ts.SyntaxKind.ExtendsKeyword) unsupported(clause, "non-extends heritage");
        for (const base of clause.types) {
          witnesses.push(witness(`header.bases[${header.bases.length}]`, base.expression));
          header.bases.push(spelling(base.expression));
        }
      }
      const measuredNode = kind === "alias" ? nameNode : node;
      const row = {
        ...common("declaration", measuredNode), role: "production", parentRef: parent.ref, kind, name,
        range: range(measuredNode), nameRange: nameNode ? range(nameNode) : null,
        header, signature: null, witnesses,
      };
      artifact.declarations.push(row);
      declarationNodes.set(node, row);
      // Alias ranges are their own binding tokens, not containers for sibling imports.
      if (kind !== "alias") owner = row;
    }
    ts.forEachChild(node, (child) => declarations(child, owner));
  }
  ts.forEachChild(source, (node) => declarations(node, module));

  const localControls = [];
  const localCalls = [];
  function addControl(node, owner, kind, arm = null) {
    const row = { ...common("control", node), ownerRef: owner.ref, parentRef: null, kind, range: range(node), arm, witnesses: [] };
    localControls.push(row);
  }
  function isDeclarationName(node) {
    const parent = node.parent;
    if (!parent) return false;
    if (parent.name === node && (
      declarationNodes.has(parent) || ts.isVariableDeclaration(parent) || ts.isParameter(parent) ||
      ts.isBindingElement(parent) || ts.isPropertyDeclaration(parent) || ts.isPropertyAssignment(parent) ||
      ts.isImportClause(parent) || ts.isImportSpecifier(parent) || ts.isNamespaceImport(parent)
    )) return true;
    if (ts.isImportSpecifier(parent) || ts.isExportSpecifier(parent)) return true;
    if (ts.isBindingElement(parent) && parent.propertyName === node) return true;
    if ((ts.isLabeledStatement(parent) || ts.isBreakStatement(parent) || ts.isContinueStatement(parent)) && parent.label === node) return true;
    return false;
  }
  function occurrences(node, callable) {
    const declaration = declarationNodes.get(node);
    if (declaration && callableKinds.has(declaration.kind)) callable = declaration;
    if (ts.isCallExpression(node) || ts.isNewExpression(node)) {
      const expression = node.expression;
      const computed = ts.isElementAccessExpression(expression);
      const callee = ts.isPropertyAccessExpression(expression) ? expression.name : expression;
      localCalls.push({
        ...common("call", node), ownerRef: callable.ref, range: range(node),
        calleeRange: computed ? null : range(callee), spelling: computed ? null : spelling(callee),
        regionRefs: [], witnesses: computed ? [] : [witness("spelling", callee)],
      });
    }
    if (ts.isIfStatement(node)) addControl(node, callable, "if");
    else if (ts.isForStatement(node) || ts.isForInStatement(node) || ts.isForOfStatement(node) || ts.isWhileStatement(node) || ts.isDoStatement(node)) addControl(node, callable, "loop");
    else if (ts.isConditionalExpression(node)) addControl(node, callable, "conditional");
    else if (ts.isTryStatement(node)) {
      addControl(node, callable, "try");
      if (node.finallyBlock) addControl(node.finallyBlock, callable, "finally");
    } else if (ts.isCatchClause(node)) addControl(node, callable, "catch");
    else if (ts.isSwitchStatement(node)) addControl(node, callable, "switch");
    else if (ts.isCaseClause(node)) addControl(node, callable, "case");
    else if (ts.isDefaultClause(node)) addControl(node, callable, "default");
    const aliasName = declarationNodes.get(node.parent)?.kind === "alias" && node.parent.name === node;
    if ((ts.isIdentifier(node) || ts.isPrivateIdentifier(node)) && (aliasName || !isDeclarationName(node))) {
      artifact.references.push({
        ...common("reference", node), ownerRef: callable.ref, range: range(node), spelling: spelling(node),
        witnesses: [witness("spelling", node)],
      });
    }
    ts.forEachChild(node, (child) => occurrences(child, callable));
  }
  occurrences(source, module);
  const contains = (outer, inner) => outer.start <= inner.start && inner.end <= outer.end;
  const order = (a, b) => a.range.start - b.range.start || a.range.end - b.range.end;
  const containing = (row) => localControls.filter((control) => control !== row && control.ownerRef === row.ownerRef && contains(control.range, row.range));
  for (const control of localControls) {
    const parents = containing(control).sort((a, b) => (a.range.end - a.range.start) - (b.range.end - b.range.start));
    control.parentRef = parents[0]?.ref ?? null;
  }
  for (const call of localCalls) {
    call.regionRefs = containing(call)
      .sort((a, b) => (b.range.end - b.range.start) - (a.range.end - a.range.start))
      .map((control) => control.ref);
  }
  artifact.calls.push(...localCalls.sort(order));
  artifact.controls.push(...localControls.sort(order));
}

async function main() {
  const args = options(process.argv.slice(2));
  const ts = createRequire(import.meta.url)(args["--typescript"]);
  const snapshotRoot = resolve(args["--snapshot-root"]);
  const output = resolve(args["--output"]);
  const paths = await sourcePaths(snapshotRoot);
  if (!paths.length) throw new Error("Snapshot root contains no .js source documents");
  if (paths.includes(output)) throw new Error("Output must not overwrite source");
  const artifact = { formatVersion: 1, producerId: "native", extractionInputs: [], declarations: [], calls: [], controls: [], references: [] };
  for (const path of paths) {
    const text = new TextDecoder("utf-8", { fatal: true }).decode(await readFile(path));
    captureDocument(ts, text, relative(snapshotRoot, path).split(sep).join("/"), args["--source-set"], args["--revision"], artifact);
  }
  await writeFile(output, JSON.stringify(artifact, null, 2) + "\n", { flag: "wx" });
  process.stdout.write(`Captured ${paths.length} documents to ${output}\n`);
}
main().catch((error) => {
  process.stderr.write(`${error.stack ?? error.message}\n`);
  process.exitCode = 1;
});
