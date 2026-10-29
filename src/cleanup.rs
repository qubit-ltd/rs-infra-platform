// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Lease-protected, ownership-checked host artifact cleanup.

use std::io::ErrorKind;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use cap_fs_ext::DirExt;

use crate::lease::FileIdentity;
use crate::lease::Lease;
use crate::lease::pin_directory;
use crate::model::CleanupPolicy;
use crate::model::CleanupReport;
use crate::run_store::OWNER_MARKER;
use crate::run_store::RunRecord;
use crate::run_store::RunState;
use crate::run_store::open_store;
use crate::run_store::read_nofollow;

/// Finalizes a run while its existing lease protects cleanup and report
/// publication. Cleanup failures remain structured in the report;
/// registration/persistence failures return Err. Disabled cleanup retains
/// artifacts; unconfirmed stops remain `CleanupPending`. Intentional file
/// retention alone does not count as a cleanup error.
pub(crate) fn finish_run(record: &mut RunRecord) -> Result<CleanupReport> {
    record
        .lease
        .as_ref()
        .context("run cleanup requires its exclusive lease")?
        .require_run(&record.run_id.0)?;
    record.state = RunState::Finalizing;
    record.persist()?;
    let mut report = CleanupReport::default();
    if record.policy == CleanupPolicy::Disabled {
        report.remaining.push(record.run_dir());
        if record
            .environments
            .iter()
            .any(|environment| environment.owned && !environment.stopped)
        {
            report
                .errors
                .push("owned environment termination remains unconfirmed; retained files preserved".to_owned());
        }
        for path in &record.pending_guest {
            report.remaining.push(path.clone());
            report
                .errors
                .push(format!("environment stop pending: {}", path.display()));
        }
        record.state = if report.errors.is_empty() {
            RunState::Retained
        } else {
            RunState::CleanupPending
        };
        record.persist()?;
        return Ok(report);
    }
    if record
        .environments
        .iter()
        .any(|environment| environment.owned && !environment.stopped)
    {
        report
            .errors
            .push("owned environment is still running; refusing artifact cleanup".to_owned());
        report.remaining.push(record.run_dir());
    } else if let Err(error) = remove_owned_artifacts(record) {
        report.errors.push(format!("host artifact cleanup failed: {error:#}"));
        report.remaining.push(record.run_dir());
    }
    for path in &record.pending_guest {
        report.remaining.push(path.clone());
        report.errors.push(format!("guest cleanup pending: {}", path.display()));
    }
    record.state = if report.errors.is_empty() {
        RunState::Cleaned
    } else {
        RunState::CleanupPending
    };
    record.persist()?;
    Ok(report)
}

fn remove_owned_artifacts(record: &mut RunRecord) -> Result<()> {
    let store = open_store(&record.root)?;
    let runs = store.open_dir_nofollow("runs")?;
    let _runs_pin = pin_directory(&runs)?;
    let metadata = match runs.symlink_metadata(&record.run_id.0) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "run path is not an ordinary directory"
    );
    let run = runs
        .open_dir_nofollow(&record.run_id.0)
        .context("open owned run directory")?;
    let run_pin = pin_directory(&run)?;
    let identity = FileIdentity::directory(&run)?;
    let marker = read_nofollow(&run, OWNER_MARKER);
    if let Ok(marker) = marker {
        ensure!(marker == record.owner.as_bytes(), "run owner marker mismatch");
        record.cleanup_directory_identity = Some(identity);
        record.persist()?;
        remove_contents(&run, record.lease.as_ref().context("missing cleanup lease")?, true)?;
        // Journal the narrow empty-root phase before unlinking the final ownership
        // marker.
        record.marker_removal_pending = true;
        record.persist()?;
        run.remove_file(OWNER_MARKER)?;
    } else {
        ensure!(
            record.marker_removal_pending && record.cleanup_directory_identity == Some(identity),
            "missing run owner marker without an authorized final directory removal"
        );
        ensure!(
            run.entries()?.next().is_none(),
            "unmarked pending run directory is not empty"
        );
    }
    // Windows keeps the no-FILE_SHARE_DELETE pin throughout recursion. Once
    // released, only an empty-directory operation remains; there is never
    // ambient recursive deletion.
    drop(run_pin);
    drop(run);
    record
        .lease
        .as_ref()
        .context("missing cleanup lease")?
        .require_run(&record.run_id.0)?;
    runs.remove_dir(&record.run_id.0)
        .context("remove empty owned run directory")?;
    Ok(())
}

