//! Explicit opt-in ACP subprocess transport; attempts are not dollar charges.
use crate::{
    answer::{self, AnswerEnvelope},
    planning::QuestionPacket,
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct AcpConfig {
    pub runner: PathBuf,
    pub state_dir: PathBuf,
    pub max_attempts: u64,
    pub workspace: PathBuf,
}
#[derive(Clone)]
pub struct Acp {
    runner: PathBuf,
    dir: PathBuf,
    workspace: String,
    cap: u64,
    flight: Arc<tokio::sync::Mutex<()>>,
    shutdown: tokio::sync::watch::Sender<bool>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpStatus {
    pub max_attempts: u64,
    pub attempts: u64,
    pub remaining_attempts: u64,
    pub model: &'static str,
    pub max_estimated_usd_per_attempt: f64,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpAnswer {
    pub attempt_id: String,
    pub answer: AnswerEnvelope,
    pub latency_ms: u64,
    pub estimated_usd: Option<f64>,
}
fn storage_error(_: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("ACP private ledger unavailable")
}
#[cfg(unix)]
fn check_file(path: &Path, create: bool) -> Result<()> {
    check_file_with_unlinked(path, create, false)
}
#[cfg(unix)]
fn check_file_with_unlinked(path: &Path, create: bool, allow_unlinked: bool) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    check_file_metadata(&file.metadata()?, allow_unlinked)
}
#[cfg(unix)]
fn check_file_metadata(m: &std::fs::Metadata, allow_unlinked: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    ensure!(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && (m.nlink() == 1 || (allow_unlinked && m.nlink() == 0))
            && m.mode() & 0o777 == 0o600,
        "insecure ledger file"
    );
    Ok(())
}
#[cfg(not(unix))]
fn check_file(_: &Path, _: bool) -> Result<()> {
    anyhow::bail!("private ledger requires Unix")
}
fn check_sidecar(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        // SQLite can unlink a journal after we open it but before fstat.
        // Only sidecars accept an unlinked inode; ownership/type/mode stay strict.
        check_file_with_unlinked(path, false, true)
    }
    #[cfg(not(unix))]
    check_file(path, false)
}
fn check_dir(path: &Path) -> Result<()> {
    #[cfg(not(unix))]
    anyhow::bail!("private ledger requires Unix");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path)?;
        ensure!(
            m.is_dir()
                && !m.file_type().is_symlink()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o777 == 0o700,
            "insecure ledger directory"
        );
        Ok(())
    }
}
#[cfg(unix)]
fn initialization_lock(dir: &Path) -> Result<std::fs::File> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let path = dir.join("initialize.lock");
    check_file(&path, true)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
        "ledger lock failed"
    );
    Ok(file)
}
#[cfg(not(unix))]
fn initialization_lock(_: &Path) -> Result<std::fs::File> {
    anyhow::bail!("private ledger requires Unix")
}

