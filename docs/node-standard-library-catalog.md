# Node standard-library labels

This is a code-shipped syntax-tier catalog of the Node.js builtin modules, not
host-runtime detection or proof of installed modules, exports or members.

## Owner ruling and source pin

`node-standard-library:v2` labels every Node 22 builtin root, in both its bare and
its `node:` spellings (owner ruling 2026-11-18,
[#1088](https://github.com/SquashMerge/trellis/issues/1088)). It replaces `v1`, which
labelled only `fs` and `path`.

Node.js **v22.23.5** (the newest Node 22 release when the ruling was built),
annotated tag object `7f419acfafbd284978a13f7d65487ed3f98f523f`,
commit **`60553651579ec33ca1aef9e1eedab2c3855f1f7e`**.
Catalog identifier: `node-standard-library:v2`.

Its public builtin IDs are those of v22.0.0 (the `v1` pin) plus `sqlite`, added in
v22.5.0; none was removed.

## Exact labels

A static specifier gets a label exactly when it is one of the 72 public builtin IDs
below, either behind `node:` or, for the 68 IDs Node also loads bare, without it.
The catalog entry, and the package of the identity, is the ID's first segment: one
of **59 roots**.

| Public builtin IDs | Catalog entry |
|---|---|
| `_http_agent`, `_http_client`, `_http_common`, `_http_incoming`, `_http_outgoing`, `_http_server` | the ID itself |
| `_stream_duplex`, `_stream_passthrough`, `_stream_readable`, `_stream_transform`, `_stream_wrap`, `_stream_writable` | the ID itself |
| `_tls_common`, `_tls_wrap` | the ID itself |
| `assert`, `assert/strict` | `assert` |
| `async_hooks`, `buffer`, `child_process`, `cluster`, `console`, `constants`, `crypto`, `dgram`, `diagnostics_channel` | the ID itself |
| `dns`, `dns/promises` | `dns` |
| `domain`, `events` | the ID itself |
| `fs`, `fs/promises` | `fs` |
| `http`, `http2`, `https` | the ID itself |
| `inspector`, `inspector/promises` | `inspector` |
| `module`, `net`, `os` | the ID itself |
| `path`, `path/posix`, `path/win32` | `path` |
| `perf_hooks`, `process`, `punycode`, `querystring` | the ID itself |
| `readline`, `readline/promises` | `readline` |
| `repl` | `repl` |
| `stream`, `stream/consumers`, `stream/promises`, `stream/web` | `stream` |
| `string_decoder`, `sys` | the ID itself |
| `timers`, `timers/promises` | `timers` |
| `tls`, `trace_events`, `tty`, `url` | the ID itself |
| `util`, `util/types` | `util` |
| `v8`, `vm`, `wasi`, `worker_threads`, `zlib` | the ID itself |
| `node:sea` only | `sea` |
| `node:sqlite` only | `sqlite` |
| `node:test`, `node:test/reporters` only | `test` |

So `node:crypto` and `crypto` both label `crypto`, `path/posix` labels `path`, and
`node:test/reporters` labels `test`. `sea`, `sqlite`, `test` and `test/reporters` are
scheme-only builtins: their bare spellings name npm packages, labelled as npm when a
supported declaration proves them. The underscore-prefixed IDs are deprecated but
public: Node lists them in `module.builtinModules`. The identity's `package` keeps
their single leading underscore, which the external-package spelling rules admit for
standard-library JavaScript and TypeScript identities only.

No arbitrary prefix stripping or descendant acceptance. Labels precede same-name npm
declarations. Any other `node:` spelling (`node:invented`, `node:fs/fake`,
`node:internal/util`) is unknown, never npm. Relative specifiers remain
resolver-owned and never fall through to this catalog.

## Conservative npm exclusions

The 56 roots of the 68 bare IDs, including all their subpaths, block npm inference.
A bare root or subpath that is not an exact builtin ID therefore gets neither label:
`http/fake` and `fs/fake` stay unknown, even with a supported npm declaration. These
are false negatives, not builtin assertions; Node itself checks the complete bare
specifier. Scoped `@scope/http` is not reserved by `http`. The scheme-only roots
`sea`, `sqlite` and `test` reserve nothing bare.

## Source provenance

All source links below use
`https://raw.githubusercontent.com/nodejs/node/60553651579ec33ca1aef9e1eedab2c3855f1f7e/`.
No host `module.builtinModules`, configured runtime, installed Node tree or
package-manager command supplied catalog evidence.

| Pinned source path | SHA-256 |
|---|---|
| `doc/api/modules.md` | `cc1d02a411107475f980c65cd8bec41e90886b9806de18fbf5b25f20e58176a7` |
| `doc/api/module.md` | `e595646faf20a284248c9654dba50c09928e8f00ba829022c13ae55321da57d8` |
| `lib/internal/bootstrap/realm.js` | `bd8fcaf6ba0938f776833aa764ded021df9c1dd5ae41094e08147f7cb4fb2ad2` |
| `lib/internal/process/pre_execution.js` | `2ce1ff6536d196e772e2c83a0ba5b8f5c23c89d183181cd993ad0a7a990413df` |
| `node.gyp` | `b7f1544d40de42a76c8c6c88a8615db560db43d05dbfeb3bcfca72e9f35d9aac` |
| `tools/js2c.cc` | `2c517f6067a8d7af09de0ef8ea348c1842a26ee2b548a377ab60f789c481b6d9` |
| `src/node_builtins.cc` | `b6ac2d0f93adc1e1b1c3ff22253b48dc40553d686cc987c59fac4dee113d553d` |
| `lib/internal/modules/esm/resolve.js` | `293d95ffc3145b5b35ca0d49304b4e2b9dff100b6d340f7c89d059b05dabb58e` |

`doc/api/modules.md` § Built-in modules documents core precedence and lists the
builtins with a mandatory `node:` prefix: `node:sea`, `node:sqlite`, `node:test` and
`node:test/reporters`. `doc/api/module.md` describes `module.builtinModules`, the
public inventory, but no runtime inventory API was executed. The `node_js2c` action
in `node.gyp` passes the whole `lib` directory to `tools/js2c.cc`, which recursively
includes its JS/MJS inputs except `lib/eslint.config_partial.mjs` and forms module
IDs by stripping `lib/` and the extension. `src/node_builtins.cc` installs these IDs
for `lib/internal/bootstrap/realm.js`. Its public IDs exclude `internal/`; its
`schemelessBlockList` is exactly `sea`, `sqlite`, `test` and `test/reporters`; and its
`experimentalModuleList` holds `sqlite` back until
`lib/internal/process/pre_execution.js` allows it, which it does unless
`--no-experimental-sqlite` is given. The official recursive Git tree at the commit
has `truncated:false`: 342 JS/MJS lib inputs, 73 outside `lib/internal/`, of which 72
are public builtin IDs. `lib/internal/modules/esm/resolve.js` checks the complete
bare builtin specifier before parsing package names or self-references.

Captured full-tree response SHA-256:
`7426ada957b1927351b2c89ac538ba4c2a5b2a08d37b2540e9196003945f651e`.
Filtered lib-path list SHA-256 (sorted, one path per line):
`ad4fb06afa476fb27a4c3c56313b9410d9ba341058c6aae39879dd7755cff2b7`.
The `v1` pin was v22.0.0, commit `12fb157f79da8c094a54bc99370994941c28c235`. These
inputs describe ordinary distributions at the fixed pin, not custom linked builtins,
exposed internals, modified embeddings, Node 23 or later, or the checkout's runtime.
Expanding label coverage requires reviewed source entries.