fn remove_contents(directory: &cap_std::fs::Dir, lease: &Lease, preserve_marker: bool) -> Result<()> {
    let names = directory
        .entries()?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    for name in names {
        if preserve_marker && name == OWNER_MARKER {
            continue;
        }
        lease.validate()?;
        let metadata = directory.symlink_metadata(&name)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            let child = directory.open_dir_nofollow(&name)?;
            let pin = pin_directory(&child)?;
            remove_contents(&child, lease, false)?;
            drop(pin);
            drop(child);
            lease.validate()?;
            directory.remove_dir(&name)?;
        } else {
            #[cfg(windows)]
            {
                match directory.remove_file(&name) {
                    Ok(()) => {}
                    Err(_) if metadata.file_type().is_symlink() => directory.remove_dir(&name)?,
                    Err(error) => return Err(error.into()),
                }
            }
            #[cfg(not(windows))]
            directory.remove_file(&name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use anyhow::Result;

    use super::finish_run;
    use crate::host_config::HostConfig;
    use crate::host_config::StorageConfig;
    use crate::model::CleanupPolicy;
    use crate::model::CleanupReport;
    use crate::model::RunId;
    use crate::run_store::OWNER_MARKER;
    use crate::run_store::RunRecord;
    use crate::run_store::RunState;
    use crate::run_store::RunStore;

    fn settings(root: &Path) -> HostConfig {
        HostConfig {
            storage: StorageConfig { root: root.to_owned() },
            ..HostConfig::default()
        }
    }

    fn clean_run(root: &Path, id: &RunId, explicit: bool) -> Result<CleanupReport> {
        crate::executor::clean_registered_run(&settings(root), id, explicit)
    }

    fn recover_stale(root: &Path) -> Result<Vec<CleanupReport>> {
        crate::executor::recover_registered_runs(&settings(root))
    }

    fn release_finalizing(run: &mut RunRecord) -> Result<()> {
        run.state = RunState::Finalizing;
        run.persist()?;
        run.lease.take();
        Ok(())
    }

    #[test]
    fn test_cleanup_requires_ownership_and_preserves_reports() {
        let root = tempfile::tempdir().expect("cleanup fixture");
        let mut run = RunStore::create(root.path(), CleanupPolicy::Enabled).expect("create run");
        fs::write(run.report_dir().join("log.txt"), "durable").expect("report log");
        fs::write(run.run_dir().join("artifact"), "disposable").expect("artifact");
        assert!(
            clean_run(root.path(), &run.run_id, true).is_err(),
            "active lease must prevent a second cleaner"
        );
        let report = finish_run(&mut run).expect("finish run");
        assert!(report.errors.is_empty());
        assert!(!run.run_dir().exists());
        assert_eq!(
            fs::read_to_string(run.report_dir().join("log.txt")).expect("preserved log"),
            "durable"
        );
        assert_eq!(
            RunStore::load(root.path(), &run.run_id).expect("durable record").state,
            RunState::Cleaned
        );
    }

    #[test]
    fn test_missing_and_mismatched_markers_refuse_deletion() {
        let root = tempfile::tempdir().expect("cleanup fixture");
        let mut run = RunStore::create(root.path(), CleanupPolicy::Enabled).expect("create run");
        fs::write(run.run_dir().join(OWNER_MARKER), "foreign").expect("tampered marker");
        let report = finish_run(&mut run).expect("structured failure");
        assert!(!report.errors.is_empty());
        assert!(run.run_dir().exists());
        fs::remove_file(run.run_dir().join(OWNER_MARKER)).expect("remove fixture marker");
        assert!(!finish_run(&mut run).expect("missing marker report").errors.is_empty());
        assert!(clean_run(root.path(), &RunId("../outside".to_owned()), true).is_err());
    }

    #[test]
    fn test_retained_runs_are_not_recovered_automatically() {
        let root = tempfile::tempdir().expect("retained fixture");
        let mut run = RunStore::create(root.path(), CleanupPolicy::Disabled).expect("retained run");
        finish_run(&mut run).expect("retain run");
        assert_eq!(run.state, RunState::Retained);
        drop(run.lease.take());
        assert!(recover_stale(root.path()).expect("stale scan").is_empty());
        assert!(run.run_dir().exists());
        clean_run(root.path(), &run.run_id, true).expect("explicit retained cleanup");
        assert!(!run.run_dir().exists());
    }

    #[test]
    fn test_disabled_cleanup_keeps_files_pending_until_stop_is_confirmed() {
        for pending_environment in [false, true] {
            let root = tempfile::tempdir().expect("disabled stop fixture");
            let mut run = RunStore::create(root.path(), CleanupPolicy::Disabled).expect("retained run");
            fs::write(run.run_dir().join("keep-data"), "retained bytes").expect("retained data");
            if pending_environment {
                run.environments.push(crate::run_store::EnvironmentLease {
                    key: "owned-vm".to_owned(),
                    owned: true,
                    stopped: false,
                });
            } else {
                run.pending_guest.push(run.report_dir());
            }
            let report = finish_run(&mut run).expect("pending stop report");
            assert_eq!(run.state, RunState::CleanupPending);
            assert!(!report.errors.is_empty(), "unconfirmed stop remains an error");
            assert!(report.remaining.contains(&run.run_dir()));
            assert_eq!(
                fs::read_to_string(run.run_dir().join("keep-data")).expect("pending data preserved"),
                "retained bytes"
            );
            for environment in &mut run.environments {
                environment.stopped = true;
            }
            run.pending_guest.clear();
            let report = finish_run(&mut run).expect("confirmed stop report");
            assert_eq!(run.state, RunState::Retained);
            assert!(
                report.errors.is_empty(),
                "intentional file retention is not a cleanup error"
            );
            assert!(report.remaining.contains(&run.run_dir()));
            assert_eq!(
                fs::read_to_string(run.run_dir().join("keep-data")).expect("retained data preserved"),
                "retained bytes"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_next_stop_only_recovery_returns_disabled_pending_run_to_retained() {
        let root = tempfile::tempdir().expect("stop-only recovery fixture");
        let mut run = RunStore::create(root.path(), CleanupPolicy::Disabled).expect("retained run");
        fs::write(run.run_dir().join("keep-data"), "retained bytes").expect("retained data");
        run.environments.push(crate::run_store::EnvironmentLease {
            key: "process-tree:fixture:sha256=unavailable".to_owned(),
            owned: true,
            stopped: false,
        });
        run.pending_guest.push(run.report_dir());
        finish_run(&mut run).expect("record unconfirmed stop");
        assert_eq!(run.state, RunState::CleanupPending);
        // A confirmed different boot proves the registered owned process tree cannot
        // survive.
        run.boot_id = Some("previous-boot-session-fixture".to_owned());
        run.persist().expect("simulate previous boot registration");
        run.lease.take();
        let reports = recover_stale(root.path()).expect("real coordinator stop-only recovery");
        assert_eq!(reports.len(), 1);
        assert!(
            reports[0].errors.is_empty(),
            "stop-only recovery report: {:?}",
            reports[0].errors
        );
        let recovered = RunStore::load(root.path(), &run.run_id).expect("recovered record");
        assert_eq!(recovered.state, RunState::Retained);
        assert!(recovered.pending_guest.is_empty());
        assert!(recovered.environments.iter().all(|environment| environment.stopped));
        assert_eq!(
            fs::read_to_string(run.run_dir().join("keep-data")).expect("stop-only keeps files"),
            "retained bytes"
        );
        assert!(recover_stale(root.path()).expect("normal retained scan").is_empty());
    }

    #[test]
    fn test_pending_guest_cleanup_survives_host_cleanup() {
        let root = tempfile::tempdir().expect("guest fixture");
        let mut run = RunStore::create(root.path(), CleanupPolicy::Enabled).expect("create run");
        run.pending_guest.push("guest-unreachable".into());
        let report = finish_run(&mut run).expect("pending guest report");
        assert!(!report.remaining.is_empty());
        assert!(!run.run_dir().exists());
        assert_eq!(
            RunStore::load(root.path(), &run.run_id).expect("pending record").state,
            RunState::CleanupPending
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_recovery_distinguishes_dead_live_and_locked_owners() {
        let root = tempfile::tempdir().expect("recovery fixture");
        let mut dead = RunStore::create(root.path(), CleanupPolicy::Enabled).expect("dead run");
        dead.pid = i32::MAX as u32;
        dead.persist().expect("dead owner registration");
        dead.lease.take();
        let mut live = RunStore::create(root.path(), CleanupPolicy::Enabled).expect("live run");
        live.lease.take();
        let mut locked = RunStore::create(root.path(), CleanupPolicy::Enabled).expect("locked run");
        locked.pid = i32::MAX as u32;
        locked.persist().expect("locked dead owner");
        let reports = recover_stale(root.path()).expect("recover only dead unlocked run");
        assert_eq!(reports.len(), 1);
        assert!(!dead.run_dir().exists());
        assert!(live.run_dir().exists());
        assert!(locked.run_dir().exists());
    }

    #[test]
    fn test_foreign_host_finalizing_run_is_not_recovered() {
        let root = tempfile::tempdir().expect("foreign host fixture");
        let mut run = RunStore::create(root.path(), CleanupPolicy::Enabled).expect("foreign run");
        run.host_id = Some("another-machine".to_owned());
        release_finalizing(&mut run).expect("foreign stopped registration");
        assert!(recover_stale(root.path()).expect("foreign scan").is_empty());
        assert!(run.run_dir().exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_partial_deletion_preserves_marker_and_can_be_retried() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("partial cleanup fixture");
        let mut run = RunStore::create(root.path(), CleanupPolicy::Enabled).expect("owned run");
        let blocked = run.run_dir().join("blocked");
        fs::create_dir(&blocked).expect("blocked directory");
        fs::write(blocked.join("input"), "locked").expect("blocked input");
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o0)).expect("block directory access");
        let report = finish_run(&mut run).expect("cleanup failure report");
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).expect("restore fixture access");
        assert!(!report.errors.is_empty());
        assert!(
            run.run_dir().join(OWNER_MARKER).is_file(),
            "marker survives a partial cleanup failure"
        );
        assert!(finish_run(&mut run).expect("retry cleanup").errors.is_empty());
        assert!(!run.run_dir().exists());
    }

    #[test]
    fn test_final_empty_root_removal_is_recoverable_after_marker_was_removed() {
        let root = tempfile::tempdir().expect("final removal fixture");
        let mut run = RunStore::create(root.path(), CleanupPolicy::Enabled).expect("owned run");
        let directory = crate::run_store::secure_dir(&run.run_dir(), false).expect("owned directory handle");
        run.cleanup_directory_identity =
            Some(crate::lease::FileIdentity::directory(&directory).expect("directory identity"));
        drop(directory);
        run.marker_removal_pending = true;
        release_finalizing(&mut run).expect("persist final removal journal");
        fs::remove_file(run.run_dir().join(OWNER_MARKER)).expect("simulate crash after final marker unlink");
        assert!(
            clean_run(root.path(), &run.run_id, true)
                .expect("retry empty final directory removal")
                .errors
                .is_empty()
        );
        assert!(!run.run_dir().exists());
    }

    #[test]
    fn test_unmarked_final_root_with_new_files_is_not_recursed() {
        let root = tempfile::tempdir().expect("final root fixture");
        let mut run = RunStore::create(root.path(), CleanupPolicy::Enabled).expect("owned run");
        let directory = crate::run_store::secure_dir(&run.run_dir(), false).expect("owned directory handle");
        run.cleanup_directory_identity =
            Some(crate::lease::FileIdentity::directory(&directory).expect("directory identity"));
        drop(directory);
        run.marker_removal_pending = true;
        release_finalizing(&mut run).expect("persist final removal journal");
        fs::remove_file(run.run_dir().join(OWNER_MARKER)).expect("simulate final marker unlink");
        fs::write(run.run_dir().join("unexpected"), "must survive").expect("unexpected replacement input");
        assert!(
            !clean_run(root.path(), &run.run_id, true)
                .expect("structured refusal")
                .errors
                .is_empty()
        );
        assert_eq!(
            fs::read_to_string(run.run_dir().join("unexpected")).expect("unknown preserved"),
            "must survive"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_run_and_parent_cannot_escape_store() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().expect("store fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        fs::write(outside.path().join("precious"), "preserve").expect("outside file");
        let mut run = RunStore::create(root.path(), CleanupPolicy::Enabled).expect("create run");
        let held = run.run_dir().with_extension("held");
        fs::rename(run.run_dir(), &held).expect("move disposable fixture");
        symlink(outside.path(), run.run_dir()).expect("malicious run link");
        assert!(!finish_run(&mut run).expect("link rejection").errors.is_empty());
        assert_eq!(
            fs::read_to_string(outside.path().join("precious")).expect("outside preserved"),
            "preserve"
        );
        let alias = outside.path().join("state-link");
        symlink(root.path(), &alias).expect("state alias");
        assert!(clean_run(&alias, &run.run_id, true).is_err());
        assert!(held.exists());
    }
}
