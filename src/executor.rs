// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Coordinates probes, owned snapshots, backend finalization and durable
//! reports.

use std::fs;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use sha2::Digest;
use sha2::Sha256;

use crate::backend::Backend;
use crate::backend::native::NativeBackend;
use crate::cleanup;
use crate::config::ProjectConfig;
use crate::host_config::HostConfig;
use crate::lease::Lease;
use crate::model::Capability;
use crate::model::CleanupPolicy;
use crate::model::CleanupReport;
use crate::model::CommandKind;
use crate::model::EnvironmentInfo;
use crate::model::ExecutionResult;
use crate::model::Platform;
use crate::model::SessionContext;
use crate::model::SuiteSpec;
use crate::plan::Route;
use crate::plan::build_plan;
use crate::report::RunSummary;
use crate::report::SuiteReport;
use crate::run_store::RunRecord;
use crate::run_store::RunState;
use crate::run_store::RunStore;
use crate::run_store::open_store;
use crate::snapshot::Snapshot;

static STOP_NONCE: AtomicU64 = AtomicU64::new(0);
static CANCELLED: AtomicBool = AtomicBool::new(false);
static SIGNALS: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Debug, Default)]
pub(crate) struct Selection {
    pub(crate) platforms: Vec<Platform>,
    pub(crate) suites: Vec<String>,
    pub(crate) required: Vec<Platform>,
    pub(crate) cleanup: Option<bool>,
}

pub(crate) type BackendFactory = fn(Platform, Route, &HostConfig) -> Result<Box<dyn Backend>>;

/// Shares the coordinator cancellation state with bounded backend processes.
pub(crate) fn cancelled() -> &'static AtomicBool {
    &CANCELLED
}

/// Installs graceful first-signal cancellation and immediate second-signal
/// exit. Returns handler-registration failures before any execution environment
/// starts.
pub(crate) fn install_signals() -> Result<()> {
    let result = SIGNALS.get_or_init(|| {
        ctrlc::set_handler(|| {
            if CANCELLED.swap(true, Ordering::SeqCst) {
                std::process::exit(130);
            }
        })
        .map_err(|error| error.to_string())
    });
    result
        .as_ref()
        .map_err(|error| anyhow::anyhow!("install cancellation handlers: {error}"))?;
    Ok(())
}

/// Creates the backend selected by the host routing matrix without starting it.
/// Machine-specific backend constructors validate their own settings.
pub(crate) fn backend_for(platform: Platform, route: Route, settings: &HostConfig) -> Result<Box<dyn Backend>> {
    match route {
        Route::Native => Ok(Box::new(NativeBackend::new(settings.native.clone()))),
        Route::Unsupported => Ok(Box::new(UnavailableBackend {
            capability: Capability::Unsupported(format!("{platform} is unsupported on this host")),
        })),
        Route::WindowsVm => Ok(Box::new(crate::backend::windows::WindowsBackend::from_table(
            settings.windows.as_ref(),
        )?)),
        Route::AndroidEmulator => Ok(Box::new(crate::backend::android::AndroidBackend::from_table(
            settings.android.as_ref(),
        )?)),
    }
}

/// Executes selected suites using owned snapshots and always attempts
/// finalization. Configuration and store allocation errors return `Err`;
/// execution and cleanup failures are preserved together in a persisted
/// unsuccessful summary.
pub(crate) fn execute_project(project: &Path, selection: &Selection) -> Result<RunSummary> {
    execute_project_with_factory(project, selection, backend_for)
}

