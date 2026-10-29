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
use storage_fixture::read_json;

#[test]
fn test_cli_builds_recursive_path_dependencies_from_retained_snapshot_only() {
    let fixture = Fixture::new(true);
    let dependency = fixture.root.path().join("dependency");
    let transitive = fixture.root.path().join("transitive");
    for (path, name, dependency, source) in [
        (
            &dependency,
            "dependency",
            "[dependencies]\ntransitive = { path = \"../transitive\" }\n",
            "pub fn value() -> u8 { transitive::value() }\n",
        ),
        (&transitive, "transitive", "", "pub fn value() -> u8 { 42 }\n"),
    ] {
        fs::create_dir_all(path.join("src")).expect("dependency source directory");
        fs::write(
            path.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{dependency}"),
        )
        .expect("dependency manifest");
        fs::write(path.join("src/lib.rs"), source).expect("dependency source");
    }
    let manifest = fs::read_to_string(fixture.project.join("Cargo.toml")).expect("project manifest");
    fs::write(
        fixture.project.join("Cargo.toml"),
        format!("{manifest}[dependencies]\ndependency = {{ path = \"../dependency\" }}\n"),
    )
    .expect("local dependency manifest");
    fs::write(
        fixture.project.join("src/lib.rs"),
        "pub fn value() -> u8 { dependency::value() }\n",
    )
    .expect("modified project source");
    fs::write(fixture.project.join("untracked.txt"), "current workspace input").expect("new input");
    fs::create_dir(fixture.project.join("target")).expect("original target");
    fs::write(fixture.project.join("target/keep"), "user artifact").expect("original artifact");
    let output = fixture.check(&["--no-cleanup"], None);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let record = fixture.record();
    let id = record["run_id"].as_str().expect("run ID");
    let snapshot = read_json(&fixture.store.join("reports").join(id).join("snapshot.json"));
    assert!(!snapshot["digest"].as_str().expect("content digest").is_empty());
    let root = std::path::Path::new(snapshot["root"].as_str().expect("snapshot root"));
    assert_eq!(
        fs::read_to_string(root.join("project/untracked.txt")).expect("snapshotted input"),
        "current workspace input"
    );
    assert!(root.join("dependency/Cargo.toml").is_file());
    assert!(root.join("transitive/src/lib.rs").is_file());
    assert!(!root.join("project/target").exists());
    assert!(root.join("project/Cargo.lock").is_file());
    for path in [&fixture.project, &dependency, &transitive] {
        assert!(!path.join("Cargo.lock").exists());
    }
    assert_eq!(
        fs::read_to_string(fixture.project.join("target/keep")).expect("original target"),
        "user artifact"
    );
}

#[cfg(unix)]
#[test]
fn test_cli_rejects_external_absolute_and_escaping_symlink_chains_before_test_writes() {
    use std::os::unix::fs::symlink;
    for escaping_chain in [false, true] {
        let fixture = Fixture::new(true);
        let original = if escaping_chain {
            fixture.project.join("original-data")
        } else {
            fixture.root.path().join("external-data")
        };
        fs::write(&original, "original bytes").expect("original data");
        if escaping_chain {
            let target = format!(
                "{}{}",
                "../".repeat(64),
                original.strip_prefix("/").expect("absolute original").display()
            );
            symlink(target, fixture.project.join("intermediate-link")).expect("escaping intermediate link");
            symlink("intermediate-link", fixture.project.join("linked-data")).expect("relative link chain");
        } else {
            symlink(&original, fixture.project.join("linked-data")).expect("absolute original-data link");
        }
        fs::write(fixture.project.join("src/lib.rs"), "#[test]\nfn snapshot_write() { std::fs::write(\"linked-data\", \"modified from snapshot\").expect(\"linked write\"); }\n").expect("real write test");
        let configuration = fixture.project.join(".infra/platform/platform.toml");
        let text = fs::read_to_string(&configuration)
            .expect("suite config")
            .replace("minimum_tests = 0", "minimum_tests = 1")
            .replace("kind = \"check\"", "kind = \"test\"")
            .replace("\"cargo\", \"check\"", "\"cargo\", \"test\"");
        fs::write(configuration, text).expect("test suite config");
        let output = fixture.check(&[], None);
        assert_eq!(
            fs::read_to_string(&original).expect("original data after real CLI"),
            "original bytes",
            "snapshot must never write through a link into the original workspace"
        );
        assert!(
            !output.status.success(),
            "unsafe links must be rejected before Cargo tests execute"
        );
        let record = fixture.record();
        let summary = read_json(
            &fixture
                .store
                .join("reports")
                .join(record["run_id"].as_str().expect("run ID"))
                .join("summary.json"),
        );
        assert!(
            summary.to_string().contains("symlink"),
            "clear symlink diagnostic: {summary}"
        );
    }
}

