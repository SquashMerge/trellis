# Import type queries in call-site type arguments

`mock.ts` and `mock.tsx` reproduce the same Vitest-style generic call. `vi.d.ts`
supplies only the contextual mock signature; `api/chat.ts` is the imported module.
With TypeScript installed, validate both fixtures with:

```sh
tsc -p tests/fixtures/typescript-import-query/tsconfig.json
```

The direct-parser and native-extraction regression is
`native_typescript_tests::call_site_import_type_queries_parse_with_both_grammars`.
On tree-sitter-typescript 0.23.2 the direct parse fails before extraction:
`importOriginal < typeof import(...)` becomes a binary/unary expression, followed
by an `ERROR` at `>()`.

The dependency is pinned to the fix in upstream
[PR #383](https://github.com/tree-sitter/tree-sitter-typescript/pull/383).
It moves the two runtime-import/type-query rule pairs from static precedences to
GLR conflicts, retaining the type interpretation until `>` disambiguates. Both
generated parsers are updated without changing either dialect's node types.
