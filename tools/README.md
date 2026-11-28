# tools

## Heavy runs and the shared lease

A full `./tools/verify` uses most of a machine. Where several sessions share one
machine (builders, review bots, containers on the same host), they take turns
through a shared lease.

- **When `MIMIR_HEAVY_LOCK_FILE` is set**, `./tools/verify` takes the lease by
  itself, for all lanes or for the lanes named with `--lane`. It waits for its
  turn, runs, and releases the lease. Arguments, environment and exit status are
  unchanged. You don't wrap the command in anything.
- **When it is unset or empty** (CI, an ordinary laptop), nothing changes and
  Python is not needed.
- **When the run is already inside the lease** (an outer wrapper took it and set
  `MIMIR_HEAVY_LOCK_HELD`), verify runs directly, so nothing waits on itself.
  `heavy_run.sh` and `heavy_run.py` do the same when they are called there.

### Tiers and the queue

The lease has two slots, and each job names its tier in `MIMIR_HEAVY_TIER`:

- **medium** holds one slot, so two medium jobs can run at once. Use it for
  verify, builds and tests whose timing is not being measured.
- **large** holds both slots and runs alone. Use it for measurements and anything
  that needs a quiet machine. The lease scripts treat a job that names no tier as
  large.

Waiting jobs queue first in, first out, so later medium jobs can't starve a large
one. A large job at the head of the queue keeps the slot it gets while it waits
for the other.

**Verify runs at the medium tier.** A caller that sets `MIMIR_HEAVY_TIER` keeps
its choice, so `MIMIR_HEAVY_TIER=large ./tools/verify` runs verify alone, for
example when the run's timing matters.

### Requirements and settings

The variable names a path in a directory that every participant can write. The
lease needs Python 3.9 or later and `timeout` (`gtimeout` on macOS).
`MIMIR_HEAVY_PYTHON` names the interpreter when `python3` isn't suitable. If the
variable is set but the lease can't be used (including an unknown tier), verify
fails instead of running without it.

`MIMIR_HEAVY_LOCK_TIMEOUT` (seconds, default 7200) covers the time spent waiting
plus the run itself. Raise it when the queue may be long. The other settings are
listed at the top of `heavy_run.sh`.

This is scheduling between cooperating sessions on one machine. It is not a
security boundary.

### Where the scripts come from

`heavy_run.sh` and `heavy_run.py` are copies of `scripts/heavy_run.sh` and
`scripts/heavy_run.py` from the public SquashMerge/mimir repository at commit
`495be6fe1866a4b49c73e2817a98494d79b3adbb`. `heavy_run.sh` is unmodified.
`heavy_run.py` has one Trellis change: when `MIMIR_HEAVY_LOCK_HELD` is non-empty,
`run()` runs the command directly, as `heavy_run.sh` does. Every participant must
run the same protocol, so don't change it here: change it in mimir, copy the new
files over and keep that one change. The SHA-256 hashes of the mimir files:

```
a7c52b56aa40214c17bdf79e51c8daa4753501b006b26984d394f211be98e5f7  heavy_run.sh
b315f4b2d2ba43299b7c2b06a045a22e382a29b9a2eddef6694066519dff8598  heavy_run.py
```

`heavy_run.sh` runs the `heavy_run.py` beside it, so the copies need no path
changes. Only its usage message still says `scripts/heavy_run.sh`.

`verify-heavy-lease-test.py` (in the `checks` lane) covers how verify uses them.
