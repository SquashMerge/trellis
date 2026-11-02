//! Startup-only user configuration for offline search. Resolution never downloads
//! files or writes defaults; managed snapshots use the one reviewed model pin.
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::{
    search_embed::{supported_model_files, supported_model_name, supported_model_revision},
    search_model::SearchConfig,
    store::topology::TopologyRoots,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    Default,
    ConfigFile,
    Environment,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchConfigSources {
    pub models_dir: ConfigSource,
    pub model: ConfigSource,
    pub backend: ConfigSource,
}

/// An immutable startup snapshot, not a claim of verified model or vector readiness.
/// `model_path` records where the daemon looked, including missing managed files.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchConfiguration {
    pub config_file: PathBuf,
    pub models_dir: PathBuf,
    pub model_name: String,
    pub model_revision: String,
    pub model_path: Option<PathBuf>,
    pub backend: String,
    pub sources: SearchConfigSources,
}

#[derive(Clone, Debug)]
pub struct ResolvedSearchConfig {
    pub diagnostics: SearchConfiguration,
    pub config: SearchConfig,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserConfig {
    models_dir: Option<PathBuf>,
    model: Option<ModelSelection>,
    backend: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelSelection {
    name: String,
    revision: String,
}

/// Layout is shared by resolution and explicit model management. Callers must
/// validate name/revision against the reviewed catalog before using this path.
pub fn snapshot_path(models_dir: &Path, name: &str, revision: &str) -> PathBuf {
    models_dir.join(name).join(revision)
}

fn config_path(home: &Path) -> PathBuf {
    home.join(".config/trellis/config.toml")
}

/// Resolve with the daemon's existing roots. Only environment reads happen here;
/// tests use `from_inputs` without changing process-global state.
pub fn resolve(roots: &TopologyRoots) -> Result<ResolvedSearchConfig> {
    let base = BaseDirs::new().context("home directory unavailable")?;
    let backend = match std::env::var("TRELLIS_SEARCH_BACKEND") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("TRELLIS_SEARCH_BACKEND must be UTF-8")
        }
    };
    ResolvedSearchConfig::from_inputs(
        config_path(base.home_dir()),
        roots.data.join("models"),
        std::env::var_os("TRELLIS_SEARCH_MODEL"),
        backend,
    )
}

impl ResolvedSearchConfig {
    pub fn from_environment() -> Result<Self> {
        resolve(&TopologyRoots::production()?)
    }

