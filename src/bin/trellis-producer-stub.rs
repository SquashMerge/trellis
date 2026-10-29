//! Test-support stand-in for a semantic producer. Its behavior comes from a
//! JSON mode file named by `--mode-file <abs>`, or by the first argument when
//! that ends in `.json` (the stub then stands in for `node`). Keys, all
//! optional, run in this order:
//!
//! - `marker`: file that receives argv before anything else;
//! - `envdump`: file that receives `{env, argv, cwd, which}`;
//! - `which`: names searched on the stub's own `PATH`, recorded in the dump;
//! - `stderr`: number of lines to write to stderr;
//! - `alloc`: MiB to allocate and touch;
//! - `grow`: bytes to write to the `--output` file;
//! - `emit`: SCIP file to copy to the `--output` file;
//! - `tree`: pidfile; spawn two sleeping descendants and record every pid;
//! - then one of `sleep` (forever), `abort`, or `exit` (code, default 0).
//!
//! `--sleep-forever` is the descendant mode used by `tree`.

use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--sleep-forever") {
        sleep_forever();
    }
    let mode_file = match args.get(1).map(String::as_str) {
        Some("--mode-file") => args.get(2).cloned(),
        Some(first) if first.ends_with(".json") => Some(first.to_owned()),
        _ => None,
    };
    let mode: Value = mode_file
        .map(|path| serde_json::from_slice(&fs::read(path).expect("mode file")).expect("mode"))
        .unwrap_or_else(|| json!({}));
    let output = args
        .iter()
        .position(|arg| arg == "--output")
        .and_then(|at| args.get(at + 1))
        .map(PathBuf::from);

    if let Some(marker) = mode["marker"].as_str() {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(marker)
            .expect("marker");
        writeln!(file, "{}", args.join(" ")).expect("marker");
    }
    if let Some(dump) = mode["envdump"].as_str() {
        let env: serde_json::Map<String, Value> =
            std::env::vars().map(|(k, v)| (k, json!(v))).collect();
        let which: serde_json::Map<String, Value> = mode["which"]
            .as_str()
            .unwrap_or("")
            .split(',')
            .filter(|name| !name.is_empty())
            .map(|name| (name.to_owned(), json!(find_on_path(name))))
            .collect();
        let report = json!({
            "env": env,
            "argv": args,
            "cwd": std::env::current_dir().ok(),
            "which": which,
        });
        fs::write(dump, serde_json::to_vec(&report).expect("dump")).expect("envdump");
    }
    if let Some(lines) = mode["stderr"].as_u64() {
        let mut stderr = std::io::stderr();
        for line in 0..lines {
            let _ = writeln!(stderr, "stub stderr line {line}");
        }
    }
    let _held = mode["alloc"].as_u64().map(|mib| {
        let block = vec![1_u8; mib as usize * 1024 * 1024];
        std::hint::black_box(block)
    });
    if let Some(bytes) = mode["grow"].as_u64() {
        let path = output.as_deref().expect("--output for grow");
        fs::write(path, vec![0_u8; bytes as usize]).expect("grow");
    }
    if let Some(source) = mode["emit"].as_str() {
        let path = output.as_deref().expect("--output for emit");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("output directory");
        }
        fs::copy(source, path).expect("emit");
    }
    if let Some(pidfile) = mode["tree"].as_str() {
        let exe = std::env::current_exe().expect("current_exe");
        let mut pids = vec![std::process::id()];
        for _ in 0..2 {
            // The descendants outlive this process on purpose.
            #[allow(clippy::zombie_processes)]
            let child = Command::new(&exe)
                .arg("--sleep-forever")
                .spawn()
                .expect("descendant");
            pids.push(child.id());
        }
        let text: String = pids.iter().map(|pid| format!("{pid}\n")).collect();
        fs::write(pidfile, text).expect("pidfile");
    }
    if mode["sleep"].as_bool() == Some(true) {
        sleep_forever();
    }
    if mode["abort"].as_bool() == Some(true) {
        std::process::abort();
    }
    std::process::exit(mode["exit"].as_i64().unwrap_or(0) as i32);
}

fn sleep_forever() -> ! {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

fn find_on_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
        .map(|found| found.display().to_string())
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}
