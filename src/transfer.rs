// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Validated, checksummed Windows snapshot transfer without archive extraction.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

/// One regular file or directory from the frozen host snapshot.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub(crate) path: String,
    pub(crate) directory: bool,
    pub(crate) size: u64,
    pub(crate) digest: String,
}

/// Rejects traversal, NTFS aliases, reserved names, and case-insensitive
/// collisions. Links have no transfer representation and are rejected while
/// enumerating the snapshot.
pub(crate) fn validate_manifest(entries: &[Entry]) -> Result<()> {
    ensure!(entries.len() <= 100_000, "snapshot manifest exceeds entry limit");
    let mut seen = BTreeSet::new();
    let mut files = BTreeSet::new();
    for entry in entries {
        validate_path(&entry.path)?;
        let key = entry.path.to_uppercase();
        ensure!(
            seen.insert(key.clone()),
            "Windows case-insensitive path collision: {}",
            entry.path
        );
        if !entry.directory {
            ensure!(
                entry.digest.len() == 64 && entry.digest.bytes().all(|c| c.is_ascii_hexdigit()),
                "invalid file digest"
            );
            files.insert(key);
        }
    }
    for entry in entries {
        let mut parent = Path::new(&entry.path).parent();
        while let Some(path) = parent {
            ensure!(
                !files.contains(&path.to_string_lossy().to_uppercase()),
                "file is also a parent directory"
            );
            parent = path.parent();
        }
    }
    Ok(())
}

/// Validates the protocol's portable slash-delimited relative path.
/// Returns errors for all Windows special syntax, reserved names, or traversal.
pub(crate) fn validate_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty() && path.len() <= 30_000, "invalid relative path length");
    for part in path.split('/') {
        ensure!(
            !part.is_empty() && part != "." && part != "..",
            "invalid relative path component"
        );
        ensure!(!part.ends_with([' ', '.']), "Windows trailing dot or space");
        ensure!(
            !part.chars().any(|c| c < ' ' || "<>:\\\"|?*".contains(c)),
            "Windows illegal filename character"
        );
        let stem = part.split('.').next().unwrap_or_default().to_uppercase();
        ensure!(
            !["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&stem.as_str()),
            "Windows reserved filename"
        );
        ensure!(
            !(stem.starts_with("COM") || stem.starts_with("LPT"))
                || !["1", "2", "3", "4", "5", "6", "7", "8", "9", "¹", "²", "³"].contains(&&stem[3..]),
            "Windows reserved device name"
        );
    }
    Ok(())
}

/// Enumerates only regular files/directories and hashes their actual snapshot
/// bytes. Returns errors for links, non-UTF-8 names, special files, or unsafe
/// NTFS layouts.
pub(crate) fn manifest(root: &Path) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    collect(root, root, &mut entries)?;
    validate_manifest(&entries)?;
    Ok(entries)
}

fn collect(root: &Path, directory: &Path, entries: &mut Vec<Entry>) -> Result<()> {
    for child in fs::read_dir(directory).context("read source snapshot")? {
        let child = child?;
        let path = child.path();
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "Windows transfer does not support snapshot symlinks: {}",
            path.display()
        );
        ensure!(
            metadata.is_dir() || metadata.is_file(),
            "unsupported snapshot file type"
        );
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .context("snapshot filename is not UTF-8")?
            .to_owned();
        entries.push(Entry {
            path: relative,
            directory: metadata.is_dir(),
            size: metadata.len(),
            digest: if metadata.is_file() {
                digest_file(&path)?
            } else {
                String::new()
            },
        });
        if metadata.is_dir() {
            collect(root, &path, entries)?;
        }
    }
    Ok(())
}

/// Streams a SHA-256 digest without allocating a complete source or log file.
pub(crate) fn digest_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    Ok(hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::Entry;
    use super::validate_manifest;
    fn file(path: &str) -> Entry {
        Entry {
            path: path.into(),
            directory: false,
            size: 0,
            digest: "00".repeat(32),
        }
    }
    #[test]
    fn test_windows_manifest_rejects_unsafe_names_and_aliases() {
        for name in [
            "../escape",
            "/absolute",
            "C:/file",
            "a\\b",
            "aux.txt",
            "name.",
            "file:ads",
            "space ",
        ] {
            assert!(validate_manifest(&[file(name)]).is_err(), "must reject {name}");
        }
        assert!(validate_manifest(&[file("Src/lib.rs"), file("src/LIB.rs")]).is_err());
        assert!(validate_manifest(&[file("same"), file("same")]).is_err());
    }
}
