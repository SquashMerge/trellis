# Synthetic cohorts

Generate the current cohort with:

```sh
node tools/synthetic-cohorts/generate.mjs --out /tmp/trellis-cohorts
node --test tools/synthetic-cohorts/test/cohorts.test.mjs
```

The destination must not exist. The pinned `manifest-v2.json` records every source
path, byte length and SHA-256 hash. Manifest version 2 replaces only the LARGE
source cohort; the seed and SMALL/MEDIUM source bytes stay unchanged. Earlier
cohorts are available from earlier git revisions, not a second generator mode.

LARGE keeps 10,000 source files, 134,217,728 source bytes and 2,500 files per
language. Each ordinary file has 36 worker functions, the exported `f0` leaf,
and the existing type/relationship scaffold. Rust includes 40 module roots in
its 2,500-file budget. The generator distributes seeded numeric terms among
existing call-bearing return expressions to use the exact byte budget without
adding declarations, repeated calls, comments or call-free padding. Both
imported peers remain called. The `f0`, `f1`, `f2` and class edit markers remain
available for body, declaration-surface and class-projection warm measurements.

The composition checker applies LARGE-only callable bounds (38–40 bodies,
at most 40 Java signatures). SMALL/MEDIUM retain the 64–80 body bounds and 72
Java signature ceiling. All other call, peer, type, relationship, body-shape
and padding quotas apply unchanged to every tier.

Callable-body counts are not graph-symbol counts. Measure density with the
production native indexer, including all module roots and indexed local and
parameter declarations. The LARGE acceptance range is 115–120 graph symbols
per source file across the full 10,000-file cohort. Record per-language means
as well; the languages index different declaration kinds, so their individual
means need not fall in that range. Run parser/indexer tests and performance
measurements only under the project's verification-lock policy.
