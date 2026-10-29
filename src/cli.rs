// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Command-line contracts and initial dispatch for the platform coordinator.

use std::path::PathBuf;

use anyhow::Result;
use anyhow::bail;
use clap::Args;
use clap::Parser;
use clap::Subcommand;

use crate::config::ProjectConfig;
use crate::executor;
use crate::executor::Selection;
use crate::host_config::HostConfig;
use crate::model::Capability;
use crate::model::Platform;
use crate::model::RunId;
use crate::plan::build_plan;

/// Parses process arguments and runs the requested command.
///
/// Returns configuration, capability, execution, or cleanup errors to the
/// caller.
pub fn run() -> Result<()> {
    dispatch(Cli::parse())
}

#[derive(Debug, Parser)]
#[command(version, about = "Run project platform suites in local environments")]
pub(crate) struct Cli {
    #[arg(long, global = true, default_value = ".")]
    pub(crate) project: PathBuf,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    Doctor,
    Plan(SelectionArgs),
    Check(CheckArgs),
    Setup(SetupArgs),
    Clean(CleanArgs),
    #[command(hide = true)]
    Guest,
    #[command(hide = true)]
    AndroidRunner(AndroidRunnerArgs),
    #[command(hide = true)]
    AndroidOwner(AndroidOwnerArgs),
}

#[derive(Debug, Args)]
pub(crate) struct SelectionArgs {
    #[arg(long, value_delimiter = ',')]
    pub(crate) platform: Vec<Platform>,
    #[arg(long, value_delimiter = ',')]
    pub(crate) suite: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    pub(crate) require: Vec<Platform>,
}

