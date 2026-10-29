// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Durable ownership records kept outside disposable run artifacts.

use std::io::ErrorKind;
use std::io::Read;
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

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

use crate::lease::FileIdentity;
use crate::lease::Lease;
use crate::model::CleanupPolicy;
use crate::model::RunId;
use crate::process_identity::boot_id;
use crate::process_identity::host_id;
use crate::process_identity::process_start;

pub(crate) const OWNER_MARKER: &str = ".rs-infra-platform-owner";
const STORE_MARKER: &str = ".rs-infra-platform-store";
const STORE_VERSION: &str = "rs-infra-platform-store-v1\n";
static NONCE: AtomicU64 = AtomicU64::new(0);

/// Persistent states survive process crashes and retain explicit opt-out runs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RunState {
    Active,
    Finalizing,
    Retained,
    CleanupPending,
    Cleaned,
}

/// Durable ownership information for a run-scoped execution environment.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvironmentLease {
    pub(crate) key: String,
    pub(crate) owned: bool,
    pub(crate) stopped: bool,
}

/// The live value owns the run lease; serialized records never contain a live
/// lock.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunRecord {
    pub(crate) run_id: RunId,
    pub(crate) owner: String,
    pub(crate) host_id: Option<String>,
    pub(crate) boot_id: Option<String>,
    pub(crate) pid: u32,
    pub(crate) process_start: Option<String>,
    pub(crate) heartbeat_unix_ms: u64,
    pub(crate) policy: CleanupPolicy,
    pub(crate) state: RunState,
    pub(crate) environments: Vec<EnvironmentLease>,
    pub(crate) snapshot_manifest: Vec<PathBuf>,
    pub(crate) pending_guest: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) cleanup_directory_identity: Option<FileIdentity>,
    #[serde(default)]
    pub(crate) marker_removal_pending: bool,
    #[serde(skip)]
    pub(crate) root: PathBuf,
    #[serde(skip)]
    pub(crate) lease: Option<Lease>,
}

/// Opens and allocates a tool-owned user state store.
pub(crate) struct RunStore;

impl RunStore {
    /// Initializes a marked store without allocating a run or acquiring its run
    /// lease. Rejects symlinked roots and unmarked nonempty directories,
    /// like `create`.
    pub(crate) fn initialize(root: &Path) -> Result<()> {
        let directory = secure_dir(root, true)?;
        initialize(&directory)?;
        for name in ["records", "runs", "reports", "locks"] {
            create_child(&directory, name)?;
        }
        Ok(())
    }

    /// Creates a marked run, durable record, separate report directory, and
    /// live lease. Rejects symlinked state paths, unsafe roots, and
    /// unmarked nonempty directories.
    pub(crate) fn create(root: &Path, policy: CleanupPolicy) -> Result<RunRecord> {
        Self::initialize(root)?;
        let directory = open_store(root)?;
        let root = std::path::absolute(root)?;
        let run_id = RunId(unique_id());
        let lease = Lease::acquire(&root, &format!("run:{}", run_id.0))?;
        let owner = format!("rs-infra-platform-v1:{}:{}", run_id.0, unique_id());
        let runs = directory.open_dir_nofollow("runs")?;
        runs.create_dir(&run_id.0)?;
        let run = runs.open_dir_nofollow(&run_id.0)?;
        let mut marker = run.open_with(OWNER_MARKER, OpenOptions::new().write(true).create_new(true))?;
        marker.write_all(owner.as_bytes())?;
        marker.sync_all()?;
        let reports = directory.open_dir_nofollow("reports")?;
        reports.create_dir(&run_id.0)?;
        let record = RunRecord {
            run_id,
            owner,
            host_id: host_id(),
            boot_id: boot_id(),
            pid: std::process::id(),
            process_start: process_start(std::process::id()),
            heartbeat_unix_ms: now_ms(),
            policy,
            state: RunState::Active,
            environments: Vec::new(),
            snapshot_manifest: Vec::new(),
            pending_guest: Vec::new(),
            cleanup_directory_identity: None,
            marker_removal_pending: false,
            root,
            lease: Some(lease),
        };
        record.persist()?;
        Ok(record)
    }

    /// Loads a validated record without acquiring its lease or mutating the
    /// store. Returns an error for unregistered IDs, mismatched records, or
    /// unsafe store paths.
    pub(crate) fn load(root: &Path, id: &RunId) -> Result<RunRecord> {
        validate_run_id(id)?;
        let directory = open_store(root)?;
        let records = directory.open_dir_nofollow("records")?;
        let bytes = read_nofollow(&records, &format!("{}.json", id.0))?;
        let mut record: RunRecord = serde_json::from_slice(&bytes).context("parse durable run record")?;
        ensure!(record.run_id == *id, "run record ID does not match its registration");
        ensure!(
            record.owner.starts_with(&format!("rs-infra-platform-v1:{}:", id.0)),
            "invalid ownership registration"
        );
        record.root = std::path::absolute(root)?;
        Ok(record)
    }
}

impl RunRecord {
    /// Reports whether this machine owns the registration's stable host
    /// identity.
    pub(crate) fn is_local_host(&self) -> bool {
        self.host_id.is_some() && self.host_id == host_id()
    }