impl Acp {
    pub fn open(config: AcpConfig) -> Result<Self> {
        ensure!(
            (1..=20).contains(&config.max_attempts),
            "ACP allowance must be 1..20 attempts"
        );
        let workspace = config.workspace.canonicalize().map_err(storage_error)?;
        ensure!(workspace.is_dir(), "ACP workspace must be a directory");
        let runner = config
            .runner
            .canonicalize()
            .map_err(|_| anyhow::anyhow!("ACP runner unavailable"))?;
        ensure!(runner.is_file(), "ACP runner unavailable");
        ensure!(
            !runner.starts_with(&workspace),
            "ACP runner must be outside workspace"
        );
        // Check the nearest existing ancestor before creating anything in a read-only source tree.
        ensure!(
            !config
                .state_dir
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "ACP private state path must not contain parent traversal"
        );
        let absolute_state = if config.state_dir.is_absolute() {
            config.state_dir.clone()
        } else {
            std::env::current_dir()
                .map_err(storage_error)?
                .join(&config.state_dir)
        };
        let mut ancestor = absolute_state.as_path();
        while !ancestor.try_exists().map_err(storage_error)? {
            ancestor = ancestor.parent().ok_or_else(|| storage_error("path"))?;
        }
        ensure!(
            !ancestor
                .canonicalize()
                .map_err(storage_error)?
                .starts_with(&workspace),
            "ACP private state must be outside workspace"
        );
        if !config.state_dir.try_exists().map_err(storage_error)? {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&config.state_dir).map_err(storage_error)?;
        }
        check_dir(&config.state_dir).map_err(storage_error)?;
        let dir = config.state_dir.canonicalize().map_err(storage_error)?;
        ensure!(
            !dir.starts_with(&workspace),
            "ACP private state must be outside workspace"
        );
        let this = Self {
            runner,
            dir,
            workspace: workspace
                .to_str()
                .ok_or_else(|| storage_error("path"))?
                .into(),
            cap: config.max_attempts,
            flight: Arc::new(tokio::sync::Mutex::new(())),
            shutdown: tokio::sync::watch::channel(false).0,
        };
        let _lock = initialization_lock(&this.dir).map_err(storage_error)?;
        let marker = this.dir.join("initialized");
        let initialized = std::fs::symlink_metadata(&marker).is_ok();
        check_file(&marker, !initialized).map_err(storage_error)?;
        std::fs::File::open(&marker)
            .and_then(|f| f.sync_all())
            .map_err(storage_error)?;
        sync_dir(&this.dir)?;
        check_file(&this.dir.join("attempts.sqlite3"), !initialized).map_err(storage_error)?;
        let mut db = this.connect()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let version: u32 = tx
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(storage_error)?;
        ensure!(
            version <= 1 && (!initialized || version == 1),
            "ACP private ledger unavailable"
        );
        if version == 0 {
            let tables: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                    [],
                    |r| r.get(0),
                )
                .map_err(storage_error)?;
            ensure!(tables == 0, "ACP private ledger unavailable");
            tx.execute_batch("CREATE TABLE binding(singleton INTEGER PRIMARY KEY CHECK(singleton=1), workspace TEXT NOT NULL, cap INTEGER NOT NULL CHECK(cap BETWEEN 1 AND 20)); CREATE TABLE attempts(id TEXT PRIMARY KEY, status TEXT NOT NULL, packet BLOB NOT NULL, model TEXT NOT NULL DEFAULT 'sonnet', protocol_version INTEGER NOT NULL DEFAULT 1, response BLOB, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);").map_err(storage_error)?;
            tx.execute(
                "INSERT INTO binding VALUES(1,?1,?2)",
                params![this.workspace, this.cap as i64],
            )
            .map_err(storage_error)?;
            tx.pragma_update(None, "user_version", 1)
                .map_err(storage_error)?;
        }
        this.read_status(&tx)?;
        tx.commit().map_err(storage_error)?;
        sync_dir(&this.dir)?;
        Ok(this)
    }
    fn connect(&self) -> Result<Connection> {
        check_dir(&self.dir).map_err(storage_error)?;
        check_file(&self.dir.join("initialized"), false).map_err(storage_error)?;
        let path = self.dir.join("attempts.sqlite3");
        check_file(&path, false).map_err(storage_error)?;
        for suffix in ["-journal", "-wal", "-shm"] {
            let sidecar = self.dir.join(format!("attempts.sqlite3{suffix}"));
            match std::fs::symlink_metadata(&sidecar) {
                Ok(_) => {
                    if let Err(e) = check_sidecar(&sidecar)
                        && !e
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                    {
                        return Err(storage_error(e));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(storage_error(e)),
            }
        }
        let db = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(storage_error)?;
        db.busy_timeout(Duration::from_secs(10))
            .map_err(storage_error)?;
        db.pragma_update(None, "synchronous", "FULL")
            .map_err(storage_error)?;
        Ok(db)
    }
    fn read_status(&self, db: &Connection) -> Result<AcpStatus> {
        let (workspace, cap): (String, i64) = db
            .query_row(
                "SELECT workspace,cap FROM binding WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(storage_error)?;
        ensure!(
            workspace == self.workspace && cap == self.cap as i64,
            "ACP ledger workspace or allowance mismatch"
        );
        let attempts: i64 = db
            .query_row("SELECT COUNT(*) FROM attempts", [], |r| r.get(0))
            .map_err(storage_error)?;
        ensure!(
            attempts >= 0 && attempts <= cap,
            "ACP private ledger unavailable"
        );
        Ok(AcpStatus {
            max_attempts: cap as u64,
            attempts: attempts as u64,
            remaining_attempts: (cap - attempts) as u64,
            model: "sonnet",
            max_estimated_usd_per_attempt: 1.0,
        })
    }
    pub fn status(&self) -> Result<AcpStatus> {
        self.read_status(&self.connect()?)
    }
    fn reserve(&self, packet: &[u8]) -> Result<String> {
        let mut db = self.connect()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure!(
            self.read_status(&tx)?.remaining_attempts > 0,
            "ACP allowance exhausted"
        );
        let id = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO attempts(id,status,packet) VALUES(?1,'reserved_incomplete',?2)",
            params![id, packet],
        )
        .map_err(storage_error)?;
        tx.commit().map_err(storage_error)?;
        Ok(id)
    }
    fn record(&self, id: &str, raw: Option<&[u8]>, status: &str) -> Result<()> {
        let db = self.connect()?;
        self.read_status(&db)?;
        ensure!(
            db.execute(
                "UPDATE attempts SET response=COALESCE(?2,response),status=?3 WHERE id=?1",
                params![id, raw, status]
            )
            .map_err(storage_error)?
                == 1,
            "ACP private ledger unavailable"
        );
        Ok(())
    }
    pub fn cancel_all(&self) {
        self.shutdown.send_replace(true);
    }
    pub async fn run(&self, packet: &QuestionPacket) -> Result<AcpAnswer> {
        let mut shutdown = self.shutdown.subscribe();
        ensure!(!*shutdown.borrow(), "ACP attempt failed: shutdown");
        let _flight = self
            .flight
            .try_lock()
            .map_err(|_| anyhow::anyhow!("ACP request already in flight"))?;
        let prompt =
            answer::build_prompt(packet).map_err(|_| anyhow::anyhow!("ACP invalid request"))?;
        let request =
            serde_json::to_vec(&serde_json::json!({"packetId":packet.packet_id,"prompt":prompt}))
                .map_err(|_| anyhow::anyhow!("ACP invalid request"))?;
        // Commit before spawning. Cancellation or crash can never refund an attempt.
        let audit_packet =
            serde_json::to_vec(packet).map_err(|_| anyhow::anyhow!("ACP invalid request"))?;
        let ledger = self.clone();
        let attempt_id = tokio::task::spawn_blocking(move || ledger.reserve(&audit_packet))
            .await
            .map_err(|_| storage_error("task"))??;
        let mut raw = Vec::new();
        let started = Instant::now();
        let received = tokio::select! {
            biased;
            _ = shutdown.changed() => Err(anyhow::anyhow!("ACP attempt failed: shutdown")),
            result = tokio::time::timeout(Duration::from_secs(120), self.execute_capture(&request, &mut raw)) =>
                result.unwrap_or_else(|_| Err(anyhow::anyhow!("ACP attempt failed: timeout"))),
        };
        raw.truncate(65536);
        // Persist raw bounded output before parsing: invalid answers can be inspected offline.
        let ledger = self.clone();
        let audit_id = attempt_id.clone();
        let audit_raw = raw.clone();
        let status = if received.is_ok() {
            "received_unvalidated"
        } else {
            "transport_failed"
        };
        tokio::task::spawn_blocking(move || ledger.record(&audit_id, Some(&audit_raw), status))
            .await
            .map_err(|_| storage_error("task"))??;
        let received = received.map_err(|error| {
            if error.to_string() == "ACP attempt failed: process"
                && let Some(message) = diagnostic_failure(&raw)
            {
                return anyhow::anyhow!(message);
            }
            error
        });
        let result = received.and_then(|_| {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Response {
                answer: serde_json::Value,
                estimated_usd: Option<f64>,
            }
            let response: Response =
                serde_json::from_slice(&raw).map_err(|_| anyhow::anyhow!("ACP invalid answer"))?;
            ensure!(
                response
                    .estimated_usd
                    .is_none_or(|v| v.is_finite() && v >= 0.0),
                "ACP invalid answer"
            );
            let answer = answer::parse_response(packet, &response.answer)
                .map_err(|_| anyhow::anyhow!("ACP invalid answer"))?;
            Ok(AcpAnswer {
                attempt_id: attempt_id.clone(),
                answer,
                latency_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                estimated_usd: response.estimated_usd,
            })
        });
        let ledger = self.clone();
        let status = if result.is_ok() { "success" } else { "failed" };
        tokio::task::spawn_blocking(move || ledger.record(&attempt_id, None, status))
            .await
            .map_err(|_| storage_error("task"))??;
        result
    }
    #[cfg(test)]
    async fn execute(&self, request: &[u8]) -> Result<Vec<u8>> {
        let mut raw = Vec::new();
        self.execute_capture(request, &mut raw).await?;
        Ok(raw)
    }
    async fn execute_capture(&self, request: &[u8], raw: &mut Vec<u8>) -> Result<()> {
        let scratch = Scratch::new(&self.dir).map_err(storage_error)?;
        let mut command = tokio::process::Command::new(&self.runner);
        command
            .env_clear()
            .current_dir(&scratch.0)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        // Subscription login lives in HOME; API credentials and all other variables are omitted.
        for key in ["HOME", "PATH"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(not(unix))]
        anyhow::bail!("ACP transport requires Unix");
        let mut child = command
            .spawn()
            .map_err(|_| anyhow::anyhow!("ACP attempt failed: process"))?;
        let _group = ProcessGroup(
            child
                .id()
                .ok_or_else(|| anyhow::anyhow!("ACP attempt failed: process"))?,
        );
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let write = async {
            stdin.write_all(request).await?;
            stdin.shutdown().await?;
            drop(stdin);
            Ok::<_, std::io::Error>(())
        };
        let read = async {
            stdout.take(65537).read_to_end(raw).await?;
            Ok::<_, std::io::Error>(())
        };
        let _ = tokio::try_join!(write, read)
            .map_err(|_| anyhow::anyhow!("ACP attempt failed: transport"))?;
        ensure!(raw.len() <= 65536, "ACP attempt failed: output limit");
        ensure!(
            child
                .wait()
                .await
                .map_err(|_| anyhow::anyhow!("ACP attempt failed: process"))?
                .success(),
            "ACP attempt failed: process"
        );
        Ok(())
    }
}
// Only a nonzero process exit may use this diagnostic envelope. Never surface model text.
fn diagnostic_failure(raw: &[u8]) -> Option<&'static str> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Diagnostic {
        error: String,
        #[serde(rename = "partialAnswer")]
        _partial_answer: String,
        #[serde(default)]
        auth_kind: Option<String>,
        #[serde(default)]
        phase: Option<String>,
    }
    let diagnostic: Diagnostic = serde_json::from_slice(raw).ok()?;
    if diagnostic.auth_kind.as_deref().is_some_and(|kind| {
        !matches!(
            kind,
            "account" | "api_key" | "gateway" | "external" | "none" | "unknown"
        )
    }) || diagnostic.phase.as_deref().is_some_and(|phase| {
        !matches!(
            phase,
            "input"
                | "launch"
                | "initialize"
                | "session"
                | "model_selection"
                | "auth"
                | "prompt"
                | "validation"
                | "unknown"
        )
    }) {
        return None;
    }
    match diagnostic.error.as_str() {
        "auth_required" | "authentication_failed" | "oauth_revoked" => {
            Some("ACP authentication required")
        }
        "model_unavailable" => Some("ACP model unavailable"),
        "model_mismatch" => Some("ACP model mismatch"),
        _ => None,
    }
}
fn sync_dir(dir: &Path) -> Result<()> {
    std::fs::File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(storage_error)
}
struct Scratch(PathBuf);
impl Scratch {
    fn new(dir: &Path) -> Result<Self> {
        let path = dir.join(format!("scratch-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
// Unlike kill_on_drop alone, kill the entire process group, including adapter descendants.
// Keep the guard alive after leader exit: descendants may still own stdout or other resources.
struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn setup(script: &str, cap: u64) -> (tempfile::TempDir, tempfile::TempDir, Acp) {
        let base = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let runner = base.path().join("runner");
        std::fs::write(&runner, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
        let acp = Acp::open(AcpConfig {
            runner,
            state_dir: base.path().join("private"),
            max_attempts: cap,
            workspace: work.path().into(),
        })
        .unwrap();
        (base, work, acp)
    }
    fn packet() -> QuestionPacket {
        use crate::{
            indexer::IndexOptions,
            planning::{QuestionRequest, prepare},
            store::Store,
        };
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(work.path().join("a.js"), "function seed() { check(); }\n").unwrap();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let store = Store::open_for_tests(state.path(), work.path()).unwrap();
        crate::store::topology::assert_topology_fixture(&store, state.path());
        let (graph, native, capture) = crate::indexer::index_workspace_bundle(
            &IndexOptions::new(work.path().into()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let session = store.writer_session().unwrap();
        let revision = store
            .publish_native(
                &graph,
                &capture,
                &native,
                session.writer_guard().unwrap(),
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        assert_eq!(store.status().unwrap().revision, revision);
        let request:QuestionRequest=serde_json::from_value(serde_json::json!({"seed":graph.nodes.iter().find(|n| n.name=="seed").unwrap().id,"question":"What does seed call?","expectedRevision":revision})).unwrap();
        let packet = prepare(&store, request).unwrap();
        drop(session);
        packet
    }
    #[tokio::test]
    async fn validated_success_invalid_failure_and_shutdown_retain_attempts() {
        let p = packet();
        let answer = serde_json::json!({"packetId":p.packet_id,"summary":[{"text":"Calls check.","citations":[{"path":"a.js","startLine":1,"endLine":1,"quote":"function seed() { check(); }"}]}],"branches":[],"limitations":[]});
        let body = serde_json::json!({"answer":answer,"estimatedUsd":null});
        let (_base, _work, acp) = setup(&format!("cat >/dev/null; printf '%s' '{}'", body), 3);
        assert!(acp.run(&p).await.is_ok());
        assert_eq!(acp.status().unwrap().attempts, 1);
        std::fs::write(&acp.runner, "#!/bin/sh\ncat >/dev/null; printf '%s' '{}'\n").unwrap();
        let error = acp.run(&p).await.err().unwrap();
        assert_eq!(error.to_string(), "ACP invalid answer");
        assert_eq!(acp.status().unwrap().attempts, 2);
        let db = acp.connect().unwrap();
        let (saved_packet, raw): (Vec<u8>, Vec<u8>) = db
            .query_row(
                "SELECT packet,response FROM attempts WHERE status='failed'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&saved_packet).unwrap()["packetId"],
            p.packet_id
        );
        assert_eq!(raw, b"{}");
        drop(db);
        std::fs::write(&acp.runner, "#!/bin/sh\ncat >/dev/null; sleep 60\n").unwrap();
        let cloned = acp.clone();
        let task = tokio::spawn(async move { cloned.run(&p).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while acp.status().unwrap().attempts < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        acp.cancel_all();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert_eq!(acp.status().unwrap().attempts, 3);
        assert!(acp.run(&packet()).await.is_err());
    }
    #[tokio::test]
    async fn nonzero_diagnostics_only_surface_fixed_allowlisted_categories() {
        for (category, expected) in [
            ("auth_required", "ACP authentication required"),
            ("model_unavailable", "ACP model unavailable"),
            ("model_mismatch", "ACP model mismatch"),
            ("SECRET", "ACP attempt failed: process"),
        ] {
            let body =
                serde_json::json!({"error":category,"partialAnswer":"PRIVATE_SOURCE_SECRET"});
            let (_base, _work, acp) = setup(
                &format!("cat >/dev/null; printf '%s' '{}'; exit 1", body),
                1,
            );
            assert_eq!(
                acp.run(&packet()).await.err().unwrap().to_string(),
                expected
            );
            assert_eq!(acp.status().unwrap().attempts, 1);
        }
        assert_eq!(diagnostic_failure(br#"{"error":"auth_required","partialAnswer":"private","authKind":"account","phase":"auth"}"#), Some("ACP authentication required"));
        assert_eq!(diagnostic_failure(br#"{"error":"authentication_failed","partialAnswer":"private","authKind":"account","phase":"auth"}"#), Some("ACP authentication required"));
        assert!(diagnostic_failure(br#"{"error":"auth_required","partialAnswer":"private","authKind":"SECRET","phase":"auth"}"#).is_none());
        assert!(
            diagnostic_failure(
                br#"{"error":"auth_required","partialAnswer":"private","extra":"secret"}"#
            )
            .is_none()
        );
    }
    #[test]
    fn rejects_source_state_before_creating_directories() {
        let (_base, work, acp) = setup("exit 1", 2);
        let nested = work.path().join("must-not-exist/private");
        assert!(
            Acp::open(AcpConfig {
                runner: acp.runner.clone(),
                state_dir: nested,
                max_attempts: 2,
                workspace: work.path().into()
            })
            .is_err()
        );
        assert!(!work.path().join("must-not-exist").exists());
    }
    #[test]
    fn durable_allowance_binding_and_exhaustion() {
        let (_base, work, acp) = setup("exit 1", 2);
        acp.reserve(b"test packet").unwrap();
        acp.reserve(b"test packet").unwrap();
        assert_eq!(
            acp.reserve(b"test packet").unwrap_err().to_string(),
            "ACP allowance exhausted"
        );
        let reopen = |cap, workspace| {
            Acp::open(AcpConfig {
                runner: acp.runner.clone(),
                state_dir: acp.dir.clone(),
                max_attempts: cap,
                workspace,
            })
        };
        assert_eq!(
            reopen(2, work.path().into())
                .unwrap()
                .status()
                .unwrap()
                .attempts,
            2
        );
        assert!(reopen(3, work.path().into()).is_err());
        assert!(reopen(2, _base.path().into()).is_err());
    }
    #[test]
    fn deleted_or_truncated_ledger_never_resets() {
        for truncate in [false, true] {
            let (_base, work, acp) = setup("exit 1", 2);
            acp.reserve(b"test packet").unwrap();
            let path = acp.dir.join("attempts.sqlite3");
            if truncate {
                std::fs::write(path, []).unwrap();
            } else {
                std::fs::remove_file(path).unwrap();
            }
            assert!(acp.status().is_err());
            assert!(
                Acp::open(AcpConfig {
                    runner: acp.runner.clone(),
                    state_dir: acp.dir.clone(),
                    max_attempts: 2,
                    workspace: work.path().into()
                })
                .is_err()
            );
        }
    }
    #[tokio::test]
    async fn process_failure_output_bound_and_private_environment() {
        let (_base, _work, acp) = setup(
            "cat >/dev/null; printf '%s' \"${JEV_KEY-unset}:${ANTHROPIC_API_KEY-unset}:$PWD\"",
            2,
        );
        let raw = acp.execute(b"{}").await.unwrap();
        let text = String::from_utf8(raw).unwrap();
        assert!(text.starts_with("unset:unset:"));
        assert!(text.contains("scratch-"));
        assert!(!text.contains(&acp.workspace));
        let (_base, _work, acp) = setup("printf 'SECRET' >&2; exit 1", 2);
        assert!(
            !acp.execute(b"{}")
                .await
                .unwrap_err()
                .to_string()
                .contains("SECRET")
        );
        let (_base, _work, acp) = setup("/usr/bin/yes x", 2);
        assert!(
            acp.execute(b"{}")
                .await
                .unwrap_err()
                .to_string()
                .contains("output limit")
        );
    }
    #[tokio::test]
    async fn cancellation_kills_descendant_group() {
        let (base, _work, acp) = setup("cat >/dev/null; sleep 60 & echo $! > \"$1\"; wait", 2);
        // PID evidence is outside scratch so it remains observable after cleanup.
        let pidfile = base.path().join("pid");
        std::fs::write(
            &acp.runner,
            format!(
                "#!/bin/sh\ncat >/dev/null\nsleep 60 &\necho $! > '{}'\nwait\n",
                pidfile.display()
            ),
        )
        .unwrap();
        let cloned = acp.clone();
        let task = tokio::spawn(async move { cloned.execute(b"{}").await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !std::fs::read_to_string(&pidfile)
                .ok()
                .is_some_and(|s| s.trim().parse::<i32>().is_ok())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if unsafe { libc::kill(pid, 0) } != 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!std::fs::read_dir(&acp.dir).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("scratch-")
        }));
    }
}
