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

use serde_json::Value;
use tempfile::TempDir;

fn fixture(source: &str, commands: &str) -> TempDir {
    let fixture = tempfile::tempdir().expect("native fixture");
    let project = fixture.path().join("project");
    fs::create_dir_all(project.join("src")).expect("source directory");
    fs::create_dir_all(project.join(".infra/platform")).expect("platform directory");
    fs::create_dir_all(project.join(".infra/tools")).expect("toolchain directory");
    fs::write(
        project.join("Cargo.toml"),
        include_str!("fixtures/native-smoke/Cargo.toml"),
    )
    .expect("manifest");
    fs::write(project.join("src/lib.rs"), source).expect("source");
    fs::write(
        project.join(".infra/tools/defaults.toml"),
        "build_toolchain = \"1.94.0\"\n",
    )
    .expect("shared toolchain");
    fs::write(project.join(".infra/platform/platform.toml"), format!("version = 1\n[[suites]]\nid = \"native\"\nplatforms = [\"{}\"]\ntimeout_seconds = 60\nminimum_tests = 1\n{commands}", std::env::consts::OS)).expect("suite");
    fs::write(
        fixture.path().join("machine.toml"),
        format!("[storage]\nroot = {:?}\n", fixture.path().join("store")),
    )
    .expect("machine config");
    fixture
}

fn check(fixture: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"))
        .arg("--project")
        .arg(fixture.join("project"))
        .arg("check")
        .args(args)
        .env("RS_INFRA_PLATFORM_CONFIG", fixture.join("machine.toml"))
        .env_remove("RS_INFRA_PLATFORM_CLEANUP")
        .output()
        .expect("native check")
}

fn report(fixture: &Path) -> Value {
    let reports = fixture.join("store/reports");
    let run = fs::read_dir(reports)
        .expect("reports directory")
        .next()
        .expect("run report")
        .expect("run entry")
        .path();
    serde_json::from_slice(&fs::read(run.join("summary.json")).expect("summary file")).expect("summary JSON")
}

const COMMANDS: &str =
    "[[suites.commands]]\nkind = \"test\"\nargv = [\"cargo\", \"test\", \"--offline\"]\ncwd = \".\"\n";

#[test]
fn test_required_compile_only_suites_report_checked_after_real_cargo_success() {
    for kind in ["check", "build"] {
        let commands =
            format!("[[suites.commands]]\nkind = {kind:?}\nargv = [\"cargo\", {kind:?}, \"--offline\"]\ncwd = \".\"\n");
        let fixture = fixture("", &commands);
        let config = fixture.path().join("project/.infra/platform/platform.toml");
        let text = fs::read_to_string(&config)
            .expect("suite")
            .replace("minimum_tests = 1", "minimum_tests = 0");
        fs::write(config, text).expect("compile-only suite");
        let output = check(fixture.path(), &["--require", std::env::consts::OS]);
        let summary = report(fixture.path());
        assert_eq!(summary["suites"][0]["commands"][0]["exit_code"], 0);
        assert!(output.status.success(), "required {kind}: {summary}");
        assert_eq!(summary["success"], true);
        assert_eq!(summary["suites"][0]["status"], "checked");
        assert_eq!(summary["suites"][0]["tests_passed"], 0);
    }
}

#[test]
fn test_required_test_suite_still_rejects_zero_tests_with_zero_configured_minimum() {
    let fixture = fixture("", COMMANDS);
    let config = fixture.path().join("project/.infra/platform/platform.toml");
    let text = fs::read_to_string(&config)
        .expect("suite")
        .replace("minimum_tests = 1", "minimum_tests = 0");
    fs::write(config, text).expect("zero configured minimum");
    let output = check(fixture.path(), &["--require", std::env::consts::OS]);
    assert!(!output.status.success());
    let summary = report(fixture.path());
    assert_eq!(summary["suites"][0]["commands"][0]["exit_code"], 0);
    assert_eq!(summary["suites"][0]["status"], "failed");
    assert!(summary["suites"][0]["errors"].to_string().contains("minimum_tests=1"));
}

