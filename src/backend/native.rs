// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Native execution inside the tool-owned source snapshot and build directory.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::Component;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;

use crate::backend::Backend;
use crate::executor::cancelled;
use crate::host_config::NativeConfig;
use crate::model::Capability;
use crate::model::CleanupReport;
use crate::model::EnvironmentInfo;
use crate::model::ExecutionResult;
use crate::model::Platform;
use crate::model::SessionContext;
use crate::model::SuiteSpec;
use crate::process;
use crate::process::ProcessSpec;

static PROBE_NONCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct NativeBackend {
    settings: NativeConfig,
    environment: Option<EnvironmentInfo>,
}

impl NativeBackend {
    pub(crate) fn new(settings: NativeConfig) -> Self {
        Self {
            settings,
            environment: None,
        }
    }
}

impl Backend for NativeBackend {
    fn probe(&self) -> Result<Capability> {
        match Command::new(&self.settings.cargo)
            .arg("--version")
            .env("RUSTUP_AUTO_INSTALL", "0")
            .output()
        {
            Ok(output) if output.status.success() => Ok(Capability::Ready),
            Ok(output) => Ok(Capability::Broken(format!(
                "cargo --version failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Capability::Unconfigured(format!(
                "Cargo program is unavailable: {}",
                self.settings.cargo
            ))),
            Err(error) => Ok(Capability::Broken(format!("probe Cargo: {error}"))),
        }
    }

    fn start(&mut self, context: &SessionContext) -> Result<EnvironmentInfo> {
        for path in [context.host_run_dir.join("tmp"), context.host_run_dir.join("target")] {
            crate::run_store::secure_dir(&path, true)
                .with_context(|| format!("create native artifact directory {}", path.display()))?;
        }
        let version = Command::new(&self.settings.cargo)
            .arg("--version")
            .env("RUSTUP_TOOLCHAIN", &context.toolchain)
            .env("RUSTUP_AUTO_INSTALL", "0")
            .output()
            .context("probe selected Cargo toolchain")?;
        ensure!(
            version.status.success(),
            "selected toolchain {} is unavailable: {}",
            context.toolchain,
            String::from_utf8_lossy(&version.stderr)
        );
        let compiler = Command::new("rustc")
            .arg("-vV")
            .env("RUSTUP_TOOLCHAIN", &context.toolchain)
            .env("RUSTUP_AUTO_INSTALL", "0")
            .output()
            .context("probe selected Rust compiler")?;
        ensure!(
            compiler.status.success(),
            "selected Rust compiler version probe failed: {}",
            String::from_utf8_lossy(&compiler.stderr)
        );
        let compiler = String::from_utf8(compiler.stdout).context("Rust compiler version is not UTF8")?;
        let target = compiler
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .context("Rust compiler did not report a host target triple")?
            .to_owned();
        let environment = EnvironmentInfo {
            platform: Platform::host()?,
            target,
            os_version: operating_system_version(),
            filesystem: filesystem_name(&context.host_run_dir)
                .unwrap_or_else(|error| format!("unavailable: {error:#}")),
            versions: BTreeMap::from([
                (
                    "cargo".to_owned(),
                    String::from_utf8_lossy(&version.stdout).trim().to_owned(),
                ),
                (
                    "rustc".to_owned(),
                    compiler.lines().next().unwrap_or_default().to_owned(),
                ),
                ("toolchain".to_owned(), context.toolchain.clone()),
            ]),
        };
        self.environment = Some(environment.clone());
        Ok(environment)
    }

