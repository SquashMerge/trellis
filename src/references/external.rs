//! Package-level evidence only. This does not inspect installed libraries or bind members.
use super::work::Work;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct ManifestInput {
    pub source_set_id: String,
    pub native_revision_id: String,
    pub dependency_hash: String,
    pub path: String,
    pub content_hash: Option<String>,
    pub bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExternalIdentity {
    pub kind: ExternalKind,
    pub source_set_id: String,
    pub language: String,
    pub package: String,
    pub standard_library: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ExternalKind {
    #[serde(rename = "externalPackage")]
    ExternalPackage,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalPackage {
    pub identity: ExternalIdentity,
    pub ecosystem: &'static str,
    pub evidence: Vec<ExternalEvidence>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ExternalEvidence {
    Manifest {
        source_set_id: String,
        native_revision_id: String,
        dependency_hash: String,
        path: String,
        content_hash: String,
        import_root: String,
        section: String,
        key: String,
        version_constraint: Option<String>,
    },
    StandardLibraryCatalog {
        catalog: &'static str,
        entry: String,
    },
}

// Exact OpenJDK 25 GA public javax exports; provenance: docs/java-standard-library-catalog.md.
const JDK_JAVAX_PACKAGES: &[&str] = &[
    "javax.accessibility",
    "javax.annotation.processing",
    "javax.crypto",
    "javax.crypto.interfaces",
    "javax.crypto.spec",
    "javax.imageio",
    "javax.imageio.event",
    "javax.imageio.metadata",
    "javax.imageio.plugins.bmp",
    "javax.imageio.plugins.jpeg",
    "javax.imageio.plugins.tiff",
    "javax.imageio.spi",
    "javax.imageio.stream",
    "javax.lang.model",
    "javax.lang.model.element",
    "javax.lang.model.type",
    "javax.lang.model.util",
    "javax.management",
    "javax.management.loading",
    "javax.management.modelmbean",
    "javax.management.monitor",
    "javax.management.openmbean",
    "javax.management.relation",
    "javax.management.remote",
    "javax.management.remote.rmi",
    "javax.management.timer",
    "javax.naming",
    "javax.naming.directory",
    "javax.naming.event",
    "javax.naming.ldap",
    "javax.naming.ldap.spi",
    "javax.naming.spi",
    "javax.net",
    "javax.net.ssl",
    "javax.print",
    "javax.print.attribute",
    "javax.print.attribute.standard",
    "javax.print.event",
    "javax.rmi.ssl",
    "javax.script",
    "javax.security.auth",
    "javax.security.auth.callback",
    "javax.security.auth.kerberos",
    "javax.security.auth.login",
    "javax.security.auth.spi",
    "javax.security.auth.x500",
    "javax.security.cert",
    "javax.security.sasl",
    "javax.smartcardio",
    "javax.sound",
    "javax.sound.midi",
    "javax.sound.midi.spi",
    "javax.sound.sampled",
    "javax.sound.sampled.spi",
    "javax.sql",
    "javax.sql.rowset",
    "javax.sql.rowset.serial",
    "javax.sql.rowset.spi",
    "javax.swing",
    "javax.swing.border",
    "javax.swing.colorchooser",
    "javax.swing.event",
    "javax.swing.filechooser",
    "javax.swing.plaf",
    "javax.swing.plaf.basic",
    "javax.swing.plaf.metal",
    "javax.swing.plaf.multi",
    "javax.swing.plaf.nimbus",
    "javax.swing.plaf.synth",
    "javax.swing.table",
    "javax.swing.text",
    "javax.swing.text.html",
    "javax.swing.text.html.parser",
    "javax.swing.text.rtf",
    "javax.swing.tree",
    "javax.swing.undo",
    "javax.tools",
    "javax.transaction.xa",
    "javax.xml",
    "javax.xml.catalog",
    "javax.xml.crypto",
    "javax.xml.crypto.dom",
    "javax.xml.crypto.dsig",
    "javax.xml.crypto.dsig.dom",
    "javax.xml.crypto.dsig.keyinfo",
    "javax.xml.crypto.dsig.spec",
    "javax.xml.datatype",
    "javax.xml.namespace",
    "javax.xml.parsers",
    "javax.xml.stream",
    "javax.xml.stream.events",
    "javax.xml.stream.util",
    "javax.xml.transform",
    "javax.xml.transform.dom",
    "javax.xml.transform.sax",
    "javax.xml.transform.stax",
    "javax.xml.transform.stream",
    "javax.xml.validation",
    "javax.xml.xpath",
];

/// Reviewed Java family labels, not proof of members or selected-JDK availability.
/// The caller proves the containing package and local precedence before requesting a label.
pub fn java_standard_library(source_set_id: &str, package_path: &str) -> Option<ExternalPackage> {
    let import_root = package_path.split('.').next()?;
    match import_root {
        "java" => {}
        "javax" if JDK_JAVAX_PACKAGES.binary_search(&package_path).is_ok() => {}
        _ => return None,
    }
    Some(ExternalPackage {
        identity: ExternalIdentity {
            kind: ExternalKind::ExternalPackage,
            source_set_id: source_set_id.into(),
            language: "java".into(),
            package: import_root.into(),
            standard_library: true,
        },
        ecosystem: "java",
        evidence: vec![ExternalEvidence::StandardLibraryCatalog {
            catalog: "java-standard-library:v2",
            entry: import_root.into(),
        }],
    })
}

// Exact CPython v3.12.0 sys.stdlib_module_names; provenance: docs/python-standard-library-catalog.md.
const PYTHON_STDLIB_ROOTS: &[&str] = &[
    "__future__",
    "_abc",
    "_aix_support",
    "_ast",
    "_asyncio",
    "_bisect",
    "_blake2",
    "_bz2",
    "_codecs",
    "_codecs_cn",
    "_codecs_hk",
    "_codecs_iso2022",
    "_codecs_jp",
    "_codecs_kr",
    "_codecs_tw",
    "_collections",
    "_collections_abc",
    "_compat_pickle",
    "_compression",
    "_contextvars",
    "_crypt",
    "_csv",
    "_ctypes",
    "_curses",
    "_curses_panel",
    "_datetime",
    "_dbm",
    "_decimal",
    "_elementtree",
    "_frozen_importlib",
    "_frozen_importlib_external",
    "_functools",
    "_gdbm",
    "_hashlib",
    "_heapq",
    "_imp",
    "_io",
    "_json",
    "_locale",
    "_lsprof",
    "_lzma",
    "_markupbase",
    "_md5",
    "_msi",
    "_multibytecodec",
    "_multiprocessing",
    "_opcode",
    "_operator",
    "_osx_support",
    "_overlapped",
    "_pickle",
    "_posixshmem",
    "_posixsubprocess",
    "_py_abc",
    "_pydatetime",
    "_pydecimal",
    "_pyio",
    "_pylong",
    "_queue",
    "_random",
    "_scproxy",
    "_sha1",
    "_sha2",
    "_sha3",
    "_signal",
    "_sitebuiltins",
    "_socket",
    "_sqlite3",
    "_sre",
    "_ssl",
    "_stat",
    "_statistics",
    "_string",
    "_strptime",
    "_struct",
    "_symtable",
    "_thread",
    "_threading_local",
    "_tkinter",
    "_tokenize",
    "_tracemalloc",
    "_typing",
    "_uuid",
    "_warnings",
    "_weakref",
    "_weakrefset",
    "_winapi",
    "_zoneinfo",
    "abc",
    "aifc",
    "antigravity",
    "argparse",
    "array",
    "ast",
    "asyncio",
    "atexit",
    "audioop",
    "base64",
    "bdb",
    "binascii",
    "bisect",
    "builtins",
    "bz2",
    "cProfile",
    "calendar",
    "cgi",
    "cgitb",
    "chunk",
    "cmath",
    "cmd",
    "code",
    "codecs",
    "codeop",
    "collections",
    "colorsys",
    "compileall",
    "concurrent",
    "configparser",
    "contextlib",
    "contextvars",
    "copy",
    "copyreg",
    "crypt",
    "csv",
    "ctypes",
    "curses",
    "dataclasses",
    "datetime",
    "dbm",
    "decimal",
    "difflib",
    "dis",
    "doctest",
    "email",
    "encodings",
    "ensurepip",
    "enum",
    "errno",
    "faulthandler",
    "fcntl",
    "filecmp",
    "fileinput",
    "fnmatch",
    "fractions",
    "ftplib",
    "functools",
    "gc",
    "genericpath",
    "getopt",
    "getpass",
    "gettext",
    "glob",
    "graphlib",
    "grp",
    "gzip",
    "hashlib",
    "heapq",
    "hmac",
    "html",
    "http",
    "idlelib",
    "imaplib",
    "imghdr",
    "importlib",
    "inspect",
    "io",
    "ipaddress",
    "itertools",
    "json",
    "keyword",
    "lib2to3",
    "linecache",
    "locale",
    "logging",
    "lzma",
    "mailbox",
    "mailcap",
    "marshal",
    "math",
    "mimetypes",
    "mmap",
    "modulefinder",
    "msilib",
    "msvcrt",
    "multiprocessing",
    "netrc",
    "nis",
    "nntplib",
    "nt",
    "ntpath",
    "nturl2path",
    "numbers",
    "opcode",
    "operator",
    "optparse",
    "os",
    "ossaudiodev",
    "pathlib",
    "pdb",
    "pickle",
    "pickletools",
    "pipes",
    "pkgutil",
    "platform",
    "plistlib",
    "poplib",
    "posix",
    "posixpath",
    "pprint",
    "profile",
    "pstats",
    "pty",
    "pwd",
    "py_compile",
    "pyclbr",
    "pydoc",
    "pydoc_data",
    "pyexpat",
    "queue",
    "quopri",
    "random",
    "re",
    "readline",
    "reprlib",
    "resource",
    "rlcompleter",
    "runpy",
    "sched",
    "secrets",
    "select",
    "selectors",
    "shelve",
    "shlex",
    "shutil",
    "signal",
    "site",
    "smtplib",
    "sndhdr",
    "socket",
    "socketserver",
    "spwd",
    "sqlite3",
    "sre_compile",
    "sre_constants",
    "sre_parse",
    "ssl",
    "stat",
    "statistics",
    "string",
    "stringprep",
    "struct",
    "subprocess",
    "sunau",
    "symtable",
    "sys",
    "sysconfig",
    "syslog",
    "tabnanny",
    "tarfile",
    "telnetlib",
    "tempfile",
    "termios",
    "textwrap",
    "this",
    "threading",
    "time",
    "timeit",
    "tkinter",
    "token",
    "tokenize",
    "tomllib",
    "trace",
    "traceback",
    "tracemalloc",
    "tty",
    "turtle",
    "turtledemo",
    "types",
    "typing",
    "unicodedata",
    "unittest",
    "urllib",
    "uu",
    "uuid",
    "venv",
    "warnings",
    "wave",
    "weakref",
    "webbrowser",
    "winreg",
    "winsound",
    "wsgiref",
    "xdrlib",
    "xml",
    "xmlrpc",
    "zipapp",
    "zipfile",
    "zipimport",
    "zlib",
    "zoneinfo",
];

/// Cross-platform CPython v3.12.0 root labels, including disabled/platform-specific modules.
/// This does not prove installation, submodule or member availability.
/// The caller proves local precedence and charges work before requesting a label.
pub fn python_standard_library(source_set_id: &str, import_root: &str) -> Option<ExternalPackage> {
    if PYTHON_STDLIB_ROOTS.binary_search(&import_root).is_err() {
        return None;
    }
    Some(ExternalPackage {
        identity: ExternalIdentity {
            kind: ExternalKind::ExternalPackage,
            source_set_id: source_set_id.into(),
            language: "python".into(),
            package: import_root.into(),
            standard_library: true,
        },
        ecosystem: "python",
        evidence: vec![ExternalEvidence::StandardLibraryCatalog {
            catalog: "python-standard-library:v2",
            entry: import_root.into(),
        }],
    })
}

/// Canonical selector spellings, not proof that a package or library is available.
pub fn valid_package(language: &str, package: &str, standard_library: bool) -> bool {
    if standard_library {
        return match language {
            "rust" => matches!(package, "std" | "core" | "alloc"),
            "java" => matches!(package, "java" | "javax"),
            "python" => identifier(package),
            "javascript" => npm_part(package) && !package.starts_with('@'),
            _ => false,
        };
    }
    match language {
        "rust" => {
            package
                .as_bytes()
                .first()
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
                && package
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        }
        "java" => {
            let mut parts = package.split(':');
            let group = parts.next().unwrap_or_default();
            let artifact = parts.next().unwrap_or_default();
            parts.next().is_none() && coordinate(group) && coordinate(artifact)
        }
        "python" => {
            !package.is_empty()
                && package
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && package
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && package
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                && !package.contains("--")
        }
        "javascript" => match package.strip_prefix('@') {
            Some(scoped) => scoped
                .split_once('/')
                .is_some_and(|(scope, name)| npm_part(scope) && npm_part(name)),
            None => npm_part(package),
        },
        _ => false,
    }
}
fn identifier(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
fn coordinate(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}
fn npm_part(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(['.', '_'])
        && value.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        })
}

// Exact public bare roots at Node v22.0.0. Exclusion only, not label coverage.
// Source pin and deliberate subpath false negatives: docs/node-standard-library-catalog.md.
const NODE_BARE_ROOTS: &[&str] = &[
    "_http_agent",
    "_http_client",
    "_http_common",
    "_http_incoming",
    "_http_outgoing",
    "_http_server",
    "_stream_duplex",
    "_stream_passthrough",
    "_stream_readable",
    "_stream_transform",
    "_stream_wrap",
    "_stream_writable",
    "_tls_common",
    "_tls_wrap",
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

/// Exact reviewed labels only. The caller charges lookup and descriptor allocation.
pub fn node_standard_library(source_set_id: &str, specifier: &str) -> Option<ExternalPackage> {
    let root = node_root(specifier)?;
    Some(ExternalPackage {
        identity: ExternalIdentity {
            kind: ExternalKind::ExternalPackage,
            source_set_id: source_set_id.into(),
            language: "javascript".into(),
            package: root.into(),
            standard_library: true,
        },
        ecosystem: "node",
        evidence: vec![ExternalEvidence::StandardLibraryCatalog {
            catalog: "node-standard-library:v1",
            entry: root.into(),
        }],
    })
}

fn node_root(specifier: &str) -> Option<&'static str> {
    match specifier {
        "fs" | "node:fs" | "fs/promises" | "node:fs/promises" => Some("fs"),
        "path" | "node:path" => Some("path"),
        _ => None,
    }
}

const NPM_SECTIONS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

// Only relevant objects are retained. Their duplicate keys must not become last-wins maps.
#[derive(Default)]
struct NpmManifest {
    name: Option<String>,
    workspaces: bool,
    sections: BTreeMap<String, NpmSection>,
}
struct NpmSection(BTreeMap<String, serde_json::Value>);
impl<'de> Deserialize<'de> for NpmSection {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct SectionVisitor;
        impl<'de> serde::de::Visitor<'de> for SectionVisitor {
            type Value = NpmSection;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a dependency object with unique keys")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut entries = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if entries.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate dependency key"));
                    }
                }
                Ok(NpmSection(entries))
            }
        }
        deserializer.deserialize_map(SectionVisitor)
    }
}
impl<'de> Deserialize<'de> for NpmManifest {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct ManifestVisitor;
        impl<'de> serde::de::Visitor<'de> for ManifestVisitor {
            type Value = NpmManifest;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a package manifest with unique relevant fields")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut result = NpmManifest::default();
                let mut seen = std::collections::BTreeSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if NPM_SECTIONS.contains(&key.as_str())
                        || matches!(key.as_str(), "name" | "workspaces")
                    {
                        if !seen.insert(key.clone()) {
                            return Err(serde::de::Error::custom(
                                "duplicate relevant manifest field",
                            ));
                        }
                        match key.as_str() {
                            "name" => result.name = Some(map.next_value()?),
                            "workspaces" => {
                                map.next_value::<serde::de::IgnoredAny>()?;
                                result.workspaces = true;
                            }
                            _ => {
                                result.sections.insert(key, map.next_value()?);
                            }
                        }
                    } else {
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                Ok(result)
            }
        }
        deserializer.deserialize_map(ManifestVisitor)
    }
}

