//! scip-typescript module markers resolved against authenticated package roots.
//!
//! Owner ruling 2026-10-29 (#443): an empty-range module-definition marker is valid when its
//! module path equals the document path relative to the producer's project root or the
//! nearest package root. The project root is the descriptor's bound workspace root. Package
//! roots are the `package.json` files retained in the captured native config, never the
//! symbol string.
use serde_json::Value;
use std::collections::BTreeMap;

/// The npm package identity scip-typescript derives from one `package.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Package {
    name: String,
    version: String,
}

impl Package {
    fn anonymous() -> Self {
        Self {
            name: String::new(),
            version: String::new(),
        }
    }

    /// scip-typescript 0.4.0 `Packages.symbol`: a string `name` with a string `version`, or
    /// `HEAD` when the version is missing; anything else is the anonymous package.
    fn from_manifest(bytes: &[u8]) -> Self {
        let Ok(Value::Object(manifest)) = serde_json::from_slice::<Value>(bytes) else {
            return Self::anonymous();
        };
        let Some(Value::String(name)) = manifest.get("name") else {
            return Self::anonymous();
        };
        let version = match manifest.get("version") {
            Some(Value::String(version)) => version.clone(),
            _ => "HEAD".to_owned(),
        };
        // A symbol spells an empty field as `.`, which parses back to empty.
        let field = |text: &str| {
            if text == "." {
                String::new()
            } else {
                text.to_owned()
            }
        };
        Self {
            name: field(name),
            version: field(&version),
        }
    }
}

/// Package roots keyed by workspace-relative directory (`""` is the workspace root).
#[derive(Debug, Default, Clone)]
pub struct ModuleRoots {
    packages: BTreeMap<String, Package>,
}

impl ModuleRoots {
    /// Reads the present `package.json` entries of a retained native config.
    pub fn from_native_config(config: &[Value]) -> Self {
        let mut packages = BTreeMap::new();
        for entry in config {
            let (Some(path), Some("present"), Some(bytes)) = (
                entry["path"].as_str(),
                entry["type"].as_str(),
                entry["bytes"].as_str(),
            ) else {
                continue;
            };
            let dir = match path.strip_suffix("package.json") {
                Some("") => "",
                Some(dir) => match dir.strip_suffix('/') {
                    Some(dir) => dir,
                    None => continue,
                },
                None => continue,
            };
            let Ok(bytes) = hex::decode(bytes) else {
                continue;
            };
            packages.insert(dir.to_owned(), Package::from_manifest(&bytes));
        }
        Self { packages }
    }

    /// The package and module path scip-typescript assigns to `document`: the nearest
    /// ancestor directory with a `package.json`, else the project (workspace) root.
    fn resolve<'d>(&self, document: &'d str) -> (Package, &'d str) {
        let mut dir = document;
        loop {
            dir = dir.rsplit_once('/').map_or("", |(parent, _)| parent);
            let relative = if dir.is_empty() {
                document
            } else {
                &document[dir.len() + 1..]
            };
            if let Some(package) = self.packages.get(dir) {
                return (package.clone(), relative);
            }
            if dir.is_empty() {
                return (Package::anonymous(), relative);
            }
        }
    }

    /// Whether `symbol` is exactly the module marker of `document` under its package root.
    pub(crate) fn names_document(&self, symbol: &scip::types::Symbol, document: &str) -> bool {
        let Some(path) = typescript_module_path(symbol) else {
            return false;
        };
        let (package, relative) = self.resolve(document);
        let (manager, name, version) = symbol.package.as_ref().map_or(("", "", ""), |p| {
            (&p.manager[..], &p.name[..], &p.version[..])
        });
        manager == "npm" && name == package.name && version == package.version && path == relative
    }
}

/// The module path of a scip-typescript file marker: every descriptor is a package segment.
pub(crate) fn typescript_module_path(symbol: &scip::types::Symbol) -> Option<String> {
    use scip::types::descriptor::Suffix;
    (symbol.scheme == "scip-typescript"
        && !symbol.descriptors.is_empty()
        && symbol
            .descriptors
            .iter()
            .all(|descriptor| descriptor.suffix.enum_value() == Ok(Suffix::Package)))
    .then(|| {
        symbol
            .descriptors
            .iter()
            .map(|descriptor| descriptor.name.as_str())
            .collect::<Vec<_>>()
            .join("/")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roots(manifests: &[(&str, &str)]) -> ModuleRoots {
        let config: Vec<Value> = manifests
            .iter()
            .map(|(path, text)| {
                json!({"path": path, "type": "present", "bytes": hex::encode(text), "byteLength": text.len()})
            })
            .chain([json!({"path": "Cargo.toml", "type": "present", "bytes": "00"})])
            .chain([json!({"path": "lib/package.json", "type": "absent"})])
            .collect();
        ModuleRoots::from_native_config(&config)
    }

    fn names(roots: &ModuleRoots, symbol: &str, document: &str) -> bool {
        roots.names_document(&scip::symbol::parse_symbol(symbol).unwrap(), document)
    }

    #[test]
    fn markers_resolve_against_the_nearest_package_root_or_project_root() {
        let roots = roots(&[
            ("package.json", r#"{"name":"root","version":"2.0.0"}"#),
            ("tools/x/package.json", r#"{"name":"@s/x"}"#),
            ("tools/y/package.json", r#"{"version":"1.0.0"}"#),
            ("tools/z/package.json", "not json"),
        ]);
        assert!(names(
            &roots,
            "scip-typescript npm root 2.0.0 src/`a.js`/",
            "src/a.js"
        ));
        assert!(names(
            &roots,
            "scip-typescript npm @s/x HEAD `a.js`/",
            "tools/x/a.js"
        ));
        assert!(names(
            &roots,
            "scip-typescript npm @s/x HEAD lib/`a.js`/",
            "tools/x/lib/a.js"
        ));
        assert!(names(
            &roots,
            "scip-typescript npm . . `a.js`/",
            "tools/y/a.js"
        ));
        assert!(names(
            &roots,
            "scip-typescript npm . . `a.js`/",
            "tools/z/a.js"
        ));
        // Wrong package, version, manager, path, or a nearer root that was skipped.
        assert!(!names(
            &roots,
            "scip-typescript npm other 2.0.0 src/`a.js`/",
            "src/a.js"
        ));
        assert!(!names(
            &roots,
            "scip-typescript npm root 1.0.0 src/`a.js`/",
            "src/a.js"
        ));
        assert!(!names(
            &roots,
            "scip-typescript pip root 2.0.0 src/`a.js`/",
            "src/a.js"
        ));
        assert!(!names(
            &roots,
            "scip-typescript npm root 2.0.0 src/`b.js`/",
            "src/a.js"
        ));
        assert!(!names(
            &roots,
            "scip-typescript npm root 2.0.0 tools/x/`a.js`/",
            "tools/x/a.js"
        ));
        assert!(!names(
            &roots,
            "scip-typescript npm root 2.0.0 `a.js`/",
            "src/a.js"
        ));
        assert!(!names(
            &roots,
            "scip-typescript npm root 2.0.0 src/`a.js`/f().",
            "src/a.js"
        ));
        let anonymous = ModuleRoots::default();
        assert!(names(
            &anonymous,
            "scip-typescript npm . . src/`a.js`/",
            "src/a.js"
        ));
        assert!(!names(
            &anonymous,
            "scip-typescript npm root 2.0.0 src/`a.js`/",
            "src/a.js"
        ));
        assert!(!names(
            &anonymous,
            "scip-typescript npm . . Users/me/src/`a.js`/",
            "src/a.js"
        ));
    }
}
