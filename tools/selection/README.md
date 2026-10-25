# Trellis view-selection tools

**Historical research only.** The Tools/provider runtime was retired by the [owner ruling 2026-10-24, #305](https://github.com/SquashMerge/trellis/issues/305#issuecomment-6394175295). This harness is not a supported runtime provider and is not run by normal startup or verification. Its closed fixture ledger and outputs are retained evidence, not a new execution allowance. Do not run provider or replay entrypoints or change the closed budget without separate owner authorization.

Compare a deterministic graph baseline, Jev typed relevance decisions, and a Claude
agent reached through ACP with graph tools exposed over MCP. This is a controlled
selection experiment, not a test of unrestricted agent investigation.

## Status

The smoke and three harder questions are complete. See [HARD-RESULTS.md](../../docs/research/selection/HARD-RESULTS.md).
The inference ledger is now closed; do not reset it or run more paid calls without new approval.
Ten source-grounded questions and their must-show rubric are in [QUESTIONS.md](../../docs/research/selection/QUESTIONS.md).
The rubric is **provisional and requires user review**. Only q01-smoke is selected
for the initial live check; do not launch the full benchmark automatically.

## Reproduce offline setup

Requires Node 24 and the saved `tests/fixtures/extraction/feature-factory.graph.json`
snapshot (paths in this paragraph are repository-relative).

```sh
cd tools/selection
npm test
```

Install pinned dependencies separately with `npm ci` only if needed; installation
can use the network. `npm test` does not download packages, execute Feature Factory,
run an indexer, call providers, or change the closed inference ledger. Budget unit
tests use synthetic ledgers in temporary directories. The MCP smoke test launches
only the local test server, not a provider adapter.

From this directory, saved packets and results are under
`../../tests/fixtures/selection/{inputs,outputs}/`. Questions and rubrics are under
`../../docs/research/selection/`. Research JSON and provider outputs retain historical
embedded paths; those strings describe the original run, not current defaults.

`prepare.mjs` builds fixed candidate packets and deterministic selections for all ten
questions, without network calls. Packets include only the question, seed, bounded
source/graph context and candidate IDs. Human must-show facts, distractor notes and
other providers' outputs are excluded.

## Historical smoke calls — ledger closed

Store `JEV_KEY` in the repository-root `.env` (ignored by Git). Never paste or commit it.
Configure Claude's existing subscription login yourself; this experiment never logs
in, switches accounts, or falls back to API-key authentication.

Live entry points `jev.mjs` and `acp-smoke.mjs` remain for reference, not as an
instruction to rerun. No further inference is authorized. Both use the same relocated
closed ledger: `../../tests/fixtures/selection/outputs/budget.json`. It was moved
byte-for-byte. Do not replace it, reset it, or create a new ledger to bypass closure.

These entry points make live inference calls. Existing results should be inspected rather
than overwritten or rerun. The initial experiment has a $10 total authorization;
every request reserves allowance in `../../tests/fixtures/selection/outputs/budget.json` before dispatch. Keep that
ledger across runs. Do not delete/reset it to bypass the cap.

Jev reserves $0.10 per request and reports a usage-derived estimate using its published
$0.042 per million input tokens. The corrected Claude run reserves $5 and is configured with
`maxBudgetUsd: 3.00`, `maxTurns: 4`, and a 4,096-output-token ceiling. The user explicitly
authorized Opus usage on their subscription beyond the initial $2 reservation. No
automatic retries, login, or fallback model is allowed. Unknown costs retain their
full reservation. Claude's SDK threshold is an estimate, **not an authoritative
provider invoice cap**. These small smoke limits leave substantial headroom; stop if
billing behavior is unknown or unexpected. Cost reports are not confirmed invoices.

## Selection contract

All providers classify the same candidates:

- `essential`: removing it loses a central step/safeguard for this question.
- `supporting`: useful when expanded.
- `incidental`: does not help this question.
- `uncertain`: evidence is insufficient.

Deterministic assembly keeps the seed, shows essential nodes, collapses supporting and
uncertain nodes, and records incidental nodes as hidden. It flags more than 12 visible
nodes rather than silently dropping essential facts. It creates **no new call edges**.
Callback references and unresolved dispatch remain boundaries.

The baseline shows the seed and one-hop internal/callback neighbors; two-hop nodes are
supporting. Disconnected name matches are uncertain, others incidental. It is a simple,
explicit baseline, not a tuned classifier.

## ACP safety and measurement

The pinned Claude adapter receives no API-key environment overrides. The requested
Opus model must be confirmed through ACP session configuration before prompting. It runs in an
empty temporary directory, with built-in tools disabled, settings sources disabled,
no client filesystem/terminal capability, and only three explicitly allowed MCP tools:
`get_candidates`, `get_candidate`, and `emit_view`. Only requests for the exact experiment MCP tools with validated arguments may be
approved; all other permission requests are denied.
This is a tool-restricted session, **not an OS sandbox**. The adapter still uses its
existing account state and may perform provider/SDK operations.

The agent must submit a validated MCP tool call, not JSON parsed from chat. The MCP
server has fixed input/output paths and cannot accept arbitrary file paths. Logs are
local and ignored; raw adapter diagnostics are discarded rather than risking secret
exposure. Token-equivalent cost is not the same as actual subscription cash charges. Packet/source data remain local except the authorized provider requests.

Reported latency is end-to-end integration latency. Claude includes process/session
startup and multiple model turns; Jev includes its HTTPS request. It is not an
isolated model-compute comparison. Do not infer calibration or model superiority from
one easy smoke question.

## Limitations before a full benchmark

- Candidate scopes are manually specified in the question fixture; retrieval is not
  being compared. Most large questions hit the 48-candidate cap.
- Long source bodies are head/tail excerpts. Missing source facts are input-coverage
  limitations, not automatically selection errors. Verify coverage before scoring.
- The smoke view tests selection and measured direct edges, not a full sequence renderer.
  Serialization/branch facts still require source-backed detail; symbol presence alone
  does not earn fact coverage.
- Jev probabilities/confidence are preserved as returned, but no calibration conclusion
  is possible until reviewed labels and enough independent examples exist.
- Full investigation by an ACP agent would be a separate experimental condition.

See [JEV-API.md](../../docs/research/selection/JEV-API.md) and [ACP-SETUP.md](../../docs/research/selection/ACP-SETUP.md) for pinned interfaces and
known provider limitations. Raw inputs, outputs, blind mappings and the budget ledger
are retained as inert fixtures because they contain source excerpts or evaluation material.
Do not execute source snapshots or expose raw provider artifacts through the viewer.

## Initial failed integration attempt

The first ACP attempt is preserved in `../../tests/fixtures/selection/outputs/q01-smoke.claude.json`. It authenticated
the existing team subscription but returned no validated view. Its session configuration
reported Opus despite a Sonnet option, one permission was denied, and it reported
$0.523703 of estimated token-equivalent usage. A duplicated MCP packet payload was
also removed. The full $2 reservation remains held. The corrected attempt uses a
different output filename and is an integration repair, not a hidden answer retry.

## Review the completed smoke

The corrected Opus result is `../../tests/fixtures/selection/outputs/q01-smoke.claude-v2.json`. All three validated
outputs were rendered to `../../tests/fixtures/selection/outputs/smoke-review/review.html`. Open that file directly,
or run `npm run review` and visit http://127.0.0.1:8875/review.html. The loopback server
serves only that HTML file, never the blind key, provider outputs or `.env`.
The provider mapping is `../../tests/fixtures/selection/outputs/smoke-review/blind-key.json`; leave it closed while
reviewing. The smoke cannot establish calibration, quality differences or general
latency/cost performance. No full benchmark has been run.

## Three harder reviews

```sh
npm run review:hard
```

Open http://127.0.0.1:8876/. The HTML-only server exposes the three generated reviews,
not private mappings, raw results or credentials. These are call-selection sketches,
not sequence diagrams. Start with the third question, where the models differ.
Inputs have complete scoped files and all35/40/73 candidates. The stricter human rubric
is in [HARD-QUESTIONS.md](../../docs/research/selection/HARD-QUESTIONS.md). Two Opus panels use validated emissions from terminal-error
sessions and show that caveat. Earlier failed and context-limited attempts are retained.

`prepare-hard.mjs --run` is offline but replaces saved input and baseline fixtures.
It is not required to view existing results or run tests. The same applies to
`node prepare.mjs`. Preserve historical artifacts unless replacement is approved. `--inline-context` on the ACP runner supplies the
compact evidence in the prompt and keeps MCP for structured output; it was needed to
avoid file-persisted tool-result delivery on the hardest case. This does not explain
all observed SDK context overhead. Read [HARD-RESULTS.md](../../docs/research/selection/HARD-RESULTS.md) before interpreting latency,
cost or selection quality. No further inference is authorized under the closed ledger.
