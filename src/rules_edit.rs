//! Browser-only edits of the fixed checkout rules file.
use crate::{model::IndexPin, rules};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, io::Write, path::Path};
use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, value};

const COLLECTION_LIMIT: usize = 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Endpoint {
    pub language: String,
    pub path: String,
}
impl Endpoint {
    fn valid(&self) -> bool {
        matches!(
            self.language.as_str(),
            "java" | "rust" | "python" | "javascript"
        ) && !self.path.is_empty()
            && crate::file_tree::valid_path(&self.path)
            && !self.path.contains(['*', '?'])
    }
    fn toml(&self) -> toml_edit::Value {
        let mut table = InlineTable::new();
        table.insert("language", self.language.clone().into());
        table.insert("path", self.path.clone().into());
        table.into()
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pair {
    pub source: Endpoint,
    pub destination: Endpoint,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum Operation {
    AddExceptions { pairs: Vec<Pair>, reason: String },
    EditException { index: usize, reason: String },
    RemoveException { index: usize },
    AssignFile { path: String, layer: String },
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    pub index_generation: String,
    pub index_revision: u64,
    pub base_hash: String,
    pub operations: Vec<Operation>,
    pub proposed_hash: Option<String>,
}
impl Request {
    pub fn pin(&self) -> Result<IndexPin> {
        serde_json::from_value(
            json!({"indexGeneration":self.index_generation,"indexRevision":self.index_revision}),
        )
        .context("invalid_request: complete code pin required")
    }
    pub fn validate(&self, apply: bool) -> Result<()> {
        self.pin()?;
        ensure!(valid_hash(&self.base_hash), "invalid_request: baseHash");
        ensure!(
            if apply {
                self.proposed_hash.as_deref().is_some_and(valid_hash)
            } else {
                self.proposed_hash.is_none()
            },
            "invalid_request: proposedHash is required only for apply"
        );
        ensure!(
            !self.operations.is_empty() && self.operations.len() <= COLLECTION_LIMIT,
            "invalid_request: operation count"
        );
        let mut pairs = 0usize;
        let mut indices = BTreeSet::new();
        for operation in &self.operations {
            match operation {
                Operation::AddExceptions {
                    pairs: selected,
                    reason,
                } => {
                    pairs += selected.len();
                    ensure!(
                        !selected.is_empty()
                            && pairs <= COLLECTION_LIMIT
                            && !reason.trim().is_empty(),
                        "invalid_request: pairs and nonblank reason required"
                    );
                    ensure!(
                        selected
                            .iter()
                            .all(|p| p.source.valid() && p.destination.valid()),
                        "invalid_request: literal language/path pairs required"
                    );
                }
                Operation::EditException { index, reason } => {
                    ensure!(
                        !reason.trim().is_empty() && indices.insert(*index),
                        "invalid_request: nonblank reason and distinct original entry indices required"
                    );
                }
                Operation::RemoveException { index } => {
                    ensure!(
                        indices.insert(*index),
                        "invalid_request: distinct original entry indices required"
                    );
                }
                Operation::AssignFile { path, layer } => {
                    ensure!(
                        !path.is_empty()
                            && crate::file_tree::valid_path(path)
                            && !path.contains(['*', '?'])
                            && !layer.trim().is_empty(),
                        "invalid_request: literal path and existing layer required"
                    );
                }
            }
        }
        Ok(())
    }
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}
pub(crate) struct Base {
    pub config: rules::Config,
    device: u64,
    inode: u64,
}
pub(crate) fn load_base(root: &Path, expected: &str) -> Result<Base> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(root.join(rules::RULES_PATH))
        .context("rules_conflict: base missing")?;
    ensure!(metadata.is_file(), "rules_conflict: base not regular");
    let text = crate::file_tree::SourceDir::open(root)?
        .read_file(rules::RULES_PATH)
        .map_err(|_| anyhow::anyhow!("rules_conflict: base file missing or replaced"))?;
    ensure!(hash(&text) == expected, "rules_conflict: base file changed");
    let after = std::fs::symlink_metadata(root.join(rules::RULES_PATH))
        .context("rules_conflict: base missing")?;
    ensure!(
        (metadata.dev(), metadata.ino()) == (after.dev(), after.ino()),
        "rules_conflict: base replaced"
    );
    Ok(Base {
        config: rules::parse(&text)?,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}
pub(crate) fn inventory(config: &rules::Config, pin: IndexPin) -> Result<Value> {
    let raw: toml::Value = toml::from_str(&config.text)?;
    let exceptions = raw
        .get("exceptions")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, entry)| {
            let mut row = serde_json::to_value(entry)?;
            row["index"] = json!(index);
            Ok(row)
        })
        .collect::<Result<Vec<Value>>>()?;
    Ok(
        json!({"indexGeneration":pin.index_generation.to_string(),"indexRevision":pin.index_revision,"rulesFile":rules::descriptor(config),"exceptions":exceptions,"layers":rules::view_layers(config)}),
    )
}
pub(crate) struct Proposal {
    pub config: rules::Config,
    pub pairs: Vec<Pair>,
    pub diff: String,
}
pub(crate) fn propose(
    base: &rules::Config,
    request: &Request,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Proposal> {
    // Every inserted string survives in the resulting document. Count its
    // actual TOML representation before expanding a shared reason over pairs.
    // This is a necessary aggregate condition, not a new per-field limit.
    let mut inserted_bytes = 0usize;
    for operation in &request.operations {
        check()?;
        let mut charge = |text: &str, count: usize| -> Result<()> {
            let encoded = toml_edit::Value::from(text).to_string().len();
            inserted_bytes = inserted_bytes.saturating_add(encoded.saturating_mul(count));
            ensure!(
                inserted_bytes <= rules::CONFIG_BYTES,
                "invalid_request: expanded rules exceed 2 MiB"
            );
            Ok(())
        };
        match operation {
            Operation::AddExceptions { pairs, reason } => {
                charge(reason, pairs.len())?;
                for pair in pairs {
                    check()?;
                    charge(&pair.source.language, 1)?;
                    charge(&pair.source.path, 1)?;
                    charge(&pair.destination.language, 1)?;
                    charge(&pair.destination.path, 1)?;
                }
            }
            Operation::EditException { reason, .. } => charge(reason, 1)?,
            Operation::AssignFile { path, .. } => charge(path, 1)?,
            Operation::RemoveException { .. } => {}
        }
    }
    let mut document = base.text.parse::<DocumentMut>()?;
    let original_count = match document.get("exceptions") {
        None => 0,
        Some(Item::ArrayOfTables(entries)) => entries.len(),
        Some(item) if item.as_array().is_some() => item.as_array().expect("checked array").len(),
        _ => anyhow::bail!("invalid_request: unsupported exceptions representation"),
    };
    let mut removed = Vec::new();
    let mut pairs = Vec::new();
    for operation in &request.operations {
        check()?;
        match operation {
            Operation::AddExceptions {
                pairs: selected,
                reason,
            } => {
                if document.get("exceptions").is_none() {
                    document["exceptions"] = Item::ArrayOfTables(ArrayOfTables::new());
                }
                for pair in selected {
                    check()?;
                    let mut table = Table::new();
                    table["rule"] = value("$layers");
                    table["target"] = value(pair.source.toml());
                    table["destination"] = value(pair.destination.toml());
                    table["reason"] = value(reason);
                    match &mut document["exceptions"] {
                        Item::ArrayOfTables(entries) => entries.push(table),
                        item => item
                            .as_array_mut()
                            .context("invalid_request: exceptions must be tables")?
                            .push(table.into_inline_table()),
                    }
                    pairs.push(pair.clone());
                }
            }
            Operation::EditException { index, reason } => {
                ensure!(*index < original_count, "invalid_request: exception index");
                let item = &mut document["exceptions"];
                if let Some(entries) = item.as_array_of_tables_mut() {
                    let table = entries
                        .get_mut(*index)
                        .context("invalid_request: exception index")?;
                    let decor = table["reason"]
                        .as_value()
                        .context("invalid_request: exception reason")?
                        .decor()
                        .clone();
                    table["reason"] = value(reason);
                    *table["reason"]
                        .as_value_mut()
                        .expect("new reason value")
                        .decor_mut() = decor;
                } else {
                    let table = item
                        .as_array_mut()
                        .and_then(|a| a.get_mut(*index))
                        .and_then(toml_edit::Value::as_inline_table_mut)
                        .context("invalid_request: exception index")?;
                    let decor = table
                        .get("reason")
                        .context("invalid_request: exception reason")?
                        .decor()
                        .clone();
                    let mut replacement: toml_edit::Value = reason.clone().into();
                    *replacement.decor_mut() = decor;
                    table.insert("reason", replacement);
                }
            }
            Operation::RemoveException { index } => {
                ensure!(*index < original_count, "invalid_request: exception index");
                removed.push(*index);
            }
            Operation::AssignFile { path, layer } => {
                let table = document
                    .get_mut("layers")
                    .and_then(Item::as_table_like_mut)
                    .and_then(|layers| layers.get_mut(layer))
                    .and_then(Item::as_table_like_mut)
                    .context("invalid_request: existing layer required")?;
                if table.get("paths").is_none() {
                    table.insert("paths", value(toml_edit::Array::new()));
                }
                table
                    .get_mut("paths")
                    .and_then(Item::as_array_mut)
                    .context("invalid_request: layer paths")?
                    .push(path);
            }
        }
    }
    removed.sort_unstable_by(|a, b| b.cmp(a));
    for index in removed {
        let item = &mut document["exceptions"];
        if let Some(entries) = item.as_array_of_tables_mut() {
            entries.remove(index);
        } else {
            item.as_array_mut()
                .expect("original exception array")
                .remove(index);
        }
    }
    let text = document.to_string();
    let config = rules::parse(&text)?;
    let diff = diff(&base.text, &text, check)?;
    Ok(Proposal {
        config,
        pairs,
        diff,
    })
}
// A single changed-span hunk is linear in the bounded input, unlike an
// edit-distance diff. No search over repository-controlled line combinations.
fn diff(old: &str, new: &str, check: &mut impl FnMut() -> Result<()>) -> Result<String> {
    if old == new {
        return Ok(String::new());
    }
    let old_lines: Vec<_> = old.split_inclusive('\n').collect();
    let new_lines: Vec<_> = new.split_inclusive('\n').collect();
    let mut prefix = 0;
    while prefix < old_lines.len().min(new_lines.len()) && old_lines[prefix] == new_lines[prefix] {
        check()?;
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < (old_lines.len() - prefix).min(new_lines.len() - prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        check()?;
        suffix += 1;
    }
    let removed = old_lines.len() - prefix - suffix;
    let added = new_lines.len() - prefix - suffix;
    let old_start = if removed == 0 { prefix } else { prefix + 1 };
    let new_start = if added == 0 { prefix } else { prefix + 1 };
    let mut output = format!(
        "--- {}\n+++ {}\n@@ -{old_start},{removed} +{new_start},{added} @@\n",
        rules::RULES_PATH,
        rules::RULES_PATH
    );
    for (marker, lines) in [
        ('-', &old_lines[prefix..old_lines.len() - suffix]),
        ('+', &new_lines[prefix..new_lines.len() - suffix]),
    ] {
        for line in lines {
            check()?;
            output.push(marker);
            output.push_str(line);
            if !line.ends_with('\n') {
                output.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    Ok(output)
}
impl Proposal {
    pub fn preview(&self, pin: IndexPin, base_hash: &str) -> Value {
        json!({"indexGeneration":pin.index_generation.to_string(),"indexRevision":pin.index_revision,"baseHash":base_hash,"proposedHash":self.config.content_hash,"text":self.config.text,"diff":self.diff,"pairs":self.pairs})
    }
}
/// New exceptions must refer to positive production witnesses, never an unresolved
/// destination or a potential future member of a layer.
pub(crate) fn validate_selection(
    request: &Request,
    evaluated: &Value,
    assignments: &std::collections::BTreeMap<&str, Option<&str>>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut witnessed = BTreeSet::new();
    if let Some(rows) = evaluated["instances"].as_array() {
        for row in rows {
            check()?;
            if row["rule"] != "$layers" || row["status"] != "deviates" {
                continue;
            }
            if let (
                Some(source_language),
                Some(source_path),
                Some(destination_language),
                Some(destination_path),
            ) = (
                row["target"]["language"].as_str(),
                row["target"]["path"].as_str(),
                row["destination"]["language"].as_str(),
                row["destination"]["path"].as_str(),
            ) {
                witnessed.insert((
                    source_language,
                    source_path,
                    destination_language,
                    destination_path,
                ));
            }
        }
    }
    for operation in &request.operations {
        check()?;
        match operation {
            Operation::AddExceptions { pairs, .. } => {
                for pair in pairs {
                    check()?;
                    ensure!(
                        witnessed.contains(&(
                            pair.source.language.as_str(),
                            pair.source.path.as_str(),
                            pair.destination.language.as_str(),
                            pair.destination.path.as_str()
                        )),
                        "invalid_request: selected pair is not a positively witnessed production layer deviation"
                    );
                }
            }
            Operation::AssignFile { path, .. } => {
                ensure!(
                    assignments.get(path.as_str()) == Some(&None),
                    "invalid_request: assignment requires an indexed unassigned file"
                );
            }
            _ => {}
        }
    }
    Ok(())
}

/// All fallible response construction and selected-read validation happens before
/// this commit. After rename, only parent-directory durability remains uncertain.
pub(crate) fn commit(
    root: &Path,
    base: &Base,
    proposed: &rules::Config,
    mut before_rename: impl FnMut() -> Result<()>,
) -> Result<&'static str> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let path = root.join(rules::RULES_PATH);
    let parent = path.parent().expect("fixed rules parent");
    let original = std::fs::symlink_metadata(&path).context("rules_conflict: base missing")?;
    ensure!(
        original.is_file() && (original.dev(), original.ino()) == (base.device, base.inode),
        "rules_conflict: base replaced"
    );
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)?;
    let mut staging = tempfile::NamedTempFile::new_in(parent)?;
    staging.as_file().set_permissions(original.permissions())?;
    staging.write_all(proposed.text.as_bytes())?;
    staging.as_file().sync_all()?;
    before_rename()?;
    let current = std::fs::symlink_metadata(&path).context("rules_conflict: base missing")?;
    ensure!(
        current.is_file() && (current.dev(), current.ino()) == (original.dev(), original.ino()),
        "rules_conflict: base replaced"
    );
    let text = crate::file_tree::SourceDir::open(root)?
        .read_file(rules::RULES_PATH)
        .context("rules_conflict: base unreadable")?;
    ensure!(text == base.config.text, "rules_conflict: base changed");
    staging.persist(&path).map_err(|error| error.error)?;
    Ok(if directory.sync_all().is_ok() {
        "committed"
    } else {
        "durability_uncertain"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const CONFIG: &str = "# keep top\nversion = 1\n[layers.a]\norder = 1\npaths = ['src/a.rs'] # keep paths\n[layers.b]\norder = 2\npaths = ['src/b.rs']\n\n# exception attached\n[[exceptions]]\nrule = '$layers'\ntarget = {language='rust',path='src/a.rs'}\ndestination = {language='rust',path='src/b.rs'}\nreason = 'old' # keep reason\n";
    fn request(operations: Value) -> Request {
        serde_json::from_value(json!({"indexGeneration":"f64aac54-3da7-4a00-8c9b-4ec2efdeff32","indexRevision":1,"baseHash":hash(CONFIG),"operations":operations})).unwrap()
    }
    #[test]
    fn selected_reason_preserves_unrelated_bytes_and_attached_comments() {
        let config = rules::parse(CONFIG).unwrap();
        let request = request(json!([{"type":"editException","index":0,"reason":"new\nline"}]));
        request.validate(false).unwrap();
        let proposal = propose(&config, &request, &mut || Ok(())).unwrap();
        assert!(
            proposal
                .config
                .text
                .starts_with(CONFIG.split("reason =").next().unwrap())
        );
        assert!(proposal.config.text.contains("# keep reason"));
        assert!(proposal.config.text.contains("# exception attached"));
        assert_eq!(hash(&proposal.config.text), proposal.config.content_hash);
        assert!(proposal.diff.contains("-reason = 'old' # keep reason"));
    }
    #[test]
    fn removing_one_exception_and_assigning_preserves_other_text() {
        let config = rules::parse(CONFIG).unwrap();
        let removal = propose(
            &config,
            &request(json!([{"type":"removeException","index":0}])),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(
            removal.config.text,
            CONFIG
                .split("\n# exception attached")
                .next()
                .unwrap()
                .to_owned()
        );
        let assigned = propose(
            &config,
            &request(json!([{"type":"assignFile","path":"src/new.rs","layer":"a"}])),
            &mut || Ok(()),
        )
        .unwrap();
        assert!(assigned.config.text.contains("# keep paths"));
        assert!(
            assigned
                .config
                .text
                .ends_with(CONFIG.split("\n# exception attached").nth(1).unwrap())
        );
        assert_eq!(
            inventory(&assigned.config, request(json!([])).pin().unwrap()).unwrap()["exceptions"]
                [0]["reason"],
            "old"
        );
    }
    #[test]
    fn inline_exception_arrays_are_editable() {
        let inline = "version=1\nexceptions=[{rule='$layers',target={language='rust',path='a.rs'},destination={language='rust',path='b.rs'},reason='old'}] # keep\n[layers.a]\norder=1\npaths=['**']\n";
        let config = rules::parse(inline).unwrap();
        let proposed = propose(
            &config,
            &request(json!([{"type":"editException","index":0,"reason":"new"}])),
            &mut || Ok(()),
        )
        .unwrap();
        assert!(proposed.config.text.contains("# keep"));
        assert_eq!(
            inventory(&proposed.config, request(json!([])).pin().unwrap()).unwrap()["exceptions"]
                [0]["reason"],
            "new"
        );
        let removed = propose(
            &config,
            &request(json!([{"type":"removeException","index":0}])),
            &mut || Ok(()),
        )
        .unwrap();
        assert!(removed.config.text.contains("exceptions=[] # keep"));
    }
    fn disk_fixture() -> (tempfile::TempDir, Base, rules::Config) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".trellis")).unwrap();
        std::fs::write(temp.path().join(rules::RULES_PATH), CONFIG).unwrap();
        let base = load_base(temp.path(), &hash(CONFIG)).unwrap();
        let proposed = propose(
            &base.config,
            &request(json!([{"type":"editException","index":0,"reason":"new"}])),
            &mut || Ok(()),
        )
        .unwrap()
        .config;
        (temp, base, proposed)
    }
    fn no_staging(temp: &tempfile::TempDir) {
        let names: Vec<_> = std::fs::read_dir(temp.path().join(".trellis"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("rules.toml")]);
    }
    #[test]
    fn exact_commit_preserves_permissions_and_cleans_staging() {
        use std::os::unix::fs::PermissionsExt;
        let (temp, base, proposed) = disk_fixture();
        let path = temp.path().join(rules::RULES_PATH);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            commit(temp.path(), &base, &proposed, || Ok(())).unwrap(),
            "committed"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), proposed.text);
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        no_staging(&temp);
    }
    #[test]
    fn fault_before_commit_never_writes_and_cleans_staging() {
        let (temp, base, proposed) = disk_fixture();
        let error =
            commit(temp.path(), &base, &proposed, || anyhow::bail!("cancelled")).unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
        assert_eq!(
            std::fs::read_to_string(temp.path().join(rules::RULES_PATH)).unwrap(),
            CONFIG
        );
        no_staging(&temp);
    }
    #[test]
    fn disk_edit_and_identical_replacement_conflict_before_commit() {
        for replacement in [false, true] {
            let (temp, base, proposed) = disk_fixture();
            let path = temp.path().join(rules::RULES_PATH);
            let error = commit(temp.path(), &base, &proposed, || {
                if replacement {
                    let other = temp.path().join("replacement");
                    std::fs::write(&other, CONFIG)?;
                    std::fs::rename(other, &path)?;
                } else {
                    std::fs::write(&path, "version=1\n# editor change\n")?;
                }
                Ok(())
            })
            .unwrap_err();
            assert!(error.to_string().starts_with("rules_conflict"));
            no_staging(&temp);
        }
    }
    #[test]
    fn symlink_missing_and_changed_hash_are_conflicts() {
        let (temp, _, _) = disk_fixture();
        assert!(load_base(temp.path(), &"0".repeat(64)).is_err());
        let path = temp.path().join(rules::RULES_PATH);
        std::fs::remove_file(&path).unwrap();
        assert!(load_base(temp.path(), &hash(CONFIG)).is_err());
        std::fs::write(temp.path().join("elsewhere"), CONFIG).unwrap();
        std::os::unix::fs::symlink("../elsewhere", path).unwrap();
        assert!(load_base(temp.path(), &hash(CONFIG)).is_err());
    }
    #[test]
    fn shared_reason_expansion_is_refused_before_cloning_the_group() {
        let config = rules::parse(CONFIG).unwrap();
        let reason = "a".repeat(900 * 1024);
        let pairs: Vec<_> = (0..1000).map(|index| json!({"source":{"language":"rust","path":format!("src/{index}.rs")},"destination":{"language":"rust","path":"src/b.rs"}})).collect();
        let request = request(json!([{"type":"addExceptions","pairs":pairs,"reason":reason}]));
        request.validate(false).unwrap();
        let mut checks = 0;
        let error = propose(&config, &request, &mut || {
            checks += 1;
            Ok(())
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains("expanded rules exceed 2 MiB"));
        assert_eq!(checks, 1, "must refuse before creating the per-pair tables");
    }
    #[test]
    fn invalid_ops_and_cancelled_diff_are_bounded_refusals() {
        for ops in [
            json!([]),
            json!([{"type":"addExceptions","pairs":[],"reason":"why"}]),
            json!([{"type":"editException","index":0,"reason":" "}]),
            json!([{"type":"assignFile","path":"*.rs","layer":"a"}]),
        ] {
            assert!(request(ops).validate(false).is_err());
        }
        let config = rules::parse(CONFIG).unwrap();
        assert!(
            propose(
                &config,
                &request(json!([{"type":"editException","index":0,"reason":"new"}])),
                &mut || anyhow::bail!("cancelled")
            )
            .is_err()
        );
        let diff = diff("a\nold", "a\nnew\n", &mut || Ok(())).unwrap();
        assert!(diff.contains("@@ -2,1 +2,1 @@"));
        assert!(diff.contains("\\ No newline at end of file"));
    }
}
