# Reviewed architecture rules

Trellis checks human-reviewed architecture in the fixed checkout file
`.trellis/rules.toml`. Checks do not infer layers or write rules. The
[Architecture workbench](architecture-view.md#review-and-authoring) can author reviewed
exceptions and exact file assignments through an explicit diff preview and confirmation.
MCP and CLI rules operations remain read-only.
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
both. Path assignment uses the most specific matching patterns:

- An exact file path beats every glob.
- A glob is narrower only when its accepted valid path language is a proper subset
  of another glob's language, not when it has a longer prefix or fewer wildcards.
- Discard broader matches. If the remaining patterns all select one layer, use it.
  Equivalent or overlapping incomparable patterns in different layers produce a
  located ambiguity error. Repeated patterns within one layer deduplicate.

For example, `src/store/search/**` beats `src/store/**`, `src/?.rs` beats
`src/*.rs`, and `src/*??*` beats `src/*` on `src/ab`. By contrast,
`src/a*/b.rs` and `src/ab/*.rs` are incomparable on `src/ab/b.rs` and are
ambiguous across layers. A unique exact match wins even if broader globs tie.
Table/array order, layer names and layer `order` never break assignment ties.

Containment considers all valid paths, not only indexed files. Comparisons are
lazy, reused within a request, cancellable and charged to the existing bounded
work budget. Exhaustion fails atomically with `bounded_work`, never a guessed
winner or ambiguity. Unassigned files remain visible and unknown, not
automatically placed.

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

### Layer display hint

By the [owner's #391 ruling](https://github.com/SquashMerge/trellis/issues/391), a
layer may set an optional presentation hint for the browser Architecture view:

```toml
[layers.fixture-inputs]
order = 9
paths = ["tests/fixtures/**"]
display = "hidden" # "expanded" | "collapsed" | "hidden"
```

- `"expanded"` draws the band open on its module chips on first view.
- `"collapsed"` draws a summary band (header, count chips and one row of module chips
  coloured by metric, or a short stack in a shared box, #449) until the viewer opens it. This is the same as no `display` field.
- `"hidden"` leaves the band out of the diagram and lists it in a footer line
  (`N hidden layers · M files · E edges`) from which the viewer can show it.

Any other value, or a non-string, is a located `invalid_rules` schema error.
Without the field a band starts collapsed (#391 ruling). Every other layer is drawn,
including one with no cross-layer references: the full stack (#449).
Under the default production metrics population, a test or fixture layer **without**
a `display` field (#424: every file is under a fixture root or holds only stored test
callables) joins the hidden footer until **Include tests and fixtures** is on. An
authored `display` hint on such a layer always wins.
Assignment, verdicts, reference counts, exceptions and every check, explain,
references and MCP result are unchanged. The Architecture layer inventory carries
`layers[].display` (`null` when absent). Under the explicit #395 browser
[`workbench=true` metrics population](metrics.md#workbench-population), authored
`display="hidden"` also excludes that layer's files from default production heat.
Include tests and fixtures restores those contributions. This scoped heat rule does
not change stored roles or policy. Client-side expansion and collapsing do not change
metric population.
Rules edits preserve the field like any other authored text.

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

**Constraints use only fresh, selected semantic bindings (#523).** Header bases,
same-name candidates, import spelling and legacy class candidates never become
positive bindings. A document whose language has no selected producer (#483), or
no usable coverage tuple, has no bindings: its `calls` constraints return `unknown`
with `call_binding_unavailable` and its `extends`/`implements` constraints return
`unknown` with `type_relationship_unavailable`. They retain actual measured
call/header evidence.

With a selected producer, a measured call of the declaration counts when its fresh
binding is resolved, direct (or a constructor) and its internal target is a
declaration of this snapshot that the target role selects; the binding is added as
`semanticBinding` evidence. A type relationship counts when it is a fresh
`extends`/`implements` relationship of the declaration whose internal target the
role selects (`typeRelationship` evidence). Then:

- `require` **conforms** with one counted binding, and otherwise stays `unknown`:
  absence of a required edge never establishes a violation (`required_call_absent`,
  `type_relationship_absent`, or `…_incomplete` while bindings are missing).
- `forbid` **deviates** with one counted binding. A forbidden call **conforms** only
  when every measured call of the declaration has a fresh binding that is external
  or direct to a declaration outside the role, in a document the producer covers
  completely; otherwise it stays `unknown` with `call_binding_incomplete`. Forbidden
  type relationships never conform by absence.

Unbound, stale, ambiguous, unresolved and dispatched (virtual, interface, dynamic or
unknown dispatch) calls, and targets a role selects only by `extends`/`implements`
selector, are uncertain. A rule instance whose declaration is certainly selected
deviates when any constraint deviates and conforms when every constraint conforms;
otherwise, including whenever its selection is uncertain, it is unknown. A forbidden
type relationship without a counted one stays unknown with
`type_relationship_absence_unproven`.

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
- **JavaScript:** static relative imports/reexports/side-effect imports use a
  unique explicit indexed extension or `.js`/`.mjs`/`.cjs` and `index.js`
  convention. A trailing `/` selects only a directory's `index.js`, never a
  sibling file. `export * from` forwards proved named exports, never `default`.
  Explicit exported names shadow star names; distinct surviving declarations
  collide, while routes to the same native declaration deduplicate. Import-binding
  reexports retain the original identity; a proved local variable alias keeps its
  own native binding identity and owner, not its initializer's. Proved forwarding
  follows at most four edges. Name expansion into `.cjs`, cycles, unsupported
  namespace/CommonJS/dynamic exports, unproved bindings and over-depth forwarding
  retain unknowns; ordinary static module edges remain available. External stars
  may label a package but provide no names and leave residual uncertainty;
  an explicit proved export settles only its matching name. Supported static
  literals use the resolver's documented decoding subset, not runtime execution.
  Bare imports can label captured npm dependencies or the exact reviewed Node
  catalog below. Relative misses never fall back to builtin or npm labels.
- **Python:** captured checkout-root `m.py` or `m/__init__.py` layouts require
  unique module ownership and proved `__init__.py` package prefixes. Relative
  imports follow captured package membership; absolute imports use only that
  proved root convention. No suffix search, namespace-package or `sys.path`
  discovery is performed. A single unconditional literal `__all__` list of
  supported string literals selects exactly its proved names, including private
  spellings. Without `__all__`, proved public top-level names are exposed.
  Dynamic, mutated or conditional `__all__` cannot fall back to that public-name
  inventory. Proved simple import aliases and forwarding retain original native
  declaration identities and follow at most four module/import edges; wildcard
  edges reach final declaration-owner files and keep real intermediate proof
  separately. Distinct competing names, cycles, a fifth edge, missing selected
  names, dynamic exports, unsupported string decoding and unproved local layout
  remain unknown. Independently proved names can map while residual uncertainty
  remains visible; partial decoding or inventory never claims complete exports.
  Explicit/local bindings settle matching names, but an external star supplies no
  names and keeps wildcard-only bindings uncertain. Reviewed stdlib labels are
  package-only; third-party distributions remain unknown without authoritative
  captured import-name mapping. This is not Python execution or runtime binding.
- **Java:** captured declared packages and public top-level class/interface/enum/record
  declarations prove ordinary single-type and on-demand imports and qualified-type
  paths. On-demand imports enumerate only the exact package, not subpackages.
  Explicit/local type bindings shadow on-demand names; distinct competing native
  types remain unknown, while independent names can still map. Static imports,
  nested/member types and unproved bindings stay unknown. The reviewed `java`
  root supplies a package-family label. `javax` requires an exact containing
  package in the [pinned OpenJDK 25 public-export catalog](java-standard-library-catalog.md).
  Both labels remain package families, not selected-JDK member availability.
  Maven/Gradle coordinates alone do not prove third-party import prefixes.

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

Python coverage follow-ups from [PR361 review](https://github.com/SquashMerge/trellis/pull/361#issuecomment-6431615903):
`from . import x` and `from pkg import sub` can map to the package initializer
instead of the child module, so child-module edges can be missed. Dynamic writes
through `setattr(sys.modules[__name__], ...)`, `vars()[...] = ...` and
`__dict__.update(...)` are not fully tracked; this projection does not claim
complete detection of dynamic namespace mutation. Tuple `__all__` remains
unknown under the ratified literal-list-only rule.

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
For `$layers`, both `target` and required `destination` are **exactly**
`{language,path}` and identify a native file-module pair:

```toml
[[exceptions]]
rule = "$layers"
target = { language = "rust", path = "src/store.rs" }
destination = { language = "rust", path = "src/navigation.rs" }
reason = "Concrete reviewed justification for this observed pair"
```

Both fields are required at each endpoint. Language is `rust`, `java`, `python`
or `javascript`. Paths are nonempty, literal, case-sensitive repository-relative
paths, not globs, basenames or suffix matches. Wildcard markers `*` and `?` are
invalid, as are absolute paths, backslashes, NUL, colons, empty components, `.` and
`..` components. Existing path-length bounds apply. There is no case normalization
or layer-specificity matching. Extra endpoint fields, including `source_set` and
`sid`, are invalid for `$layers`; the old full-identity layer form is not accepted.
No new version, syntax tag or compatibility mode is needed.

Portable matching requires exactly one source-set ID in the **full admitted
checkout snapshot**, including document and retained-manifest identities, before
request-source filters. Multiple languages in one source set are valid. A second
source set is a located `invalid_rules` / `ambiguity` error even if its paths,
languages, roles or edges are unrelated to the exception or hidden by a filter.
The error aborts evaluation without partial instances; filters cannot choose the
scope. Within that unique scope, both native module endpoints must match their
respective literal language/path pairs. An edit that changes a module's native SID
but keeps its language/path is covered. A rename or move is not covered until the
exception is reviewed and updated. An absent endpoint simply matches no pair.

A named declaration-rule exception retains its **exactly four-field** target
`{source_set,language,path,sid}` and forbids `destination`:

```toml
[[exceptions]]
rule = "resource-service"
target = { source_set = "ACTUAL_SOURCE_SET", language = "java", path = "src/Resource.java", sid = "sid:v1:00000000000000000000000000000000" }
reason = "Concrete reviewed justification for this declaration"
```

The named rule must exist. Copy the actual emitted source-set ID and native
declaration SID, not a name or presentation label. A changed declaration SID or
source set does not match. Portable targets are invalid for declaration rules. Duplicate rule/endpoint tuples, blank reasons and other
closed-schema violations are located `invalid_rules` errors, not partial results.
Module exceptions never accept package destinations, and `$external` has no
exceptions; narrow a ban through a reviewed policy edit.

An exception keeps the underlying status, constraints, exact reference counts and
evidence, sets `excepted=true` and records its original reason. It is excluded from
deviation totals. An approved module pair allows additional sites on that same
pair, **not** a new source, destination, language or path. Production and test rows
remain separate; the role-neutral pair can decorate both, but test-only rows never
fail production checks. A null-destination unknown is not a module pair. A config
entry with no observed pair creates no instance and proves no absence.

Portability changes only exception TOML and matching. JSON targets and module
destinations retain their full native identities, including `sourceSetId` and actual SIDs, with
unchanged evidence/provenance closure. Explain requests still use the actual full
native identities from evaluated instances, never `{language,path}` in their place.
There is no scalar-count ratchet, separate mining engine, historical-count import
or automatic baseline write. A proposal may suggest exceptions only for current
positively witnessed production deviations for human review; historical #163
counts are not current facts.

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
accounting is iterative. Projection admits at most 2 MiB per captured source or
retained manifest and 256 MiB total admitted captured source and present
retained-manifest bytes, before source filters. AST depth remains 128. Reference
walks retain the 1,000,000-visit bound per walk; rule selector walks retain it per
document. The 250,000-site bound applies both to each raw source extraction and
to deduplicated selected sites across the request. Reference samples accept at
most 50 sites per edge. Snapshot admission also caps the inventory at 100,000
documents and 512 MiB of aggregate stored input. These bounds refuse the entire
result rather than silently dropping evidence.

Reference work has one request-wide allowance of
`min(100_000_000, 5_000_000 + 20_000*N + 512*K)` units. Rules use a separate
allowance of `min(50_000_000, 5_000_000 + 10_000*N + 256*K)` units, shared by
layer assignment, exact glob comparisons, declaration evaluation and evidence.
`N` counts all admitted source documents, including empty documents and
unselected languages. `K` is the ceiling KiB of the aggregate captured UTF-8
source bytes plus present retained-manifest bytes, rounded once after summing.
Absent manifests add no bytes; configuration bytes do not enlarge either
allowance. Both allowances use the whole admitted snapshot before source
filters, sample limits or rule selection, and arithmetic saturates at the
absolute ceilings. Explain only prunes an already admitted evaluation; its
package-closure recomputation is bounded by the 50,000,000-unit rules ceiling.

The growth terms are capacity policy, not measured runtime guarantees. AST visit
and site bounds stay fixed: visits already reset for each local traversal, and
measurements showed no visit or site pressure. Adding unrelated files should
not enlarge a pathological document's local traversal allowance or evidence
memory bound. The five-second MCP request deadline and cancellation checks are
unchanged and independent of work accounting.

Snapshot admission reads every selected document's rows on one connection, then
decodes them on at most eight workers. Per-file parsing is unbudgeted and runs as
one pass over all documents; after a failure no later document starts. Source
extraction/owner-call binding and rule declaration work run in bounded batches
of at most eight workers. Independent language catalog builds may run
concurrently, and the JavaScript catalog computes selected documents' write and
CommonJS hazards in parallel; Rust name resolution remains ordered. Workers
count local work against the phase's remaining allowance; one ordered owner
admits their counts and rows into the request total. A batch may perform
bounded speculative work, but workers never receive independent admitted
request budgets. Actual CPU work can reach about eleven times the allowance
(three catalog jobs beside eight JavaScript hazard workers) before failure is
reported, with the unchanged five-second deadline as backstop.
Serial and parallel plans use the same phases, charge units and output order.
Failure discards the whole result.

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
 evidenceTier:"syntax"|"mixed",semanticComplete:false,
 semanticProducers:{language,producerId}[],
 modules:Module[],externalPackages:ExternalPackage[],
 production:Edge[],tests:Edge[],unresolved:Unresolved[],
 counts:{scannedDocuments:UInt,mappedSites:UInt,unresolvedSites:UInt,semanticSites:UInt},
 limitations:string[]}
Module = {identity:Target,revisionId,contentHash,provenanceId}
ExternalPackage = {identity:ExternalIdentity,ecosystem:"cargo"|"rust"|"java"|"python"|"npm"|"node",evidence:PackageEvidence[]}
ExternalIdentity = {kind:"externalPackage",sourceSetId,language,package,standardLibrary:boolean}
PackageEvidence = ManifestEvidence | CatalogEvidence
ManifestEvidence = {kind:"manifest",sourceSetId,nativeRevisionId,dependencyHash,
                    path,contentHash,importRoot,section,key,versionConstraint:string|null}
CatalogEvidence = {kind:"standardLibraryCatalog",catalog:"rust-standard-library:v1",
                   entry:"std"|"core"|"alloc"}
                | {kind:"standardLibraryCatalog",catalog:"java-standard-library:v2",
                   entry:"java"|"javax"}
                | {kind:"standardLibraryCatalog",catalog:"python-standard-library:v2",
                   entry:<one of the pinned CPython v3.12.0 stdlib roots>}
                | {kind:"standardLibraryCatalog",catalog:"node-standard-library:v1",
                   entry:"fs"|"path"}
Edge = {source:Target,target:Destination,count:UInt,sites:Site[],
        omittedSites:UInt,evidenceTier:"syntax"|"semantic"|"mixed",semanticSites:UInt}
Unresolved = {source:Target,site:Site,reason:string}
Site = {ownerSyntaxId:string|null,role:"production"|"test"|null,
        range:Range,lines:Lines,relationKind:"reference"|"import"|"call"|"type",
        spelling:string,callId:string|null,
        mapping:"structural"|"semantic",provenanceId:string|null}
```

#### Semantic bindings (#523)

Where a document's language has a selected producer (the per-language default of
#483: the sole published producer covering it), the projection also reads that
producer's **fresh** evidence for the document, from a selected `complete` or
`partial` coverage tuple: call bindings of measured calls, resolved or external
use-site semantic references, and `extends`/`implements` relationships. Rows whose
proof is not `fresh`, and internal call targets not confirmed current
(`staleTarget` not `false`), are left out, so stale evidence reverts to structural
evidence. A call is represented by its lowest-provenance binding, as in the call
graph; when that one is unusable the call has no binding. Any internal target must
be a declaration of this snapshot (semantic references carry no separate
current-target flag); its module is the declaration's file.

- A structural site takes in every binding inside its range, and a call joined on a
  whole invocation that starts with it. It is bound by the binding that ends where
  it ends (its callee or final name; a call binding wins over a name), else by such
  an invocation. A resolved internal binding maps the site to its target's module;
  an external binding resolves the site without a destination unless structural
  evidence proved a package. An ambiguous or unresolved binding keeps the
  structural result.
- A measured call with a resolved internal binding that no structural site covers
  (a method call, say) becomes a `call` site, and a resolved internal `type`
  reference likewise a `type` site; one site per range, a call first. A target in
  the site's own module adds no site. Calls whose binding is external, ambiguous,
  unresolved, stale or missing add no site: they are not counted, as without an
  import.
- A site an external binding resolved is neither mapped nor unresolved, so `sites`
  can exceed `mappedSites + unresolvedSites`.
- A site mapped or resolved by a binding has `mapping:"semantic"` and its binding's
  `provenanceId`; other sites are `structural` with a null `provenanceId`.
  `counts.semanticSites` counts semantic sites with no residual. An edge's
  `semanticSites` counts its semantic sites, and its `evidenceTier` is `syntax`,
  `semantic` or `mixed` accordingly. The payload's `evidenceTier` is `mixed` once
  any site is semantic, and `semanticProducers` lists each language's selected
  producer.

Semantic bindings do not make the view complete: `semanticComplete` stays false,
unbound calls are not counted, and bindings outside the checkout name no package.
At most 2,000,000 semantic rows are admitted per snapshot (`bounded_work`), and
semantic sites count against the same site bound as structural ones.

Each external descriptor has nonempty `evidence`. Rust derives
`ecosystem:"cargo"` for declared packages and `ecosystem:"rust"` for the reviewed
Rust standard-library catalog. Java uses `ecosystem:"java"` and the reviewed
`java-standard-library:v2` catalog, with exact entries `java` and `javax`.
The `java` root policy is unchanged. The `javax` label requires exact containing-package
membership in the pinned OpenJDK 25 public-export catalog; parent packages and
invented descendants are not implied. These entries label package families, not
member existence or selected-JDK availability. Python uses `ecosystem:"python"` and the
`python-standard-library:v2` catalog: the exact 300 roots of the pinned CPython
v3.12.0 `sys.stdlib_module_names` source list. The
[Python catalog record](python-standard-library-catalog.md) gives its tag, commit,
source hashes and coverage. Upstream defines the same list on every platform,
including unavailable platform-specific and build-disabled modules; test modules
are excluded and only main package names are listed. This labels roots, not the
checkout runtime, installation, submodules or members. It never discovers or
checks the host Python version. Unlisted roots remain unknown. Local module
precedence and ambiguity are resolved before catalog
lookup; an unproved or ambiguous local root never falls back to an external
label. Python external star imports supply package edges, not member inventories;
their unknown names keep wildcard-only bindings uncertain unless a proved
explicit import or local declaration settles that name. Lookalike roots are not
catalog matches. Manifest evidence identifies the selected source
set/native revision and retained dependency hash, captured manifest path/content
hash, actual import root, declaration section/key and nullable original version
constraint. Catalog evidence identifies a reviewed code-shipped entry, not a
detected installed runtime. No precise source range is invented. Package evidence
is capture/catalog evidence, not native measured syntax provenance; actual source
sites and native intermediate-module proof remain separate.

Captured root Cargo and package.json bytes come from the selected revision's retained dependencies
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
not member/API inventories. Java's `java`/`javax` labels share this external
identity shape; the javax package gate does not change that identity. Non-JDK javax
packages require independent captured mapping evidence or remain unknown; no
third-party Maven/Gradle prefix mapping is inferred. Python
third-party distribution mappings remain unsupported.
Maven groups and Python distribution names alone do not prove import prefixes.
No Python third-party mapping table, installed-distribution lookup or additional
mapping-metadata capture is part of this slice.


JavaScript uses `ecosystem:"npm"` only for supported declarations in the captured
root `package.json`: `dependencies`, `devDependencies`, `optionalDependencies`
and `peerDependencies`. Constraints are literal decimal `M.m.p`, `^M.m.p` or
`~M.m.p`, without leading-zero components, whitespace, suffixes or compound ranges.
The original constraint is evidence, not an installed or resolved version.
Identical keys across sections merge their evidence; unsupported competing values
block that root. Duplicate relevant sections/keys or malformed JSON make manifest
proof unknown, never last-wins. Aliases/protocols, workspace/link/file/Git/URL
values, non-string constraints and unproved declarations cannot supply npm targets.
A root `workspaces` field blocks npm inference; the root package's own `name`
never proves a dependency/self-reference. Nested/workspace manifests, import maps,
package exports and installed node_modules are not discovered or resolved.

`pkg/sub/path` proves only declared key `pkg`; `@scope/pkg/sub` proves only
`@scope/pkg`. Empty/traversal/query/hash/backslash/scheme forms stay unknown.
No subpath/export/member existence is asserted. Dependency section names never
change native source-site production/test roles. JavaScript constructor work is
request-local, once when selected JS sources need it, charged to the existing
work budget with cancellation. There is no new capture/table/cache/live read.

Node uses `ecosystem:"node"` and `node-standard-library:v1`, exactly
`fs`, `node:fs`, `fs/promises`, `node:fs/promises` → `fs`, and
`path`, `node:path` → `path`. These labels take precedence over npm declarations.
All other `node:` forms remain unknown. The separately reviewed 56 pinned bare
builtin roots block npm labels, including their subpaths, without adding catalog
labels. This intentionally leaves false negatives such as `http/fake` and
`path/posix`; it is not Node execution semantics. Bare `test`/`sea` are not
reserved (their builtins require `node:`). The
[Node catalog record](node-standard-library-catalog.md) gives the fixed source
pin, exact spellings, root exclusion inventory and provenance. No host runtime,
`module.builtinModules`, compiler or package-manager command is consulted.

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
response bytes. No cap is raised and no verdict or proof is clipped. The MCP tools
page larger selected results ([paging](mcp-readonly-pilot-contract.md#architecture-references-and-reviewed-rules-129)); the CLI and
the browser detail reads ([Architecture view](architecture-view.md#summary-and-detail-reads-448)) keep these exact shapes.
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
                    targetRole:string,status:"conforms"|"deviates"|"unknown",
                    evidence:Evidence[],unresolved:string[]}
Evidence = {document:DocumentKey,revisionId:string,range:Range,
            kind:"declaration"|"call"|"structuralReference"|"semanticBinding"|"typeRelationship",
            syntaxId:string|null,callId:string|null,provenanceId:string}
Counts = {comparable:UInt,conforms:UInt,deviates:UInt,unknown:UInt,excepted:UInt}
```

`ConstraintResult.status` follows the [constraint rules](#roles-and-constraints):
`unknown` unless a fresh selected binding settles it. A `semanticBinding` evidence
item is the call's binding (its range is the binding's anchor, `syntaxId` the bound
target and `provenanceId` the semantic proof); a `typeRelationship` item names the
relationship's target and proof. A `$layers` instance's evidence for a semantic
site is `semanticBinding` with the site's owner `syntaxId` and the binding's
provenance; a structural site stays `structuralReference` with the source module's
native provenance. The second limitation says whether any
bindings were admitted: "Calls and type relationships have no admitted bindings and
remain unknown." appears only when no document has a selected producer.
Layer instances have no constraints; declaration instances have
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

Over MCP only, both payloads add the required `page:{offset,nextOffset,total}`, and
their arrays hold one page of the
[paged sequence](mcp-readonly-pilot-contract.md#architecture-references-and-reviewed-rules-129). Each
check/explain `Instance` keeps at most `limit` (default 5) `evidence` entries and adds
`omittedEvidence:UInt`, the exact number not shown. Counts, statuses and
`referenceCount` are those of the whole evaluation. The CLI and the browser's detail
reads return the complete arrays above.

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
