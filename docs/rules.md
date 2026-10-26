# Reviewed architecture rules

Trellis checks human-reviewed architecture in the fixed checkout file
`.trellis/rules.toml`. It does not infer layers during a check or write rules.
Layers select indexed **native file modules by path** and proved **external package
identities**. Package targets are not native modules or resolved library symbols. It exposes
read-only references, rule check/explain tools, a proposal prompt and
`trellis rules check --format json`. See the [MCP wire contract](mcp-readonly-pilot-contract.md)
for transport, attribution and limits.

## Start with reviewed layers

```toml
version = 1

# Cross-peer and shared-to-ordered references default to forbid.
# To allow either direction explicitly, add before the layer tables:
# [layer_policy]
# peers = "allow"
# shared_outbound = "allow"

[layers.web]
order = 1
paths = ["src/web/**"]

[layers.listeners]
order = 1
paths = ["src/listeners/**"]

[layers.service]
order = 2
paths = ["src/service/**"]

[layers.shared]
shared = true
paths = ["src/model.rs", "src/testing.rs"]
```

Cross-peer references and shared-to-ordered references are deviations by default.
Omitting `[layer_policy]` or either key is equivalent to setting that key to
`"forbid"`. Set `peers = "allow"` or `shared_outbound = "allow"` explicitly to
allow that direction. An exact reviewed exception can approve an individual pair
without changing its underlying deviation status.

An integer `order` is explicit: lower means higher, equal integers are peers.
Table order has no meaning. A layer has either `order` or `shared = true`, never
both. Assignment to two different layers is a located ambiguity error; overlapping
patterns within one layer deduplicate. Unassigned files remain visible and unknown,
not automatically placed.

For an observed production structural reference pair:

| Direction | Result |
| --- | --- |
| Same layer | conforms |
| Ordered source to larger order (downward, including skipped levels) | conforms |
| Ordered source to smaller order (upward) | deviates |
| Different ordered layers with equal order | deviates unless `peers = "allow"` |
| Any source into shared, including shared into shared | conforms |
| Shared into ordered | deviates unless `shared_outbound = "allow"` |
| Unassigned endpoint or unresolved mapping/ownership | unknown |

Conformance describes **that observed structural pair**, not the absence of hidden
semantic dependencies in a module. A stricter no-skipping rule is not implicit.

## External package policy

A layer needs a nonempty `paths` or `external` collection; either selects that
layer. External selectors are closed objects with all three fields required:

```toml
[external_policy]
deny = [{ language = "rust", package = "legacy-db", stdlib = false }]

[layers.adapters]
order = 2
external = [{ language = "rust", package = "serde_json", stdlib = false }]

[layers.foundation]
shared = true
external = [{ language = "rust", package = "std", stdlib = true }]
```

`language` is `rust`, `java`, `python` or `javascript`. `package` is a literal
canonical identity, not a glob, import alias or version; `stdlib` is required and
distinguishes standard-library targets from declared dependencies of the same
spelling. Canonical dependency names are Cargo's upstream declared package name
(preserving hyphens/underscores), Maven `groupId:artifactId`, PEP 503-normalized
Python distribution name, and exact npm name or `@scope/name`. Import bindings
and version constraints are evidence, not identity. Unknown fields and invalid
identifiers are located `invalid_rules` errors. Repeats within a layer deduplicate;
two different layers selecting one external identity are a located ambiguity.
These arrays share existing configuration collection/work bounds.

`[external_policy]` accepts only optional `deny`. An absent or empty list means no
external bans. Every positively observed matching external edge emits a reserved
`$external` instance with `status:"deviates"`, even if its source has no layer.
Nonmatches emit no `$external` row; absent edges do not prove absence. Aggregate
by source module, external identity and stored production/test role with the
existing exact counts and bounded samples. Test-only instances never fail the
production CLI check. Unproved roots stay unknown, never guessed to match a ban.

`$layers` still checks direction for assigned external destinations. A ban and a
layer violation can both be reported: two policy instances, not two references.
`$external` has no exceptions. Remove or narrow a ban through a reviewed policy
edit. Neither declaration exceptions nor module-to-module exceptions accept
external package destinations.

