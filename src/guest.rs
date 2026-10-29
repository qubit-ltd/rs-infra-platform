// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Bounded, versioned SSH guest requests; stdout contains exactly one JSON
//! frame.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use cap_fs_ext::DirExt;
use cap_fs_ext::FollowSymlinks;
use cap_fs_ext::OpenOptionsFollowExt;
use cap_std::fs::Dir;
use cap_std::fs::OpenOptions;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::cleanup::finish_run;
use crate::lease::Lease;
use crate::model::CleanupPolicy;
use crate::model::EnvironmentInfo;
use crate::model::RunId;
use crate::model::SuiteSpec;
use crate::process;
use crate::process::ProcessSpec;
use crate::run_store::RunStore;
use crate::run_store::open_store;
use crate::run_store::read_nofollow;
use crate::run_store::secure_dir;
use crate::run_store::validate_run_id;
use crate::transfer;
use crate::transfer::Entry;

pub(crate) const VERSION: u32 = 1;
pub(crate) const MAX_FRAME: u64 = 4 * 1024 * 1024;
pub(crate) const CHUNK: usize = 64 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub(crate) protocol_version: u32,
    pub(crate) request_id: String,
    pub(crate) run_id: String,
    pub(crate) lease_token: String,
    pub(crate) op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) manifest: Option<Vec<Entry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) suite: Option<SuiteSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project_relative: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) toolchain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cleanup: Option<CleanupPolicy>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Response {
    pub(crate) protocol_version: u32,
    pub(crate) request_id: String,
    pub(crate) run_id: String,
    pub(crate) ok: bool,
    pub(crate) payload: Value,
    pub(crate) error: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    internal_id: Option<RunId>,
    token: String,
    owner: Option<String>,
    #[serde(default)]
    state: RegistrationState,
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum RegistrationState {
    Preparing,
    #[default]
    Active,
    Rolledback,
}

/// Reads one bounded stdin JSON request and writes a single bounded JSON
/// response. Protocol errors become `ok=false`; stdout IO failures are returned
/// to the caller.
pub(crate) fn run() -> Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin().take(MAX_FRAME + 1).read_to_end(&mut bytes)?;
    let parsed = if bytes.len() as u64 > MAX_FRAME {
        Err(anyhow::anyhow!("request frame exceeds limit"))
    } else {
        serde_json::from_slice::<Request>(&bytes).context("invalid request")
    };
    let (id, run) = parsed
        .as_ref()
        .map(|request| (request.request_id.clone(), request.run_id.clone()))
        .unwrap_or_default();
    let result = parsed.and_then(|request| dispatch(&guest_root()?, &request));
    let response = match result {
        Ok(payload) => Response {
            protocol_version: VERSION,
            request_id: id,
            run_id: run,
            ok: true,
            payload,
            error: None,
        },
        Err(error) => match partial_payload(&error) {
            Some(payload) => Response {
                protocol_version: VERSION,
                request_id: id,
                run_id: run,
                ok: true,
                payload,
                error: None,
            },
            None => Response {
                protocol_version: VERSION,
                request_id: id,
                run_id: run,
                ok: false,
                payload: Value::Null,
                error: Some(format!("{error:#}").chars().take(8192).collect()),
            },
        },
    };
    let bytes = serde_json::to_vec(&response)?;
    ensure!(bytes.len() as u64 <= MAX_FRAME, "response frame exceeds limit");
    std::io::stdout().write_all(&bytes)?;
    Ok(())
}

fn guest_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("RS_INFRA_PLATFORM_GUEST_ROOT") {
        return Ok(path.into());
    }
    let root =
        std::env::var_os("LOCALAPPDATA").context("configure RS_INFRA_PLATFORM_GUEST_ROOT in the guest environment")?;
    Ok(PathBuf::from(root).join("rs-infra-platform").join("guest"))
}

