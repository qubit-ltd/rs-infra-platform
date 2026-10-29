// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Backend implementations for native and guest environments.

pub(crate) mod android;
mod contract;
pub(crate) mod native;
pub(crate) mod windows;

pub(crate) use contract::Backend;
