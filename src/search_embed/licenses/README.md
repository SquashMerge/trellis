# Embedding port attribution

`../qwen3.rs` is adapted from Hugging Face Candle Qwen3, 0.11.0,
commit `31f35b147389700ed2a178ee66a91c3cc25cc80d`. The adaptation adds a full
bidirectional valid-key padding mask and the Voyage tokenwise projection, and removes
causal generation and KV-cache paths. We use the Apache-2.0 license option for this
adapted material. Candle's distributed license is preserved in `CANDLE-LICENSE`.

The architecture follows `voyageai/voyage-4-nano` revision
`5fb34e870f49f19e79c94d5e118f5ee730133279`, licensed Apache-2.0. Its license and notice
are retained in `VOYAGE-LICENSE.txt` and `VOYAGE-NOTICE.txt`. The custom Python code and
model weights are not bundled or executed by the Rust implementation.

The committed numerical reference fixture under `tests/fixtures/semantic-search/` was
produced using that model. Its pin and evaluation-source hashes are recorded in the fixture.
These third-party materials retain their upstream licenses, not Trellis's BUSL terms.