#[derive(Default)]
pub struct JavascriptPackages {
    bindings: BTreeMap<String, BTreeMap<String, Option<ExternalPackage>>>,
}
impl JavascriptPackages {
    pub(crate) fn new(
        manifests: &[ManifestInput],
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut result = Self::default();
        let mut seen = std::collections::BTreeSet::new();
        for manifest in manifests {
            super::owner_step(work, check)?;
            if manifest.path != "package.json" {
                continue;
            }
            ensure!(
                seen.insert(&manifest.source_set_id),
                "store_unavailable: duplicate architecture package manifest"
            );
            ensure!(
                manifest.bytes.is_some() == manifest.content_hash.is_some(),
                "store_unavailable: architecture manifest content"
            );
            let Some(bytes) = &manifest.bytes else {
                continue;
            };
            ensure!(
                work.charge(bytes.len()),
                "bounded_work: reference manifest parsing"
            );
            check()?;
            let parsed = serde_json::from_slice::<NpmManifest>(bytes);
            check()?;
            let Ok(parsed) = parsed else {
                continue;
            };
            if parsed.workspaces {
                continue;
            }
            let mut bindings = BTreeMap::<String, Option<ExternalPackage>>::new();
            for (section, entries) in parsed.sections {
                for (key, value) in entries.0 {
                    super::owner_step(work, check)?;
                    if !valid_package("javascript", &key, false) {
                        continue;
                    }
                    let version = value.as_str().filter(|value| npm_constraint(value));
                    let version = version.filter(|_| parsed.name.as_deref() != Some(key.as_str()));
                    if let Some(version) = version {
                        let allocation = manifest
                            .source_set_id
                            .len()
                            .saturating_mul(2)
                            .saturating_add(manifest.native_revision_id.len())
                            .saturating_add(manifest.dependency_hash.len())
                            .saturating_add(manifest.path.len())
                            .saturating_add(manifest.content_hash.as_ref().map_or(0, String::len))
                            .saturating_add(key.len().saturating_mul(3))
                            .saturating_add(section.len())
                            .saturating_add(version.len())
                            .saturating_add("javascript".len());
                        ensure!(
                            work.charge(allocation),
                            "bounded_work: reference package evidence"
                        );
                        check()?;
                    }
                    let descriptor = version.map(|version| ExternalPackage {
                        identity: ExternalIdentity {
                            kind: ExternalKind::ExternalPackage,
                            source_set_id: manifest.source_set_id.clone(),
                            language: "javascript".into(),
                            package: key.clone(),
                            standard_library: false,
                        },
                        ecosystem: "npm",
                        evidence: vec![ExternalEvidence::Manifest {
                            source_set_id: manifest.source_set_id.clone(),
                            native_revision_id: manifest.native_revision_id.clone(),
                            dependency_hash: manifest.dependency_hash.clone(),
                            path: manifest.path.clone(),
                            content_hash: manifest
                                .content_hash
                                .clone()
                                .expect("present manifest hash"),
                            import_root: key.clone(),
                            section: section.clone(),
                            key: key.clone(),
                            version_constraint: Some(version.into()),
                        }],
                    });
                    match bindings.entry(key) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(descriptor);
                        }
                        std::collections::btree_map::Entry::Occupied(mut entry) => {
                            match (entry.get_mut().as_mut(), descriptor) {
                                (Some(existing), Some(additional)) => {
                                    existing.evidence.extend(additional.evidence);
                                    existing.evidence.sort();
                                    existing.evidence.dedup();
                                }
                                _ => {
                                    *entry.get_mut() = None;
                                }
                            }
                        }
                    }
                }
            }
            result
                .bindings
                .insert(manifest.source_set_id.clone(), bindings);
        }
        Ok(result)
    }

    /// A captured root declaration relation, not node_modules/member/export-map resolution.
    /// Call only for supported static syntax after local/context checks and work charging.
    pub fn resolve(&self, source_set_id: &str, specifier: &str) -> Option<ExternalPackage> {
        if let Some(package) = node_standard_library(source_set_id, specifier) {
            return Some(package);
        }
        self.binding(source_set_id, specifier).cloned()
    }

    /// Borrow-only size of owned evidence copied by resolve, including misses.
    /// The caller charges this to the shared budget and checks cancellation first.
    pub fn resolution_work(&self, source_set_id: &str, specifier: &str) -> usize {
        if let Some(root) = node_root(specifier) {
            return 1usize
                .saturating_add(source_set_id.len())
                .saturating_add("javascript".len())
                .saturating_add(root.len().saturating_mul(2));
        }
        let Some(package) = self.binding(source_set_id, specifier) else {
            return 1;
        };
        let mut size = 1usize
            .saturating_add(package.identity.source_set_id.len())
            .saturating_add(package.identity.language.len())
            .saturating_add(package.identity.package.len());
        for evidence in &package.evidence {
            if let ExternalEvidence::Manifest {
                source_set_id,
                native_revision_id,
                dependency_hash,
                path,
                content_hash,
                import_root,
                section,
                key,
                version_constraint,
            } = evidence
            {
                for value in [
                    source_set_id,
                    native_revision_id,
                    dependency_hash,
                    path,
                    content_hash,
                    import_root,
                    section,
                    key,
                ] {
                    size = size.saturating_add(value.len());
                }
                size = size.saturating_add(version_constraint.as_ref().map_or(0, String::len));
            }
        }
        size
    }

    fn binding(&self, source_set_id: &str, specifier: &str) -> Option<&ExternalPackage> {
        if specifier.starts_with("node:") {
            return None;
        }
        let root = npm_root(specifier)?;
        if NODE_BARE_ROOTS.binary_search(&root).is_ok() {
            return None;
        }
        self.bindings.get(source_set_id)?.get(root)?.as_ref()
    }
}
fn npm_constraint(value: &str) -> bool {
    let value = value.strip_prefix(['^', '~']).unwrap_or(value);
    let mut parts = value.split('.');
    (0..3).all(|_| {
        parts.next().is_some_and(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0'))
        })
    }) && parts.next().is_none()
}
fn npm_root(specifier: &str) -> Option<&str> {
    let mut parts = specifier.split('/');
    let first = parts.next()?;
    let root_len = if first.starts_with('@') {
        first.len() + 1 + parts.next()?.len()
    } else {
        first.len()
    };
    if parts.any(|part| {
        part.is_empty()
            || matches!(part, "." | "..")
            || !part
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
    }) {
        return None;
    }
    let root = &specifier[..root_len];
    valid_package("javascript", root, false).then_some(root)
}

