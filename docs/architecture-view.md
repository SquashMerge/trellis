# Architecture view

Architecture is a tab in Diagrams for reviewing dependencies and authoring reviewed
exceptions in the checkout. It shows authored layer bands and directed module references:
**structural references · syntax evidence**, plus fresh call and type bindings of each
language's selected semantic producer where one is imported
([#523](rules.md#semantic-bindings-523)). A native module is one
indexed file with its full `(sourceSetId, language, path, sid)` identity. Paths are labels,
not inferred package names or identities; module chips (below) only group them for display.

## Layers and evidence

The fixed checkout file [`.trellis/rules.toml`](rules.md) supplies all layer names,
membership and order. Smaller integer orders appear above larger ones. Equal orders are
separate peers. Shared layers sit beside the ordered stack and remain separate. Empty
layers stay visible. Unassigned modules are explicitly **Unassigned / unknown** outside
the stack; Trellis does not guess membership.

Missing rules or a configuration without layers shows setup help for the existing
`trellis_propose_rules` MCP prompt. The browser does not call a model. Authoring
requires an existing valid rules file. Editing or removing authored exceptions does not
require layers or current matching instances; new layer-pair exceptions require
positively witnessed production deviations.
Invalid or ambiguous rules show the located `invalid_rules` diagnostic, including the
fixed path, line and column, without a partial diagram.

Layer edges count reference **sites**, not module pairs or rule-check instances.
Production references are solid. Test references are dashed and toggleable. Stored
roles determine this split: `cfg(test)` is test, while `cfg(feature="test-support")`
remains production under Decision 0011. The **Test-only edges** overlay does not change production reference totals,
policy or metric role selection. The separate **Include tests and fixtures in metrics** checkbox changes heat
without changing which reference edges are shown.

Downward references, including skipped orders, conform. Upward references deviate.
Cross-peer and shared-to-ordered references deviate by default; the independent
`layer_policy` overrides may allow them. Incoming references to shared conform.
Reviewed exceptions retain their underlying verdict and reason. Upward counts carry
the word **deviation** as well as error color; peer/shared deviations are not called
upward references. Same-layer sites remain in evidence totals rather than module arrows.
Unassigned endpoints and unresolved sites remain explicit unknown evidence.
(#783: a checkout with two or more apps draws one layer stack per app; see
[Amendment (#783)](#amendment-783-apps-in-the-architecture-view).)

## Amendment (#783): apps in the Architecture view

**Status: owner-ratified 2026-11-07 (#783).** The rules side is
[rules.md Amendment (#783)](rules.md#amendment-783-apps-and-per-app-layers): app detection,
the `app` and shared `apps` layer keys, order compared only within an app, and the
`cross_app` verdict. This section says what the view and its reads must keep true. The
switcher and the drawing are [#789](https://github.com/SquashMerge/trellis/issues/789) and
[#790](https://github.com/SquashMerge/trellis/issues/790). The data is
[#784](https://github.com/SquashMerge/trellis/issues/784).

- **Apps are detected or authored, never guessed.** (#794: an app can also be authored in
  `.trellis/rules.toml`; see [rules.md Amendment (#794)](rules.md#amendment-794-authored-apps).
  The proposal prompt may draft authored apps for review; the browser never does.) The
  summary lists the apps,
  each with its name, its kind (`cargo`, `npm` or `python`; python added by owner ruling 2026-11-08, #807), its manifest path, its roots and its
  entries. It also gives each layer its app: `layers[].app` for an ordered layer, the served
  apps for a shared layer. #784 fixes the exact fields. The view never draws a layer or an
  app that `rules.toml` and the manifests don't define. It has no function-level layers:
  layers stay whole files.
- **One stack per app.** An app's view draws that app's ordered layers, including its hidden
  ones, by their own `order`. It also draws the shared layers that serve the app. With zero
  or one app, the view is the single stack above, with the same per-destination `↑` badges
  (see the #789 badge amendment under the band headers).
- **An app with no layers yet** lists its files as unassigned, under that app, and shows
  the `trellis_propose_rules` setup help.
- **Tooling, test harnesses and fixtures are hidden layers of their app, not apps** (#524).
  They wait in that app's hidden-layer note and its toggle. There is no app chip for them.
- **Unassigned files are listed per app,** by the most specific containing app root, plus
  a "no app" group. They are never dropped. The checkout-wide unassigned count is unchanged.
- **No cross-app arrows inside a stack.** A reference between layers that share no app is
  not drawn as a layer arrow or counted as a deviation. It is an `unknown` `$layers`
  instance with reason `cross_app`, counted with the other unknowns and listed in the review
  list. Interface edges between apps wait for
  [#686](https://github.com/SquashMerge/trellis/issues/686).
- **Counts equal the CLI per app.** A band's counts are sums over that app's `$layers`
  instances on the same pin and rules hash. A cross-app pair is never a deviation, so the
  apps' deviation totals cover every deviation in `rules.counts.deviates`. A deviation
  involving a shared layer counts under each app that the pair's layers have in common.
- **Exceptions are unchanged:** one file pair, previewed and applied through the existing
  routes.

## Apps and HTTP interfaces: data and extraction (#784, #788)

**Status: proposed (owner-delegated, run 803).** This section says what the summary and
detail reads carry for apps ([#784](https://github.com/SquashMerge/trellis/issues/784)) and
for HTTP interface evidence ([#788](https://github.com/SquashMerge/trellis/issues/788)). The
drawing is #789 and #790. App detection and the counting rule are in
[rules.md implementation notes (#784)](rules.md#implementation-notes-784); the evidence
meaning is the [contract-v1 #788 amendment](semantic-evidence/contract-v1.md). Detected app
kinds are `cargo`, `npm` and `python` (owner ruling 2026-11-08); `python` apps are detected
from `pyproject.toml` ([#807](https://github.com/SquashMerge/trellis/issues/807)).

**Summary fields.**

- `apps[]`: one entry per detected manifest, in manifest-path order, each with its `name`,
  `kind`, `manifest`, `roots`, `entries`, `counts` (production `$layers` and other rule
  instances given to the app, equal to the CLI's `apps[].counts`), `languages`, `fileCount`,
  `unassignedFileCount` and `source: "manifest"`. `fileCount` and `unassignedFileCount` count
  the modules whose app is this one. (#794: authored apps come first, by name, with
  `source: "override"`, `kind` and `manifest` null, `entries` empty and `roots` as authored,
  possibly several; a detected app an authored root shadows is not listed. The browser labels
  an authored app "Authored in .trellis/rules.toml" where a detected one says "Detected from
  <manifest> (<kind>)", and lists every root. See
  [rules.md Amendment (#794)](rules.md#amendment-794-authored-apps).)
- `modules[].app`: the app name of the module's file, or null.
- `layers[].app` and `layers[].apps`: the authored values, or null.
- `noApp`: `{counts, unassignedFileCount}`. It is null only in the summary head, before the
  rules are ready. With no detected app it holds every count. The two numbers count different
  things:
  - `counts` counts production **rule instances** that belong to no app. They are `cross_app`
    pairs, pairs between layers that share more than one app, rows attributed by a file that
    is in no single-app layer and outside every app root, and every instance when no app is
    detected.
  - `unassignedFileCount` counts **files** with no layer that lie outside every app root.
  - With every app's counts, `counts` sums to `rules.counts`, field by field.
- `interfaces[]`: one entry per ordered pair of apps `(client, server)` with at least one
  matched call, ordered by the client's inventory index, then the server's. With fewer than
  two apps it is empty. Each entry is:

  ```text
  {client,server,kind:"http",
   ends:[{clientLayer:string|null,serverLayer:string|null,routes:UInt,callSites:UInt}],
   wrappers:{callees:["api","request","api.request","owner.request","fetch","axios",...],
             prefix:{client:"/api/",server:"/api/checkouts/{root}/"}},
   counts:{matchedRoutes,callSites,serverOnly,clientOnly,
           confidence:{exact,template,wrapperInferred,unresolved}}}
  ```

  `wrappers.callees` lists the built-in wrappers, then `fetch` and `axios`, then every
  wrapper configured in `.trellis/rules.toml` (#855).
  `client` and `server` are app names. An end's app is the app of its file and layer under
  the `bucket` rule's file attribution. `ends[]` groups matched pairs by the layer of the call
  site and the layer of the handler (null sorts last): `routes` counts distinct routes and
  `callSites` counts call sites. `counts.confidence` counts call sites, matched and
  client-only. `serverOnly` is the number of the server's routes that no call matched, and
  `clientOnly` the number of the client's unmatched calls.
- Summary `edges` omit `cross_app` instances. They stay in `rules.counts.unknown`.
- A single-app checkout whose app root is a subdirectory, and which has unassigned files
  outside that root, reports part of its per-app output under `noApp`: `apps[0].counts`
  equals `rules.counts` only when no row's own file is unassigned and outside the app's root.

**Detail reads.**

- `kind=instances` gains an `app` filter. It matches the name of the instance's app (the
  `bucket` rule), and an app name that several apps share matches each of them.
- `kind=interfaces` pages the interface rows. Its filters are `client`, `server` and `list`
  (`matched`, `serverOnly` or `clientOnly`); any other value fails `invalid_request`. A reader
  passes `client` and `server` with every `list`. Paging, the pin and the `rulesHash` work
  as for the other kinds. The three item shapes are:

  ```text
  {client,server,list:"matched",
   entry:{method,pathTemplate,confidence,handler:H,callSites:[C]}}
  {client,server,list:"serverOnly",entry:{method,pathTemplate,handler:H}}
  {client,server,list:"clientOnly",
   entry:{method:string|null,pathTemplate:string|null,confidence:"unresolved",callSite:C}}
  H = {kind:"declaration"|"closure"|"dispatchArm",syntaxId:string|null,
       stableLookupKey:string|null,path,lines:{start,end},variant:string|null,
       layer:string|null}
  C = {document:{sourceSetId,language,path},range:{start,end},lines:{start,end},
       callId:string|null,caller:Caller|null,confidence,layer:string|null}
  ```

  `H.layer` is the layer that owns the handler's file and `C.layer` the layer that owns the
  call site's file; both are null when the file is unassigned. `lines` are 1-based and
  inclusive. `caller` is the measured caller of the call, as in rule evidence.
- Where two apps share a name, they are one group in the browser: one chip and one `app`
  filter value, with every manifest path listed.

**Extraction (#788).** Interface evidence is structural and read from the same admitted
snapshot as the rules. It is extracted only when two or more apps are detected (#794:
detected and authored apps together), with or without a rules file (#851: without one,
every detected manifest is an app, for MCP `trellis_endpoints`). Documents
with an evidence gap are skipped. Work is bounded; over the bound the read fails
`bounded_work`.

- **Server routes.** Rust axum `<expr>.route("/literal", <method-router>)` registrations
  with exactly two arguments. A method router is `get`, `post`, `put`, `delete`, `patch`,
  `head` or `options` applied to a handler, optionally path-qualified and chained; each
  (method, handler) pair is one route. `:name` is read as `{name}` and `*name` as `{*name}`.
  Routes are one per method and path template; the first by path and position wins.
- **Dispatch table.** A `match` on a two-element tuple inside `impl T { fn f(..) }`, with an
  arm of the form `(Method::M, "literal")`, is a dispatch table. The arm's variant `V` is the
  first `Self::V` or `T::V` in it, and arms nested inside the `_` arm give `<literal>/{*}`.
  An axum literal ending in `{*name}` whose named handler `F` calls `T::f(..)` is replaced
  by the table's routes: the literal's prefix plus the arm's suffix, with the arm's method
  (kept only if the registration includes it). Such routes are marked as coming from the
  table, which makes their matches `template`.
- **actix-web routes (#858).** Read in Rust documents that mention `actix_web` or `web::`:
  - a route attribute on a function: `#[get("/p")]` (any method above, optionally
    `actix_web::`-qualified) or `#[route("/p", method = "GET", method = "POST")]`; the handler
    is the function;
  - `<App or Scope>.route("/p", R)` and `web::resource("/p").route(R)`, where `R` is
    `web::<method>().to(h)`, `web::method(Method::M).to(h)` or
    `web::route().method(Method::M).to(h)`, with other links such as `.guard(..)` between.
    An `R` without a readable method (`web::route().to(h)`, `resource.to(h)`) is not a route;
  - prefixes: a registration inside the arguments of a call on a `web::scope("/p")` chain is
    under `/p`, and scopes nest. A function with a route attribute is under the prefix of
    each `.service(f)` (or `.service(path::f)`) that names it, in any document, matched by
    name; with no such registration it is at its own path. A scope whose prefix is not a
    literal hides the routes under it. A registration counts only on a proven chain: one that
    starts at `App::new()` (always the root), at an inline `web::scope(..)` or at
    `web::resource(..)`. Any other receiver, such as a variable bound to a scope, is mounted
    at an unknown prefix, so nothing registers under it or in its arguments.
    `.configure(..)` is not followed, and nothing in its callback registers.
  - the expansion of attributed routes under their prefixes is charged to the work bound
    before it is built.
  - `{name:regex}` is read as `{name}`; a last segment `{name:.*}`, `{name:.+}` or
    `{name}*` as `{*name}`; an unbalanced brace makes the path unreadable and the
    registration is not a route. Handler identity, exclusions and the one route per method
    and template rule are as for axum.
- **Python server routes (#856).** Read from production Python documents. Objects, mounts
  and views are followed only through bindings that resolve; anything else registers
  nothing, never a route at the root.
  - **Bindings.** A name is read only when it is bound exactly once in its document: no other
    assignment, `+=`, import or alias, parameter, `for` or comprehension target, `with .. as`,
    `except .. as`, walrus, `global`, `class`, `def`, `match` pattern or deletion of that name
    anywhere in the document. A module-level `from m import *` hides every name bound before
    it, and one inside a function or class hides every name of the document. A string
    constant is such a name bound by a module-level assignment. An object or view imported
    from another module must be bound in that module's own scope, not inside a function.
  - **Objects.** A Flask app is a name bound to `Flask(..)`, a blueprint to `Blueprint(..)`
    (with its `url_prefix`), a FastAPI app to `FastAPI(..)` and a router to `APIRouter(..)`
    (with its `prefix`), each class imported from `flask` or `fastapi` (`from flask import
    Flask` or `import flask`, then `flask.Flask`). Every Flask and FastAPI app is a root; a
    blueprint or router is reached only through registrations from a root.
  - **Routes.** On a Flask app or blueprint, `@o.route(rule, methods=[..])` (no `methods` is
    `GET`) and `@o.get/post/put/delete/patch(rule)`. On a FastAPI app or router,
    `@o.get/post/put/delete/patch/head/options(path)` and `@o.api_route(path, methods=[..])`.
    The handler is the decorated function. A `methods` value that is not a list, tuple or set
    of readable method names, or that a `**` splat may supply, registers nothing.
  - **Mounts.** Flask `parent.register_blueprint(bp, url_prefix=..)`: the registration's
    `url_prefix` replaces the blueprint's own, and a nested blueprint's prefix is joined to its
    parent's with one `/`. FastAPI `parent.include_router(router, prefix=..)`: the include
    prefix, then the router's own prefix, are prepended as written. Mounts are followed across
    documents through `import m`, `import a.b as m` and `from m import n` bindings: a relative
    import from the document's package, an absolute one from the one directory above the
    document that holds the module (two or more such directories are ambiguous, and resolve
    nothing), at most 16 deep. A prefix or object that cannot be read, or that a `**` splat may
    supply, mounts nothing. Only candidate documents are read: a module that mentions no
    framework word and is not a package `__init__.py` (such as a plain re-export module) is
    not parsed, so a mount that passes through it is omitted, never guessed.
  - **Django.** A module-level `ROOT_URLCONF = "pkg.urls"` in any document names a root
    urlconf; without one no Django pattern is a route. A urlconf's `urlpatterns` is its one
    module-level list or tuple, extended by module-level `+=` of lists. `path(route, view)` and
    `re_path(regex, view)` from `django.urls` are routes that answer `ANY`. `include(..)` of a
    module string, an imported module, a pattern list or an inline list mounts its patterns
    under the route, whose text is prepended as written; an include that cannot be resolved
    mounts nothing. The handler is the function the view resolves to, else the view's span.
  - **Normalisation.** Flask and Django `<name>` and `<converter:name>` (Flask's
    `<converter(args):name>` too) are `{name}`; FastAPI `{name}` and `{name:converter}` are
    `{name}`. The `path` converter is `{*name}` as the whole last segment, else `{name}`. A
    `re_path` regex must start with `^`; a final unescaped `$` is dropped, `(?P<name>..)` is
    `{name}` (`{*name}` for `.*` or `.+`) and an escaped punctuation character is itself. A
    `{name}` group body must provably never match `/`: literal characters other than `/`,
    `\d`, `\w`, `\s`, escaped punctuation other than `/`, and classes that exclude `/` (a
    positive class with no `/`, no range spanning it and no `\D`, `\W` or `\S`, or a negated
    class that names `/`), each optionally quantified. Any other regex syntax (such as `.`
    inside a body, or `[^x]+`), an optional or repeated group, a brace in a Flask or Django
    route, or an unbalanced or unnamed placeholder makes the route unreadable. A `re_path`
    whose regex ends in an unescaped `$` matches only an empty remainder, so an `include`
    behind it mounts nothing.
  - **Bounds.** Every production Python document is scanned for candidate words, and each
    candidate is parsed and walked, all charged to the work bound. Each object's routes under
    each prefix it is mounted at, and each Django list under each include, are charged
    before they are built.
  - **Duplicates.** Python routes are one per method, template and registering document.
- **Handler identity.** `handler.kind` is `declaration` when the handler names exactly one
  same-document function (the function's span), `closure` otherwise (the span of the handler
  expression), or `dispatchArm` for a table route answered by an arm that does not call
  exactly one same-document function (the arm's span). The answering arm is the first arm
  outside `f` that names `T::V` and does not use `unreachable!`, `panic!`, `todo!` or
  `unimplemented!`. If no arm answers, the handler is the declaration `F`. `variant` is the
  table variant `V` for every table route and null otherwise.
- **`syntaxId` and `stableLookupKey`.** They come from the innermost named callable that
  contains the handler (or the named function, for `declaration`), with keys from the
  declaration lookup keys. They are null when no named callable exists.
- **Client calls.** JavaScript and TypeScript calls to the built-in wrappers (there is no
  rules-file key for these; #855 adds `fetch`, axios and `[[interfaces.wrappers]]`, below):

  | Callee | Path | Method | Default |
  | --- | --- | --- | --- |
  | `api` | argument 0 | argument 1, a string literal | `GET` |
  | `request` | argument 0 | `method` in the object literal at argument 2 | `GET` |
  | `api.request` or `owner.request` | argument 0 | `method` in the object literal at argument 1 | `GET` |

  A string literal gives the path. A template literal gives its fragments with `{}` in place
  of each `${...}`. An escape sequence or any other expression makes the path unreadable. A
  readable path that does not start with `/` is not a client call. The path is cut at `?` or
  `#` and split on `/`; a segment containing `{}` is a substitution segment; a trailing empty
  segment is dropped. A method is unreadable when it is not a literal, when the options object
  has a top-level spread and no `method`, or when it is not one of `GET`, `POST`, `PUT`,
  `DELETE`, `PATCH`, `HEAD` or `OPTIONS` (after uppercasing). An unreadable path or method
  never matches and is `unresolved`.
- **JavaScript and TypeScript server routes (#855).** They are read from production
  documents.
  - **App objects.** A route is reached from an app object through its mounts. An app object
    is a `const`, `let` or `var` binding made by `express()`, `fastify()` (or `Fastify()`) or
    `new Koa()`, where the callee is imported or required from the `express`, `fastify` or `koa`
    package. Routers are bindings made by `express.Router()` (or a `Router` imported from
    `express`) or `new Router()` from `@koa/router` or `koa-router`. A router that no app object
    reaches registers nothing.
  - **Registrations.** On an express app or router, `x.get/post/put/delete/patch/head/options`
    and `x.all`, and the chain `x.route(path).get(h).post(h)`. On fastify, the same methods and
    `x.route({method, url, handler})`, where `method` is a string or an array of strings. On a
    Koa router, the same methods, `del`, and the named form `router.get(name, path, h)`. The
    path is argument 0. It is a string, a template literal or a `+` of them, with `const`
    bindings and imported constants resolved. The handler is the last argument, or the
    `handler` of a fastify options object. `all` registers the method `ANY`.
  - **Mounts.** express `use([path,] ...routers)`. Koa `use([path,] router.routes())`, a
    router's `prefix` option, and `router.prefix(path)`. fastify
    `register(plugin, {prefix})`, where the plugin's first parameter is the child object. A
    plugin wrapped in `fastify-plugin` shares its parent's scope, so `prefix` does not apply to
    it (Fastify Routes reference, "Route Prefixing and fastify-plugin"), unless it is wrapped
    with `encapsulate: true`. Wrapper options that cannot be read register nothing. Passing an app or router to a function, such as
    `routes(app)` or `require('./routes')(app)`, makes that parameter the same object. Mounts
    are followed across relative imports and `require`, through `module.exports`,
    `exports.name` and ESM exports, at most 16 deep.
  - **Unreadable paths and patterns.** A path or prefix that cannot be read, or that uses
    pattern syntax (`?`, `+`, `(`, `[`, `{`), registers nothing under it. A `use` whose first
    argument has no readable value (an unbound name, a call, a conditional) mounts nothing.
  - **Object members.** An options object is read in write order. A spread, a computed key,
    or an accessor or method of the key's name may set that key to anything, so the key is
    unreadable until a later explicit member sets it. This applies to `prefix`, `url`,
    `path`, `method`, `handler`, `baseURL` and `encapsulate`.
  - **Normalisation.** `:name` is read as `{name}`, `*name` as `{*name}` and a bare `*` as
    `{*}`. The mount prefix and the path are joined with one `/` and no trailing `/`.
  - **Handlers.** A handler that names a function declaration, or a variable bound to a
    function, is that `declaration`, even across an import. Any other handler is a
    `closure`: its own span, inside the named callable around it.
  - **Next.js.** A `package.json` that depends on `next` (in `dependencies`,
    `devDependencies` or `peerDependencies`) makes its directory a Next.js root. Under it, or
    under its `src/`, two kinds of file are routes:
    - `pages/api/**`, whose default export answers `ANY`;
    - `app/**/route.(js|jsx|ts|tsx|mjs|cjs)`, whose exported `GET`, `POST`, `PUT`, `PATCH`,
      `DELETE`, `HEAD` and `OPTIONS` (declarations, `const` exports or `export { h as GET }`)
      each answer their method.

    In the path, `[id]` is `{id}` and `[...rest]` is `{*rest}`. `[[...rest]]` gives two
    routes, one without the segment and one with `{*rest}`. Route groups `(g)` and slots
    `@s` are dropped. A segment starting with `_` or `(.` excludes the file, and so does a
    pages-router file named `_*`. `index` is its directory.
  - **Duplicates.** JavaScript routes are one per method, template and registering file.
- **JavaScript and TypeScript clients (#855).** Besides the wrappers above:
  - `fetch(path, options)`, including `window.fetch`, `globalThis.fetch` and `self.fetch`.
    The method is the `method` of the options object, as for `request`.
  - axios: `axios(config)`, `axios(url, config)`, `axios.request(config)`,
    `axios.get/delete/head/options(url, config)` and `axios.post/put/patch(url, data,
    config)` (and the `Form` variants). The same calls work on a binding made by
    `axios.create(config)`. The URL is the argument or the config's `url`. The method is the
    call's own, or the config's `method`, or `GET`. A `baseURL` in the call's config or the
    instance's config is joined before the URL. A joined match is `wrapperInferred`. An
    absolute URL or `baseURL` is not a client call, and an unreadable `baseURL` makes the path
    unreadable, as does a config member that may set it (see Object members). A verb method's
    own method wins over the config's. When any document mentions axios, an identifier call or
    an axios method name is read through its binding, across imports, whatever the calling
    file says.
  - **Constants.** Paths read `const` bindings, imported constants, template literals and
    `+`, with `{}` for any part that cannot be read.
  - **The wrapper's own call.** A `fetch` or axios call inside a named callable that has a
    wrapper's name (such as `api`) is the wrapper's own implementation, not a call.
  - **Configured wrappers.** `.trellis/rules.toml` may configure wrappers (owner ruling
    2026-11-10):

    ```toml
    [[interfaces.wrappers]]
    callee = "apiFetch"   # required: an identifier or object.property
    method = "options"    # "options" (default) or "argument"
    at = 1                # 1 to 8, default 1
    prefix = "/api"       # optional rooted literal the wrapper adds to every path
    root = "web"          # optional directory the wrapper applies under; default "."
    # app = "web"         # or: the app whose files the wrapper applies to (not with root)
    ```

    - **`method` and `at`.** With `"options"`, the method is `method` in the object
      literal at argument `at`. With `"argument"`, it is a string at argument `at`.
    - **`prefix`.** It has no query, fragment or pattern. A match through it is
      `wrapperInferred`.
    - **`root`.** It follows the `[apps]` root rules (#794). The wrapper applies to files under
      that directory.
    - **`app`.** It names an app of the effective inventory (owner ruling 2026-11-10). The
      wrapper applies to the files that rule evaluation assigns to that app: the one app of the
      file's layer, else the app whose root contains it. `root` and `app` are exclusive. An
      unknown or shared app name is a located rules error when the apps are resolved.
    - **Precedence.** For one callee, an `app` scope wins over any root scope, including a
      deeper one. A deeper root wins over a shallower one, and an unscoped wrapper (`.`) comes
      last.
    - **Matching.** A configured wrapper is matched only as assembled, never through the
      built-in `/api/` rule. It replaces a built-in wrapper with the same callee.
    - **Errors.** Each invalid entry is a located `invalid_rules` error, like any other rules
      error: an unknown key, a bad value, both `root` and `app`, the same callee twice for one
      scope, or more than 64 entries. `wrappers.callees` shows what took effect. A `package.json` configures no
      wrapper.
- **reqwest calls (#858).** Read in Rust documents that mention `reqwest`:
  `reqwest::get(url)` and `reqwest::blocking::get(url)` (`GET`); `c.get(url)`, `.post`,
  `.put`, `.delete`, `.patch` and `.head` (the method of the name); and
  `c.request(Method::M, url)` (method unreadable unless `Method::M` or a path ending in
  `Method::M`). The receiver `c` must read as a client: a name or field containing `client`
  in any case, or a `Client::new()` or `Client::builder()` chain. The URL, looked through
  `&`, is a string literal without escapes, a visible constant, or a `format!` whose
  substitutions are read where they are a string literal or a visible constant with no
  format specification (`{x}`, `{}`, `{0}`), and are `{}` otherwise: a fill, width,
  precision or mode such as `{x:0>3}` or `{:?}` is always `{}`. A visible constant is a
  `const` or `static` string at the top level of the document, its name declared nowhere
  else in it (no other `const` or `static`, and no `use` import or alias, parameter,
  closure parameter, `let`, `match`, `if let`/`while let` or `for` pattern that binds it),
  read from a call outside every inline `mod`. A glob import (`use a::*`) anywhere below the
  top level may bind any name, so it makes every constant of the document unreadable; a
  top-level glob does not, since the top-level item shadows it; any other identifier is
  unreadable. Each substituted value is charged to the work bound before it is copied. The
  path is what follows the host of an `http://` or `https://` URL (`/` when there is none), or the text itself when it starts
  with `/`; a URL that starts with a substitution, or anything else, is unreadable and
  `unresolved`. The path is cut at `?` or `#` and split as above.
- **Python clients (#856).** Read in production Python documents that mention `requests` or
  `httpx`:
  - `requests.get/post/put/delete/patch/head/options(url)` and `requests.request(method,
    url)`, the same functions on `httpx`, and `httpx.stream(method, url)`, called through the
    module (`import requests`, `import httpx as h`) or a function imported from it (`from
    requests import post`);
  - the same methods on a client: `requests.Session()` (or `session()`), `httpx.Client(..)`
    or `httpx.AsyncClient(..)`, written inline or bound once by an assignment or by `with`
    (or `async with`) `.. as c`. `self.session` and other attributes are not read.
  - **Method.** The function's own, or for `request` and `stream` a readable string argument
    (`method` or argument 0) after uppercasing; else unreadable.
  - **URL.** `url` or its argument position. A string literal (no escapes, no bytes), a visible
    constant (see Bindings above), an f-string, `+` and implicit concatenation, `%` with a
    tuple, a single value or a dictionary, and `.format(..)`, with `{}` for any part that
    cannot be read. An f-string substitution, a `%s` or a `.format` field is read only without
    a conversion, format specification, flag, width or `=` (`{x:03}`, `{x!r}`, `%5s` are
    always `{}`). A `*args` or `**kwargs` may supply any argument or key not written, and a
    dictionary splat or unreadable key hides the keys written before it. Each substituted
    value is charged to the work bound before it is copied, and cancellation is checked for
    each substitution.
  - **Path.** An `http://` or `https://` URL gives its path as for reqwest. A relative URL
    needs the client's absolute `base_url` (httpx), readable in full, and is joined to the
    base's path with one `/`; when the base has a path, the match is `wrapperInferred`. A relative URL
    without a base, or with an unreadable base (including one a `**` splat may supply), is
    unreadable and `unresolved`.
- **Segment matching.** Methods must be equal, or the route is `ANY`. A literal segment must equal a literal
  segment. A server `{name}` matches exactly one segment, and `{*name}` matches the rest.
  A client substitution segment matches only a server `{name}` or `{*name}`. If nothing
  matches as written, the call is matched again as assembled by the wrapper's prefix:
  `/api/` becomes `/api/checkouts/{root}/`, with `{root}` a substitution segment. That rule
  applies to the built-in wrappers only, never to a configured wrapper or a reqwest,
  requests or httpx call.
  Among the matches the one with the most literal segments wins. Between equal counts, a route that names the method wins over an `ANY`
  route. Any other tie is no match.
- **Confidence.** `wrapperInferred` when the prefix was needed. Otherwise `template` when the
  call used a client substitution, or the route has a server `{name}` or `{*name}`, came
  from the dispatch table or is `ANY`. Otherwise `exact`. A call with no match is `unresolved`. A route
  takes its best call site: `exact`, then `template`, then `wrapperInferred`. The values are
  defined in the contract-v1 amendment.
- **Exclusions.** Documents whose file module has the `test` role, routes and calls whose
  innermost declaration has the `test` role, items under a `test-support` feature attribute,
  and routes that serve static assets (a handler that contains `include_str!` or
  `include_bytes!`) are not extracted.
- **Evidence, never proof.** Interfaces are never an input to `$layers`. A pair of layers in
  different apps stays `unknown` with reason `cross_app`, and the view makes no claim that one
  app calls another. Absence of a match proves nothing.

## Module chips and counts (#424)

The view always draws the full stack: every ordered layer, with the shared layers beside
it (#449). Only authored `display = "hidden"` layers and unhinted test or fixture layers
wait in the hidden-layer note. Deviations are highlighted and allowed arrows (conforming or
excepted) are dimmed; no layer or arrow is dropped to focus on deviations.

Layout (#525): each render fits as many chip columns as the canvas width holds, then sizes
the chips so the drawing fills the panel width without passing it (only a layout narrower
than its minimum scrolls inside the canvas). Titles, band headers and chips **wrap** at
separators (space, `·`, `_`, `-`, `/`, `.`) instead of being cut short; only a pathological
name longer than twelve wrapped lines is clipped, and the accessible name always keeps the
whole text. A band's count chips wrap onto further header lines when they do not fit one.
Arrow routing (#637): Pele lays out the bands and chips only; the view routes the arrows.
A **gutter** opens under each band row, and arrows run orthogonally through the gutters, so
no arrow segment crosses a band or a module chip: an arrow leaves and reaches a band only at
its border. Arrows between adjacent rows cross one gutter (straight where the bands overlap),
peer arrows dip into the gutter under their row, and arrows to or from the nearest shared box
run along a gutter to its side. A **long upward** arrow (one that skips a band row) runs in a
side channel left of the stack; a long downward arrow, or one to or from a farther shared box,
runs in a channel right of the stack (and a strip under the drawing to reach the box). Each count tag
sits in a gutter's tag rows on its own arrow, clear of bands, chips and other tags, never on a
band border. Arrow **width** grows with the arrow's reference-site count on a log scale, from
1.5 drawing units at one site to 7 at 1,000 or more, so one large edge does not swamp the
rest; colour keeps its meaning, the tag and the inspector keep the exact count, and the arrow
key under the drawing shows the width at 1, 10, 100 and ≥1000 sites. The channels count
toward the fitted width. This is layout only: records, counts and identities are unchanged.

All apps layout (#790, #850; owner decision 2026-11-09). With two or more apps, All apps draws
one column per app, side by side in **manifest order** when they fit (each at least 352px,
with a 176px gutter between columns), else stacked. Each column is the same Pele drawing as
the one-app view, fitted to its column width (#860's stack span included), with its own
gutters and side channels. An interface end between **adjacent** side-by-side columns, with
both of its layers drawn as bands, draws
one overlay line in its own lane of the gutter, from its band's side straight across to the
partner band; any other end is named in words on its card. A column's **Shared** boxes sit on
the side away from its interface partners, so a line between ordered bands never crosses
them. The rule counts, per column, only those drawn lines, and the ones each side would cross: an end on an ordered band
crosses the Shared boxes when they are on the partner's side, and an end on a Shared box
crosses the ordered stack when the boxes are on the other side. The column takes the side
with fewer crossings; a tie, including a column with no drawn line or a stacked layout, keeps
the right, as in the one-app view. With two apps whose interface runs between ordered layers,
the left app's Shared boxes are on its left and the right app's on its right. A middle app with
partners on both sides takes the side with fewer lines; one with as many lines each way keeps
the right, and its lines on that side then cross the Shared boxes. On the left, the nearest
Shared box sits beside the stack, the long-upward channel runs between them, arrows to the
nearest box run left along a gutter to its side, a farther box is still reached through the
right channel and the strip under the drawing, and the `→`/`←` tags of shared arrows are
mirrored so a tag points the way its arrow runs. Band headers, the Shared header included,
wrap whole at separators and keep their size (AC26); no overlay line runs through a Shared
header when the boxes are away from the partner. The one-app view is unchanged.
When the panel width changes (window resize, inspector shown or hidden), the drawing
is laid out again after a short quiet period (150 ms), from the same response with no new
read, keeping the size cap, the inspector, the selected node and keyboard focus. The hidden-layer note
(`N hidden layers · M files · E production edges`, with its toggle) is laid out inside the
canvas directly under the drawing.

Nothing but the toolbar sits above the drawing, apart from loading and error states and the
one-line outcome of a rules edit. The heat legend and the arrow key sit
under it, with the coverage sentence said once (see below). The snapshot counts (`N file
modules · production / test-only cross-layer sites · Metrics`), the files not drawn under
the size cap, the heat exclusions and the rule-instance counts are behind the ⓘ.

An open band draws **one chip per module**, not per file. Bands start collapsed unless
their authored `display` is `"expanded"` (#391 ruling; see [rules](rules.md#layer-display-hint)).
A collapsed band draws one row of its module chips (a shared box: up to six stacked),
coloured by metric and hottest first, with a last chip counting the modules the row leaves
out; the band header opens it. Open bands charge the size-cap budget first, so summary rows
use only what remains and never fold a band the viewer opened. A module is a browser display
grouping computed from the existing response, not an identity or a new API field: the
first path component below the band's common directory, where a file and its same-named
directory form one module (`src/store.rs` and `src/store/**` are `store`), and a
directory's `mod.rs` or `__init__.py` is that module's root file. A one-file
module is that file's chip. Selecting a module chip opens it one level (its root file,
then nested modules) down to file chips; selecting a band header folds or opens it.
Chips show the module's maximum (minimum for Maintainability) displayed metric and its
measured file count; files without selected callables remain counted in a note.

Band headers show only the **non-zero** outgoing production reference-site counts as
compact chips: `↑` upward deviation (one chip per destination layer, see below) and `⇄`
peer or shared-outbound deviation in the error colour, then `↓` downward, `→shared`,
`allowed` (by `layer_policy`), `excepted` and `unknown`. Arrows between bands carry the same counts (`↑52`, `↓8 excepted`), with
the direction and verdict in their accessible names. These are sums of the summary
edges, so they equal the `$layers` instances' `referenceCount` from
`trellis rules check --format json` on the same pin and rules.

Counts are lower bounds whenever their source layer has unresolved sites (#449): a
reference that maps to no module draws no arrow. A bounded count reads `↑≥102`, and a bounded
highlighted (deviating or unknown) arrow also shows the share of its source layer's sites
that map to a module (`↑≥102 · 9% mapped`). The accessible name says the same in words.
These figures come from the summary's measured `references.coverage`, never estimates. Unknown-verdict arrows are drawn in neutral grey.
Each summary edge carries its own `coverage` (#523): its source layer and role's
`{sites, mappedSites, unresolvedSites, semanticSites, complete}`, where `complete` means no
site stayed unresolved. The arrow drops the `≥` exactly when `complete` is true, and its
accessible name says when the count rests on semantic bindings (`evidenceTier`
`semantic` or `mixed`). Semantic bindings resolve sites that structural conventions
cannot, so an imported producer is what usually turns a lower bound into an exact count.
A compact arrow key sits under the canvas with the whole snapshot's coverage, said once in
the view and inspector (`3,727 of 40,219 reference sites mapped to a module (9%) · ≥ counts
are lower bounds`, #525). The rule-instance counts (`740 of 1,095 production rule instances
unknown`) and the longer prose live behind the ⓘ.

The inspector opens on the counted review chips, **23 deviations · 156 unassigned ·
0 exceptions**, and every Architecture inspection keeps them at its head. The deviations
chip opens the Deviations tree below, the one review path for deviations (#845). The other
chips open the unassigned files and the authored exceptions. Deviations are `rules.counts.deviates`, unassigned files
are the full native module inventory with no assigned layer (including module-only files),
and exceptions is `exceptionCount`. The evidence disclosure separately labels
`rules.unassignedCount` as identities, including external packages. There is no refresh button: the
view re-reads when a new revision is selected or the rules change, not every time the tab
is opened (see the note below). A detail list
that fails with `rules_conflict` offers **Reload Architecture**. On narrow screens the
overview does not open the inspector drawer.

**Per-destination `↑` badges and interface stubs (#789, proposed (owner-delegated, run 803)).**
This amends the single `↑` chip above, in every stack. A band has one `↑` badge for each
destination layer it reaches upward: production `$layers` edges with the `deviates` verdict,
not excepted, to a layer with a smaller `order`. It reads `↑12 references · entry` (`↑≥12` for a
lower bound, `↑1 reference · entry` for one): the count is reference sites, the tree's unit after
#846 (owner ruling 2026-11-09, #845). It sums the summary edges, so the badges of a band add up to its former `↑`
chip. Each badge is its own focusable control, the next sibling of its band, and opens the
Deviations tree with that edge's chip pressed, no line selected and Calls only off. The `↓`
chip stays display-only.

In a one-app stack, a band that holds this app's end of an HTTP interface also carries a stub
read from `interfaces[].ends`: `→ <server> · N routes · M call sites` on the client end,
`← <client> · N routes` on the server end. Ends with no layer have no band and no stub. Several ends on
one server band can share a route, so that stub reads `≥N routes`. A stub is a button with
`aria-pressed`; selecting it opens the interface inspector (the ends, the counts, the wrappers,
the server-only and client-only lists, and **Open endpoints** for the matched routes, each
with **Read handler source** and **Read call-site source**) and shows the word `Selected` on the stub; while it is selected, its band's label also says
`client end` or `server end`. The app
inspector (name, languages and roots, a grid of files, layers, calls to each server app, deviations
and unassigned files, and the interfaces the app takes part in) is selected from an app column header in All apps (#790).

**Opening the tab again (#789, proposed (owner-delegated, run 803)).** This supersedes
"re-reads when the tab is opened again". Leaving the Architecture tab keeps the loaded
snapshot. Opening it again draws that snapshot without a read when nothing it rests on has
changed: the checkout, the session, the index pin, the aggregation, the metric population
and the rules epoch (a rules edit applied here, or a `rules_conflict`, advances it). Any
change re-reads, with the usual loading message. Choosing an app, selecting or clearing a
selection, and returning from the Deviations tree re-draw the held snapshot and never read.
The chosen app resets when the checkout changes.

The SPEC §9 size cap applies to chips: at most 40 per band or open module and 150 in
the whole diagram, counting every note. The rest of a list becomes one visible "N files
not drawn" note that opens the layer's complete file list in the inspector. Once the
budget is spent, further bands and modules stay folded and no more notes are added; the
ⓘ reports the total not drawn. Test and fixture layers without a `display`
hint stay in the hidden-layer note until **Include tests and fixtures in metrics** is on.

Click a module or select it with Enter/Space to inspect its identity, layer, captured
physical file lines, callable count, every metric, and its incident references and rule
instances (read on demand, [below](#summary-and-detail-reads-448)).

**Arrows and badges open the Deviations tree (#845, owner ruling 2026-11-09).** Selecting an
arrow that the tree lists, a production `$layers` edge with the `deviates` verdict and no
exception (upward `↑` or peer and shared `⇄`), opens the Deviations tree with that edge's
chip pressed, no line selected and Calls only off, exactly as its band's `↑` badge does. The
tree's back link returns to the drawing and focuses the arrow or badge that opened it. Any
other arrow (allowed, excepted, unknown or test-only) opens the edge inspector: its exact
contributing module pairs, production/test counts, verdict, exception and source-site
samples. The edge inspector lists no rule instances; the per-edge exception review it used
to open (a rule-instance list with checkboxes, **Select witnessed pairs** and **Add selected
exceptions**) is retired. Omitted sample counts stay visible;
sampling never changes totals. Explicit source actions read cached indexed text at
the displayed revision. They never fall back to the live file.

### Boundary calls to Sequence (#646)

For excepted references (and other arrows the edge inspector shows), the inspector groups sampled boundary call sites by
calling method. **Open sequence** opens that exact measured caller in Interactions and
highlights its boundary calls, scrolling the first visible call into view. If Interactions
hides a focused call, the notice offers **Show all steps**. Browser Back returns to the
Architecture selection. Authored exceptions offer **Inspect boundary calls** for the same
path without changing the exception.

A caller link requires a measured call's captured callable owner and stable lookup key.
Non-call syntax references and unavailable callers stay listed with a reason, without a
guessed method. Rules check/explain evidence carries the same caller key for MCP chaining;
it does not upgrade the target reference's evidence tier. Sampled counts remain sampled.

Status: proposed (owner-delegated, run 802). A generated Sequence link carries up to 50
repeated `callId` parameters for the highlighted calls. Reload, Back and Forward keep the
highlight: Back and Forward restore it from the in-memory entry, and a reload rebuilds it
from the link. A group of more than 50 calls highlights in memory only; its link omits
`callId`, and the status says that reloading shows the sequence without highlights.

## Deviations tree (#786)

Status: proposed (owner-delegated, run 802).

The deviations chip replaces the diagram with a tree of every production deviation, found
without choosing an edge first. It reads `kind=instances`, `rule=$layers`, `status=deviates`,
`role=production`, `excepted=false` at the view's pin and rules hash, page by page, and
shows nothing if any page fails. The tree is a flat list of files, each with its functions
and then its lines, sorted by references; files are not nested under edges. A file row
carries the edges it reaches up or across, and the edge chips filter the list. The tree
replaces the drawing and the Architecture toolbar (app switcher, metric controls, ⓘ); its
header leads back with "← Architecture", or "← <app>" in a one-app stack.

- **Naming (#816).** A *deviation* is a rule instance, as on the review chip (**23
  deviations**). The tree's chips and Reaches up column count *references* (reference
  sites), in the design's compact form (#845): a chip reads `storage → indexing · 445` and a
  file's Reaches up reads `▲ indexing 403`; their accessible names say "references" (or
  "calls" under Calls only). Modules read `from → to`; "target" is used only for **Go to
  definition**, the resolved declaration.
- **Function names (#845).** A function row is the owner's stable lookup key without the
  file's `path::` prefix (`EvidenceResponse::architecture_config` under `src/store.rs`). A
  site whose smallest enclosing declaration is a closure (no name and no stable key) is
  listed under its nearest enclosing callable, read from the Files outline (`GET symbols`) at
  the view's pin, and its line panel says "(in a closure)". A callable is a named function,
  method or constructor, or a binding whose whole initializer is a function
  (`callableInitializer`); named variables, fields and types on the way are skipped (#860). A
  method reads `Type::name` from its `impl` block. A closure with no enclosing callable up to
  its file's root is "(module level)". At most 50 files are named per tree; a file whose
  outline cannot be read keeps "(anonymous)", but an outline read that answers
  `revision_conflict`, `pin_expired` or `root_changed` is the tree's conflict, handled as below,
  and shows no tree.
- **Filters.** The chips are **All upward** (or **All deviations** when a peer or shared
  edge is listed) and one per edge, with `⇄` before a peer or shared edge. When a listed
  edge's source layer has unresolved sites, the chip still shows the mapped count and one
  line under the chips gives the reason: "Counts are references mapped to a file. Some sites
  in <layers> stayed unresolved, so their edges may have more." A line's tag is its
  classified relation kind (`relationKind`): `call`, `type use`, `import` or `reference`.
  **Calls only** keeps exactly the lines tagged `call`; type uses, imports and other
  references are hidden. A JavaScript or TypeScript `require(...)` or `import(...)` stays an
  `import` even when it carries a caller, so it is tagged `import` and hidden under Calls
  only; it can still offer **Open in Sequence**.
- **Counts.** A file's `referenceCount` and `kindCounts` are exact. Lines are sampled per
  file pair, calls first. A function's count reads `≥N` only when a file pair that feeds it
  has more sites (or calls) than are listed; a function fed only by fully listed pairs is
  exact. The footer says what `≥` means.
- **Line rows** read `L<line> <symbol>` with a kind tag and, on one line, `→ <file>:<line>` for
  the definition, or `→ <file> · file only` without a binding; the full path and the reason
  are the tooltip and the accessible name.
- **Line actions.** One menu with the label "Line actions" opens from the `⋯` button, a
  right-click or Shift+F10: **Open in Sequence** (only when the line has a measured caller),
  **Go to call site**, **Go to definition** (only with a target), **Copy location** and
  **Add exception for this file pair…**. The exception note reads "Approves all N references
  from A to B, including future ones on this pair." and goes through the normal preview and
  confirm; once it is applied the pair leaves the tree.
- **Keyboard.** The tree has one tab stop. Arrow keys move and expand, Home and End jump,
  Enter or Space toggle a file or function, and Enter selects a line.
- **Hidden selection.** A filter that hides the selected line clears the selection and says
  "Selected line is hidden by this filter." **Clear filter** returns to All deviations with
  Calls only off and selects nothing. A line is never selected for the user.
- **Unknown.** "N unknown rule instances are not deviations." links to the existing
  **Inspect same-layer and unassigned references** view, and closes the tree.
- **Open in Sequence and Back.** The browser history entry keeps whether the tree was open,
  the chip, Calls only, the expanded rows, the selected line and focus. **Back** reopens the
  tree and reselects and focuses that line. If the line is gone it says "The selected line is
  no longer listed." and selects nothing.
- **Refresh and conflicts.** A moved revision (`revision_conflict`, `pin_expired` or
  `root_changed`) is handled like any stale Architecture read: the app reads "Architecture
  snapshot changed. Refresh status before reopening Architecture.", refreshes status and
  resets the Architecture view, which closes the tree; open it again from the deviations
  chip. Changed rules read "Rules changed since these deviations were loaded. Reload
  deviations."; a building index reads "Architecture is still building for this revision."
  **Reload deviations** re-reads and reopens the tree with the same chip and Calls only and
  no selection. Applying an exception does the same. A reset for any other reason,
  such as leaving the tab, drops that request.

## One computation per revision (#556)

The daemon computes what Architecture needs once per published revision and rules file, in the
background, on one per-checkout owner thread. A worker step that leaves a different head or
rules file from the last step hints the owner (#798), which compares the head and the rules
file's hash with what it last published and starts a build when they differ; a newer head stops a build that is under way. The owner builds every active runtime's
new head, with or without a rules file, so the first read after an activation or a publication
usually finds its result published (#798). Reads never compute the head: any number of them wait for
the same build, and each rechecks the pin and rules hash, so a new revision or an edited rules
file is never served from an older result. A daemon restart or an idle checkout's retirement
drops the result, and the next activation builds it again.

A build reads each document once in one read transaction, ends it, and only then resolves
references, prepares rule rows and measures metrics. A publication or a retirement waits for
that read at most, never for the computing, and a read waits for the owner before it admits
its own transaction. A build keeps no source text, syntax trees or semantic rows, only the
per-source reference rows (up to 50 sampled sites per edge, calls first), rule rows, metric rows and each
finished summary, so a checkout holds one result, bounded by the existing reference and rule
budgets. A checkout keeps at most two: the head's, and the one it replaced or an older retained
revision a page is showing. Every read projects from the result.

A build resolves references and prepares declaration rules for the whole revision, under the
existing budgets; layer-edge and exception work is charged per read, on the references that
read selects. A revision over those budgets fails `bounded_work` for the summary, details and
unscoped reads. A path-scoped MCP or CLI read then computes on the request: it reads the
revision itself, resolves references and evaluates rules only for its own paths, and answers
under the budgets of that scope and its usual deadline. That read holds a read transaction
while it computes. If it cannot finish in time it fails `deadline_exceeded`, which is retryable.
A scoped read does not compute while the owner is still building its revision: it waits like
an unscoped read and then answers `building`, so it never repeats the build's work (#798).

A build reports its progress as `{phase,completed,total}`. The phases are `reading`,
`resolving`, `evaluating` and `measuring`; `reading` and `measuring` advance per document.


| Read | While the build runs |
| --- | --- |
| `GET architecture` | Waits 1 s for the owner, then answers **202** with `{"state":"building","progress":{"phase","completed","total"},"retryAfterMs"}`. The view shows `Building Architecture for this revision: reading indexed files 120 of 604…` and asks again; the daemon answers as soon as the build is done. |
| `GET architecture/details` | Waits 1 s, then answers **503** `architecture_building`. |
| MCP `trellis_rules_check`, `trellis_rules_explain`, `trellis_references`, the layer prompt | A read, with or without `paths`, waits until one second before the 5 s call deadline, then fails with the typed retryable `building`: `{code:"building",retryable:true,message,progress:{phase,completed,total}\|null,retryAfterMs}` (#798). The message names the phase and count. The prompt carries the same error as its JSON-RPC error `data`. |
| CLI `trellis rules check`, `trellis references` | A read, with or without paths, waits up to 30 s, then fails `deadline_exceeded` with a message that says the build is running. |
| `metrics` (revision or directory scope) | Waits up to 5 s for a running build, then computes its own rows. It uses a published build's rows at once, and never asks for a build. |

The periods above are bounds on one read's wait: a waiter wakes on the owner's signal, and
looks at its own cancellation and deadline every 100 ms because those have no signal of their
own. The page asks again because a browser has no push channel to the daemon.

A failure this revision would repeat (`invalid_rules` found while evaluating, `bounded_work`,
`invalid_source_syntax`, a corrupt row) is kept and answered like a result. Contention retries
on the owner within a bound. A panicked build is reported once as `store_unavailable`, and the
next read starts over. The page does not move to a new head until it reloads, so the owner
also serves an older retained revision: the core it replaced stays readable, and it builds a
revision a page asks for once the head is done. An MCP or CLI read of such a revision computes
it on the request unless the owner holds it.

## Summary and detail reads (#448)

`GET architecture` returns what the view draws plus exact counts, not per-site
detail. On this repository it went from about 35 MB to about 0.75 MB. Its fields are:

- `layers`, `modules` and summary `edges` (each edge keeps its contributing pairs and
  their bounded site samples). Each edge also has `semanticSites` (the sum of its pairs'),
  `evidenceTier` (`syntax`, `semantic` or `mixed`) and `coverage`, its source layer and
  role's coverage row plus `complete` (no unresolved site) (#523);
- `evidenceTier` (`syntax` or `mixed`) at the top level, as in the references payload;
- `partialFiles` at the top level and in `references` and `rules`: the files with a syntax error,
  with syntax past the walk bounds, published without evidence, or not read (#576) (see
  [Work bounds](rules.md#work-bounds-and-located-failures)). One bad file is a reported gap here,
  never a refusal of the whole view (#554, #563, #564). Only a `ready` payload carries it;
- `references`: `referencesVersion`, `evidenceTier`, `semanticComplete`,
  `semanticProducers`, `counts` (`scannedDocuments`, `mappedSites`,
  `unresolvedSites`, `semanticSites`), `partialFiles`, `limitations` and `externalPackages`, each descriptor with the
  display `layer` its `$layers` instances report (null when unassigned);
- `references.coverage` (#449): one row per source layer (null when unassigned) and
  stored site role, `{layer, role, sites, mappedSites, unresolvedSites, semanticSites}`. `sites` counts
  every scanned reference site once, structural or semantic; a site with at least one mapped target is mapped,
  and one with a residual reason is unresolved, so a site can be both; one an external
  semantic binding resolved is neither. `semanticSites`
  counts sites a fresh binding mapped or bound outside the checkout. The rows sum to
  `counts.mappedSites`, `counts.unresolvedSites` and `counts.semanticSites`, and there are at most two per layer;
- `apps`, `noApp`, `interfaces` and `modules[].app` (#784, #788; see
  [Apps and HTTP interfaces](#apps-and-http-interfaces-data-and-extraction-784-788));
- `exceptionCount`: the authored `[[exceptions]]` entries (null without a rules file);
- `rules`: `rulesVersion`, `rulesFile`, `counts`, `testCounts`, `unassignedCount`, `partialFiles` and
  `limitations`.

`counts`, `testCounts` and `unassignedCount` (the length of `unassigned`) equal
`trellis rules check --format json` on the same pin and rules file. The reference
`counts` equal `trellis references`. The summary has no `modules`, `production`,
`tests` or `unresolved` reference arrays, and no `instances`, `testOnly`,
`unassigned` or `externalPackages` rule arrays.

`GET architecture/details` serves that detail one bounded page at a time. It requires
`kind`, the complete pin (`indexGeneration`, `indexRevision`) and the summary's
`rulesHash`. `offset` defaults to 0, and `limit` defaults to 100 (range 1–500). A
page also stops at 4 MiB of serialized items, but always holds at least one item.
The kinds and their exact-match filters are:

| `kind` | Item | Filters |
| --- | --- | --- |
| `instances` | `{role, instance}`: production `instances`, then `testOnly` | `role`, `rule`, `status`, `excepted`, `exceptionReason`, `sourceLayer`, `destinationLayer`, `layer` (source or destination), `path` (target or destination path), `app` (#784: the instance's app name) |
| `pairs` | `{role, sourceLayer, targetLayer, pair}`: production pairs, then test pairs | `role`, `path` (source or target path), `source` and `target` (an exact endpoint identity as JSON: a native target or an external package), `layer` (either endpoint), `unlayered=true` (same-layer or an unassigned endpoint) |
| `unresolved` | one unresolved site | `path` (source path) |
| `interfaces` (#788) | one interface row | `client`, `server`, `list` (`matched`, `serverOnly` or `clientOnly`) |

The response is `{indexGeneration, indexRevision, rulesFile, kind, offset, total,
nextOffset, items}`. `total` counts every matching item; `nextOffset` is null on the
last page. Items keep the evaluation's order and shapes, so the pages of one kind
concatenate to the CLI's arrays. Unknown or duplicate fields, filters that do not
apply to the kind, and an offset past `total` fail `invalid_request` (HTTP 400). A
stale pin fails like the summary (409 `revision_conflict` or `pin_expired`). A rules
file that changed since the summary fails 409 `rules_conflict` (or 422 `invalid_rules`
with the located diagnostic when the new file does not parse). A detail read
evaluates the same admitted snapshot as the summary, without metrics, and it never
indexes or writes. Existing Bearer, Host/Origin, deadline, cancellation and
response limits apply.

The browser reads details only when an inspector or list needs them:

- a module's pairs and rule instances;
- a layer's pairs and instances;
- unresolved sites;
- same-layer and unassigned pairs;
- `$external` results;
- the Deviations tree's production deviations.

`$external` results have their own list in the notices. Each list shows one page and a **Show more** button for the next one. Repository
text in details is inserted with `textContent`.

## Review and authoring

Deviations are reviewed in the Deviations tree, above. A module's or layer's inspector lists
its rule instances from `architecture/details` with source/destination files, layers,
production or test role, underlying verdict and exception reason. Structural evidence is syntax-tier: captured source-owner identities and
spellings are not resolved destination-symbol bindings. Semantic sites name their
binding's provenance, and rule evidence calls them `semanticBinding`. Site samples and omitted counts
stay explicit. The Unassigned list includes files with no observed references.

An exception is per file pair. It starts from a line of the Deviations tree, **Add exception
for this file pair…** (the line menu or the line panel), or from a rule instance in a module
or layer inspector, and needs a nonblank reason. It goes through one preview and one apply.
There is no bulk selection (#845): the design has none, and each pair is reviewed from the
lines that witness it. Exceptions do not persist wildcards or waive future files. The portable format remains literal language/path endpoints under #365.
Unknown destinations and external packages cannot become portable pair exceptions.
Existing authored exceptions can be edited or removed even when no current pair matches.
An unassigned file can be assigned to an existing layer by adding its exact path.

Every operation shows the exact prospective `.trellis/rules.toml` diff before confirmation.
The editor preserves unrelated TOML formatting and comments. Confirm writes those exact
preview bytes atomically, without a Git commit, restart or index publication. The view
then checks the new rules against its indexed code pin. MCP remains read-only.

The browser-only checkout routes are `GET architecture/rules`,
`POST architecture/rules/preview` and `POST architecture/rules/apply`. Existing Bearer,
Host/Origin, request/response and rules limits apply. Writes are restricted to the fixed
checkout rules path. Each edit uses the loaded rules hash as well as the complete code pin.
If the file changed since load, HTTP 409 `rules_conflict` preserves the draft and requires
reload and a new preview; saving never retries or overwrites silently. Trellis serializes
its own writes, stages and syncs a temporary file, compares the current original immediately
before atomic rename, then syncs the parent. This is optimistic conflict detection, not
an OS transaction against arbitrary external editors in the final compare/rename interval.
Post-rename durability uncertainty is reported as a changed-file outcome, not “not written.”

## Metrics

The color choices are Layer, Cyclomatic, Cognitive, Max nesting, Maintainability,
Halstead volume and Fan-out. Layer mode uses neutral module chips and explicit
membership, not an invented numeric metric. Numeric aggregates are max (default), sum
and mean over raw [`metrics:v1`](metrics.md) callable scores. Heat explicitly starts with
`role=production`; the unchecked **Include tests and fixtures in metrics** checkbox requests `all` when checked.
Omitted API/CLI role remains `all`; `test` is available through those transports. The Architecture
payload echoes `role` (rather than `metricsScope`). Stored native declaration roles are authoritative
for Rust, Java, Python, JavaScript and TypeScript under Decision 0011, not browser path heuristics or runner
discovery. Inline Rust `cfg(test)` callables are test; feature-only `test-support` remains production.
Each module carries per-file `roleCounts: {production, test}` independent of the selected role;
its `declarationCount` and metrics describe the selected population. Fan-out is the measured callable metric,
not structural dependency degree. Null values stay unavailable, with reasons; they
never become zero. Test-only files have unavailable production heat, not a fabricated zero.
Callable-range LOC/SLOC can overlap and are not physical production file lines; no physical
production-LOC measurement is claimed.

Band headers show the maximum available displayed module value at the chosen
aggregate, not a mean of file means. Heat uses the shared
[fixed bands](metrics.md#workbench-heat-395) for max/mean. Sum is neutral and labelled relative
totals. **Relative**, off by default, uses modules in the admitted layers and the
unassigned inventory (drawn as chips even when collapsed), after measured filtering. Collapsed
layer/directory summaries include their member modules in that basis; expanding them does not
move the cuts. The hidden-layer note can change the Relative basis. Maintainability reverses severity, not values.
Browser requests set `workbench=true`; stored tests, fixture roots, authored hidden layers and
`web/vendor/` do not contribute default heat. Counts report excluded callables and files.
Include tests and fixtures restores tests/fixtures/hidden-layer metrics, but not vendor metrics.
Module identities, physical lines, reference totals and Files access remain unchanged.

## Revision and rendering boundaries

One admitted snapshot supplies code evidence, assignments and metrics. The view retains
the full `{indexGeneration,indexRevision}` pin and the separate rules content hash:
indexed code does not pin live configuration. Evidence actions retain both. Changed
rules require refresh rather than explaining an old diagram using new rules. Checkout,
session and full-pin changes clear pending evidence and selection before rebinding. Metric-role
changes also clear old heat and inspection before fetching. Role participates in query, response
validation and currentness alongside the aggregate, root/session, full pin and request serial,
including async Pele completion and indexed-source callbacks.
Browsing does not index, execute repository code or contact providers.

Patched Pele 1.10.0 renders app-generated `block-beta` layer summaries. App-owned opaque
IDs map every interactive module and summary edge to its exact DTO identity. In the summary
display Pele draws no connectors (any it draws fail admission); the adapter draws each arrow
and count tag itself from the projection's own edge records. Repository
labels are inserted with `textContent`, not Mermaid syntax. A missing, duplicate or
changed renderer mapping clears the interactive canvas and shows an error; no guessed
or partial mapping is installed. Local assets and the existing CSP remain in force.
Natural-size diagrams scroll inside their canvas instead of shrinking text to fit.
Desktop panes scroll independently; narrow screens reuse Explorer/Inspector drawers.

Architecture evidence is syntax-tier (`evidenceTier:"syntax"`), or `mixed` once fresh
semantic bindings map any site; `semanticComplete` stays false either way. Coverage and
limitations are visible; missing references do not prove there is no dependency.
Module-level Flow is deferred to [#324](https://github.com/SquashMerge/trellis/issues/324).
The module inspector's **Open in Treemap** action opens the exact source-set/language/path
file at the same full pin. [Treemap](treemap.md) uses physical file lines (including
inline test lines) for area; its production heat and file visibility are independent.
