// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Shared backend lifecycle contract.

use anyhow::Result;

use crate::model::Capability;
use crate::model::CleanupReport;
use crate::model::EnvironmentInfo;
use crate::model::ExecutionResult;
use crate::model::SessionContext;
use crate::model::SuiteSpec;

/// Implementations receive machine-specific settings in their constructors.
pub(crate) trait Backend {
    /// Reports readiness without installing tools or starting environments.
    fn probe(&self) -> Result<Capability>;
    /// Prepares this run and returns its actual environment; startup errors are
    /// fatal.
    fn start(&mut self, context: &SessionContext) -> Result<EnvironmentInfo>;
    /// Executes suite commands and records their actual outcomes and log paths.
    fn execute(&mut self, suite: &SuiteSpec, context: &SessionContext) -> Result<Vec<ExecutionResult>>;
    /// Removes only owned run artifacts, preserving cleanup failures in the
    /// report.
    fn cleanup(&mut self, context: &SessionContext) -> Result<CleanupReport>;
    /// Stops environments started by this tool, including when cleanup is
    /// disabled.
    fn stop(&mut self) -> Result<()>;
}
