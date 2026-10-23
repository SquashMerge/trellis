/**
 * Closed v1 policy/cohort/hardware schema: EXPECTED below pins every required field,
 * ordered array entry, explicit null and literal. Objects may reorder their keys.
 * Pct is 0–100 percent; PercentagePoints is an absolute point difference. Ms,
 * Seconds, MiB and GiB are milliseconds, seconds and binary memory units;
 * hardware GB/TB are advertised decimal units, distinct from observed GiB.
 * Multiplier is dimensionless. Counts are nonnegative safe integers; the only
 * fractional literals are Wilson z and the mixed-load publisher multiplier.
 * Versions, dates, revisions and HTTP policy descriptions are strings, except
 * browserBusyStatusForbidden, an integer status code.
 *
 * The default depth 2/150 nodes/500 calls comes from contract-v1.md. Wilson's
 * two-sided 95% lower endpoint uses p=successes/n, precision TP/(TP+FP), and
 * unique supported oracle facts for recall (type/possible-dispatch uses its
 * supported subset). Both independent 1000-case oracle populations and each
 * metric denominator must reach 1000 per language/capability; otherwise the
 * result is notMeasured/null/nonpassing. Invalid counts are invalid input.
 * The 100% conformance, preservation and direct-dispatch zero-error rules are
 * exact, not Wilson: even 1000/1000 has lower endpoint about 99.6173%.
 * Conformance's 32 scenarios are not the real-producer 1000/1000 population.
 * Unsupported/ambiguous/unresolved/dynamic dispositions remain in full oracle
 * reports, with support labels fixed before producer output. Static execution
 * restrictions supplement #22 rather than replacing its freshness/provenance.
 * Relative regression uses adverse (current-baseline)/baseline for costs,
 * (baseline-current)/baseline for throughput, and baseline-current percentage
 * points for quality; equality does not fail, zero-to-positive counts always
 * fail, and absolute limits always gate. A zero non-count baseline uses the
 * pinned conservative policy, without division. The first fully passing run
 * alone establishes semantic-v1; no later run refreshes it. These are pinned
 * normative descriptions, not an executable benchmark, result gate or baseline.
 * Exclusions:null means unrecorded, not empty/zero.
 * Hash field names are future binding requirements, not recorded hash evidence.
 * This checker validates policy consistency, never benchmark qualification.
 */
import fs from "node:fs";
import assert from "node:assert/strict";

