// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Local platform test coordinator. Execution contracts remain crate-internal.

mod android_runner;
mod backend;
mod cleanup;
mod cleanup_policy;
mod cli;
mod config;
mod executor;
mod guest;
mod host_config;
mod lease;
mod model;
mod plan;
mod process;
mod process_identity;
mod report;
mod reuse;
mod run_store;
mod snapshot;
mod transfer;

pub use cli::run;
