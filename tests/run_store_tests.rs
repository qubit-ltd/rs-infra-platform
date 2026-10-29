// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
#[path = "internal/storage_fixture.rs"]
mod storage_fixture;

use std::fs;

use storage_fixture::Fixture;

#[test]
fn test_cli_records_and_reports_survive_disposable_run_cleanup() {
    let fixture = Fixture::new(false);
    let output = fixture.check(&[], None);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let record = fixture.record();
    let id = record["run_id"].as_str().expect("run ID");
    assert!(!fixture.store.join("runs").join(id).exists());
    assert!(fixture.store.join("reports").join(id).join("summary.json").is_file());
    assert!(!record["owner"].as_str().expect("owner token").is_empty());
    assert!(record["heartbeat_unix_ms"].as_u64().expect("heartbeat") > 0);
    assert_eq!(record["state"], "cleaned");
}

#[test]
fn test_cli_rejects_traversing_run_ids_and_preserves_external_files() {
    let fixture = Fixture::new(false);
    let outside = fixture.root.path().join("precious");
    fs::write(&outside, "user data").expect("outside file");
    let output = fixture.check(&["--no-cleanup"], None);
    assert!(output.status.success());
    let output = fixture
        .command()
        .args(["clean", "--run", "../../precious"])
        .output()
        .expect("unsafe clean request");
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(outside).expect("outside preserved"), "user data");
    let record = fixture.record();
    assert!(
        fixture
            .store
            .join("runs")
            .join(record["run_id"].as_str().expect("retained ID"))
            .is_dir()
    );
}