#[derive(Default)]
pub struct RustPackages {
    bindings: BTreeMap<(String, String), (String, Option<ExternalPackage>)>,
    roots: BTreeMap<String, (bool, bool)>,
}
impl RustPackages {
    pub(crate) fn new(
        manifests: &[ManifestInput],
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut result = Self::default();
        let mut seen = std::collections::BTreeSet::new();
        for manifest in manifests {
            super::owner_step(work, check)?;
            ensure!(
                seen.insert((&manifest.source_set_id, &manifest.path)),
                "store_unavailable: duplicate architecture manifest"
            );
            if manifest.path != "Cargo.toml" {
                continue;
            }
            ensure!(
                manifest.bytes.is_some() == manifest.content_hash.is_some(),
                "store_unavailable: architecture manifest content"
            );
            let Some(bytes) = &manifest.bytes else {
                continue;
            };
            // Parsing cost and all subsequent fan-out share the existing request work budget.
            ensure!(
                work.charge(bytes.len()),
                "bounded_work: reference manifest parsing"
            );
            check()?;
            let Ok(text) = std::str::from_utf8(bytes) else {
                continue;
            };
            let parsed = toml::from_str::<toml::Value>(text);
            check()?;
            let Ok(value) = parsed else { continue };
            let Some(table) = value.as_table() else {
                continue;
            };
            let Some(roots) = conventional_roots(table) else {
                continue;
            };
            result.roots.insert(manifest.source_set_id.clone(), roots);
            for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(dependencies) = table.get(section).and_then(toml::Value::as_table) {
                    result.section(manifest, section, dependencies, work, check)?;
                }
            }
            if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
                for (target, value) in targets {
                    super::owner_step(work, check)?;
                    let Some(target_table) = value.as_table() else {
                        continue;
                    };
                    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                        if let Some(dependencies) =
                            target_table.get(section).and_then(toml::Value::as_table)
                        {
                            result.section(
                                manifest,
                                &format!("target.{target}.{section}"),
                                dependencies,
                                work,
                                check,
                            )?;
                        }
                    }
                }
            }
        }
        Ok(result)
    }
    fn section(
        &mut self,
        manifest: &ManifestInput,
        section: &str,
        dependencies: &toml::map::Map<String, toml::Value>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        for (key, value) in dependencies {
            super::owner_step(work, check)?;
            let root = key.replace('-', "_");
            if !identifier(&root) {
                continue;
            }
            let (package, version, proved) = match value {
                toml::Value::String(version) => (key.as_str(), Some(version.as_str()), true),
                toml::Value::Table(declaration) => {
                    let package = declaration
                        .get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(key);
                    let version = declaration.get("version").and_then(toml::Value::as_str);
                    let proved = !declaration.contains_key("path")
                        && !declaration.contains_key("workspace")
                        && declaration.get("package").is_none_or(toml::Value::is_str)
                        && declaration.get("version").is_none_or(toml::Value::is_str)
                        && declaration.get("git").is_none_or(toml::Value::is_str)
                        && (version.is_some()
                            || declaration
                                .get("git")
                                .and_then(toml::Value::as_str)
                                .is_some());
                    (package, version, proved)
                }
                _ => (key.as_str(), None, false),
            };
            let descriptor =
                (proved && valid_package("rust", package, false)).then(|| ExternalPackage {
                    identity: ExternalIdentity {
                        kind: ExternalKind::ExternalPackage,
                        source_set_id: manifest.source_set_id.clone(),
                        language: "rust".into(),
                        package: package.into(),
                        standard_library: false,
                    },
                    ecosystem: "cargo",
                    evidence: vec![ExternalEvidence::Manifest {
                        source_set_id: manifest.source_set_id.clone(),
                        native_revision_id: manifest.native_revision_id.clone(),
                        dependency_hash: manifest.dependency_hash.clone(),
                        path: manifest.path.clone(),
                        content_hash: manifest
                            .content_hash
                            .clone()
                            .expect("present manifest hash"),
                        import_root: root.clone(),
                        section: section.into(),
                        key: key.clone(),
                        version_constraint: version.map(str::to_owned),
                    }],
                });
            match self.bindings.entry((manifest.source_set_id.clone(), root)) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert((key.clone(), descriptor));
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let (original_key, binding) = entry.get_mut();
                    match (binding.as_mut(), descriptor) {
                        (Some(existing), Some(additional))
                            if original_key == key && existing.identity == additional.identity =>
                        {
                            existing.evidence.extend(additional.evidence);
                            existing.evidence.sort();
                            existing.evidence.dedup();
                        }
                        _ => *binding = None,
                    }
                }
            }
        }
        Ok(())
    }
    /// Check captured Cargo root settings only after the resolver proves the module chain.
    pub fn root_allowed(&self, source_set_id: &str, root_path: &str) -> bool {
        self.roots
            .get(source_set_id)
            .is_some_and(|(lib, bin)| match root_path {
                "src/lib.rs" => *lib,
                "src/main.rs" => *bin,
                _ => false,
            })
    }
    /// The Rust module resolver, not this manifest parser, proves root ownership.
    /// Call only after applying the language's local binding precedence.
    pub fn resolve(
        &self,
        source_set_id: &str,
        import_root: &str,
        root_owned: bool,
    ) -> Option<ExternalPackage> {
        if root_owned
            && let Some(binding) = self
                .bindings
                .get(&(source_set_id.into(), import_root.into()))
        {
            return binding.1.clone();
        }
        if !matches!(import_root, "std" | "core" | "alloc") {
            return None;
        }
        Some(ExternalPackage {
            identity: ExternalIdentity {
                kind: ExternalKind::ExternalPackage,
                source_set_id: source_set_id.into(),
                language: "rust".into(),
                package: import_root.into(),
                standard_library: true,
            },
            ecosystem: "rust",
            evidence: vec![ExternalEvidence::StandardLibraryCatalog {
                catalog: "rust-standard-library:v1",
                entry: import_root.into(),
            }],
        })
    }
}
fn conventional_roots(table: &toml::map::Map<String, toml::Value>) -> Option<(bool, bool)> {
    let package = table.get("package")?.as_table()?;
    let name = package.get("name")?.as_str()?;
    if !valid_package("rust", name, false) {
        return None;
    }
    let lib = match table.get("lib") {
        Some(lib) => lib.as_table().is_some_and(|lib| {
            lib.get("path")
                .is_none_or(|path| path.as_str() == Some("src/lib.rs"))
        }),
        None => package.get("autolib").and_then(toml::Value::as_bool) != Some(false),
    };
    let bin = match table.get("bin") {
        Some(bins) => bins.as_array().is_some_and(|bins| {
            bins.iter().any(|bin| {
                bin.as_table().is_some_and(|bin| match bin.get("path") {
                    Some(path) => path.as_str() == Some("src/main.rs"),
                    None => bin.get("name").and_then(toml::Value::as_str) == Some(name),
                })
            })
        }),
        None => package.get("autobins").and_then(toml::Value::as_bool) != Some(false),
    };
    Some((lib, bin))
}

