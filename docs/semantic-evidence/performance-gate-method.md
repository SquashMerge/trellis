# #61 combined native + semantic-import gate: ratified method

Status: ratified method for the runner `tools/verify-61-combined-gate.py`. The
runner never produces authoritative acceptance: reports always carry
`authoritativeAcceptance: false` and `issue11: "open"`. Thresholds are read from
`benchmark-policy-v1.json` and `cohorts-v1.json`; this document restates none of
them as constants.

## What is measured

One contiguous interval per repetition, from the moment the full native
`trellis index --workspace W` request is issued to the final successful return of
`trellis semantic import --workspace W --batch <descriptor> --batch-sha256 <sha>`.
It includes native capture and indexing, semantic admission, decode, join, import,
SQLite publication and FIFO waits. `interval = t2.end - (t1.end - t1.duration)`
from monotonic clocks; only #90 `command()` bookkeeping (output read-back and observation
records, milliseconds) runs between the two commands, inside the interval, so it
only lengthens it; component times are never summed.

Excluded and recorded separately: cohort generation, artifact generation, the
header probe, batch preparation, daemon start, post-run validation and daemon stop.
Producer generation time is excluded.

## Cold means virgin Trellis state

Each repetition uses a fresh HOME/XDG, workspace, daemon and derived index. OS and
SSD caches are not purged; preparation and the header probe warm the page cache.
No OS-cold claim is made.

## Header probe

The descriptor binds the captured revision, source set and manifest hash, which
are functions of the binary, the canonical workspace path and the workspace bytes.
Per repetition the runner creates the workspace at its final path, starts a daemon
in a scratch `probe-home`, runs `trellis index`, reads the header and source
inventory from that index, stops the daemon and deletes `probe-home`, then writes
the batch into `W/.trellis-semantic-batch/`. The native capture skips hidden
directories, so the batch cannot change the revision. After the timed run the
runner asserts the published revision's source revision, source set, root and
manifest hash equal the probe's; a mismatch is a fault.

## Artifacts

Synthetic: `trellis-synthetic-scip` (profile `trellis-synthetic-scip-v1`,
`generatorSource` identity, trust origin `syntheticProfile`), one binary SCIP
artifact per language, four artifacts in one batch, every document covered. Artifact
hashes must equal `performance-gate-preparation.json`. No real-producer
qualification is claimed.

## Campaign

Three fixed repetitions for each of small, medium and large, in that order, with
no warmup, replacement or extra sample. Every original attempt is retained.

- A fault (source or cohort mismatch, hash mismatch, native-only or mixed result,
  missing coverage, request or pin disagreement, floor miss, preflight failure)
  stops the campaign as `unqualified`; the remaining repetitions are
  `notAttempted`. No re-run happens under the same campaign directory.
- A threshold miss on qualified data is `failed`, does not stop the campaign and
  leaves #11 open.
- The campaign directory must be absolute, nonexistent and, outside `--smoke`, not
  under `/tmp`, `/private/tmp` or `/var/folders`.

## Resources

- Worker RSS: `os.wait4` `ru_maxrss` of the daemon process (bytes on Darwin, KiB
  times 1024 on Linux). It is the daemon's lifetime high-water including setup and
  post-run validation, a conservative upper bound. CLI RSS is recorded separately.
- SQLite peak: a sampler stats `index.db`, `-wal`, `-shm` and stage directories
  every 50 ms from before the index command to after the import, plus a final stat.
  The method is not yet reviewed (`sqlitePeakMethodReviewed: false`), so the peak
  criterion is `notMeasured`.
- Steady storage is read after a clean daemon stop. The canonical serialized
  evidence denominator is unratified (#61 open item 2): `denominator.status` is
  `notMeasured`, `ratios.steady` is null and the steady ratio criterion is
  `notMeasured`. The absolute steady limit is evaluated.

## Qualification

`tools/semantic-contract/qualify-performance.py` `qualify(report, policy, cohorts)`.
A criterion is `notMeasured` in smoke, off the reference host, with a missing
repetition or with its instrumentation flag false. Wall uses the median of the
three raw samples; RSS requires every sample within the limit; the steady ratio and
peak rows apply to medium and large only. Status precedence is `unqualified` >
`failed` > `notMeasured` > `passed`; `passed` is unreachable while
`sqlitePeakMethodReviewed` and `denominatorRatified` are false. Runner stop reasons
are prefixed `preflight` or `fault`; only `preflight` reasons are read as
unqualified input by the qualifier, and a repetition with status `fault` also
makes the report unqualified.

The reference host is detected, never assumed: the profile id comes from
`benchmark-policy-v1.json` `referenceHost.profileId`, the profile file is
`hardware/<profileId>.json`, and the host is the reference when the platform is
Darwin and `hw.model` equals the profile's `observed.modelIdentifier`.

## Report schema

`report.json` (`formatVersion: 1`, `kind: "trellis.combined-gate-report"`, sorted
keys, trailing newline) and a deterministic `report.md`. Top level: `status`,
`statusReasons`, `authoritativeAcceptance`, `issue11`, `smoke`, `statements`,
`inclusion`, `campaign`, `policy`, `qualification`, `preparation`, `sizes`.

- `campaign`: argv, source (git HEAD, clean tree), binary path, SHA-256 and
  version, toolchain, hashes, `decoderVersion`, `schemaVersion`, and `host`
  (`profileId`, `profilePath`, `profileSha256`, `modelIdentifier`, `reference`,
  `inventory`).
- `sizes[]`: `size`, `repetitions[]`, `median.wallSeconds`, `peakWorkerRssBytes`,
  `evaluations{wall, rss, steadyAbsolute, steadyRatio, peakRatio}`.
- repetition: `ordinal`, `status` (`completed|fault|notAttempted`), `startedAt`,
  `finishedAt`, `probe`, `batch`, `interval{wallSeconds, commands[]}` with CLI
  stdout and stderr inline, `daemonLog`, `pins`, `requests`, `nativeFacts`,
  `coverage`, `rss`, `storage`, `cleanup[]`, and `fault` when faulted.
- In smoke every time, RSS, timestamp and `host.inventory` is null and
  `host.reference` is false, so smoke reports are byte-identical.

## Commands

    python3 tools/verify-61-combined-gate.py --campaign-dir <dir> --smoke
    python3 tools/verify-61-combined-gate.py --campaign-dir <dir> [--binary <release trellis>]
    python3 tools/semantic-contract/qualify-performance.py --report <report.json>
    python3 tools/verify-61-combined-gate-test.py

Smoke runs the small cohort and generator, writes a batch against a fixed header,
and builds the report with no binary, daemon, Cargo or timing. Before an
authoritative campaign: coordinate the host, check load and take the verify lock;
the runner itself takes no lock.