    fn execute(&mut self, suite: &SuiteSpec, context: &SessionContext) -> Result<Vec<ExecutionResult>> {
        let owned_root = context
            .snapshot_root
            .canonicalize()
            .context("resolve owned snapshot root")?;
        let root = owned_root.join(&context.project_relative);
        let mut results = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(suite.timeout_seconds);
        probe_requirements(
            suite,
            context,
            self.environment
                .as_ref()
                .context("native environment has not started")?,
            deadline,
        )?;
        for (index, command) in suite.commands.iter().enumerate() {
            let outcome = (|| -> Result<ExecutionResult> {
                let mut argv = command.argv.clone();
                let program = argv[0].rsplit(['/', '\\']).next().unwrap_or_default();
                let cargo = program.eq_ignore_ascii_case("cargo") || program.eq_ignore_ascii_case("cargo.exe");
                if cargo {
                    let option_end = argv.iter().position(|arg| arg == "--").unwrap_or(argv.len());
                    let options = &argv[..option_end];
                    ensure!(
                        !options.iter().any(|arg| [
                            "--target-dir",
                            "--build-dir",
                            "--artifact-dir",
                            "--out-dir",
                            "--config"
                        ]
                        .iter()
                        .any(|flag| arg == flag || arg.starts_with(&format!("{flag}=")))),
                        "suite {} must not redirect Cargo output or configuration",
                        suite.id
                    );
                    for (position, argument) in options.iter().enumerate() {
                        let manifest = if argument == "--manifest-path" {
                            Some(
                                options
                                    .get(position + 1)
                                    .context("--manifest-path requires a relative path")?
                                    .as_str(),
                            )
                        } else {
                            argument.strip_prefix("--manifest-path=")
                        };
                        if let Some(manifest) = manifest {
                            let path = Path::new(manifest);
                            ensure!(
                                !path.is_absolute()
                                    && !manifest.contains('\\')
                                    && !manifest.contains(':')
                                    && !path.components().any(|part| matches!(
                                        part,
                                        Component::ParentDir | Component::Prefix(_) | Component::RootDir
                                    )),
                                "manifest path must remain relative to the snapshot"
                            );
                            let resolved = root
                                .join(&command.cwd)
                                .join(path)
                                .canonicalize()
                                .context("resolve snapshot manifest")?;
                            ensure!(resolved.starts_with(&owned_root), "manifest path escapes the snapshot");
                        }
                    }
                    if let Some(target) = &suite.target {
                        let mut actual = None;
                        for (position, argument) in options.iter().enumerate() {
                            let value = if argument == "--target" {
                                Some(
                                    options
                                        .get(position + 1)
                                        .context("--target requires a target triple")?
                                        .as_str(),
                                )
                            } else {
                                argument.strip_prefix("--target=")
                            };
                            if let Some(value) = value {
                                ensure!(actual.is_none(), "duplicate Cargo --target arguments");
                                actual = Some(value);
                                ensure!(
                                    value == target,
                                    "Cargo --target {value} differs from suite target {target}"
                                );
                            }
                        }
                        if actual.is_none()
                            && argv
                                .get(1)
                                .is_some_and(|verb| matches!(verb.as_str(), "check" | "build" | "test"))
                        {
                            argv.splice(option_end..option_end, ["--target".to_owned(), target.clone()]);
                        }
                    }
                    argv[0] = self.settings.cargo.clone();
                    let position = argv.iter().position(|arg| arg == "--").unwrap_or(argv.len());
                    argv.splice(
                        position..position,
                        [
                            "--target-dir".to_owned(),
                            context.host_run_dir.join("target").to_string_lossy().into_owned(),
                        ],
                    );
                }
                let cwd = root
                    .join(&command.cwd)
                    .canonicalize()
                    .context("resolve snapshot command cwd")?;
                ensure!(cwd.starts_with(&owned_root), "command cwd escaped snapshot");
                let mut env = BTreeMap::from([
                    (
                        OsString::from("CARGO_TARGET_DIR"),
                        context.host_run_dir.join("target").into_os_string(),
                    ),
                    (
                        OsString::from("CARGO_BUILD_BUILD_DIR"),
                        context.host_run_dir.join("target/build").into_os_string(),
                    ),
                    (OsString::from("RUSTUP_TOOLCHAIN"), OsString::from(&context.toolchain)),
                    (OsString::from("RUSTUP_AUTO_INSTALL"), OsString::from("0")),
                ]);
                for key in ["TMPDIR", "TEMP", "TMP"] {
                    env.insert(OsString::from(key), context.host_run_dir.join("tmp").into_os_string());
                }
                let log_stem = format!("command-{index}");
                process::run(
                    &ProcessSpec {
                        argv,
                        cwd,
                        env,
                        timeout: deadline.saturating_duration_since(Instant::now()),
                        stdout: context.report_dir.join(format!("{log_stem}.stdout.log")),
                        stderr: context.report_dir.join(format!("{log_stem}.stderr.log")),
                    },
                    cancelled(),
                )
            })();
            let result = outcome.map_err(|error| crate::report::with_completed_results(error, &results))?;
            let stop = result.exit_code != Some(0) || result.cancelled || result.timed_out;
            results.push(result);
            if stop {
                break;
            }
        }
        Ok(results)
    }