## Roles and constraints

```toml
[roles.resource]
select = { paths = ["src/web/**"], name = "*Resource", language = ["java"] }

[roles.service]
select = { name = "*Service" }

[roles.repository]
select = { extends = "JpaRepository" }

[[rules]]
id = "layered-write-path"
for_each = "resource"
require = [{ calls = "service" }]
forbid = [{ calls = "repository" }]
```

The closed top-level keys are `version`, `layer_policy`, `external_policy`, `layers`,
`roles`, `rules` and `exceptions`. `version` is required and exactly `1`; optional collections
start empty. A selector has at least one of `paths`, `name`, `language`, `extends`
and `implements`. Keys combine with AND; entries within `paths` or `language`
combine with OR. Languages are `java`, `rust`, `python`, `javascript`. Name patterns
match the measured nullable name; null never receives an invented name.

`extends` and `implements` **selectors** match direct header syntax spelling where
the grammar distinguishes it, not resolved ancestry. An unavailable or unsupported
distinction retains uncertain selection as unknown rather than silently excluding
the declaration. A header spelling match does not create a type relationship.

Each rule names an existing source role in `for_each`. Constraints are closed
single-key objects, exactly `calls`, `extends` or `implements`, naming an existing
target role. Arrays are conjunctive. Unknown keys/references, duplicate rule IDs,
invalid identities and malformed input are located errors, never ignored fields.
No annotations, recursive Boolean DSL or transitive-path syntax is supported here.

**Existing calls have no resolved callee binding; existing header bases are not
semantic type relationships.** Consequently semantic `calls` constraints return
`unknown` with `call_binding_unavailable`, and semantic `extends`/`implements`
constraints return `unknown` with `type_relationship_unavailable`. They retain
actual measured call/header evidence. Same-name candidates, import spelling and
legacy class candidates cannot become positive bindings. Absence of a required
edge never establishes a violation.

An instance is `conforms`, `deviates` or `unknown`. A proven deviation wins over
uncertainty, but uncertainty remains in the details. Instances deduplicate by
`(rule, sourceSetId, language, path, sid)`, even if roles/selectors overlap. Layer
checking uses the reserved rule `$layers` and one instance per observed native
source to native-module or external-package destination pair. Absent or empty `layers` disables `$layers` checks;
unassigned native module and observed external identities remain informational. Ordinary declaration rules
still run and the references view remains available.

## Path patterns

Patterns are case-sensitive, checkout-relative, slash-separated and anchored.
`*` and `?` do not cross `/`; `**` spans directories. `src/store/**` selects
descendants, **not** `src/store.rs`; name the latter separately. Absolute paths,
backslashes, NUL, dot/parent components and unsupported glob syntax are rejected.
These are not ignore-file patterns: no negation or ambient ignore semantics.
Path filters on a request select **source** instances/sites only. Destination
mapping and target-role selection still use the whole selected indexed snapshot.

## What a reference means

The shared view-time projection parses **captured source**, using stored native
declarations and their document identities. A module is an indexed file identified
by its full `DocumentKey` and native file-module SID. A projected site is identified
by document version, exact byte range and relation kind; it is not a fabricated
`occ:v2` call or semantic relationship. Native links retain their real declaration
or occurrence IDs and proof; derived structural mapping is labelled as such.

Supported mapping is deliberately narrow:

- **Rust:** proved conventional module roots and lexical `crate`/`self`/`super`
  paths, explicit aliases/reexports and wildcard name exposure follow at most four
  import/reexport edges. Native final-owner mapping and real intermediate-module
  proof stay distinct. A wildcard can provide positive native mappings while
  residual unknowns remain; no native or external member inventory is invented.
  Captured Cargo declarations and reviewed `std/core/alloc` roots prove package
  targets, never semantic calls. Custom roots/`#[path]`, macro/cfg uncertainty,
  ambiguous ownership and depth beyond four remain unknown.
