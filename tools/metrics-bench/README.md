# Manual `metrics:v1` whole-revision benchmark

This is a manual report, **not a CI performance gate**. It measures complete `revision` DTOs for canonical `trellis-synthetic-cohorts-v1` medium (1,000 files, 16,777,216 source bytes) and large (10,000 files, 134,217,728 source bytes) cohorts. It never samples, requests aggregate-only data, changes the production 300-second work deadline or the 64 MiB response limit, or calls a typed error a full measurement. See [2026-10-18.md](2026-10-18.md) for observed results and failed attempts. Owner authorization to retry with a release build is [issue #120 comment](https://github.com/SquashMerge/trellis/issues/120#issuecomment-6244174345).

## Generate and index once

From a Trellis checkout with Python 3, Node.js, the Rust toolchain pinned in `rust-toolchain.toml`, native build prerequisites, and enough temporary disk space for the large index (over 15 GiB was observed):

```sh
export CARGO_TARGET_DIR=/tmp/trellis-factory-120-bench-target
mkdir -p /tmp/trellis-factory-120-metrics-bench
node tools/synthetic-cohorts/generate.mjs --out /tmp/trellis-factory-120-metrics-bench/generated
cargo build --locked --release --bin trellis
python3 tools/metrics-bench/run.py --binary "$CARGO_TARGET_DIR/release/trellis" --cohort medium --transport http --generated /tmp/trellis-factory-120-metrics-bench/generated --output-dir /tmp/trellis-factory-120-metrics-bench/results-release
python3 tools/metrics-bench/run.py --binary "$CARGO_TARGET_DIR/release/trellis" --cohort large --transport http --generated /tmp/trellis-factory-120-metrics-bench/generated --output-dir /tmp/trellis-factory-120-metrics-bench/results-release --file-on-error
```

The generator creates small too, but neither measurement selects it. Use one runner at a time. For a new home, the runner starts `trellis serve --workspace <generated>/<size> --bind 127.0.0.1:0 --token-file <external-token>`, discovers the `rootKey` with authenticated `GET /api/checkouts`, enqueues `POST /api/checkouts/{rootKey}/index`, and polls `GET /api/checkouts/{rootKey}/jobs/{id}` to `state=done`. **Indexing can take longer than 1,200 seconds.** Its setup backstop is now 7,200 seconds and is separate from the metrics 300-second work deadline. If a setup runner ends before an already accepted index job completes, restart it with `--resume-index` and the same output directory/home. Never enqueue a duplicate job for this purpose. The generator writes `manifest.json` with its seed, sizes, file checksums and totals. The runner verifies 1,000/10,000 on-disk source-file counts before indexing.

## Reuse an index for a truly cold new memo

The initial 2026-10-18 run indexed both cohorts externally with a debug binary. The approved release rerun reused those indexes and pins. To reproduce that exact arrangement rather than indexing again, replace the paths below only if your `HOME`, token and pin are known. Each `--reuse-home` run verifies generation and that the selected revision is not newer than the current head. The **actual pinned metrics request** decides whether that revision is still retained; startup watcher work can publish a newer head without invalidating the selected pin.

```sh
python3 tools/metrics-bench/run.py --binary "$CARGO_TARGET_DIR/release/trellis" --cohort medium --transport http --generated /tmp/trellis-factory-120-metrics-bench/generated --output-dir /tmp/trellis-factory-120-metrics-bench/results-release --reuse-home /tmp/trellis-factory-120-metrics-bench/results/medium-http-home --pin e8a17415-0fa6-435c-822c-5df7b847fe3d:2 --token-file /tmp/trellis-factory-120-metrics-bench/results/medium-http-token
# Stop only the completed medium HTTP runner's idle daemon; check the PID and binary first.
BENCH_PID=$(lsof -t '/tmp/trellis-factory-120-metrics-bench/results/medium-http-home/Library/Application Support/dev.squashmerge.trellis/run/daemon.lock')
ps -p "$BENCH_PID" -o command
kill -TERM "$BENCH_PID"
python3 tools/metrics-bench/run.py --binary "$CARGO_TARGET_DIR/release/trellis" --cohort medium --transport cli --generated /tmp/trellis-factory-120-metrics-bench/generated --output-dir /tmp/trellis-factory-120-metrics-bench/results-release --reuse-home /tmp/trellis-factory-120-metrics-bench/results/medium-http-home --pin e8a17415-0fa6-435c-822c-5df7b847fe3d:2 --token-file /tmp/trellis-factory-120-metrics-bench/results/medium-http-token
# If a pinned file request returns storage_busy, check GET /jobs/current is terminal
# and the selected pin still belongs to this index. Retry that exact file once:
python3 tools/metrics-bench/run.py --binary "$CARGO_TARGET_DIR/release/trellis" --cohort medium --transport http --generated /tmp/trellis-factory-120-metrics-bench/generated --output-dir /tmp/trellis-factory-120-metrics-bench/results-release-file-retry --reuse-home /tmp/trellis-factory-120-metrics-bench/results/medium-http-home --pin e8a17415-0fa6-435c-822c-5df7b847fe3d:2 --token-file /tmp/trellis-factory-120-metrics-bench/results/medium-http-token --file-only --file-path java/Cmedium0000.java
# Stop the idle medium daemon before starting large HTTP.
python3 tools/metrics-bench/run.py --binary "$CARGO_TARGET_DIR/release/trellis" --cohort large --transport http --generated /tmp/trellis-factory-120-metrics-bench/generated --output-dir /tmp/trellis-factory-120-metrics-bench/results-release --reuse-home /tmp/trellis-factory-120-metrics-bench/results/large-http-home --pin caa9c854-a489-4b72-969c-cae874135ceb:2 --token-file /tmp/trellis-factory-120-metrics-bench/results/large-http-token --file-on-error
# Stop idle large HTTP daemon and run CLI on same index/pin with an empty memo.
python3 tools/metrics-bench/run.py --binary "$CARGO_TARGET_DIR/release/trellis" --cohort large --transport cli --generated /tmp/trellis-factory-120-metrics-bench/generated --output-dir /tmp/trellis-factory-120-metrics-bench/results-release --reuse-home /tmp/trellis-factory-120-metrics-bench/results/large-http-home --pin caa9c854-a489-4b72-969c-cae874135ceb:2 --token-file /tmp/trellis-factory-120-metrics-bench/results/large-http-token --file-on-error
```

On Linux, the same daemon election lock is `<home>/.local/share/trellis/run/daemon.lock`. Long macOS paths put the socket under a private hashed `/tmp/trellis-*` directory; use the election lock, not a guessed socket path, to find the owned daemon. The runner terminates its `serve` subprocess on exit, but an idle daemon may remain. Only signal the known isolated benchmark daemon after no request is active. An index must not be copied into the operator's normal `HOME`.

## Interpreting output

Each attempt appends an external JSONL file `<output-dir>/<size>-<transport>.jsonl`. Use a fresh output directory for a fresh report, or distinguish appended attempts by their `setup` entry and release `binary`. The runner records full commands or URLs, `elapsedSeconds` measured by `time.perf_counter()` from request start until **all** bytes are received and JSON is parsed, pin, selected metrics, aggregate, `catchingUp`, row counts, and actual raw byte length. HTTP `responseBytes` is the full uncompressed response body; CLI `responseBytes` is complete pretty-printed `--json` stdout. Index and server setup are outside timed requests. `validFullResponse` needs every file (1,000/10,000), every expected directory (9/44 including root), zero revision declaration rows, and all 12 default selected metrics. `callablesComputed` sums each file row's declaration count; all declarations are computed even though the full revision DTO omits declaration rows. The same pin is passed to the second warm request without reindexing. A pinned Java `file` scope request then checks declaration rows. `--file-on-error` attempts that file even after a typed full-revision failure, but **does not** attempt or assert a warm full revision after failed cold. `--file-only --file-path java/Cmedium0000.java` runs only that bounded selected pinned file request after checking the recorded index job is terminal; it never substitutes for the revision result.

A response with HTTP 408 `deadline_exceeded`, HTTP 409 `storage_busy`, CLI nonzero exit, a parse error, or a client timeout is **not** a successful full response. Error body bytes are labeled error bytes, not large DTO bytes. The runner records typed error bodies/CLI stderr and exits 2 on failure. Do not increase timeout, silently retry full large requests, or infer a response size from an error. Reproduce report timings only under the stated OS/CPU and process conditions; an active watcher, `catchingUp`, and another process can change results.