#[derive(Debug, Args)]
pub(crate) struct CheckArgs {
    #[command(flatten)]
    pub(crate) selection: SelectionArgs,
    #[arg(long, conflicts_with = "no_cleanup")]
    pub(crate) cleanup: bool,
    #[arg(long, conflicts_with = "cleanup")]
    pub(crate) no_cleanup: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SetupArgs {
    #[arg(long, value_delimiter = ',')]
    pub(crate) platform: Vec<Platform>,
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub(crate) struct CleanArgs {
    #[arg(long)]
    pub(crate) run: Option<String>,
    #[arg(long)]
    pub(crate) stale: bool,
}

#[derive(Debug, Args)]
pub(crate) struct AndroidRunnerArgs {
    #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
    pub(crate) argv: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct AndroidOwnerArgs {
    #[arg(long)]
    pub(crate) context: PathBuf,
    #[arg(long)]
    pub(crate) child: bool,
}

/// Dispatches read-only diagnostics or the owned-run execution lifecycle.
/// Returns selected-suite, execution, report, and cleanup failures to the CLI.
pub(crate) fn dispatch(cli: Cli) -> Result<()> {
    let required = match &cli.command {
        Command::Check(args) => Some(args.selection.require.as_slice()),
        Command::Plan(args) => Some(args.require.as_slice()),
        Command::Doctor => Some([].as_slice()),
        _ => None,
    };
    if let Some(required) = required
        && supported_host(Platform::host(), required)?.is_none()
    {
        println!("no platform tests executed");
        return Ok(());
    }
    match cli.command {
        Command::Doctor => {
            println!(
                "diagnostic: host={}; no installation or environment startup performed",
                Platform::host()?
            );
            let path = cli.project.join(".infra/platform/platform.toml");
            if path.exists() {
                ProjectConfig::load(&cli.project)?;
                println!("project configuration: valid");
            } else {
                println!("project configuration: unconfigured ({})", path.display());
            }
            let settings = HostConfig::load()?;
            let host = Platform::host()?;
            let mut broken = Vec::new();
            for platform in [Platform::Linux, Platform::Windows, Platform::Macos, Platform::Android] {
                match executor::backend_for(platform, crate::plan::route(host, platform), &settings)?.probe()? {
                    Capability::Ready => println!("{platform}: ready"),
                    Capability::Unconfigured(reason) | Capability::Unsupported(reason) => {
                        println!("WARN+SKIP {platform}: {reason}")
                    }
                    Capability::Broken(reason) => {
                        println!("ERROR {platform}: {reason}");
                        broken.push(reason);
                    }
                }
            }
            if !broken.is_empty() {
                bail!("broken platform capabilities: {}", broken.join("; "));
            }
        }
        Command::Plan(selection) => {
            let config = ProjectConfig::load(&cli.project)?;
            let plan = build_plan(
                &config,
                Platform::host()?,
                &selection.platform,
                &selection.suite,
                &selection.require,
            )?;
            let settings = HostConfig::load()?;
            let mut value = serde_json::to_value(&plan)?;
            for (entry, output) in plan
                .entries
                .iter()
                .zip(value["entries"].as_array_mut().into_iter().flatten())
            {
                let capability = executor::backend_for(entry.platform, entry.route, &settings)?.probe()?;
                output["capability"] = serde_json::to_value(capability)?;
                if let Some(suite) = config.suites.iter().find(|suite| suite.id == entry.suite_id) {
                    output["commands"] = serde_json::to_value(&suite.commands)?;
                    output["target"] = serde_json::to_value(&suite.target)?;
                }
            }
            value["cleanup"] = serde_json::to_value(executor::cleanup_policy(None)?)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Command::Check(args) => {
            let summary = executor::execute_project(
                &cli.project,
                &Selection {
                    platforms: args.selection.platform,
                    suites: args.selection.suite,
                    required: args.selection.require,
                    cleanup: if args.cleanup {
                        Some(true)
                    } else if args.no_cleanup {
                        Some(false)
                    } else {
                        None
                    },
                },
            )?;
            summary.print();
            if !summary.success {
                bail!(
                    "platform run {} failed; report {}",
                    summary.run_id,
                    summary.report_dir.join("summary.json").display()
                );
            }
        }
        Command::Setup(args) => {
            let platforms = if args.platform.is_empty() {
                vec![Platform::Linux, Platform::Windows, Platform::Macos, Platform::Android]
            } else {
                args.platform
            };
            for platform in platforms {
                match platform {
                    Platform::Windows => println!(
                        "windows: manual preparation required: install VM image, MSVC Rust, build tools, OpenSSH and guest runner"
                    ),
                    Platform::Android => println!(
                        "android: prepare SDK, NDK, matching emulator ABI and AVD; accept vendor licenses manually"
                    ),
                    Platform::Linux | Platform::Macos => {
                        println!("{platform}: prepare the shared Rust toolchain and suite requirements")
                    }
                }
            }
        }
        Command::Clean(args) => {
            let settings = HostConfig::load()?;
            let reports = if let Some(id) = args.run {
                vec![executor::clean_registered_run(&settings, &RunId(id), true)?]
            } else {
                executor::recover_registered_runs(&settings)?
            };
            let mut errors = Vec::new();
            for report in reports {
                for path in &report.remaining {
                    println!("remaining: {}", path.display());
                }
                errors.extend(report.errors);
            }
            if !errors.is_empty() {
                bail!("cleanup failed: {}", errors.join("; "));
            }
            println!("cleanup complete");
        }
        Command::Guest => crate::guest::run()?,
        Command::AndroidRunner(args) => std::process::exit(crate::android_runner::run(&args.argv)?),
        Command::AndroidOwner(args) => {
            std::process::exit(crate::backend::android::emulator_owner(&args.context, args.child)?)
        }
    }
    Ok(())
}

fn supported_host(host: Result<Platform>, required: &[Platform]) -> Result<Option<Platform>> {
    match host {
        Ok(host) => Ok(Some(host)),
        Err(error) if required.is_empty() => {
            eprintln!("WARN+SKIP: {error}");
            Ok(None)
        }
        Err(error) => bail!("required platform cannot execute: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::supported_host;
    use crate::model::Platform;

    #[test]
    fn test_unknown_host_skips_unless_a_platform_is_required() {
        assert!(
            supported_host(
                Err(anyhow::anyhow!("unsupported host operating system: fixture-os")),
                &[]
            )
            .expect("optional unsupported host")
            .is_none()
        );
        let error = supported_host(
            Err(anyhow::anyhow!("unsupported host operating system: fixture-os")),
            &[Platform::Linux],
        )
        .expect_err("required platform cannot run on unknown host");
        assert!(error.to_string().contains("required platform"));
    }
}