- **JavaScript:** static relative imports/reexports/side-effect imports; unique
  explicit indexed extension or `.js`/`.mjs`/`.cjs` and `index.js` convention.
  Bare, dynamic and ambiguous imports remain unresolved.
- **Python:** explicit relative imports with a unique `.py`/`__init__.py` directory
  convention. Module-versus-attribute ambiguity is not guessed. Absolute roots,
  aliases and wildcards remain unresolved.
- **Java:** import/qualified-type syntax is visible, but no package-to-file resolver
  is invented; non-path imports remain unresolved.

Comments and string decoys are ignored; actual import module-specifier strings
are syntax. Unsupported coverage and unresolved counts stay visible. Paths hidden
behind unproved aliases are not claimed covered. Rust/Java external-name
uncertainty does not veto an independently proved in-repo wildcard name; two
native candidates, macro/cfg uncertainty or unproved explicit bindings still do.
Python/JavaScript retain wildcard-only names as unknown under external-name
uncertainty unless an explicit/local binding settles them. This is **syntactic,
structurally mapped reference evidence**, not a compiler dependency graph,
resolved library membership or call binding. A positive mapped upward reference can deviate under this syntax contract;
no observed edges cannot prove no semantic dependencies.

Counts aggregate `(source module, destination module, source role)`, with bounded
deterministic sample sites and exact omitted-sample counts. Sampling does not
truncate counts or verdicts. Parse failures, cancellation and work bounds fail
atomically, not as a healthy-looking partial projection.

## Production and test evidence

A site's role comes from its innermost enclosing **stored** native declaration,
falling back to the stored file-module owner. Ambiguous ownership is unknown.
There is no read-time path or cfg classifier.
[Decision 0011](semantic-evidence/decisions/0011-test-production-declaration-role.md)
governs the role: Rust `cfg(test)` and inherited positive test markers are test;
`cfg(feature = "test-support")` is **production**, not a third role. A filename
such as `src/testing.rs` alone does not override the stored role.

Test references/instances are reported separately and never count as violations. Test-only policy comparisons retain their underlying
status and a `test_only` reason, including hypothetical deviations; only the
production counts determine failure.
A mixed file can therefore contribute production and test references separately.
Older baseline wording that classified feature-only test-support seams as test
must not override Decision 0011.

## Reviewed exceptions, not count baselines

An exception names an existing `rule`, an exact `target` and a nonblank `reason`.
The TOML target fields are `source_set`, `language`, `path`, `sid`; use the actual
emitted source-set ID and native SID, not a name or presentation label. For `$layers`,
`target` identifies the source module and required `destination` identifies the
exact native destination module in the same full identity shape. Ordinary
declaration exceptions have no destination. The approved portable module-to-module
form `{language,path}` is a **separate follow-up PR, not available in this Rust
slice**. Until that PR lands, both module endpoints require `source_set`,
`language`, `path` and `sid`; proposals must use only this parser-valid full form.
The future portable form remains module-only, never a package selector. `$external` has no exceptions. JSON identities use `sourceSetId` in place of TOML `source_set`.

An exact exception keeps the underlying status and evidence, sets `excepted=true`
and records its reason. It is excluded from deviation totals. An approved module
pair allows additional sites on that same pair, **not** a new destination. There
is no scalar-count ratchet, separate mining engine, historical-count import or
automatic baseline write. A proposal may suggest exceptions for current positively
witnessed deviations for human review; historical #163 counts are not current facts.

## One code snapshot, separately identified configuration

References, checks, explanations, CLI and prompt use one daemon-owned admitted
snapshot with the complete `{indexGeneration,indexRevision}` pin and existing
checkout/root/finish fences. A stale pin fails; no moving-head joins or stale-pin
fallback. Projection is not persisted, writes no `index.db`, and changes neither
native producer output nor Stage 3 extraction.

The one current-file exception is root `.trellis/rules.toml`, read once per
check/explain/prompt as bounded configuration. References alone do not read it.
Its descriptor is `{path:".trellis/rules.toml",source:"checkout-config",contentHash}`.
**The code pin does not identify these rules bytes.** Editing only rules can change
the digest and outcome at an unchanged code pin. Code evidence never falls back
to live source. Missing config permits a proposal from scratch; check/explain fail
`rules_not_found`. Malformed config fails all three atomically with a located error.

