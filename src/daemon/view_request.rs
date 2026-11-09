//! The latest agent "show" of one checkout (#818): what `trellis_show` asked open viewers to
//! display, and whether any viewer is open.
//!
//! The checkout runtime owns one [`ViewRequests`]. `trellis_show` records a validated target;
//! the browser's and the Herdr pane's status polls publish it as `viewRequest` and mark a
//! viewer as open. Nothing here is written to the index, the rules or saved state, and there is
//! no queue: a newer request replaces the older one.
use crate::model::IndexPin;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A status read this recent means a viewer of the checkout is open. The browser polls every
/// 5 s, the Herdr pane every 5 s (10 s on another machine); a hidden browser tab may be
/// throttled to one poll a minute.
pub const VIEWER_WINDOW: Duration = Duration::from_secs(90);

/// Generations are strictly increasing across every checkout of this process. They start at
/// the process's start time in milliseconds, so a restarted daemon's requests are newer than an
/// earlier daemon's that a long-lived viewer may have seen.
fn next_generation() -> u64 {
    static NEXT: std::sync::OnceLock<AtomicU64> = std::sync::OnceLock::new();
    NEXT.get_or_init(|| {
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |elapsed| elapsed.as_millis() as u64)
            .max(1);
        AtomicU64::new(start)
    })
    .fetch_add(1, Ordering::Relaxed)
}

/// A view an agent can show (#818 owner ruling: sequence, class, source).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewTarget {
    Sequence { symbol_id: String },
    Class { symbol_id: String },
    Source { path: String, line: u64 },
}

impl ViewTarget {
    pub fn view(&self) -> &'static str {
        match self {
            Self::Sequence { .. } => "sequence",
            Self::Class { .. } => "class",
            Self::Source { .. } => "source",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Recorded {
    generation: u64,
    target: ViewTarget,
    pin: IndexPin,
}

/// What [`ViewRequests::show`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    /// Recorded under this generation for the open viewers.
    Shown(u64),
    /// No viewer is open, so nothing was recorded.
    NoViewer,
}

#[derive(Default)]
struct State {
    latest: Option<Recorded>,
    viewer_seen: Option<Instant>,
}

/// One checkout's latest view request and viewer presence.
#[derive(Default)]
pub struct ViewRequests(Mutex<State>);

impl ViewRequests {
    /// A viewer read this checkout's status at `now`.
    pub fn note_viewer(&self, now: Instant) {
        let mut state = self.0.lock().unwrap();
        if state.viewer_seen.is_none_or(|seen| seen < now) {
            state.viewer_seen = Some(now);
        }
    }

    /// Records `target`, validated at `pin`, as the latest request if a viewer is open at `now`.
    pub fn show(&self, target: ViewTarget, pin: IndexPin, now: Instant) -> Shown {
        let mut state = self.0.lock().unwrap();
        let open = state
            .viewer_seen
            .is_some_and(|seen| now.saturating_duration_since(seen) <= VIEWER_WINDOW);
        if !open {
            return Shown::NoViewer;
        }
        let generation = next_generation();
        state.latest = Some(Recorded {
            generation,
            target,
            pin,
        });
        Shown::Shown(generation)
    }

    /// The status field `viewRequest`: the latest request, or null.
    pub fn status_json(&self) -> Value {
        let state = self.0.lock().unwrap();
        let Some(recorded) = &state.latest else {
            return Value::Null;
        };
        let revision = json!({
            "indexGeneration": recorded.pin.index_generation.to_string(),
            "indexRevision": recorded.pin.index_revision,
        });
        match &recorded.target {
            ViewTarget::Sequence { symbol_id } | ViewTarget::Class { symbol_id } => json!({
                "generation": recorded.generation,
                "view": recorded.target.view(),
                "symbolId": symbol_id,
                "revision": revision,
            }),
            ViewTarget::Source { path, line } => json!({
                "generation": recorded.generation,
                "view": "source",
                "path": path,
                "line": line,
                "revision": revision,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin() -> IndexPin {
        IndexPin {
            index_generation: uuid::Uuid::parse_str("12345678-1234-4123-8123-123456789abc")
                .unwrap(),
            index_revision: 3,
        }
    }
    fn sequence(id: &str) -> ViewTarget {
        ViewTarget::Sequence {
            symbol_id: id.to_owned(),
        }
    }

    #[test]
    fn no_viewer_records_nothing() {
        let requests = ViewRequests::default();
        let now = Instant::now();
        assert_eq!(requests.show(sequence("a"), pin(), now), Shown::NoViewer);
        assert_eq!(requests.status_json(), Value::Null);
        // A viewer seen longer ago than the window is not open either.
        requests.note_viewer(now);
        let later = now + VIEWER_WINDOW + Duration::from_millis(1);
        assert_eq!(requests.show(sequence("a"), pin(), later), Shown::NoViewer);
        assert_eq!(requests.status_json(), Value::Null);
    }

    #[test]
    fn each_show_replaces_the_last_under_a_newer_generation() {
        let requests = ViewRequests::default();
        let now = Instant::now();
        requests.note_viewer(now);
        let Shown::Shown(first) = requests.show(sequence("a"), pin(), now) else {
            panic!("a viewer is open")
        };
        let Shown::Shown(second) = requests.show(
            ViewTarget::Source {
                path: "src/a.rs".into(),
                line: 4,
            },
            pin(),
            now + VIEWER_WINDOW,
        ) else {
            panic!("still inside the window")
        };
        assert!(second > first, "{second} > {first}");
        assert_eq!(
            requests.status_json(),
            json!({"generation":second,"view":"source","path":"src/a.rs","line":4,
                "revision":{"indexGeneration":"12345678-1234-4123-8123-123456789abc","indexRevision":3}})
        );
        // Generations are process-wide: another checkout's request is newer still.
        let other = ViewRequests::default();
        other.note_viewer(now);
        let Shown::Shown(third) = other.show(
            ViewTarget::Class {
                symbol_id: "b".into(),
            },
            pin(),
            now,
        ) else {
            panic!("a viewer is open")
        };
        assert!(third > second);
        assert_eq!(other.status_json()["view"], "class");
        assert_eq!(other.status_json()["symbolId"], "b");
    }
}
