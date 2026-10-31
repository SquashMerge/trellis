//! Producer detection, version verification (D3) and the declared environment
//! (D12). Every check is a file read or a hash: nothing here executes a
//! producer, a runtime, rustup, node or java.

use super::profiles::{Stub, Target};
use super::{ProducerName, TrustProfile};
use crate::scip_admission::PinnedProducer;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::{self, File},
    io::Read,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const MAX_PATH_BYTES: usize = 8192;
const MAX_PATH_ENTRIES: usize = 128;
const MAX_VALUE_BYTES: usize = 4096;
const MAX_PACKAGE_JSON_BYTES: u64 = 1024 * 1024;
/// Ancestors of an npm launcher searched for its package manifest.
const PACKAGE_ANCESTORS: usize = 4;
/// ELF, Mach-O 64-bit (both byte orders) and fat Mach-O.
const NATIVE_MAGICS: [[u8; 4]; 4] = [
    [0x7f, b'E', b'L', b'F'],
    [0xfe, 0xed, 0xfa, 0xcf],
    [0xcf, 0xfa, 0xed, 0xfe],
    [0xca, 0xfe, 0xba, 0xbe],
];

/// The environment the CLI declares for resolution (D12). The daemon never
/// reads its own `PATH` or `HOME` for a producer.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    #[serde(rename = "PATH")]
    pub path: String,
    #[serde(rename = "HOME")]
    pub home: String,
    #[serde(
        rename = "RUSTUP_HOME",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub rustup_home: Option<String>,
    #[serde(
        rename = "CARGO_HOME",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub cargo_home: Option<String>,
    #[serde(
        rename = "COURSIER_CACHE",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub coursier_cache: Option<String>,
    #[serde(
        rename = "COURSIER_ARCHIVE_CACHE",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub coursier_archive_cache: Option<String>,
}

fn declared_value(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => anyhow::bail!("invalid_request: environment {name} is not UTF-8"),
    }
}

impl Environment {
    /// The calling process's declaration, as the CLI sends it.
    pub fn from_process() -> Result<Self> {
        let environment = Self {
            path: declared_value("PATH")?.unwrap_or_default(),
            home: declared_value("HOME")?
                .ok_or_else(|| anyhow::anyhow!("invalid_request: environment HOME is not set"))?,
            rustup_home: declared_value("RUSTUP_HOME")?,
            cargo_home: declared_value("CARGO_HOME")?,
            coursier_cache: declared_value("COURSIER_CACHE")?,
            coursier_archive_cache: declared_value("COURSIER_ARCHIVE_CACHE")?,
        };
        environment.validate()?;
        Ok(environment)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.path.len() <= MAX_PATH_BYTES && self.path.split(':').count() <= MAX_PATH_ENTRIES,
            "invalid_request: environment PATH bounds"
        );
        let absolute = |name: &str, value: &str| -> Result<()> {
            ensure!(
                value.len() <= MAX_VALUE_BYTES && Path::new(value).is_absolute(),
                "invalid_request: environment {name}"
            );
            Ok(())
        };
        absolute("HOME", &self.home)?;
        for (name, value) in [
            ("RUSTUP_HOME", &self.rustup_home),
            ("CARGO_HOME", &self.cargo_home),
            ("COURSIER_CACHE", &self.coursier_cache),
            ("COURSIER_ARCHIVE_CACHE", &self.coursier_archive_cache),
        ] {
            if let Some(value) = value {
                absolute(name, value)?;
            }
        }
        Ok(())
    }
}

/// The validated declaration with the defaults of D12 applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Declared {
    /// Absolute `PATH` directories in order; empty and relative entries are
    /// never resolved against the daemon's working directory.
    pub path_dirs: Vec<PathBuf>,
    pub home: PathBuf,
    pub rustup_home: PathBuf,
    pub cargo_home: PathBuf,
    pub coursier_cache: PathBuf,
    pub coursier_archive_cache: PathBuf,
}