`trellis_rules_explain` filters the same evaluated instances by exact rule/target
and, for `$layers` or `$external`, the actual native/external destination identity; it does not interpret a rule a second way.
`trellis rules check --format json` uses that same daemon evaluator. Unexcepted
production deviations fail the command; unknown-only evaluation succeeds.
Parser and operational failures retain ordinary nonzero error exits, not an
incorrectly successful evaluation. SARIF is deferred.

## CLI JSON

```sh
trellis references --workspace /absolute/checkout --path 'src/web/**' --limit 5
trellis rules check --format json --workspace /absolute/checkout --path 'src/web/**'
```

Both are read-only daemon operations; JSON is the references default and `json`
is the only check format. Repeat `--path` for source globs. Omit it to check all
sources. Use `--pin GENERATION:REVISION` for a complete generation UUID and numeric
revision, never a bare revision. MCP uses the object pair instead. CLI references
defaults to 5 sample sites per edge; MCP references and the prompt use the same default. Both accept
1–50; neither sample limit changes exact site counts or rule results.

## Work bounds and located failures

The fixed configuration read is limited to 2 MiB, with at most 1,024 entries per
named collection and 8,192 aggregate glob patterns. The locked TOML parser keeps
its default recursion guard of 80; unbounded parsing is not enabled. Structural
accounting is iterative. Projection admits at most 2 MiB per captured source,
256 MiB total selected source bytes, 1,000,000 AST visits per document, AST depth
128 and 250,000 total sites. Reference samples accept at most 50 sites per edge.
Snapshot admission also caps the inventory at 100,000 documents and 512 MiB of
aggregate stored input. These bounds refuse the entire result rather than
silently dropping evidence.

A rule diagnostic is exactly
`{code,kind,path,range,line,column,attribution,message}`. `code` is `invalid_rules`;
`kind` is `schema`, `selector`, `constraint`, `glob`, `reference`, `ambiguity`,
`too_large`, `too_deep` or `read`. The fixed `path` is `.trellis/rules.toml`. Byte ranges
are zero-based half-open; line and column are 1-based. `attribution` is `token`,
`file` or `boundary`: a file-level/depth or size-boundary location is explicitly
labelled, never presented as an exact offending token. Ordinary I/O/configuration
read errors also carry a located file attribution. No partial instances accompany
a configuration error.

## Public JSON payloads