    /// Parse optional overrides from a trusted user file. Missing files select
    /// defaults; all other read/parse/validation failures are startup errors.
    pub fn from_inputs(
        config_file: PathBuf,
        default_models_dir: PathBuf,
        env_model: Option<OsString>,
        env_backend: Option<String>,
    ) -> Result<Self> {
        let user: UserConfig = match std::fs::read_to_string(&config_file) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("parse search configuration {}", config_file.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => UserConfig::default(),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("read search configuration {}", config_file.display())
                });
            }
        };
        if let Some(model) = &user.model {
            ensure!(
                model.name == supported_model_name()
                    && model.revision == supported_model_revision(),
                "unsupported search model {:?}@{:?}; supported model is {}@{}",
                model.name,
                model.revision,
                supported_model_name(),
                supported_model_revision()
            );
        }
        // Reject invalid file values even when a development override is set.
        SearchConfig::from_values(None, user.backend.as_deref())?;
        let mut sources = SearchConfigSources {
            models_dir: ConfigSource::Default,
            model: ConfigSource::Default,
            backend: ConfigSource::Default,
        };
        let models_dir = match user.models_dir {
            Some(path) => {
                ensure!(path.is_absolute(), "models_dir must be an absolute path");
                sources.models_dir = ConfigSource::ConfigFile;
                path
            }
            None => default_models_dir,
        };
        if user.model.is_some() {
            sources.model = ConfigSource::ConfigFile;
        }
        let backend = if let Some(value) = env_backend {
            sources.backend = ConfigSource::Environment;
            value
        } else if let Some(value) = user.backend {
            sources.backend = ConfigSource::ConfigFile;
            value
        } else {
            "auto".to_owned()
        };
        let managed_path = snapshot_path(
            &models_dir,
            supported_model_name(),
            supported_model_revision(),
        );
        let (model_path, runtime_path) = if let Some(value) = env_model {
            sources.model = ConfigSource::Environment;
            let path = (!value.is_empty()).then(|| PathBuf::from(value));
            (path.clone(), path)
        } else {
            // Presence is not verification. The existing loader still checks every
            // pinned hash and owns the verified bytes before constructing a model.
            let mut present = true;
            for file in supported_model_files() {
                let name = file.path;
                match std::fs::metadata(managed_path.join(name)) {
                    Ok(metadata) => present &= metadata.is_file(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => present = false,
                    // A broken or unreadable model is not a malformed user
                    // configuration. Let the loader report model_load_failed and
                    // keep lexical retrieval available.
                    Err(_) => {}
                }
            }
            (Some(managed_path.clone()), present.then_some(managed_path))
        };
        let config = SearchConfig::from_values(runtime_path, Some(&backend))?;
        Ok(Self {
            diagnostics: SearchConfiguration {
                config_file,
                models_dir,
                model_name: supported_model_name().to_owned(),
                model_revision: supported_model_revision().to_owned(),
                model_path,
                backend,
                sources,
            },
            config,
        })
    }

    pub fn runtime_config(&self) -> Result<SearchConfig> {
        Ok(self.config.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: tempfile::TempDir,
        config: PathBuf,
        models: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            Self {
                config: config_path(root.path()),
                models: root.path().join("data/models"),
                root,
            }
        }
        fn write(&self, text: &str) {
            std::fs::create_dir_all(self.config.parent().unwrap()).unwrap();
            std::fs::write(&self.config, text).unwrap();
        }
        fn resolve(
            &self,
            model: Option<OsString>,
            backend: Option<&str>,
        ) -> Result<ResolvedSearchConfig> {
            ResolvedSearchConfig::from_inputs(
                self.config.clone(),
                self.models.clone(),
                model,
                backend.map(str::to_owned),
            )
        }
        fn snapshot(&self) -> PathBuf {
            snapshot_path(
                &self.models,
                supported_model_name(),
                supported_model_revision(),
            )
        }
    }

    #[test]
    fn missing_config_and_model_use_defaults_without_writes() {
        let fixture = Fixture::new();
        let resolved = fixture.resolve(None, None).unwrap();
        assert_eq!(
            resolved.diagnostics.config_file,
            fixture.root.path().join(".config/trellis/config.toml")
        );
        assert_eq!(resolved.diagnostics.models_dir, fixture.models);
        assert_eq!(
            resolved.diagnostics.model_path.as_deref(),
            Some(fixture.snapshot().as_path())
        );
        assert_eq!(resolved.diagnostics.backend, "auto");
        assert_eq!(resolved.diagnostics.sources.model, ConfigSource::Default);
        assert_eq!(resolved.runtime_config().unwrap().model_path(), None);
        assert!(!fixture.config.exists());
        assert!(!fixture.models.exists());
    }

    #[test]
    fn empty_config_is_equivalent_to_missing_config() {
        let fixture = Fixture::new();
        let before = fixture.resolve(None, None).unwrap();
        fixture.write("");
        let after = fixture.resolve(None, None).unwrap();
        assert_eq!(before.diagnostics, after.diagnostics);
        assert_eq!(
            before.runtime_config().unwrap(),
            after.runtime_config().unwrap()
        );
    }

    #[test]
    fn optional_overrides_select_reviewed_snapshot_and_sources() {
        let fixture = Fixture::new();
        let models = fixture.root.path().join("custom-models");
        fixture.write(&format!(
            "models_dir = {:?}\nbackend = \"cpu\"\n[model]\nname = {:?}\nrevision = {:?}\n",
            models.to_str().unwrap(),
            supported_model_name(),
            supported_model_revision(),
        ));
        let resolved = fixture.resolve(None, None).unwrap();
        assert_eq!(resolved.diagnostics.models_dir, models);
        assert_eq!(resolved.diagnostics.backend, "cpu");
        assert_eq!(
            resolved.diagnostics.sources,
            SearchConfigSources {
                models_dir: ConfigSource::ConfigFile,
                model: ConfigSource::ConfigFile,
                backend: ConfigSource::ConfigFile,
            }
        );
        assert!(!models.exists());
    }

    #[test]
    fn environment_overrides_are_field_wise_and_empty_path_disables() {
        let fixture = Fixture::new();
        fixture.write("backend = \"cpu\"\n");
        let path = fixture.root.path().join("development-model");
        let resolved = fixture
            .resolve(Some(path.clone().into_os_string()), Some("auto"))
            .unwrap();
        assert_eq!(
            resolved.runtime_config().unwrap().model_path(),
            Some(path.as_path())
        );
        assert_eq!(
            resolved.diagnostics.model_path.as_deref(),
            Some(path.as_path())
        );
        assert_eq!(resolved.diagnostics.models_dir, fixture.models);
        assert_eq!(
            resolved.diagnostics.sources.model,
            ConfigSource::Environment
        );
        assert_eq!(
            resolved.diagnostics.sources.backend,
            ConfigSource::Environment
        );
        assert_eq!(
            resolved.diagnostics.sources.models_dir,
            ConfigSource::Default
        );
        let disabled = fixture.resolve(Some(OsString::new()), None).unwrap();
        assert_eq!(disabled.diagnostics.model_path, None);
        assert_eq!(disabled.runtime_config().unwrap().model_path(), None);
        assert_eq!(disabled.diagnostics.backend, "cpu");
    }

    #[test]
    fn managed_presence_requires_every_file_but_does_not_claim_hash_verification() {
        let fixture = Fixture::new();
        for file in supported_model_files() {
            let name = file.path;
            let path = fixture.snapshot().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "not a verified model").unwrap();
        }
        let resolved = fixture.resolve(None, None).unwrap();
        assert_eq!(
            resolved.runtime_config().unwrap().model_path(),
            Some(fixture.snapshot().as_path())
        );
        std::fs::remove_file(fixture.snapshot().join(supported_model_files()[0].path)).unwrap();
        assert_eq!(
            fixture
                .resolve(None, None)
                .unwrap()
                .runtime_config()
                .unwrap()
                .model_path(),
            None
        );
        // Resolution is a startup snapshot, not a live config or presence read.
        assert_eq!(
            resolved.runtime_config().unwrap().model_path(),
            Some(fixture.snapshot().as_path())
        );
    }

    #[test]
    fn malformed_managed_tree_is_left_to_loader_without_refusing_startup() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.models.parent().unwrap()).unwrap();
        std::fs::write(&fixture.models, "not a directory").unwrap();
        let resolved = fixture.resolve(None, Some("cpu")).unwrap();
        assert_eq!(
            resolved.runtime_config().unwrap().model_path(),
            Some(fixture.snapshot().as_path())
        );
        let mut model =
            crate::search_model::SearchModel::new(resolved.runtime_config().unwrap()).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let failed = model.with_job(&crate::search_embed::EmbeddingControl::new(&cancel), |_| {
            Ok(())
        });
        assert!(failed.is_err());
        assert_eq!(
            model.status_handle().availability().unavailable_reason,
            Some(crate::search_model::SearchUnavailableReason::ModelLoadFailed)
        );
    }

    #[test]
    fn invalid_config_never_silently_falls_back() {
        let fixture = Fixture::new();
        for text in [
            "not toml",
            "models_dir = \"relative\"",
            "models_dir = \"~/models\"",
            "backend = \"gpu\"",
            "unknown = true",
            "[model]\nname = \"voyageai/voyage-4-nano\"",
            "[model]\nname = \"../other\"\nrevision = \"unreviewed\"",
            "[model]\nname = \"voyageai/voyage-4-nano\"\nrevision = \"unreviewed\"",
        ] {
            fixture.write(text);
            assert!(
                fixture.resolve(Some(OsString::new()), Some("cpu")).is_err(),
                "{text}"
            );
        }
        fixture.write("");
        assert!(fixture.resolve(None, Some("invalid")).is_err());
    }

    #[test]
    fn config_read_errors_are_not_missing_configuration() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(&fixture.config).unwrap();
        assert!(fixture.resolve(None, None).is_err());
        std::fs::remove_dir(&fixture.config).unwrap();
        std::fs::write(&fixture.config, [0xff]).unwrap();
        assert!(fixture.resolve(None, None).is_err());
    }

    #[test]
    fn explicit_data_roots_and_serialized_provenance_are_preserved() {
        let fixture = Fixture::new();
        for data in [
            "Library/Application Support/dev.squashmerge.trellis/models",
            ".local/share/trellis/models",
            "xdg-data/trellis/models",
        ] {
            let resolved = ResolvedSearchConfig::from_inputs(
                fixture.config.clone(),
                fixture.root.path().join(data),
                None,
                None,
            )
            .unwrap();
            assert_eq!(
                resolved.diagnostics.models_dir,
                fixture.root.path().join(data)
            );
            let json = serde_json::to_value(&resolved.diagnostics).unwrap();
            assert_eq!(json["sources"]["modelsDir"], "default");
            assert_eq!(json["modelName"], supported_model_name());
            assert_eq!(json["modelRevision"], supported_model_revision());
        }
    }
}
