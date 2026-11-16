//! Checks captured under the registry lock and run without it: a cold checkout's prior
//! head and the browser's checkout listing.
use super::{BrowserCheckoutEntry, BrowserCheckoutsProbe, CheckoutRegistry, PriorHeadProbe};
use crate::store::Store;
use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::Instant;

impl PriorHeadProbe {
    pub fn check(self) -> anyhow::Result<()> {
        let identity = self.identity.rediscover()?;
        let roots = match self.roots {
            Some(roots) => roots,
            None => TopologyRoots::production()?,
        };
        match Store::status_existing_readonly(roots, identity) {
            Ok(_) => Ok(()),
            // A cache tree without an admissible lock/format has no selected
            // head. Preserve cold W1's inert answer without creating state.
            Err(error)
                if error.to_string().starts_with("unsafe managed directory:")
                    || error
                        .to_string()
                        .starts_with("incompatible_index: missing or unsafe use lock")
                    || error
                        .to_string()
                        .starts_with("recovery_required: exceptional index format;") =>
            {
                anyhow::bail!("index_not_ready: no admitted head")
            }
            Err(error) => Err(error),
        }
    }
}

impl BrowserCheckoutsProbe {
    pub fn check(self) -> Vec<serde_json::Value> {
        let mut rows = std::collections::BTreeMap::new();
        let mut replacements = HashSet::new();
        let roots = self
            .roots
            .clone()
            .map(Ok)
            .unwrap_or_else(TopologyRoots::production);
        for (key, entry) in &self.entries {
            let index = roots
                .as_ref()
                .ok()
                .map(|roots| roots.cache.join("indexes").join(key).join("index.db"));
            let old_verified = entry.identity.rediscover().is_ok();
            // The old index belongs to the old inode and is never evidence for
            // its replacement. Listing is observational: a new same-key root is
            // selectable only after the old runtime has actually released.
            // browser_identity rechecks and performs the mutable retirement.
            let replaced_available = !old_verified
                && entry.replacement_idle
                && entry
                    .lost_reads
                    .as_ref()
                    .is_none_or(|reads| reads.load(std::sync::atomic::Ordering::Acquire) == 0)
                && WorkspaceIdentity::discover_unattached(
                    Some(&entry.identity.root),
                    &entry.identity.root,
                )
                .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
                .is_ok_and(|new| {
                    new.root_key == *key
                        && !entry.identity.matches(&new)
                        && WorkspaceIdentity::discover_unattached(Some(&new.root), &new.root)
                            .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
                            .is_ok_and(|found| {
                                !entry.identity.matches(&found)
                                    && found.record_id == new.record_id
                                    && (found.device, found.inode) == (new.device, new.inode)
                            })
                });
            let state = if replaced_available {
                replacements.insert(key.clone());
                "available"
            } else if !old_verified {
                "unavailable"
            } else if entry.runtime.is_some() {
                "available"
            } else if index.as_ref().is_some_and(|index| index.is_file()) {
                roots
                    .as_ref()
                    .ok()
                    .and_then(|roots| Store::browser_index_root_existing(roots, key).err())
                    .unwrap_or("available")
            } else {
                "available"
            };
            rows.insert(
                key.clone(),
                serde_json::json!({
                    "rootKey": key, "workspaceRoot": entry.identity.root,
                    "state": state,
                    "active": entry.active
                }),
            );
        }
        if let Ok(roots) = &roots
            && let Ok(indexes) = fs::read_dir(roots.cache.join("indexes"))
        {
            for candidate in indexes.flatten() {
                let key = candidate.file_name().to_string_lossy().to_string();
                if rows.contains_key(&key)
                    || key.len() != 64
                    || !key
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                {
                    continue;
                }
                let row = match Store::browser_index_root_existing(roots, &key) {
                    Ok(spelling)
                        if WorkspaceIdentity::discover_unattached(
                            Some(Path::new(&spelling)),
                            Path::new(&spelling),
                        )
                        .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
                        .is_ok_and(|identity| identity.root_key == key) =>
                    {
                        serde_json::json!({"rootKey": key, "workspaceRoot": spelling, "state":"available", "active":false})
                    }
                    Ok(spelling) => {
                        serde_json::json!({"rootKey": key, "workspaceRoot": spelling, "state":"unavailable", "active":false})
                    }
                    Err(state) => {
                        serde_json::json!({"rootKey": key, "state":state, "active":false})
                    }
                };
                rows.insert(key, row);
            }
        }
        for (key, row) in &mut rows {
            row["revision"] = serde_json::Value::Null;
            row["catchingUp"] = serde_json::Value::Null;
            row["indexError"] = serde_json::Value::Null;
            if row["state"] != "available" {
                row["indexError"] = row["state"].clone();
                continue;
            }
            let entry = self.entries.get(key);
            // A selectable replacement has no claim on its predecessor's pin.
            if replacements.contains(key) {
                row["indexError"] = serde_json::json!("index_not_ready");
                continue;
            }
            let runtime = entry.and_then(|entry| entry.runtime.as_ref());
            let observation = (|| {
                if let Some(entry) = entry {
                    entry.identity.rediscover()?;
                }
                if let Some(runtime) = runtime {
                    // The listing carries each checkout's Git header; a publication that holds
                    // readers out reports `storage_busy` at once instead of delaying it (#905).
                    crate::store::with_spill_answer_by(Instant::now(), || {
                        let (response, catching_up) = runtime.evidence_response()?;
                        let revision = response.status()?.revision;
                        response.finish((revision, Some(catching_up)))
                    })
                } else {
                    let roots = roots
                        .as_ref()
                        .map_err(|_| anyhow::anyhow!("unavailable: topology roots unavailable"))?;
                    let spelling = row["workspaceRoot"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("unavailable: checkout root unavailable"))?;
                    let root = Path::new(spelling);
                    let identity = WorkspaceIdentity::discover_unattached(Some(root), root)?
                        .attach_existing_marker_readonly()?;
                    anyhow::ensure!(
                        identity.root_key == *key,
                        "root_changed: checkout key changed"
                    );
                    let status = Store::status_existing_readonly(roots.clone(), identity)?;
                    Ok((status.revision, None))
                }
            })();
            match observation {
                Ok((revision, catching_up)) => {
                    row["revision"] = serde_json::json!(revision);
                    row["catchingUp"] = serde_json::json!(catching_up);
                }
                Err(error) => {
                    let code = browser_index_error(&error);
                    row["indexError"] = serde_json::json!(code);
                    if code == "index_not_ready" {
                        row["catchingUp"] =
                            serde_json::json!(runtime.map(|runtime| runtime.catching_up()));
                    } else {
                        row["state"] = serde_json::json!(match code {
                            "storage_busy" => "storage_busy",
                            "corrupt" => "corrupt",
                            _ => "unavailable",
                        });
                    }
                }
            }
        }
        rows.into_values().collect()
    }
}

