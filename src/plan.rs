// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Side-effect-free platform selection and host-to-environment routing.

use anyhow::Result;
use anyhow::ensure;
use serde::Serialize;

use crate::config::ProjectConfig;
use crate::model::Platform;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Route {
    Native,
    WindowsVm,
    AndroidEmulator,
    Unsupported,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PlanEntry {
    pub(crate) suite_id: String,
    pub(crate) platform: Platform,
    pub(crate) route: Route,
    pub(crate) required: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ExecutionPlan {
    pub(crate) version: u32,
    pub(crate) host: Platform,
    pub(crate) entries: Vec<PlanEntry>,
}

/// Resolves a host/target pair without probing or starting the environment.
pub(crate) fn route(host: Platform, target: Platform) -> Route {
    match (host, target) {
        (left, right) if left == right && left != Platform::Android => Route::Native,
        (Platform::Linux, Platform::Windows) => Route::WindowsVm,
        (Platform::Linux | Platform::Macos, Platform::Android) => Route::AndroidEmulator,
        _ => Route::Unsupported,
    }
}

/// Builds a read-only plan, rejecting unknown suite filters and required
/// platforms excluded by the chosen suites or platform filter.
pub(crate) fn build_plan(
    config: &ProjectConfig,
    host: Platform,
    platforms: &[Platform],
    suites: &[String],
    required: &[Platform],
) -> Result<ExecutionPlan> {
    for id in suites {
        ensure!(config.suites.iter().any(|suite| &suite.id == id), "unknown suite: {id}");
    }
    let entries: Vec<PlanEntry> = config
        .suites
        .iter()
        .filter(|suite| suites.is_empty() || suites.contains(&suite.id))
        .flat_map(|suite| {
            suite
                .platforms
                .iter()
                .filter(|platform| platforms.is_empty() || platforms.contains(platform))
                .map(|platform| PlanEntry {
                    suite_id: suite.id.clone(),
                    platform: *platform,
                    route: route(host, *platform),
                    required: required.contains(platform),
                })
        })
        .collect();
    for platform in required {
        ensure!(
            entries.iter().any(|entry| entry.platform == *platform),
            "required platform {platform} is absent from selected suites"
        );
    }
    Ok(ExecutionPlan {
        version: 1,
        host,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use crate::model::Platform;
    use crate::plan::Route;
    use crate::plan::route;

    #[test]
    fn test_all_host_target_routes() {
        for host in [Platform::Linux, Platform::Windows, Platform::Macos] {
            for target in [Platform::Linux, Platform::Windows, Platform::Macos, Platform::Android] {
                let expected = match (host, target) {
                    (left, right) if left == right => Route::Native,
                    (Platform::Linux, Platform::Windows) => Route::WindowsVm,
                    (Platform::Linux | Platform::Macos, Platform::Android) => Route::AndroidEmulator,
                    _ => Route::Unsupported,
                };
                assert_eq!(route(host, target), expected, "host={host}, target={target}");
            }
        }
    }
}