/// Executes a run with the registered backend constructor and identical
/// finalization. A constructor or probe error affects its suite while
/// independent suites continue.
pub(crate) fn execute_project_with_factory(
    project: &Path,
    selection: &Selection,
    factory: BackendFactory,
) -> Result<RunSummary> {
    let config = ProjectConfig::load(project)?;
    let plan = build_plan(
        &config,
        Platform::host()?,
        &selection.platforms,
        &selection.suites,
        &selection.required,
    )?;
    let settings = HostConfig::load()?;
    let policy = cleanup_policy(selection.cleanup)?;
    install_signals()?;
    let original_root = project.canonicalize().context("resolve original project")?;
    ensure!(
        !resolved_storage_root(&settings.storage.root)?.starts_with(&original_root),
        "storage.root must be outside the original project"
    );
    if settings.storage.root.join(".rs-infra-platform-store").exists() {
        for report in recover_registered_runs(&settings)? {
            for error in report.errors {
                eprintln!("recovery: {error}");
            }
        }
    }
    let data: Vec<_> = config
        .suites
        .iter()
        .filter(|suite| plan.entries.iter().any(|entry| entry.suite_id == suite.id))
        .flat_map(|suite| suite.data.iter().cloned())
        .collect();
    let reusable = policy == CleanupPolicy::Disabled
        && !plan.entries.is_empty()
        && plan
            .entries
            .iter()
            .all(|entry| matches!(entry.route, Route::Native | Route::AndroidEmulator));
    let reuse_toolchain = if reusable { shared_toolchain(project).ok() } else { None };
    let (mut record, mut snapshot, report_dir, reuse) = if let Some(toolchain) = reuse_toolchain {
        let environments: Vec<_> = plan
            .entries
            .iter()
            .map(|entry| {
                let target = config
                    .suites
                    .iter()
                    .find(|suite| suite.id == entry.suite_id)
                    .and_then(|suite| suite.target.as_ref());
                (entry.platform, entry.route, target)
            })
            .collect();
        let environment = serde_json::to_string(&(
            std::env::consts::ARCH,
            plan.host,
            environments,
            &settings.native.cargo,
            &settings.android,
        ))?;
        let allocation = crate::reuse::allocate(&settings.storage.root, project, &environment, &toolchain, &data)?;
        (
            allocation.record,
            allocation.snapshot,
            allocation.report_dir,
            Some(allocation.reuse),
        )
    } else {
        let record = RunStore::create(&settings.storage.root, policy)?;
        let report_dir = record.report_dir();
        (record, None, report_dir, None)
    };
    let mut summary = RunSummary {
        version: 1,
        run_id: record.run_id.0.clone(),
        success: false,
        state: "running".to_owned(),
        snapshot_digest: snapshot.as_ref().map(|snapshot| snapshot.digest.clone()),
        suites: Vec::new(),
        errors: Vec::new(),
        cleanup: CleanupReport::default(),
        report_dir,
        retained_path: None,
    };
    let initial = summary.persist();
    if let Err(error) = initial {
        summary.errors.push(format!("initial report: {error:#}"));
    } else {
        for (index, entry) in plan.entries.iter().enumerate() {
            let suite = config
                .suites
                .iter()
                .find(|suite| suite.id == entry.suite_id)
                .context("planned suite no longer exists")?;
            let mut report = SuiteReport {
                suite_id: suite.id.clone(),
                platform: entry.platform,
                status: "pending".to_owned(),
                environment: None,
                commands: Vec::new(),
                tests_passed: 0,
                tests_failed: 0,
                tests_ignored: 0,
                errors: Vec::new(),
            };
            let backend = factory(entry.platform, entry.route, &settings);
            let mut backend = match backend {
                Ok(backend) => backend,
                Err(error) => {
                    report.status = "infrastructure_failed".to_owned();
                    report.errors.push(format!("backend configuration: {error:#}"));
                    summary.suites.push(report);
                    continue;
                }
            };
            match backend.probe() {
                Ok(Capability::Ready) => {}
                Ok(Capability::Unsupported(reason) | Capability::Unconfigured(reason)) => {
                    report.status = "skipped".to_owned();
                    eprintln!("WARN+SKIP {} / {}: {reason}", entry.platform, suite.id);
                    if entry.required {
                        report.errors.push(format!(
                            "required platform {} suite {} did not execute: {reason}",
                            entry.platform, suite.id
                        ));
                    }
                    summary.suites.push(report);
                    continue;
                }
                Ok(Capability::Broken(reason)) => {
                    report.status = "infrastructure_failed".to_owned();
                    report.errors.push(format!("ERROR: {reason}"));
                    summary.suites.push(report);
                    continue;
                }
                Err(error) => {
                    report.status = "infrastructure_failed".to_owned();
                    report.errors.push(format!("ERROR: capability probe: {error:#}"));
                    summary.suites.push(report);
                    continue;
                }
            }
            let preparation = (|| -> Result<SessionContext> {
                ensure!(!cancelled().load(Ordering::SeqCst), "run canceled before suite startup");
                if snapshot.is_none() {
                    let created = Snapshot::create_with_data(project, &record.run_dir().join("snapshot"), &data)?;
                    record.snapshot_manifest = created
                        .entries
                        .iter()
                        .map(|entry| entry.relative_path.clone())
                        .collect();
                    record.persist()?;
                    if let Some(reuse) = &reuse {
                        reuse.save_snapshot(&record, &summary.report_dir, &created)?;
                    } else {
                        fs::write(
                            summary.report_dir.join("snapshot.json"),
                            serde_json::to_vec_pretty(&created)?,
                        )?;
                    }
                    summary.snapshot_digest = Some(created.digest.clone());
                    snapshot = Some(created);
                }
                let snapshot = snapshot.as_ref().context("snapshot preparation did not complete")?;
                let report_dir = summary.report_dir.join(format!("entry-{index}"));
                fs::create_dir_all(&report_dir)?;
                Ok(SessionContext {
                    run_id: record.run_id.clone(),
                    host_run_dir: record.run_dir(),
                    snapshot_root: snapshot.root.clone(),
                    project_relative: snapshot.project_relative.clone(),
                    report_dir,
                    toolchain: shared_toolchain(project)?,
                    cleanup: policy,
                })
            })();
            let context = match preparation {
                Ok(context) => context,
                Err(error) => {
                    report.status = "infrastructure_failed".to_owned();
                    report.errors.push(format!("prepare execution: {error:#}"));
                    summary.suites.push(report);
                    continue;
                }
            };
            // start can partially allocate an environment, so stop is unconditional.
            let execution = (|| -> Result<()> {
                report.environment = Some(backend.start(&context)?);
                report.commands = backend.execute(suite, &context)?;
                evaluate_suite(suite, entry.required, &mut report);
                Ok(())
            })();
            if let Err(error) = execution {
                if let Some(completed) = crate::report::completed_results(&error) {
                    report.commands = completed.to_vec();
                    evaluate_suite(suite, entry.required, &mut report);
                }
                if let Err(record_error) = preserve_unstopped_tree(&mut record, &error, &context.report_dir) {
                    summary
                        .errors
                        .push(format!("persist process cleanup guard: {record_error:#}"));
                }
                report.status = "infrastructure_failed".to_owned();
                report.errors.push(format!("execution: {error:#}"));
            }
            summary.suites.push(report);
            if let Err(error) = summary.persist() {
                summary.errors.push(format!("pre-cleanup report: {error:#}"));
            }
            if policy == CleanupPolicy::Enabled && !has_unstopped_tree(&record) {
                match backend.cleanup(&context) {
                    Ok(cleaned) => {
                        if !cleaned.errors.is_empty() || !cleaned.remaining.is_empty() {
                            record.pending_guest.extend(cleaned.remaining.iter().cloned());
                            if cleaned.remaining.is_empty() {
                                record.pending_guest.push(context.report_dir.clone());
                            }
                        }
                        merge_cleanup(&mut summary.cleanup, cleaned);
                    }
                    Err(error) => {
                        if let Err(record_error) = preserve_unstopped_tree(&mut record, &error, &context.report_dir) {
                            summary
                                .errors
                                .push(format!("persist process cleanup guard: {record_error:#}"));
                        }
                        record.pending_guest.push(context.report_dir.clone());
                        summary
                            .cleanup
                            .errors
                            .push(format!("{} guest cleanup: {error:#}", entry.platform));
                    }
                }
            }
            if let Err(error) = backend.stop() {
                if let Err(record_error) = preserve_unstopped_tree(&mut record, &error, &context.report_dir) {
                    summary
                        .errors
                        .push(format!("persist process cleanup guard: {record_error:#}"));
                }
                record.pending_guest.push(context.report_dir.clone());
                summary
                    .cleanup
                    .errors
                    .push(format!("{} environment stop: {error:#}", entry.platform));
            }
            if let Err(error) = record.heartbeat() {
                summary.errors.push(format!("persist heartbeat: {error:#}"));
            }
        }
    }
    summary.state = "finalizing".to_owned();
    if let Err(error) = summary.persist() {
        summary.errors.push(format!("pre-cleanup report: {error:#}"));
    }
    match cleanup::finish_run(&mut record) {
        Ok(cleaned) => merge_cleanup(&mut summary.cleanup, cleaned),
        Err(error) => summary.cleanup.errors.push(format!("host cleanup: {error:#}")),
    }
    if policy == CleanupPolicy::Disabled {
        summary.retained_path = Some(record.run_dir());
    }
    summary.state = if cancelled().load(Ordering::SeqCst) {
        "canceled"
    } else {
        "finished"
    }
    .to_owned();
    summary.success = !cancelled().load(Ordering::SeqCst)
        && summary.errors.is_empty()
        && summary.cleanup.errors.is_empty()
        && summary.suites.iter().all(|suite| suite.errors.is_empty());
    // Keep the live lease until the final report has been published.
    if let Err(error) = summary.persist() {
        summary.success = false;
        summary.errors.push(format!("final report: {error:#}"));
        eprintln!(
            "{}",
            summary.errors.last().context("final report failure was recorded")?
        );
    }
    drop(record);
    drop(reuse);
    Ok(summary)
}

