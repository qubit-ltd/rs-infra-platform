// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Explicit opt-in checks against actual native, VM, and Android environments.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn run_platform(platform: &str) {
    let fixture = tempfile::tempdir().expect("E2E workspace");
    let project = fixture.path().join("project");
    fs::create_dir_all(project.join("src")).expect("source directory");
    fs::create_dir_all(project.join("assets")).expect("test data directory");
    fs::create_dir_all(project.join(".infra/platform")).expect("suite directory");
    fs::create_dir_all(project.join(".infra/tools")).expect("toolchain directory");
    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"platform-e2e\"\nversion = \"0.1.0\"\nedition = \"2024\"\nrust-version = \"1.94\"\n[workspace]\n",
    )
    .expect("test manifest");
    fs::write(project.join("assets/payload.txt"), "current-workspace").expect("runtime test data");
    let source = r#"
#[test]
fn actual_platform_filesystem() {
    assert_eq!(std::env::consts::OS, "EXPECTED_PLATFORM");
    assert_eq!(std::fs::read_to_string("assets/payload.txt").expect("runtime data"), "current-workspace");
    let root = std::env::temp_dir().join(format!("platform-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("writable temporary directory");
    std::fs::write(root.join("before"), b"rename-contract").expect("write file");
    std::fs::rename(root.join("before"), root.join("after")).expect("native rename");
    assert_eq!(std::fs::read(root.join("after")).expect("read renamed file"), b"rename-contract");
    std::fs::remove_dir_all(root).expect("remove test-owned temporary directory");
}
"#
    .replace("EXPECTED_PLATFORM", platform);
    fs::write(project.join("src/lib.rs"), source).expect("current uncommitted source");
    fs::write(
        project.join(".infra/tools/defaults.toml"),
        "build_toolchain = \"1.94.0\"\n",
    )
    .expect("shared toolchain");
    // Build the TOML through serde to preserve Windows and spaced path spelling.
    let suite_json = serde_json::json!({
        "version": 1,
        "suites": [{
            "id": "real-filesystem",
            "platforms": [platform],
            "timeout_seconds": 300,
            "minimum_tests": 1,
            "requirements": [],
            "data": ["assets"],
            "commands": [{
                "kind": "test",
                "argv": ["cargo", "test", "--locked", "--offline", "--lib"],
                "cwd": "."
            }]
        }]
    });
    let suite_toml = toml::Value::try_from(suite_json).expect("TOML suite");
    fs::write(
        project.join(".infra/platform/platform.toml"),
        toml::to_string(&suite_toml).expect("serialize suite"),
    )
    .expect("write suite");
    let locked = Command::new("cargo")
        .args(["+1.94.0", "generate-lockfile", "--offline"])
        .current_dir(&project)
        .output()
        .expect("generate fixture lockfile");
    assert!(locked.status.success(), "{}", String::from_utf8_lossy(&locked.stderr));

    let mut settings = match std::env::var_os("RS_INFRA_PLATFORM_E2E_CONFIG") {
        Some(path) => fs::read_to_string(path)
            .expect("read explicitly selected E2E machine settings")
            .parse::<toml::Table>()
            .expect("parse E2E machine settings"),
        None if platform == std::env::consts::OS => toml::Table::new(),
        None => {
            let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).expect("home directory");
            fs::read_to_string(Path::new(&home).join(".config/rs-infra/platform.toml"))
                .expect("prepare machine settings or set RS_INFRA_PLATFORM_E2E_CONFIG")
                .parse::<toml::Table>()
                .expect("parse default machine settings")
        }
    };
    let store = fixture.path().join("store");
    settings.insert(
        "storage".into(),
        toml::Value::Table(toml::Table::from_iter([(
            "root".into(),
            toml::Value::String(store.to_str().expect("UTF-8 E2E state path").to_owned()),
        )])),
    );
    let machine = fixture.path().join("machine.toml");
    fs::write(
        &machine,
        toml::to_string(&settings).expect("serialize machine settings"),
    )
    .expect("write isolated machine settings");
    let output = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"))
        .arg("--project")
        .arg(&project)
        .args(["check", "--platform", platform, "--require", platform, "--cleanup"])
        .env("RS_INFRA_PLATFORM_CONFIG", machine)
        .env_remove("RS_INFRA_PLATFORM_CLEANUP")
        .output()
        .expect("run actual platform coordinator");
    assert!(
        output.status.success(),
        "actual {platform} check failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report_dir = fs::read_dir(store.join("reports"))
        .expect("durable reports")
        .next()
        .expect("one run")
        .expect("run entry")
        .path();
    let summary: Value = serde_json::from_slice(&fs::read(report_dir.join("summary.json")).expect("read summary"))
        .expect("valid JSON summary");
    assert_eq!(summary["success"], true);
    assert_eq!(summary["suites"][0]["platform"], platform);
    assert_eq!(summary["suites"][0]["status"], "passed");
    assert_eq!(summary["suites"][0]["tests_passed"], 1);
    assert_eq!(summary["suites"][0]["tests_failed"], 0);
    assert_eq!(summary["cleanup"]["errors"], serde_json::json!([]));
    assert_eq!(summary["cleanup"]["remaining"], serde_json::json!([]));
    assert_eq!(fs::read_dir(store.join("runs")).expect("owned runs").count(), 0);
    assert_eq!(
        fs::read_to_string(project.join("assets/payload.txt")).expect("original data"),
        "current-workspace"
    );
    println!(
        "actual {platform}: 1 test passed; default cleanup verified; environment={}",
        summary["suites"][0]["environment"]
    );
}

#[test]
#[ignore = "explicit real Linux integration test"]
#[cfg(target_os = "linux")]
fn linux_native() {
    run_platform("linux");
}

#[test]
#[ignore = "explicit real macOS integration test"]
#[cfg(target_os = "macos")]
fn macos_native() {
    run_platform("macos");
}

#[test]
#[ignore = "requires an installed Windows VM or native Windows environment"]
fn windows_vm() {
    run_platform("windows");
}

#[test]
#[ignore = "requires prepared Android SDK, NDK, and device or AVD"]
fn android_emulator() {
    run_platform("android");
}