#[cfg(test)]
mod python_catalog_tests {
    use super::*;

    #[test]
    fn python_catalog_roots_are_sorted_and_unique() {
        assert_eq!(PYTHON_STDLIB_ROOTS.len(), 300);
        assert!(PYTHON_STDLIB_ROOTS.windows(2).all(|pair| pair[0] < pair[1]));
        for root in PYTHON_STDLIB_ROOTS {
            assert!(python_standard_library("fixture", root).is_some());
        }
    }

    #[test]
    fn python_catalog_recognizes_pinned_cross_platform_roots() {
        for root in [
            "os", "email", "asyncio", "tomllib", "_winapi", "posix", "cProfile",
        ] {
            let package = python_standard_library("fixture", root).expect("pinned stdlib root");
            assert_eq!(package.identity.source_set_id, "fixture");
            assert_eq!(package.identity.language, "python");
            assert_eq!(package.identity.package, root);
            assert!(package.identity.standard_library);
            assert_eq!(package.ecosystem, "python");
            assert_eq!(
                package.evidence,
                vec![ExternalEvidence::StandardLibraryCatalog {
                    catalog: "python-standard-library:v2",
                    entry: root.into(),
                }]
            );
        }
        for root in [
            "email.mime",
            "jsonish",
            "OS",
            "requests",
            "test",
            "_testcapi",
        ] {
            assert!(python_standard_library("fixture", root).is_none());
        }
    }
}

