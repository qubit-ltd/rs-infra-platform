// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Stable source snapshots with recursive Cargo path dependencies and file
//! manifests.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::fs;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::lease::pin_ancestors;
use crate::lease::pin_directory;
use crate::run_store::read_nofollow;
use crate::run_store::secure_dir;

const SNAPSHOT_MARKER: &str = ".rs-infra-platform-snapshot";

/// One input whose relative location and content are recorded in a snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SnapshotEntry {
    pub(crate) relative_path: PathBuf,
    pub(crate) digest: String,
    pub(crate) symlink_target: Option<PathBuf>,
    pub(crate) executable: bool,
}

/// An isolated source tree preserving the shared layout of all local
/// dependencies.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Snapshot {
    pub(crate) root: PathBuf,
    pub(crate) project_relative: PathBuf,
    pub(crate) entries: Vec<SnapshotEntry>,
    pub(crate) digest: String,
    owner: String,
}

impl Snapshot {
    /// Updates only previously manifested inputs while preserving artifacts and
    /// unknown files. Requires a matching reuse lease and marked snapshot.
    /// Unsafe manifests, new layout, unknown-file collisions, or changing
    /// sources return errors without touching originals.
    pub(crate) fn synchronize(&self, project: &Path, data: &[PathBuf], lease: &crate::lease::Lease) -> Result<Self> {
        let project = project.canonicalize()?;
        lease.require_reuse(&project)?;
        let _ancestor_pins = pin_ancestors(&self.root)?;
        let output = secure_dir(&self.root, false)?;
        let _output_pin = pin_directory(&output)?;
        ensure!(
            read_nofollow(&output, SNAPSHOT_MARKER)? == self.owner.as_bytes(),
            "snapshot ownership marker mismatch"
        );
        let roots = discover_roots(&project)?;
        let snapshot_root = self.root.canonicalize().context("resolve retained snapshot root")?;
        ensure!(
            roots
                .iter()
                .all(|root| !snapshot_root.starts_with(root) && !root.starts_with(&snapshot_root)),
            "retained snapshot overlaps original workspace"
        );
        let common = common_ancestor(&roots)?;
        ensure!(
            project.strip_prefix(&common)? == self.project_relative,
            "dependency layout changed; create a new snapshot"
        );
        let before = inputs(&roots, &project, &common, data)?;
        let source_before = manifest(&before)?;
        let entries = relocated_manifest(&source_before, &roots, &common)?;
        let old: BTreeSet<_> = self.entries.iter().map(|entry| entry.relative_path.clone()).collect();
        for path in old.iter().chain(before.keys()) {
            ensure!(
                safe_relative(path) && !excluded(path) && path != Path::new(SNAPSHOT_MARKER),
                "unsafe retained manifest input"
            );
        }
        // Check all unknown collisions before removing any managed input.
        for relative in before.keys().filter(|path| !old.contains(*path)) {
            match output.symlink_metadata(relative) {
                Ok(_) => bail!(
                    "new input collides with an unknown retained file: {}",
                    relative.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        for relative in &old {
            lease.validate()?;
            let parent = relative_parent(&output, relative, false)?;
            let name = relative.file_name().context("manifest input has no file name")?;
            match parent.remove_file(name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        for ((_, source), entry) in before.iter().zip(&entries) {
            lease.validate()?;
            copy_input(source, &output, entry)?;
        }
        let after = inputs(&roots, &project, &common, data)?;
        let source_after = manifest(&after)?;
        ensure!(
            discover_roots(&project)? == roots
                && source_after == source_before
                && relocated_manifest(&source_after, &roots, &common)? == entries
                && manifest_in_output(&output, &entries)? == entries,
            "source changed during retained synchronization"
        );
        let digest = hex_digest(&serde_json::to_vec(&entries)?);
        Ok(Self {
            root: self.root.clone(),
            project_relative: self.project_relative.clone(),
            entries,
            digest,
            owner: self.owner.clone(),
        })
    }
    /// Copies current nonignored sources and explicitly declared
    /// project-relative data. Cargo is never invoked against the original
    /// workspace. Unsafe/overlapping paths, `.git`/`target` data inputs, or
    /// inputs that keep changing after three attempts return errors.
    /// Absolute Cargo dependency paths are rejected. Native absolute symlinks
    /// are relocated only when their targets are captured inputs/directories;
    /// all output links must resolve entirely within the snapshot manifest.
    pub(crate) fn create_with_data(project: &Path, destination: &Path, data: &[PathBuf]) -> Result<Self> {
        let project = project.canonicalize().context("resolve source project")?;
        let roots = discover_roots(&project)?;
        let common = common_ancestor(&roots)?;
        let destination = std::path::absolute(destination)?;
        let parent = destination.parent().context("snapshot destination has no parent")?;
        secure_dir(parent, false)?;
        let destination = parent
            .canonicalize()
            .context("resolve snapshot parent")?
            .join(destination.file_name().context("snapshot destination has no name")?);
        ensure!(
            roots
                .iter()
                .all(|root| !destination.starts_with(root) && !root.starts_with(&destination)),
            "snapshot destination overlaps source workspace"
        );
        let parent = destination.parent().context("snapshot destination has no parent")?;
        let _ancestor_pins = pin_ancestors(parent)?;
        let parent_handle = secure_dir(parent, false)?;
        let name = destination.file_name().context("snapshot destination has no name")?;
        parent_handle
            .create_dir(name)
            .context("snapshot destination must be a new directory")?;
        let output = parent_handle.open_dir_nofollow(name)?;
        let _output_pin = pin_directory(&output)?;
        let owner = format!(
            "rs-infra-platform-snapshot-v1-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        );
        use std::io::Write;
        let mut marker = output.open_with(
            SNAPSHOT_MARKER,
            cap_std::fs::OpenOptions::new().write(true).create_new(true),
        )?;
        marker.write_all(owner.as_bytes())?;
        marker.sync_all()?;
        for _attempt in 0..3 {
            let before = inputs(&roots, &project, &common, data)?;
            let source_before = manifest(&before)?;
            let entries_before = relocated_manifest(&source_before, &roots, &common)?;
            for ((_, source), entry) in before.iter().zip(&entries_before) {
                copy_input(source, &output, entry)?;
            }
            let after = inputs(&roots, &project, &common, data)?;
            ensure!(
                discover_roots(&project)? == roots,
                "dependency graph changed during snapshot creation"
            );
            let source_after = manifest(&after)?;
            let entries_after = relocated_manifest(&source_after, &roots, &common)?;
            let copied = manifest_in_output(&output, &entries_before)?;
            if source_before == source_after && entries_before == entries_after && copied == entries_before {
                let digest = hex_digest(&serde_json::to_vec(&entries_before)?);
                return Ok(Self {
                    root: destination,
                    project_relative: project.strip_prefix(common)?.to_owned(),
                    entries: entries_before,
                    digest,
                    owner,
                });
            }
            // Only entries written by this attempt are removed; unrelated files are never
            // guessed.
            for entry in entries_before {
                output
                    .remove_file(&entry.relative_path)
                    .with_context(|| format!("remove unstable snapshot input {}", entry.relative_path.display()))?;
            }
        }
        bail!("source inputs changed during all three snapshot attempts")
    }
}

fn discover_roots(project: &Path) -> Result<Vec<PathBuf>> {
    ensure!(project.join("Cargo.toml").is_file(), "source project has no Cargo.toml");
    let mut visited = BTreeSet::new();
    let mut queue = vec![project.to_owned()];
    // Cargo workspace inheritance may refer to dependencies declared in an ancestor
    // manifest.
    for parent in project.ancestors().skip(1) {
        let manifest = parent.join("Cargo.toml");
        if manifest.is_file() {
            let value: toml::Value = toml::from_str(&fs::read_to_string(&manifest)?)?;
            if value.get("workspace").is_some() {
                queue.push(parent.to_owned());
                break;
            }
        }
    }
    while let Some(root) = queue.pop() {
        let root = root.canonicalize().context("resolve Cargo path dependency")?;
        if !visited.insert(root.clone()) {
            continue;
        }
        let path = root.join("Cargo.toml");
        let value: toml::Value = toml::from_str(
            &fs::read_to_string(&path).with_context(|| format!("read dependency manifest {}", path.display()))?,
        )?;
        collect_dependency_paths(&value, &root, &mut queue)?;
        if let Some(workspace) = value.get("workspace") {
            for path in workspace
                .get("default-members")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(toml::Value::as_str)
            {
                validate_dependency_path(path)?;
            }
            let excluded: Vec<_> = workspace
                .get("exclude")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(toml::Value::as_str)
                .map(glob::Pattern::new)
                .collect::<std::result::Result<_, _>>()?;
            for member in workspace
                .get("members")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(toml::Value::as_str)
            {
                validate_dependency_path(member)?;
                let pattern = root.join(member);
                for member in glob::glob(pattern.to_str().context("workspace member glob is not UTF8")?)? {
                    let member = member?;
                    let relative = member.strip_prefix(&root)?;
                    if excluded.iter().any(|pattern| pattern.matches_path(relative)) {
                        continue;
                    }
                    ensure!(
                        member.join("Cargo.toml").is_file(),
                        "workspace member has no Cargo.toml: {}",
                        member.display()
                    );
                    queue.push(member);
                }
            }
        }
        if let Some(workspace) = value
            .get("package")
            .and_then(|value| value.get("workspace"))
            .and_then(toml::Value::as_str)
        {
            validate_dependency_path(workspace)?;
            queue.push(root.join(workspace));
        }
    }
    // A workspace root already contains its member crates; avoid copying nested
    // trees twice.
    let all: Vec<_> = visited.into_iter().collect();
    Ok(all
        .iter()
        .filter(|root| !all.iter().any(|other| other != *root && root.starts_with(other)))
        .cloned()
        .collect())
}

fn collect_dependency_paths(value: &toml::Value, root: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    let Some(table) = value.as_table() else {
        return Ok(());
    };
    for (key, value) in table {
        if matches!(
            key.as_str(),
            "dependencies" | "dev-dependencies" | "build-dependencies" | "replace"
        ) {
            if let Some(dependencies) = value.as_table() {
                for dependency in dependencies.values() {
                    if let Some(path) = dependency.get("path").and_then(toml::Value::as_str) {
                        validate_dependency_path(path)?;
                        output.push(root.join(path));
                    }
                }
            }
        } else if key == "patch" {
            if let Some(registries) = value.as_table() {
                for dependencies in registries.values().filter_map(toml::Value::as_table) {
                    for dependency in dependencies.values() {
                        if let Some(path) = dependency.get("path").and_then(toml::Value::as_str) {
                            validate_dependency_path(path)?;
                            output.push(root.join(path));
                        }
                    }
                }
            }
        } else {
            collect_dependency_paths(value, root, output)?;
        }
    }
    Ok(())
}

fn validate_dependency_path(path: &str) -> Result<()> {
    ensure!(
        portable_relative_path(Path::new(path)),
        "absolute Cargo path dependency or incompatible path separators are unsupported: {path}"
    );
    Ok(())
}

fn validate_symlink_targets(entries: &[SnapshotEntry]) -> Result<()> {
    let paths: BTreeSet<_> = entries.iter().map(|entry| entry.relative_path.as_path()).collect();
    let links: BTreeMap<_, _> = entries
        .iter()
        .filter_map(|entry| {
            entry
                .symlink_target
                .as_deref()
                .map(|target| (entry.relative_path.as_path(), target))
        })
        .collect();
    let directories: BTreeSet<_> = entries
        .iter()
        .flat_map(|entry| entry.relative_path.parent().into_iter().flat_map(Path::ancestors))
        .collect();
    for entry in entries.iter().filter(|entry| entry.symlink_target.is_some()) {
        let target = entry.symlink_target.as_deref().context("missing symlink target")?;
        let mut pending = symlink_components(target, &entry.relative_path)?;
        let mut resolved = entry
            .relative_path
            .parent()
            .context("symlink has no parent")?
            .to_owned();
        let mut traversed = 0;
        while let Some(component) = pending.pop_front() {
            match Path::new(&component)
                .components()
                .next()
                .context("empty symlink component")?
            {
                Component::CurDir => {}
                Component::ParentDir => ensure!(
                    resolved.pop(),
                    "symlink escapes snapshot: {}",
                    entry.relative_path.display()
                ),
                Component::Normal(name) => {
                    resolved.push(name);
                    if let Some(target) = links.get(resolved.as_path()) {
                        traversed += 1;
                        ensure!(
                            traversed <= 40,
                            "symlink cycle or chain exceeds 40 links: {}",
                            entry.relative_path.display()
                        );
                        let mut expansion = symlink_components(target, &entry.relative_path)?;
                        expansion.append(&mut pending);
                        pending = expansion;
                        resolved.pop();
                    } else if !pending.is_empty() {
                        ensure!(
                            directories.contains(resolved.as_path()),
                            "symlink target directory is not included in snapshot: {}",
                            entry.relative_path.display()
                        );
                    }
                }
                Component::RootDir | Component::Prefix(_) => bail!(
                    "absolute symlink target is unsupported: {}",
                    entry.relative_path.display()
                ),
            }
        }
        ensure!(
            paths.contains(resolved.as_path()) || directories.contains(resolved.as_path()),
            "symlink target is not included in snapshot: {}",
            entry.relative_path.display()
        );
    }
    Ok(())
}

fn symlink_components(target: &Path, link: &Path) -> Result<VecDeque<std::ffi::OsString>> {
    ensure!(
        !target.as_os_str().is_empty() && portable_relative_path(target),
        "absolute symlink target or incompatible path separators are unsupported: {} -> {}",
        link.display(),
        target.display()
    );
    Ok(target
        .components()
        .map(|component| component.as_os_str().to_owned())
        .collect())
}

fn portable_relative_path(path: &Path) -> bool {
    let raw = path.to_string_lossy();
    let foreign_absolute = raw.starts_with('\\') || raw.as_bytes().get(1) == Some(&b':');
    let foreign_separator = cfg!(not(windows)) && raw.contains('\\');
    !foreign_absolute
        && !foreign_separator
        && path
            .components()
            .all(|component| !matches!(component, Component::RootDir | Component::Prefix(_)))
}

fn common_ancestor(roots: &[PathBuf]) -> Result<PathBuf> {
    let mut common = roots.first().context("no source roots")?.clone();
    while !roots.iter().all(|root| root.starts_with(&common)) {
        ensure!(common.pop(), "path dependencies do not share a filesystem root");
    }
    Ok(common)
}

fn inputs(roots: &[PathBuf], project: &Path, common: &Path, data: &[PathBuf]) -> Result<BTreeMap<PathBuf, PathBuf>> {
    let mut result = BTreeMap::new();
    for root in roots {
        let git = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
                "--",
                ".",
            ])
            .output();
        if let Ok(output) = git
            && output.status.success()
        {
            for bytes in output.stdout.split(|byte| *byte == 0).filter(|bytes| !bytes.is_empty()) {
                #[cfg(unix)]
                let relative = {
                    use std::os::unix::ffi::OsStrExt;
                    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
                };
                #[cfg(not(unix))]
                let relative = PathBuf::from(
                    std::str::from_utf8(bytes).context("non-UTF8 Git path cannot be represented on this host")?,
                );
                if excluded(&relative) {
                    continue;
                }
                let source = root.join(&relative);
                match fs::symlink_metadata(&source) {
                    Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                        result.insert(source.strip_prefix(common)?.to_owned(), source);
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        } else {
            collect_tree(root, common, &mut result)?;
        }
    }
    for relative in data {
        ensure!(
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
                && !excluded(relative),
            "explicit data must be a safe project-relative input: {}",
            relative.display()
        );
        collect_tree(&project.join(relative), common, &mut result)?;
    }
    Ok(result)
}

fn excluded(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::Normal(name) if name == ".git" || name == "target"))
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn relative_parent(root: &Dir, path: &Path, create: bool) -> Result<Dir> {
    ensure!(safe_relative(path), "unsafe snapshot input path");
    let mut parent = root.try_clone()?;
    for component in path.parent().context("input has no parent")?.components() {
        let Component::Normal(name) = component else {
            bail!("unsafe snapshot parent");
        };
        if create {
            match parent.create_dir(name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        parent = parent.open_dir_nofollow(name)?;
    }
    Ok(parent)
}

fn collect_tree(source: &Path, common: &Path, output: &mut BTreeMap<PathBuf, PathBuf>) -> Result<()> {
    let relative = source.strip_prefix(common)?;
    if excluded(relative) {
        return Ok(());
    }
    let parent = secure_dir(source.parent().context("source path has no parent")?, false)?;
    let name = source.file_name().context("source path has no name")?;
    let metadata = parent.symlink_metadata(name)?;
    if metadata.is_file() || metadata.file_type().is_symlink() {
        output.insert(relative.to_owned(), source.to_owned());
    } else if metadata.is_dir() {
        let directory = parent.open_dir_nofollow(name)?;
        for entry in directory.entries()? {
            collect_tree(&source.join(entry?.file_name()), common, output)?;
        }
    } else {
        bail!("unsupported special source input: {}", source.display());
    }
    Ok(())
}

fn manifest(inputs: &BTreeMap<PathBuf, PathBuf>) -> Result<Vec<SnapshotEntry>> {
    inputs
        .iter()
        .map(|(relative, source)| {
            let parent = secure_dir(source.parent().context("source has no parent")?, false)?;
            let name = source
                .file_name()
                .context("source has no name")?
                .to_str()
                .context("source file name is not UTF8")?;
            describe(&parent, name, relative)
        })
        .collect()
}

fn relocated_manifest(source: &[SnapshotEntry], roots: &[PathBuf], common: &Path) -> Result<Vec<SnapshotEntry>> {
    let captured: BTreeSet<_> = source
        .iter()
        .flat_map(|entry| entry.relative_path.ancestors().map(Path::to_owned))
        .collect();
    let mut output = source.to_vec();
    for entry in &mut output {
        let Some(target) = entry.symlink_target.as_deref().filter(|target| target.is_absolute()) else {
            continue;
        };
        let resolved = target.canonicalize().with_context(|| {
            format!(
                "cannot resolve absolute symlink target (missing or cyclic): {}",
                entry.relative_path.display()
            )
        })?;
        ensure!(
            roots.iter().any(|root| resolved.starts_with(root)),
            "external absolute symlink target is not captured: {} -> {}",
            entry.relative_path.display(),
            target.display()
        );
        let mapped = resolved
            .strip_prefix(common)
            .context("absolute symlink target is outside the captured source layout")?;
        ensure!(
            captured.contains(mapped),
            "absolute symlink target is not included in snapshot: {} -> {}",
            entry.relative_path.display(),
            target.display()
        );
        let parent = entry.relative_path.parent().context("symlink input has no parent")?;
        let relative = relative_link_target(parent, mapped);
        entry.digest = hex_digest(&serde_json::to_vec(&relative)?);
        entry.symlink_target = Some(relative);
    }
    validate_symlink_targets(&output)?;
    Ok(output)
}

fn relative_link_target(parent: &Path, target: &Path) -> PathBuf {
    let parent: Vec<_> = parent.components().collect();
    let target: Vec<_> = target.components().collect();
    let shared = parent
        .iter()
        .zip(&target)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in &parent[shared..] {
        relative.push("..");
    }
    for component in &target[shared..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    relative
}

fn describe(parent: &Dir, name: &str, relative: &Path) -> Result<SnapshotEntry> {
    let metadata = parent.symlink_metadata(name)?;
    let (digest, target) = if metadata.file_type().is_symlink() {
        let target = parent.read_link_contents(name)?;
        (hex_digest(&serde_json::to_vec(&target)?), Some(target))
    } else {
        ensure!(metadata.is_file(), "snapshot input changed type");
        (hex_digest(&read_nofollow(parent, name)?), None)
    };
    #[cfg(unix)]
    let executable = {
        use cap_std::fs::MetadataExt;
        metadata.mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    Ok(SnapshotEntry {
        relative_path: relative.to_owned(),
        digest,
        symlink_target: target,
        executable,
    })
}

fn copy_input(source: &Path, output: &Dir, entry: &SnapshotEntry) -> Result<()> {
    let relative = &entry.relative_path;
    let parent = secure_dir(source.parent().context("source has no parent")?, false)?;
    let name = source
        .file_name()
        .context("source has no name")?
        .to_str()
        .context("source file name is not UTF8")?;
    let metadata = parent.symlink_metadata(name)?;
    ensure!(
        metadata.file_type().is_symlink() == entry.symlink_target.is_some(),
        "source input changed type during snapshot copying"
    );
    let output = relative_parent(output, relative, true)?;
    let output_name = relative.file_name().context("snapshot input has no name")?;
    if metadata.file_type().is_symlink() {
        let target = entry
            .symlink_target
            .as_deref()
            .context("mapped symlink target is missing")?;
        #[cfg(unix)]
        output.symlink_contents(target, output_name)?;
        #[cfg(windows)]
        if fs::metadata(source).map(|metadata| metadata.is_dir()).unwrap_or(false) {
            output.symlink_dir(target, output_name)?;
        } else {
            output.symlink_file(target, output_name)?;
        }
        #[cfg(not(any(unix, windows)))]
        bail!("symbolic link snapshots are unsupported on this host");
    } else {
        let bytes = read_nofollow(&parent, name)?;
        let mut options = cap_std::fs::OpenOptions::new();
        use cap_fs_ext::FollowSymlinks;
        use cap_fs_ext::OpenOptionsFollowExt;
        options.write(true).create_new(true).follow(FollowSymlinks::No);
        use std::io::Write;
        let mut file = output.open_with(output_name, &options)?;
        file.write_all(&bytes)?;
        file.set_permissions(metadata.permissions())?;
    }
    Ok(())
}

fn manifest_in_output(output: &Dir, entries: &[SnapshotEntry]) -> Result<Vec<SnapshotEntry>> {
    entries
        .iter()
        .map(|entry| {
            let parent = relative_parent(output, &entry.relative_path, false)?;
            let name = entry
                .relative_path
                .file_name()
                .and_then(|name| name.to_str())
                .context("output name is not UTF8")?;
            describe(&parent, name, &entry.relative_path)
        })
        .collect()
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::process::Command;
    use std::process::Stdio;

    use super::Snapshot;

    fn fixture_crate(root: &std::path::Path, name: &str, dependency: &str) {
        fs::create_dir_all(root.join("src")).expect("crate directories");
        fs::write(
            root.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{dependency}"),
        )
        .expect("manifest");
        fs::write(root.join("src/lib.rs"), format!("// {name}\n")).expect("source");
    }

    #[test]
    fn test_absolute_path_dependencies_are_rejected_in_every_manifest_section() {
        let fixture = tempfile::tempdir().expect("absolute dependency sections fixture");
        let project = fixture.path().join("project");
        let dependency = fixture.path().join("dependency");
        fixture_crate(&dependency, "dependency", "");
        for (index, section) in [
            "dependencies",
            "dev-dependencies",
            "build-dependencies",
            "target.'cfg(unix)'.dependencies",
            "workspace.dependencies",
            "patch.crates-io",
            "replace",
        ]
        .iter()
        .enumerate()
        {
            let key = if *section == "replace" {
                "\"dependency:0.1.0\""
            } else {
                "dependency"
            };
            fixture_crate(
                &project,
                "project",
                &format!("[{section}]\n{key} = {{ path = {dependency:?} }}\n"),
            );
            let result = Snapshot::create_with_data(&project, &fixture.path().join(format!("snapshot-{index}")), &[]);
            let error = result.expect_err("absolute dependency cannot preserve snapshot isolation");
            assert!(
                format!("{error:#}").contains("absolute Cargo path dependency"),
                "precise dependency rejection for {section}: {error:#}"
            );
        }
    }

    #[test]
    fn test_absolute_workspace_members_cannot_point_back_to_original_tree() {
        let fixture = tempfile::tempdir().expect("absolute workspace member fixture");
        let project = fixture.path().join("project");
        let member = project.join("member");
        fixture_crate(&member, "member", "");
        fixture_crate(&project, "project", &format!("[workspace]\nmembers = [{member:?}]\n"));
        let result = Snapshot::create_with_data(&project, &fixture.path().join("snapshot"), &[]);
        assert!(
            result.is_err(),
            "absolute workspace members cannot retain original source paths"
        );
    }

    #[test]
    fn test_foreign_windows_absolute_paths_are_rejected_before_host_interpretation() {
        for raw in [
            r"C:\external",
            "C:/external",
            r"\\server\share\external",
            r"\rooted",
            "C:drive-relative",
        ] {
            assert!(
                super::validate_dependency_path(raw).is_err(),
                "foreign dependency path: {raw}"
            );
            assert!(
                super::symlink_components(std::path::Path::new(raw), std::path::Path::new("link")).is_err(),
                "foreign symlink target: {raw}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_relative_symlink_validation_checks_complete_chain_and_manifest_membership() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().expect("symlink chain fixture");
        let project = fixture.path().join("project");
        fixture_crate(&project, "project", "");
        symlink("src/lib.rs", project.join("inner")).expect("safe inner link");
        symlink("inner", project.join("outer")).expect("safe chain");
        Snapshot::create_with_data(&project, &fixture.path().join("safe"), &[]).expect("complete internal chain");
        fs::remove_file(project.join("inner")).expect("replace generated inner link");
        symlink("outer", project.join("inner")).expect("cycle");
        assert!(
            Snapshot::create_with_data(&project, &fixture.path().join("cycle"), &[]).is_err(),
            "cyclic chains are rejected"
        );
        fs::remove_file(project.join("inner")).expect("replace generated inner link");
        symlink("missing-input", project.join("inner")).expect("missing target");
        assert!(
            Snapshot::create_with_data(&project, &fixture.path().join("missing"), &[]).is_err(),
            "targets outside the copied manifest are rejected"
        );
    }

    #[test]
    fn test_destination_inside_original_project_is_rejected_with_native_path_prefix() {
        let fixture = tempfile::tempdir().expect("destination boundary fixture");
        let project = fixture.path().join("project");
        fixture_crate(&project, "project", "");
        let destination = project.join("snapshot");
        let error = Snapshot::create_with_data(&project, &destination, &[])
            .expect_err("original project cannot contain the snapshot");
        assert!(format!("{error:#}").contains("overlaps source workspace"));
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_retained_sync_rejects_unsafe_new_links_before_changing_old_snapshot() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().expect("retained unsafe link fixture");
        let project = fixture.path().join("project");
        fixture_crate(&project, "project", "");
        let original = project.join("original-data");
        fs::write(&original, "original bytes").expect("original data");
        let snapshot =
            Snapshot::create_with_data(&project, &fixture.path().join("snapshot"), &[]).expect("initial safe snapshot");
        let external = fixture.path().join("external-data");
        fs::write(&external, "external bytes").expect("external data");
        symlink(&external, project.join("unsafe-link")).expect("new unsafe input");
        let lease = crate::lease::Lease::for_reuse(&fixture.path().join("leases"), &project, "native", "1.94.0")
            .expect("reuse tuple lease");
        let error = snapshot
            .synchronize(&project, &[], &lease)
            .expect_err("unsafe link must fail before synchronization mutates the retained tree");
        assert!(format!("{error:#}").contains("absolute symlink"));
        assert!(!snapshot.root.join("unsafe-link").exists());
        assert_eq!(
            fs::read_to_string(snapshot.root.join("original-data")).expect("old snapshot intact"),
            "original bytes"
        );
        assert_eq!(fs::read_to_string(original).expect("original intact"), "original bytes");
    }

    #[cfg(unix)]
    #[test]
    fn test_captured_root_alias_and_cross_dependency_absolute_links_are_relocated() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().expect("absolute link mapping fixture");
        let project = fixture.path().join("project");
        let dependency = fixture.path().join("dependency");
        fixture_crate(
            &project,
            "project",
            "[dependencies]\ndependency = { path = \"../dependency\" }\n",
        );
        fixture_crate(&dependency, "dependency", "");
        fs::write(dependency.join("data"), "captured dependency bytes").expect("dependency data");
        symlink(&dependency, dependency.join("dependency")).expect("self-root alias");
        symlink(dependency.join("data"), project.join("linked-data")).expect("cross-dependency absolute link");
        let snapshot = Snapshot::create_with_data(&project, &fixture.path().join("snapshot"), &[])
            .expect("relocated absolute links");
        assert_eq!(
            fs::read_link(snapshot.root.join("dependency/dependency")).expect("root alias target"),
            std::path::PathBuf::from(".")
        );
        assert_eq!(
            fs::read_link(snapshot.root.join("project/linked-data")).expect("cross-dependency target"),
            std::path::PathBuf::from("../dependency/data")
        );
        assert_eq!(
            fs::read_to_string(snapshot.root.join("project/linked-data")).expect("captured link content"),
            "captured dependency bytes"
        );
        assert!(
            snapshot.entries.len() < 12,
            "self-root alias must never be recursively traversed"
        );
        assert_eq!(
            fs::read_link(dependency.join("dependency")).expect("source alias unchanged"),
            dependency
        );
        for entry in snapshot.entries.iter().filter(|entry| entry.symlink_target.is_some()) {
            assert!(!entry.symlink_target.as_ref().expect("mapped target").is_absolute());
            assert_eq!(
                entry.digest,
                super::hex_digest(
                    &serde_json::to_vec(entry.symlink_target.as_ref().expect("mapped target")).expect("target bytes")
                )
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_retained_sync_maps_new_internal_absolute_link_without_writing_original() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().expect("retained absolute mapping fixture");
        let project = fixture.path().join("project");
        fixture_crate(&project, "project", "");
        let original = project.join("data");
        fs::write(&original, "original bytes").expect("source data");
        let snapshot =
            Snapshot::create_with_data(&project, &fixture.path().join("snapshot"), &[]).expect("initial snapshot");
        symlink(&original, project.join("absolute-link")).expect("new internal absolute link");
        let lease = crate::lease::Lease::for_reuse(&fixture.path().join("leases"), &project, "native", "1.94.0")
            .expect("reuse tuple lease");
        let updated = snapshot
            .synchronize(&project, &[], &lease)
            .expect("incremental relocated link");
        assert_eq!(
            fs::read_link(updated.root.join("absolute-link")).expect("updated relative target"),
            std::path::PathBuf::from("data")
        );
        fs::write(updated.root.join("absolute-link"), "snapshot bytes").expect("write only retained snapshot");
        assert_eq!(
            fs::read_to_string(original).expect("original unchanged"),
            "original bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_original_absolute_target_changes_remain_visible_when_mapping_is_identical() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().expect("raw source target fixture");
        let project = fixture.path().join("project");
        fixture_crate(&project, "project", "");
        fs::write(project.join("data"), "captured").expect("captured data");
        let link = project.join("link");
        symlink(project.join("data"), &link).expect("first absolute spelling");
        let roots = super::discover_roots(&project).expect("source roots");
        let common = super::common_ancestor(&roots).expect("source layout");
        let before = super::manifest(&super::inputs(&roots, &project, &common, &[]).expect("before inputs"))
            .expect("raw before manifest");
        fs::remove_file(&link).expect("replace generated source link");
        symlink(format!("{}/./data", project.display()), &link).expect("different original target spelling");
        let after = super::manifest(&super::inputs(&roots, &project, &common, &[]).expect("after inputs"))
            .expect("raw after manifest");
        assert_ne!(
            before, after,
            "raw source fingerprint must preserve original target bytes"
        );
        assert_eq!(
            super::relocated_manifest(&before, &roots, &common).expect("before mapped manifest"),
            super::relocated_manifest(&after, &roots, &common).expect("after mapped manifest")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_uncaptured_absolute_targets_and_absolute_cycles_are_rejected() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().expect("unmapped absolute link fixture");
        let project = fixture.path().join("project");
        fixture_crate(&project, "project", "");
        assert!(
            Command::new("git")
                .arg("init")
                .arg(&project)
                .output()
                .expect("initialize generated Git fixture")
                .status
                .success()
        );
        fs::write(project.join(".gitignore"), "ignored-data\n").expect("ignored input rule");
        fs::write(project.join("ignored-data"), "uncaptured bytes").expect("ignored source data");
        let link = project.join("link");
        symlink(project.join("ignored-data"), &link).expect("uncaptured absolute target");
        let error = Snapshot::create_with_data(&project, &fixture.path().join("uncaptured"), &[])
            .expect_err("ignored target is not copied by dereference");
        assert!(format!("{error:#}").contains("not included in snapshot"));
        fs::remove_file(&link).expect("replace generated link");
        symlink(&link, &link).expect("absolute self cycle");
        let error = Snapshot::create_with_data(&project, &fixture.path().join("cyclic"), &[])
            .expect_err("absolute cycle must be rejected");
        assert!(format!("{error:#}").contains("cyclic"));
    }

    #[test]
    fn test_recursive_dependencies_preserve_layout_and_original_inputs() {
        let fixture = tempfile::tempdir().expect("snapshot fixture");
        let project = fixture.path().join("project");
        fixture_crate(
            &project,
            "project",
            "[dependencies]\ndependency = { path = \"../dependency\" }\n",
        );
        fixture_crate(
            &fixture.path().join("dependency"),
            "dependency",
            "[dependencies]\ntransitive = { path = \"../transitive\" }\n",
        );
        fixture_crate(&fixture.path().join("transitive"), "transitive", "");
        fs::create_dir_all(project.join("target")).expect("original target");
        fs::write(project.join("target/keep"), "original target").expect("target fixture");
        fs::write(project.join("untracked.rs"), "new input").expect("untracked fixture");
        let destination = fixture.path().join("snapshot");
        let snapshot = Snapshot::create_with_data(&project, &destination, &[]).expect("create snapshot");
        assert_eq!(
            fs::read_to_string(snapshot.root.join(&snapshot.project_relative).join("untracked.rs"))
                .expect("copied untracked"),
            "new input"
        );
        assert_eq!(
            fs::read_to_string(destination.join("transitive/src/lib.rs")).expect("transitive copy"),
            "// transitive\n"
        );
        assert!(!destination.join("project/target").exists());
        assert!(!project.join("Cargo.lock").exists());
        assert_eq!(
            fs::read_to_string(project.join("target/keep")).expect("untouched target"),
            "original target"
        );
        assert!(!snapshot.digest.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_git_modified_untracked_ignored_data_and_symlink_manifest() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().expect("git snapshot fixture");
        let project = fixture.path().join("project");
        fixture_crate(&project, "project", "");
        assert!(
            Command::new("git")
                .arg("init")
                .arg(&project)
                .output()
                .expect("init fixture Git repository")
                .status
                .success()
        );
        let mut hash = Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("write fixture tracked object");
        hash.stdin
            .take()
            .expect("object stdin")
            .write_all(b"// committed\n")
            .expect("object bytes");
        let hash = hash.wait_with_output().expect("fixture object hash");
        let hash = String::from_utf8(hash.stdout).expect("ASCII object hash");
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&project)
                .args([
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    "100644",
                    hash.trim(),
                    "src/lib.rs"
                ])
                .status()
                .expect("track fixture source")
                .success()
        );
        fs::write(project.join("src/lib.rs"), "// modified\n").expect("modified tracked source");
        fs::write(project.join(".gitignore"), "ignored/\n").expect("ignore rules");
        fs::create_dir(project.join("ignored")).expect("ignored directory");
        fs::write(project.join("ignored/data.bin"), "explicit data").expect("ignored fixture");
        fs::write(project.join("fresh.txt"), "untracked").expect("untracked fixture");
        symlink("src/lib.rs", project.join("source-link")).expect("source link");
        let snapshot =
            Snapshot::create_with_data(&project, &fixture.path().join("snapshot"), &["ignored/data.bin".into()])
                .expect("snapshot with explicit ignored data");
        assert_eq!(
            fs::read_to_string(snapshot.root.join("src/lib.rs")).expect("modified copy"),
            "// modified\n"
        );
        assert_eq!(
            fs::read_to_string(snapshot.root.join("ignored/data.bin")).expect("explicit data copy"),
            "explicit data"
        );
        assert_eq!(
            fs::read_link(snapshot.root.join("source-link")).expect("preserved link"),
            std::path::PathBuf::from("src/lib.rs")
        );
        assert!(!snapshot.root.join(".git").exists());
        assert_eq!(snapshot.entries.len(), 6);
        assert!(snapshot.entries.iter().all(|entry| !entry.digest.is_empty()));
        assert_eq!(
            fs::read_to_string(project.join("src/lib.rs")).expect("original modified source"),
            "// modified\n"
        );
        assert!(!project.join("Cargo.lock").exists());
    }

    #[test]
    fn test_workspace_member_globs_discover_external_dependency_graph() {
        let fixture = tempfile::tempdir().expect("workspace fixture");
        let workspace = fixture.path().join("workspace");
        let main = workspace.join("crates/main");
        fixture_crate(&main, "main", "");
        fixture_crate(
            &workspace.join("crates/other"),
            "other",
            "[dependencies]\nexternal = { path = \"../../../external\" }\n",
        );
        fixture_crate(&fixture.path().join("external"), "external", "");
        fs::write(
            workspace.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"3\"\n",
        )
        .expect("workspace manifest");
        let snapshot =
            Snapshot::create_with_data(&main, &fixture.path().join("snapshot"), &[]).expect("workspace snapshot");
        assert!(snapshot.root.join("external/Cargo.toml").is_file());
        assert_eq!(
            snapshot.project_relative,
            std::path::PathBuf::from("workspace/crates/main")
        );
    }

    #[test]
    fn test_retained_sync_deletes_only_old_manifest_inputs_and_keeps_artifacts() {
        let fixture = tempfile::tempdir().expect("retained sync fixture");
        let project = fixture.path().join("project");
        fixture_crate(&project, "project", "");
        fs::write(project.join("removed.txt"), "old input").expect("old input");
        let snapshot =
            Snapshot::create_with_data(&project, &fixture.path().join("snapshot"), &[]).expect("initial snapshot");
        fs::create_dir(snapshot.root.join("target")).expect("artifact directory");
        fs::write(snapshot.root.join("target/artifact"), "cached").expect("artifact cache");
        fs::write(snapshot.root.join("unknown.txt"), "external file").expect("unknown file");
        fs::remove_file(project.join("removed.txt")).expect("remove disposable fixture input");
        fs::write(project.join("fresh.txt"), "new input").expect("fresh input");
        let lease = crate::lease::Lease::for_reuse(&fixture.path().join("leases"), &project, "native-linux", "1.94.0")
            .expect("reuse tuple lease");
        let updated = snapshot
            .synchronize(&project, &[], &lease)
            .expect("incremental synchronization");
        assert!(!updated.root.join("removed.txt").exists());
        assert_eq!(
            fs::read_to_string(updated.root.join("fresh.txt")).expect("fresh copy"),
            "new input"
        );
        assert_eq!(
            fs::read_to_string(updated.root.join("target/artifact")).expect("preserved cache"),
            "cached"
        );
        assert_eq!(
            fs::read_to_string(updated.root.join("unknown.txt")).expect("preserved unknown input"),
            "external file"
        );
        let generic =
            crate::lease::Lease::acquire(&fixture.path().join("leases"), "unrelated").expect("unrelated lease");
        assert!(updated.synchronize(&project, &[], &generic).is_err());
    }
}