impl Declared {
    pub fn new(environment: &Environment) -> Result<Self> {
        environment.validate()?;
        let home = PathBuf::from(&environment.home);
        let pick = |value: &Option<String>, default: PathBuf| {
            value.as_deref().map_or(default, PathBuf::from)
        };
        let coursier = if cfg!(target_os = "macos") {
            home.join("Library/Caches/Coursier")
        } else {
            home.join(".cache/coursier")
        };
        Ok(Self {
            path_dirs: environment
                .path
                .split(':')
                .filter(|entry| Path::new(entry).is_absolute())
                .map(PathBuf::from)
                .collect(),
            rustup_home: pick(&environment.rustup_home, home.join(".rustup")),
            cargo_home: pick(&environment.cargo_home, home.join(".cargo")),
            coursier_cache: pick(&environment.coursier_cache, coursier.join("v1")),
            coursier_archive_cache: pick(&environment.coursier_archive_cache, coursier.join("arc")),
            home,
        })
    }
}

/// Why a producer cannot be planned: the text after `producer_unavailable: `.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Unavailable(pub String);

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unavailable {}

impl Unavailable {
    pub fn into_error(self) -> anyhow::Error {
        anyhow::anyhow!("{}: {}", super::PRODUCER_UNAVAILABLE, self.0)
    }
}

fn unavailable<T>(reason: impl Into<String>) -> Result<T, Unavailable> {
    Err(Unavailable(reason.into()))
}

/// An executable found by resolution and its content hash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Exe {
    pub path: PathBuf,
    pub sha256: String,
    /// The declared `PATH` directory it was found in (not canonicalized), or
    /// its parent for an explicit `--executable`.
    pub dir: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Verification {
    /// `executableSha256`, `packageManifestVersion` or `testStub`.
    pub method: &'static str,
    pub observed: String,
}