    fn cleanup(&mut self, _context: &SessionContext) -> Result<CleanupReport> {
        Ok(CleanupReport::default())
    }
    fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

fn probe_requirements(
    suite: &SuiteSpec,
    context: &SessionContext,
    environment: &EnvironmentInfo,
    deadline: Instant,
) -> Result<()> {
    for requirement in &suite.requirements {
        ensure!(
            [
                "native-shell",
                "writable-tmpdir",
                "ntfs",
                "msvc",
                "symlink",
                "symlinks",
                "long-path",
                "long-paths"
            ]
            .contains(&requirement.as_str()),
            "unknown native requirement: {requirement}"
        );
        if requirement == "ntfs" {
            ensure!(
                environment.filesystem.to_ascii_lowercase().starts_with("ntfs"),
                "suite requires ntfs; actual filesystem: {}",
                environment.filesystem
            );
        }
        if requirement == "msvc" {
            ensure!(
                cfg!(windows) && environment.target.ends_with("-msvc"),
                "suite requires msvc; actual native target: {}",
                environment.target
            );
        }
    }
    if suite.requirements.is_empty() {
        return Ok(());
    }
    let temporary = context.host_run_dir.join("tmp");
    let parent = crate::run_store::secure_dir(&temporary, false)?;
    let root = loop {
        let name = format!(
            "requirements-{}-{}",
            std::process::id(),
            PROBE_NONCE.fetch_add(1, Ordering::Relaxed)
        );
        match parent.create_dir(&name) {
            Ok(()) => break temporary.join(name),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("create owned requirement probe directory"),
        }
    };
    for (index, requirement) in suite.requirements.iter().enumerate() {
        let label = format!("requirement-{index}-{requirement}");
        match requirement.as_str() {
            "ntfs" => {}
            "writable-tmpdir" => {
                let source = root.join(format!("writable-{index}"));
                let destination = source.with_extension("renamed");
                fs::write(&source, b"native temporary filesystem probe").context("suite requires writable-tmpdir")?;
                fs::rename(&source, &destination).context("suite requires writable temporary rename")?;
                ensure!(
                    fs::read(&destination)? == b"native temporary filesystem probe",
                    "writable-tmpdir probe roundtrip failed"
                );
            }
            "symlink" | "symlinks" => {
                let source = root.join(format!("symlink-target-{index}"));
                let link = root.join(format!("symlink-{index}"));
                fs::write(&source, b"symlink capability probe")?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(&source, &link).context("suite requires symlinks")?;
                #[cfg(windows)]
                std::os::windows::fs::symlink_file(&source, &link)
                    .context("suite requires symlinks; enable the required Windows privilege")?;
                ensure!(
                    fs::read(&link)? == b"symlink capability probe",
                    "symlink capability probe failed"
                );
            }
            "long-path" | "long-paths" => {
                let mut long = root.join(format!("long-path-{index}"));
                for _ in 0..5 {
                    long.push("long-path-component-".repeat(4));
                }
                fs::create_dir_all(&long).context("suite requires long-paths")?;
                let file = long.join("probe.txt");
                fs::write(&file, b"long path capability probe").context("suite requires long-path file writes")?;
                ensure!(
                    fs::read(file)? == b"long path capability probe",
                    "long-path capability probe failed"
                );
            }
            "msvc" => {
                let source = root.join("msvc-probe.rs");
                fs::write(&source, "fn main() {}\n")?;
                let spec = requirement_spec(
                    context,
                    &root,
                    &label,
                    vec![
                        "rustc".to_owned(),
                        source.to_string_lossy().into_owned(),
                        "-o".to_owned(),
                        root.join("msvc-probe.exe").to_string_lossy().into_owned(),
                    ],
                    deadline,
                );
                ensure_requirement_process(process::run(&spec, cancelled())?, requirement)?;
            }
            "native-shell" => probe_native_shell(context, &root, &label, deadline)?,
            _ => unreachable!("requirements were validated before probes"),
        }
    }
    Ok(())
}

fn requirement_spec(
    context: &SessionContext,
    root: &Path,
    label: &str,
    argv: Vec<String>,
    deadline: Instant,
) -> ProcessSpec {
    let mut env = BTreeMap::from([
        (OsString::from("RUSTUP_TOOLCHAIN"), OsString::from(&context.toolchain)),
        (OsString::from("RUSTUP_AUTO_INSTALL"), OsString::from("0")),
    ]);
    for key in ["TMPDIR", "TEMP", "TMP"] {
        env.insert(OsString::from(key), context.host_run_dir.join("tmp").into_os_string());
    }
    ProcessSpec {
        argv,
        cwd: root.to_owned(),
        env,
        timeout: deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(30)),
        stdout: context.report_dir.join(format!("{label}.stdout.log")),
        stderr: context.report_dir.join(format!("{label}.stderr.log")),
    }
}

fn ensure_requirement_process(result: ExecutionResult, requirement: &str) -> Result<()> {
    ensure!(!result.cancelled, "requirement {requirement} probe canceled");
    ensure!(!result.timed_out, "requirement {requirement} probe timed out");
    ensure!(
        result.exit_code == Some(0),
        "requirement {requirement} probe failed with exit {:?}; log {}",
        result.exit_code,
        result.stderr.display()
    );
    Ok(())
}

