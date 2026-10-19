# Semantic search evaluation set

Status: **proposed, not ratified**. This is the step 2 evaluation set for semantic symbol search
([#211](https://github.com/SquashMerge/trellis/issues/211)). It measures retrieval quality and
defines no product behavior. See
[`../semantic-search-contract-proposal.md`](../semantic-search-contract-proposal.md) for the
contract proposal.

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
Answer = {language, path, qualifiedName, kind, declarationId}
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
    not on a body-only edit. When the code moves, re-resolve from the path, qualified name and
    kind, and update the ID.
- `category` is one of:
  - `concept`: describes behavior, and the declaration's name doesn't help;
  - `named`: a well-named lookup, where the words are close to the identifier;
  - `error`: an error or status message, looked up by its text;
  - `js`: the browser JavaScript, for cross-language coverage. The index holds Rust and JavaScript
    together, so every query competes across both languages.

The candidate set is every **named** declaration in the head revision of a Trellis index of
`resolvedAt.commit`. That includes tests and test fixtures, because an agent's search sees them
too. All answers are production declarations.

## How the queries were written, and how restatement was avoided

1. **Drafted blind.** The queries were drafted in one pass from behavior descriptions in the
   contracts (`local-topology.md`, `mcp-readonly-pilot-contract.md`, `contract-v1.md`, the
   decision records), `CLAUDE.md` and the issue text, **before** searching the code for answers.
   The draft list is kept with the benchmark evidence.
2. **Answers resolved afterwards.** Each answer was then located by reading the code. It was
   recorded as a `path:line` anchor inside the declaration, then resolved mechanically to the
   innermost named declaration in the index.
3. **No rewording toward the answer.** A drafted query was not reworded once its answer's name
   was known:
   - Two drafts were dropped, because the behavior they describe has no single home in the code:
     a "racily clean mtime" re-hash, and warning ordering, which is inline in the projections.
   - One draft (`q46`) was rephrased, because the behavior it named ("when a call is clicked")
     isn't how the browser navigates; it navigates from a source line.
   - For one draft (`q45`, saving a view or note), the saving code is an anonymous form callback,
     with no named declaration. The answers are the named functions that build the saved
     payloads.
   - The five drafted error queries named error *codes*. They were replaced with the message
     *text* an agent would actually see, taken verbatim from the code.
4. **Labelled overlap.** Queries in the `named` category deliberately share words with the
   identifier. They are labelled so their easier lookups can be reported separately.
5. **Measured overlap.** For every query, the benchmark measures the share of the answer's
   split-identifier name tokens that also appear in the query. For `concept` queries the median is
   reported with the results on #211. A low value means the query doesn't restate the name.

## Metrics

Each method returns a ranked list of declarations, cut at depth 100. For query `q` with answer set
`A(q)`:

- **recall@k**: the fraction of queries with at least one answer in the top *k*. Report k = 5 and
  k = 10. This is a success rate ("did the agent land on a right place within k candidates"),
  not the fraction of all answers retrieved.
- **MRR**: the mean over queries of `1 / rank` of the first answer in the list, or 0 when no
  answer is in the top 100.

Report every metric for all queries and per category. List the queries missed at 10 for each
method, so misses can be inspected.

## Running it

The benchmark harness is local evidence, not part of the repository. In outline:

1. Index `resolvedAt.commit` with a release `trellis` in an isolated `HOME`.
2. Read the head revision's declarations, headers, parameters, calls and source bytes read-only
   from that `index.db`, and build cards.
3. Embed the cards with each local model, rank by cosine similarity, and score.

The results and the recommendation are on #211.