/// Recovers a stopped registered run while holding the same lease through
/// remote cleanup, owned host cleanup, and durable state publication. Retained
/// runs need explicit selection; live or unverifiable active owners are
/// rejected.
pub(crate) fn clean_registered_run(
    settings: &HostConfig,
    id: &crate::model::RunId,
    explicit: bool,
) -> Result<CleanupReport> {
    RunStore::load(&settings.storage.root, id)?;
    let lease = Lease::acquire(&settings.storage.root, &format!("run:{}", id.0))?;
    let mut record = RunStore::load(&settings.storage.root, id)?;
    ensure!(
        record.state != RunState::Active || record.owner_alive() == Some(false),
        "run owner is alive or its identity cannot be verified"
    );
    ensure!(
        explicit || (record.policy == CleanupPolicy::Enabled && record.state != RunState::Retained),
        "retained runs require explicit clean --run"
    );
    record.lease = Some(lease);
    recover_record(settings, &mut record)
}

/// Scans durable registrations and conservatively retries interrupted automatic
/// runs. Live locks, retained runs, and unknown host identities are left
/// untouched.
pub(crate) fn recover_registered_runs(settings: &HostConfig) -> Result<Vec<CleanupReport>> {
    if !settings.storage.root.exists() {
        return Ok(Vec::new());
    }
    let store = open_store(&settings.storage.root)?;
    use cap_fs_ext::DirExt;
    let records = store.open_dir_nofollow("records")?;
    let mut reports = Vec::new();
    for entry in records.entries()? {
        let name = entry?.file_name();
        let Some(name) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        let id = crate::model::RunId(name.to_owned());
        let record = RunStore::load(&settings.storage.root, &id)?;
        if matches!(record.state, RunState::Retained | RunState::Cleaned) || record.owner_alive() != Some(false) {
            continue;
        }
        let Ok(lease) = Lease::acquire(&settings.storage.root, &format!("run:{}", id.0)) else {
            continue;
        };
        let mut record = RunStore::load(&settings.storage.root, &id)?;
        if matches!(record.state, RunState::Retained | RunState::Cleaned) || record.owner_alive() != Some(false) {
            continue;
        }
        record.lease = Some(lease);
        reports.push(if record.policy == CleanupPolicy::Disabled {
            recover_retained_record(settings, &mut record)?
        } else {
            recover_record(settings, &mut record)?
        });
    }
    Ok(reports)
}

