//! Explicit management of the reviewed local search-model snapshot.
//!
//! Only `install` can access the network. These operations never select a model,
//! write configuration, contact a daemon, or restart it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::search_config::snapshot_path;
use crate::search_embed::{supported_model_files, supported_model_name, supported_model_revision};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallReport {
    pub name: String,
    pub revision: String,
    pub directory: PathBuf,
    pub verified: bool,
    pub already_installed: bool,
    pub restart_required: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledModel {
    pub name: String,
    pub revision: String,
    pub directory: PathBuf,
    /// Required-file presence for the reviewed revision; unknown for old revisions.
    pub files_present: Option<bool>,
    /// Listing does not hash files. Runtime verification remains authoritative.
    pub verified: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveReport {
    pub name: String,
    pub revision: String,
    pub directory: PathBuf,
    pub removed: bool,
    pub restart_required: bool,
}

/// Download only the currently reviewed immutable snapshot, verifying every file
/// before publication. Existing snapshots are fully verified without a download.
/// An invalid existing snapshot must be explicitly removed before reinstalling.
pub async fn install(models_dir: &Path, name: &str, revision: &str) -> Result<InstallReport> {
    validate_install_selection(name, revision)?;
    install_with(
        models_dir,
        name,
        revision,
        supported_model_files(),
        &HttpSource {
            client: reqwest::Client::new(),
            name,
            revision,
        },
    )
    .await
}

/// List managed revisions in the reviewed model's namespace, without writes,
/// network access, or weight-file hashing. Old revisions remain removable, not
/// loadable or installable merely because they are listed here.
pub fn list(models_dir: &Path) -> Result<Vec<InstalledModel>> {
    list_with(
        models_dir,
        supported_model_name(),
        supported_model_revision(),
        supported_model_files(),
    )
}

/// Remove a managed revision, including the selected/default one. Configuration
/// is unchanged; a future daemon startup can therefore find the model missing.
pub fn remove(models_dir: &Path, name: &str, revision: &str) -> Result<RemoveReport> {
    validate_managed_selection(name, revision)?;
    let directory = snapshot_path(models_dir, name, revision);
    let mut report = RemoveReport {
        name: name.to_owned(),
        revision: revision.to_owned(),
        directory,
        removed: false,
        restart_required: true,
    };
    if !models_dir.try_exists()? {
        return Ok(report);
    }
    let _lock = management_lock(models_dir)?;
    // Rename first so readers never observe a partially removed revision.
    if report.directory.try_exists()? {
        if !fs::metadata(&report.directory)?.is_dir() {
            bail!("managed model snapshot is not a directory");
        }
        let parent = report.directory.parent().expect("managed revision parent");
        let trash = tempfile::Builder::new()
            .prefix(".remove-")
            .tempdir_in(parent)?;
        let removed = trash.path().join("snapshot");
        fs::rename(&report.directory, &removed).context("hide managed model for removal")?;
        File::open(parent)?.sync_all()?;
        fs::remove_dir_all(removed).context("delete removed model files")?;
        report.removed = true;
    }
    Ok(report)
}

fn validate_install_selection(name: &str, revision: &str) -> Result<()> {
    validate_managed_selection(name, revision)?;
    if revision != supported_model_revision() {
        bail!(
            "only the reviewed model revision {} can be installed",
            supported_model_revision()
        );
    }
    Ok(())
}

fn validate_managed_selection(name: &str, revision: &str) -> Result<()> {
    if name != supported_model_name() {
        bail!("unsupported model name: {name}");
    }
    if !is_revision(revision) {
        bail!("model revision must be 40 lowercase hexadecimal characters");
    }
    Ok(())
}

fn is_revision(revision: &str) -> bool {
    revision.len() == 40
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// A previous creator may have crashed before syncing an already-existing
// ancestor, so anchor the complete chain on every explicit mutation.
fn managed_create_dir_all_durable(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory)?;
    for ancestor in directory
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
    {
        File::open(ancestor)?
            .sync_all()
            .context("sync managed model directory ancestry")?;
    }
    if directory.is_relative() {
        File::open(".")?.sync_all()?;
    }
    Ok(())
}

fn management_lock(models_dir: &Path) -> Result<File> {
    managed_create_dir_all_durable(models_dir).context("create models directory")?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(models_dir.join(".management.lock"))
        .context("open model management lock")?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "model management is busy; retry after the other install/remove finishes",
        ))
        .context("lock model management"),
        Err(std::fs::TryLockError::Error(error)) => Err(error).context("lock model management"),
    }
}

