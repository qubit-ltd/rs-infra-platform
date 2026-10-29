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

use serde_json::Value;
use tempfile::TempDir;

pub struct Fixture {
    pub root: TempDir,
    pub project: PathBuf,
    pub store: PathBuf,
}

impl Fixture {
    pub fn new(native: bool) -> Self {
        let root = tempfile::tempdir().expect("storage CLI fixture");
        let project = root.path().join("project");
        let store = root.path().join("store");
        fs::create_dir_all(project.join("src")).expect("source directory");
        fs::create_dir_all(project.join(".infra/platform")).expect("suite directory");
        fs::create_dir_all(project.join(".infra/tools")).expect("toolchain directory");
        fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"storage-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .expect("manifest");
        fs::write(project.join("src/lib.rs"), "pub fn value() -> u8 { 42 }\n").expect("source");
        fs::write(
            project.join(".infra/tools/defaults.toml"),
            "build_toolchain = \"1.94.0\"\n",
        )
        .expect("shared toolchain");
        let platform = if native {
            std::env::consts::OS
        } else if cfg!(target_os = "macos") {
            "linux"
        } else {
            "macos"
        };
        fs::write(project.join(".infra/platform/platform.toml"), format!("version = 1\n[[suites]]\nid = \"storage\"\nplatforms = [\"{platform}\"]\ntimeout_seconds = 60\nminimum_tests = 0\n[[suites.commands]]\nkind = \"check\"\nargv = [\"cargo\", \"check\", \"--offline\"]\ncwd = \".\"\n")).expect("suite");
        fs::write(
            root.path().join("machine.toml"),
            format!("[storage]\nroot = {store:?}\n"),
        )
        .expect("machine settings");
        Self { root, project, store }
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"));
        command
            .arg("--project")
            .arg(&self.project)
            .env("RS_INFRA_PLATFORM_CONFIG", self.root.path().join("machine.toml"))
            .env_remove("RS_INFRA_PLATFORM_CLEANUP");
        command
    }

    pub fn check(&self, args: &[&str], environment: Option<&str>) -> Output {
        let mut command = self.command();
        command.arg("check").args(args);
        if let Some(value) = environment {
            command.env("RS_INFRA_PLATFORM_CLEANUP", value);
        }
        command.output().expect("run CLI check")
    }

    pub fn record(&self) -> Value {
        let record = fs::read_dir(self.store.join("records"))
            .expect("registered records")
            .map(|entry| entry.expect("record entry").path())
            .find(|path| path.extension().is_some_and(|extension| extension == "json"))
            .expect("JSON record");
        read_json(&record)
    }
}

pub fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("JSON file")).expect("valid JSON")
}
