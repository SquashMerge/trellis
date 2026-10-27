# Java standard-library catalog

## #163 owner ruling (2026-10-26)

After PR #357, only JDK `javax` packages may receive `standardLibrary:true`.
The reviewed definition is exact package membership in public **unqualified
exports** of pinned OpenJDK module descriptors. Qualified exports, opens clauses,
comments, parent names and invented descendants do not add packages.

`java-standard-library:v2` preserves the existing `java` root admission policy
and the external family identities and evidence entries `java` and `javax`.
For `javax`, named imports and qualified type references check their containing
package; wildcard imports check their exact package prefix. Local captured
package/type precedence and wildcard competition rules still apply first.
No member existence or selected/installed-JDK availability is claimed.

Examples admitted independently include `javax.net` and `javax.net.ssl`.
`javax.security.auth` is admitted; its parent `javax.security` is not.
`javax.inject`, `javax.servlet`, `javax.persistence`, `javax.ws.rs` and removed
`javax.xml.bind` are not in this catalog. A non-JDK package needs independent
captured dependency-to-import mapping evidence or stays unknown. Maven/Gradle
coordinates alone do not prove a namespace.

## Fixed source and review

- OpenJDK 25 GA, build 36; tag `jdk-25+36`.
- Tag object: `e8600ecfd0aed19084116e66c79aeecac1aa8811`.
- Commit: `6c48f4ed707bf0b15f9b6098de30db8aae6fa40f`.
- [Pinned upstream source](https://github.com/openjdk/jdk/tree/6c48f4ed707bf0b15f9b6098de30db8aae6fa40f).
- Reviewed all 69 `src/*/share/classes/module-info.java` descriptors.
  Exactly 99 javax packages from 16 modules meet the definition.
- The sorted literal `JDK_JAVAX_PACKAGES` in
  [`src/references/external.rs`](../src/references/external.rs) is the admission list.
  It is not a prefix allowlist.

The source acquisition and an independent source review agreed on the complete
package-to-module map and each contributing source hash. The table below pins
the contributing descriptors. Source text, archives, a Java toolchain and network
access are not runtime or test dependencies. Updating this catalog requires a
new reviewed source pin; it never discovers packages from the host JDK.

## Contributing module descriptors (SHA-256)

All paths are relative to the pinned upstream source root.

| Path | SHA-256 |
| --- | --- |
| `src/java.base/share/classes/module-info.java` | `d232d1ceacb79dde61b74071cba6ff965b47930c1e181a75df7f048b24b0dd2b` |
| `src/java.compiler/share/classes/module-info.java` | `2000aabb2636a0cb894ee269a53ef6d3cce80326ef890504af92a6266686ceb6` |
| `src/java.desktop/share/classes/module-info.java` | `604db01c435092cb3909bc71b36334cfa952c604d5b80f8866373a2c51e5a4b9` |
| `src/java.management/share/classes/module-info.java` | `efdee16def3b209b98d067604d4b58e00e4c661f3cbd5b7c3baa77b9fc92d1b7` |
| `src/java.management.rmi/share/classes/module-info.java` | `5dc10550c5f5d8afbd40e1c5b3380708c8ff88e8381957cf8d1f6bbf3feba173` |
| `src/java.naming/share/classes/module-info.java` | `3807edba53a52c054c2ef361b54bdd797cd6374300a55a78d9e91816e8e3cb58` |
| `src/java.rmi/share/classes/module-info.java` | `a587d29b528b3cf600ea8e0167c9540ad4c201cdce6b6d2192eb7dd744fd589d` |
| `src/java.scripting/share/classes/module-info.java` | `9f4bb3fbff6e0da1646751fcf1a42fa43ce472b66ab981e046bc243183a01ca3` |
| `src/java.security.jgss/share/classes/module-info.java` | `452f6e992a6ddf0c79d99b45bb774df59fd8050bc57ada8b6b04b94a61314316` |
| `src/java.security.sasl/share/classes/module-info.java` | `2edbffe1adbc15c439210415fdb5318703f8ab3825e5d45e3dc7d72dbe6444fe` |
| `src/java.smartcardio/share/classes/module-info.java` | `175418d52ba35baac6648ebb8dc5e26bfc922fc2387c45a03b20001234daa9f6` |
| `src/java.sql/share/classes/module-info.java` | `61aae53c2fce8a2fdcd2d8a0ca74b27297535c9c2e57edf84ecf31a2f2749ee0` |
| `src/java.sql.rowset/share/classes/module-info.java` | `93a2557f6a661ddc4bbb4883253edc277bc0fb4be86192c03b30cef726635b3b` |
| `src/java.transaction.xa/share/classes/module-info.java` | `69b78361528699b3947d083ad286b6fa3016cdbb06b818b368140db0947c7aa9` |
| `src/java.xml/share/classes/module-info.java` | `cbbb9316e2a8458d30d1c640c6f94bbd34792340ab15fa6ccaf640ff07e5cbcb` |
| `src/java.xml.crypto/share/classes/module-info.java` | `a80ea6f850e78f4fc0e848f67d76ebaf6fb91f8b54c97e9c870d1688ec8a35db` |
