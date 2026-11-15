# tools

## Heavy runs and the shared lease

A full `./tools/verify` uses most of a machine. Where several sessions share one
machine (builders, review bots, containers on the same host), they take turns
through a shared lease.

- **When `MIMIR_HEAVY_LOCK_FILE` is set**, `./tools/verify` takes the lease by
  itself, for all lanes or for the lanes named with `--lane`. It waits until the
  lease is free, runs, and releases it. Arguments, environment and exit status
  are unchanged. You don't wrap the command in anything.
- **When it is unset or empty** (CI, an ordinary laptop), nothing changes and
  Python is not needed.
- **When the run is already inside the lease** (an outer wrapper took it and set
  `MIMIR_HEAVY_LOCK_HELD`), verify runs directly, so nothing waits on itself.

The variable names a path in a directory that every participant can write. The
lease needs Python 3 and `timeout` (`gtimeout` on macOS). If the variable is set
but the lease can't be used, verify fails instead of running without it.

`MIMIR_HEAVY_LOCK_TIMEOUT` (seconds, default 7200) covers the time spent waiting
plus the run itself. Raise it when the queue may be long. The other settings are
listed at the top of `heavy_run.sh`.

This is scheduling between cooperating sessions on one machine. It is not a
security boundary.

### Where the scripts come from

`heavy_run.sh` and `heavy_run.py` are unmodified copies of `scripts/heavy_run.sh`
and `scripts/heavy_run.py` from the public SquashMerge/mimir repository at commit
`cebf3d1d2`. Every participant must run the same protocol, so don't edit them
here: change them in mimir and copy the new files over. Their SHA-256 hashes:

```
4eb0fe96820bbd1f48d9513720c2eba6ebf69bdad1f3cf2bb9bdf194bc93130d  heavy_run.sh
702c6a43f0847008a35409ce23467bccc39441836580c7ac83bed69f44adab46  heavy_run.py
```

`verify-heavy-lease-test.py` (in the `checks` lane) covers how verify uses them.
