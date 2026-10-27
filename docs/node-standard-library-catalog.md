# Node standard-library labels

This is a deliberately partial, code-shipped syntax-tier catalog, not host-runtime
detection or proof of installed modules, exports or members.

## Fixed source pin and exact labels

Node.js **v22.0.0**, annotated tag `ec49bec48284ab642db1d109d917c6ae3b695c13`,
commit **`12fb157f79da8c094a54bc99370994941c28c235`**.
Catalog identifier: `node-standard-library:v1`.

| Exact static specifiers | Catalog entry |
|---|---|
| `fs`, `node:fs`, `fs/promises`, `node:fs/promises` | `fs` |
| `path`, `node:path` | `path` |

No arbitrary prefix stripping or descendant acceptance. Exact labels precede
same-name npm declarations. Any remaining `node:` spelling is unknown, never npm.
Relative specifiers remain resolver-owned and never fall through to this catalog.

## Conservative npm exclusions

The following **56 exact case-sensitive bare roots**, including all their subpaths,
block npm inference without emitting a standard-library descriptor:

```text
_http_agent
_http_client
_http_common
_http_incoming
_http_outgoing
_http_server
_stream_duplex
_stream_passthrough
_stream_readable
_stream_transform
_stream_wrap
_stream_writable
_tls_common
_tls_wrap
assert
async_hooks
buffer
child_process
cluster
console
constants
crypto
dgram
diagnostics_channel
dns
domain
events
fs
http
http2
https
inspector
module
net
os
path
perf_hooks
process
punycode
querystring
readline
repl
stream
string_decoder
sys
timers
tls
trace_events
tty
url
util
v8
vm
wasi
worker_threads
zlib
```

This is intentionally more conservative than Node's exact full-specifier lookup:
`http/fake`, `fs/fake`, `path/posix` and `path/win32` stay unknown, even with a
supported npm declaration. These are false negatives, not builtin assertions.
Scoped `@scope/http` is not reserved by `http`. Bare `sea`, `test` and
`test/reporters` are not reserved: their Node forms require `node:`. Supported
captured declarations may label these bare roots as npm.

## Source provenance

All source links below use
`https://raw.githubusercontent.com/nodejs/node/12fb157f79da8c094a54bc99370994941c28c235/`.
No host `module.builtinModules`, configured runtime, installed Node tree or
package-manager command supplied catalog evidence.

| Pinned source path | SHA-256 |
|---|---|
| `doc/api/fs.md` | `44d7c053a5bc3365d3444458a298c815e239172d2673eeaf8457afff333b25c4` |
| `doc/api/path.md` | `91b3490839798ba57cabda2cdc7f6134ac9072fb5fc6dda381ef5a9a5401c385` |
| `doc/api/modules.md` | `76b6642fa8b4b437cb22ffa8afb893eaef94b62d767e57816e21981560b057f8` |
| `lib/internal/bootstrap/realm.js` | `984a46d68c4f5f02ba0f5f6a5b7d64eb664aef98a7c32db949186d1b260eccf4` |
| `node.gyp` | `ea028555006bb642e662b0201e3fe91da7b7738a1ec5290a922e8fe863d5c4f3` |
| `src/node_builtins.cc` | `d6f474e141182f971e9bd71a26648e36eead2af6a1b1e6a69cc3cb0e85ca6639` |
| `tools/js2c.cc` | `7ed042126a7d8c0805f23655e8a4546aa5b2d8dc01e9e4d1f74b8ad26360d3c8` |
| `lib/internal/modules/esm/resolve.js` | `f8a0f1117a6687be2302e0e912fa8f20c31a71dfcea5f392f9afaf28db092cd3` |
| `doc/api/module.md` | `800abf8120fb922926048579a11046493b1a3ca601583196942961a5184319dd` |

`doc/api/fs.md` and `doc/api/path.md` document the exact label spellings;
`doc/api/modules.md` documents core precedence. `node.gyp` supplies the whole
`lib` directory to `tools/js2c.cc`, which recursively includes JS/MJS inputs and
forms module IDs by stripping `lib/` and the extension. `src/node_builtins.cc`
installs these IDs for `lib/internal/bootstrap/realm.js`. The official recursive
Git tree at the commit has `truncated:false`: 316 JS/MJS lib inputs, 71 outside
`lib/internal/`. realm.js excludes `internal/` and identifies only `sea`, `test`
and `test/reporters` as scheme-only. Thus 68 bare public full IDs have 56 roots.
Deprecated and underscore-prefixed public IDs remain in the exclusions; C++
code-cache/availability categories are not a public-inventory admission filter.
`lib/internal/modules/esm/resolve.js` checks the complete bare builtin specifier
before parsing package names or self-references. `doc/api/module.md` describes
the public inventory, but no runtime inventory API was executed.

Captured full-tree SHA-256:
`3dc6468ab9ae696a02e7a1122d7a6b1c5f1d7d2e0975633d2fab0e5a3d9d0241`.
Filtered lib-path list SHA-256:
`23e33d45e6df572881c62572b5b74415bba42c611d1045d8ed07c6441e99bde4`.
These inputs describe ordinary distributions at the fixed pin, not custom linked
builtins, exposed internals, modified embeddings, future versions or the checkout's
runtime. Expanding label coverage requires reviewed source entries.