fn dispatch(root: &Path, request: &Request) -> Result<Value> {
    ensure!(request.protocol_version == VERSION, "unsupported protocol version");
    validate_run_id(&RunId(request.run_id.clone()))?;
    ensure!(
        !request.request_id.is_empty()
            && request.request_id.len() <= 128
            && request
                .request_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-'),
        "invalid request ID"
    );
    ensure!(
        (32..=128).contains(&request.lease_token.len()) && request.lease_token.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid lease token"
    );
    ensure!(
        ["probe", "prepare", "execute", "cleanup", "cancel", "read", "status"].contains(&request.op.as_str()),
        "unknown operation"
    );
    if !["prepare", "read"].contains(&request.op.as_str()) {
        ensure!(
            request.path.is_none() && request.offset.is_none() && request.data.is_none(),
            "unknown field for operation"
        );
    }
    match request.op.as_str() {
        "probe" | "cleanup" | "cancel" | "status" => ensure!(
            request.action.is_none()
                && request.manifest.is_none()
                && request.suite.is_none()
                && request.project_relative.is_none()
                && request.toolchain.is_none()
                && request.cleanup.is_none(),
            "unknown field for operation"
        ),
        "execute" => ensure!(
            request.action.is_none() && request.manifest.is_none() && request.cleanup.is_none(),
            "unknown field for execute operation"
        ),
        "read" => ensure!(
            request.action.is_none()
                && request.manifest.is_none()
                && request.data.is_none()
                && request.suite.is_none()
                && request.project_relative.is_none()
                && request.toolchain.is_none()
                && request.cleanup.is_none(),
            "unknown field for read operation"
        ),
        _ => ensure!(
            request.suite.is_none() && request.project_relative.is_none() && request.toolchain.is_none(),
            "unknown field for prepare operation"
        ),
    }
    if request.op == "probe" {
        #[cfg(not(windows))]
        return Ok(serde_json::to_value(environment(root, &[])?)?);
        #[cfg(windows)]
        {
            let mut record = RunStore::create(root, CleanupPolicy::Enabled)?;
            let result = environment(&record.run_dir(), &[]);
            if let Err(error) = &result {
                protect_failed_process_tree(&mut record, request, error)?;
            }
            let cleanup = finish_run(&mut record)?;
            ensure!(cleanup.errors.is_empty(), "guest probe cleanup failed");
            return Ok(serde_json::to_value(result?)?);
        }
    }
    if request.op == "prepare" && request.action.as_deref() == Some("start") {
        return prepare(root, request);
    }
    let registration = registration(root, request)?;
    if registration.state == RegistrationState::Rolledback {
        return match request.op.as_str() {
            "status" => Ok(json!({"registered":true,"state":"rolledback","cleanup_pending":false})),
            "cleanup" => Ok(serde_json::to_value(crate::model::CleanupReport::default())?),
            _ => bail!("guest prepare was rolled back; no active run remains"),
        };
    }
    if registration.internal_id.is_none() && request.op == "status" {
        return Ok(json!({"registered":true,"state":"preparing","cleanup_pending":true}));
    }
    let internal_id = registration
        .internal_id
        .as_ref()
        .context("guest prepare allocation is unresolved; cleanup remains pending")?;
    let mut record = RunStore::load(root, internal_id).context("load guest registration")?;
    ensure!(
        Some(record.owner.as_str()) == registration.owner.as_deref(),
        "guest registration owner mismatch"
    );
    if request.op == "status" {
        return Ok(json!({"registered":true}));
    }
    receipt(&record.report_dir(), request)?;
    if request.op == "cancel" {
        let directory = secure_dir(&record.report_dir(), false)?;
        let mut file = directory.open_with(
            "cancel",
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .follow(FollowSymlinks::No),
        )?;
        file.write_all(b"cancel\n")?;
        return Ok(json!({"cancel_requested":true}));
    }
    let _operation = Lease::acquire(root, &format!("guest-operation:{}", request.run_id))?;
    if request.op == "cleanup" {
        record.policy = CleanupPolicy::Enabled;
        record.lease = Some(Lease::acquire(root, &format!("run:{}", internal_id.0))?);
        retry_guest_processes(&mut record)?;
        #[cfg(windows)]
        let before = available_space(&record.report_dir()).ok();
        let report = finish_run(&mut record)?;
        #[cfg(windows)]
        let report = {
            let mut report = report;
            if report.errors.is_empty()
                && let (Some(before), Ok(after)) = (before, available_space(&record.report_dir()))
            {
                report.guest_available_delta = Some(after.saturating_sub(before));
            }
            report
        };
        if report.errors.is_empty() && report.remaining.is_empty() && registration.state == RegistrationState::Preparing
        {
            let mut terminal = registration;
            terminal.state = RegistrationState::Rolledback;
            let directory = open_store(root)?.open_dir_nofollow("guest-registrations")?;
            persist_registration(&directory, &format!("{}.json", request.run_id), &terminal)?;
        }
        return Ok(serde_json::to_value(report)?);
    }
    ensure!(
        registration.state == RegistrationState::Active,
        "guest prepare is incomplete"
    );
    if request.op == "read" {
        return read_log(&record.report_dir(), request);
    }
    if request.op == "execute" {
        return match execute(&record, request) {
            Ok(payload) => Ok(payload),
            Err(error) => {
                if let Err(protection) = protect_failed_process_tree(&mut record, request, &error) {
                    return Err(error.context(format!("persist guest process-stop protection failed: {protection:#}")));
                }
                Err(error)
            }
        };
    }
    let snapshot = record.run_dir().join("snapshot");
    let entries: Vec<Entry> = serde_json::from_slice(&read_nofollow(
        &secure_dir(&record.report_dir(), false)?,
        "manifest.json",
    )?)?;
    match request.action.as_deref() {
        Some("chunk") => {
            let path = request.path.as_deref().context("missing chunk path")?;
            let entry = entries
                .iter()
                .find(|entry| entry.path == path && !entry.directory)
                .context("path not in snapshot manifest")?;
            let data = request.data.as_ref().context("missing chunk bytes")?;
            let offset = request.offset.context("missing chunk offset")?;
            ensure!(
                data.len() <= CHUNK
                    && offset
                        .checked_add(data.len() as u64)
                        .is_some_and(|end| end <= entry.size),
                "chunk exceeds manifest size"
            );
            transfer::validate_path(path)?;
            let full = snapshot.join(path);
            let parent = secure_dir(full.parent().context("missing parent")?, false)?;
            let mut file = parent.open_with(
                full.file_name().context("missing filename")?,
                OpenOptions::new().read(true).write(true).follow(FollowSymlinks::No),
            )?;
            ensure!(
                file.metadata()?.len() == offset,
                "chunk offset does not match uploaded size"
            );
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(data)?;
            Ok(json!({"uploaded":offset + data.len() as u64}))
        }
        Some("seal") => {
            for entry in &entries {
                if !entry.directory {
                    let path = snapshot.join(&entry.path);
                    ensure!(
                        fs::symlink_metadata(&path)?.len() == entry.size
                            && transfer::digest_file(&path)? == entry.digest,
                        "incomplete upload or digest mismatch: {}",
                        entry.path
                    );
                }
            }
            let dir = secure_dir(&record.report_dir(), false)?;
            dir.write("prepared", b"complete\n")?;
            Ok(json!({"prepared":true}))
        }
        _ => bail!("unknown prepare action"),
    }
}

