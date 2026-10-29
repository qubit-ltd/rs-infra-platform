// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Exclusive leases whose registered filesystem identities survive path
//! replacement checks.

use std::fs::File;
use std::io::ErrorKind;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use cap_fs_ext::DirExt;
use cap_fs_ext::FollowSymlinks;
use cap_fs_ext::MetadataExt;
use cap_fs_ext::OpenOptionsFollowExt;
use cap_std::fs::Dir;
use cap_std::fs::OpenOptions;
use fs2::FileExt;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::run_store::read_nofollow;
use crate::run_store::secure_dir;

const AUTHORITY: &str = ".rs-infra-platform-lease-authority";
static REGISTRATION_NONCE: AtomicU64 = AtomicU64::new(0);

/// Filesystem object identity is read from an open handle rather than a
/// pathname.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    /// Returns the stable identity of the opened directory, or its metadata IO
    /// error.
    pub(crate) fn directory(directory: &Dir) -> Result<Self> {
        let metadata = directory.dir_metadata()?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn file(file: &File) -> Result<Self> {
        let metadata = cap_std::fs::File::from_std(file.try_clone()?).metadata()?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Authority {
    store: FileIdentity,
    locks: FileIdentity,
}

/// Releases ownership explicitly, including when initialization fails after
/// locking. Closing the last descriptor is insufficient when a concurrent
/// Unix fork temporarily retains the same open file description.
#[derive(Debug)]
struct LeaseLock {
    file: File,
}

impl Drop for LeaseLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Retains the lock, registration identities, and Windows no-delete directory
/// guards.
#[derive(Debug)]
pub(crate) struct Lease {
    lock: LeaseLock,
    store: Dir,
    locks: Dir,
    authority: FileIdentity,
    file_identity: FileIdentity,
    key: String,
    file_name: String,
    root: PathBuf,
    _pins: Vec<cap_std::fs::File>,
    reuse_project: Option<PathBuf>,
}

impl Lease {
    /// Acquires an exclusive nonblocking lease with immutable per-key identity
    /// registration. Rejects replaced lock files/directories, unsafe
    /// parents, or another active owner.
    pub(crate) fn acquire(root: &Path, key: &str) -> Result<Self> {
        let directory = secure_dir(root, true)?;
        let mut pins = pin_ancestors(root)?;
        if let Some(pin) = pin_directory(&directory)? {
            pins.push(pin);
        }
        match directory.create_dir("locks") {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let locks = directory.open_dir_nofollow("locks")?;
        if let Some(pin) = pin_directory(&locks)? {
            pins.push(pin);
        }
        let registration = Authority {
            store: FileIdentity::directory(&directory)?,
            locks: FileIdentity::directory(&locks)?,
        };
        register_identity(&directory, AUTHORITY, &registration)?;
        let authority = FileIdentity::file(&directory.open(AUTHORITY)?.into_std())?;
        let digest = Sha256::digest(key.as_bytes());
        let file_name = format!(
            "{}.lock",
            digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
        );
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).follow(FollowSymlinks::No);
        #[cfg(windows)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.share_mode(0x1 | 0x2);
        }
        let file = locks.open_with(&file_name, &options)?.into_std();
        FileExt::try_lock_exclusive(&file).with_context(|| format!("lease already held: {key}"))?;
        let lock = LeaseLock { file };
        let file_identity = FileIdentity::file(&lock.file)?;
        register_identity(
            &directory,
            &format!(".rs-infra-platform-lease-{}", file_name),
            &file_identity,
        )?;
        let lease = Self {
            lock,
            store: directory,
            locks,
            authority,
            file_identity,
            key: key.to_owned(),
            file_name,
            root: std::path::absolute(root)?,
            _pins: pins,
            reuse_project: None,
        };
        lease.validate()?;
        Ok(lease)
    }

    /// Verifies that all current paths still refer to the held store, locks,
    /// and lock file. Replacement or registration tampering returns an
    /// error before a destructive operation.
    pub(crate) fn validate(&self) -> Result<()> {
        let store = secure_dir(&self.root, false)?;
        ensure!(
            FileIdentity::directory(&store)? == FileIdentity::directory(&self.store)?,
            "lease store directory identity changed"
        );
        let locks = store.open_dir_nofollow("locks")?;
        ensure!(
            FileIdentity::directory(&locks)? == FileIdentity::directory(&self.locks)?,
            "lease lock directory identity changed"
        );
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let authority = store.open_with(AUTHORITY, &options)?.into_std();
        ensure!(
            FileIdentity::file(&authority)? == self.authority,
            "lease authority identity changed"
        );
        let file = locks.open_with(&self.file_name, &options)?.into_std();
        ensure!(
            FileIdentity::file(&file)? == self.file_identity
                && FileIdentity::file(&self.lock.file)? == self.file_identity,
            "lease lock file identity changed"
        );
        let registered: FileIdentity = serde_json::from_slice(&read_nofollow(
            &store,
            &format!(".rs-infra-platform-lease-{}", self.file_name),
        )?)?;
        ensure!(registered == self.file_identity, "lease registration identity mismatch");
        Ok(())
    }

    /// Confirms that a held, unchanged lease belongs to this exact run.
    pub(crate) fn require_run(&self, id: &str) -> Result<()> {
        ensure!(self.key == format!("run:{id}"), "incorrect run lease key");
        self.validate()
    }

    /// Acquires a project/environment/toolchain tuple lock for retained
    /// artifact reuse. Returns identity, path, and ownership errors from
    /// `acquire`.
    pub(crate) fn for_reuse(root: &Path, project: &Path, environment: &str, toolchain: &str) -> Result<Self> {
        let project = project.canonicalize().context("resolve reuse project")?;
        let key = serde_json::to_string(&(&project, environment, toolchain))?;
        let mut lease = Self::acquire(root, &format!("reuse:{key}"))?;
        lease.reuse_project = Some(project);
        Ok(lease)
    }

    /// Rejects generic, foreign-project, or replaced leases before retained
    /// input mutation.
    pub(crate) fn require_reuse(&self, project: &Path) -> Result<()> {
        ensure!(
            self.reuse_project.as_deref() == Some(project),
            "retained synchronization requires the project/environment/toolchain lease"
        );
        self.validate()
    }
}

fn register_identity<T: Serialize + for<'de> Deserialize<'de> + PartialEq>(
    directory: &Dir,
    name: &str,
    identity: &T,
) -> Result<()> {
    register_identity_with(directory, name, identity, |file, bytes| {
        file.write_all(bytes)?;
        file.sync_all()
    })
}

fn register_identity_with<T, W>(directory: &Dir, name: &str, identity: &T, writer: W) -> Result<()>
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq,
    W: FnOnce(&mut cap_std::fs::File, &[u8]) -> std::io::Result<()>,
{
    match read_nofollow(directory, name) {
        Ok(bytes) => {
            let registered: T = serde_json::from_slice(&bytes)?;
            ensure!(registered == *identity, "registered lease filesystem identity changed");
            return Ok(());
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    let bytes = serde_json::to_vec(identity)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = format!(
        ".rs-infra-platform-registration-{}-{stamp:x}-{:x}.tmp",
        std::process::id(),
        REGISTRATION_NONCE.fetch_add(1, Ordering::Relaxed)
    );
    let mut file = directory.open_with(&temporary, OpenOptions::new().write(true).create_new(true))?;
    if let Err(error) = writer(&mut file, &bytes) {
        drop(file);
        let _ = directory.remove_file(&temporary);
        return Err(error).context("write complete lease identity before publication");
    }
    file.sync_all()?;
    drop(file);
    // Hard-link publication is atomic and never replaces a prior authority/key
    // registration.
    let published = directory.hard_link(&temporary, directory, name);
    let removed = directory.remove_file(&temporary);
    match published {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("publish immutable lease identity"),
    }
    removed.context("remove published lease identity temporary file")?;
    #[cfg(unix)]
    directory.open(".")?.sync_all()?;
    let registered: T = serde_json::from_slice(&read_nofollow(directory, name)?)?;
    ensure!(registered == *identity, "registered lease filesystem identity changed");
    Ok(())
}

/// Pins a Windows directory against rename/delete while it is inspected or
/// traversed. Unix operations remain relative to open directory descriptors.
pub(crate) fn pin_directory(directory: &Dir) -> Result<Option<cap_std::fs::File>> {
    #[cfg(windows)]
    {
        use cap_fs_ext::OpenOptionsMaybeDirExt;
        use cap_std::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .maybe_dir(true)
            .share_mode(0x1 | 0x2)
            .follow(FollowSymlinks::No);
        let file = directory.open_with(".", &options)?;
        let identity = FileIdentity::file(&file.try_clone()?.into_std())?;
        ensure!(
            identity == FileIdentity::directory(directory)?,
            "directory identity changed while pinning"
        );
        Ok(Some(file))
    }
    #[cfg(not(windows))]
    {
        let _ = directory;
        Ok(None)
    }
}

/// Pins every Windows ancestor before relying on APIs that internally recover
/// ambient paths. Returns an error if an ancestor is a link or cannot be held
/// without delete sharing.
pub(crate) fn pin_ancestors(path: &Path) -> Result<Vec<cap_std::fs::File>> {
    #[cfg(windows)]
    {
        use std::path::Component;
        let absolute = std::path::absolute(path)?;
        let mut anchor = PathBuf::new();
        let mut names = Vec::new();
        for component in absolute.components() {
            match component {
                Component::Prefix(prefix) => anchor.push(prefix.as_os_str()),
                Component::RootDir => anchor.push(component.as_os_str()),
                Component::Normal(name) => names.push(name.to_owned()),
                Component::CurDir => {}
                Component::ParentDir => anyhow::bail!("unsafe Windows path ancestor"),
            }
        }
        let mut directory = Dir::open_ambient_dir(anchor, cap_std::ambient_authority())?;
        let mut pins = Vec::new();
        if let Some(pin) = pin_directory(&directory)? {
            pins.push(pin);
        }
        for name in names {
            directory = directory.open_dir_nofollow(name)?;
            if let Some(pin) = pin_directory(&directory)? {
                pins.push(pin);
            }
        }
        Ok(pins)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use super::Lease;
    use super::register_identity;
    use super::register_identity_with;
    use crate::run_store::secure_dir;

    #[test]
    fn test_exclusive_lease_releases_on_drop() {
        let root = tempfile::tempdir().expect("lease fixture");
        let first = Lease::acquire(root.path(), "environment-vm").expect("first lease");
        assert!(Lease::acquire(root.path(), "environment-vm").is_err());
        drop(first);
        Lease::acquire(root.path(), "environment-vm").expect("released lease");
    }

    #[cfg(unix)]
    #[test]
    fn test_drop_releases_lease_with_inherited_file_description() {
        let root = tempfile::tempdir().expect("inherited lease fixture");
        let first = Lease::acquire(root.path(), "environment-vm").expect("first lease");
        // A concurrent fork temporarily retains the same open file description,
        // exactly as this duplicated descriptor does, before exec/close completes.
        let inherited = first.lock.file.try_clone().expect("inherited lease descriptor");
        assert!(Lease::acquire(root.path(), "environment-vm").is_err());
        drop(first);
        let replacement = Lease::acquire(root.path(), "environment-vm")
            .expect("lease ownership ends even while a fork retains its descriptor");
        assert!(Lease::acquire(root.path(), "environment-vm").is_err());
        drop(inherited);
        assert!(Lease::acquire(root.path(), "environment-vm").is_err());
        drop(replacement);
    }

    #[cfg(unix)]
    #[test]
    fn test_replacing_lock_file_or_directory_does_not_create_second_lease() {
        let root = tempfile::tempdir().expect("replacement fixture");
        let first = Lease::acquire(root.path(), "environment-vm").expect("original lease");
        let lock = fs::read_dir(root.path().join("locks"))
            .expect("lock directory")
            .map(|entry| entry.expect("lock entry").path())
            .find(|path| path.extension().is_some_and(|extension| extension == "lock"))
            .expect("lock file");
        fs::rename(&lock, lock.with_extension("held")).expect("replace fixture lock inode");
        assert!(Lease::acquire(root.path(), "environment-vm").is_err());
        fs::rename(root.path().join("locks"), root.path().join("old-locks")).expect("replace fixture lock directory");
        fs::create_dir(root.path().join("locks")).expect("new lock directory");
        assert!(Lease::acquire(root.path(), "environment-vm").is_err());
        drop(first);
    }

    #[test]
    fn test_partial_registration_write_is_not_published_and_retry_succeeds() {
        let root = tempfile::tempdir().expect("registration write fixture");
        let directory = secure_dir(root.path(), false).expect("registration directory");
        let result = register_identity_with(&directory, "identity.json", &42u64, |file, bytes| {
            file.write_all(&bytes[..1])?;
            Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "injected full disk after partial write",
            ))
        });
        assert!(result.is_err());
        assert!(
            !root.path().join("identity.json").exists(),
            "partial identity must never occupy its permanent name"
        );
        register_identity(&directory, "identity.json", &42u64).expect("retry after failed publication");
    }

    #[test]
    fn test_concurrent_first_registration_reads_only_complete_winner() {
        let root = tempfile::tempdir().expect("concurrent registration fixture");
        let directory = secure_dir(root.path(), false).expect("registration directory");
        let first_directory = directory.try_clone().expect("first writer directory");
        let (partial_tx, partial_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let first = std::thread::spawn(move || {
            register_identity_with(&first_directory, "identity.json", &42u64, |file, bytes| {
                file.write_all(&bytes[..1])?;
                partial_tx.send(()).expect("notify partial temporary write");
                resume_rx.recv().expect("resume first writer");
                file.write_all(&bytes[1..])?;
                file.sync_all()
            })
        });
        partial_rx.recv().expect("first writer reached partial write");
        let second = register_identity(&directory, "identity.json", &42u64);
        resume_tx.send(()).expect("resume first writer after second completion");
        let first = first.join().expect("first writer thread");
        assert!(
            second.is_ok(),
            "second initializer must observe a complete identity: {second:?}"
        );
        assert!(
            first.is_ok(),
            "first initializer accepts the same complete winner: {first:?}"
        );
    }

    #[test]
    fn test_existing_identity_needs_no_temporary_write_during_full_disk() {
        let root = tempfile::tempdir().expect("full disk recovery fixture");
        let directory = secure_dir(root.path(), false).expect("registration directory");
        register_identity(&directory, "identity.json", &42u64).expect("existing complete identity");
        let mut invoked = false;
        let result = register_identity_with(&directory, "identity.json", &42u64, |_file, _bytes| {
            invoked = true;
            Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "injected full disk",
            ))
        });
        assert!(
            result.is_ok(),
            "an existing matching identity must remain usable on a full disk: {result:?}"
        );
        assert!(!invoked, "existing registration must not invoke the temporary writer");
    }
}
