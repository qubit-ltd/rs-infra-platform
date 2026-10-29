// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
#[path = "internal/storage_fixture.rs"]
mod storage_fixture;

use storage_fixture::Fixture;

#[test]
fn test_cli_environment_and_cleanup_switch_precedence() {
    for (environment, args, retained) in [
        (None, vec![], false),
        (Some("OFF"), vec![], true),
        (Some("tRuE"), vec![], false),
        (Some("invalid"), vec!["--cleanup"], false),
        (Some("invalid"), vec!["--no-cleanup"], true),
        (Some("1"), vec!["--no-cleanup"], true),
        (Some("0"), vec!["--cleanup"], false),
    ] {
        let fixture = Fixture::new(false);
        let output = fixture.check(&args, environment);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let record = fixture.record();
        assert_eq!(record["state"], if retained { "retained" } else { "cleaned" });
        assert_eq!(record["policy"], if retained { "disabled" } else { "enabled" });
    }
}

#[test]
fn test_empty_and_invalid_environment_fail_before_allocating_a_run() {
    for environment in ["", "sometimes"] {
        let fixture = Fixture::new(false);
        let output = fixture.check(&[], Some(environment));
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid RS_INFRA_PLATFORM_CLEANUP"));
        assert!(!fixture.store.exists());
    }
}