fn protect_failed_process_tree(
    record: &mut crate::run_store::RunRecord,
    request: &Request,
    error: &anyhow::Error,
) -> Result<()> {
    if process::tree_stop_failed(error) {
        record.environments.push(crate::run_store::EnvironmentLease {
            key: format!("guest-process-tree:{}", request.request_id),
            owned: true,
            stopped: false,
        });
        record
            .persist()
            .context("persist unconfirmed guest process termination before cleanup")?;
        if let Some(identity) = process::stop_identity(error) {
            let directory = secure_dir(&record.report_dir(), false)?;
            let mut file = directory.open_with(
                format!("guest-process-{}.json", request.request_id),
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .follow(FollowSymlinks::No),
            )?;
            file.write_all(&serde_json::to_vec(identity)?)?;
            file.sync_all()?;
        }
    }
    Ok(())
}

fn retry_guest_processes(record: &mut crate::run_store::RunRecord) -> Result<()> {
    let directory = secure_dir(&record.report_dir(), false)?;
    let mut changed = false;
    for environment in &mut record.environments {
        if !environment.owned || environment.stopped {
            continue;
        }
        let Some(id) = environment.key.strip_prefix("guest-process-tree:") else {
            continue;
        };
        ensure!(
            !id.is_empty() && id.len() <= 128 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'),
            "invalid guest process-stop registration"
        );
        let bytes = match read_nofollow(&directory, &format!("guest-process-{id}.json")) {
            Ok(bytes) => bytes,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let identity: process::StopIdentity =
            serde_json::from_slice(&bytes).context("invalid guest process-stop identity")?;
        if process::retry_stop(&identity)? {
            environment.stopped = true;
            changed = true;
        }
    }
    if changed {
        record.persist()?;
    }
    Ok(())
}

fn registration(root: &Path, request: &Request) -> Result<Registration> {
    let store = open_store(root).context("guest registration store is unavailable")?;
    let directory = store
        .open_dir_nofollow("guest-registrations")
        .context("guest registration not found")?;
    let registration: Registration = serde_json::from_slice(
        &read_nofollow(&directory, &format!("{}.json", request.run_id)).context("guest registration not found")?,
    )?;
    ensure!(
        registration.token == request.lease_token,
        "guest registration lease token mismatch"
    );
    Ok(registration)
}

fn receipt(report_dir: &Path, request: &Request) -> Result<()> {
    let directory = secure_dir(report_dir, false)?;
    let file = directory
        .open_with(
            format!("request-{}", request.request_id),
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No),
        )
        .context("duplicate request ID or unsafe request receipt")?;
    file.sync_all()?;
    Ok(())
}