#[cfg(test)]
mod javascript_package_tests {
    use super::*;

    fn packages(text: &str) -> JavascriptPackages {
        JavascriptPackages::new(
            &[input(text)],
            &mut Work::new(super::super::work::reference_limit(0, 0)),
            &mut || Ok(()),
        )
        .unwrap()
    }
    fn input(text: &str) -> ManifestInput {
        ManifestInput {
            source_set_id: "fixture".into(),
            native_revision_id: "revision".into(),
            dependency_hash: "dependencies".into(),
            path: "package.json".into(),
            content_hash: Some("hash".into()),
            bytes: Some(text.as_bytes().to_vec()),
        }
    }
    #[test]
    fn node_labels_and_reserved_roots_are_exact_and_conservative() {
        assert_eq!(NODE_BARE_ROOTS.len(), 56);
        assert!(NODE_BARE_ROOTS.windows(2).all(|pair| pair[0] < pair[1]));
        for (specifier, root) in [
            ("fs", "fs"),
            ("node:fs", "fs"),
            ("fs/promises", "fs"),
            ("node:fs/promises", "fs"),
            ("path", "path"),
            ("node:path", "path"),
        ] {
            let package = node_standard_library("fixture", specifier).unwrap();
            assert_eq!(package.identity.package, root);
            assert!(package.identity.standard_library);
            assert_eq!(package.ecosystem, "node");
            assert_eq!(
                package.evidence,
                vec![ExternalEvidence::StandardLibraryCatalog {
                    catalog: "node-standard-library:v1",
                    entry: root.into(),
                }]
            );
        }
        let dependencies: BTreeMap<_, _> = NODE_BARE_ROOTS
            .iter()
            .map(|root| (*root, "1.2.3"))
            .collect();
        let packages = packages(&serde_json::json!({"dependencies":dependencies}).to_string());
        for root in NODE_BARE_ROOTS {
            if !matches!(*root, "fs" | "path") {
                assert!(packages.resolve("fixture", root).is_none());
            }
            assert!(
                packages
                    .resolve("fixture", &format!("{root}/fake"))
                    .is_none()
            );
        }
        for specifier in [
            "node:unknown",
            "node:test",
            "node:sea",
            "node:node:fs",
            "path/posix",
            "node:path/win32",
            "./fs.js",
            "../path.js",
        ] {
            assert!(packages.resolve("fixture", specifier).is_none());
        }
    }
    #[test]
    fn captured_bindings_merge_sections_but_never_infer_installation_or_members() {
        let packages = packages(
            r#"{"dependencies":{"widget":"^1.2.3","@scope/tool":"2.3.4","test":"1.0.0","sea":"1.0.0"},"devDependencies":{"widget":"~2.3.4"},"optionalDependencies":{"widget":"3.4.5"},"peerDependencies":{"widget":"4.5.6"}}"#,
        );
        assert!(packages.resolution_work("fixture", "widget/sub/path") > 4);
        assert_eq!(packages.resolution_work("fixture", "absent"), 1);
        assert!(packages.resolution_work("fixture", "node:fs") > 1);
        let widget = packages.resolve("fixture", "widget/sub/path").unwrap();
        assert_eq!(widget.identity.package, "widget");
        assert!(!widget.identity.standard_library);
        assert_eq!(widget.ecosystem, "npm");
        assert_eq!(widget.evidence.len(), 4);
        for evidence in widget.evidence {
            let ExternalEvidence::Manifest {
                import_root,
                key,
                version_constraint,
                ..
            } = evidence
            else {
                panic!("manifest evidence");
            };
            assert_eq!(import_root, "widget");
            assert_eq!(key, "widget");
            assert!(version_constraint.is_some());
        }
        for specifier in ["@scope/tool", "@scope/tool/sub", "test/reporters", "sea"] {
            assert!(packages.resolve("fixture", specifier).is_some());
        }
        for specifier in [
            "absent",
            "widget/",
            "widget//x",
            "widget/../x",
            "widget/x?query",
            "widget/x#hash",
            "widget/x\\y",
            "@scope",
            "@scope//x",
            "https://widget",
            "#widget",
        ] {
            assert!(packages.resolve("fixture", specifier).is_none());
        }
        assert!(packages.resolve("other-source-set", "widget").is_none());
    }
    #[test]
    fn unsupported_competitors_duplicates_and_contexts_never_last_win() {
        for constraint in [
            "npm:other@1.2.3",
            "workspace:*",
            "file:../x",
            "latest",
            "*",
            ">=1.2.3",
            "1.2",
            "1.2.3-beta",
            "1.2.3+build",
            " 1.2.3",
            "01.2.3",
        ] {
            let text = serde_json::json!({"dependencies":{"widget":"1.2.3"}, "peerDependencies":{"widget":constraint}}).to_string();
            assert!(
                packages(&text).resolve("fixture", "widget").is_none(),
                "{constraint}"
            );
        }
        for text in [
            r#"{"dependencies":{"widget":"1.2.3","widget":"2.3.4"}}"#,
            r#"{"dependencies":{"widget":"1.2.3"},"dependencies":{"widget":"2.3.4"}}"#,
            r#"{"dependencies":{"widget":"1.2.3"},"devDependencies":{"widget":42}}"#,
            r#"{"name":"widget","dependencies":{"widget":"1.2.3"}}"#,
            r#"{"workspaces":["packages/*"],"dependencies":{"widget":"1.2.3"}}"#,
            r#"{"dependencies":{"widget":"1.2.3"},"peerDependencies":[]}"#,
            "invalid json",
        ] {
            assert!(
                packages(text).resolve("fixture", "widget").is_none(),
                "{text}"
            );
        }
    }
    #[test]
    fn parsing_shares_work_budget_and_checks_cancellation() {
        let input = input(r#"{"dependencies":{"widget":"1.2.3"}}"#);
        let mut work = Work::new(0);
        let error =
            JavascriptPackages::new(std::slice::from_ref(&input), &mut work, &mut || Ok(()))
                .err()
                .unwrap();
        assert!(error.to_string().starts_with("bounded_work:"));
        let mut visits = 0;
        let error = JavascriptPackages::new(
            &[input],
            &mut Work::new(super::super::work::reference_limit(0, 0)),
            &mut || {
                visits += 1;
                if visits >= 2 {
                    anyhow::bail!("cancelled");
                }
                Ok(())
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "cancelled");
    }
}

#[cfg(test)]
mod rust_package_tests {
    use super::*;

    const MANIFEST: &str = r#"[package]
name = "fixture"
version = "0.1.0"
edition = "2021"
[dependencies]
wire = { package = "upstream-wire", version = "^2" }
local = { path = "vendor/local", version = "1" }
[dev-dependencies]
test_wire = { package = "upstream-wire", version = "^3" }
"#;

    fn manifest(text: &str) -> ManifestInput {
        ManifestInput {
            source_set_id: "source-set:test".into(),
            native_revision_id: "revision:test".into(),
            dependency_hash: "dependency-hash".into(),
            path: "Cargo.toml".into(),
            content_hash: Some("content-hash".into()),
            bytes: Some(text.as_bytes().to_vec()),
        }
    }
    #[test]
    fn normalized_import_collisions_and_unproved_ownership_never_guess_packages() {
        let input = manifest(
            "[package]
name='fixture'
version='1.0.0'
[dependencies]
wire-core='1'
wire_core='1'
plain={package='literal-upstream',version='2'}
local={path='vendor/local',version='1'}
[build-dependencies]
build_wire={package='build-upstream',version='3'}
[target.'cfg(unix)'.dependencies]
target_wire={package='target-upstream',version='4'}",
        );
        let limit =
            crate::references::work::reference_limit(1, input.bytes.as_ref().map_or(0, Vec::len));
        let packages = RustPackages::new(&[input], &mut Work::new(limit), &mut || Ok(())).unwrap();
        assert!(
            packages
                .resolve("source-set:test", "wire_core", true)
                .is_none()
        );
        assert!(
            packages
                .resolve("source-set:test", "plain", false)
                .is_none()
        );
        assert!(packages.resolve("source-set:test", "local", true).is_none());
        for (root, name) in [
            ("plain", "literal-upstream"),
            ("build_wire", "build-upstream"),
            ("target_wire", "target-upstream"),
        ] {
            let package = packages.resolve("source-set:test", root, true).unwrap();
            assert_eq!(package.identity.package, name);
            assert!(!package.identity.standard_library);
        }
        let limit = crate::references::work::reference_limit(1, MANIFEST.len());
        let error = RustPackages::new(&[manifest(MANIFEST)], &mut Work::new(limit), &mut || {
            anyhow::bail!("cancelled")
        })
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "cancelled");
    }

    #[test]
    fn repeated_identical_binding_merges_evidence_and_cargo_roots_are_checked_separately() {
        let input = manifest(
            "[package]\nname='fixture'\nversion='1.0.0'\nautobins=false\n[dependencies]\nwire={package='upstream-wire',version='1'}\n[dev-dependencies]\nwire={package='upstream-wire',version='2'}\n[target.'cfg(unix)'.dependencies]\nwire={package='upstream-wire',version='3'}",
        );
        let limit =
            crate::references::work::reference_limit(1, input.bytes.as_ref().map_or(0, Vec::len));
        let packages = RustPackages::new(&[input], &mut Work::new(limit), &mut || Ok(())).unwrap();
        assert!(packages.root_allowed("source-set:test", "src/lib.rs"));
        assert!(!packages.root_allowed("source-set:test", "src/main.rs"));
        let package = packages.resolve("source-set:test", "wire", true).unwrap();
        assert_eq!(package.evidence.len(), 3);
        assert_eq!(package.identity.package, "upstream-wire");
        let input = manifest(
            "[package]\nname='fixture'\nversion='1.0.0'\n[dependencies]\nwire={package='one',version='1'}\n[dev-dependencies]\nwire={package='two',version='1'}",
        );
        let limit =
            crate::references::work::reference_limit(1, input.bytes.as_ref().map_or(0, Vec::len));
        let packages = RustPackages::new(&[input], &mut Work::new(limit), &mut || Ok(())).unwrap();
        assert!(packages.resolve("source-set:test", "wire", true).is_none());
    }
}
