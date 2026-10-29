// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Strict machine settings and the dedicated per-user state location.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostConfig {
    #[serde(default)]
    pub(crate) storage: StorageConfig,
    #[serde(default)]
    pub(crate) native: NativeConfig,
    pub(crate) windows: Option<toml::Value>,
    pub(crate) android: Option<toml::Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StorageConfig {
    #[serde(default)]
    pub(crate) root: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeConfig {
    #[serde(default = "default_cargo")]
    pub(crate) cargo: String,
}

fn default_cargo() -> String {
    "cargo".to_owned()
}

impl Default for NativeConfig {
    fn default() -> Self {
        Self { cargo: default_cargo() }
    }
}

impl HostConfig {
    /// Loads the explicitly selected machine file, or the optional user
    /// default. Rejects unknown fields, non-table backend settings, and
    /// nonabsolute state roots.
    pub(crate) fn load() -> Result<Self> {
        let explicit = std::env::var_os("RS_INFRA_PLATFORM_CONFIG");
        let path = match &explicit {
            Some(path) => PathBuf::from(path),
            None => home()?.join(".config/rs-infra/platform.toml"),
        };
        let mut settings = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str::<Self>(&text)
                .with_context(|| format!("parse machine configuration {}", path.display()))?,
            Err(error) if explicit.is_none() && error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                return Err(error).with_context(|| format!("read machine configuration {}", path.display()));
            }
        };
        if settings.storage.root.as_os_str().is_empty() {
            settings.storage.root = default_storage()?;
        }
        ensure!(
            settings.storage.root.is_absolute(),
            "storage.root must be an absolute path"
        );
        ensure!(
            !settings.native.cargo.trim().is_empty(),
            "native.cargo must name a program"
        );
        for (name, table) in [("windows", &settings.windows), ("android", &settings.android)] {
            ensure!(
                table.as_ref().is_none_or(toml::Value::is_table),
                "{name} must be a table"
            );
        }
        // Constructors validate schemas only; probes and environment startup stay
        // in the coordinator even for settings of an unselected backend.
        crate::backend::windows::WindowsBackend::from_table(settings.windows.as_ref())?;
        crate::backend::android::AndroidBackend::from_table(settings.android.as_ref())?;
        Ok(settings)
    }
}

fn home() -> Result<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .context("cannot determine user home directory")
}

fn default_storage() -> Result<PathBuf> {
    if cfg!(windows) {
        Ok(std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or(home()?.join("AppData/Local"))
            .join("rs-infra/platform"))
    } else if cfg!(target_os = "macos") {
        Ok(home()?.join("Library/Caches/rs-infra/platform"))
    } else {
        Ok(std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|path| Path::new(path).is_absolute())
            .unwrap_or(home()?.join(".cache"))
            .join("rs-infra/platform"))
    }
}