fn persist_registration(directory: &Dir, name: &str, registration: &Registration) -> Result<()> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = format!("{name}.{nonce:x}.tmp");
    let mut file = directory.open_with(
        &temporary,
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No),
    )?;
    let result = (|| {
        file.write_all(&serde_json::to_vec(registration)?)?;
        file.sync_all()?;
        directory.rename(&temporary, directory, name)?;
        #[cfg(unix)]
        directory.open(".")?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = directory.remove_file(&temporary);
    }
    result
}

fn prepare(root: &Path, request: &Request) -> Result<Value> {
    ensure!(
        request.path.is_none() && request.offset.is_none() && request.data.is_none(),
        "unknown field for prepare start"
    );
    let entries = request.manifest.as_ref().context("missing snapshot manifest")?;
    RunStore::initialize(root)?;
    let _registration = Lease::acquire(root, &format!("guest-registration:{}", request.run_id))?;
    let _operation = Lease::acquire(root, &format!("guest-operation:{}", request.run_id))?;
    let store = open_store(root)?;
    match store.create_dir("guest-registrations") {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let directory = store.open_dir_nofollow("guest-registrations")?;
    let name = format!("{}.json", request.run_id);
    match read_nofollow(&directory, &name) {
        Ok(bytes) => {
            let existing: Registration =
                serde_json::from_slice(&bytes).context("invalid existing guest registration")?;
            ensure!(
                existing.token == request.lease_token,
                "guest registration lease token mismatch"
            );
            bail!("duplicate run registration");
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error).context("read existing guest registration"),
    }
    // Publish the token before allocating artifacts; an unknown allocation remains
    // pending.
    let mut registration = Registration {
        internal_id: None,
        token: request.lease_token.clone(),
        owner: None,
        state: RegistrationState::Preparing,
    };
    persist_registration(&directory, &name, &registration)?;
    if let Err(error) = transfer::validate_manifest(entries) {
        registration.state = RegistrationState::Rolledback;
        persist_registration(&directory, &name, &registration)?;
        return Err(error);
    }
    let mut record = RunStore::create(root, request.cleanup.unwrap_or(CleanupPolicy::Enabled))?;
    registration.internal_id = Some(record.run_id.clone());
    registration.owner = Some(record.owner.clone());
    let prepared = (|| -> Result<Value> {
        persist_registration(&directory, &name, &registration)?;
        let snapshot = record.run_dir().join("snapshot");
        secure_dir(&snapshot, true)?;
        for entry in entries {
            let full = snapshot.join(&entry.path);
            if entry.directory {
                secure_dir(&full, true)?;
            } else {
                let parent = secure_dir(full.parent().context("file parent")?, true)?;
                parent.open_with(
                    full.file_name().context("filename")?,
                    OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .follow(FollowSymlinks::No),
                )?;
            }
        }
        secure_dir(&record.report_dir(), false)?.write("manifest.json", serde_json::to_vec(entries)?)?;
        receipt(&record.report_dir(), request)?;
        registration.state = RegistrationState::Active;
        persist_registration(&directory, &name, &registration)?;
        Ok(json!({"registered":true}))
    })();
    if let Err(error) = prepared {
        record.policy = CleanupPolicy::Enabled;
        let cleanup = finish_run(&mut record).context("rollback failed guest registration")?;
        ensure!(
            cleanup.errors.is_empty() && cleanup.remaining.is_empty(),
            "{error:#}; guest registration rollback failed: {}",
            cleanup.errors.join("; ")
        );
        registration.state = RegistrationState::Rolledback;
        persist_registration(&directory, &name, &registration)
            .with_context(|| format!("{error:#}; failed to persist authenticated rollback terminal"))?;
        return Err(error);
    }
    prepared
}

fn execute(record: &crate::run_store::RunRecord, request: &Request) -> Result<Value> {
    let report = record.report_dir();
    ensure!(
        read_nofollow(&secure_dir(&report, false)?, "prepared").is_ok(),
        "snapshot upload is not prepared"
    );
    let suite = request.suite.as_ref().context("missing suite")?;
    ensure!(
        suite
            .target
            .as_deref()
            .is_none_or(|target| target == "x86_64-pc-windows-msvc"),
        "Windows guest suite target must be x86_64-pc-windows-msvc"
    );
    let environment = environment(&record.run_dir(), &suite.requirements)?;
    #[cfg(windows)]
    let mut env = msvc_environment_at(&record.run_dir())?;
    #[cfg(not(windows))]
    let mut env = msvc_environment()?;
    let target = record.run_dir().join("target");
    let temporary = record.run_dir().join("tmp");
    secure_dir(&temporary, true)?;
    env.insert("CARGO_TARGET_DIR".into(), target.into_os_string());
    env.insert("TMP".into(), temporary.as_os_str().to_owned());
    env.insert("TEMP".into(), temporary.into_os_string());
    let results = match execute_commands(record, request, &env) {
        Ok(results) => results,
        Err(error) => {
            if let Some(payload) = partial_payload(&error)
                && let Err(persistence) = persist_result(&report, request, &payload)
            {
                return Err(error.context(format!("persist partial guest result failed: {persistence:#}")));
            }
            return Err(error);
        }
    };
    let value = json!({"results":&results,"environment":environment});
    persist_result(&report, request, &value).map_err(|error| crate::report::with_completed_results(error, &results))?;
    Ok(value)
}

fn partial_payload(error: &anyhow::Error) -> Option<Value> {
    crate::report::completed_results(error).map(|results| json!({"results":results,"infrastructure_error":format!("{error:#}").chars().take(8192).collect::<String>()}))
}

fn persist_result(report: &Path, request: &Request, value: &Value) -> Result<()> {
    let directory = secure_dir(report, false)?;
    let temporary = format!("{}.result.tmp", request.request_id);
    let mut file = directory.open_with(
        &temporary,
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No),
    )?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    drop(file);
    directory.rename(&temporary, &directory, format!("{}.result.json", request.request_id))?;
    Ok(())
}

