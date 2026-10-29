// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Strict project configuration parsing and project-relative path validation.

use std::collections::BTreeSet;
use std::fs;
use std::path::Component;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;

use crate::model::SuiteSpec;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectConfig {
    pub(crate) version: u32,
    pub(crate) suites: Vec<SuiteSpec>,
}

impl ProjectConfig {
    /// Reads project suites without modifying the project or launching
    /// processes. Rejects unsupported schemas, invalid commands, and paths
    /// outside the project.
    pub(crate) fn load(project: &Path) -> Result<Self> {
        let path = project.join(".infra/platform/platform.toml");
        let text =
            fs::read_to_string(&path).with_context(|| format!("read platform configuration {}", path.display()))?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parse platform configuration {}", path.display()))?;
        config.validate(project)?;
        Ok(config)
    }

    fn validate(&self, project: &Path) -> Result<()> {
        ensure!(self.version == 1, "unsupported version: {}", self.version);
        ensure!(!self.suites.is_empty(), "configuration contains no suites");
        let root = project.canonicalize().context("resolve project root")?;
        let mut ids = BTreeSet::new();
        for suite in &self.suites {
            ensure!(!suite.id.trim().is_empty(), "suite id must not be empty");
            ensure!(ids.insert(&suite.id), "duplicate suite id: {}", suite.id);
            ensure!(!suite.platforms.is_empty(), "suite {} has no platforms", suite.id);
            ensure!(!suite.commands.is_empty(), "suite {} has no commands", suite.id);
            ensure!(suite.timeout_seconds > 0, "suite {} timeout must be positive", suite.id);
            if let Some(target) = &suite.target {
                ensure!(!target.trim().is_empty(), "suite {} target must not be empty", suite.id);
            }
            for command in &suite.commands {
                ensure!(
                    !command.argv.is_empty() && !command.argv[0].trim().is_empty(),
                    "suite {} argv must name a program",
                    suite.id
                );
                let program = command.argv[0].rsplit(['/', '\\']).next().unwrap_or_default();
                let cargo = program.eq_ignore_ascii_case("cargo") || program.eq_ignore_ascii_case("cargo.exe");
                ensure!(
                    !(cargo && command.argv.get(1).is_some_and(|arg| arg.starts_with('+'))),
                    "suite {} argv must not embed a +toolchain; use the shared toolchain",
                    suite.id
                );
                validate_path(&root, &command.cwd).with_context(|| format!("suite {} cwd", suite.id))?;
            }
            for path in &suite.data {
                validate_path(&root, path).with_context(|| format!("suite {} data", suite.id))?;
            }
        }
        Ok(())
    }
}

fn validate_path(root: &Path, path: &Path) -> Result<()> {
    let raw = path.to_string_lossy();
    let foreign_absolute = raw.starts_with('\\') || raw.as_bytes().get(1) == Some(&b':');
    let foreign_parent = raw.split('\\').any(|part| part == "..");
    ensure!(
        !raw.is_empty()
            && !foreign_absolute
            && !foreign_parent
            && !path.is_absolute()
            && !path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_) | Component::RootDir)),
        "expected a relative project path: {}",
        path.display()
    );
    let joined = root.join(path);
    let mut ancestor = joined.as_path();
    while !ancestor.exists() {
        if fs::symlink_metadata(ancestor).is_ok() {
            bail!(
                "relative project path has an unresolved symbolic link: {}",
                path.display()
            );
        }
        ancestor = ancestor
            .parent()
            .context("relative project path has no existing parent")?;
    }
    ensure!(
        ancestor
            .canonicalize()
            .context("resolve relative project path")?
            .starts_with(root),
        "relative project path escapes through a symbolic link: {}",
        path.display()
    );
    Ok(())
}
