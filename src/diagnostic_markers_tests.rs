use super::*;
#[test]
fn bounded_marker_channel_reports_losses_without_blocking_writer() {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let markers = DiagnosticMarkers {
        sender,
        lost: std::sync::atomic::AtomicU64::new(0),
        origin: std::time::Instant::now(),
    };
    markers.emit("incarnation", "start", "kind=retention");
    markers.emit("incarnation", "end", "outcome=deferred");
    assert_eq!(markers.lost.load(Ordering::Acquire), 1);
    let first = receiver.try_recv().unwrap();
    assert!(first.contains("event=start"));
    assert!(first.contains("marker_lost=0"));
    markers.emit("incarnation", "end", "outcome=progress");
    let last = receiver.try_recv().unwrap();
    assert!(last.contains("event=end"));
    assert!(last.contains("marker_lost=1"));
    assert!(last.contains("monotonic_us="));
    assert!(last.contains("wall_ms="));
    assert_eq!(markers.lost.load(Ordering::Acquire), 0);
}