trait FileSource {
    async fn write_file(&self, relative: &str, destination: &mut HashingWriter<File>)
    -> Result<()>;
}

struct HttpSource<'a> {
    client: reqwest::Client,
    name: &'a str,
    revision: &'a str,
}

impl FileSource for HttpSource<'_> {
    async fn write_file(
        &self,
        relative: &str,
        destination: &mut HashingWriter<File>,
    ) -> Result<()> {
        let url = format!(
            "https://huggingface.co/{}/resolve/{}/{relative}",
            self.name, self.revision
        );
        let mut response = self.client.get(url).send().await?.error_for_status()?;
        while let Some(chunk) = response.chunk().await? {
            destination.write_all(&chunk)?;
        }
        Ok(())
    }
}

struct HashingWriter<W> {
    output: W,
    digest: Sha256,
}

impl<W> HashingWriter<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            digest: Sha256::new(),
        }
    }

    fn hash(self) -> String {
        hex::encode(self.digest.finalize())
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.output.write(bytes)?;
        self.digest.update(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

async fn install_with(
    models_dir: &Path,
    name: &str,
    revision: &str,
    files: &[(&str, &str)],
    source: &impl FileSource,
) -> Result<InstallReport> {
    let _lock = management_lock(models_dir)?;
    let directory = snapshot_path(models_dir, name, revision);
    let mut report = InstallReport {
        name: name.to_owned(),
        revision: revision.to_owned(),
        directory,
        verified: true,
        already_installed: false,
        restart_required: true,
    };
    if report.directory.try_exists()? {
        verify_snapshot(&report.directory, files).context(
            "model files are incomplete or invalid; remove this revision, then install again",
        )?;
        report.already_installed = true;
        return Ok(report);
    }
    let parent = report.directory.parent().expect("managed revision parent");
    managed_create_dir_all_durable(parent).context("create model namespace")?;
    let stage = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(parent)?;
    for &(relative, expected_hash) in files {
        let path = stage.path().join(relative);
        fs::create_dir_all(path.parent().expect("reviewed file parent"))?;
        let mut destination = HashingWriter::new(File::create(&path)?);
        source
            .write_file(relative, &mut destination)
            .await
            .with_context(|| format!("download model file {relative}"))?;
        destination
            .output
            .sync_all()
            .with_context(|| format!("sync model file {relative}"))?;
        if destination.hash() != expected_hash {
            bail!("SHA-256 mismatch for model file {relative}");
        }
    }
    sync_directories(stage.path())?;
    // The management lock keeps the target absent until this single publication.
    fs::rename(stage.path(), &report.directory).context("publish verified model snapshot")?;
    File::open(parent)?
        .sync_all()
        .context("sync published model directory")?;
    Ok(report)
}

fn verify_snapshot(directory: &Path, files: &[(&str, &str)]) -> Result<()> {
    for &(relative, expected_hash) in files {
        let mut file = File::open(directory.join(relative))
            .with_context(|| format!("read model file {relative}"))?;
        let mut hash = HashingWriter::new(io::sink());
        io::copy(&mut file, &mut hash).with_context(|| format!("hash model file {relative}"))?;
        if hash.hash() != expected_hash {
            bail!("SHA-256 mismatch for model file {relative}");
        }
    }
    Ok(())
}

fn sync_directories(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_directories(&entry.path())?;
        }
    }
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn list_with(
    models_dir: &Path,
    name: &str,
    reviewed_revision: &str,
    files: &[(&str, &str)],
) -> Result<Vec<InstalledModel>> {
    let namespace = models_dir.join(name);
    let entries = match fs::read_dir(&namespace) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("list managed models"),
    };
    let mut snapshots = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(revision) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_revision(&revision) || !entry.file_type()?.is_dir() {
            continue;
        }
        let files_present = if revision == reviewed_revision {
            let mut present = true;
            for &(relative, _) in files {
                match fs::metadata(entry.path().join(relative)) {
                    Ok(metadata) => present &= metadata.is_file(),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => present = false,
                    Err(error) => return Err(error).context("inspect managed model files"),
                }
            }
            Some(present)
        } else {
            None
        };
        snapshots.push(InstalledModel {
            name: name.to_owned(),
            revision,
            directory: entry.path(),
            files_present,
            verified: false,
        });
    }
    snapshots.sort_by(|left, right| left.revision.cmp(&right.revision));
    Ok(snapshots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::future::Future;
    use std::task::{Context as TaskContext, Poll, Waker};

    const CONTENTS: &[(&str, &[u8])] = &[
        ("config.json", b"fixture config"),
        ("nested/tokenizer.json", b"fixture tokenizer"),
        ("weights.bin", b"fixture weights"),
    ];

    struct FixtureSource {
        calls: RefCell<Vec<String>>,
        wrong_file: Option<&'static str>,
        missing_file: Option<&'static str>,
    }

    impl FixtureSource {
        fn good() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                wrong_file: None,
                missing_file: None,
            }
        }
    }

    impl FileSource for FixtureSource {
        async fn write_file(
            &self,
            relative: &str,
            destination: &mut HashingWriter<File>,
        ) -> Result<()> {
            self.calls.borrow_mut().push(relative.to_owned());
            if self.missing_file == Some(relative) {
                bail!("fixture source missing file");
            }
            let bytes = if self.wrong_file == Some(relative) {
                b"wrong bytes".as_slice()
            } else {
                CONTENTS
                    .iter()
                    .find(|(path, _)| *path == relative)
                    .unwrap()
                    .1
            };
            // Exercise hashing across writes rather than just a whole-file digest.
            for chunk in bytes.chunks(3) {
                destination.write_all(chunk)?;
            }
            Ok(())
        }
    }

    fn hashes() -> Vec<(&'static str, String)> {
        CONTENTS
            .iter()
            .map(|(path, bytes)| (*path, hex::encode(Sha256::digest(bytes))))
            .collect()
    }

    fn descriptor<'a>(hashes: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
        hashes
            .iter()
            .map(|(path, hash)| (*path, hash.as_str()))
            .collect()
    }

    #[test]
    fn durable_directory_creation_handles_nested_and_existing_paths() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("new/data/models/provider/model");
        managed_create_dir_all_durable(&directory).unwrap();
        assert!(directory.is_dir());
        managed_create_dir_all_durable(&directory).unwrap();
        let file = temp.path().join("not-a-directory");
        fs::write(&file, b"existing file").unwrap();
        assert!(managed_create_dir_all_durable(&file.join("models")).is_err());
    }

    #[tokio::test]
    async fn install_verifies_all_files_and_existing_snapshot_without_fetching() {
        let temp = tempfile::tempdir().unwrap();
        let models = temp.path().join("models");
        let name = supported_model_name();
        let revision = supported_model_revision();
        let hashes = hashes();
        let files = descriptor(&hashes);
        let source = FixtureSource::good();
        assert!(list(&models).unwrap().is_empty());
        assert!(!models.exists());
        let report = install_with(&models, name, revision, &files, &source)
            .await
            .unwrap();
        assert!(report.verified && report.restart_required && !report.already_installed);
        assert_eq!(
            *source.calls.borrow(),
            CONTENTS
                .iter()
                .map(|(path, _)| path.to_string())
                .collect::<Vec<_>>()
        );
        for (relative, bytes) in CONTENTS {
            assert_eq!(fs::read(report.directory.join(relative)).unwrap(), *bytes);
        }
        source.calls.borrow_mut().clear();
        assert!(
            install_with(&models, name, revision, &files, &source)
                .await
                .unwrap()
                .already_installed
        );
        assert!(source.calls.borrow().is_empty());
        fs::write(report.directory.join("weights.bin"), "modified").unwrap();
        let listed = list_with(&models, name, revision, &files).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].files_present, Some(true));
        assert!(!listed[0].verified);
        let error = install_with(&models, name, revision, &files, &source)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("remove this revision, then install again")
        );
        assert!(source.calls.borrow().is_empty());
        assert_eq!(
            fs::read(report.directory.join("weights.bin")).unwrap(),
            b"modified"
        );
    }

    #[tokio::test]
    async fn every_hash_and_fetch_failure_leaves_no_published_or_staged_snapshot() {
        for &(relative, _) in CONTENTS {
            for missing in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let hashes = hashes();
                let files = descriptor(&hashes);
                let mut source = FixtureSource::good();
                if missing {
                    source.missing_file = Some(relative);
                } else {
                    source.wrong_file = Some(relative);
                }
                assert!(
                    install_with(
                        temp.path(),
                        supported_model_name(),
                        supported_model_revision(),
                        &files,
                        &source
                    )
                    .await
                    .is_err()
                );
                assert!(list(temp.path()).unwrap().is_empty());
                let namespace = temp.path().join(supported_model_name());
                assert_eq!(fs::read_dir(namespace).unwrap().count(), 0);
            }
        }
    }

    struct PendingSource;

    impl FileSource for PendingSource {
        async fn write_file(&self, _: &str, destination: &mut HashingWriter<File>) -> Result<()> {
            destination.write_all(b"partial")?;
            std::future::pending().await
        }
    }

    #[test]
    fn cancellation_cleans_stage_and_mutators_are_serialized() {
        let temp = tempfile::tempdir().unwrap();
        let hashes = hashes();
        let files = descriptor(&hashes);
        let mut install = Box::pin(install_with(
            temp.path(),
            supported_model_name(),
            supported_model_revision(),
            &files,
            &PendingSource,
        ));
        let mut context = TaskContext::from_waker(Waker::noop());
        assert!(matches!(install.as_mut().poll(&mut context), Poll::Pending));
        assert!(list(temp.path()).unwrap().is_empty());
        let error = remove(
            temp.path(),
            supported_model_name(),
            supported_model_revision(),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(install);
        let namespace = temp.path().join(supported_model_name());
        assert_eq!(fs::read_dir(namespace).unwrap().count(), 0);
        let lock = management_lock(temp.path()).unwrap();
        drop(lock);
    }

    #[test]
    fn list_and_remove_old_revisions_ignore_unmanaged_and_hidden_directories() {
        let temp = tempfile::tempdir().unwrap();
        let models = temp.path().join("models");
        let name = supported_model_name();
        let current = supported_model_revision();
        let old = "1111111111111111111111111111111111111111";
        for revision in [current, old, ".install-abandoned", "not-a-revision"] {
            fs::create_dir_all(snapshot_path(&models, name, revision)).unwrap();
        }
        let unrelated = models.join("other/model").join(old);
        fs::create_dir_all(&unrelated).unwrap();
        let listed = list(&models).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(
            listed
                .iter()
                .find(|model| model.revision == old)
                .unwrap()
                .files_present,
            None
        );
        assert_eq!(
            listed
                .iter()
                .find(|model| model.revision == current)
                .unwrap()
                .files_present,
            Some(false)
        );
        assert!(listed.iter().all(|model| !model.verified));
        assert!(remove(&models, name, "../escape").is_err());
        assert!(remove(&models, "other/model", old).is_err());
        assert!(validate_install_selection(name, old).is_err());
        for revision in [old, current] {
            let report = remove(&models, name, revision).unwrap();
            assert!(report.removed && report.restart_required);
            assert!(!snapshot_path(&models, name, revision).exists());
            assert!(!remove(&models, name, revision).unwrap().removed);
        }
        assert!(unrelated.exists());
        assert!(snapshot_path(&models, name, ".install-abandoned").exists());
    }
}
