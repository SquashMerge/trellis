# Research evidence and historical validation

These reports record earlier extraction and view-selection work. They are historical evidence,
not a claim that every prototype is part of the current daemon or that provider trials may resume.

- [Extraction results](EXTRACTION-RESULTS.md)
- Extraction reports and screenshots: [extraction/](extraction/)
- View-selection reports and methodology: [selection/](selection/)

Runnable offline harnesses live in `tools/extraction` and `tools/selection`. Reusable source/data
fixtures live in `tests/fixtures/extraction` and `tests/fixtures/selection`. See each tool's README
for its historical commands. The selection harness is not a supported runtime provider and is not
part of normal startup or verification after #305; its entrypoints must not be run without separate authorization. Public text captures with local-path redactions are labelled, with
byte-exact originals retained privately. Other captures may retain historical paths and timestamps;
they are not current runnable instructions. Feature-factory evidence is intentionally retained.

Private application-specific evidence is excluded from the repository. Sanitized public summaries
identify their limits rather than presenting replacement examples as observed results.

The selection budget ledger in `tests/fixtures/selection/outputs/budget.json` is the original closed
ledger, relocated without changing its bytes. It is historical accounting, not a fresh allowance.
Do not reset it or run paid/provider trials without separate authorization. Tests may use isolated
synthetic temporary ledgers; they must not mutate this one.