// Hand-pinned from the approved brief, independent of files loaded at runtime.
const EXPECTED = {
  'benchmark-policy-v1.json': {
  "version": 1,
  "status": "ratifiedPolicyNotBenchmarkEvidence",
  "approval": {
    "approvedOn": "2026-09-22",
    "amendmentsApprovedOn": "2026-09-23",
    "amendments": ["A1", "A2"],
    "contractReviewer": "@jasoncarreira",
    "finalApprover": "@jasoncarreira",
    "benchmarkDecisionOwner": "@jasoncarreira",
    "independentTechnicalCheck": "Feature Factory work-reviewer",
    "boundArtifactHashes": ["contract", "corpus", "policy", "baseline"],
    "hashRecordLocations": ["stage1PullRequest", "decisionRegister"]
  },
  "quality": {
    "languages": ["java", "rust", "python", "javascript"],
    "conformancePassPct": 100,
    "preservationPct": 100,
    "preservedProperties": ["ranges", "hashes", "IDs", "owners", "ordinals", "controlRegions", "callbackBoundaries", "expectedCoverageRows"],
    "identicalImports": 3,
    "normalizedOutput": "deterministic",
    "acceptedStaleFactsMax": 0,
    "acceptedMismatchedFactsMax": 0,
    "staticRuntimeObservedClaimsMax": 0,
    "falseDirectExecutionPromotionsMax": 0,
    "nonExecutableEvidence": ["ambiguous", "unresolved", "dynamic", "external", "declarationOnly"],
    "directDispatchPrecisionPct": 100,
    "directDispatchGate": "empiricalPrecisionAndZeroFalsePromotions",
    "insufficientRequiredPopulationResult": {"status": "notMeasured", "value": null, "passes": false},
    "realProducerQualification": {
      "interval": "wilsonScoreTwoSidedLowerBound",
      "confidencePct": 95,
      "z": 1.959963984540054,
      "lowerBoundFormula": "(p+z*z/(2*n)-z*sqrt(p*(1-p)/n+z*z/(4*n*n)))/(1+z*z/n)",
      "pDefinition": "successes/denominator",
      "minimumPositiveOracleCases": 1000,
      "minimumHardNegativeOracleCases": 1000,
      "minimumMetricDenominator": 1000,
      "minimumScope": "perLanguagePerMeasuredCapability",
      "undersizedResult": "notMeasured",
      "notMeasuredValue": null,
      "notMeasuredPasses": false,
      "comparison": "unroundedLowerBoundTimes100GteFloor",
      "floorsPct": {
        "java": {"exactBindingReferencePrecision": 99, "supportedFactRecall": 95, "typePossibleDispatchRecall": 95},
        "rust": {"exactBindingReferencePrecision": 99, "supportedFactRecall": 90, "typePossibleDispatchRecall": 90},
        "python": {"exactBindingReferencePrecision": 99, "supportedFactRecall": 85, "typePossibleDispatchRecall": 80},
        "javascript": {"exactBindingReferencePrecision": 99, "supportedFactRecall": 95, "typePossibleDispatchRecall": 90}
      }
    },
    "recallPopulation": "oracleFactsDeclaredSupportedByPinnedProfile",
    "dispositionReportPopulation": "fullOracleDenominator",
    "separateDispositions": ["unsupported", "ambiguous", "unresolved", "dynamic"],
    "relabelAfterProducerOutputAllowed": false
  },
  "performance": {
    "limitComparison": "lte",
    "defaultBoundedQuery": {"depth": 2, "maxNodes": 150, "maxCalls": 500, "p95Ms": 100},
    "maximumBoundedQuery": {"depth": 5, "maxNodes": 150, "maxCalls": 500, "p95Ms": 250, "p99Ms": 750},
    "symbolSubstringSearch": {"p95Ms": 250},
    "status": {"p95Ms": 25},
    "sourceByKey": {"p95Ms": 25},
    "fullIndex": {
      "includedStages": ["nativeIndex", "semanticImport", "sqlitePublication"],
      "producerGenerationIncluded": false,
      "small": {"medianSeconds": 2, "rssMiB": 512},
      "medium": {"medianSeconds": 30, "rssGiB": 2},
      "large": {"medianSeconds": 180, "rssGiB": 8}
    },
    "incrementalUpdate": {"scope": "trueSingleFile", "mediumP95Seconds": 2, "largeP95Seconds": 5},
    "storage": {
      "steadyToCanonicalSerializedEvidenceMaxMultiplier": 3,
      "steadyMaxGiB": 4,
      "steadyLimitsCombinedBy": "and",
      "publicationPeakToSteadyMaxMultiplier": 2,
      "publicationPeakIncludes": ["database", "WAL", "SHM"]
    },
    "mixedLoad": {
      "concurrentMcpReaderProcesses": 8,
      "concurrentPublishers": ["nativeRefresh", "explicitIndex"],
      "mixedRevisionsMax": 0,
      "partialPublishesMax": 0,
      "unexpectedToolErrorsMax": 0,
      "unexpectedProtocolErrorsMax": 0,
      "lostUpdatesMax": 0,
      "queryP95Ms": 500,
      "queryP99Seconds": 1,
      "publisherWallToSoloMedianMaxMultiplier": 1.25,
      "explicitRequestWhileJobRunning": {
        "responses": ["accepted", "queued"],
        "responseMaxMs": 100,
        "watcherLeadershipMayBlock": false,
        "browserDaemonIndexRoute": "queue",
        "browserBusyStatusForbidden": 409
      }
    }
  },
  "regressions": {
    "absoluteLimitsAlwaysGate": true,
    "relativeFailureComparison": "gt",
    "queryTimeIncreasePct": 15,
    "indexTimeIncreasePct": 15,
    "rssIncreasePct": 10,
    "storageIncreasePct": 10,
    "concurrencyThroughputDecreasePct": 10,
    "qualityDecreasePercentagePoints": 1,
    "zeroBaselineCounts": {"correctness": 0, "privacy": 0, "falsePromotion": 0},
    "zeroToPositiveAlwaysFails": true,
    "nonCountZeroBaselinePolicy": "zeroUnchangedIsNoRegressionAdverseChangeFailsBeneficialChangeDoesNotFail",
    "comparisonBasis": "sameMetricCohortAndCompleteHardwareProfile"
  },
  "baseline": {
    "name": "semantic-v1",
    "establishment": "firstFullyPassingBenchmark",
    "immutableAfterEstablishment": true,
    "failedRunMayEstablish": false,
    "failedRunMayRefresh": false,
    "laterPassingRunMayRefresh": false,
    "requiredMeasurementMayBeNotMeasured": false,
    "completeObservedHardwareRequired": true,
    "allSemanticProducerVersionsRequired": true,
    "frozenCohortManifestRequired": true,
    "bindToCompleteHardwareProfile": true,
    "bindToApprovalArtifactHashes": true
  },
  "referenceHost": {
    "profileId": "mac-mini-m5-pro-v1",
    "location": "ownerLocal",
    "paidCloudAuthorized": false,
    "githubActionsRole": "correctnessOnly",
    "githubActionsAuthoritativePerformanceHost": false,
    "missingInventoryBlocksImplementation": false,
    "completeInventoryRequiredBefore": ["performanceBaselineAcceptance", "performanceThresholdAcceptance", "authoritativeBenchmarkAcceptance", "releaseBaselineAcceptance"]
  }
},
  'cohorts-v1.json': {
  "version": 1,
  "status": "requirementsNotMeasuredCorpus",
  "languages": ["java", "rust", "python", "javascript"],
  "conformance": {
    "minimumScenariosPerLanguage": 32,
    "minimumScenariosPerCategoryPerLanguage": 4,
    "categories": ["sameNameOverload", "importsAliases", "callableValues", "recursion", "relationshipsDispatch", "unicodeCoordinates", "coverageFreshness", "compatibilityControl"],
    "minimumMeasuredCallAnchorsPerLanguage": 120,
    "minimumReferenceRoleFactsPerLanguage": 40,
    "referenceCountingUnit": "distinctReferenceRecordNotRole",
    "allApplicableReferenceRolesRequired": true,
    "nonApplicableReferenceRoles": {"java": ["alias"], "rust": [], "python": [], "javascript": []},
    "minimumCallableValueNegativesPerLanguage": 20,
    "minimumApplicableTypeRelationshipsPerLanguage": 20,
    "minimumOutcomesPerLanguage": {"resolved": 20, "external": 20, "ambiguous": 20, "unresolved": 20, "unsupported": 20},
    "externalOutcomeRequires": "provenExternal",
    "anchorsIndependentlyAuthored": true,
    "expectedAnswers": "handAuthoredIndependentlyOfEvaluator"
  },
  "realProjects": {
    "selection": "allTrackedFilesOfRelevantLanguage",
    "exclusionKinds": ["generated", "vendor"],
    "exclusionsRecordedIn": "cohortManifest",
    "manifestFrozenBeforeProducerOutput": true,
    "projects": [
      {"language": "java", "repository": "junit-team/junit4", "revision": "890f3c972647de378f25e7271d8fbbd9d3456b79", "exclusionInventoryStatus": "notRecorded", "exclusions": null},
      {"language": "java", "repository": "google/gson", "revision": "854c8255b625cf1e13c701a83ea9ccb4caaa576a", "exclusionInventoryStatus": "notRecorded", "exclusions": null},
      {"language": "java", "repository": "google/guava", "revision": "2a11c2a311cfa400ceda7b847b20f0742bf6748f", "exclusionInventoryStatus": "notRecorded", "exclusions": null},
      {"language": "javascript", "repository": "sindresorhus/p-limit", "revision": "a8a6fbec4e0e866d6d779b10889bb4f5567e70eb", "exclusionInventoryStatus": "notRecorded", "exclusions": null},
      {"language": "javascript", "repository": "expressjs/express", "revision": "9a34acf03cb818ff3f8bc40e44176e277a25cbb9", "exclusionInventoryStatus": "notRecorded", "exclusions": null},
      {"language": "javascript", "repository": "eslint/eslint", "revision": "3d2e7cedb7409d8a2c5f2c2fafb14fa22790e40e", "exclusionInventoryStatus": "notRecorded", "exclusions": null},
      {"language": "rust", "repository": "dtolnay/anyhow", "revision": "c63b279f3f4af2b02ca6267d9eb47d6d10497f69", "exclusionInventoryStatus": "notRecorded", "exclusions": null},
      {"language": "rust", "repository": "BurntSushi/ripgrep", "revision": "3fce3b5bb0236da2df6d99672afb8a719642eca7", "exclusionInventoryStatus": "notRecorded", "exclusions": null},
      {"language": "rust", "repository": "tokio-rs/tokio", "revision": "5d5ca1181e3f17d56e03eac9756fa78dba399a39", "exclusionInventoryStatus": "notRecorded", "exclusions": null},
      {"language": "python", "repository": "psf/requests", "revision": "611c6162cbc4ac2020a2f91c7cfa4f3abf9bbb60", "exclusionInventoryStatus": "notRecorded", "exclusions": null},
      {"language": "python", "repository": "pallets/flask", "revision": "d73fa1cdcbd8b1465c151db8924ba58b1dd14e35", "exclusionInventoryStatus": "notRecorded", "exclusions": null},
      {"language": "python", "repository": "django/django", "revision": "dd6f6b1531984823e3dc56740dfa93f3ceb09357", "exclusionInventoryStatus": "notRecorded", "exclusions": null}
    ]
  },
  "synthetic": {
    "small": {"files": 100, "sourceBytes": 1677722, "minimumNormalizedFacts": 5000, "factsScope": "perLanguage"},
    "medium": {"files": 1000, "sourceMiB": 16, "minimumNormalizedFacts": 50000, "factsScope": "perLanguage"},
    "large": {"files": 10000, "sourceMiB": 128, "minimumNormalizedFacts": 500000, "factsScope": "overall"},
    "atCap": {"files": 100000, "sourceMiB": 256, "purpose": "boundedFailureSafetyLimits", "sloApplies": false},
    "minimumComposition": {
      "minimumCallableBodiesPerOrdinaryFile": 64,
      "maximumCallableBodiesPerOrdinaryFile": 80,
      "maximumJavaMethodsPerFile": 72,
      "minimumNonLeafCallablePercentPerOrdinaryFile": 90,
      "minimumBodyCallsPerNonLeafCallable": 2,
      "minimumIntraFileTargetCallsPerNonLeafCallable": 1,
      "minimumImportedPeerCallingNonLeafPercentPerOrdinaryFile": 40,
      "minimumCrossFileCallingBodiesPerOrdinaryFile": 8,
      "exactSameLanguageImportedPeerFilesPerOrdinaryFile": 2,
      "minimumOrdinaryFilesPerLocalPeerGroup": 3,
      "maximumOrdinaryFilesPerLocalPeerGroup": 5,
      "minimumTypedOrdinaryFilePercentPerLanguageCohort": 10,
      "minimumMethodBodiesPerTypedFile": 2,
      "minimumRelationshipTypedFilePercentPerLanguageCohort": 10,
      "minimumInheritanceInterfaceTraitCasesPerLanguageCohort": 1,
      "maximumCallFreeBodyStatementBytesPercentPerOrdinaryFile": 15,
      "maximumRepeatedCallFreeArithmeticAssignmentCopiesPerCallableBody": 0,
      "maximumIdenticalNormalizedNonLeafBodyShapePercentPerOrdinaryFile": 80,
      "maximumCommentPaddingPercentPerOrdinaryFile": 10,
      "maximumCommentPaddingPercentPerCohort": 10,
      "maximumRustModuleRootFiles": {
        "small": 1,
        "medium": 5,
        "large": 40
      },
      "maximumLargeIndexedDeclarationNodes": 900000,
      "maximumClassDetailRecordsAtLargestIndexScope": 225000
    }
  }
},
  'hardware/mac-mini-m5-pro-v1.json': {
  "version": 1,
  "profileId": "mac-mini-m5-pro-v1",
  "intended": {"chip": "Apple M5 Pro", "unifiedMemoryGB": 64, "os": "macOS", "storage": "local SSD", "location": "ownerLocal", "paidCloudAuthorized": false},
  "observed": {
    "capturedOn": "2026-09-23",
    "modelIdentifier": "Mac17,16",
    "chip": "Apple M5 Pro",
    "cpuCores": 18,
    "cpuCoreGroups": [6, 12],
    "gpuCores": 20,
    "memoryGiB": 64,
    "ssd": {"model": "APPLE SSD AP1024Z", "capacityTB": 1},
    "os": {"name": "macOS", "version": "27.0", "build": "26A428"},
    "rustVersion": "1.98.1",
    "cargoVersion": "1.98.1",
    "nodeVersion": "24.11.1"
  },
  "semanticProducers": [
    {"name": "native-conformance-javascript", "status": "recorded", "version": "1", "installedAtCapture": true, "capturedOn": "2026-10-23", "installMethod": "fixture-local capture-native.mjs and TypeScript 5.9.3 typescript.js packed as deterministic USTAR outside the repository; sorted members, mtime/uid/gid zero, empty owner names, mode 0644", "artifactDigest": "sha256:2aa11c2830377fff48c38772b597c94d846766e4c26ecbdf22aa07e2b8f619d5", "executableHash": "2aa11c2830377fff48c38772b597c94d846766e4c26ecbdf22aa07e2b8f619d5", "runtime": "Conformance native@1 (not production trellis.native.syntax); Node 24.11.1. Frozen parser SHA-256 3ae902c92cc44dace175c0e69e13a4b0899f6983c6121d76b9ab8dd5795e7675; source-only extractionInputs []."},
    {"name": "scip-java", "status": "recorded", "version": "0.12.3", "installedAtCapture": true, "capturedOn": "2026-09-27", "installMethod": "coursier bootstrap --standalone com.sourcegraph:scip-java_2.13:0.12.3", "artifactDigest": "sha256:5bef376c8960f1b25fcbf1156bb3f61b4d3f338d94b6bbd1a474d2db957a36ff", "executableHash": "5bef376c8960f1b25fcbf1156bb3f61b4d3f338d94b6bbd1a474d2db957a36ff", "runtime": "coursier-managed Temurin javac selected by the build config jvm field: 17.0.20.1 by default, 25.0.4.1 verified with jvm 25; system OpenJDK 25.0.3"},
    {"name": "scip-typescript", "status": "recorded", "version": "0.4.0", "installedAtCapture": true, "capturedOn": "2026-09-27", "installMethod": "npm install -g @sourcegraph/scip-typescript@0.4.0", "artifactDigest": "sha512-k+AtsrqmS41Sd5qjkZlHcmvoSQIvBOonRj4jpgp0KNFM6aqvMGpdSuPUqrUcg8ENTKjUbfaUVszgQwq3bCOvwA==", "executableHash": "fa4c8398393d2743e1306fdc7eaccfaafe488690bbfed481354abbf5a33559b9", "runtime": "Node 24.11.1"},
    {"name": "scip-python", "status": "recorded", "version": "0.6.6", "installedAtCapture": true, "capturedOn": "2026-09-27", "installMethod": "npm install -g @sourcegraph/scip-python@0.6.6", "artifactDigest": "sha512-qoKL1Rggg0o5newAFbCFAKlS0AjWxG5MA+mC28BtgxOv0DhO4zdL8u7151FxEppDpXMVvm7+yXSjXotoVH9cMQ==", "executableHash": "be8e0a1ec180423c60e9f2c2672c208f49a35ab690ac0f8f1259e4223dbd6e42", "runtime": "Node 24.11.1"},
    {"name": "rust-analyzer", "status": "recorded", "version": "1.98.1 (48a229ce 2026-09-01)", "installedAtCapture": true, "capturedOn": "2026-09-27", "installMethod": "rustup component add rust-analyzer (stable-aarch64-apple-darwin)", "artifactDigest": "sha256:78082ea1a96a008751767bd35f43738b5d4565cbef06ee5b31ad742a316133b5", "executableHash": "78082ea1a96a008751767bd35f43738b5d4565cbef06ee5b31ad742a316133b5", "runtime": "Rust 1.98.1 toolchain"}
  ],
  "toolchains": [
    {"name": "node-24.11.1-darwin-arm64", "status": "recorded", "version": "24.11.1", "installedAtCapture": true, "capturedOn": "2026-10-23", "installMethod": "existing mise-managed Node 24.11.1 installation", "artifactDigest": "sha256:4255a388254ca4319e2f95f1da375d5deaddf25baf9c7c85070b67f9543b15d0", "toolchainHash": "4255a388254ca4319e2f95f1da375d5deaddf25baf9c7c85070b67f9543b15d0", "runtime": "macOS arm64 Node executable; native conformance parser TypeScript 5.9.3 lib/typescript.js SHA-256 3ae902c92cc44dace175c0e69e13a4b0899f6983c6121d76b9ab8dd5795e7675 is frozen in the producer artifact, not a revision input"}
  ],
  "qualification": {"producerInventoryComplete": true, "readyForAuthoritativeBaseline": true, "benchmarkEvidence": null, "passingBenchmarkClaim": false}
},
};

