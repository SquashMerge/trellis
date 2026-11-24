//! Stable source membership and revision-current, source-local package descriptors.
use super::*;

type Identity = external::ExternalIdentity;
pub(super) type SourcePackageDescriptors = BTreeMap<Identity, external::ExternalPackage>;

pub(super) struct ExternalPackageTable {
    pub(super) by_source: Vec<SourcePackageDescriptors>,
}

impl ExternalPackageTable {
    pub(super) fn new(documents: usize) -> Self {
        Self {
            by_source: vec![BTreeMap::new(); documents],
        }
    }
    pub(super) fn complete(
        &mut self,
        source: usize,
        descriptors: SourcePackageDescriptors,
    ) -> BTreeSet<Identity> {
        let membership = descriptors.keys().cloned().collect();
        self.by_source[source] = descriptors;
        membership
    }
    pub(super) fn validate(&self, sources: &[Option<Arc<SourceReferences>>]) -> Result<()> {
        ensure!(
            self.by_source.len() == sources.len(),
            "store_unavailable: reference package slots"
        );
        for (source, descriptors) in sources.iter().zip(&self.by_source) {
            ensure!(
                source.as_ref().map_or(descriptors.is_empty(), |source| {
                    source.packages.iter().eq(descriptors.keys())
                }) && descriptors
                    .iter()
                    .all(|(identity, package)| identity == &package.identity),
                "store_unavailable: reference package membership"
            );
        }
        Ok(())
    }
    pub(super) fn selected<'a>(
        &'a self,
        source: usize,
        membership: &'a BTreeSet<Identity>,
    ) -> impl Iterator<Item = (&'a Identity, &'a external::ExternalPackage)> {
        membership
            .iter()
            .map(move |identity| (identity, &self.by_source[source][identity]))
    }

    pub(super) fn canonical(&self) -> Value {
        let Self { by_source } = self;
        json!(
            by_source
                .iter()
                .map(|slot| slot.iter().collect::<Vec<_>>())
                .collect::<Vec<_>>()
        )
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::references::parallel_tests::snapshot_of;
    use sha2::{Digest, Sha256};

    const CARGO: &str = "[package]\nname='fixture'\nversion='0.1.0'\nedition='2021'\n\
        [dependencies]\nrenamed={package='serde',version='1'}\n\
        [dev-dependencies]\nrenamed={package='serde',version='1'}\n";
    const NPM: &str =
        r#"{"dependencies":{"shared":"1.0.0","fs":"9.0.0"},"devDependencies":{"shared":"1.0.0"}}"#;

    fn manifest(snapshot: &Snapshot, path: &str, text: &str) -> external::ManifestInput {
        external::ManifestInput {
            source_set_id: snapshot.documents[0].key.source_set_id.clone(),
            native_revision_id: "native".into(),
            dependency_hash: "dependencies".into(),
            path: path.into(),
            content_hash: Some(hex::encode(Sha256::digest(text.as_bytes()))),
            bytes: Some(text.as_bytes().to_vec()),
        }
    }
    fn fixture() -> Snapshot {
        let mut snapshot = snapshot_of(&[
            (
                "src/lib.rs",
                "pub fn run() { renamed::de::value(); renamed::de::value(); std::mem::drop(1); }\n",
            ),
            (
                "root.js",
                "import one from 'shared'; import two from 'shared'; import fs from 'node:fs';\n",
            ),
            ("other.js", "import one from 'shared';\n"),
            ("tools/nested.js", "import one from 'shared';\n"),
            ("typed.ts", "import one from 'shared';\n"),
            (
                "Use.java",
                "import java.util.List; public class Use { List value; }\n",
            ),
            ("use.py", "import sys\nsys.exit()\n"),
            ("idle.py", ""),
        ]);
        snapshot.manifests = vec![
            manifest(&snapshot, "Cargo.toml", CARGO),
            manifest(&snapshot, "package.json", NPM),
            manifest(
                &snapshot,
                "tools/package.json",
                r#"{"optionalDependencies":{"shared":"2.0.0"}}"#,
            ),
        ];
        snapshot
    }
    fn slot(base: &ReferenceBase, path: &str) -> usize {
        base.identities
            .iter()
            .position(|identity| identity.path == path)
            .unwrap()
    }
    fn select(base: &ReferenceBase, paths: &[&str]) -> Value {
        base.select(
            &paths.iter().map(|path| (*path).into()).collect::<Vec<_>>(),
            5,
            &mut || Ok(()),
        )
        .unwrap()
        .data
    }
    fn package(data: &Value, language: &str, name: &str) -> Value {
        data["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|package| {
                package["identity"]["language"] == language
                    && package["identity"]["package"] == name
            })
            .unwrap_or_else(|| panic!("missing {language} {name}: {data}"))
            .clone()
    }

    #[test]
    fn cargo_merged_evidence() {
        let snapshot = fixture();
        let mut work = Work::new(1_000_000);
        let packages =
            external::RustPackages::new(&snapshot.manifests, &mut work, &mut || Ok(())).unwrap();
        let source_set = &snapshot.documents[0].key.source_set_id;
        let descriptor = packages.resolve(source_set, "renamed", true).unwrap();
        let expected = ["dependencies", "dev-dependencies"].map(|section| {
            json!({
                "kind":"manifest", "sourceSetId":source_set, "nativeRevisionId":"native",
                "dependencyHash":"dependencies", "path":"Cargo.toml",
                "contentHash":snapshot.manifests[0].content_hash,
                "importRoot":"renamed", "section":section, "key":"renamed", "versionConstraint":"1",
            })
        });
        assert_eq!(descriptor.ecosystem, "cargo");
        assert_eq!(
            serde_json::to_value(descriptor.evidence).unwrap(),
            json!(expected)
        );
        let mut conflict = snapshot.manifests[0].clone();
        conflict.bytes = Some(
            CARGO
                .replace(
                    "[dev-dependencies]\nrenamed={package='serde'",
                    "[dev-dependencies]\nrenamed={package='other'",
                )
                .into_bytes(),
        );
        let packages = external::RustPackages::new(&[conflict], &mut work, &mut || Ok(())).unwrap();
        assert!(packages.resolve(source_set, "renamed", true).is_none());
    }

    #[test]
    fn npm_sections_and_suppression() {
        let snapshot = fixture();
        let manifests = [manifest(
            &snapshot,
            "package.json",
            r#"{
            "dependencies":{"shared":"1.0.0","bad":null,"conflict":"1.0.0"},
            "devDependencies":{"shared":"1.0.0","conflict":17},
            "peerDependencies":{"shared":"2.0.0"},
            "optionalDependencies":{"shared":"3.0.0"}}"#,
        )];
        let mut work = Work::new(1_000_000);
        let workspace =
            js_workspace::Workspaces::new(&manifests, &mut work, &mut || Ok(())).unwrap();
        let packages =
            external::JavascriptPackages::new(&manifests, &workspace, &mut work, &mut || Ok(()))
                .unwrap();
        let source_set = &snapshot.documents[0].key.source_set_id;
        let descriptor = packages
            .resolve(source_set, "root.js", "shared", "javascript")
            .unwrap();
        let sections: BTreeSet<_> = descriptor
            .evidence
            .iter()
            .map(|evidence| match evidence {
                external::ExternalEvidence::Manifest { section, .. } => section.as_str(),
                _ => panic!("npm manifest evidence"),
            })
            .collect();
        assert_eq!(
            sections,
            BTreeSet::from([
                "dependencies",
                "devDependencies",
                "peerDependencies",
                "optionalDependencies"
            ])
        );
        for specifier in ["bad", "conflict"] {
            assert!(
                packages
                    .resolve(source_set, "root.js", specifier, "javascript")
                    .is_none()
            );
        }
    }

    #[test]
    fn membership_and_source_slots() {
        let snapshot = fixture();
        let base = resolve_all(&snapshot, &mut || Ok(())).unwrap();
        assert_eq!(base.sources.len(), snapshot.documents.len());
        base.external_packages.validate(&base.sources).unwrap();
        for (source, descriptors) in base.sources.iter().zip(&base.external_packages.by_source) {
            assert_eq!(
                source.as_ref().unwrap().packages,
                descriptors.keys().cloned().collect()
            );
        }
        let paths = vec!["root.js".into()];
        let mut check = || Ok(());
        let request = parallel_check::RequestCheck::new(&mut check);
        let scoped = resolve(
            &snapshot,
            Sources {
                paths: &paths,
                prefix: None,
                files: None,
                test_features: &[],
            },
            5,
            1,
            &request,
        )
        .unwrap();
        for (index, identity) in scoped.identities.iter().enumerate() {
            assert_eq!(scoped.sources[index].is_some(), identity.path == "root.js");
            if identity.path != "root.js" {
                assert!(scoped.external_packages.by_source[index].is_empty());
                assert!(scoped.source_facts()[index].plan.is_none());
            }
        }
        let idle = slot(&base, "idle.py");
        assert!(base.sources[idle].is_some());
        assert!(base.source_facts()[idle].plan.is_some());
        assert_eq!(base.source_facts()[idle].sites, 0);
        let mut table = ExternalPackageTable::new(base.sources.len());
        assert!(
            table.validate(&base.sources).is_err(),
            "missing descriptors fail assembly"
        );
        table.by_source = base.external_packages.by_source.clone();
        let root = slot(&base, "root.js");
        let identity = table.by_source[root].keys().next().unwrap().clone();
        table.by_source[root]
            .get_mut(&identity)
            .unwrap()
            .identity
            .package = "wrong".into();
        assert!(
            table.validate(&base.sources).is_err(),
            "descriptor identity must match its key"
        );
    }

    #[test]
    fn selected_union_deduplicates_without_leaking() {
        let base = resolve_all(&fixture(), &mut || Ok(())).unwrap();
        let root = package(&select(&base, &["root.js"]), "javascript", "shared");
        let nested = package(&select(&base, &["tools/nested.js"]), "javascript", "shared");
        assert_eq!(root["evidence"].as_array().unwrap().len(), 2);
        assert_eq!(nested["evidence"].as_array().unwrap().len(), 1);
        assert!(
            root["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["path"] == "package.json")
        );
        assert_eq!(nested["evidence"][0]["path"], "tools/package.json");
        let both = package(
            &select(&base, &["root.js", "other.js", "tools/nested.js"]),
            "javascript",
            "shared",
        );
        assert_eq!(both["evidence"].as_array().unwrap().len(), 3);
        assert_eq!(
            package(
                &select(&base, &["root.js", "other.js"]),
                "javascript",
                "shared"
            ),
            root
        );
        let typed = package(&select(&base, &["typed.ts"]), "typescript", "shared");
        assert_ne!(typed["identity"], root["identity"]);
        let under = base
            .select_under(&[], Some("tools"), 5, &mut || Ok(()))
            .unwrap()
            .data;
        assert_eq!(package(&under, "javascript", "shared"), nested);
    }

    #[test]
    fn nearest_nonlabeling_manifest_never_falls_back() {
        let mut snapshot = fixture();
        snapshot.manifests[2] = manifest(&snapshot, "tools/package.json", "{}");
        let base = resolve_all(&snapshot, &mut || Ok(())).unwrap();
        assert!(
            select(&base, &["tools/nested.js"])["externalPackages"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            !select(&base, &["root.js"])["externalPackages"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        snapshot.manifests[2] = manifest(&snapshot, "tools/package.json", "invalid json");
        let base = resolve_all(&snapshot, &mut || Ok(())).unwrap();
        assert!(
            select(&base, &["tools/nested.js"])["externalPackages"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn semantic_override_keeps_accumulated_packages() {
        let mut snapshot = fixture();
        let document = snapshot
            .documents
            .iter_mut()
            .find(|doc| doc.key.path == "src/lib.rs")
            .unwrap();
        document.semantic.producer_id = Some("producer".into());
        document.semantic.anchors = document
            .calls
            .iter()
            .map(|call| SemanticAnchor {
                range: call.callee_range.clone().unwrap_or(call.range.clone()),
                call_id: Some(call.id.clone()),
                type_role: false,
                direct: true,
                outcome: SemanticOutcome::Boundary,
                basis_site: true,
                provenance_id: "boundary".into(),
                metadata: SemanticMetadata::default(),
            })
            .collect();
        let base = resolve_all(&snapshot, &mut || Ok(())).unwrap();
        let data = select(&base, &["src/lib.rs"]);
        assert_eq!(
            package(&data, "rust", "serde")["evidence"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(
            data["production"]
                .as_array()
                .unwrap()
                .iter()
                .all(|edge| edge["target"]["package"] != "serde")
        );
        let source = slot(&base, "src/lib.rs");
        assert!(
            base.sources[source]
                .as_ref()
                .unwrap()
                .packages
                .iter()
                .any(|identity| identity.package == "serde")
        );
    }

    #[test]
    fn current_descriptor_size_charges_and_older_tables() {
        let mut snapshot = fixture();
        let before = resolve_all(&snapshot, &mut || Ok(())).unwrap();
        let frozen = before.canonical().unwrap();
        for manifest in &mut snapshot.manifests {
            manifest.native_revision_id.push_str("-new-native");
            manifest.dependency_hash.push_str("-new-dependencies");
        }
        let mut after = resolve_all(&snapshot, &mut || Ok(())).unwrap();
        let extra = "-new-native".len() + "-new-dependencies".len();
        for path in ["root.js", "other.js", "tools/nested.js", "typed.ts"] {
            let index = slot(&before, path);
            let evidence = before.external_packages.by_source[index]
                .values()
                .filter(|package| !package.identity.standard_library)
                .map(|package| package.evidence.len())
                .sum::<usize>();
            assert_eq!(
                after.source_facts()[index].work - before.source_facts()[index].work,
                evidence * extra,
                "{path} first-copy charges"
            );
            after.sources[index] = before.sources[index].clone();
        }
        assert!(after.work_used > before.work_used);
        after.external_packages.validate(&after.sources).unwrap();
        let current = package(&select(&after, &["root.js"]), "javascript", "shared");
        assert_eq!(
            current["evidence"][0]["nativeRevisionId"],
            "native-new-native"
        );
        assert_eq!(
            current["evidence"][0]["dependencyHash"],
            "dependencies-new-dependencies"
        );
        assert_eq!(
            before.canonical().unwrap(),
            frozen,
            "older table is immutable"
        );
        assert_ne!(
            before.external_packages.canonical(),
            after.external_packages.canonical()
        );
        assert_ne!(
            before.canonical().unwrap(),
            after.canonical().unwrap(),
            "canonical reads the current source descriptor table even with shared sources"
        );
    }

    #[test]
    fn all_four_standard_library_catalogs_are_stable_and_manifest_free() {
        let mut snapshot = fixture();
        let before = resolve_all(&snapshot, &mut || Ok(())).unwrap();
        for manifest in &mut snapshot.manifests {
            manifest.native_revision_id = "different".into();
            manifest.dependency_hash = "different".into();
        }
        let after = resolve_all(&snapshot, &mut || Ok(())).unwrap();
        for (language, name, catalog) in [
            ("rust", "std", "rust-standard-library:v1"),
            ("javascript", "fs", "node-standard-library:v2"),
            ("java", "java", "java-standard-library:v2"),
            ("python", "sys", "python-standard-library:v2"),
        ] {
            let package = package(&select(&before, &[]), language, name);
            assert_eq!(package, self::package(&select(&after, &[]), language, name));
            assert_eq!(
                package["evidence"],
                json!([{"kind":"standardLibraryCatalog","catalog":catalog,"entry":name}])
            );
        }
        let source_set = &snapshot.documents[0].key.source_set_id;
        assert!(
            external::node_standard_library(source_set, "fs", "typescript")
                .unwrap()
                .identity
                .standard_library
        );
    }
}
