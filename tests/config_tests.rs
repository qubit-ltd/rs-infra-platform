// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
use std::fs;
use std::path::Path;
use std::process::Command;
use std::process::Output;

use tempfile::TempDir;

const VALID: &str = r#"version = 1
[[suites]]
id = "default"
platforms = ["linux", "windows", "macos", "android"]
timeout_seconds = 1200
minimum_tests = 1
requirements = []
data = []
[[suites.commands]]
kind = "test"
argv = ["never-execute-this-command", "test", "--locked"]
cwd = "."
"#;

fn project(config: &str) -> TempDir {
    let root = tempfile::tempdir().expect("create fixture project");
    fs::create_dir_all(root.path().join(".infra/platform")).expect("create config directory");
    fs::write(root.path().join(".infra/platform/platform.toml"), config).expect("write fixture config");
    root
}

fn run(root: &Path, args: &[&str]) -> Output {
    let machine = tempfile::tempdir().expect("isolated machine configuration");
    let settings = machine.path().join("machine.toml");
    fs::write(
        &settings,
        format!("[storage]\nroot = {:?}\n", machine.path().join("store")),
    )
    .expect("machine settings");
    Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"))
        .arg("--project")
        .arg(root)
        .args(args)
        .env("RS_INFRA_PLATFORM_CONFIG", settings)
        .env_remove("RS_INFRA_PLATFORM_CLEANUP")
        .output()
        .expect("execute platform CLI")
}

fn rejected(config: &str, args: &[&str], expected: &str) {
    let root = project(config);
    let output = run(root.path(), args);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "invalid input accepted: {args:?}");
    assert!(stderr.contains(expected), "missing {expected:?} in {stderr}");
}

#[test]
fn test_cleanup_flags_are_mutually_exclusive() {
    rejected(VALID, &["check", "--cleanup", "--no-cleanup"], "cannot be used");
}

#[test]
fn test_unknown_selection_is_rejected() {
    rejected(VALID, &["check", "--suite", "missing"], "unknown suite");
    rejected(VALID, &["check", "--platform", "missing"], "invalid value");
    rejected(VALID, &["check", "--require", "missing"], "invalid value");
}

#[test]
fn test_unknown_fields_and_version_are_rejected() {
    rejected(&format!("unexpected = true\n{VALID}"), &["plan"], "unknown field");
    rejected(
        &VALID.replace("version = 1", "version = 2"),
        &["plan"],
        "unsupported version",
    );
    rejected(
        &VALID.replace("kind = \"test\"", "kind = \"unknown\""),
        &["plan"],
        "unknown variant",
    );
    rejected(
        &VALID.replace("cwd = \".\"", "cwd = \".\"\nfeatures = []"),
        &["plan"],
        "unknown field",
    );
}

#[test]
fn test_duplicate_suite_ids_are_rejected() {
    let (_, suite) = VALID.split_once("[[suites]]").expect("suite fixture");
    rejected(&format!("{VALID}\n[[suites]]{suite}"), &["plan"], "duplicate suite");
}

#[test]
fn test_empty_argv_and_embedded_toolchain_are_rejected() {
    rejected(
        &VALID.replace("[\"never-execute-this-command\", \"test\", \"--locked\"]", "[]"),
        &["plan"],
        "argv",
    );
    rejected(
        &VALID.replace(
            "[\"never-execute-this-command\", \"test\", \"--locked\"]",
            "[\"cargo\", \"+nightly\", \"test\"]",
        ),
        &["plan"],
        "toolchain",
    );
}

#[test]
fn test_plus_arguments_are_allowed_outside_cargo_toolchain_selection() {
    for argv in [
        vec!["cargo", "test", "--", "+fixture"],
        vec!["custom-checker", "+fixture"],
        vec!["/tools/cargo", "test", "--", "+fixture"],
    ] {
        let config = VALID.replace(
            "[\"never-execute-this-command\", \"test\", \"--locked\"]",
            &serde_json::to_string(&argv).expect("serialize command arguments"),
        );
        let root = project(&config);
        let output = run(root.path(), &["plan"]);
        assert!(
            output.status.success(),
            "valid argv {argv:?} rejected: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn test_cargo_toolchain_selection_is_rejected_for_program_paths() {
    for program in ["cargo", "cargo.exe", "/tools/cargo", r"C:\tools\cargo.exe"] {
        let config = VALID.replace(
            "[\"never-execute-this-command\", \"test\", \"--locked\"]",
            &serde_json::to_string(&[program, "+nightly", "test"]).expect("serialize command arguments"),
        );
        rejected(&config, &["plan"], "toolchain");
    }
}

#[test]
fn test_cwd_and_data_cannot_escape_project() {
    for path in ["../outside", "/absolute", "C:\\\\outside", "..\\\\outside"] {
        rejected(
            &VALID.replace("cwd = \".\"", &format!("cwd = \"{path}\"")),
            &["plan"],
            "relative project path",
        );
        rejected(
            &VALID.replace("data = []", &format!("data = [\"{path}\"]")),
            &["plan"],
            "relative project path",
        );
    }
}

#[test]
fn test_plan_is_read_only_and_does_not_execute_suite() {
    let root = project(VALID);
    let before = fs::read(root.path().join(".infra/platform/platform.toml")).expect("read config");
    let output = run(root.path(), &["plan"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("default"),
        "plan should describe selected suite: {stdout}"
    );
    assert!(
        stdout.contains("unsupported"),
        "plan should describe unavailable routes: {stdout}"
    );
    assert_eq!(
        before,
        fs::read(root.path().join(".infra/platform/platform.toml")).expect("reread config")
    );
    assert_eq!(fs::read_dir(root.path()).expect("read project").count(), 1);
    assert_eq!(fs::read_dir(root.path().join(".infra")).expect("read infra").count(), 1);
    assert_eq!(
        fs::read_dir(root.path().join(".infra/platform"))
            .expect("read platform")
            .count(),
        1
    );
}

#[test]
fn test_doctor_only_diagnoses_and_project_is_global() {
    let root = tempfile::tempdir().expect("create empty project");
    let output = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"))
        .args(["doctor", "--project"])
        .arg(root.path())
        .env("PATH", "")
        .output()
        .expect("execute doctor");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("diagnostic"));
    assert_eq!(fs::read_dir(root.path()).expect("read project").count(), 0);
}

#[test]
fn test_clean_requires_exactly_one_selection() {
    rejected(VALID, &["clean"], "required");
    rejected(VALID, &["clean", "--run", "one", "--stale"], "cannot be used");
}

#[test]
fn test_unregistered_check_cannot_report_success() {
    rejected(
        VALID,
        &["check", "--platform", "linux"],
        "source project has no Cargo.toml",
    );
}

#[test]
fn test_guest_accepts_stdin_protocol_without_request_argument() {
    let root = project(VALID);
    let output = run(root.path(), &["guest"]);
    assert!(
        output.status.success(),
        "protocol errors are returned in a JSON response"
    );
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).expect("guest response JSON");
    assert_eq!(response["ok"], false);
    assert!(
        response["error"]
            .as_str()
            .is_some_and(|error| error.contains("invalid request"))
    );
}

#[test]
fn test_setup_only_describes_manual_preparation() {
    let root = tempfile::tempdir().expect("create empty project");
    let output = run(root.path(), &["setup", "--platform", "windows"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("manual"));
    assert_eq!(fs::read_dir(root.path()).expect("read project").count(), 0);
}
