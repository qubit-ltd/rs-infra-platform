// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Resolves the coordinator's cleanup policy before starting any environment.

use anyhow::Result;

use crate::model::CleanupPolicy;

/// CLI selection takes precedence, even over an invalid environment value.
/// Returns an error for empty or unsupported environment values without a CLI
/// override.
pub(crate) fn resolve_cleanup(cli: Option<bool>, value: Option<&str>) -> Result<bool> {
    if let Some(enabled) = cli {
        return Ok(enabled);
    }
    match value {
        None => Ok(true),
        Some(raw) => match raw.to_ascii_lowercase().as_str() {
            "1" | "true" | "on" => Ok(true),
            "0" | "false" | "off" => Ok(false),
            _ => anyhow::bail!("invalid RS_INFRA_PLATFORM_CLEANUP value: {raw:?}"),
        },
    }
}

impl CleanupPolicy {
    /// Resolves the run policy, rejecting invalid environment values without a
    /// CLI override.
    pub(crate) fn resolve(cli: Option<bool>, value: Option<&str>) -> Result<Self> {
        Ok(if resolve_cleanup(cli, value)? {
            Self::Enabled
        } else {
            Self::Disabled
        })
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_cleanup;

    #[test]
    fn cleanup_defaults_and_precedence() {
        assert!(resolve_cleanup(None, None).expect("default"));
        assert!(!resolve_cleanup(None, Some("OFF")).expect("environment"));
        assert!(resolve_cleanup(Some(true), Some("0")).expect("CLI"));
        assert!(!resolve_cleanup(Some(false), Some("invalid")).expect("CLI override"));
        assert!(resolve_cleanup(None, Some("")).is_err());
        assert!(resolve_cleanup(None, Some("sometimes")).is_err());
    }
}
