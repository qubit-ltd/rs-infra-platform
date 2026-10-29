// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
use std::io::Write;
use std::process::Command;
use std::process::Stdio;

fn request(value: &serde_json::Value) -> serde_json::Value {
    let root = tempfile::tempdir().expect("guest state");
    request_at(&root.path().join("state"), value)
}

fn request_at(root: &std::path::Path, value: &serde_json::Value) -> serde_json::Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"))
        .arg("guest")
        .env("RS_INFRA_PLATFORM_GUEST_ROOT", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start guest");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(value.to_string().as_bytes())
        .expect("request");
    let output = child.wait_with_output().expect("guest response");
    assert!(
        output.status.success(),
        "protocol errors must have structured responses: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("only a JSON response on stdout")
}

#[test]
fn test_protocol_version_mismatch_has_structured_error() {
    let response = request(
        &serde_json::json!({"protocol_version":99,"request_id":"a","run_id":"aabb","lease_token":"0123456789abcdef0123456789abcdef","op":"probe"}),
    );
    assert_eq!(response["ok"], false);
    assert!(response["error"].as_str().expect("error").contains("protocol version"));
}

#[test]
fn test_cleanup_rejects_arbitrary_absolute_path() {
    let response = request(
        &serde_json::json!({"protocol_version":1,"request_id":"a","run_id":"aabb","lease_token":"0123456789abcdef0123456789abcdef","op":"cleanup","path":"C:\\precious"}),
    );
    assert_eq!(response["ok"], false);
    assert!(response["error"].as_str().expect("error").contains("unknown field"));
}

#[test]
fn test_cleanup_rejects_unregistered_run() {
    let response = request(
        &serde_json::json!({"protocol_version":1,"request_id":"a","run_id":"aabb","lease_token":"0123456789abcdef0123456789abcdef","op":"cleanup"}),
    );
    assert_eq!(response["ok"], false);
    assert!(response["error"].as_str().expect("error").contains("registration"));
}

#[test]
fn test_guest_probe_never_claims_ntfs_on_linux() {
    let response = request(
        &serde_json::json!({"protocol_version":1,"request_id":"a","run_id":"aabb","lease_token":"0123456789abcdef0123456789abcdef","op":"probe"}),
    );
    if !cfg!(windows) {
        assert_eq!(response["ok"], false);
        assert!(response["error"].as_str().expect("error").contains("Windows"));
    }
}

fn frame(id: &str, op: &str) -> serde_json::Value {
    serde_json::json!({"protocol_version":1,"request_id":id,"run_id":"aabb","lease_token":"0123456789abcdef0123456789abcdef","op":op})
}

#[test]
fn test_interrupted_upload_cannot_be_sealed() {
    let root = tempfile::tempdir().expect("upload fixture");
    let state = root.path().join("guest");
    let mut prepare = frame("start", "prepare");
    prepare["action"] = "start".into();
    prepare["manifest"] =
        serde_json::json!([{"path":"source with spaces/test.rs","directory":false,"size":5,"digest":"00".repeat(32)}]);
    assert_eq!(request_at(&state, &prepare)["ok"], true);
    let mut seal = frame("seal", "prepare");
    seal["action"] = "seal".into();
    let response = request_at(&state, &seal);
    assert_eq!(response["ok"], false);
    assert!(response["error"].as_str().expect("error").contains("incomplete upload"));
    let clean = request_at(&state, &frame("cleanup", "cleanup"));
    assert_eq!(clean["ok"], true);
    assert!(clean["payload"]["errors"].as_array().expect("errors").is_empty());
}

#[test]
fn test_cleanup_token_mismatch_preserves_registered_files() {
    let root = tempfile::tempdir().expect("token fixture");
    let state = root.path().join("guest");
    let mut prepare = frame("start", "prepare");
    prepare["action"] = "start".into();
    prepare["manifest"] = serde_json::json!([]);
    assert_eq!(request_at(&state, &prepare)["ok"], true);
    let mut attack = frame("cleanup", "cleanup");
    attack["lease_token"] = "ff".repeat(32).into();
    let response = request_at(&state, &attack);
    assert_eq!(response["ok"], false);
    assert!(response["error"].as_str().expect("error").contains("token mismatch"));
    assert_eq!(std::fs::read_dir(state.join("runs")).expect("runs remain").count(), 1);
    assert_eq!(request_at(&state, &frame("cleanup-valid", "cleanup"))["ok"], true);
}

#[test]
fn test_upload_rejects_outside_manifest_and_duplicate_requests() {
    let root = tempfile::tempdir().expect("upload fixture");
    let state = root.path().join("guest");
    let mut prepare = frame("start", "prepare");
    prepare["action"] = "start".into();
    prepare["manifest"] = serde_json::json!([]);
    assert_eq!(request_at(&state, &prepare)["ok"], true);
    let mut chunk = frame("chunk", "prepare");
    chunk["action"] = "chunk".into();
    chunk["path"] = "../escape".into();
    chunk["offset"] = 0.into();
    chunk["data"] = serde_json::json!([1]);
    let response = request_at(&state, &chunk);
    assert_eq!(response["ok"], false);
    assert!(
        response["error"]
            .as_str()
            .expect("error")
            .contains("not in snapshot manifest")
    );
    let response = request_at(&state, &chunk);
    assert!(response["error"].as_str().expect("error").contains("duplicate request"));
}

#[test]
fn test_prepare_rejects_windows_case_collision() {
    let mut prepare = frame("start", "prepare");
    prepare["action"] = "start".into();
    prepare["manifest"] = serde_json::json!([{"path":"Src/main.rs","directory":false,"size":0,"digest":"00".repeat(32)}, {"path":"src/MAIN.rs","directory":false,"size":0,"digest":"00".repeat(32)}]);
    let response = request(&prepare);
    assert_eq!(response["ok"], false);
    assert!(
        response["error"]
            .as_str()
            .expect("error")
            .contains("case-insensitive path collision")
    );
}

#[test]
fn test_wrong_token_prepare_does_not_allocate_an_orphan_run() {
    let root = tempfile::tempdir().expect("registration fixture");
    let state = root.path().join("guest");
    let mut start = frame("start", "prepare");
    start["action"] = "start".into();
    start["manifest"] = serde_json::json!([]);
    assert_eq!(request_at(&state, &start)["ok"], true);
    start["request_id"] = "attacker".into();
    start["lease_token"] = "ff".repeat(32).into();
    let response = request_at(&state, &start);
    assert_eq!(response["ok"], false);
    assert!(response["error"].as_str().expect("error").contains("token mismatch"));
    assert_eq!(
        std::fs::read_dir(state.join("runs")).expect("guest runs").count(),
        1,
        "a rejected registration must allocate no internal run"
    );
    assert_eq!(request_at(&state, &frame("clean", "cleanup"))["ok"], true);
    assert_eq!(std::fs::read_dir(state.join("runs")).expect("guest runs").count(), 0);
}

#[test]
fn test_prepare_start_receipt_prevents_cross_operation_replay() {
    let root = tempfile::tempdir().expect("receipt fixture");
    let state = root.path().join("guest");
    let mut start = frame("same-id", "prepare");
    start["action"] = "start".into();
    start["manifest"] = serde_json::json!([]);
    assert_eq!(request_at(&state, &start)["ok"], true);
    let mut seal = frame("same-id", "prepare");
    seal["action"] = "seal".into();
    let response = request_at(&state, &seal);
    assert_eq!(response["ok"], false);
    assert!(response["error"].as_str().expect("error").contains("duplicate request"));
}

#[test]
fn test_authenticated_status_needs_no_windows_execution_capabilities() {
    let root = tempfile::tempdir().expect("status fixture");
    let state = root.path().join("guest");
    let mut start = frame("start", "prepare");
    start["action"] = "start".into();
    start["manifest"] = serde_json::json!([]);
    assert_eq!(request_at(&state, &start)["ok"], true);
    let status = request_at(&state, &frame("status", "status"));
    assert_eq!(status["ok"], true);
    assert_eq!(status["payload"]["registered"], true);
    let mut invalid = frame("bad-status", "status");
    invalid["lease_token"] = "ff".repeat(32).into();
    assert_eq!(request_at(&state, &invalid)["ok"], false);
}

#[test]
fn test_guest_cleanup_retains_files_when_process_stop_identity_is_unknown() {
    let root = tempfile::tempdir().expect("pending process fixture");
    let state = root.path().join("guest");
    let mut start = frame("start", "prepare");
    start["action"] = "start".into();
    start["manifest"] = serde_json::json!([]);
    assert_eq!(request_at(&state, &start)["ok"], true);
    let registration: serde_json::Value = serde_json::from_slice(
        &std::fs::read(state.join("guest-registrations/aabb.json")).expect("guest registration"),
    )
    .expect("registration JSON");
    let internal = registration["internal_id"].as_str().expect("internal run ID");
    let record_path = state.join("records").join(format!("{internal}.json"));
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_path).expect("run registration")).expect("run JSON");
    record["environments"] = serde_json::json!([{"key":"guest-process-tree:unknown","owned":true,"stopped":false}]);
    std::fs::write(&record_path, serde_json::to_vec(&record).expect("pending registration"))
        .expect("persist pending fixture");
    let response = request_at(&state, &frame("clean", "cleanup"));
    assert_eq!(response["ok"], true);
    assert!(
        !response["payload"]["errors"]
            .as_array()
            .expect("cleanup errors")
            .is_empty()
    );
    assert!(
        state.join("runs").join(internal).exists(),
        "unknown process termination must retain run files"
    );
}

