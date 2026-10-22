# Main 0532a70 graph payload vectors

`vectors.json` freezes **112 SQL-stored graph payload rows** (56 plain and 56 with SCIP) from the unmodified main writer at `0532a7090003904317136aeac98a23716aea1fc2`. Each node, call and region has its stored JSON `payload` string and its SHA-256. The test compares the raw UTF-8 payload bytes, not parsed/reformatted JSON, then checks graph hashes, graph projection IDs, the Java/Python class extraction `F` bytes, class hashes and class projection IDs. Do not refresh these bytes from `project_document` or the S-3 writer.

The baseline binary was recorded at `~/projects/odin/trellis-evidence/s123/s3/bin/trellis-main-0532` (SHA-256 `536cdd7231b66c7a8e807d528084b6f00377a96d425ca6c5183a7a7728ff7d9f`); its build record is `s3/build-main-0532.json` in that evidence directory. The checked-in vectors file SHA-256 is `8ea4f3e809b35f5d6378864e33f32548b0845e31a6dad13d09c2cfedd38538a9`. Each document also records its source SHA-256, and SCIP mode records `labels.scip` SHA-256. The extraction command/script that captured the main SQL rows was **not preserved** alongside this fixture; do not claim that their generation is reproducible from a recorded command.

Coverage: Rust, JavaScript (`javascript.js`), Python and Java; a separate Rust region-label boundary fixture; module/anonymous names; JavaScript accessors; nested regions and calls with ordered region IDs; SCIP display labels; a 140-character region label ending in `é` at the multi-byte cut; and children whose SyntaxId sorts before their parent. These vectors do **not** contain calls tied on start byte, tied on the full SQL sort key except ID, or an excluded call at a query-view limit boundary. Those require a new, genuine main-writer capture before they can be claimed as golden coverage. The unrelated `.ts` source and admission check were removed under S-3 answer 6; there is no TypeScript vector or ingestion change.

`native-main.json` is a separate, checked-in **main-produced native input** snapshot.
It was decoded from the main home `s3/golden-main-0532-home/Library/Caches/dev.squashmerge.trellis/indexes/b109e2ee53a3cdc52326a4e10c4b7fc186450d7aaa15e10cb8866ff5e5eaa7f4/index.db`
(SHA-256 `7216ed8f812ca86f4637d36df9a27e6c22677e59fb11c94ccad81b48528fe60c`).
Snapshot SHA-256: `8c47ae03958e0955b1e42d5d3310d6cb38be1b72d34658509a4392cd148d762d`.
For each `document_versions` row it copies the native declarations, calls and
control regions from `native_version_*`, including ordered ancestor/signature,
header, parameter and call-region child tables. Document/revision/proof fields
are reconstructed from the selected main SQL document and native revision; the
proof ID uses `trellis.native-proof.v1` over the stored producer, document key
and native revision. No node/call/region IDs come from the candidate graph or
from `vectors.json`. This keeps native IDs fixed when tests run from another
checkout inode. The test checks the snapshot hash independently before
projecting these typed native DTOs against the unchanged raw main payloads.
The one-off SQL-to-JSON conversion script was not preserved; the source DB
and its SHA above identify the original input, not a reproducible command.
