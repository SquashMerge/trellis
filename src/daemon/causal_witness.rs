//! Optional, bounded test-only causal witness stream for the MCP fixture.
//!
//! The producers never open a socket or wait for a reader. A single detached
//! task sends one JSON object per connection, in FIFO order. On a lost event or
//! transport error the stream stops: later ACKs must not certify a gap.
use crate::model::IndexPin;
#[cfg(feature = "test-support")]
use serde::{Serialize, Serializer};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[cfg(feature = "test-support")]
use std::{env, path::PathBuf, time::Duration};
use tokio::sync::mpsc;
#[cfg(feature = "test-support")]
use tokio::{io::AsyncWriteExt, net::UnixStream};
use uuid::Uuid;

#[cfg(feature = "test-support")]
const QUEUE_CAPACITY: usize = 64;
#[cfg(feature = "test-support")]
const MAX_RECORD_BYTES: usize = 512;
#[cfg(feature = "test-support")]
const IO_DEADLINE: Duration = Duration::from_millis(500);

/// The owner and watcher identities which make generation numbers comparable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lineage {
    pub owner_incarnation: Uuid,
    pub watch_epoch: Uuid,
    pub ordinal: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WatchAck {
    pub lineage: Lineage,
    pub generation: u64,
    pub pin: IndexPin,
    pub request_seq: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg(feature = "test-support")]
struct Event {
    kind: &'static str,
    root_key: String,
    #[serde(serialize_with = "serialize_uuid")]
    owner_incarnation: Uuid,
    #[serde(serialize_with = "serialize_uuid")]
    watch_epoch: Uuid,
    lineage_ordinal: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    watch_generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pin: Option<IndexPin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_empty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_seq: Option<i64>,
    #[serde(serialize_with = "serialize_uuid")]
    stream_id: Uuid,
    stream_seq: u64,
}