fn execute_commands(
    record: &crate::run_store::RunRecord,
    request: &Request,
    env: &BTreeMap<OsString, OsString>,
) -> Result<Vec<crate::model::ExecutionResult>> {
    let report = record.report_dir();
    let snapshot = record.run_dir().join("snapshot");
    let suite = request.suite.as_ref().context("missing suite")?;
    let project = request
        .project_relative
        .as_deref()
        .context("missing project-relative path")?;
    if project != "." && !project.is_empty() {
        transfer::validate_path(project)?;
    }
    let toolchain = request.toolchain.as_deref().context("missing toolchain")?;
    ensure!(
        !toolchain.is_empty()
            && toolchain
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c)),
        "invalid Rust toolchain"
    );
    let cancelled = AtomicBool::new(false);
    let complete = AtomicBool::new(false);
    let results = std::thread::scope(|scope| -> Result<_> {
        scope.spawn(|| {
            while !complete.load(Ordering::Acquire) {
                if report.join("cancel").exists() {
                    cancelled.store(true, Ordering::Release);
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let execution = (|| {
            let mut results = Vec::new();
            for (index, command) in suite.commands.iter().enumerate() {
                let outcome = (|| -> Result<crate::model::ExecutionResult> {
                    let relative = command.cwd.to_str().context("non-UTF-8 command cwd")?;
                    if relative != "." && !relative.is_empty() {
                        transfer::validate_path(relative)?;
                    }
                    let cwd = snapshot.join(project).join(&command.cwd);
                    secure_dir(&cwd, false)?;
                    let mut argv = command.argv.clone();
                    ensure!(
                        !argv.is_empty() && argv.iter().all(|arg| !arg.contains('\0')),
                        "invalid suite argv"
                    );
                    if argv[0] == "cargo" {
                        argv.insert(1, format!("+{toolchain}"));
                    }
                    let spec = ProcessSpec {
                        argv,
                        cwd,
                        env: env.clone(),
                        timeout: Duration::from_secs(suite.timeout_seconds),
                        stdout: report.join(format!("{}-{index}.stdout", request.request_id)),
                        stderr: report.join(format!("{}-{index}.stderr", request.request_id)),
                    };
                    process::run(&spec, &cancelled)
                })()
                .with_context(|| {
                    format!(
                        "guest command {} failed: {}",
                        index + 1,
                        command.argv.first().map_or("<empty>", String::as_str)
                    )
                });
                let result = outcome.map_err(|error| crate::report::with_completed_results(error, &results))?;
                let failed = result.exit_code != Some(0) || result.cancelled || result.timed_out;
                results.push(result);
                if failed {
                    break;
                }
            }
            Ok(results)
        })();
        complete.store(true, Ordering::Release);
        execution
    })?;
    Ok(results)
}

fn read_log(report: &Path, request: &Request) -> Result<Value> {
    let name = request.path.as_deref().context("missing log name")?;
    ensure!(
        !name.contains(['/', '\\']) && (name.ends_with(".stdout") || name.ends_with(".stderr")),
        "invalid log name"
    );
    let directory = secure_dir(report, false)?;
    let mut file = directory.open_with(name, OpenOptions::new().read(true).follow(FollowSymlinks::No))?;
    file.seek(SeekFrom::Start(request.offset.unwrap_or_default()))?;
    let mut bytes = vec![0; CHUNK];
    let count = file.read(&mut bytes)?;
    bytes.truncate(count);
    Ok(json!({"data":bytes,"eof":count < CHUNK}))
}

#[cfg(not(windows))]
fn environment(_root: &Path, _requirements: &[String]) -> Result<EnvironmentInfo> {
    bail!("Windows guest execution requires actual Windows and NTFS")
}

#[cfg(not(windows))]
fn msvc_environment() -> Result<BTreeMap<OsString, OsString>> {
    bail!("MSVC requires Windows")
}

#[cfg(windows)]
fn environment(root: &Path, requirements: &[String]) -> Result<EnvironmentInfo> {
    let mut env = msvc_environment_at(root)?;
    env.insert("RS_INFRA_PROBE_ROOT".into(), root.as_os_str().to_owned());
    let filesystem = helper(
        root,
        &[
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference='Stop'; (Get-Volume -FilePath $env:RS_INFRA_PROBE_ROOT).FileSystem",
        ],
        &env,
    )?;
    ensure!(
        filesystem.trim().eq_ignore_ascii_case("NTFS"),
        "guest run root must be on NTFS; found {}",
        filesystem.trim()
    );
    let rustc = helper(root, &["rustc", "-vV"], &env)?;
    ensure!(
        rustc.contains("host: x86_64-pc-windows-msvc"),
        "guest requires x86_64 MSVC Rust toolchain"
    );
    helper(root, &["where.exe", "cl.exe"], &env)?;
    helper(root, &["where.exe", "link.exe"], &env)?;
    // A real linker invocation catches missing SDK libraries and unusable VS
    // environments.
    let source = root.join("msvc-probe.rs");
    fs::write(&source, "fn main() {}\n")?;
    helper(
        root,
        &[
            "rustc",
            source.to_str().context("probe path is not UTF-8")?,
            "-o",
            root.join("msvc-probe.exe")
                .to_str()
                .context("probe path is not UTF-8")?,
        ],
        &env,
    )?;
    for requirement in requirements {
        match requirement.as_str() {
            "ntfs" | "msvc" => {}
            "symlinks" => {
                let link = root.join("capability-symlink");
                if !link.exists() {
                    std::os::windows::fs::symlink_file(&source, &link)
                        .context("suite requires symbolic-link permission")?;
                }
                ensure!(fs::read(&link)? == fs::read(&source)?, "symbolic-link probe failed");
            }
            "long-paths" => {
                let long = root
                    .join("long-path-capability")
                    .join("a".repeat(100))
                    .join("b".repeat(100))
                    .join("c".repeat(100));
                fs::create_dir_all(&long).context("suite requires long paths")?;
                fs::write(long.join("probe"), "long path")?;
            }
            _ => bail!("unsupported Windows suite requirement: {requirement}"),
        }
    }
    let os = helper(root, &["cmd.exe", "/u", "/d", "/c", "ver"], &env)?;
    let mut versions = BTreeMap::new();
    versions.insert("rustc".into(), rustc.trim().into());
    versions.insert(
        "guest_root".into(),
        root.parent()
            .and_then(Path::parent)
            .context("invalid guest run root")?
            .to_string_lossy()
            .into_owned(),
    );
    let free = helper(
        root,
        &[
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference='Stop'; (Get-Volume -FilePath $env:RS_INFRA_PROBE_ROOT).SizeRemaining",
        ],
        &env,
    )?;
    let available: u64 = free.trim().parse().context("invalid guest free-space probe result")?;
    ensure!(
        available >= 512 * 1024 * 1024,
        "Windows guest has less than 512 MiB available space"
    );
    versions.insert("guest_available_bytes".into(), available.to_string());
    let trim = helper(root, &["fsutil.exe", "behavior", "query", "DisableDeleteNotify"], &env)
        .unwrap_or_else(|error| format!("TRIM status unavailable: {error:#}"));
    versions.insert(
        "discard_trim".into(),
        format!("{}; physical host block reclamation is unverified", trim.trim()),
    );
    Ok(EnvironmentInfo {
        platform: crate::model::Platform::Windows,
        target: "x86_64-pc-windows-msvc".into(),
        os_version: os.trim().into(),
        filesystem: "NTFS".into(),
        versions,
    })
}

#[cfg(windows)]
fn msvc_environment_at(root: &Path) -> Result<BTreeMap<OsString, OsString>> {
    let program_files = std::env::var_os("ProgramFiles(x86)").context("ProgramFiles(x86) is unset")?;
    let vswhere = PathBuf::from(program_files).join("Microsoft Visual Studio/Installer/vswhere.exe");
    ensure!(
        vswhere.is_file(),
        "install official Visual Studio Installer vswhere.exe"
    );
    let installation = helper(
        root,
        &[
            vswhere.to_str().context("vswhere path is not UTF-8")?,
            "-utf8",
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ],
        &BTreeMap::new(),
    )?;
    let installation = installation.trim();
    ensure!(
        !installation.is_empty() && !installation.contains(['"', '&', '|', '<', '>', '^', '%', '!', '\r', '\n']),
        "invalid Visual Studio installation path"
    );
    let batch = Path::new(installation).join("Common7/Tools/VsDevCmd.bat");
    ensure!(batch.is_file(), "Visual Studio VsDevCmd.bat is missing");
    // Only the trusted VS installation path enters cmd; suite argv always uses
    // native spawn.
    let script = format!(
        "\"\"{}\" -no_logo -arch=x64 -host_arch=x64 >nul && set\"",
        batch.display()
    );
    let output = helper(root, &["cmd.exe", "/u", "/d", "/s", "/c", &script], &BTreeMap::new())?;
    let mut env = BTreeMap::new();
    for line in output.lines() {
        if line.starts_with('=') {
            continue;
        }
        if let Some((name, value)) = line.split_once('=') {
            env.insert(name.into(), value.into());
        }
    }
    ensure!(
        env.keys()
            .any(|key: &OsString| key.to_string_lossy().eq_ignore_ascii_case("VCToolsInstallDir")),
        "MSVC environment initialization failed"
    );
    Ok(env)
}

#[cfg(windows)]
fn helper(root: &Path, argv: &[&str], env: &BTreeMap<OsString, OsString>) -> Result<String> {
    let spec = ProcessSpec {
        argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
        cwd: root.to_owned(),
        env: env.clone(),
        timeout: Duration::from_secs(30),
        stdout: root.join("capability.stdout"),
        stderr: root.join("capability.stderr"),
    };
    let result = if argv
        .first()
        .is_some_and(|program| program.eq_ignore_ascii_case("cmd.exe"))
        && argv.len() == 6
        && argv[1..5] == ["/u", "/d", "/s", "/c"]
    {
        let script = OsString::from(argv[5]);
        let mut controlled = spec.argv.clone();
        controlled.pop();
        let trusted = ProcessSpec {
            argv: controlled,
            cwd: spec.cwd.clone(),
            env: spec.env.clone(),
            timeout: spec.timeout,
            stdout: spec.stdout.clone(),
            stderr: spec.stderr.clone(),
        };
        process::run_windows_script(&trusted, &script, &AtomicBool::new(false))?
    } else {
        process::run(&spec, &AtomicBool::new(false))?
    };
    ensure!(
        result.exit_code == Some(0) && !result.timed_out,
        "guest capability command failed: {}: {}",
        argv[0],
        fs::read_to_string(&spec.stderr).unwrap_or_default()
    );
    ensure!(
        fs::metadata(&spec.stdout)?.len() <= MAX_FRAME,
        "capability output exceeds limit"
    );
    let bytes = fs::read(spec.stdout)?;
    if argv.contains(&"/u") {
        ensure!(bytes.len() % 2 == 0, "invalid UTF-16 capability output");
        return String::from_utf16(
            &bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        )
        .context("invalid UTF-16 capability output");
    }
    if argv[0].eq_ignore_ascii_case("where.exe") {
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    String::from_utf8(bytes).context("capability output is not UTF-8")
}

#[cfg(windows)]
fn available_space(root: &Path) -> Result<u64> {
    let mut env = BTreeMap::new();
    env.insert("RS_INFRA_PROBE_ROOT".into(), root.as_os_str().to_owned());
    helper(
        root,
        &[
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference='Stop'; (Get-Volume -FilePath $env:RS_INFRA_PROBE_ROOT).SizeRemaining",
        ],
        &env,
    )?
    .trim()
    .parse()
    .context("invalid guest available-space measurement")
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::process::Command;

    use super::Request;
    use super::execute_commands;
    use crate::model::CleanupPolicy;
    use crate::run_store::RunStore;

    #[test]
    fn test_guest_preserves_real_completed_test_before_later_spawn_failure() {
        let root = tempfile::tempdir().expect("guest command fixture");
        let record = RunStore::create(&root.path().join("guest"), CleanupPolicy::Enabled).expect("guest run");
        let project = record.run_dir().join("snapshot/project");
        fs::create_dir_all(&project).expect("snapshot project");
        let source = project.join("passes.rs");
        fs::write(&source, "#[test] fn passes() {}\n").expect("actual Rust test source");
        let binary = project.join("passes-test");
        let compilation = Command::new("rustc")
            .arg("--test")
            .arg(&source)
            .arg("-o")
            .arg(&binary)
            .output()
            .expect("compile real guest test fixture");
        assert!(
            compilation.status.success(),
            "{}",
            String::from_utf8_lossy(&compilation.stderr)
        );
        let request: Request = serde_json::from_value(serde_json::json!({"protocol_version":1,"request_id":"execute","run_id":"aabb","lease_token":"00".repeat(32),"op":"execute","project_relative":"project","toolchain":"1.94.0","suite":{"id":"partial","platforms":["windows"],"target":null,"commands":[{"kind":"test","argv":[binary],"cwd":"."},{"kind":"test","argv":[project.join("missing-program")],"cwd":"."}],"timeout_seconds":10,"minimum_tests":1,"requirements":[],"data":[]}})).expect("guest request");
        let error =
            execute_commands(&record, &request, &BTreeMap::new()).expect_err("second command must fail to spawn");
        let completed =
            crate::report::completed_results(&error).expect("guest error must retain earlier real test results");
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].tests_passed, 1);
        let payload = super::partial_payload(&error).expect("completed guest error has a structured partial response");
        assert_eq!(payload["results"][0]["tests_passed"], 1);
        assert!(
            payload["infrastructure_error"]
                .as_str()
                .expect("infrastructure error")
                .contains("missing-program")
        );
        assert!(
            fs::read_to_string(&completed[0].stdout)
                .expect("first command stdout")
                .contains("1 passed")
        );
    }
}