#[test]
fn test_real_native_snapshot_runs_current_workspace_and_cleans() {
    let fixture = fixture(include_str!("fixtures/native-smoke/src/lib.rs"), COMMANDS);
    fs::create_dir_all(fixture.path().join("project/target")).expect("original target");
    fs::write(fixture.path().join("project/target/keep"), b"original").expect("original file");
    let output = check(fixture.path(), &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let summary = report(fixture.path());
    assert_eq!(summary["success"], true);
    assert_eq!(summary["suites"][0]["tests_passed"], 1);
    assert_eq!(summary["suites"][0]["status"], "passed");
    assert_eq!(
        fs::read_dir(fixture.path().join("store/runs")).expect("runs").count(),
        0
    );
    assert_eq!(
        fs::read(fixture.path().join("project/target/keep")).expect("original intact"),
        b"original"
    );
    assert!(!fixture.path().join("project/Cargo.lock").exists());
}

#[test]
fn test_zero_and_ignored_tests_fail_minimum_and_preserve_logs() {
    for source in ["", "#[test]\n#[ignore]\nfn ignored_test() {}\n"] {
        let fixture = fixture(source, COMMANDS);
        let output = check(fixture.path(), &[]);
        assert!(!output.status.success());
        let summary = report(fixture.path());
        assert_eq!(summary["success"], false);
        assert!(
            summary["suites"][0]["errors"]
                .as_array()
                .expect("suite errors")
                .iter()
                .any(|e| e.as_str().is_some_and(|e| e.contains("minimum_tests")))
        );
        let log = summary["suites"][0]["commands"][0]["stdout"]
            .as_str()
            .expect("stdout log");
        assert!(Path::new(log).is_file());
    }
}

#[test]
fn test_failed_command_and_no_cleanup_report_retained_run() {
    let fixture = fixture("#[test]\nfn failure() { panic!(\"workspace edit\"); }", COMMANDS);
    let output = check(fixture.path(), &["--no-cleanup"]);
    assert!(!output.status.success());
    let summary = report(fixture.path());
    assert_eq!(summary["success"], false);
    assert_eq!(summary["suites"][0]["tests_failed"], 1);
    assert!(
        summary["retained_path"]
            .as_str()
            .is_some_and(|path| Path::new(path).exists())
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("clean --run"));
}

#[test]
fn test_all_skipped_does_not_claim_passed_and_require_fails() {
    let fixture = fixture("", COMMANDS);
    let config = fixture.path().join("project/.infra/platform/platform.toml");
    let unsupported = if cfg!(target_os = "macos") { "linux" } else { "macos" };
    let contents = fs::read_to_string(&config)
        .expect("read config")
        .replace(std::env::consts::OS, unsupported);
    fs::write(config, contents).expect("unsupported configuration");
    let output = check(fixture.path(), &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("no platform tests executed"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("passed"));
    let output = check(fixture.path(), &["--require", unsupported]);
    assert!(!output.status.success());
}

#[test]
fn test_cargo_target_and_relative_manifest_are_validated() {
    let fixture = fixture(include_str!("fixtures/native-smoke/src/lib.rs"), COMMANDS);
    let triple = String::from_utf8(Command::new("rustc").arg("-vV").output().expect("rustc host").stdout)
        .expect("UTF8 rustc")
        .lines()
        .find_map(|line| line.strip_prefix("host: ").map(str::to_owned))
        .expect("host triple");
    let config_path = fixture.path().join("project/.infra/platform/platform.toml");
    let config = fs::read_to_string(&config_path)
        .expect("config")
        .replace("minimum_tests = 1", &format!("minimum_tests = 1\ntarget = {triple:?}"))
        .replace(
            "\"--offline\"",
            &format!("\"--offline\", \"--manifest-path\", \"Cargo.toml\", \"--target\", {triple:?}"),
        );
    fs::write(&config_path, config).expect("target config");
    let output = check(fixture.path(), &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(report(fixture.path())["suites"][0]["tests_passed"], 1);
}

#[test]
fn test_cli_plan_exposes_commands_and_target_without_running() {
    let fixture = fixture("", COMMANDS);
    let output = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"))
        .arg("--project")
        .arg(fixture.path().join("project"))
        .arg("plan")
        .env("RS_INFRA_PLATFORM_CONFIG", fixture.path().join("machine.toml"))
        .output()
        .expect("plan");
    assert!(output.status.success());
    let plan: Value = serde_json::from_slice(&output.stdout).expect("plan JSON");
    assert_eq!(plan["entries"][0]["commands"][0]["argv"][0], "cargo");
    assert!(plan["entries"][0].get("target").is_some());
    assert!(!fixture.path().join("store").exists());
}

#[test]
fn test_machine_schema_rejects_unknown_or_unsafe_fields_before_execution() {
    for machine in [
        "unexpected = true\n",
        "[storage]\nroot = \"relative\"\n",
        "[native]\ncargo = \"\"\n",
        "[native]\nunknown = true\n",
        "android = \"wrong table type\"\n",
        "[windows]\nunknown = true\n",
    ] {
        let fixture = fixture("", COMMANDS);
        let text = if machine.starts_with("[storage]") {
            machine.to_owned()
        } else {
            format!("storage = {{ root = {:?} }}\n{machine}", fixture.path().join("store"))
        };
        fs::write(fixture.path().join("machine.toml"), text).expect("invalid machine settings");
        let output = check(fixture.path(), &[]);
        assert!(!output.status.success(), "invalid machine settings accepted: {machine}");
        assert!(!fixture.path().join("store").exists());
    }
}

#[test]
fn test_cli_cleanup_flags_override_invalid_environment() {
    let fixture = fixture(include_str!("fixtures/native-smoke/src/lib.rs"), COMMANDS);
    let output = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"))
        .arg("--project")
        .arg(fixture.path().join("project"))
        .args(["check", "--cleanup"])
        .env("RS_INFRA_PLATFORM_CONFIG", fixture.path().join("machine.toml"))
        .env("RS_INFRA_PLATFORM_CLEANUP", "invalid")
        .output()
        .expect("CLI cleanup precedence");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(report(fixture.path())["retained_path"], Value::Null);
}

#[test]
fn test_retained_native_reuses_snapshot_target_and_keeps_attempt_reports() {
    let fixture = fixture(include_str!("fixtures/native-smoke/src/lib.rs"), COMMANDS);
    let original = fixture.path().join("project");
    fs::write(original.join("obsolete.txt"), b"old source input").expect("initial untracked input");
    let first = check(fixture.path(), &["--no-cleanup"]);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let summary = report(fixture.path());
    let run_id = summary["run_id"].as_str().expect("retained run id");
    let retained = Path::new(summary["retained_path"].as_str().expect("retained path"));
    let reports = fixture.path().join("store/reports").join(run_id);
    let first_summary = fs::read(reports.join("summary.json")).expect("first report");
    fs::write(retained.join("target/reuse-sentinel"), b"cargo artifacts survive").expect("target sentinel");
    fs::remove_file(original.join("obsolete.txt")).expect("remove generated source fixture");
    fs::write(
        original.join("src/lib.rs"),
        "#[test]\nfn edited_workspace_test() { assert_eq!(2+2,4); }\n",
    )
    .expect("edited current source");
    let second = check(fixture.path(), &["--no-cleanup"]);
    assert!(second.status.success(), "{}", String::from_utf8_lossy(&second.stderr));
    assert_eq!(
        fs::read_dir(fixture.path().join("store/runs"))
            .expect("retained runs")
            .count(),
        1,
        "matching no-cleanup invocation must reuse the registered run"
    );
    assert_eq!(
        fs::read(retained.join("target/reuse-sentinel")).expect("preserved target"),
        b"cargo artifacts survive"
    );
    assert!(!retained.join("snapshot/obsolete.txt").exists());
    assert_eq!(
        fs::read(reports.join("summary.json")).expect("old report retained"),
        first_summary
    );
    let latest: Value =
        serde_json::from_slice(&fs::read(reports.join("attempt-1/summary.json")).expect("new attempt report"))
            .expect("latest summary");
    assert_eq!(latest["run_id"], run_id);
    assert_eq!(latest["suites"][0]["tests_passed"], 1);
    assert!(
        fs::read_to_string(reports.join("attempt-1/entry-0/command-0.stdout.log"))
            .expect("latest actual tests")
            .contains("edited_workspace_test")
    );
    let cleaned = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"))
        .args(["clean", "--run", run_id])
        .env("RS_INFRA_PLATFORM_CONFIG", fixture.path().join("machine.toml"))
        .output()
        .expect("clean all registered attempts");
    assert!(cleaned.status.success(), "{}", String::from_utf8_lossy(&cleaned.stderr));
    assert!(!retained.exists());
    assert_eq!(
        fs::read(reports.join("summary.json")).expect("historical first report"),
        first_summary
    );
    assert!(reports.join("attempt-1/summary.json").is_file());
}

#[test]
fn test_native_environment_reports_measured_target_os_and_filesystem() {
    let fixture = fixture(include_str!("fixtures/native-smoke/src/lib.rs"), COMMANDS);
    let output = check(fixture.path(), &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let summary = report(fixture.path());
    let environment = &summary["suites"][0]["environment"];
    assert!(environment["target"].as_str().expect("target triple").contains('-'));
    assert_ne!(environment["os_version"], std::env::consts::OS);
    assert_ne!(environment["filesystem"], "not detected");
    assert!(
        environment["versions"]["rustc"]
            .as_str()
            .is_some_and(|version| version.contains("rustc"))
    );
}

#[test]
fn test_retained_run_lease_prevents_concurrent_mutation() {
    use fs2::FileExt;
    use sha2::Digest;
    use sha2::Sha256;
    let fixture = fixture(include_str!("fixtures/native-smoke/src/lib.rs"), COMMANDS);
    assert!(check(fixture.path(), &["--no-cleanup"]).status.success());
    let summary = report(fixture.path());
    let id = summary["run_id"].as_str().expect("run id");
    let digest: String = Sha256::digest(format!("run:{id}").as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.path().join("store/locks").join(format!("{digest}.lock")))
        .expect("registered run lock");
    held.try_lock_exclusive().expect("hold exact run lease");
    let output = check(fixture.path(), &["--no-cleanup"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("matching retained run is in use"));
    assert_eq!(
        fs::read_dir(fixture.path().join("store/runs"))
            .expect("run count")
            .count(),
        1
    );
    drop(held);
    assert!(check(fixture.path(), &["--no-cleanup"]).status.success());
}

#[test]
fn test_retained_configuration_mismatch_allocates_a_new_run() {
    let fixture = fixture(include_str!("fixtures/native-smoke/src/lib.rs"), COMMANDS);
    assert!(check(fixture.path(), &["--no-cleanup"]).status.success());
    let machine = fs::read_to_string(fixture.path().join("machine.toml")).expect("machine settings");
    fs::write(
        fixture.path().join("machine.toml"),
        format!("{machine}\n[native]\ncargo = {:?}\n", env!("CARGO")),
    )
    .expect("different native compiler program setting");
    let output = check(fixture.path(), &["--no-cleanup"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        fs::read_dir(fixture.path().join("store/runs"))
            .expect("separate run allocations")
            .count(),
        2
    );
}

#[cfg(unix)]
#[test]
fn test_non_utf8_cleanup_environment_is_invalid_unless_cli_overrides() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    for override_flag in [None, Some("--cleanup")] {
        let fixture = fixture(include_str!("fixtures/native-smoke/src/lib.rs"), COMMANDS);
        let mut command = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"));
        command
            .arg("--project")
            .arg(fixture.path().join("project"))
            .arg("check")
            .env("RS_INFRA_PLATFORM_CONFIG", fixture.path().join("machine.toml"))
            .env("RS_INFRA_PLATFORM_CLEANUP", OsString::from_vec(vec![0xff]));
        if let Some(flag) = override_flag {
            command.arg(flag);
        }
        let output = command.output().expect("non-UTF8 policy fixture");
        assert_eq!(output.status.success(), override_flag.is_some());
        if override_flag.is_none() {
            assert!(String::from_utf8_lossy(&output.stderr).contains("RS_INFRA_PLATFORM_CLEANUP"));
            assert!(!fixture.path().join("store").exists());
        }
    }
}

#[test]
fn test_retained_attempt_report_collision_preserves_stopped_state() {
    let fixture = fixture(include_str!("fixtures/native-smoke/src/lib.rs"), COMMANDS);
    assert!(check(fixture.path(), &["--no-cleanup"]).status.success());
    let summary = report(fixture.path());
    let id = summary["run_id"].as_str().expect("retained id");
    fs::create_dir(fixture.path().join("store/reports").join(id).join("attempt-1"))
        .expect("occupied generated report attempt");
    let output = check(fixture.path(), &["--no-cleanup"]);
    assert!(!output.status.success());
    let record: Value = serde_json::from_slice(
        &fs::read(fixture.path().join("store/records").join(format!("{id}.json"))).expect("durable retained record"),
    )
    .expect("record JSON");
    assert_eq!(
        record["state"], "retained",
        "failed reuse preparation must not leave an active owner registration"
    );
}

#[test]
fn test_unknown_native_requirement_is_rejected_before_suite_commands() {
    let fixture = fixture(include_str!("fixtures/native-smoke/src/lib.rs"), COMMANDS);
    let path = fixture.path().join("project/.infra/platform/platform.toml");
    let config = fs::read_to_string(&path).expect("suite config").replace(
        "minimum_tests = 1",
        "minimum_tests = 1\nrequirements = [\"unknown-required-capability\"]",
    );
    fs::write(path, config).expect("unsupported requirement");
    let output = check(fixture.path(), &[]);
    assert!(
        !output.status.success(),
        "unknown requirements cannot be silently accepted"
    );
    let summary = report(fixture.path());
    assert_eq!(summary["suites"][0]["commands"].as_array().expect("commands").len(), 0);
    assert!(
        summary["suites"][0]["errors"]
            .as_array()
            .expect("errors")
            .iter()
            .any(|error| error
                .as_str()
                .is_some_and(|error| error.contains("unknown-required-capability")))
    );
}

#[cfg(target_os = "linux")]
#[test]
fn test_native_ntfs_requirement_uses_the_actual_filesystem() {
    let fixture = fixture(include_str!("fixtures/native-smoke/src/lib.rs"), COMMANDS);
    let path = fixture.path().join("project/.infra/platform/platform.toml");
    let config = fs::read_to_string(&path)
        .expect("suite config")
        .replace("minimum_tests = 1", "minimum_tests = 1\nrequirements = [\"ntfs\"]");
    fs::write(path, config).expect("NTFS requirement");
    let output = check(fixture.path(), &[]);
    let summary = report(fixture.path());
    let filesystem = summary["suites"][0]["environment"]["filesystem"]
        .as_str()
        .expect("actual filesystem");
    let ntfs = filesystem.to_ascii_lowercase().starts_with("ntfs");
    assert_eq!(
        output.status.success(),
        ntfs,
        "NTFS requirement result must reflect measured {filesystem}"
    );
    if !ntfs {
        assert_eq!(summary["suites"][0]["commands"].as_array().expect("commands").len(), 0);
    }
}

#[cfg(unix)]
#[test]
fn test_supported_native_requirements_probe_real_operations() {
    let fixture = fixture(include_str!("fixtures/native-smoke/src/lib.rs"), COMMANDS);
    let path = fixture.path().join("project/.infra/platform/platform.toml");
    let config = fs::read_to_string(&path).expect("suite config").replace(
        "minimum_tests = 1",
        "minimum_tests = 1\nrequirements = [\"native-shell\", \"writable-tmpdir\", \"symlinks\", \"long-paths\"]",
    );
    fs::write(path, config).expect("native requirement probes");
    let output = check(fixture.path(), &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let summary = report(fixture.path());
    assert_eq!(summary["suites"][0]["tests_passed"], 1);
}

#[test]
fn test_completed_test_results_survive_a_later_command_spawn_failure() {
    let commands = format!(
        "{COMMANDS}\n[[suites.commands]]\nkind = \"test\"\nargv = [\"rs-infra-nonexistent-command-for-controlled-fixture\"]\ncwd = \".\"\n"
    );
    let fixture = fixture(include_str!("fixtures/native-smoke/src/lib.rs"), &commands);
    let output = check(fixture.path(), &[]);
    assert!(!output.status.success());
    let summary = report(fixture.path());
    assert_eq!(
        summary["suites"][0]["tests_passed"], 1,
        "already executed tests must remain in an infrastructure failure report"
    );
    assert_eq!(
        summary["suites"][0]["commands"]
            .as_array()
            .expect("completed commands")
            .len(),
        1
    );
    assert_eq!(summary["suites"][0]["status"], "infrastructure_failed");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("no platform tests executed"));
}