All objects below are closed; all listed fields are required. `UInt`, `Range`,
`DocumentKey`, language tags and the complete code pin follow the
[MCP shared types](mcp-readonly-pilot-contract.md#shared-schema-and-limits).
`Target` is exactly `{sourceSetId,language,path,sid}`; `sid` is an actual native
file-module/declaration SID, not a display label. Sources and rule targets always
use this native shape. `Destination` is the closed union of `Target` and
`{kind:"externalPackage",sourceSetId,language,package,standardLibrary}`. The full
source-set/language/package/standard-library tuple defines external equality.
No path, SID, native declaration, DocumentKey, source metric or semantic call target
is invented for a package. A config descriptor is exactly
`{path:".trellis/rules.toml",source:"checkout-config",contentHash}`.

### References data

`trellis_references` data is exactly:

```text
{referencesVersion:"references:v2",indexGeneration,indexRevision,
 evidenceTier:"syntax",semanticComplete:false,
 modules:Module[],externalPackages:ExternalPackage[],
 production:Edge[],tests:Edge[],unresolved:Unresolved[],
 counts:{scannedDocuments:UInt,mappedSites:UInt,unresolvedSites:UInt},
 limitations:string[]}
Module = {identity:Target,revisionId,contentHash,provenanceId}
ExternalPackage = {identity:ExternalIdentity,ecosystem:"cargo"|"rust",evidence:PackageEvidence[]}
ExternalIdentity = {kind:"externalPackage",sourceSetId,language,package,standardLibrary:boolean}
PackageEvidence = ManifestEvidence | CatalogEvidence
ManifestEvidence = {kind:"manifest",sourceSetId,nativeRevisionId,dependencyHash,
                    path,contentHash,importRoot,section,key,versionConstraint:string|null}
CatalogEvidence = {kind:"standardLibraryCatalog",catalog:"rust-standard-library:v1",
                   entry:"std"|"core"|"alloc"}
Edge = {source:Target,target:Destination,count:UInt,sites:Site[],
        omittedSites:UInt,mapping:"structural"}
Unresolved = {source:Target,site:Site,reason:string}
Site = {ownerSyntaxId:string|null,role:"production"|"test"|null,
        range:Range,lines:Lines,relationKind:"reference"|"import",
        spelling:string,callId:string|null}
```

Each external descriptor has nonempty `evidence`. The current Rust producer derives
`ecosystem:"cargo"` for declared packages and `ecosystem:"rust"` for the reviewed
Rust standard-library catalog. Manifest evidence identifies the selected source
set/native revision and retained dependency hash, captured manifest path/content
hash, actual import root, declaration section/key and nullable original version
constraint. Catalog evidence identifies a reviewed code-shipped entry, not a
detected installed runtime. No precise source range is invented. Package evidence
is capture/catalog evidence, not native measured syntax provenance; actual source
sites and native intermediate-module proof remain separate.

Captured root Cargo bytes come from the selected revision's retained dependencies
through the same admitted read and transaction as source. There are no live
manifest reads, Cargo invocations, installed-library reads or parallel caches.
Malformed repository manifests stay unproved, not empty dependencies. Malformed
stored components fail with the existing typed integrity failure. Manifest parsing
shares the existing source/stored-byte and work bounds, with cancellation before
growth and atomic typed failure on exhaustion.

The first Rust slice applies root Cargo evidence only to proved conventional root
entry/module chains. Nested/workspace members, inherited dependencies without a
member binding, custom roots and unproved ownership remain unknown. Root
bindings distinguish upstream package names from Cargo keys/renames (including
hyphen normalization). Conflicting normalized bindings and local path dependencies
are not guessed external packages. Sections, optional and target qualifiers do
not prove installation/activation or change stored site roles. In particular,
`build-dependencies` and `dev-dependencies` are retained declaration evidence,
not a claim that Cargo exposes them to a production lib/bin target. Conflicting
identities across sections stay unknown; target-applicable Cargo bindings would
require a separate contract change. `std/core/alloc` are reviewed root labels,
not member/API inventories. Java/Python/JavaScript
share the external identity shape but their package adapters are later work;
Maven groups and Python distribution names alone do not prove import prefixes.

The Rust name inventory does not register foreign function-signature bindings or
expand procedural derives. Names introduced by those forms are not a complete
export or shadowing inventory; they are an unsupported coverage gap, not proof
that no competing name exists. This projection is not compiler name resolution.

`Lines` is the existing inclusive 1-based line shape or null. Target mapping
always uses the full admitted snapshot module map internally. By the
[owner's #347 ruling](https://github.com/SquashMerge/trellis/issues/347#issuecomment-6415629907),
`modules` emits only the module/proof closure of selected sources (including
sources with no sites), native destinations of emitted production or test edges,
and real native intermediate-module proof closure. `externalPackages` contains
only descriptors for emitted external destinations, never inserted into `modules`.
Every emitted module retains its native revision, content hash and provenance.
A source filter matching no documents returns empty `modules`, `production`,
`tests`, `externalPackages` and `unresolved` arrays with zero counts; this is a complete empty
structural scope, not semantic completeness. Unrelated modules do not consume
response bytes. No cap is raised and no verdict or proof is clipped; genuinely
oversized selected evidence still fails atomically with `response_too_large`.
Modules are ordered by full document key. Edges sort by source and target identities within each role
array; samples sort by range, kind and spelling. Unresolved sites sort by source,
range, kind and reason and are not clipped. `count` is exact and
`omittedSites = count - sites.length`. `limit` bounds samples **per edge**, not
edge counts or verdict scope. A `callId` is present only if an actual stored
call range encloses that site. Module provenance is the real native measured
syntax proof, not semantic proof for the association. `semanticComplete:false`
is invariant, including empty results.

### Check and explain data

Both tools return exactly:

```text
{rulesVersion:"rules:v2",indexGeneration,indexRevision,
 rulesFile:ConfigDescriptor,externalPackages:ExternalPackage[],
 instances:Instance[],testOnly:Instance[],
 counts:Counts,testCounts:Counts,unassigned:Destination[],limitations:string[]}
Instance = {rule:string,target:Target,destination:Destination|null,
            sourceLayer:string|null,destinationLayer:string|null,
            status:"conforms"|"deviates"|"unknown",excepted:boolean,
            exceptionReason:string|null,constraints:ConstraintResult[],
            evidence:Evidence[],unresolved:string[],referenceCount:UInt}
ConstraintResult = {mode:"require"|"forbid",kind:"calls"|"extends"|"implements",
                    targetRole:string,status:"unknown",
                    evidence:Evidence[],unresolved:string[]}
Evidence = {document:DocumentKey,revisionId:string,range:Range,
            kind:"declaration"|"call"|"structuralReference",
            syntaxId:string|null,callId:string|null,provenanceId:string}
Counts = {comparable:UInt,conforms:UInt,deviates:UInt,unknown:UInt,excepted:UInt}
```

`ConstraintResult.status` is currently **only unknown**: no call or type binding
producer exists. Layer instances have no constraints; declaration instances have
null layer/destination fields and `referenceCount:0`. An unresolved reference
produces a `$layers` unknown instance with null destination, aggregated by source.
Unassigned module identities are listed even when no reference exists, using
the full snapshot module map and observed unassigned external identities. Empty
or absent layers disables `$layers` instances,
not this informational list, ordinary declaration rules or references.

Counts are independent for production and tests. `comparable` includes exceptions;
excepted instances are excluded from the three status totals while their status
and evidence remain intact. Test-only layer results retain their underlying policy status, including
hypothetical `deviates`; `testCounts` counts that array coherently. These are
hypothetical policy comparisons, **not violations**, and never fail the CLI.
Only production `counts.deviates` determines evaluation failure. Structural evidence carries actual
native source-document proof, not relationship proof. These new DTOs use their
explicit native document/revision/hash/provenance links rather than the existing
find/declaration/call tools' `coverage`/`provenance` array closure; that older
closure contract is unchanged for those tools. No new snapshot or semantic proof
is invented. Explain filters both arrays
by the exact identity and recomputes counts for that filtered output. Check and
explain include the external descriptors needed by returned instances and
unassigned identities; explain preserves its informational unassigned catalog.
These pre-release `references:v2` / `rules:v2` shapes replace v1 without a
compatibility mode.

## Proposal workflow

The no-argument MCP prompt `trellis_propose_layers` uses the launch checkout.
Its static list is workspace-independent; get uses the same evidence admission,
deadline, cancellation and final-root lifecycle as tools. It supplies server task
instructions separately from untrusted repository data: the pinned structural
reference view, limitations and production/test split, plus current exact rules
bytes/digest or explicit absence.

Ask the agent for integer layer orders, explicit peer/shared policy choices,
placement justifications, exact reviewed baseline exceptions and a `rules.toml`
**diff for human review**. Ambiguous placement belongs in normal chat. Trellis
never applies that diff, samples a model, invokes a provider, elicits input or
requests indexing. Any later file edit uses the agent's ordinary authorized tools.

MCP request and complete response bounds still apply, including prompt messages,
errors, summary text and LF. The larger static tool-catalog allowance does not
apply to prompt evidence. Whole-result overflow fails atomically; TOML and verdicts
are never clipped to make them fit.

This document does not certify production Claude Code interoperability or
interactive command display. Those require observations against the built Trellis
binary, not schema validation alone.
Resolved library members/APIs, framework/DI/events, automatic architecture inference, mining,
SARIF and the #121 UI remain separate work.