fn recover_record(settings: &HostConfig, record: &mut RunRecord) -> Result<CleanupReport> {
    record.policy = CleanupPolicy::Enabled;
    let mut report = retry_process_trees(record)?;
    let stop_only = has_unstopped_tree(record);
    merge_cleanup(&mut report, recover_backend_reports(settings, record, stop_only)?);
    if !stop_only && report.errors.is_empty() {
        record.pending_guest.clear();
    } else if record.pending_guest.is_empty() {
        record.pending_guest.push(record.report_dir());
    }
    record.persist()?;
    merge_cleanup(&mut report, cleanup::finish_run(record)?);
    Ok(report)
}

fn recover_retained_record(settings: &HostConfig, record: &mut RunRecord) -> Result<CleanupReport> {
    let mut report = retry_process_trees(record)?;
    merge_cleanup(&mut report, recover_backend_reports(settings, record, true)?);
    if record
        .environments
        .iter()
        .any(|environment| environment.owned && !environment.stopped)
    {
        report
            .errors
            .push("owned environment termination remains unconfirmed; retained files preserved".to_owned());
    }
    record.state = if report.errors.is_empty() {
        record.pending_guest.clear();
        RunState::Retained
    } else {
        RunState::CleanupPending
    };
    record.persist()?;
    report.remaining.push(record.run_dir());
    Ok(report)
}

fn recovery_report_directories(record: &RunRecord) -> Result<Vec<std::path::PathBuf>> {
    let mut reports = vec![record.report_dir()];
    let directory = crate::run_store::secure_dir(&record.report_dir(), false)?;
    for entry in directory.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name
            .strip_prefix("attempt-")
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        {
            use cap_fs_ext::DirExt;
            directory.open_dir_nofollow(name)?;
            reports.push(record.report_dir().join(name));
        }
    }
    Ok(reports)
}

