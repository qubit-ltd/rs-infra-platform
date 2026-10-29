// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use tempfile::TempDir;

const FAULT_SOURCE: &str = r#"
#[test]
fn fault_fixture() {
    let mode = std::env::var("PLATFORM_NATIVE_FAULT").unwrap_or_default();
    let target = std::path::PathBuf::from(std::env::var_os("CARGO_TARGET_DIR").expect("owned target"));
    let run = target.parent().expect("owned run");
    if mode.contains("cleanup") {
        std::fs::write(run.join(".rs-infra-platform-owner"), b"foreign marker").expect("tamper generated fixture marker");
    }
    if mode == "report" {
        let id = run.file_name().expect("run id");
        let store = run.parent().expect("runs").parent().expect("store");
        std::fs::create_dir(store.join("reports").join(id).join("summary.json.tmp")).expect("block report publication");
    }
    #[cfg(unix)]
    if mode == "report-link" {
        let id = run.file_name().expect("run id");
        let store = run.parent().expect("runs").parent().expect("store");
        let victim = std::env::var_os("PLATFORM_REPORT_VICTIM").expect("fixture victim");
        std::os::unix::fs::symlink(victim, store.join("reports").join(id).join("summary.json.tmp")).expect("fixture report link");
    }
    if mode == "sleep" {
        std::fs::write(run.join("test-started"), b"started").expect("signal fixture ready");
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
    if mode == "exit-nine" { std::process::exit(9); }
    assert_ne!(mode, "failure-cleanup", "original test failure");
}
"#;

fn fixture(timeout: u64) -> TempDir {
    let fixture = tempfile::tempdir().expect("recovery fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(project.join("src")).expect("sources");
    fs::create_dir_all(project.join(".infra/platform")).expect("suite directory");
    fs::create_dir_all(project.join(".infra/tools")).expect("toolchain directory");
    fs::write(
        project.join("Cargo.toml"),
        include_str!("fixtures/native-smoke/Cargo.toml"),
    )
    .expect("manifest");
    fs::write(project.join("src/lib.rs"), FAULT_SOURCE).expect("fault test source");
    fs::write(
        project.join(".infra/tools/defaults.toml"),
        "build_toolchain = \"1.94.0\"\n",
    )
    .expect("toolchain");
    fs::write(project.join(".infra/platform/platform.toml"), format!("version = 1\n[[suites]]\nid = \"fault\"\nplatforms = [\"{}\"]\ntimeout_seconds = {timeout}\nminimum_tests = 1\n[[suites.commands]]\nkind = \"test\"\nargv = [\"cargo\", \"test\", \"--offline\"]\ncwd = \".\"\n", std::env::consts::OS)).expect("suite");
    fs::write(
        fixture.path().join("machine.toml"),
        format!("[storage]\nroot = {:?}\n", fixture.path().join("store")),
    )
    .expect("machine settings");
    fixture
}

fn command(fixture: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"));
    command
        .arg("--project")
        .arg(fixture.join("project"))
        .env("RS_INFRA_PLATFORM_CONFIG", fixture.join("machine.toml"))
        .env_remove("RS_INFRA_PLATFORM_CLEANUP");
    command
}

fn check(fixture: &Path, mode: &str, args: &[&str]) -> Output {
    command(fixture)
        .arg("check")
        .args(args)
        .env("PLATFORM_NATIVE_FAULT", mode)
        .output()
        .expect("check fault fixture")
}

fn report(fixture: &Path) -> (PathBuf, Value) {
    let directory = fs::read_dir(fixture.join("store/reports"))
        .expect("reports")
        .next()
        .expect("report entry")
        .expect("report path")
        .path();
    let summary =
        serde_json::from_slice(&fs::read(directory.join("summary.json")).expect("report JSON")).expect("parse report");
    (directory, summary)
}

#[test]
fn test_test_failure_and_cleanup_failure_are_both_preserved() {
    for (mode, tests_failed) in [("cleanup", 0), ("failure-cleanup", 1)] {
        let fixture = fixture(60);
        let output = check(fixture.path(), mode, &[]);
        assert!(!output.status.success(), "required cleanup failure must fail the run");
        let (_, summary) = report(fixture.path());
        assert_eq!(summary["success"], false);
        assert_eq!(summary["suites"][0]["tests_failed"], tests_failed);
        assert!(
            !summary["cleanup"]["errors"]
                .as_array()
                .expect("cleanup errors")
                .is_empty()
        );
        assert!(
            !summary["cleanup"]["remaining"]
                .as_array()
                .expect("remaining")
                .is_empty()
        );
        if tests_failed > 0 {
            assert!(
                summary["suites"][0]["errors"]
                    .as_array()
                    .expect("test errors")
                    .iter()
                    .any(|error| error.as_str().is_some_and(|error| error.contains("command failed")))
            );
        }
    }
}

#[test]
fn test_exit_nine_and_timeout_are_distinct_and_cleanup_runs() {
    for (mode, timeout) in [("exit-nine", 60), ("sleep", 1)] {
        let fixture = fixture(timeout);
        let output = check(fixture.path(), mode, &[]);
        assert!(!output.status.success());
        let (logs, summary) = report(fixture.path());
        let result = &summary["suites"][0]["commands"][0];
        // Cargo preserves the native test-process exit status.
        if mode == "sleep" {
            assert_eq!(result["timed_out"], true);
        } else {
            assert_eq!(result["exit_code"], 9);
            assert!(
                fs::read_to_string(logs.join("entry-0/command-0.stderr.log"))
                    .expect("stderr")
                    .contains("exit status: 9")
            );
        }
        assert_eq!(
            fs::read_dir(fixture.path().join("store/runs")).expect("runs").count(),
            0
        );
    }
}

#[test]
fn test_report_failure_still_cleans_artifacts_and_preserves_logs() {
    let fixture = fixture(60);
    let output = check(fixture.path(), "report", &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("report"));
    let (logs, _) = report(fixture.path());
    assert!(logs.join("entry-0/command-0.stdout.log").is_file());
    assert_eq!(
        fs::read_dir(fixture.path().join("store/runs")).expect("runs").count(),
        0
    );
}

#[test]
fn test_retained_run_survives_stale_scan_until_explicit_cleanup() {
    let fixture = fixture(60);
    let output = check(fixture.path(), "", &["--no-cleanup"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let (_, summary) = report(fixture.path());
    let retained = Path::new(summary["retained_path"].as_str().expect("retained location"));
    let output = command(fixture.path())
        .args(["clean", "--stale"])
        .output()
        .expect("stale cleanup");
    assert!(output.status.success());
    assert!(retained.exists());
    let output = command(fixture.path())
        .args(["clean", "--run", summary["run_id"].as_str().expect("run id")])
        .output()
        .expect("explicit cleanup");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!retained.exists());
}

#[cfg(unix)]
#[test]
fn test_sigterm_cancels_real_cli_and_preserves_final_report() {
    let fixture = fixture(60);
    let mut child = command(fixture.path())
        .arg("check")
        .env("PLATFORM_NATIVE_FAULT", "sleep")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start cancellable CLI");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut started = false;
    while Instant::now() < deadline {
        if let Ok(runs) = fs::read_dir(fixture.path().join("store/runs")) {
            started = runs
                .filter_map(Result::ok)
                .any(|run| run.path().join("test-started").exists());
            if started {
                break;
            }
        }
        if child.try_wait().expect("observe CLI").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: the PID belongs to the live child created and retained by this test.
    unsafe {
        libc::kill(child.id().try_into().expect("child PID"), libc::SIGTERM);
    }
    assert!(!child.wait().expect("wait canceled CLI").success());
    assert!(started, "real test process must start before cancellation");
    let (_, summary) = report(fixture.path());
    assert_eq!(summary["state"], "canceled");
    assert_eq!(summary["suites"][0]["commands"][0]["cancelled"], true);
    assert_eq!(
        fs::read_dir(fixture.path().join("store/runs")).expect("runs").count(),
        0
    );
}

#[cfg(unix)]
#[test]
fn test_report_temporary_symlink_never_overwrites_external_file() {
    let fixture = fixture(60);
    let victim = fixture.path().join("precious.txt");
    fs::write(&victim, b"preserve user contents").expect("fixture victim");
    let output = command(fixture.path())
        .arg("check")
        .env("PLATFORM_NATIVE_FAULT", "report-link")
        .env("PLATFORM_REPORT_VICTIM", &victim)
        .output()
        .expect("report symlink fixture");
    assert!(!output.status.success());
    let (logs, _) = report(fixture.path());
    assert!(
        fs::read_to_string(logs.join("entry-0/command-0.stdout.log"))
            .expect("real fault fixture log")
            .contains("1 passed")
    );
    assert_eq!(
        fs::read(&victim).expect("preserved external file"),
        b"preserve user contents"
    );
}

#[cfg(unix)]
#[test]
fn test_sigkill_run_is_recovered_by_next_check_without_removing_reports() {
    let fixture = fixture(60);
    let mut child = command(fixture.path())
        .arg("check")
        .env("PLATFORM_NATIVE_FAULT", "sleep")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start interrupted CLI");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut old_run = None;
    while Instant::now() < deadline {
        if let Ok(runs) = fs::read_dir(fixture.path().join("store/runs")) {
            old_run = runs
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| path.join("test-started").exists());
            if old_run.is_some() {
                break;
            }
        }
        if child.try_wait().expect("observe CLI").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.kill().expect("SIGKILL test coordinator");
    child.wait().expect("reap interrupted coordinator");
    let old_run = old_run.expect("real test began before abrupt interruption");
    let id = old_run.file_name().expect("old run id").to_string_lossy().into_owned();
    let old_report = fixture.path().join("store/reports").join(&id);
    let output = check(fixture.path(), "", &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!old_run.exists(), "next check must recover the provably dead run");
    assert!(old_report.join("entry-0/command-0.stdout.log").is_file());
    let record: Value = serde_json::from_slice(
        &fs::read(fixture.path().join("store/records").join(format!("{id}.json"))).expect("recovered durable record"),
    )
    .expect("record JSON");
    assert_eq!(record["state"], "cleaned");
}

#[test]
fn test_explicit_cleanup_preserves_unconfirmed_process_tree_artifacts() {
    let fixture = fixture(60);
    let output = check(fixture.path(), "", &["--no-cleanup"]);
    assert!(output.status.success());
    let (_, summary) = report(fixture.path());
    let id = summary["run_id"].as_str().expect("retained id");
    let path = fixture.path().join("store/records").join(format!("{id}.json"));
    let mut record: Value =
        serde_json::from_slice(&fs::read(&path).expect("registered retained record")).expect("record JSON");
    record["environments"] = serde_json::json!([{ "key": "process-tree:entry-0", "owned": true, "stopped": false }]);
    fs::write(
        &path,
        serde_json::to_vec(&record).expect("serialize controlled pending state"),
    )
    .expect("record pending tree fixture");
    let output = command(fixture.path())
        .args(["clean", "--run", id])
        .output()
        .expect("clean guarded process artifacts");
    assert!(
        !output.status.success(),
        "cleanup must not report complete while a process tree is unconfirmed"
    );
    assert!(Path::new(summary["retained_path"].as_str().expect("retained artifacts")).exists());
    let record: Value =
        serde_json::from_slice(&fs::read(path).expect("pending durable record")).expect("pending record JSON");
    assert_eq!(record["state"], "cleanup-pending");
}

#[test]
fn test_next_check_stops_interrupted_no_cleanup_run_without_removing_files() {
    for state in ["active", "cleanup-pending"] {
        let fixture = fixture(60);
        assert!(check(fixture.path(), "", &["--no-cleanup"]).status.success());
        let (_, summary) = report(fixture.path());
        let id = summary["run_id"].as_str().expect("retained id");
        let retained = Path::new(summary["retained_path"].as_str().expect("retained directory"));
        fs::write(retained.join("preserved-after-interruption"), b"no cleanup requested")
            .expect("retained fixture data");
        let path = fixture.path().join("store/records").join(format!("{id}.json"));
        let mut record: Value =
            serde_json::from_slice(&fs::read(&path).expect("retained record")).expect("record JSON");
        record["state"] = state.into();
        fs::write(&path, serde_json::to_vec(&record).expect("serialize interrupted state"))
            .expect("controlled interruption record");
        let output = check(fixture.path(), "", &["--cleanup"]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(
            fs::read(retained.join("preserved-after-interruption")).expect("preserved retained artifact"),
            b"no cleanup requested"
        );
        let record: Value =
            serde_json::from_slice(&fs::read(path).expect("recovered retained state")).expect("record JSON");
        assert_eq!(
            record["state"], "retained",
            "dead no-cleanup run must complete stop-only recovery"
        );
        assert_eq!(record["policy"], "disabled");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn test_registered_stopped_process_identity_allows_safe_same_boot_cleanup() {
    let fixture = fixture(60);
    assert!(check(fixture.path(), "", &["--no-cleanup"]).status.success());
    let (_, summary) = report(fixture.path());
    let id = summary["run_id"].as_str().expect("retained run id");
    let metadata = fixture
        .path()
        .join("store/reports")
        .join(id)
        .join("entry-0/process-stop-0.json");
    // This impossible kernel group has no live members; no unrelated process is
    // signaled.
    let identity = serde_json::json!({
        "mechanism": "Unix", "host": fs::read_to_string("/etc/machine-id").expect("machine identity").trim(),
        "boot": fs::read_to_string("/proc/sys/kernel/random/boot_id").expect("boot identity").trim(),
        "group": i32::MAX, "leader": { "pid": i32::MAX, "start": "fixture-leader" },
        "anchor": { "pid": i32::MAX-1, "start": "fixture-anchor" }
    });
    use sha2::Digest;
    use sha2::Sha256;
    let bytes = serde_json::to_vec(&identity).expect("serialize stop identity");
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    fs::write(metadata, bytes).expect("registered process metadata");
    let path = fixture.path().join("store/records").join(format!("{id}.json"));
    let mut record: Value = serde_json::from_slice(&fs::read(&path).expect("retained record")).expect("record JSON");
    record["environments"] = serde_json::json!([{ "key": format!("process-tree:entry-0/process-stop-0.json:sha256={digest}"), "owned": true, "stopped": false }]);
    fs::write(
        path,
        serde_json::to_vec(&record).expect("serialize controlled process record"),
    )
    .expect("record process guard");
    let output = command(fixture.path())
        .args(["clean", "--run", id])
        .output()
        .expect("retry proven stopped process identity");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!Path::new(summary["retained_path"].as_str().expect("retained path")).exists());
}