    /// Returns this run's disposable host directory; no recorded guest path is
    /// used.
    pub(crate) fn run_dir(&self) -> PathBuf {
        self.root.join("runs").join(&self.run_id.0)
    }

    /// Returns the durable report directory, which cleanup preserves.
    pub(crate) fn report_dir(&self) -> PathBuf {
        self.root.join("reports").join(&self.run_id.0)
    }

    /// Atomically replaces the JSON record after syncing its complete temporary
    /// file. Returns errors for unsafe stores, invalid IDs, serialization,
    /// or filesystem failures.
    pub(crate) fn persist(&self) -> Result<()> {
        validate_run_id(&self.run_id)?;
        if let Some(lease) = &self.lease {
            lease.require_run(&self.run_id.0)?;
        }
        let directory = open_store(&self.root)?;
        let records = directory.open_dir_nofollow("records")?;
        let temporary = format!("{}.{}.tmp", self.run_id.0, unique_id());
        let bytes = serde_json::to_vec_pretty(self)?;
        let mut file = records.open_with(&temporary, OpenOptions::new().write(true).create_new(true))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        records.rename(&temporary, &records, format!("{}.json", self.run_id.0))?;
        #[cfg(unix)]
        records.open(".")?.sync_all()?;
        Ok(())
    }

    /// Persists a fresh heartbeat; IO failures leave the prior complete record
    /// readable.
    pub(crate) fn heartbeat(&mut self) -> Result<()> {
        self.heartbeat_unix_ms = now_ms();
        self.persist()
    }

    /// Returns `Some(false)` only when this host can establish the original
    /// process is dead. Foreign-host or unsupported process identity
    /// returns `None` and recovery skips it.
    pub(crate) fn owner_alive(&self) -> Option<bool> {
        crate::process_identity::owner_alive(
            self.host_id.as_deref(),
            self.boot_id.as_deref(),
            self.pid,
            self.process_start.as_deref(),
        )
    }
}

/// Rejects separators, traversal, and values outside the generated run-ID
/// alphabet.
pub(crate) fn validate_run_id(id: &RunId) -> Result<()> {
    ensure!(
        !id.0.is_empty() && id.0.len() <= 128 && id.0.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-'),
        "invalid run ID"
    );
    Ok(())
}

/// Opens each directory component with no-follow semantics and retains its
/// handle. Rejects filesystem roots, parent traversal, symlinks, and
/// non-directory components.
pub(crate) fn secure_dir(path: &Path, create: bool) -> Result<Dir> {
    let absolute = std::path::absolute(path)?;
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => anchor.push(prefix.as_os_str()),
            Component::RootDir => anchor.push(component.as_os_str()),
            Component::Normal(name) => names.push(name.to_owned()),
            Component::CurDir => {}
            Component::ParentDir => bail!("parent traversal is not allowed in tool state paths"),
        }
    }
    ensure!(!names.is_empty(), "refusing filesystem root as tool state directory");
    let mut directory = Dir::open_ambient_dir(anchor, cap_std::ambient_authority())?;
    for name in names {
        if create {
            match directory.create_dir(&name) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        directory = directory
            .open_dir_nofollow(&name)
            .with_context(|| format!("unsafe tool state component: {}", name.to_string_lossy()))?;
    }
    Ok(directory)
}

/// Opens only a marked store; does not adopt unmarked directories during
/// cleanup.
pub(crate) fn open_store(root: &Path) -> Result<Dir> {
    let directory = secure_dir(root, false)?;
    ensure!(
        read_nofollow(&directory, STORE_MARKER)? == STORE_VERSION.as_bytes(),
        "state store marker mismatch"
    );
    Ok(directory)
}

/// Reads an ordinary file relative to an already validated parent without
/// following links.
pub(crate) fn read_nofollow(directory: &Dir, name: &str) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = directory.open_with(name, &options)?;
    ensure!(file.metadata()?.is_file(), "expected a regular registration file");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn initialize(directory: &Dir) -> Result<()> {
    match directory.symlink_metadata(STORE_MARKER) {
        Ok(_) => ensure!(
            read_nofollow(directory, STORE_MARKER)? == STORE_VERSION.as_bytes(),
            "state store marker mismatch"
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            ensure!(
                directory.entries()?.next().is_none(),
                "refusing to adopt nonempty unmarked state directory"
            );
            let mut marker = directory.open_with(STORE_MARKER, OpenOptions::new().write(true).create_new(true))?;
            marker.write_all(STORE_VERSION.as_bytes())?;
            marker.sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn create_child(directory: &Dir, name: &str) -> Result<()> {
    match directory.create_dir(name) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    directory.open_dir_nofollow(name)?;
    Ok(())
}

fn unique_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{nanos:x}-{:x}-{:x}",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::RunStore;
    use crate::model::CleanupPolicy;

    #[test]
    fn test_create_separates_records_reports_and_artifacts() {
        let root = tempfile::tempdir().expect("store fixture");
        let run = RunStore::create(root.path(), CleanupPolicy::Enabled).expect("create run");
        assert!(run.run_dir().join(".rs-infra-platform-owner").is_file());
        assert!(run.report_dir().is_dir());
        assert!(
            root.path()
                .join("records")
                .join(format!("{}.json", run.run_id.0))
                .is_file()
        );
    }
}