fn recover_backend_reports(settings: &HostConfig, record: &RunRecord, stop_only: bool) -> Result<CleanupReport> {
    let mut report = CleanupReport::default();
    for report_dir in recovery_report_directories(record)? {
        let windows = if stop_only {
            crate::backend::windows::recover_stop(&report_dir, settings.windows.as_ref())
        } else {
            crate::backend::windows::recover_cleanup(&report_dir, settings.windows.as_ref())
        };
        let android = if stop_only {
            crate::backend::android::recover_stop(&report_dir, settings.android.as_ref())
        } else {
            crate::backend::android::recover_cleanup(&report_dir, settings.android.as_ref())
        };
        for (backend, result) in [("windows", windows), ("android", android)] {
            if let Err(error) = result {
                report.errors.push(format!(
                    "{backend} {} recovery: {error:#}",
                    if stop_only { "stop" } else { "cleanup" }
                ));
                report.remaining.push(report_dir.clone());
            }
        }
    }
    Ok(report)
}

/// Resolves cleanup before allocation, preserving CLI precedence over any
/// invalid operating-system environment value, including a non-UTF8 value on
/// Unix.
pub(crate) fn cleanup_policy(cli: Option<bool>) -> Result<CleanupPolicy> {
    if cli.is_some() {
        return CleanupPolicy::resolve(cli, None);
    }
    let raw = std::env::var_os("RS_INFRA_PLATFORM_CLEANUP");
    let value = raw
        .as_ref()
        .map(|value| {
            value
                .to_str()
                .context("invalid RS_INFRA_PLATFORM_CLEANUP value: not UTF8")
        })
        .transpose()?;
    CleanupPolicy::resolve(None, value)
}

fn preserve_unstopped_tree(record: &mut RunRecord, error: &anyhow::Error, report_dir: &Path) -> Result<()> {
    if !crate::process::tree_stop_failed(error) {
        return Ok(());
    }
    let number = STOP_NONCE.fetch_add(1, Ordering::Relaxed);
    let relative = report_dir
        .strip_prefix(record.report_dir())?
        .join(format!("process-stop-{number}.json"));
    let bytes = crate::process::stop_identity(error)
        .map(serde_json::to_vec)
        .transpose()?;
    let digest = bytes
        .as_ref()
        .map(|bytes| identity_digest(bytes))
        .unwrap_or_else(|| "missing-identity".to_owned());
    let key = format!("process-tree:{}:sha256={digest}", relative.to_string_lossy());
    record.environments.push(crate::run_store::EnvironmentLease {
        key,
        owned: true,
        stopped: false,
    });
    // Persist the fail-closed guard before attempting any optional recovery
    // metadata IO.
    record.persist()?;
    if let Some(bytes) = bytes {
        use std::io::Write;
        let directory = crate::run_store::secure_dir(report_dir, false)?;
        let mut file = directory.open_with(
            format!("process-stop-{number}.json"),
            cap_std::fs::OpenOptions::new().write(true).create_new(true),
        )?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        directory.open(".")?.sync_all()?;
    }
    Ok(())
}

fn retry_process_trees(record: &mut RunRecord) -> Result<CleanupReport> {
    let root = record.report_dir();
    let new_boot = record.is_local_host()
        && record.boot_id.is_some()
        && crate::process_identity::boot_id().is_some()
        && record.boot_id != crate::process_identity::boot_id();
    let mut report = CleanupReport::default();
    for environment in &mut record.environments {
        if !environment.owned || environment.stopped || !environment.key.starts_with("process-tree:") {
            continue;
        }
        let stopped = (|| -> Result<bool> {
            if new_boot {
                return Ok(true);
            }
            let registration = environment
                .key
                .strip_prefix("process-tree:")
                .context("missing process recovery registration")?;
            let (relative, digest) = registration
                .rsplit_once(":sha256=")
                .context("process stop identity was not registered")?;
            let path = Path::new(relative);
            ensure!(
                path.components()
                    .all(|component| matches!(component, std::path::Component::Normal(_))),
                "invalid process identity relative path"
            );
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .context("missing process identity filename")?;
            ensure!(
                name.starts_with("process-stop-") && name.ends_with(".json"),
                "invalid process identity filename"
            );
            let directory = crate::run_store::secure_dir(
                &root.join(path.parent().context("missing process identity parent")?),
                false,
            )?;
            let bytes = crate::run_store::read_nofollow(&directory, name)?;
            ensure!(
                identity_digest(&bytes) == digest,
                "registered process identity digest mismatch"
            );
            let identity: crate::process::StopIdentity =
                serde_json::from_slice(&bytes).context("parse registered process stop identity")?;
            crate::process::retry_stop(&identity)
        })();
        match stopped {
            Ok(true) => environment.stopped = true,
            Ok(false) => report
                .errors
                .push(format!("{}: process termination is still unconfirmed", environment.key)),
            Err(error) => report
                .errors
                .push(format!("{}: process stop recovery: {error:#}", environment.key)),
        }
    }
    if !report.errors.is_empty() {
        report.remaining.push(record.run_dir());
    }
    record.persist()?;
    Ok(report)
}