#[cfg(feature = "test-support")]
fn serialize_uuid<S: Serializer>(value: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

#[cfg_attr(not(feature = "test-support"), allow(dead_code))]
pub(crate) struct QueuedEvent {
    pub(crate) kind: &'static str,
    lineage: Lineage,
    pub(crate) watch_generation: Option<u64>,
    pub(crate) pin: Option<IndexPin>,
    pub(crate) queue_empty: Option<bool>,
    pub(crate) request_seq: Option<i64>,
}

/// Clone the `Arc<CausalWitness>` into each producer; it holds no root owner.
pub struct CausalWitness {
    sender: mpsc::Sender<QueuedEvent>,
    failed: Arc<AtomicBool>,
}

impl CausalWitness {
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn in_memory_for_tests() -> (Arc<Self>, mpsc::Receiver<QueuedEvent>) {
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        (
            Arc::new(Self {
                sender,
                failed: Arc::new(AtomicBool::new(false)),
            }),
            receiver,
        )
    }

    /// Activate only for an absolute socket directly inside the fixture HOME.
    /// Its test owns the private directory and socket; ordinary runs are inert.
    #[cfg(feature = "test-support")]
    pub fn from_env(root_key: String) -> Option<Arc<Self>> {
        let home = PathBuf::from(env::var_os("HOME")?);
        let socket = PathBuf::from(env::var_os("TRELLIS_TEST_MCP_CAUSAL_SOCKET")?);
        if !home.is_absolute() || !socket.is_absolute() || socket.parent()? != home {
            return None;
        }
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let failed = Arc::new(AtomicBool::new(false));
        let reporter = Arc::new(Self {
            sender,
            failed: failed.clone(),
        });
        handle.spawn(write_events(
            socket,
            root_key,
            Uuid::new_v4(),
            receiver,
            failed,
        ));
        Some(reporter)
    }

    pub fn h_ready(&self, lineage: Lineage, pin: IndexPin) {
        self.send(QueuedEvent {
            kind: "H_READY",
            lineage,
            watch_generation: None,
            pin: Some(pin),
            queue_empty: None,
            request_seq: None,
        });
    }

    pub fn watch_pending(&self, lineage: Lineage, generation: u64) {
        self.send(QueuedEvent {
            kind: "WATCH_PENDING",
            lineage,
            watch_generation: Some(generation),
            pin: None,
            queue_empty: None,
            request_seq: None,
        });
    }

    /// A nonempty FIFO is not acknowledged; emit only after inventory drains.
    pub fn watch_ack(
        &self,
        lineage: Lineage,
        generation: u64,
        pin: IndexPin,
        queue_empty: bool,
        request_seq: i64,
    ) {
        if !queue_empty {
            return;
        }
        self.send(QueuedEvent {
            kind: "WATCH_ACK",
            lineage,
            watch_generation: Some(generation),
            pin: Some(pin),
            queue_empty: Some(true),
            request_seq: Some(request_seq),
        });
    }

    fn send(&self, event: QueuedEvent) {
        if !self.failed.load(Ordering::Acquire) && self.sender.try_send(event).is_err() {
            self.failed.store(true, Ordering::Release);
        }
    }
}

#[cfg(feature = "test-support")]
async fn write_events(
    socket: PathBuf,
    root_key: String,
    stream_id: Uuid,
    mut receiver: mpsc::Receiver<QueuedEvent>,
    failed: Arc<AtomicBool>,
) {
    let mut stream_seq = 0u64;
    while let Some(queued) = receiver.recv().await {
        if failed.load(Ordering::Acquire) {
            send_error(&socket, &root_key, stream_id, stream_seq, queued.lineage).await;
            break;
        }
        let Some(next) = stream_seq.checked_add(1) else {
            failed.store(true, Ordering::Release);
            break;
        };
        stream_seq = next; // Sequence belongs to dequeue order, never producer scheduling.
        let event = Event {
            kind: queued.kind,
            root_key: root_key.clone(),
            owner_incarnation: queued.lineage.owner_incarnation,
            watch_epoch: queued.lineage.watch_epoch,
            lineage_ordinal: queued.lineage.ordinal,
            watch_generation: queued.watch_generation,
            pin: queued.pin,
            queue_empty: queued.queue_empty,
            request_seq: queued.request_seq,
            stream_id,
            stream_seq,
        };
        if failed.load(Ordering::Acquire) {
            send_error(
                &socket,
                &root_key,
                stream_id,
                stream_seq - 1,
                queued.lineage,
            )
            .await;
            break;
        }
        if !write_one(&socket, &event).await {
            failed.store(true, Ordering::Release);
            // The event may have been partially transmitted. ERROR is only a
            // best-effort signal; a missing or partial event itself cannot
            // certify readiness, and the stream is permanently stopped.
            send_error(&socket, &root_key, stream_id, stream_seq, queued.lineage).await;
            break;
        }
    }
}

#[cfg(feature = "test-support")]
async fn write_one(socket: &PathBuf, event: &Event) -> bool {
    let Ok(bytes) = serde_json::to_vec(event) else {
        return false;
    };
    if bytes.is_empty() || bytes.len() > MAX_RECORD_BYTES {
        return false;
    }
    // One bounded connection and one complete object. The fixture reads
    // until EOF, so shutdown and drop happen before the next event.
    matches!(
        tokio::time::timeout(IO_DEADLINE, async {
            let mut connection = UnixStream::connect(socket).await?;
            connection.write_all(&bytes).await?;
            connection.shutdown().await
        })
        .await,
        Ok(Ok(()))
    )
}

#[cfg(feature = "test-support")]
async fn send_error(
    socket: &PathBuf,
    root_key: &str,
    stream_id: Uuid,
    prior: u64,
    lineage: Lineage,
) {
    let Some(stream_seq) = prior.checked_add(1) else {
        return;
    };
    let error = Event {
        kind: "ERROR",
        root_key: root_key.to_owned(),
        owner_incarnation: lineage.owner_incarnation,
        watch_epoch: lineage.watch_epoch,
        lineage_ordinal: lineage.ordinal,
        watch_generation: None,
        pin: None,
        queue_empty: None,
        request_seq: None,
        stream_id,
        stream_seq,
    };
    let _ = write_one(socket, &error).await;
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;

    fn lineage() -> Lineage {
        Lineage {
            owner_incarnation: Uuid::new_v4(),
            watch_epoch: Uuid::new_v4(),
            ordinal: 1,
        }
    }
    fn pin() -> IndexPin {
        IndexPin {
            index_generation: Uuid::new_v4(),
            index_revision: 1,
        }
    }

    async fn receive_event(listener: &tokio::net::UnixListener) -> serde_json::Value {
        use tokio::io::AsyncReadExt;
        let (mut peer, _) = tokio::time::timeout(crate::testing::HANG_BACKSTOP, listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(crate::testing::HANG_BACKSTOP, peer.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(bytes.len() <= MAX_RECORD_BYTES);
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn writer_delivers_one_ordered_connection_per_event() {
        let home = tempfile::tempdir().unwrap();
        let socket = home.path().join("witness.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (sender, receiver) = mpsc::channel(4);
        let failed = Arc::new(AtomicBool::new(false));
        let reporter = CausalWitness {
            sender,
            failed: failed.clone(),
        };
        let writer = tokio::spawn(write_events(
            socket,
            "fixture".to_owned(),
            Uuid::new_v4(),
            receiver,
            failed.clone(),
        ));
        let owner = lineage();
        reporter.watch_pending(owner, 1);
        reporter.h_ready(owner, pin());
        reporter.watch_ack(owner, 1, pin(), true, 0);
        let mut stream_id = None;
        for (seq, kind) in [(1, "WATCH_PENDING"), (2, "H_READY"), (3, "WATCH_ACK")] {
            let event = receive_event(&listener).await;
            assert_eq!(event["kind"], kind);
            assert_eq!(event["streamSeq"], seq);
            assert_eq!(
                event["ownerIncarnation"],
                owner.owner_incarnation.to_string()
            );
            assert_eq!(event["watchEpoch"], owner.watch_epoch.to_string());
            if let Some(stream) = &stream_id {
                assert_eq!(&event["streamId"], stream);
            }
            stream_id = Some(event["streamId"].clone());
        }
        assert!(!failed.load(Ordering::Acquire));
        drop(reporter);
        tokio::time::timeout(crate::testing::HANG_BACKSTOP, writer)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn overflow_delivers_error_instead_of_stale_ack() {
        let home = tempfile::tempdir().unwrap();
        let socket = home.path().join("witness.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (sender, receiver) = mpsc::channel(1);
        let failed = Arc::new(AtomicBool::new(false));
        let reporter = CausalWitness {
            sender,
            failed: failed.clone(),
        };
        let owner = lineage();
        reporter.watch_pending(owner, 1);
        reporter.watch_pending(owner, 2); // deterministic backpressure before writer starts
        reporter.watch_ack(owner, 2, pin(), true, 0);
        assert!(failed.load(Ordering::Acquire));
        let writer = tokio::spawn(write_events(
            socket,
            "fixture".to_owned(),
            Uuid::new_v4(),
            receiver,
            failed,
        ));
        let event = receive_event(&listener).await;
        assert_eq!(event["kind"], "ERROR");
        assert_eq!(event["streamSeq"], 1);
        drop(reporter);
        tokio::time::timeout(crate::testing::HANG_BACKSTOP, writer)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn full_producer_queue_permanently_stops_later_ack() {
        let (sender, mut receiver) = mpsc::channel(1);
        let failed = Arc::new(AtomicBool::new(false));
        let reporter = CausalWitness {
            sender,
            failed: failed.clone(),
        };
        let owner = lineage();
        reporter.watch_pending(owner, 1);
        reporter.watch_pending(owner, 2); // overflow cannot silently discard this hint
        assert!(failed.load(Ordering::Acquire));
        reporter.watch_ack(owner, 2, pin(), true, 0);
        assert_eq!(receiver.try_recv().unwrap().kind, "WATCH_PENDING");
        assert!(receiver.try_recv().is_err(), "no ACK after a lost hint");
    }

    #[tokio::test]
    async fn missing_listener_stops_writer_without_later_ack() {
        let home = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::channel(2);
        let failed = Arc::new(AtomicBool::new(false));
        let reporter = CausalWitness {
            sender,
            failed: failed.clone(),
        };
        let owner = lineage();
        let writer = tokio::spawn(write_events(
            home.path().join("unbound.sock"),
            "fixture".to_owned(),
            Uuid::new_v4(),
            receiver,
            failed.clone(),
        ));
        reporter.watch_pending(owner, 1);
        tokio::time::timeout(crate::testing::HANG_BACKSTOP, writer)
            .await
            .unwrap()
            .unwrap();
        assert!(failed.load(Ordering::Acquire));
        reporter.watch_ack(owner, 1, pin(), true, 0);
        assert!(failed.load(Ordering::Acquire));
    }
}
