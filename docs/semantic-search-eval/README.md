# Semantic search evaluation set

This directory preserves the historical #214 development set used by
[#211](https://github.com/SquashMerge/trellis/issues/211), plus a separately pinned #389
exact-name supplement. The sets measure retrieval quality and define no product behavior.
[Decision 0018](../semantic-evidence/decisions/0018-semantic-symbol-retrieval.md) records the
ratified ranking and role-filter contract; the historical evaluation's proposed labels are
not a competing product contract.

## Format

`queries.json` holds one object:

```text
{
  format: "trellis-semantic-search-eval/v1",
  repository: "SquashMerge/trellis",
  resolvedAt: {commit, indexGeneration, indexRevision, extractorVersion},
  queries: [Query]
}
Query  = {id, category, query, answers: [Answer]}
Answer = {language, path, qualifiedName, kind, startLine, declarationId}
```

- `query` is what an agent or a person would type: "find the code that …", or an error message
  they are looking at.
- `answers` are the declarations that count as a correct hit. A query with several answers
  succeeds when **any** of them is retrieved, because a query often has more than one reasonable
  landing point (for example, a helper and the function that calls it).
- An answer is a **stable identity**:
  - `language`, the root-relative `path`, the `qualifiedName` (ancestor names then the declaration
    name, joined with `.`, as in the #24 `displayKey` after the `#`) and the measured `kind`;
  - `declarationId`, the #22 `sid:v1:` stable syntax ID at `resolvedAt.commit`. It changes when
    the path, name, container or sibling ordinal changes (contract-v1 § Stable identifiers), but
    not on a body-only edit.
  - `startLine` is the 1-based line where the declaration's measured range starts at
    `resolvedAt.commit`. It is a disambiguating hint, not identity.
  - **Re-resolving after the code moves.** Path, qualified name and kind are **not unique** in
    general: the #24 display key can repeat for Java overloads, and the stable ID also uses the
    source set, signature and sibling ordinal. Re-resolve by path, qualified name and kind; if
    several declarations match, choose by measured signature and nearest `startLine`, check the
    choice by reading it, then update `declarationId` and `startLine`.
- `category` is one of:
  - `concept`: describes behavior, and the declaration's name doesn't help;
  - `named`: a well-named lookup, where the words are close to the identifier;
  - `error`: an error or status message, looked up by its text;
  - `js`: the browser JavaScript, for cross-language coverage. The index holds Rust and JavaScript
    together, so every query competes across both languages. This is a **language** stratum, not
    a difficulty stratum: several of its queries repeat answer-name words (see below).

The candidate set is every **named** declaration in the head revision of a Trellis index of
`resolvedAt.commit`. That includes tests and test fixtures, because an agent's search sees them
too. All answers are production declarations (`role: production`); this is checked mechanically
when the set is resolved.

**Size and correlation.** 50 queries, 82 answer slots (73 distinct declarations), 24 source files. Several declarations
answer more than one query (for example `q27`/`e8`, `e5`/`q43` and `q11`/`q34`), so the queries are
correlated, not 50 independent trials. The `named` and `error` strata are intentionally easy:
`named` mirrors identifier words, and `error` uses verbatim message text, so they measure
lexically assisted lookup, not semantic paraphrase. **None of the 50 query strings equals an
answer's measured declaration name.** The six historical named queries are prose, not exact
identifier queries; they do not cover #389's raw-name rank-1 requirement.
**Report metrics per category, and inspect per-query misses.** An
overall number is not evidence of semantic retrieval; the `concept` stratum is.

## How the queries were written, and how restatement was avoided

1. **Drafted blind.** The queries were drafted in one pass from behavior descriptions in the
   contracts (`local-topology.md`, `mcp-readonly-pilot-contract.md`, `contract-v1.md`, the
   decision records), `CLAUDE.md` and the issue text, **before** searching the code for answers.
   The original draft list is checked in as [`blind-drafts.txt`](blind-drafts.txt).
2. **Answers resolved afterwards.** Each answer was then located by reading the code. It was
   recorded as a `path:line` anchor inside the declaration, then resolved mechanically to the
   innermost named declaration in the index.
3. **First-round changes to the drafts** (`da2aa63`):
   - Two drafts were dropped, because the behavior they describe has no single home in the code:
     a "racily clean mtime" re-hash, and warning ordering, which is inline in the projections.
   - The five drafted error queries named error *codes*. They were replaced with the message
     *text* an agent would actually see, taken verbatim from production code.
4. **Review corrections** (PR #214 review of `da2aa63`). Each change fixes a label or removes a
   lexical shortcut; none moves a query toward its answer's name:
   - **Wrong or partial answers fixed.**
     - `q09`: only `McpSelectedLines::range` maps bytes to lines; the inverse `mcp_lf_range` and the
       enumerator `mcp_lf_lines` were removed.
     - `q21`: `acp::reserve` reserves an attempt count, not money; removed.
     - `q01`: `retry_stale_hot_journal` retries a stale hot-journal verdict, not lock contention;
       removed.
     - `q02`: `validated_expected_basis` only converts a validated value; removed. The query now
       asks whether the pinned revision is still published and available, which is what
       `read_revision_for` and `validate_pin` check.
     - `q13`: the orphan check `inspect_index_with_open_hook` added beside the age check.
     - `q19`: the duration getter `relay_call_deadline` replaced by the code that enforces the
       deadline, `Relay::decide` and the daemon's `coordinate`.
     - `q31`: the CLI wait loop `cli_index` is the answer. In the second review round (`c4dcdb3`),
       the generic terminal-change signal helpers `wait_terminal_change` and
       `notify_terminal_waiters` were removed: they don't identify the request or read its
       outcome.
   - **Mislabelled tasks reworded.**
     - `q45` now asks for building the saved view or note *payload*, which is what its answers do
       (the saving code is an anonymous callback).
     - `q46` now asks for the line-based navigation menu, not go-to-definition.
     - `q24` states the real order (production callers first, then proximity).
     - `q07` no longer claims re-serialization.
   - **Test-only answer replaced.** `e6` quoted a message raised only in `#[cfg(test)]` code. It is
     replaced by a production message, "storage_busy: index journal sidecar present".
   - **Concept queries that repeated answer-name words rewritten**, so no concept query names its
     answer:
     - `q01` (read), `q02` (pin, revision), `q04` (git, parse), `q05` (durable, marker) and
       `q06` (stable);
     - `q10` (source), `q12` (release, revision), `q13` (index), `q20` (declaration, test) and
       `q21` (reserve);
     - `q22` (canonical), `q25` (source, walk), `q26` (root, replaced), `q28` (cancel) and
       `q30` (diagnostic, marker);
     - `q31` (index, terminal, wait).
5. **Labelled overlap.** Queries in the `named` category deliberately share words with the
   identifier, and so do some `js` queries (`q41`, `q43`, `q45`, `q46`). They are labelled so their
   easier lookups can be reported separately.
6. **Measured overlap.** For every query, the benchmark measures the share of the answer's
   split-identifier name tokens that the query repeats. A token counts when it is equal, or when
   one is a prefix of the other and both have at least four characters, so "indexing" repeats
   "index". After the corrections, 29 of 30 `concept` queries repeat no answer-name token. The
   remaining one is `q02`, which repeats only the stopword "for" (from `read_revision_for`).

## Metrics

Each method returns a ranked list of declarations, cut at depth 100. For query `q` with answer set
`A(q)`:

- **recall@k**: the fraction of queries with at least one answer in the top *k*. Report k = 5 and
  k = 10. This is a success rate ("did the agent land on a right place within k candidates"),
  not the fraction of all answers retrieved.
- **MRR**: the mean over queries of `1 / rank` of the first answer in the list, or 0 when no
  answer is in the top 100.

Report every metric for all queries and per category. List the queries missed at 10 for each
method, so misses can be inspected. Keep the historical 50-query denominator, `queries.json`,
resolved commit/IDs, saved vectors, input hashes and receipts unchanged. Do not merge the
supplement into those historical metrics or silently re-resolve the historical answers.

## Separately pinned exact-name supplement (#389)

[`exact-name-queries.json`](exact-name-queries.json) is a six-query supplement, not a revision
of the historical 50. Its SHA-256 is
`e097a948b727bc84037dc82031b61ccf63855221d93a2a1a9d69c4c1ab6a9789`.
It includes raw `read_status_for` and `enqueue_request` snake_case queries, `renderSequence`
camelCase, and the real same-name production/test `compile` alternatives, with `any`,
`production` and `test` role requests. The two `compile` declarations share a production file;
this exercises stored roles rather than path guesses. No kind preference is implied.

The file has format `trellis-semantic-search-exact-name-eval/v1`, its own `resolvedAt`, and
queries with `id`, `category:"exact_name"`, `shape`, raw `query`, effective `role`, `answers`
and `expectedFirstRole`. Answers add measured `name` and `role` to the historical answer
fields. `qualifiedName` joins only named measured ancestors, omitting unnamed impls; it is a
resolution hint, not an identity or ranking input. Use the measured declaration IDs.

Resolution is from the stopped private #379 native snapshot used in the #389 investigation:
index generation `7cec6dda-27a5-4682-9268-0f5e2fd74b24`, revision 1, and native revision
`revision:v2:8375127b5ad1671bedc462032712e74575a954a1a9af6cbf6891b2afde0328f8`.
The supplement pins captured-source hashes for each selected file. `baseCommit` is #379's
`49ca2fcccf0a82b801ae0761de314bccbc0512f4`, **not a claim that captured bytes equal that commit**:
the capture may include uncommitted work, so `sourceCommit` is null. It is neither the original
issue dogfood pin nor a new #389 capture. Current checkout declarations must be re-resolved at
their own pin before reporting current-head results; do not copy these IDs as unmeasured labels.

Report the supplement separately: top-1 answer success **with the required role**, exact-name
group order (production before test for `any`), role-filter purity, and per-query old/new ranks,
names, kinds, stored roles and `matchedBy`. Any-answer R@10 alone cannot establish the rank-1
or same-name-role requirement. Report old/new metrics per `shape` and role, including every
regression; the repeated `compile` requests are correlated checks, not independent trials.
The historical concept/named/error/JavaScript metrics keep their original categories.

No supplemental ranking run or query/card embedding receipt is implied by these labels.
Historical saved query vectors do not cover these new strings, and historical card vectors
do not establish current native-card quality. Untimed functional replay or correctness fixtures
must identify their real inputs and distinguish lexical-only from hybrid results. Do not
substitute the old BM25 candidate list for the amended lexical rule. New inference or timed
measurements need a separate explicit plan.

## Running it

The benchmark harness is local evidence, not part of the repository. In outline:

1. Index `resolvedAt.commit` with a release `trellis` in an isolated `HOME`.
2. Read the head revision's declarations, headers, parameters, calls and source bytes read-only
   from that `index.db`, and build cards.
3. Embed the cards with each local model, rank by cosine similarity, and score.

The results and the recommendation are on #211.
