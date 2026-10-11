//! Explicit opt-in transport. Reservations are conservative, not provider invoices.
//! Request and bounded raw-response artifacts live atomically in the private SQLite DB.
use crate::{
    jev::{parse_response, request_for, response_warnings},
    planning::{QuestionPacket, SelectionEnvelope},
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::Serialize;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const MAX_RESPONSE: usize = 2 * 1024 * 1024;
const RESERVATION: u64 = 10;
const TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetStatus {
    pub cap_cents: u64,
    pub reserved_cents: u64,
    pub remaining_cents: u64,
    pub attempts: u64,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSelection {
    pub attempt_id: String,
    pub selection: SelectionEnvelope,
    pub latency_ms: u64,
    /// Published input-only estimate ($0.042/M tokens), not an invoice.
    pub estimated_usd: Option<f64>,
    pub usage: Option<Value>,
    pub warnings: Vec<String>,
}
// Intentionally no Debug: the client has a sensitive authorization header.
#[derive(Clone)]
pub struct LiveJev {
    dir: PathBuf,
    workspace: String,
    cap: u64,
    client: reqwest::Client,
    #[cfg(test)]
    test_endpoint: Option<String>,
}

fn storage_error(_: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("Jev private ledger unavailable")
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
impl LiveJev {
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn with_test_endpoint(mut self, endpoint: String) -> Self {
        let url = reqwest::Url::parse(&endpoint).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        self.test_endpoint = Some(endpoint);
        self
    }
    pub fn open(dir: &Path, key: String, cap_cents: u64, workspace: &Path) -> Result<Self> {
        ensure!(
            (10..=500).contains(&cap_cents),
            "Jev cap must be 10..500 cents"
        );
        ensure!(!key.trim().is_empty(), "Jev credential is required");
        let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| anyhow::anyhow!("invalid Jev credential"))?;
        authorization.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, authorization);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .retry(reqwest::retry::never())
            .timeout(TIMEOUT)
            .build()
            .map_err(|_| anyhow::anyhow!("Jev transport initialization failed"))?;
        let workspace = workspace.canonicalize().map_err(storage_error)?;
        ensure!(workspace.is_dir(), "Jev workspace must be a directory");
        let workspace = workspace
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid workspace path"))?
            .to_owned();
        if !dir.try_exists().map_err(storage_error)? {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(dir).map_err(storage_error)?;
        }
        check_dir(dir).map_err(storage_error)?;
        let this = Self {
            dir: dir.canonicalize().map_err(storage_error)?,
            workspace,
            cap: cap_cents,
            client,
            #[cfg(test)]
            test_endpoint: None,
        };
        // Serialize first initialization across processes before writing the durable marker.
        let _init_lock = initialization_lock(&this.dir).map_err(storage_error)?;
        let marker = this.dir.join("initialized");
        let initialized = std::fs::symlink_metadata(&marker).is_ok();
        if initialized {
            check_file(&marker, false).map_err(storage_error)?;
        } else {
            check_file(&marker, true).map_err(storage_error)?;
            std::fs::File::open(&marker)
                .and_then(|f| f.sync_all())
                .map_err(storage_error)?;
            std::fs::File::open(&this.dir)
                .and_then(|f| f.sync_all())
                .map_err(storage_error)?;
        }
        // Once marked, missing or truncated databases never silently restore a budget.
        check_file(&this.dir.join("budget.sqlite3"), !initialized).map_err(storage_error)?;
        let mut db = this.connect()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let version: u32 = tx
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(storage_error)?;
        ensure!(
            version <= 1 && (!initialized || version == 1),
            "unsupported or incomplete Jev ledger schema"
        );
        if version == 0 {
            let tables: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                    [],
                    |r| r.get(0),
                )
                .map_err(storage_error)?;
            ensure!(tables == 0, "unrecognized Jev ledger schema");
            tx.execute_batch(
                "CREATE TABLE binding (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), workspace TEXT NOT NULL,
            cap INTEGER NOT NULL CHECK(cap BETWEEN 10 AND 500));
            CREATE TABLE attempts (
            id TEXT PRIMARY KEY, reserved INTEGER NOT NULL CHECK(reserved=10),
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            request BLOB NOT NULL, response BLOB, status TEXT NOT NULL,
            http_status INTEGER, latency_ms INTEGER);",
            )
            .map_err(storage_error)?;
            tx.execute(
                "INSERT OR IGNORE INTO binding VALUES(1,?1,?2)",
                params![this.workspace, this.cap as i64],
            )
            .map_err(storage_error)?;
            tx.pragma_update(None, "user_version", 1)
                .map_err(storage_error)?;
        }
        this.check_binding(&tx)?;
        tx.prepare("SELECT id,reserved,created_at,request,response,status,http_status,latency_ms FROM attempts")
            .map_err(storage_error)?;
        this.status(&tx)?;
        tx.commit().map_err(storage_error)?;
        // Persist directory entries as well as SQLite's FULL-synchronous transaction.
        std::fs::File::open(&this.dir)
            .and_then(|f| f.sync_all())
            .map_err(storage_error)?;
        Ok(this)
    }
    fn connect(&self) -> Result<Connection> {
        check_dir(&self.dir).map_err(storage_error)?;
        let path = self.dir.join("budget.sqlite3");
        check_file(&path, false).map_err(storage_error)?;
        for suffix in ["-journal", "-wal", "-shm"] {
            let sidecar = self.dir.join(format!("budget.sqlite3{suffix}"));
            match std::fs::symlink_metadata(&sidecar) {
                Ok(_) => {
                    if let Err(error) = check_sidecar(&sidecar) {
                        // A concurrent SQLite writer may unlink its rollback journal.
                        if !error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                        {
                            return Err(storage_error(error));
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
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
        // Default rollback journal: SQLite derives 0600 journal permissions from the DB.
        Ok(db)
    }
    fn check_binding(&self, db: &Connection) -> Result<()> {
        let (workspace, cap): (String, i64) = db
            .query_row(
                "SELECT workspace,cap FROM binding WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(storage_error)?;
        ensure!(
            workspace == self.workspace && cap == self.cap as i64,
            "Jev ledger workspace or cap mismatch"
        );
        Ok(())
    }
    fn status(&self, db: &Connection) -> Result<BudgetStatus> {
        self.check_binding(db)?;
        let (reserved, attempts): (i64, i64) = db
            .query_row(
                "SELECT COALESCE(SUM(reserved),0),COUNT(*) FROM attempts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(storage_error)?;
        ensure!(
            reserved >= 0 && reserved <= self.cap as i64 && attempts >= 0,
            "invalid Jev ledger totals"
        );
        Ok(BudgetStatus {
            cap_cents: self.cap,
            reserved_cents: reserved as u64,
            remaining_cents: self.cap - reserved as u64,
            attempts: attempts as u64,
        })
    }
    pub fn budget(&self) -> Result<BudgetStatus> {
        self.status(&self.connect()?)
    }
    fn reserve(&self, request: &[u8]) -> Result<String> {
        let mut db = self.connect()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        ensure!(
            self.status(&tx)?.remaining_cents >= RESERVATION,
            "Jev budget exhausted: no reservation available"
        );
        let id = uuid::Uuid::new_v4().to_string();
        tx.execute("INSERT INTO attempts(id,reserved,request,status) VALUES(?1,10,?2,'reserved_incomplete')",
            params![id, request]).map_err(storage_error)?;
        tx.commit().map_err(storage_error)?;
        Ok(id)
    }
    fn finish(
        &self,
        id: &str,
        raw: &[u8],
        status: &str,
        http: Option<u16>,
        latency: u64,
    ) -> Result<()> {
        let db = self.connect()?;
        self.check_binding(&db)?;
        ensure!(db.execute("UPDATE attempts SET response=?2,status=?3,http_status=?4,latency_ms=?5 WHERE id=?1",
            params![id,raw,status,http,latency as i64]).map_err(storage_error)? == 1, "Jev audit attempt missing");
        Ok(())
    }
    pub async fn run(&self, packet: &QuestionPacket) -> Result<LiveSelection> {
        let request = request_for(packet).map_err(|_| anyhow::anyhow!("invalid Jev request"))?;
        ensure!(
            !packet.context.calls.is_empty(),
            "Jev requires at least one candidate"
        );
        let bytes =
            serde_json::to_vec(&request).map_err(|_| anyhow::anyhow!("invalid Jev request"))?;
        let ledger = self.clone();
        let audit_request = bytes.clone();
        let attempt_id = tokio::task::spawn_blocking(move || ledger.reserve(&audit_request))
            .await
            .map_err(|_| anyhow::anyhow!("Jev reservation task failed"))??;
        let started = Instant::now();
        let mut raw = Vec::new();
        let mut http = None;
        let endpoint = ENDPOINT;
        #[cfg(test)]
        let endpoint = self.test_endpoint.as_deref().unwrap_or(endpoint);
        // Cancellation/crash leaves reserved_incomplete; no reservation is ever refunded.
        let received = tokio::time::timeout(TIMEOUT, async {
            let mut response = self
                .client
                .post(endpoint)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(bytes)
                .send()
                .await
                .map_err(|_| "transport_failed")?;
            http = Some(response.status().as_u16());
            while let Some(chunk) = response.chunk().await.map_err(|_| "response_read_failed")? {
                let remaining = MAX_RESPONSE - raw.len();
                raw.extend_from_slice(&chunk[..remaining.min(chunk.len())]);
                if chunk.len() > remaining {
                    return Err("response_too_large");
                }
            }
            if !response.status().is_success() {
                // Only this exact, allowlisted provider shape earns a specific status.
                // Never include arbitrary response fields or text in diagnostics.
                if response.status() == reqwest::StatusCode::BAD_REQUEST
                    && serde_json::from_slice::<Value>(&raw).ok().as_ref()
                        == Some(&serde_json::json!({"detail":{"error_type":"max_tokens_exceeded"}}))
                {
                    return Err("context_exceeded");
                }
                return Err("http_failed");
            }
            Ok(())
        })
        .await
        .unwrap_or(Err("timeout"));
        let latency_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let parsed = received.and_then(|_| {
            let value: Value = serde_json::from_slice(&raw).map_err(|_| "invalid_json")?;
            let selection = parse_response(packet, &value).map_err(|_| "invalid_selection")?;
            Ok((value, selection))
        });
        let status = parsed.as_ref().map(|_| "success").unwrap_or_else(|e| *e);
        let ledger = self.clone();
        let audit_id = attempt_id.clone();
        tokio::task::spawn_blocking(move || {
            ledger.finish(&audit_id, &raw, status, http, latency_ms)
        })
        .await
        .map_err(|_| anyhow::anyhow!("Jev audit task failed"))??;
        let (value, selection) =
            parsed.map_err(|status| anyhow::anyhow!("Jev attempt failed: {status}"))?;
        let warnings = response_warnings(&value);
        let usage = value.get("usage").cloned();
        let estimated_usd = usage
            .as_ref()
            .and_then(|v| v["input_tokens"].as_u64())
            .map(|tokens| tokens as f64 * 0.042 / 1_000_000.0);
        Ok(LiveSelection {
            attempt_id,
            selection,
            latency_ms,
            estimated_usd,
            usage,
            warnings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        indexer::IndexOptions,
        planning::{QuestionRequest, prepare},
        store::Store,
    };
    use serde_json::json;
    use std::sync::{Arc, atomic::AtomicBool};
    const CODE: &str =
        "// UNIQUE_COMPLETE_SOURCE\nfunction seed(flag) { if (flag) check(); run(callback); }\n";

    fn packet_with(code: &str, question: &str) -> QuestionPacket {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::write(work.path().join("a.js"), code).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
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
        let request: QuestionRequest = serde_json::from_value(json!({
            "seed":graph.nodes.iter().find(|n| n.name == "seed").unwrap().id,
            "question":question,"expectedRevision":revision
        }))
        .unwrap();
        let packet = prepare(&store, request).unwrap();
        drop(session);
        packet
    }
    fn packet() -> QuestionPacket {
        packet_with(CODE, "How is the request checked?")
    }
    fn alias(packet: &QuestionPacket, index: usize) -> String {
        format!("c{index}_{}", packet.packet_id)
    }
    fn synthetic_response(packet: &QuestionPacket) -> Value {
        let answers: serde_json::Map<String, Value> = packet
            .context
            .calls
            .iter()
            .enumerate()
            .map(|(i, _)| {
                (
                    alias(packet, i),
                    json!({"type":"choice","choice":"essential","confidence":0.6,
        "probabilities":{"essential":0.7,"supporting":0.2,"incidental":0.1,"uncertain":0.0}}),
                )
            })
            .collect();
        json!({"model":"jev-1.13.0","answers":answers,"usage":{"input_tokens":1,"output_tokens":1}})
    }

    fn provider(dir: &Path, workspace: &Path, cap: u64) -> LiveJev {
        #[cfg(unix)]
        if dir.is_dir() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        LiveJev::open(dir, "SYNTHETIC_TEST_KEY".into(), cap, workspace).unwrap()
    }
    async fn mock(body: Vec<u8>, status: u16) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mock", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut buf = [0; 4096];
            loop {
                let count = socket.read(&mut buf).await.unwrap();
                assert!(count > 0);
                received.extend_from_slice(&buf[..count]);
                if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&received[..end]).to_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if received.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let header = format!(
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nLocation: http://127.0.0.1:1/forbidden\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(header.as_bytes()).await;
            let _ = socket.write_all(&body).await;
        });
        (url, task)
    }
    #[cfg(unix)]
    #[test]
    fn unlinked_sidecar_metadata_is_safe_but_main_file_stays_strict() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        std::fs::hard_link(&path, dir.path().join("link")).unwrap();
        assert!(check_file_metadata(&file.metadata().unwrap(), true).is_err());
        std::fs::remove_file(dir.path().join("link")).unwrap();
        assert!(check_file_metadata(&file.metadata().unwrap(), false).is_ok());
        std::fs::remove_file(&path).unwrap();
        assert!(check_file_metadata(&file.metadata().unwrap(), true).is_ok());
        assert!(check_file_metadata(&file.metadata().unwrap(), false).is_err());
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(check_file_metadata(&file.metadata().unwrap(), true).is_err());
    }

    #[test]
    fn reservation_concurrent_reopen_and_binding() {
        let dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let live = provider(dir.path(), work.path(), 500);
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.path().to_owned();
                let work = work.path().to_owned();
                std::thread::spawn(move || {
                    let live = provider(&dir, &work, 500);
                    (0..15)
                        .filter(|_| live.reserve(b"synthetic request").is_ok())
                        .count()
                })
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .map(|h| h.join().unwrap())
                .sum::<usize>(),
            50
        );
        drop(live);
        let reopened = provider(dir.path(), work.path(), 500);
        let b = reopened.budget().unwrap();
        assert_eq!(
            (b.reserved_cents, b.remaining_cents, b.attempts),
            (500, 0, 50)
        );
        assert!(
            reopened
                .reserve(b"ignored")
                .unwrap_err()
                .to_string()
                .starts_with("Jev budget exhausted")
        );
        assert!(LiveJev::open(dir.path(), "fake".into(), 400, work.path()).is_err());
        let other = tempfile::tempdir().unwrap();
        assert!(LiveJev::open(dir.path(), "fake".into(), 500, other.path()).is_err());
    }
    #[tokio::test]
    async fn mock_success_and_failed_responses_are_audited_and_retained() {
        let dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let p = packet();
        let cases = vec![
            (
                serde_json::to_vec(&synthetic_response(&p)).unwrap(),
                200,
                "success",
            ),
            (b"private invalid body".to_vec(), 200, "invalid_json"),
            (b"{}".to_vec(), 200, "invalid_selection"),
            (b"private server failure".to_vec(), 500, "http_failed"),
            (
                br#"{"detail":{"error_type":"max_tokens_exceeded"}}"#.to_vec(),
                400,
                "context_exceeded",
            ),
            (
                br#"{"detail":{"error_type":"max_tokens_exceeded"}}"#.to_vec(),
                500,
                "http_failed",
            ),
            (
                br#"{"detail":{"error_type":"max_tokens_exceeded","secret":"private"}}"#.to_vec(),
                400,
                "http_failed",
            ),
            (b"redirect".to_vec(), 302, "http_failed"),
            (vec![b'x'; MAX_RESPONSE + 10], 200, "response_too_large"),
        ];
        for (body, http, status) in cases {
            let (url, server) = mock(body.clone(), http).await;
            let mut live = provider(dir.path(), work.path(), 500);
            live.test_endpoint = Some(url);
            let outcome = live.run(&p).await;
            server.await.unwrap();
            if status == "success" {
                let result = outcome.unwrap();
                assert_eq!(result.selection.decisions.len(), p.context.calls.len());
                assert_eq!(result.estimated_usd, Some(0.042 / 1_000_000.0));
            } else {
                let error = outcome.err().unwrap().to_string();
                assert_eq!(error, format!("Jev attempt failed: {status}"));
                assert!(!error.contains("private"));
            }
            let db = live.connect().unwrap();
            let (saved, raw, request): (String, Vec<u8>, Vec<u8>) = db
                .query_row(
                    "SELECT status,response,request FROM attempts ORDER BY rowid DESC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(saved, status);
            assert_eq!(raw, &body[..body.len().min(MAX_RESPONSE)]);
            assert!(!String::from_utf8_lossy(&request).contains("SYNTHETIC_TEST_KEY"));
        }
        let b = provider(dir.path(), work.path(), 500).budget().unwrap();
        assert_eq!((b.attempts, b.reserved_cents), (9, 90));
    }
    #[tokio::test]
    async fn rounded_success_preserves_raw_and_historical_failure_without_refunds() {
        let dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let p = packet();
        let mut live = provider(dir.path(), work.path(), 500);
        let mut rounded = synthetic_response(&p);
        for answer in rounded["answers"].as_object_mut().unwrap().values_mut() {
            answer["probabilities"]["essential"] = json!(0.69);
        }
        let raw = serde_json::to_vec(&rounded).unwrap();
        // A synthetic historical failure must not be reclassified by a new attempt.
        let historical = live.reserve(b"synthetic historical request").unwrap();
        live.finish(&historical, &raw, "invalid_selection", Some(200), 1)
            .unwrap();
        for essential in [0.69, 0.71, 0.60] {
            let mut response = rounded.clone();
            for answer in response["answers"].as_object_mut().unwrap().values_mut() {
                answer["probabilities"]["essential"] = json!(essential);
            }
            let body = serde_json::to_vec(&response).unwrap();
            let (url, server) = mock(body.clone(), 200).await;
            live.test_endpoint = Some(url);
            let result = live.run(&p).await;
            server.await.unwrap();
            let expected_status = if essential == 0.60 {
                assert_eq!(
                    result.err().unwrap().to_string(),
                    "Jev attempt failed: invalid_selection"
                );
                "invalid_selection"
            } else {
                assert!(!result.unwrap().warnings.is_empty());
                "success"
            };
            let db = live.connect().unwrap();
            let (status, saved): (String, Vec<u8>) = db
                .query_row(
                    "SELECT status,response FROM attempts ORDER BY rowid DESC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(status, expected_status);
            assert_eq!(saved, body);
            let (status, saved): (String, Vec<u8>) = db
                .query_row(
                    "SELECT status,response FROM attempts WHERE id=?",
                    [&historical],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(status, "invalid_selection");
            assert_eq!(saved, raw);
        }
        let budget = live.budget().unwrap();
        assert_eq!(
            (
                budget.attempts,
                budget.reserved_cents,
                budget.remaining_cents
            ),
            (4, 40, 460)
        );
    }

    #[tokio::test]
    async fn invalid_and_empty_packet_never_reserve() {
        let dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let live = provider(dir.path(), work.path(), 500);
        let mut p = packet();
        p.packet_id = "invalid".into();
        assert!(live.run(&p).await.is_err());
        let p = packet_with("function seed() {}", "What happens?");
        assert!(live.run(&p).await.is_err());
        assert_eq!(live.budget().unwrap().attempts, 0);
    }
    #[cfg(unix)]
    #[test]
    fn rejects_insecure_files_and_unknown_schema() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let dir = root.path().join("budget");
        let live = provider(&dir, work.path(), 500);
        let path = dir.join("budget.sqlite3");
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        std::fs::hard_link(&path, dir.join("link")).unwrap();
        assert!(live.budget().is_err());
        std::fs::remove_file(dir.join("link")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(live.budget().is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&path, dir.join("budget.sqlite3-journal")).unwrap();
        assert!(live.budget().is_err());
        std::fs::remove_file(dir.join("budget.sqlite3-journal")).unwrap();
        symlink(&dir, root.path().join("alias")).unwrap();
        assert!(
            LiveJev::open(&root.path().join("alias"), "fake".into(), 500, work.path()).is_err()
        );
        drop(live);
        std::fs::remove_file(&path).unwrap();
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE unknown(value TEXT)")
            .unwrap();
        drop(db);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(LiveJev::open(&dir, "fake".into(), 500, work.path()).is_err());
    }

    #[test]
    fn process_reservation_worker() {
        let Some(dir) = std::env::var_os("TRELLIS_TEST_LEDGER_DIR") else {
            return;
        };
        let work = std::env::var_os("TRELLIS_TEST_LEDGER_WORK").unwrap();
        let live = provider(Path::new(&dir), Path::new(&work), 500);
        for _ in 0..30 {
            let _ = live.reserve(b"synthetic process request");
        }
    }
    #[test]
    fn independent_processes_share_cap() {
        let dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let live = provider(dir.path(), work.path(), 500);
        let mut children: Vec<_> = (0..4)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "live_jev::tests::process_reservation_worker"])
                    .env("TRELLIS_TEST_LEDGER_DIR", dir.path())
                    .env("TRELLIS_TEST_LEDGER_WORK", work.path())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(live.budget().unwrap().reserved_cents, 500);
    }
    #[tokio::test]
    async fn cancellation_retains_pending_attempt() {
        use tokio::io::AsyncReadExt;
        let dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut live = provider(dir.path(), work.path(), 500);
        live.test_endpoint = Some(format!("http://{}/mock", listener.local_addr().unwrap()));
        let live = Arc::new(live);
        let copy = live.clone();
        let p = packet();
        let task = tokio::spawn(async move { copy.run(&p).await });
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut byte = [0];
        socket.read_exact(&mut byte).await.unwrap();
        task.abort();
        assert!(task.await.err().unwrap().is_cancelled());
        drop(socket);
        assert_eq!(live.budget().unwrap().reserved_cents, 10);
        let status: String = live
            .connect()
            .unwrap()
            .query_row("SELECT status FROM attempts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "reserved_incomplete");
    }

    #[test]
    fn deleted_or_truncated_database_never_restores_budget() {
        for truncate in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let work = tempfile::tempdir().unwrap();
            let dir = root.path().join("budget");
            let live = provider(&dir, work.path(), 500);
            live.reserve(b"synthetic").unwrap();
            drop(live);
            let db = dir.join("budget.sqlite3");
            if truncate {
                std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(db)
                    .unwrap();
            } else {
                std::fs::remove_file(db).unwrap();
            }
            assert!(LiveJev::open(&dir, "fake".into(), 500, work.path()).is_err());
        }
    }
    #[test]
    fn simultaneous_initial_open_is_safe() {
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let dir = root.path().join("budget");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                let work = work.path().to_owned();
                std::thread::spawn(move || LiveJev::open(&dir, "fake".into(), 500, &work).unwrap())
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap().budget().unwrap().attempts, 0);
        }
    }
}
