use serde_json::json;
use std::io::{self, Cursor};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;
use trellis::daemon::client::{self, CallError, StartOutcome};
use trellis::daemon::protocol::{self, Reply, Request};
use trellis::daemon::{SocketOwner, SocketPaths};

fn paths(temp: &tempfile::TempDir) -> SocketPaths {
    SocketPaths::new(&temp.path().join("trellis"))
}
fn request() -> Request {
    Request {
        id: 42,
        operation: "status".into(),
        payload: json!({"workspace":"/tmp/a"}),
    }
}

#[test]
fn election_preserves_private_socket_and_only_holder_recovers_stale_socket() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(&temp);
    let first = SocketOwner::acquire(&paths).unwrap().unwrap();
    assert_eq!(
        std::fs::metadata(&paths.run).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&paths.socket)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let inode = std::fs::metadata(&paths.socket).unwrap().ino();
    assert!(SocketOwner::acquire(&paths).unwrap().is_none());
    assert_eq!(std::fs::metadata(&paths.socket).unwrap().ino(), inode);
    drop(first);
    let stale = UnixListener::bind(&paths.socket).unwrap();
    drop(stale);
    let next = SocketOwner::acquire(&paths).unwrap().unwrap();
    // Inode numbers are recycled, so prove replacement by behavior: the stale
    // socket file has no listener, while the new owner accepts (#131).
    UnixStream::connect(&paths.socket).expect("new owner must replace the stale socket");
    drop(next);
    assert!(!paths.socket.exists());
}

#[test]
fn simultaneous_starters_elect_one_socket_owner() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(&temp);
    let barrier = Arc::new(Barrier::new(3));
    let owners: Vec<_> = (0..2)
        .map(|_| {
            let paths = paths.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                SocketOwner::acquire(&paths).unwrap()
            })
        })
        .collect();
    barrier.wait();
    let held: Vec<_> = owners
        .into_iter()
        .filter_map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(held.len(), 1);
    assert!(UnixStream::connect(&paths.socket).is_ok());
}

#[test]
fn bounded_frames_roundtrip_and_reject_oversized_input() {
    let mut frame = vec![];
    protocol::write_frame(&mut frame, &request()).unwrap();
    assert_eq!(
        protocol::read_frame::<Request>(&mut Cursor::new(frame)).unwrap(),
        request()
    );
    let mut oversized = Cursor::new(((protocol::MAX_FRAME + 1) as u32).to_be_bytes().to_vec());
    assert_eq!(
        protocol::read_frame::<Request>(&mut oversized)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(
        protocol::write_frame(
            &mut vec![],
            &json!({"data": "x".repeat(protocol::MAX_FRAME)})
        )
        .unwrap_err()
        .kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn interrupted_read_retries_once_but_ambiguous_mutation_is_not_replayed() {
    for read_only in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let received: Request = protocol::read_frame(&mut first).unwrap();
            assert_eq!(received.id, 42);
            drop(first); // Crash after receipt, before acknowledgement.
            if read_only {
                let (mut second, _) = listener.accept().unwrap();
                let retried: Request = protocol::read_frame(&mut second).unwrap();
                assert_eq!(retried, received);
                protocol::write_frame(
                    &mut second,
                    &Reply {
                        id: 42,
                        payload: json!("ok"),
                    },
                )
                .unwrap();
            }
        });
        let result = client::call(&request(), read_only, || {
            count.fetch_add(1, Ordering::SeqCst);
            UnixStream::connect(&path)
        });
        server.join().unwrap();
        if read_only {
            assert_eq!(result.unwrap().payload, "ok");
            assert_eq!(attempts.load(Ordering::SeqCst), 2);
        } else {
            assert_eq!(result.unwrap_err(), CallError::OutcomeUnknown);
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
        }
    }
}

#[test]
fn no_listener_is_typed_unavailable_and_demand_start_is_socket_only() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(&temp);
    let mut held = None;
    let owner = client::connect_or_start(
        &paths.socket,
        || {
            let guard = SocketOwner::acquire(&paths)?.unwrap();
            // Socket startup does not create an HTTP listener or checkout index.
            assert!(!paths.run.parent().unwrap().join("indexes").exists());
            held = Some(guard);
            Ok(StartOutcome::Started)
        },
        Duration::from_millis(200),
    );
    assert!(owner.is_ok());
    drop(owner);
    drop(held);
    assert_eq!(
        client::call(&request(), false, || Err(io::Error::new(
            io::ErrorKind::NotFound,
            "off"
        )))
        .unwrap_err(),
        CallError::DaemonUnavailable
    );
}

#[test]
fn nested_cold_start_does_not_activate_a_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let paths = SocketPaths::new(&temp.path().join("one/two/three/trellis"));
    let owner = SocketOwner::acquire(&paths).unwrap().unwrap();
    for dir in [
        temp.path().join("one"),
        temp.path().join("one/two"),
        temp.path().join("one/two/three"),
        paths.run.parent().unwrap().to_path_buf(),
        paths.run.clone(),
    ] {
        assert_eq!(
            std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert!(!paths.run.parent().unwrap().join("indexes").exists());
    assert!(UnixStream::connect(&paths.socket).is_ok());
    drop(owner);
}

#[test]
fn two_demand_starters_both_connect_after_one_election() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(&temp);
    let before_start = Arc::new(Barrier::new(3));
    let owners = Arc::new(Mutex::new(Vec::new()));
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let clients: Vec<_> = (0..2)
        .map(|_| {
            let paths = paths.clone();
            let before_start = before_start.clone();
            let owners = owners.clone();
            let outcomes = outcomes.clone();
            thread::spawn(move || {
                client::connect_or_start(
                    &paths.socket,
                    || {
                        // Both clients failed the initial connect before either begins election.
                        before_start.wait();
                        let outcome = match SocketOwner::acquire(&paths)? {
                            Some(owner) => {
                                owners.lock().unwrap().push(owner);
                                StartOutcome::Started
                            }
                            None => StartOutcome::ElectionInProgress,
                        };
                        outcomes.lock().unwrap().push(outcome);
                        Ok(outcome)
                    },
                    Duration::from_secs(2),
                )
            })
        })
        .collect();
    before_start.wait();
    let streams: Vec<_> = clients
        .into_iter()
        .map(|client| client.join().unwrap())
        .collect();
    assert!(
        streams.iter().all(Result::is_ok),
        "both starters must connect"
    );
    assert_eq!(owners.lock().unwrap().len(), 1);
    let outcomes = outcomes.lock().unwrap();
    assert!(outcomes.contains(&StartOutcome::Started));
    assert!(outcomes.contains(&StartOutcome::ElectionInProgress));
}