fn identity_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn has_unstopped_tree(record: &RunRecord) -> bool {
    record
        .environments
        .iter()
        .any(|environment| environment.owned && !environment.stopped && environment.key.starts_with("process-tree:"))
}

fn resolved_storage_root(root: &Path) -> Result<std::path::PathBuf> {
    let absolute = std::path::absolute(root)?;
    let mut ancestor = absolute.as_path();
    while !ancestor.exists() {
        ancestor = ancestor.parent().context("storage root has no existing ancestor")?;
    }
    Ok(ancestor
        .canonicalize()
        .context("resolve storage root ancestor")?
        .join(absolute.strip_prefix(ancestor)?))
}

fn evaluate_suite(suite: &SuiteSpec, required: bool, report: &mut SuiteReport) {
    for (command, result) in suite.commands.iter().zip(&report.commands) {
        if command.kind == CommandKind::Test {
            report.tests_passed += result.tests_passed;
            report.tests_failed += result.tests_failed;
            report.tests_ignored += result.tests_ignored;
        }
        if result.cancelled {
            report.errors.push("command canceled".to_owned());
        } else if result.timed_out {
            report.errors.push("command timeout".to_owned());
        } else if result.exit_code != Some(0) {
            report
                .errors
                .push(format!("command failed with exit code {:?}", result.exit_code));
        }
        if result.tests_failed > 0 {
            report.errors.push(format!("{} tests failed", result.tests_failed));
        }
    }
    let count = report.tests_passed + report.tests_failed;
    let has_tests = suite.commands.iter().any(|command| command.kind == CommandKind::Test);
    let minimum = suite.minimum_tests.max(u64::from(required));
    if has_tests && count < minimum {
        report.errors.push(format!(
            "minimum_tests={minimum}, but only {count} tests actually executed (ignored tests do not count)"
        ));
    }
    report.status = if !report.errors.is_empty() {
        "failed"
    } else if count > 0 {
        "passed"
    } else {
        "checked"
    }
    .to_owned();
}

fn merge_cleanup(target: &mut CleanupReport, source: CleanupReport) {
    target.remaining.extend(source.remaining);
    target.errors.extend(source.errors);
    target.host_reclaimed_bytes = match (target.host_reclaimed_bytes, source.host_reclaimed_bytes) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    };
    target.guest_available_delta = match (target.guest_available_delta, source.guest_available_delta) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    };
}

fn shared_toolchain(project: &Path) -> Result<String> {
    let path = project.join(".infra/tools/defaults.toml");
    let text = fs::read_to_string(&path).with_context(|| format!("read shared Rust toolchain {}", path.display()))?;
    let defaults: toml::Table = toml::from_str(&text).context("parse shared toolchain defaults")?;
    let value = defaults
        .get("build_toolchain")
        .and_then(toml::Value::as_str)
        .context("shared defaults build_toolchain must be a string")?;
    ensure!(
        !value.is_empty() && !value.starts_with(['+', '-']) && !value.chars().any(char::is_whitespace),
        "invalid shared build_toolchain"
    );
    Ok(value.to_owned())
}

struct UnavailableBackend {
    capability: Capability,
}

impl Backend for UnavailableBackend {
    fn probe(&self) -> Result<Capability> {
        Ok(self.capability.clone())
    }
    fn start(&mut self, _context: &SessionContext) -> Result<EnvironmentInfo> {
        bail!("unavailable backend cannot start")
    }
    fn execute(&mut self, _suite: &SuiteSpec, _context: &SessionContext) -> Result<Vec<ExecutionResult>> {
        bail!("unavailable backend cannot execute")
    }
    fn cleanup(&mut self, _context: &SessionContext) -> Result<CleanupReport> {
        Ok(CleanupReport::default())
    }
    fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
