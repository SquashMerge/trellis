//! Optional maintenance markers on stderr, enabled by `TRELLIS_INDEX_DIAGNOSTICS=1`.
use std::sync::atomic::Ordering;

// Optional diagnostics never write on the publication or maintenance thread.
// A full bounded channel increments a loss counter rather than blocking work.
struct DiagnosticMarkers {
    sender: std::sync::mpsc::SyncSender<String>,
    lost: std::sync::atomic::AtomicU64,
    origin: std::time::Instant,
}
impl DiagnosticMarkers {
    fn emit(&self, incarnation: &str, event: &str, detail: &str) {
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.as_millis());
        let elapsed = self.origin.elapsed().as_micros();
        let lost = self.lost.swap(0, Ordering::AcqRel);
        let message = format!(
            "maintenance wall_ms={wall} monotonic_us={elapsed} incarnation={incarnation} event={event} detail={detail} marker_lost={lost}\n"
        );
        if self.sender.try_send(message).is_err() {
            self.lost.fetch_add(lost + 1, Ordering::AcqRel);
        }
    }
}

fn diagnostics() -> Option<&'static DiagnosticMarkers> {
    static MARKERS: std::sync::OnceLock<Option<DiagnosticMarkers>> = std::sync::OnceLock::new();
    MARKERS
        .get_or_init(|| {
            if std::env::var("TRELLIS_INDEX_DIAGNOSTICS").as_deref() != Ok("1") {
                return None;
            }
            let (sender, receiver) = std::sync::mpsc::sync_channel::<String>(256);
            std::thread::Builder::new()
                .name("trellis-diagnostic-writer".into())
                .spawn(move || {
                    use std::io::Write;
                    while let Ok(line) = receiver.recv() {
                        // Never hold stderr's global lock while waiting for
                        // the next marker: CLI publication diagnostics and
                        // fixture barriers use stderr on other threads.
                        let mut out = std::io::stderr().lock();
                        let _ = out.write_all(line.as_bytes());
                        let _ = out.flush();
                    }
                })
                .ok()?;
            Some(DiagnosticMarkers {
                sender,
                lost: std::sync::atomic::AtomicU64::new(0),
                origin: std::time::Instant::now(),
            })
        })
        .as_ref()
}
pub(crate) fn diagnostics_enabled() -> bool {
    diagnostics().is_some()
}
pub(crate) fn diagnostic_marker(incarnation: &str, event: &str, detail: &str) {
    if let Some(markers) = diagnostics() {
        markers.emit(incarnation, event, detail);
    }
}

#[cfg(test)]
#[path = "diagnostic_markers_tests.rs"]
mod tests;
