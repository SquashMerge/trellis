# Main 0532a70 nested call oracle

`nested.js` is genuine JavaScript source indexed with the unmodified main binary
`~/projects/odin/trellis-evidence/s123/s3/bin/trellis-main-0532` (SHA-256
`536cdd7231b66c7a8e807d528084b6f00377a96d425ca6c5183a7a7728ff7d9f`).
The isolated main home is `/tmp/trellis-s3-tiny-main-home-eg39bmpo`. Its
`index.db` held the SQL rows for `oracle.json`; do not regenerate the oracle
with the S-3 writer. `oracle.json` SHA-256:
`31d00ec6611cab6e29ebc62533f787e23e436f16b1c1ca2b97fe410b1460c242`.

The SQL capture selected `id,payload` from each `graph_nodes`, `graph_calls`,
and `graph_regions`, ordered by `id`. It copied the stored UTF-8 `payload`
strings exactly and recorded each SHA-256. It also selected graph and class
projection hashes and IDs and the graph's document version ID. The old SQL
query-view ordering was checked with:

```sql
SELECT c.payload FROM graph_calls c
JOIN revision_documents m
  ON m.revision_id=?3 AND m.graph_projection_id=c.projection_id
WHERE c.caller=?1
ORDER BY c.path,json_extract(c.payload,'$.range.startByte'),
         json_extract(c.payload,'$.range.endByte') DESC,c.id
LIMIT ?2
```

Both measured calls begin at byte 75; the SQL returns the end-byte 83 call
before end-byte 81. With the main daemon using that isolated HOME, three
main CLI calls on this workspace and seed
`sid:v1:e493b84cc86c98985e2d910ae9a89b04` supplied the `queries`
expectations and full stdout SHA-256 values in `oracle.json`:

```text
trellis-main-0532 query --workspace <this directory> --seed <seed> --max-calls 1
trellis-main-0532 query --workspace <this directory> --seed <seed> --max-calls 2
trellis-main-0532 query --workspace <this directory> --seed <seed> --max-calls 1 --exclude-path nested.js
```

At `max_calls=1`, the second call is the `max_calls+1` overflow probe, so
`truncated` is true. At 2, both calls return without truncation. Excluding
`nested.js` removes both calls because it is the seed's path. No main source
here yields two calls tied on start and end byte; answer 8 requires a separate
direct comparator test on synthetic `CallSite` DTOs, not a forged artifact.
The original 112 main SQL payload rows in sibling `graph-payload-main-0532a70/`
remain untouched.
