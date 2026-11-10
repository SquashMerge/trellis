//! `trellis producer install <producer>` (#880): the only place Trellis runs a producer's
//! install command, and only on that explicit CLI command. It prints the pinned command, asks
//! for confirmation on a terminal unless `--yes` is given, runs exactly that command, and then
//! checks the producer again with `producer doctor`. It never installs a prerequisite. The
//! daemon, `producer plan`, `run`, `pickup`, auto-runs and every read never reach this module;
//! the producer spawn inventory test enforces that.

use super::ProducerName;
use super::detect::Declared;
use super::doctor::{self, ProducerReport, State};
use anyhow::{Result, bail};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

pub const PREREQUISITE_MISSING: &str = "prerequisite_missing";
pub const CONFIRMATION_REQUIRED: &str = "confirmation_required";
pub const INSTALL_DECLINED: &str = "install_declined";
pub const INSTALL_FAILED: &str = "install_failed";
pub const NO_INSTALL_COMMAND: &str = "no_install_command";

/// What `producer install` does once the command has been printed.
pub enum Confirmation {
    /// `--yes`: no prompt.
    Assumed,
    /// Ask on the terminal; refuse when stdin is not one.
    Ask,
}

/// Where the install command's stdout goes. Its stderr is always the CLI's stderr.
pub enum ChildOutput {
    Inherit,
    /// `--json`: the CLI's stdout carries only the JSON report.
    Stderr,
}

/// Run `producer`'s pinned install command under `declared` and return the doctor entry
/// afterwards. A producer that is already ready runs nothing. Progress goes to `out`.
pub fn run_pinned_install(
    producer: ProducerName,
    declared: &Declared,
    confirmation: Confirmation,
    child_output: ChildOutput,
    out: &mut dyn Write,
) -> Result<ProducerReport> {
    let before = doctor::check(producer, declared);
    if before.state == State::Ready {
        writeln!(
            out,
            "{} is already ready ({}); nothing was run.",
            producer.as_str(),
            before.pinned
        )?;
        return Ok(before);
    }
    let Some(command) = doctor::pinned_install(producer, &declared.home) else {
        bail!(
            "{NO_INSTALL_COMMAND}: {} has no pinned install command; nothing was run",
            producer.as_str()
        );
    };
    writeln!(out, "Pinned install command for {}:", producer.as_str())?;
    writeln!(out, "  {}", command.display())?;
    // The output directory the command writes into, created first when absent.
    let create = command
        .output()
        .and_then(Path::parent)
        .filter(|dir| !dir.is_dir())
        .map(Path::to_path_buf);
    if let Some(dir) = &create {
        writeln!(out, "  (creates {} first)", dir.display())?;
    }
    let missing: Vec<String> = before
        .prerequisites
        .iter()
        .filter(|need| !need.present && need.needed_for.contains(&"install"))
        .map(|need| format!("{} ({})", need.name, need.detail))
        .collect();
    if !missing.is_empty() {
        bail!(
            "{PREREQUISITE_MISSING}: {}; Trellis does not install prerequisites, nothing was run",
            missing.join("; ")
        );
    }
    let Some(program) = doctor::on_path(&command.argv[0], declared) else {
        bail!(
            "{PREREQUISITE_MISSING}: no {} on the declared PATH; nothing was run",
            command.argv[0]
        );
    };
    if let Confirmation::Ask = confirmation {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            bail!(
                "{CONFIRMATION_REQUIRED}: stdin is not a terminal; pass --yes to run the pinned \
                 command; nothing was run"
            );
        }
        let mut stderr = std::io::stderr();
        write!(stderr, "Run it? [y/N] ")?;
        stderr.flush()?;
        let mut answer = String::new();
        stdin.lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
            bail!("{INSTALL_DECLINED}: nothing was run");
        }
    }
    out.flush()?;
    if let Some(dir) = &create {
        std::fs::create_dir_all(dir)?;
    }
    let mut child = std::process::Command::new(&program);
    child.args(&command.argv[1..]);
    if let ChildOutput::Stderr = child_output {
        child.stdout(std::io::stderr());
    }
    if producer == ProducerName::RustAnalyzer {
        // Adding the component must never install the toolchain, a prerequisite.
        child.env("RUSTUP_AUTO_INSTALL", "0");
    }
    let status = child.status()?;
    if !status.success() {
        bail!(
            "{INSTALL_FAILED}: `{}` exited with {status}",
            command.display()
        );
    }
    let after = doctor::check(producer, declared);
    Ok(after)
}
