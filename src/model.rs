// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Internal platform configuration and execution contracts.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::Result as FmtResult;
use std::path::PathBuf;

use clap::ValueEnum;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Platform {
    Linux,
    Windows,
    Android,
    Macos,
}

impl Display for Platform {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> FmtResult {
        formatter.write_str(match self {
            Self::Linux => "linux",
            Self::Windows => "windows",
            Self::Android => "android",
            Self::Macos => "macos",
        })
    }
}

impl Platform {
    /// Returns the current host, or an error for an unsupported host OS.
    pub(crate) fn host() -> anyhow::Result<Self> {
        match std::env::consts::OS {
            "linux" => Ok(Self::Linux),
            "windows" => Ok(Self::Windows),
            "macos" => Ok(Self::Macos),
            os => anyhow::bail!("unsupported host operating system: {os}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CommandKind {
    Check,
    Build,
    Test,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandSpec {
    pub(crate) kind: CommandKind,
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SuiteSpec {
    pub(crate) id: String,
    pub(crate) platforms: Vec<Platform>,
    pub(crate) target: Option<String>,
    pub(crate) commands: Vec<CommandSpec>,
    pub(crate) timeout_seconds: u64,
    pub(crate) minimum_tests: u64,
    #[serde(default)]
    pub(crate) requirements: Vec<String>,
    #[serde(default)]
    pub(crate) data: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CleanupPolicy {
    Enabled,
    Disabled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct RunId(pub(crate) String);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionResult {
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) cancelled: bool,
    pub(crate) tests_passed: u64,
    pub(crate) tests_failed: u64,
    pub(crate) tests_ignored: u64,
    pub(crate) stdout: PathBuf,
    pub(crate) stderr: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) enum Capability {
    Ready,
    Unconfigured(String),
    Unsupported(String),
    Broken(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvironmentInfo {
    pub(crate) platform: Platform,
    pub(crate) target: String,
    pub(crate) os_version: String,
    pub(crate) filesystem: String,
    pub(crate) versions: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionContext {
    pub(crate) run_id: RunId,
    pub(crate) host_run_dir: PathBuf,
    pub(crate) snapshot_root: PathBuf,
    pub(crate) project_relative: PathBuf,
    pub(crate) report_dir: PathBuf,
    pub(crate) toolchain: String,
    pub(crate) cleanup: CleanupPolicy,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupReport {
    pub(crate) remaining: Vec<PathBuf>,
    pub(crate) errors: Vec<String>,
    pub(crate) host_reclaimed_bytes: Option<u64>,
    pub(crate) guest_available_delta: Option<u64>,
}
