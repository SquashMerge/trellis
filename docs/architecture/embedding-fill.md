# Faster embedding fill (#355)

## Scheduling and limits

The existing daemon fill driver still owns the only loaded model and all matrix
publication. Its lazy load and 600-second idle lifetime are unchanged. Source
paging stays at 16 cards; store read admission and trigram ranking are unchanged.

A subordinate reader checks the shared vector cache and tokenizes a window of at
most 512 scanned cards with a clone of the loaded model's tokenizer. It uses the
same prompt, byte cap, stripping, NFC, truncation and no-special-token policy.
Missing cards are stably sorted by actual prompted token count within that
window. In-flight hashes prevent duplicate work until writes commit. Cache
snapshot and pending-hash changes share one mutex so a committed vector cannot
fall between the cache check and deduplication.

The owner embeds slices of at most 128 documents and at most 8,192 padded tokens
(`rows * longest_sequence`). Thus 64-token inputs can use 128 rows; 128-token
inputs can use 64; 512-token inputs remain at 16. This bounds padded-token work
per forward to the previous 16-by-512 maximum, not elapsed runtime. Numerical
parity at the new shapes still requires the CPU and Metal acceptance run.

The prepared-window queue has capacity one. The reader can hold a next window
while one is queued and one is owned by inference: at most 1,536 prepared inputs,
each capped at 512 token IDs. There is one current 16-card source page. Text is
bounded by the existing card/input caps, not a new read-side field limit. The
writer has a capacity-one queue and a capacity-one acknowledgement queue; the
owner permits only one submitted write while embedding the next slice. There
are never concurrent model forwards or a second model instance.

The writer runs `put_many`; the original owner consumes its acknowledgement and
publishes the committed vectors to resident matrices. Source/tokenizer and
writer threads are operation-scoped and joined after channels are dropped on
all exits. Cancellation rejects unfinished inference/writes; already committed
content-addressed vectors remain useful. A prefetch read fault drains already
submitted writes and stays typed. A changed-head read retains committed progress;
a later existing hint reconciles the new head. Captured old-head work may finish
if its final read preceded the head change. It cannot falsely complete the new
pin.

## Query wait

FIFO queries run before and after every non-preemptible forward slice. They also
run while the owner waits for a prepared window or a write acknowledgement.
Producer/writer completion and query enqueue share the admission mutex and
condition variable, preventing lost wakeups without polling. Stage panics become
an embedding runtime failure and wake the owner. Existing query deadlines and
the absolute 3,000 ms optional-vector budget inside the 5,000 ms call deadline
are unchanged.

The scheduling worst case is the residual current forward slice, any earlier
FIFO query work, and owner handoff/publication work. A whole 512-card window or
cache write is not an uninterrupted model reservation. The token-work bound is
not a proof of a wall-clock maximum on a loaded host; actual in-fill wire latency
and the largest observed wait remain **unmeasured pending supervisor approval**.

## Matrix allocation accounting

The aggregate matrix allocation cap remains 2 GiB. Per-head fill scratch rises
from 53,248 bytes to 1,870,848 bytes, including overlapping int8 slices,
publication bookkeeping, and a 128-row normalized 2,048-F32 host transfer during
quantization. Model tensors/weights and tokenizer working sets are not matrix
allocations. The reservation remains live through physical staging cleanup and
worker joins. `examples/search_matrix_sizing.rs` mirrors this reservation; the
normal-size admission/lifetime tests use the production constant.

## Acceptance evidence

Evidence directory: `~/projects/odin/trellis-evidence/issue-355/`.

- Original API focused tests: 13 passed; test-support check passed.
- Fill regression suite after causal fixture updates: 24 passed.
- Combined fill, pipeline, and matrix admission/lifetime tests: 46 passed in
  the focused run; the final writer-panic regression also passed in full verify.
  Fixed-size fixture limits now add their unchanged matrix allowances to the
  production scratch constant.
- CPU and Metal new-shape full-corpus parity: passed (6,722 form-c cards,
  minimum cosine 0.9996462464 CPU / 0.9996463656 Metal; gold R@10 0.56
  unchanged, MRR 0.2862346824 → 0.2893460726 on both).
- Supplemental 128-card shape parity: passed on CPU and Metal; minimum cosine
  0.9999629259 / 0.9999628067 against the same saved reference rows.
- Saved-vector CPU/Metal matrix cold+delta replay: passed; hybrid R@10 remains
  31/50, with no MRR regression. This is not new inference parity.
- Quiet MEDIUM before/after throughput and actual full-fill wall time: pending
  supervisor authorization. Both stopped no-model indexes have 75,614 cards and
  empty vector caches. Historical loaded-host baseline was about 50 minutes;
  reserve roughly one hour per arm for planning, not an after-time/speedup claim.
- In-fill query wire latency, worst observed wait: pending. No timing run was
  authorized or performed in this PR preparation.
- Pinned Clippy (production and test-support): passed. Full verification:
  four lanes passed; tests lane failed in two unchanged MCP deadline fixtures
  with early `workspace_selection_failed/unavailable` responses. No rerun.
  See local `full-verify-01-result.json` and the PR for current status.

`semantic_search_parity --document-batch-shape bucketed` uses the production
prepared-reference path, 512-input windows, and original-order output. Its shape
receipts identify actual row/token counts. Default `serial` remains the old
16-input control. The diagnostic does not measure pipeline overlap or prove
query latency. Do not substitute historical saved-vector replay for inference
at the new shapes.
