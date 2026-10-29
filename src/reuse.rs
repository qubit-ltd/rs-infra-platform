// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Exclusive retained-run reuse with an immutable report directory per attempt.

use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use cap_fs_ext::DirExt;
use cap_std::fs::OpenOptions;
use serde::Deserialize;
use serde::Serialize;

use crate::lease::Lease;
use crate::model::CleanupPolicy;
use crate::model::RunId;
use crate::process_identity;
use crate::run_store::OWNER_MARKER;
use crate::run_store::RunRecord;
use crate::run_store::RunState;
use crate::run_store::RunStore;
use crate::run_store::open_store;
use crate::run_store::read_nofollow;
use crate::run_store::secure_dir;
use crate::snapshot::Snapshot;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    version: u32,
    project: PathBuf,
    environment: String,
    toolchain: String,
    attempt: u64,
}

/// Retains both the tuple lease and the run lease until final report
/// publication.
pub(crate) struct Allocation {
    pub(crate) record: RunRecord,
    pub(crate) snapshot: Option<Snapshot>,
    pub(crate) report_dir: PathBuf,
    pub(crate) reuse: ReuseLease,
}

/// Serializes retained source synchronization and keeps its tuple registration.
pub(crate) struct ReuseLease {
    _lease: Lease,
    registration: Registration,
}

/// Claims a matching stopped retained run, or allocates a new retained run.
/// A matching run is synchronized only under both exact leases. Unsafe
/// metadata, marker mismatches, source-layout changes, or synchronization
/// errors are fatal; neither original project inputs nor historical reports are
/// overwritten.
pub(crate) fn allocate(
    root: &Path,
    project: &Path,
    environment: &str,
    toolchain: &str,
    data: &[PathBuf],
) -> Result<Allocation> {
    let project = project.canonicalize()?;
    RunStore::initialize(root)?;
    let lease = Lease::for_reuse(root, &project, environment, toolchain)?;
    let store = open_store(root)?;
    let records = store.open_dir_nofollow("records")?;
    let mut ids = Vec::new();
    for entry in records.entries()? {
        let name = entry?.file_name();
        if let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) {
            ids.push(RunId(id.to_owned()));
        }
    }
    ids.sort_by(|left, right| right.0.cmp(&left.0));
    for id in ids {
        let record = RunStore::load(root, &id)?;
        if !eligible(&record) {
            continue;
        }
        let reports = secure_dir(&record.report_dir(), false)?;
        let registration = match read_nofollow(&reports, "reuse.json") {
            Ok(bytes) => serde_json::from_slice::<Registration>(&bytes).context("parse retained reuse registration")?,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error).context("read retained reuse registration"),
        };
        ensure!(registration.version == 1, "unsupported retained registration version");
        if registration.project != project
            || registration.environment != environment
            || registration.toolchain != toolchain
        {
            continue;
        }
        let run_lease = Lease::acquire(root, &format!("run:{}", id.0)).context("matching retained run is in use")?;
        let mut record = RunStore::load(root, &id)?;
        ensure!(
            eligible(&record),
            "retained registration changed while acquiring its run lease"
        );
        record.lease = Some(run_lease);
        let run = secure_dir(&record.run_dir(), false)?;
        ensure!(
            read_nofollow(&run, OWNER_MARKER)? == record.owner.as_bytes(),
            "retained run owner marker mismatch"
        );
        let prior_report = attempt_dir(&record.report_dir(), registration.attempt);
        let prior = secure_dir(&prior_report, false)?;
        let snapshot: Snapshot = serde_json::from_slice(&read_nofollow(&prior, "snapshot.json")?)
            .context("read retained snapshot manifest")?;
        ensure!(
            snapshot.root.canonicalize()? == record.run_dir().join("snapshot").canonicalize()?,
            "retained snapshot root does not match its owned run"
        );
        let mut registration = registration;
        registration.attempt = registration
            .attempt
            .checked_add(1)
            .context("retained attempt counter overflow")?;
        let report_dir = attempt_dir(&record.report_dir(), registration.attempt);
        reports
            .create_dir(format!("attempt-{}", registration.attempt))
            .context("retained attempt report directory must be new")?;
        refresh_owner(&mut record)?;
        let snapshot = match snapshot.synchronize(&project, data, &lease) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                record.state = RunState::Retained;
                record.persist()?;
                return Err(error)
                    .context("synchronize retained source; run without --no-cleanup for a fresh snapshot");
            }
        };
        record.snapshot_manifest = snapshot
            .entries
            .iter()
            .map(|entry| entry.relative_path.clone())
            .collect();
        record.persist()?;
        let reuse = ReuseLease {
            _lease: lease,
            registration,
        };
        if let Err(error) = reuse.save_snapshot(&record, &report_dir, &snapshot) {
            record.state = RunState::Retained;
            if let Err(persist_error) = record.persist() {
                return Err(error).context(format!("persist stopped reuse state also failed: {persist_error:#}"));
            }
            return Err(error).context("publish retained snapshot attempt");
        }
        return Ok(Allocation {
            record,
            snapshot: Some(snapshot),
            report_dir,
            reuse,
        });
    }
    let record = RunStore::create(root, CleanupPolicy::Disabled)?;
    let report_dir = record.report_dir();
    let reuse = ReuseLease {
        _lease: lease,
        registration: Registration {
            version: 1,
            project,
            environment: environment.to_owned(),
            toolchain: toolchain.to_owned(),
            attempt: 0,
        },
    };
    Ok(Allocation {
        record,
        snapshot: None,
        report_dir,
        reuse,
    })
}

impl ReuseLease {
    /// Publishes the synchronized manifest before advancing the durable attempt
    /// index. Returns ownership, serialization or IO failures while both
    /// leases remain held.
    pub(crate) fn save_snapshot(&self, record: &RunRecord, report_dir: &Path, snapshot: &Snapshot) -> Result<()> {
        self._lease.require_reuse(&self.registration.project)?;
        record
            .lease
            .as_ref()
            .context("retained run lease missing")?
            .require_run(&record.run_id.0)?;
        write_json(report_dir, "snapshot.json", &serde_json::to_vec_pretty(snapshot)?)?;
        write_json(
            &record.report_dir(),
            "reuse.json",
            &serde_json::to_vec_pretty(&self.registration)?,
        )
    }
}

fn eligible(record: &RunRecord) -> bool {
    record.is_local_host()
        && record.policy == CleanupPolicy::Disabled
        && record.state == RunState::Retained
        && record.pending_guest.is_empty()
        && record
            .environments
            .iter()
            .all(|environment| !environment.owned || environment.stopped)
}

fn refresh_owner(record: &mut RunRecord) -> Result<()> {
    record.pid = std::process::id();
    record.host_id = process_identity::host_id();
    record.boot_id = process_identity::boot_id();
    record.process_start = process_identity::process_start(record.pid);
    record.heartbeat_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    record.state = RunState::Active;
    record.persist()
}

fn attempt_dir(root: &Path, attempt: u64) -> PathBuf {
    if attempt == 0 {
        root.to_owned()
    } else {
        root.join(format!("attempt-{attempt}"))
    }
}

fn write_json(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let directory = secure_dir(root, false)?;
    let temporary = format!("{name}.tmp");
    let mut file = directory.open_with(&temporary, OpenOptions::new().write(true).create_new(true))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    directory.rename(&temporary, &directory, name)?;
    #[cfg(unix)]
    directory.open(".")?.sync_all()?;
    Ok(())
}