/// Everything planning learned about one producer without running it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resolution {
    /// The producer's launcher: scip-java, the npm entry point `main.js`, the
    /// rustup proxy or the test stub.
    pub launcher: PathBuf,
    pub verification: Verification,
    pub node: Option<Exe>,
    pub java: Option<Exe>,
    /// The JDK home when `<home>/release` exists.
    pub java_home: Option<PathBuf>,
    pub rustup: Option<Exe>,
    pub python3: Option<Exe>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeClaim {
    /// Descriptor provenance claims it as observed.
    Runtime,
    NotClaimed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeExecutable {
    pub name: String,
    pub path: String,
    pub sha256: String,
    pub generation: RuntimeClaim,
}

impl Resolution {
    /// Runtime executables for inspection, sorted by name.
    pub fn runtime_executables(&self) -> Vec<RuntimeExecutable> {
        let mut out = Vec::new();
        let mut push = |name: &str, exe: &Option<Exe>, generation| {
            if let Some(exe) = exe {
                out.push(RuntimeExecutable {
                    name: name.to_owned(),
                    path: exe.path.to_string_lossy().into_owned(),
                    sha256: exe.sha256.clone(),
                    generation,
                });
            }
        };
        push("java", &self.java, RuntimeClaim::NotClaimed);
        push("node", &self.node, RuntimeClaim::Runtime);
        push("python3", &self.python3, RuntimeClaim::NotClaimed);
        push("rustup", &self.rustup, RuntimeClaim::NotClaimed);
        out
    }
}

fn utf8(path: &Path) -> Result<(), Unavailable> {
    if path.to_str().is_none() {
        return unavailable("path is not UTF-8");
    }
    Ok(())
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The first declared directory holding an executable `name`.
fn find_on_path(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .find(|dir| is_executable_file(&dir.join(name)))
        .cloned()
}

fn canonical(path: &Path) -> Result<PathBuf, Unavailable> {
    let resolved = fs::canonicalize(path)
        .map_err(|_| Unavailable(format!("cannot resolve {}", path.display())))?;
    utf8(&resolved)?;
    Ok(resolved)
}

fn sha256_file(path: &Path) -> Result<String, Unavailable> {
    let unreadable = |_| Unavailable(format!("cannot read {}", path.display()));
    let mut file = File::open(path).map_err(unreadable)?;
    if !file.metadata().map_err(unreadable)?.is_file() {
        return unavailable(format!("{} is not a regular file", path.display()));
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(unreadable)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn exe(path: PathBuf, dir: PathBuf) -> Result<Exe, Unavailable> {
    utf8(&path)?;
    utf8(&dir)?;
    let sha256 = sha256_file(&path)?;
    Ok(Exe { path, sha256, dir })
}

/// The first `name` on the declared `PATH`, canonicalized and hashed.
fn runtime_on_path(name: &str, declared: &Declared) -> Result<Exe, Unavailable> {
    let Some(dir) = find_on_path(name, &declared.path_dirs) else {
        return unavailable(format!("no {name} on the declared PATH"));
    };
    exe(canonical(&dir.join(name))?, dir)
}

struct Package {
    name: String,
    version: String,
}

/// `package.json` of `dir`: no-follow, at most 1 MiB, closed parse of `name`
/// and `version`. Anything else is not a package manifest.
fn read_package(dir: &Path) -> Option<Package> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("package.json"))
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_PACKAGE_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_PACKAGE_JSON_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some(Package {
        name: value.get("name")?.as_str()?.to_owned(),
        version: value.get("version")?.as_str()?.to_owned(),
    })
}

fn is_native_executable(path: &Path) -> bool {
    let mut magic = [0_u8; 4];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && NATIVE_MAGICS.contains(&magic)
}

fn npm_package(producer: ProducerName) -> &'static str {
    match producer {
        ProducerName::ScipPython => "@sourcegraph/scip-python",
        _ => "@sourcegraph/scip-typescript",
    }
}

fn resolve_npm(
    producer: ProducerName,
    pin: &PinnedProducer,
    declared: &Declared,
    executable: Option<&Path>,
) -> Result<Resolution, Unavailable> {
    let (launcher, launcher_dir) = match executable {
        Some(path) => (canonical(path)?, None),
        None => {
            let name = producer.as_str();
            let Some(dir) = find_on_path(name, &declared.path_dirs) else {
                return unavailable(format!("no {name} on the declared PATH"));
            };
            (canonical(&dir.join(name))?, Some(dir))
        }
    };
    let wanted = npm_package(producer);
    let package = launcher
        .ancestors()
        .skip(1)
        .take(PACKAGE_ANCESTORS)
        .filter_map(read_package)
        .find(|package| package.name == wanted);
    let Some(package) = package else {
        return unavailable("launcher is not an npm package entry point (shim?)");
    };
    if package.version != pin.version {
        return unavailable(format!(
            "version {} != pinned {}",
            package.version, pin.version
        ));
    }
    // npm -g and mise installs keep `node` next to the launcher: prefer it.
    let node_dir = match launcher_dir {
        Some(dir) if dir.join("node").exists() => dir,
        _ => find_on_path("node", &declared.path_dirs)
            .ok_or_else(|| Unavailable("no node on the declared PATH".into()))?,
    };
    let node_path = canonical(&node_dir.join("node"))?;
    if !is_native_executable(&node_path) {
        return unavailable("node is not a native executable (shim?)");
    }
    Ok(Resolution {
        launcher,
        verification: Verification {
            method: "packageManifestVersion",
            observed: package.version,
        },
        node: Some(exe(node_path, node_dir)?),
        java: None,
        java_home: None,
        rustup: None,
        python3: None,
    })
}

fn resolve_java(
    pin: &PinnedProducer,
    declared: &Declared,
    executable: Option<&Path>,
) -> Result<Resolution, Unavailable> {
    let launcher = match executable {
        Some(path) => canonical(path)?,
        None => {
            let Some(dir) = find_on_path("scip-java", &declared.path_dirs) else {
                return unavailable("no scip-java on the declared PATH");
            };
            canonical(&dir.join("scip-java"))?
        }
    };
    let observed = sha256_file(&launcher)?;
    if observed != pin.executable_hash {
        return unavailable(format!(
            "executable sha256 {} != pinned {}",
            short(&observed),
            short(&pin.executable_hash)
        ));
    }
    let java = runtime_on_path("java", declared)?;
    let java_home = java
        .path
        .parent()
        .and_then(Path::parent)
        .filter(|home| home.join("release").is_file())
        .map(Path::to_path_buf);
    Ok(Resolution {
        launcher,
        verification: Verification {
            method: "executableSha256",
            observed,
        },
        node: None,
        java: Some(java),
        java_home,
        rustup: None,
        python3: None,
    })
}

/// The toolchain triple of the host, for hosts a rust-analyzer pin exists on.
pub(crate) fn host_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64" | "x86_64") => Some(if std::env::consts::ARCH == "aarch64" {
            "aarch64-apple-darwin"
        } else {
            "x86_64-apple-darwin"
        }),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        _ => None,
    }
}