fn probe_native_shell(context: &SessionContext, root: &Path, label: &str, deadline: Instant) -> Result<()> {
    #[cfg(unix)]
    let result = process::run(
        &requirement_spec(
            context,
            root,
            label,
            vec!["/bin/sh".to_owned(), "-c".to_owned(), "exit 0".to_owned()],
            deadline,
        ),
        cancelled(),
    )?;
    #[cfg(windows)]
    let result = {
        let system = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute())
            .context("native-shell requires an absolute SystemRoot")?;
        let argv = vec![
            system.join("System32/cmd.exe").to_string_lossy().into_owned(),
            "/u".to_owned(),
            "/d".to_owned(),
            "/s".to_owned(),
            "/c".to_owned(),
        ];
        process::run_windows_script(
            &requirement_spec(context, root, label, argv, deadline),
            std::ffi::OsStr::new("exit /b 0"),
            cancelled(),
        )?
    };
    #[cfg(any(unix, windows))]
    return ensure_requirement_process(result, "native-shell");
    #[cfg(not(any(unix, windows)))]
    anyhow::bail!("native-shell is unsupported on this host")
}

#[cfg(unix)]
fn operating_system_version() -> String {
    use std::ffi::CStr;
    let mut name = std::mem::MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: uname initializes the complete, correctly aligned utsname on success.
    if unsafe { libc::uname(name.as_mut_ptr()) } != 0 {
        return format!(
            "{}: version unavailable ({})",
            std::env::consts::OS,
            std::io::Error::last_os_error()
        );
    }
    // SAFETY: uname succeeded and POSIX guarantees NUL-terminated field arrays.
    let name = unsafe { name.assume_init() };
    // SAFETY: these field pointers belong to the initialized utsname above.
    unsafe {
        format!(
            "{} {} ({})",
            CStr::from_ptr(name.sysname.as_ptr()).to_string_lossy(),
            CStr::from_ptr(name.release.as_ptr()).to_string_lossy(),
            CStr::from_ptr(name.machine.as_ptr()).to_string_lossy()
        )
    }
}

#[cfg(windows)]
fn operating_system_version() -> String {
    let command = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|root| root.join("System32/cmd.exe"));
    if let Some(command) = command {
        if let Ok(output) = Command::new(command).args(["/d", "/c", "ver"]).output() {
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout).trim().to_owned();
            }
        }
    }
    "windows: version unavailable".to_owned()
}

#[cfg(not(any(unix, windows)))]
fn operating_system_version() -> String {
    format!("{}: version unavailable", std::env::consts::OS)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn filesystem_name(path: &Path) -> Result<String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).context("filesystem probe path contains NUL")?;
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: statfs receives a valid C path and an aligned output allocation.
    ensure!(
        unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) } == 0,
        "statfs failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: statfs returned success and initialized its output.
    let info = unsafe { info.assume_init() };
    #[cfg(target_os = "linux")]
    {
        let name = match info.f_type as u64 {
            0xEF53 => "ext2/ext3/ext4",
            0x5346544e => "ntfs",
            0x58465342 => "xfs",
            0x9123683E => "btrfs",
            0x794c7630 => "overlayfs",
            0x01021994 => "tmpfs",
            0x6969 => "nfs",
            0x65735546 => "fuse",
            0xFF534D42 => "cifs",
            0x2FC12FC1 => "zfs",
            _ => return Ok(format!("statfs type 0x{:x}", info.f_type)),
        };
        Ok(format!("{name} (statfs 0x{:x})", info.f_type))
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: Darwin statfs guarantees a NUL-terminated f_fstypename.
        Ok(unsafe { std::ffi::CStr::from_ptr(info.f_fstypename.as_ptr()) }
            .to_string_lossy()
            .into_owned())
    }
}

#[cfg(windows)]
fn filesystem_name(path: &Path) -> Result<String> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut volume = vec![0u16; 32768];
    // SAFETY: Windows receives initialized, bounded UTF-16 buffers and valid
    // lengths.
    ensure!(
        unsafe { GetVolumePathNameW(path.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) } != 0,
        "GetVolumePathNameW failed: {}",
        std::io::Error::last_os_error()
    );
    let mut filesystem = [0u16; 256];
    // SAFETY: optional unused outputs are null; filesystem has the supplied
    // capacity.
    ensure!(
        unsafe {
            GetVolumeInformationW(
                volume.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            )
        } != 0,
        "GetVolumeInformationW failed: {}",
        std::io::Error::last_os_error()
    );
    let length = filesystem
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(filesystem.len());
    Ok(String::from_utf16_lossy(&filesystem[..length]))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn filesystem_name(_path: &Path) -> Result<String> {
    Ok("unavailable on this host".to_owned())
}
