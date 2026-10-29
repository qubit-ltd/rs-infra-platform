// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Durable execution summaries, separate from disposable build artifacts.

use std::io::Write;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use cap_std::fs::OpenOptions;
use serde::Serialize;

use crate::model::CleanupReport;
use crate::model::EnvironmentInfo;
use crate::model::ExecutionResult;
use crate::model::Platform;
use crate::run_store::secure_dir;

#[derive(Debug, Serialize)]
pub(crate) struct SuiteReport {
    pub(crate) suite_id: String,
    pub(crate) platform: Platform,
    pub(crate) status: String,
    pub(crate) environment: Option<EnvironmentInfo>,
    pub(crate) commands: Vec<ExecutionResult>,
    pub(crate) tests_passed: u64,
    pub(crate) tests_failed: u64,
    pub(crate) tests_ignored: u64,
    pub(crate) errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RunSummary {
    pub(crate) version: u32,
    pub(crate) run_id: String,
    pub(crate) success: bool,
    pub(crate) state: String,
    pub(crate) snapshot_digest: Option<String>,
    pub(crate) suites: Vec<SuiteReport>,
    pub(crate) errors: Vec<String>,
    pub(crate) cleanup: CleanupReport,
    pub(crate) report_dir: PathBuf,
    pub(crate) retained_path: Option<PathBuf>,
}

impl RunSummary {
    /// Writes and syncs a complete summary before atomically replacing its
    /// predecessor. Reports serialization and filesystem failures with the
    /// durable report path.
    pub(crate) fn persist(&self) -> Result<()> {
        let target = self.report_dir.join("summary.json");
        let directory = secure_dir(&self.report_dir, false)
            .with_context(|| format!("open report directory {}", self.report_dir.display()))?;
        let bytes = serde_json::to_vec_pretty(self)?;
        let mut file = directory
            .open_with("summary.json.tmp", OpenOptions::new().write(true).create_new(true))
            .with_context(|| format!("write report {}", target.display()))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        directory
            .rename("summary.json.tmp", &directory, "summary.json")
            .with_context(|| format!("publish report {}", target.display()))?;
        #[cfg(unix)]
        directory.open(".")?.sync_all()?;
        Ok(())
    }

    /// Prints truthful run outcomes; skipped suites never count as executed
    /// tests.
    pub(crate) fn print(&self) {
        for suite in &self.suites {
            println!("{} / {}: {}", suite.platform, suite.suite_id, suite.status);
            for error in &suite.errors {
                eprintln!("  {error}");
            }
        }
        if self
            .suites
            .iter()
            .all(|suite| suite.tests_passed + suite.tests_failed == 0)
        {
            println!("no platform tests executed");
        }
        println!("report: {}", self.report_dir.join("summary.json").display());
        if let Some(path) = &self.retained_path {
            println!(
                "retained artifacts: {}; reclaim with rs-infra-platform clean --run {}",
                path.display(),
                self.run_id
            );
        }
        for error in self.errors.iter().chain(self.cleanup.errors.iter()) {
            eprintln!("{error}");
        }
    }
}

#[derive(Debug)]
struct CompletedCommands {
    results: Vec<ExecutionResult>,
}

impl std::fmt::Display for CompletedCommands {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "infrastructure failure after {} completed commands",
            self.results.len()
        )
    }
}

/// Adds completed outcomes as a thin context while preserving the original
/// typed error chain, including process-stop and transport identity markers.
pub(crate) fn with_completed_results(error: anyhow::Error, results: &[ExecutionResult]) -> anyhow::Error {
    error.context(CompletedCommands {
        results: results.to_vec(),
    })
}

/// Reads completed outcomes from an infrastructure failure without consuming
/// it.
pub(crate) fn completed_results(error: &anyhow::Error) -> Option<&[ExecutionResult]> {
    error
        .downcast_ref::<CompletedCommands>()
        .map(|completed| completed.results.as_slice())
}