#[test]
fn test_cli_rejects_absolute_dependency_instead_of_building_original_crate() {
    let fixture = Fixture::new(true);
    let dependency = fixture.root.path().join("dependency");
    fs::create_dir_all(dependency.join("src")).expect("dependency source directory");
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname = \"dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("dependency manifest");
    let dependency_source = "pub fn origin() -> &'static str { env!(\"CARGO_MANIFEST_DIR\") }\n";
    fs::write(dependency.join("src/lib.rs"), dependency_source).expect("manifest directory evidence");
    let original_manifest = fs::read_to_string(fixture.project.join("Cargo.toml")).expect("project manifest");
    fs::write(
        fixture.project.join("Cargo.toml"),
        format!("{original_manifest}[dependencies]\ndependency = {{ path = {dependency:?} }}\n"),
    )
    .expect("absolute path dependency");
    fs::write(
        fixture.project.join("src/lib.rs"),
        "pub fn origin() -> &'static str { dependency::origin() }\n",
    )
    .expect("dependency consumer");
    let output = fixture.check(&[], None);
    assert!(
        !output.status.success(),
        "absolute dependency must be rejected rather than cargo-checking its original directory"
    );
    let record = fixture.record();
    let summary = read_json(
        &fixture
            .store
            .join("reports")
            .join(record["run_id"].as_str().expect("run ID"))
            .join("summary.json"),
    );
    assert!(
        summary.to_string().contains("absolute Cargo path dependency"),
        "clear absolute dependency diagnostic: {summary}"
    );
    assert_eq!(
        fs::read_to_string(dependency.join("src/lib.rs")).expect("original dependency source"),
        dependency_source
    );
    assert!(!dependency.join("Cargo.lock").exists());
    assert!(!fixture.project.join("Cargo.lock").exists());
}

#[cfg(unix)]
#[test]
fn test_cli_internal_relative_link_writes_only_the_snapshot_copy() {
    assert_internal_link_writes_snapshot(false);
}

#[cfg(unix)]
#[test]
fn test_cli_internal_absolute_link_is_relocated_before_test_writes() {
    assert_internal_link_writes_snapshot(true);
}

#[cfg(unix)]
fn assert_internal_link_writes_snapshot(absolute: bool) {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new(true);
    let original = fixture.project.join("original-data");
    fs::write(&original, "original bytes").expect("original data");
    let target = if absolute {
        original.as_path()
    } else {
        std::path::Path::new("original-data")
    };
    symlink(target, fixture.project.join("linked-data")).expect("internal link");
    fs::write(fixture.project.join("src/lib.rs"), "#[test]\nfn snapshot_write() { std::fs::write(\"linked-data\", \"snapshot bytes\").expect(\"write through internal link\"); }\n").expect("actual link-write test");
    let config = fixture.project.join(".infra/platform/platform.toml");
    let text = fs::read_to_string(&config)
        .expect("suite config")
        .replace("minimum_tests = 0", "minimum_tests = 1")
        .replace("kind = \"check\"", "kind = \"test\"")
        .replace("\"cargo\", \"check\"", "\"cargo\", \"test\"");
    fs::write(config, text).expect("test suite config");
    let output = fixture.check(&["--no-cleanup"], None);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        fs::read_to_string(original).expect("original after actual test"),
        "original bytes"
    );
    let record = fixture.record();
    let snapshot = read_json(
        &fixture
            .store
            .join("reports")
            .join(record["run_id"].as_str().expect("run ID"))
            .join("snapshot.json"),
    );
    let root = std::path::Path::new(snapshot["root"].as_str().expect("snapshot root"));
    assert_eq!(
        fs::read_link(root.join("linked-data")).expect("relocated link"),
        std::path::PathBuf::from("original-data")
    );
    let linked = snapshot["entries"]
        .as_array()
        .expect("snapshot manifest")
        .iter()
        .find(|entry| entry["relative_path"] == "linked-data")
        .expect("link manifest entry");
    assert_eq!(linked["symlink_target"], "original-data");
    assert_eq!(
        fs::read_to_string(root.join("original-data")).expect("snapshot write result"),
        "snapshot bytes"
    );
}
