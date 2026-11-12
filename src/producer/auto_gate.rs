//! The gates on an automatic re-run (#660 AC10): a start is deferred while the host runs on
//! battery or its load is saturated. A reading that cannot be taken never gates. A gate is
//! looked at only when a producer would otherwise start, never during a run.

use super::auto_config::AutoConfig;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Gate {
    Battery,
    Load,
}

impl Gate {
    /// The `reason` of a producer deferred by this gate.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Battery => "deferred: battery",
            Self::Load => "deferred: load",
        }
    }
}

/// What the host reports. `None` is unknown, which does not gate.
pub(crate) trait Probes: Send + Sync {
    fn on_battery(&self) -> Option<bool>;
    fn load_saturated(&self) -> Option<bool>;
}

/// The first gate that holds `cfg`'s producer back: battery, then load.
pub(crate) fn check(cfg: &AutoConfig, probes: &dyn Probes) -> Option<Gate> {
    if cfg.battery_gate && probes.on_battery() == Some(true) {
        return Some(Gate::Battery);
    }
    if cfg.load_gate && probes.load_saturated() == Some(true) {
        return Some(Gate::Load);
    }
    None
}

/// The probes a daemon uses: the host's, or under `test-support` the file named by
/// `TRELLIS_TEST_AUTORUN_PROBES`.
pub(crate) fn current() -> Arc<dyn Probes> {
    #[cfg(feature = "test-support")]
    if let Some(path) = std::env::var_os("TRELLIS_TEST_AUTORUN_PROBES") {
        return Arc::new(FileProbes(path.into()));
    }
    Arc::new(SystemProbes)
}

/// Whether `load1` saturates `cpus` processors.
pub(crate) fn saturated(load1: f64, cpus: usize) -> bool {
    load1 >= cpus as f64
}

pub(crate) struct SystemProbes;

impl Probes for SystemProbes {
    fn on_battery(&self) -> Option<bool> {
        system_on_battery()
    }

    fn load_saturated(&self) -> Option<bool> {
        let mut load = [0f64; 3];
        // SAFETY: `load` has room for the one sample requested.
        let samples = unsafe { libc::getloadavg(load.as_mut_ptr(), 1) };
        if samples < 1 || !load[0].is_finite() {
            return None;
        }
        // SAFETY: `sysconf` takes no pointer.
        let online = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        let cpus = if online > 0 {
            online as usize
        } else {
            std::thread::available_parallelism().ok()?.get()
        };
        Some(saturated(load[0], cpus))
    }
}

#[cfg(target_os = "linux")]
fn system_on_battery() -> Option<bool> {
    power_from_dir(std::path::Path::new("/sys/class/power_supply"))
}

#[cfg(target_os = "macos")]
fn system_on_battery() -> Option<bool> {
    macos::providing_source().and_then(|source| classify_source(&source))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn system_on_battery() -> Option<bool> {
    None
}

/// Power from a `power_supply` directory: any `Mains` supply reading `online` 1 means on AC; the
/// host is on battery only when at least one `Mains` supply exists and every one reads `online` 0.
/// No `Mains` supply, or one whose `online` is missing, unreadable or neither 0 nor 1 (with none
/// online), is unknown. At most 64 entries and 64 bytes a file.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn power_from_dir(dir: &std::path::Path) -> Option<bool> {
    use std::io::Read;
    const ENTRIES: usize = 64;
    const BYTES: u64 = 64;
    let read = |path: std::path::PathBuf| -> Option<String> {
        let mut text = String::new();
        std::fs::File::open(path)
            .ok()?
            .take(BYTES)
            .read_to_string(&mut text)
            .ok()?;
        Some(text.trim().to_owned())
    };
    let mut mains = false;
    let mut unknown = false;
    for entry in std::fs::read_dir(dir).ok()?.take(ENTRIES).flatten() {
        let path = entry.path();
        if read(path.join("type")).as_deref() != Some("Mains") {
            continue;
        }
        mains = true;
        match read(path.join("online")).as_deref() {
            Some("1") => return Some(false),
            Some("0") => {}
            // Missing, unreadable or unrecognised: this supply's state is unknown, and an unknown
            // reading never gates (T09/AC10).
            _ => unknown = true,
        }
    }
    (mains && !unknown).then_some(true)
}

/// What macOS's providing power source type says: `Some(true)` on battery.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn classify_source(source: &str) -> Option<bool> {
    match source {
        "Battery Power" => Some(true),
        "AC Power" | "UPS Power" => Some(false),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{c_char, c_void};

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> *const c_void;
        fn IOPSGetProvidingPowerSourceType(snapshot: *const c_void) -> *const c_void;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringGetCString(
            string: *const c_void,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(object: *const c_void);
    }

    /// kCFStringEncodingUTF8.
    const UTF8: u32 = 0x0800_0100;

    /// The providing power source type, for example `AC Power`.
    pub(super) fn providing_source() -> Option<String> {
        let mut buffer = [0 as c_char; 64];
        // SAFETY: the snapshot is owned (Copy rule) and released once. The type string follows
        // the Get rule, so it is read while the snapshot lives and never released. The buffer's
        // length is passed, so the copy cannot overrun it.
        unsafe {
            let snapshot = IOPSCopyPowerSourcesInfo();
            if snapshot.is_null() {
                return None;
            }
            let kind = IOPSGetProvidingPowerSourceType(snapshot);
            let copied = !kind.is_null()
                && CFStringGetCString(kind, buffer.as_mut_ptr(), buffer.len() as isize, UTF8) != 0;
            CFRelease(snapshot);
            if !copied {
                return None;
            }
        }
        let bytes: Vec<u8> = buffer
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        String::from_utf8(bytes).ok()
    }
}

/// The test-support probes: `{"battery": bool|null, "load": bool|null}` in a file that is read at
/// every check. A missing or unreadable file means unknown.
#[cfg(any(test, feature = "test-support"))]
pub(crate) struct FileProbes(pub std::path::PathBuf);

#[cfg(any(test, feature = "test-support"))]
impl FileProbes {
    fn read(&self) -> (Option<bool>, Option<bool>) {
        std::fs::read_to_string(&self.0)
            .map(|text| parse_probe_file(&text))
            .unwrap_or_default()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Probes for FileProbes {
    fn on_battery(&self) -> Option<bool> {
        self.read().0
    }

    fn load_saturated(&self) -> Option<bool> {
        self.read().1
    }
}

/// `(battery, load)` of a probe file; anything but a boolean is unknown.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn parse_probe_file(text: &str) -> (Option<bool>, Option<bool>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return (None, None);
    };
    (
        value.get("battery").and_then(serde_json::Value::as_bool),
        value.get("load").and_then(serde_json::Value::as_bool),
    )
}
