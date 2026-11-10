//! Real scip-java through the restricted profile. The default variant skips with
//! `SKIP producer_unavailable: <reason>` when scip-java or its offline JDK cache is not
//! provisioned (CI). The strict `#[ignore]` variant fails instead and prints an
//! `E2E-EVIDENCE` line on pass. Nothing is installed.
#![cfg(feature = "test-support")]

use crate::real;
use real::{Availability, Case};

const JAVA: Case = Case {
    producer: "scip-java",
    fixture: "java",
    extension: "java",
    trust: "restricted",
};

#[test]
fn scip_java_restricted_end_to_end() {
    real::run_case(&JAVA, Availability::SkipIfUnavailable);
}

#[test]
#[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
fn scip_java_restricted_end_to_end_strict() {
    assert!(real::run_case(&JAVA, Availability::Required).is_some());
}

/// #717: the build tool the trusted plan derives is the one the pinned scip-java detects. Each
/// case runs the launcher (from a restricted plan, so `COURSIER_MODE=offline`) in a directory
/// holding only those build files, with `--build-tool trellis-probe`: that matches no tool, so
/// scip-java reports what it detected and runs no build.
fn build_tool_detection(availability: Availability) -> Option<String> {
    use std::{fs, process::Command};
    use trellis::producer::{
        TrustProfile,
        profiles::Rules,
        settings,
        worktree::{RetainedInputs, RootInput},
    };
    let checkout = crate::support::start_checkout(
        |root| real::copy_tree(&real::fixture_dir("java"), root),
        &[],
    );
    let plan = availability.resolve(
        "scip-java",
        real::plan(&checkout, "scip-java", "restricted", &[]),
    )?;
    let launcher = plan["command"]["argv"][0].as_str().unwrap().to_owned();
    let env = plan["command"]["env"].as_object().unwrap().clone();
    let cases: [&[&str]; 8] = [
        &[],
        &["pom.xml"],
        &["build.gradle"],
        &["build.gradle.kts"],
        &["settings.gradle"],
        &["settings.gradle.kts"],
        &["gradle.properties"],
        &["pom.xml", "build.gradle"],
    ];
    for files in cases {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("w");
        for sub in ["w", "home", "tmp"] {
            fs::create_dir(dir.path().join(sub)).unwrap();
        }
        for file in files {
            fs::write(root.join(file), "\n").unwrap();
        }
        let mut command = Command::new(&launcher);
        command
            .args(["index", "--output"])
            .arg(dir.path().join("index.scip"))
            .args(["--build-tool", "trellis-probe"])
            .current_dir(&root)
            .env_clear();
        for (name, value) in &env {
            command.env(name, value.as_str().unwrap());
        }
        command
            .env("HOME", dir.path().join("home"))
            .env("TMPDIR", dir.path().join("tmp"));
        let output = command.output().unwrap();
        let text = String::from_utf8_lossy(&[output.stdout, output.stderr].concat())
            .split('\u{1b}')
            .map(|part| match part.find('m') {
                Some(end) if part.starts_with('[') => &part[end + 1..],
                _ => part,
            })
            .collect::<String>();
        let detected: Vec<&str> = if text.contains("No build tool detected") {
            Vec::new()
        } else {
            let (_, rest) = text
                .split_once("Automatically detected the build tool(s) ")
                .unwrap_or_else(|| panic!("{files:?}: unexpected scip-java output: {text}"));
            let (list, _) = rest.split_once(" but none").unwrap();
            list.split(", ").collect()
        };
        let inputs = RetainedInputs {
            config: files
                .iter()
                .map(|file| RootInput {
                    path: (*file).into(),
                    bytes: b"\n".to_vec(),
                })
                .collect(),
            dependencies: Vec::new(),
        };
        let derived = settings::java(&mut Rules::default(), &inputs, TrustProfile::Trusted, None);
        match (detected.as_slice(), derived) {
            ([], Ok(settings)) => assert!(settings[0].value.is_null(), "{files:?}"),
            ([tool], Ok(settings)) => assert_eq!(settings[0].value, *tool, "{files:?}"),
            ([_, _, ..], Err(error)) => {
                assert!(
                    error.to_string().contains("select Maven and Gradle"),
                    "{error}"
                )
            }
            (detected, derived) => {
                panic!("{files:?}: scip-java detected {detected:?}, the plan derived {derived:?}")
            }
        }
    }
    let line = format!(
        "E2E-EVIDENCE producer=scip-java buildToolDetection=matched cases={}",
        cases.len()
    );
    println!("{line}");
    Some(line)
}

#[test]
fn scip_java_build_tool_detection_matches_the_plan() {
    build_tool_detection(Availability::SkipIfUnavailable);
}

#[test]
#[ignore = "strict real-producer evidence: fails when the producer is unavailable"]
fn scip_java_build_tool_detection_matches_the_plan_strict() {
    assert!(build_tool_detection(Availability::Required).is_some());
}