const MAX_BYTES = 8 * 1024 * 1024;
const MAX_DEPTH = 128;
const FRACTIONAL_PATHS = new Set([
  "benchmark-policy-v1.json:$.quality.realProducerQualification.z",
  "benchmark-policy-v1.json:$.performance.mixedLoad.publisherWallToSoloMedianMaxMultiplier",
]);
const own = Object.hasOwn;
function fail(path, reason) { throw new Error(`${path}: ${reason}`); }
function plain(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value) &&
    (Object.getPrototypeOf(value) === Object.prototype || Object.getPrototypeOf(value) === null);
}
function validString(value, path) {
  if (typeof value !== "string") fail(path, "expected string");
  for (const char of value) {
    const point = char.codePointAt(0);
    if (point >= 0xd800 && point <= 0xdfff) fail(path, "lone surrogate");
  }
}
function validateExact(actual, expected, path) {
  if (Array.isArray(expected)) {
    if (!Array.isArray(actual)) fail(path, "expected array");
    if (actual.length !== expected.length) fail(path, `expected ${expected.length} entries, got ${actual.length}`);
    expected.forEach((entry, index) => validateExact(actual[index], entry, `${path}[${index}]`));
  } else if (plain(expected)) {
    if (!plain(actual)) fail(path, "expected plain object");
    for (const key of Object.keys(actual)) {
      validString(key, `${path} key`);
      if (!own(expected, key)) fail(`${path}.${key}`, "unknown field");
    }
    for (const key of Object.keys(expected)) {
      if (!own(actual, key)) fail(`${path}.${key}`, "missing field");
      validateExact(actual[key], expected[key], `${path}.${key}`);
    }
  } else {
    if (typeof actual !== typeof expected || (expected === null && actual !== null))
      fail(path, `expected ${JSON.stringify(expected)}`);
    if (typeof actual === "string") validString(actual, path);
    if (typeof actual === "number" &&
        (!Number.isFinite(actual) || Object.is(actual, -0) ||
         (!FRACTIONAL_PATHS.has(path) && (!Number.isSafeInteger(actual) || actual < 0))))
      fail(path, "invalid number");
    if (!Object.is(actual, expected)) fail(path, `expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

// JSON.parse owns grammar checking. This bounded lexical pass only tracks nesting
// and decoded object keys, so escaped Unicode names share the same identity.
function scanKeysAndDepth(source, file) {
  const stack = [];
  for (let i = 0; i < source.length; i++) {
    const char = source[i];
    if (char === '"') {
      const start = i;
      while (++i < source.length) {
        if (source[i] === "\\") { i++; continue; }
        if (source[i] === '"') break;
      }
      if (i >= source.length) return; // JSON.parse reports malformed syntax.
      const frame = stack.at(-1);
      if (frame?.expectKey) {
        let key;
        try { key = JSON.parse(source.slice(start, i + 1)); }
        catch { return; }
        validString(key, `${file}:$ key`);
        if (frame.keys.has(key)) fail(file, `duplicate object key ${JSON.stringify(key)} at offset ${start}`);
        frame.keys.add(key);
        frame.expectKey = false;
      }
    } else if (char === "{" || char === "[") {
      if (stack.length >= MAX_DEPTH) fail(file, "JSON nesting exceeds 128");
      stack.push(char === "{" ? {keys: new Set(), expectKey: true} : null);
    } else if (char === "}" || char === "]") {
      stack.pop();
    } else if (char === "," && stack.at(-1)?.keys) {
      stack.at(-1).expectKey = true;
    }
  }
}
function parseStrict(bytes, file) {
  if (bytes.length > MAX_BYTES) fail(file, "input exceeds 8 MiB");
  const source = bytes.toString("utf8");
  if (!Buffer.from(source, "utf8").equals(bytes)) fail(file, "invalid UTF-8");
  if (source.charCodeAt(0) === 0xfeff) fail(file, "UTF-8 BOM forbidden");
  let value;
  try { value = JSON.parse(source); }
  catch (error) { fail(file, `invalid JSON: ${error.message}`); }
  scanKeysAndDepth(source, file);
  return value;
}
function checkDocument(bytes, file) {
  const value = parseStrict(bytes, file);
  validateExact(value, EXPECTED[file], `${file}:$`);
  return value;
}
function crossFileTests(documents) {
  const policy = documents[names[0]];
  const cohort = documents[names[1]];
  const hardware = documents[names[2]];
  const identical = (left, right, path) => {
    if (JSON.stringify(left) !== JSON.stringify(right)) fail(path, "cross-file mismatch");
  };
  identical(policy.quality.languages, cohort.languages, "cohorts-v1.json:$.languages");
  identical([...policy.quality.languages].sort(),
    Object.keys(policy.quality.realProducerQualification.floorsPct).sort(),
    "benchmark-policy-v1.json:$.quality.realProducerQualification.floorsPct");
  identical(policy.referenceHost.profileId, hardware.profileId, "hardware/mac-mini-m5-pro-v1.json:$.profileId");
  identical(hardware.observed.cpuCoreGroups.reduce((sum, count) => sum + count, 0),
    hardware.observed.cpuCores, "hardware/mac-mini-m5-pro-v1.json:$.observed.cpuCoreGroups");
  identical(cohort.conformance.categories.length * cohort.conformance.minimumScenariosPerCategoryPerLanguage,
    cohort.conformance.minimumScenariosPerLanguage, "cohorts-v1.json:$.conformance.categories");
  const pins = EXPECTED[names[1]].realProjects.projects;
  identical(cohort.realProjects.projects.length, 12, "cohorts-v1.json:$.realProjects.projects");
  const seen = new Set();
  for (const [index, project] of cohort.realProjects.projects.entries()) {
    const pin = pins[index];
    identical([project.language, project.repository, project.revision],
      [pin.language, pin.repository, pin.revision], `cohorts-v1.json:$.realProjects.projects[${index}]`);
    if (seen.has(project.repository)) fail(`cohorts-v1.json:$.realProjects.projects[${index}]`, "duplicate repository");
    seen.add(project.repository);
  }
}
const names = Object.keys(EXPECTED);
function loadStrictJson(name) {
  const url = new URL(`./${name}`, import.meta.url);
  if (fs.statSync(url).size > MAX_BYTES) fail(name, "input exceeds 8 MiB");
  return checkDocument(fs.readFileSync(url), name);
}
function validateFiles() {
  const documents = Object.fromEntries(names.map((name) => [name, loadStrictJson(name)]));
  crossFileTests(documents);
  return documents;
}

function selfTest() {
  const snapshots = Object.fromEntries(names.map((name) => [name, structuredClone(EXPECTED[name])]));
  const buffer = (value) => Buffer.from(JSON.stringify(value));
  const reject = (raw, name, label, pattern) =>
    assert.throws(() => checkDocument(raw, name), pattern, `${name}: ${label}`);
  const mutation = (name, label, change) => {
    const document = structuredClone(snapshots[name]);
    change(document);
    reject(buffer(document), name, label, /expected|missing|unknown|invalid|entries|lone surrogate/);
  };
  // Literal groups independently check each committed file before mutations.
  function policyLiteralTests() { assert.doesNotThrow(() => checkDocument(buffer(snapshots[names[0]]), names[0])); }
  function cohortLiteralTests() { assert.doesNotThrow(() => checkDocument(buffer(snapshots[names[1]]), names[1])); }
  function hardwareLiteralTests() { assert.doesNotThrow(() => checkDocument(buffer(snapshots[names[2]]), names[2])); }
  policyLiteralTests(); cohortLiteralTests(); hardwareLiteralTests();
  const changeScalar = (value) => value === null ? "invented" :
    typeof value === "number" ? value + 1 : typeof value === "boolean" ? !value : `${value}X`;
  let cases = 0;
  function allFieldMutationTests(name, expected, route = []) {
    const at = (document) => route.reduce((value, key) => value[key], document);
    const label = route.length ? route.join(".") : "$";
    if (Array.isArray(expected)) {
      if (expected.length) { mutation(name, `${label} remove`, (document) => at(document).pop()); cases++; }
      mutation(name, `${label} add`, (document) => at(document).push(null)); cases++;
      if (expected.length > 1 && JSON.stringify(expected[0]) !== JSON.stringify(expected[1])) {
        mutation(name, `${label} reorder`, (document) => {
          const array = at(document); [array[0], array[1]] = [array[1], array[0]];
        }); cases++;
      }
      if (expected.length) {
        mutation(name, `${label} kind`, (document) => {
          at(document)[0] = typeof expected[0] === "number" ? "wrong-kind" : 77;
        }); cases++;
      }
      expected.forEach((entry, i) => allFieldMutationTests(name, entry, [...route, i]));
    } else if (plain(expected)) {
      for (const key of Object.keys(expected)) {
        mutation(name, `${label} missing ${key}`, (document) => { delete at(document)[key]; }); cases++;
        allFieldMutationTests(name, expected[key], [...route, key]);
      }
      mutation(name, `${label} extra`, (document) => { at(document).__unknown = true; }); cases++;
    } else {
      mutation(name, label, (document) => {
        const parent = route.slice(0, -1).reduce((value, key) => value[key], document);
        parent[route.at(-1)] = changeScalar(expected);
      }); cases++;
    }
  }
  for (const name of names) allFieldMutationTests(name, snapshots[name]);
  function policyGuardMutationTests() {
    const name = names[0];
    for (const [route, value] of [
      [["quality", "realProducerQualification", "minimumPositiveOracleCases"], 500],
      [["quality", "realProducerQualification", "minimumHardNegativeOracleCases"], 500],
      [["quality", "realProducerQualification", "notMeasuredValue"], 100],
      [["quality", "realProducerQualification", "notMeasuredPasses"], true],
      [["quality", "relabelAfterProducerOutputAllowed"], true],
      [["regressions", "zeroToPositiveAlwaysFails"], false],
      [["baseline", "failedRunMayRefresh"], true],
      [["performance", "mixedLoad", "explicitRequestWhileJobRunning", "browserDaemonIndexRoute"], "busy"],
    ]) {
      mutation(name, route.join("."), (document) => {
        const parent = route.slice(0, -1).reduce((item, key) => item[key], document);
        parent[route.at(-1)] = value;
      }); cases++;
    }
  }
  function cohortMutationTests() {
    for (let i = 0; i < 12; i++) {
      mutation(names[1], `revision ${i}`, (document) => { document.realProjects.projects[i].revision = "different"; }); cases++;
    }
  }
  function hardwareMutationTests() {
    mutation(names[2], "producer version", (document) => { document.semanticProducers[0].version = "1.0"; }); cases++;
    mutation(names[2], "baseline claim", (document) => { document.qualification.passingBenchmarkClaim = true; }); cases++;
  }
  policyGuardMutationTests(); cohortMutationTests(); hardwareMutationTests();
  function strictInputTests() {
    const file = names[0];
    for (const [label, raw, pattern] of [
      ["UTF-8", Buffer.from([0xff]), /invalid UTF-8/],
      ["BOM", Buffer.from("\ufeff{}"), /BOM/],
      ["syntax", Buffer.from("{"), /invalid JSON/],
      ["trailing", Buffer.from("{} trailing"), /invalid JSON/],
      ["literal duplicate", Buffer.from('{"a":1,"a":2}'), /duplicate object key/],
      ["escaped duplicate", Buffer.from('{"a":1,"\\u0061":2}'), /duplicate object key/],
      ["astral duplicate", Buffer.from('{"😀":1,"\\uD83D\\uDE00":2}'), /duplicate object key/],
      ["nested duplicate", Buffer.from('{"x":{"a":1,"a":2}}'), /duplicate object key/],
      ["root kind", Buffer.from("[]"), /expected plain object/],
      ["lone surrogate", Buffer.from('{"\\ud800":1}'), /lone surrogate/],
      ["unsafe integer", Buffer.from('{"version":9007199254740992}'), /invalid number/],
      ["negative zero", Buffer.from('{"version":-0}'), /invalid number/],
      ["oversized", Buffer.alloc(MAX_BYTES + 1, 32), /exceeds 8 MiB/],
      ["depth", Buffer.from("[".repeat(129) + "]".repeat(129)), /nesting exceeds/],
    ]) { reject(raw, file, label, pattern); cases++; }
    assert.equal(parseStrict(Buffer.from('{"a":1,"inner":{"a":2},"text":"\\"a,{}"}'), file).inner.a, 2);
    const reordered = JSON.parse(JSON.stringify(snapshots[file]));
    reordered.approval = Object.fromEntries(Object.entries(reordered.approval).reverse());
    assert.doesNotThrow(() => checkDocument(Buffer.from(" \n" + JSON.stringify(reordered) + "\n"), file));
  }
  strictInputTests();
  const reorderedFloors = structuredClone(snapshots);
  const floors = reorderedFloors[names[0]].quality.realProducerQualification;
  floors.floorsPct = Object.fromEntries(Object.entries(floors.floorsPct).reverse());
  const checkedReorder = Object.fromEntries(names.map((name) =>
    [name, checkDocument(buffer(reorderedFloors[name]), name)]));
  assert.doesNotThrow(() => crossFileTests(checkedReorder));
  const copies = structuredClone(snapshots);
  copies[names[2]].profileId = "other";
  assert.throws(() => crossFileTests(copies), /profileId: cross-file mismatch/); cases++;
  copies[names[2]].profileId = snapshots[names[2]].profileId;
  copies[names[2]].observed.cpuCoreGroups[0] = 7;
  assert.throws(() => crossFileTests(copies), /cpuCoreGroups: cross-file mismatch/); cases++;
  console.log(`policy checker self-test passed (${cases} negative mutations); not benchmark qualification`);
}
try {
  if (process.argv.length > 3 || (process.argv.length === 3 && process.argv[2] !== "--self-test"))
    fail("arguments", "usage: node docs/semantic-evidence/check-policy.mjs [--self-test]");
  validateFiles();
  console.log("policy/cohort/hardware validation passed; not benchmark qualification");
  if (process.argv[2] === "--self-test") selfTest();
} catch (error) {
  console.error(`policy checker failed: ${error.message}`);
  process.exitCode = 1;
}