fn browser_index_error(error: &anyhow::Error) -> &'static str {
    if crate::store::transient_storage_contention(error) {
        return "storage_busy";
    }
    for cause in error.chain() {
        let text = cause.to_string();
        for code in [
            "root_changed",
            "root_key_collision",
            "workspace_id_changed",
            "index_not_ready",
        ] {
            if text.starts_with(code) {
                return code;
            }
        }
        if text.starts_with("incompatible_index")
            || text.starts_with("corrupt")
            || text.starts_with("recovery_required: exceptional index format")
        {
            return "corrupt";
        }
    }
    "unavailable"
}

impl CheckoutRegistry {
    /// Probe only this registry's configured topology. A virgin checkout must not
    /// create derived state to answer a read, while a published head may activate.
    pub fn eligible_prior_head(&self, key: &str) -> anyhow::Result<()> {
        self.prior_head_probe(key)?.check()
    }

    /// Capture what a cold prior-head check needs, so the caller can release
    /// the registry lock before rediscovery and the SQLite open.
    pub fn prior_head_probe(&self, key: &str) -> anyhow::Result<PriorHeadProbe> {
        let entry = self
            .entries
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("index_not_ready: checkout not attached"))?;
        Ok(PriorHeadProbe {
            identity: entry.identity.clone(),
            roots: self.roots.clone(),
        })
    }

    /// Capture observational metadata only. Run `check` after releasing the registry.
    pub fn browser_checkouts_probe(&self) -> BrowserCheckoutsProbe {
        BrowserCheckoutsProbe {
            roots: self.roots.clone(),
            entries: self
                .entries
                .iter()
                .map(|(key, entry)| {
                    (
                        key.clone(),
                        BrowserCheckoutEntry {
                            identity: entry.identity.clone(),
                            active: self.runtimes.contains_key(key),
                            runtime: self.runtimes.get(key).cloned(),
                            replacement_idle: entry.released
                                && !self.runtimes.contains_key(key)
                                && entry.sessions.is_empty()
                                && entry.browser_until.is_none()
                                && !entry.pending_work
                                && !entry.external_work,
                            lost_reads: entry.lost_reads.clone(),
                        },
                    )
                })
                .collect(),
        }
    }

    /// Synchronous convenience for fixtures; daemon callers use the probe.
    pub fn browser_checkouts(&self) -> Vec<serde_json::Value> {
        self.browser_checkouts_probe().check()
    }
}