/// The toolchain name rustup runs the pinned rust-analyzer on, `1.98.1` for
/// the pin version `1.98.1 (48a229ce 2026-09-01)`.
pub fn rustup_toolchain(pin: &PinnedProducer) -> &str {
    pin.version.split(' ').next().unwrap_or(&pin.version)
}

fn resolve_rust(
    pin: &PinnedProducer,
    declared: &Declared,
    executable: Option<&Path>,
) -> Result<Resolution, Unavailable> {
    let Some(triple) = host_triple() else {
        return unavailable("no rust-analyzer for host");
    };
    let rustup = match executable {
        Some(path) => {
            let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
            exe(path.to_path_buf(), dir)?
        }
        None => {
            let Some(dir) = find_on_path("rustup", &declared.path_dirs) else {
                return unavailable("no rustup on the declared PATH");
            };
            exe(dir.join("rustup"), dir)?
        }
    };
    let real = declared
        .rustup_home
        .join("toolchains")
        .join(format!("{}-{triple}", rustup_toolchain(pin)))
        .join("bin/rust-analyzer");
    if !is_executable_file(&real) {
        return unavailable(format!("no rust-analyzer for host at {}", real.display()));
    }
    let observed = sha256_file(&canonical(&real)?)?;
    if observed != pin.executable_hash {
        return unavailable(format!(
            "executable sha256 {} != pinned {}",
            short(&observed),
            short(&pin.executable_hash)
        ));
    }
    Ok(Resolution {
        launcher: rustup.path.clone(),
        verification: Verification {
            method: "executableSha256",
            observed,
        },
        node: None,
        java: None,
        java_home: None,
        rustup: Some(rustup),
        python3: None,
    })
}

fn resolve_stub(pin: &PinnedProducer, stub: Option<&Stub>) -> Result<Resolution, Unavailable> {
    let Some(stub) = stub else {
        return unavailable("test producer is not registered");
    };
    Ok(Resolution {
        launcher: canonical(&stub.path)?,
        verification: Verification {
            method: "testStub",
            observed: pin.version.clone(),
        },
        node: None,
        java: None,
        java_home: None,
        rustup: None,
        python3: None,
    })
}

/// Resolve and verify `target` independent of the trust profile.
pub fn resolve_base(
    target: Target,
    pin: &PinnedProducer,
    declared: &Declared,
    executable: Option<&Path>,
    stub: Option<&Stub>,
) -> Result<Resolution, Unavailable> {
    match target {
        Target::Stub => resolve_stub(pin, stub),
        Target::Real(ProducerName::ScipJava) => resolve_java(pin, declared, executable),
        Target::Real(ProducerName::RustAnalyzer) => resolve_rust(pin, declared, executable),
        Target::Real(producer) => resolve_npm(producer, pin, declared, executable),
    }
}

/// Resolve everything the row for `trust` needs: trusted scip-python also
/// needs the `python3` it will expose.
pub fn resolve(
    target: Target,
    trust: TrustProfile,
    pin: &PinnedProducer,
    declared: &Declared,
    executable: Option<&Path>,
    stub: Option<&Stub>,
) -> Result<Resolution, Unavailable> {
    let mut resolution = resolve_base(target, pin, declared, executable, stub)?;
    if target == Target::Real(ProducerName::ScipPython) && trust == TrustProfile::Trusted {
        resolution.python3 = Some(runtime_on_path("python3", declared)?);
    }
    Ok(resolution)
}