#[test]
fn test_prepare_initialization_failure_rolls_back_allocated_run() {
    let root = tempfile::tempdir().expect("registration rollback fixture");
    let state = root.path().join("guest");
    let mut start = frame("start", "prepare");
    start["action"] = "start".into();
    start["manifest"] =
        serde_json::json!([{"path":"a".repeat(300),"directory":false,"size":0,"digest":"00".repeat(32)}]);
    let response = request_at(&state, &start);
    assert_eq!(
        response["ok"], false,
        "filesystem rejection must not publish a prepared run"
    );
    assert_eq!(
        std::fs::read_dir(state.join("runs")).expect("guest run store").count(),
        0,
        "failed initialization must roll back its allocated run"
    );
    let registration: serde_json::Value = serde_json::from_slice(
        &std::fs::read(state.join("guest-registrations/aabb.json")).expect("authenticated rollback tombstone"),
    )
    .expect("tombstone JSON");
    assert_eq!(registration["state"], "rolledback");
}

#[test]
fn test_failed_prepare_has_authenticated_terminal_cleanup_state() {
    let root = tempfile::tempdir().expect("terminal rollback fixture");
    let state = root.path().join("guest");
    let mut start = frame("start", "prepare");
    start["action"] = "start".into();
    start["manifest"] =
        serde_json::json!([{"path":"a".repeat(300),"directory":false,"size":0,"digest":"00".repeat(32)}]);
    assert_eq!(request_at(&state, &start)["ok"], false);
    assert_eq!(std::fs::read_dir(state.join("runs")).expect("guest runs").count(), 0);
    let status = request_at(&state, &frame("recover-status", "status"));
    assert_eq!(
        status["ok"], true,
        "a completed rollback must remain token-authenticated for recovery"
    );
    assert_eq!(status["payload"]["registered"], true);
    assert_eq!(status["payload"]["state"], "rolledback");
    let cleanup = request_at(&state, &frame("recover-cleanup", "cleanup"));
    assert_eq!(cleanup["ok"], true);
    assert!(
        cleanup["payload"]["errors"]
            .as_array()
            .expect("cleanup errors")
            .is_empty()
    );
    let mut wrong = frame("wrong-token", "status");
    wrong["lease_token"] = "ff".repeat(32).into();
    assert_eq!(request_at(&state, &wrong)["ok"], false);
    let mut absent = frame("absent", "cleanup");
    absent["run_id"] = "bbcc".into();
    assert_eq!(
        request_at(&state, &absent)["ok"],
        false,
        "an arbitrary missing registration must still be rejected"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn test_host_clean_recovers_actual_guest_prepare_rollback() {
    use std::fs;
    use std::io::BufRead;
    use std::io::BufReader;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    use sha2::Digest;
    use sha2::Sha256;

    let root = tempfile::tempdir().expect("host/guest rollback recovery fixture");
    let project = root.path().join("project");
    let store = root.path().join("host-store");
    fs::create_dir_all(project.join("src")).expect("project sources");
    fs::create_dir_all(project.join(".infra/platform")).expect("suite directory");
    fs::create_dir_all(project.join(".infra/tools")).expect("toolchain directory");
    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname='rollback-host-fixture'\nversion='0.1.0'\nedition='2024'\n",
    )
    .expect("fixture manifest");
    fs::write(project.join("src/lib.rs"), "").expect("fixture source");
    fs::write(project.join(".infra/tools/defaults.toml"), "build_toolchain='1.94.0'\n").expect("shared toolchain");
    fs::write(project.join(".infra/platform/platform.toml"), "version=1\n[[suites]]\nid='fixture'\nplatforms=['linux']\ntimeout_seconds=10\nminimum_tests=0\n[[suites.commands]]\nkind='check'\nargv=['/bin/true']\ncwd='.'\n").expect("fixture suite");
    let machine = root.path().join("machine.toml");
    fs::write(
        &machine,
        toml::to_string(&serde_json::json!({"storage":{"root":store}})).expect("machine TOML"),
    )
    .expect("machine config");
    let binary = env!("CARGO_BIN_EXE_rs-infra-platform");
    let retained = Command::new(binary)
        .arg("--project")
        .arg(&project)
        .args(["check", "--platform", "linux", "--no-cleanup"])
        .env("RS_INFRA_PLATFORM_CONFIG", &machine)
        .env_remove("RS_INFRA_PLATFORM_CLEANUP")
        .output()
        .expect("allocate real retained host run");
    assert!(
        retained.status.success(),
        "host fixture: {}",
        String::from_utf8_lossy(&retained.stderr)
    );
    let record_path = fs::read_dir(store.join("records"))
        .expect("host records")
        .next()
        .expect("host record")
        .expect("record entry")
        .path();
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(&record_path).expect("host record bytes")).expect("record JSON");
    let run_id = record["run_id"].as_str().expect("host run ID");
    let entry = store.join("reports").join(run_id).join("entry-0");
    let digest = Sha256::digest(b"entry-0");
    let remote_id = format!(
        "{run_id}-{}",
        digest[..8].iter().map(|byte| format!("{byte:02x}")).collect::<String>()
    );
    let guest_root = root.path().join("guest-store");
    let mut start = frame("start", "prepare");
    start["run_id"] = remote_id.into();
    start["action"] = "start".into();
    start["manifest"] =
        serde_json::json!([{"path":"a".repeat(300),"directory":false,"size":0,"digest":"00".repeat(32)}]);
    assert_eq!(
        request_at(&guest_root, &start)["ok"],
        false,
        "real guest initialization must roll back"
    );
    let socket_dir = root.path().join("qmp");
    fs::create_dir(&socket_dir).expect("QMP directory");
    fs::write(socket_dir.join(".rs-infra-platform-qmp"), "windows-qmp-v1\n").expect("QMP marker");
    let socket = socket_dir.join("windows.qmp");
    let listener = UnixListener::bind(&socket).expect("borrowed QMP socket");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("host QMP connection");
        writeln!(stream, "{}", serde_json::json!({"QMP":{}})).expect("greeting");
        let mut reader = BufReader::new(stream.try_clone().expect("QMP reader"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("capabilities");
        writeln!(stream, "{}", serde_json::json!({"return":{},"id":"capabilities"})).expect("capabilities reply");
        line.clear();
        reader.read_line(&mut line).expect("status query");
        writeln!(
            stream,
            "{}",
            serde_json::json!({"return":{"status":"running"},"id":"command"})
        )
        .expect("status reply");
    });
    let ssh = root.path().join("guest-proxy");
    fs::write(&ssh, format!("#!/usr/bin/python3\nimport subprocess,sys,os\ne=os.environ.copy();e['RS_INFRA_PLATFORM_GUEST_ROOT']={:?}\nr=subprocess.run([{:?},'guest'],input=sys.stdin.buffer.read(),stdout=subprocess.PIPE,stderr=subprocess.PIPE,env=e)\nsys.stdout.buffer.write(r.stdout);sys.stderr.buffer.write(r.stderr);sys.exit(r.returncode)\n", guest_root.to_str().expect("guest root UTF-8"), binary)).expect("real guest SSH proxy");
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).expect("executable proxy");
    let image = root.path().join("image.qcow2");
    fs::write(&image, "fixture image").expect("image");
    fs::write(&machine, toml::to_string(&serde_json::json!({"storage":{"root":store},"windows":{"qemu":"/bin/true","image":image,"qmp_socket":socket,"ssh_alias":"fixture","guest_root":"C:\\fixture","ssh":ssh,"startup_timeout_seconds":1}})).expect("Windows machine TOML")).expect("Windows config");
    fs::write(entry.join("windows-session.json"), serde_json::to_vec(&serde_json::json!({"context":{"run_id":run_id,"host_run_dir":store.join("runs").join(run_id),"snapshot_root":store.join("runs").join(run_id).join("snapshot"),"project_relative":".","report_dir":entry,"toolchain":"1.94.0","cleanup":"enabled"},"token":start["lease_token"],"image":image,"qmp_socket":socket,"ssh_alias":"fixture","guest_pending":true,"owned_stop_pending":false,"owned":null})).expect("Windows session JSON")).expect("pending Windows session");
    let cleaned = Command::new(binary)
        .arg("--project")
        .arg(&project)
        .args(["clean", "--run", run_id])
        .env("RS_INFRA_PLATFORM_CONFIG", &machine)
        .output()
        .expect("real host cleanup recovery");
    server.join().expect("QMP server");
    assert!(
        cleaned.status.success(),
        "host must recover the authenticated rolled-back guest: {}",
        String::from_utf8_lossy(&cleaned.stderr)
    );
    let session: serde_json::Value =
        serde_json::from_slice(&fs::read(entry.join("windows-session.json")).expect("recovered session"))
            .expect("session JSON");
    assert_eq!(session["guest_pending"], false);
    assert!(
        !store.join("runs").join(run_id).exists(),
        "host cleanup must complete after authenticated remote rollback"
    );
}
