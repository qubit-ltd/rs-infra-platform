// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Android NDK compilation and explicitly selected Emulator/device lifecycle.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
#[cfg(unix)]
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use fs2::FileExt;
use serde::Deserialize;
use serde::Serialize;

use crate::android_runner;
use crate::android_runner::RunnerContext;
use crate::backend::Backend;
use crate::executor::cancelled;
use crate::lease::Lease;
use crate::model::Capability;
use crate::model::CleanupReport;
use crate::model::EnvironmentInfo;
use crate::model::ExecutionResult;
use crate::model::Platform;
use crate::model::SessionContext;
use crate::model::SuiteSpec;
use crate::process;
use crate::process::ProcessSpec;
use crate::process_identity;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AndroidConfig {
    sdk_root: PathBuf,
    ndk_version: String,
    #[serde(default = "default_api")]
    api: u32,
    #[serde(default = "default_abi")]
    abi: String,
    serial: Option<String>,
    avd: Option<String>,
    avd_home: Option<PathBuf>,
    #[serde(default = "default_port")]
    emulator_port: u16,
    #[serde(default = "default_boot_timeout")]
    boot_timeout_seconds: u64,
    #[serde(default = "default_cargo")]
    cargo: String,
}
fn default_api() -> u32 {
    35
}
fn default_abi() -> String {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "arm64-v8a"
    } else {
        "x86_64"
    }
    .into()
}
fn default_port() -> u16 {
    5580
}
fn default_boot_timeout() -> u64 {
    180
}
fn default_cargo() -> String {
    "cargo".into()
}

pub(crate) struct AndroidBackend {
    settings: Option<AndroidConfig>,
    runner: Option<RunnerContext>,
    emulator: Option<Child>,
    lease: Option<Lease>,
    remote_created: bool,
    registration: Option<PathBuf>,
}

impl AndroidBackend {
    /// Parses only the Android machine table. Missing settings are
    /// unconfigured; unknown keys, ambiguous device selection and invalid
    /// API/ABI are errors.
    pub(crate) fn from_table(table: Option<&toml::Value>) -> Result<Self> {
        let settings = table
            .map(|table| {
                table
                    .clone()
                    .try_into::<AndroidConfig>()
                    .context("parse Android configuration")
            })
            .transpose()?;
        if let Some(settings) = &settings {
            ensure!(settings.sdk_root.is_absolute(), "android.sdk_root must be absolute");
            ensure!(
                android_runner::valid_id(&settings.ndk_version)
                    || settings
                        .ndk_version
                        .split('.')
                        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())),
                "invalid Android NDK version"
            );
            ensure!(
                settings.serial.is_some() != settings.avd.is_some(),
                "configure exactly one of android.serial (borrowed) or android.avd (owned)"
            );
            ensure!(
                settings.serial.as_deref().is_none_or(android_runner::valid_serial),
                "invalid Android serial"
            );
            ensure!(
                settings.avd.as_deref().is_none_or(|name| {
                    !name.is_empty()
                        && name.len() <= 100
                        && !name.starts_with('-')
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
                }),
                "invalid Android AVD name"
            );
            ensure!(
                settings.avd_home.as_ref().is_none_or(|path| path.is_absolute()),
                "android.avd_home must be absolute"
            );
            ensure!(
                (21..=35).contains(&settings.api),
                "Android API must be in tested NDK range 21..=35"
            );
            ensure!(
                (5554..=5682).contains(&settings.emulator_port) && settings.emulator_port % 2 == 0,
                "Android emulator_port must be an even console port 5554..=5682"
            );
            ensure!(
                (1..=3600).contains(&settings.boot_timeout_seconds),
                "Android boot timeout must be in 1..=3600 seconds"
            );
            ensure!(!settings.cargo.is_empty(), "Android cargo program missing");
            target(&settings.abi)?;
        }
        Ok(Self {
            settings,
            runner: None,
            emulator: None,
            lease: None,
            remote_created: false,
            registration: None,
        })
    }

    fn settings(&self) -> Result<&AndroidConfig> {
        self.settings.as_ref().context("Android is unconfigured")
    }

    fn ndk(&self) -> Result<PathBuf> {
        let settings = self.settings()?;
        Ok(settings.sdk_root.join("ndk").join(&settings.ndk_version))
    }
    fn llvm(&self) -> Result<PathBuf> {
        Ok(self
            .ndk()?
            .join("toolchains/llvm/prebuilt")
            .join(if cfg!(target_os = "macos") {
                "darwin-x86_64"
            } else {
                "linux-x86_64"
            }))
    }

    fn check_tools(&self) -> Result<()> {
        let settings = self.settings()?;
        ensure!(
            cfg!(any(target_os = "linux", target_os = "macos")),
            "Android host is unsupported"
        );
        let properties = fs::read_to_string(self.ndk()?.join("source.properties"))
            .context("read configured NDK source.properties")?;
        ensure!(
            properties.lines().any(|line| line
                .split_once('=')
                .is_some_and(|(key, value)| key.trim() == "Pkg.Revision" && value.trim() == settings.ndk_version)),
            "configured Android NDK version mismatch"
        );
        for path in [
            settings.sdk_root.join("platform-tools/adb"),
            self.llvm()?.join("bin/llvm-readelf"),
            self.linker(false)?,
            self.linker(true)?,
            self.llvm()?.join("bin/llvm-ar"),
        ] {
            ensure!(path.is_file(), "configured Android tool missing: {}", path.display());
        }
        ensure!(
            self.system_dir()?.is_dir(),
            "NDK API/ABI system library directory missing"
        );
        let adb = Command::new(settings.sdk_root.join("platform-tools/adb"))
            .arg("version")
            .output()
            .context("probe adb version")?;
        ensure!(adb.status.success(), "configured adb version probe failed");
        if let Some(avd) = &settings.avd {
            let emulator = settings.sdk_root.join("emulator/emulator");
            ensure!(emulator.is_file(), "configured Android emulator missing");
            for args in [["-version"].as_slice(), ["-accel-check"].as_slice()] {
                let output = Command::new(&emulator)
                    .args(args)
                    .output()
                    .context("probe Android emulator/acceleration")?;
                ensure!(
                    output.status.success(),
                    "Android emulator {:?} failed: {}",
                    args,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            let config = fs::read_to_string(self.avd_home()?.join(format!("{avd}.avd/config.ini")))
                .context("read configured AVD config.ini")?;
            let property = |key: &str| {
                config.lines().find_map(|line| {
                    let (name, value) = line.split_once('=')?;
                    (name.trim() == key).then_some(value.trim())
                })
            };
            ensure!(
                property("abi.type") == Some(settings.abi.as_str()),
                "AVD ABI does not match android.abi"
            );
            let image = property("image.sysdir.1").context("AVD image.sysdir.1 missing")?;
            let image_properties = fs::read_to_string(settings.sdk_root.join(image).join("source.properties"))
                .context("read Android image source.properties")?;
            ensure!(
                image_properties.lines().any(|line| line
                    .split_once('=')
                    .is_some_and(|(key, value)| key.trim() == "AndroidVersion.ApiLevel"
                        && value.trim().parse::<u32>().is_ok_and(|api| api >= settings.api))),
                "AVD image API is lower than Android compilation API"
            );
        }
        Ok(())
    }
    fn avd_home(&self) -> Result<PathBuf> {
        if let Some(path) = &self.settings()?.avd_home {
            return Ok(path.clone());
        }
        if let Some(path) = std::env::var_os("ANDROID_AVD_HOME") {
            return Ok(path.into());
        }
        Ok(
            PathBuf::from(std::env::var_os("HOME").context("HOME missing for Android AVD lookup")?)
                .join(".android/avd"),
        )
    }
    fn linker(&self, cxx: bool) -> Result<PathBuf> {
        let settings = self.settings()?;
        let triple = if settings.abi == "armeabi-v7a" {
            "armv7a-linux-androideabi"
        } else {
            target(&settings.abi)?
        };
        Ok(self.llvm()?.join("bin").join(format!(
            "{triple}{}-{}",
            settings.api,
            if cxx { "clang++" } else { "clang" }
        )))
    }
    fn system_dir(&self) -> Result<PathBuf> {
        let settings = self.settings()?;
        let triple = if settings.abi == "armeabi-v7a" {
            "arm-linux-androideabi"
        } else {
            target(&settings.abi)?
        };
        Ok(self
            .llvm()?
            .join("sysroot/usr/lib")
            .join(triple)
            .join(settings.api.to_string()))
    }
    fn cancel_invocations(&self) -> Result<()> {
        if !self.remote_created {
            return Ok(());
        }
        let Some(context) = &self.runner else {
            return Ok(());
        };
        let mut errors = Vec::new();
        for entry in fs::read_dir(&context.local_root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && entry.path().join("prepared").is_file() {
                let invocation = entry.file_name().to_string_lossy().into_owned();
                if let Err(error) =
                    android_runner::helper(context, "cancel", Some(&invocation), Duration::from_secs(15))
                {
                    errors.push(format!("{invocation}: {error:#}"));
                }
            }
        }
        ensure!(errors.is_empty(), "Android cancellation failed: {}", errors.join("; "));
        Ok(())
    }
}

impl AndroidBackend {
    fn build_helper(&self, runner: &RunnerContext, toolchain: &str) -> Result<PathBuf> {
        let helper_source = runner.local_root.join("supervisor.rs");
        fs::write(&helper_source, include_str!("../android_device.rs"))?;
        let helper_binary = runner.local_root.join("supervisor");
        let compiled = process::run(
            &ProcessSpec {
                argv: vec![
                    "rustc".into(),
                    "--edition=2024".into(),
                    "-O".into(),
                    "--target".into(),
                    runner.target.clone(),
                    "-C".into(),
                    format!("linker={}", self.linker(false)?.display()),
                    helper_source.to_string_lossy().into_owned(),
                    "-o".into(),
                    helper_binary.to_string_lossy().into_owned(),
                ],
                cwd: runner.local_root.clone(),
                env: BTreeMap::from([(OsString::from("RUSTUP_TOOLCHAIN"), OsString::from(toolchain))]),
                timeout: Duration::from_secs(120),
                stdout: runner.report_dir.join("android-helper-build.stdout.log"),
                stderr: runner.report_dir.join("android-helper-build.stderr.log"),
            },
            cancelled(),
        )?;
        ensure!(
            compiled.exit_code == Some(0) && !compiled.timed_out && !compiled.cancelled,
            "Android supervisor compilation failed; install Rust target {} for toolchain {}; see {}",
            runner.target,
            toolchain,
            compiled.stderr.display()
        );
        Ok(helper_binary)
    }

    fn launch_owned(&mut self) -> Result<()> {
        #[cfg(not(unix))]
        {
            bail!("Android owner requires Unix");
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let path = self
                .registration
                .as_ref()
                .context("Android registration missing")?
                .clone();
            let generation = android_runner::random_hex(16)?;
            let registration = update_registration(&path, |value| {
                value.generation = generation;
                value.guard = None;
                value.emulator = None;
                value.anchor = None;
                value.owner_state = "intent".into();
                value.owned_stop_pending = true;
            })?;
            let child = Command::new(std::env::current_exe()?)
                .args(["android-owner", "--context"])
                .arg(&path)
                .stdin(Stdio::piped())
                .stdout(File::create(
                    registration.runner.report_dir.join("android-owner.stdout.log"),
                )?)
                .stderr(File::create(
                    registration.runner.report_dir.join("android-owner.stderr.log"),
                )?)
                .process_group(0)
                .spawn()?;
            self.emulator = Some(child);
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let registered = read_registration(&path)?;
                let child = self.emulator.as_mut().context("Android owner handle missing")?;
                if registered
                    .guard
                    .as_ref()
                    .is_some_and(|identity| identity.pid == child.id())
                    && registered.owner_state == "owner_ready"
                {
                    child
                        .stdin
                        .as_mut()
                        .context("Android owner gate missing")?
                        .write_all(b"G")?;
                    return Ok(());
                }
                ensure!(!child_exited(child)?, "Android owner exited before launch permission");
                ensure!(Instant::now() < deadline, "Android owner registration timeout");
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    fn install_helper(&self) -> Result<()> {
        let path = self.registration.as_ref().context("Android registration missing")?;
        let registration = read_registration(path)?;
        let runner = self.runner.as_ref().context("Android session missing")?;
        let remote = android_runner::device_root(runner);
        // The marker exists before upload/chmod/init, so every later bootstrap phase
        // can retry.
        let verify = format!(
            "test ! -L {remote} && test ! -L {remote}/.owner && test \"$(cat {remote}/.owner)\" = {}",
            runner.token
        );
        android_runner::adb(runner, &["shell".into(), "-T".into(), verify], Duration::from_secs(10))?;
        let existing = runner.local_root.join("supervisor");
        let helper = if existing.is_file() {
            existing
        } else {
            self.build_helper(runner, &registration.toolchain)?
        };
        android_runner::adb(
            runner,
            &[
                "push".into(),
                helper.to_string_lossy().into_owned(),
                format!("{remote}/supervisor"),
            ],
            Duration::from_secs(120),
        )?;
        update_registration(path, |value| value.bootstrap = "helper_uploaded".into())?;
        android_runner::adb(
            runner,
            &["shell".into(), "-T".into(), format!("chmod 700 {remote}/supervisor")],
            Duration::from_secs(10),
        )?;
        android_runner::helper(runner, "init", None, Duration::from_secs(10))?;
        update_registration(path, |value| value.bootstrap = "ready".into())?;
        Ok(())
    }
}

impl Backend for AndroidBackend {
    fn probe(&self) -> Result<Capability> {
        if self.settings.is_none() {
            return Ok(Capability::Unconfigured(
                "Android SDK/NDK and fixed serial or AVD are not configured".into(),
            ));
        }
        match self.check_tools() {
            Ok(()) => Ok(Capability::Ready),
            Err(error) => Ok(Capability::Broken(format!("{error:#}"))),
        }
    }

    fn start(&mut self, context: &SessionContext) -> Result<EnvironmentInfo> {
        self.check_tools()?;
        let settings = self.settings()?.clone();
        let serial = settings
            .serial
            .clone()
            .unwrap_or_else(|| format!("emulator-{}", settings.emulator_port));
        let storage = context
            .host_run_dir
            .parent()
            .and_then(Path::parent)
            .context("Android run storage root unavailable")?;
        self.lease = Some(Lease::acquire(storage, &format!("android:{serial}"))?);
        let local_root = context
            .host_run_dir
            .join(format!("android-{}", android_runner::random_hex(8)?));
        fs::create_dir(&local_root)?;
        let mut system_libraries = Vec::new();
        for entry in fs::read_dir(self.system_dir()?)? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|extension| extension == "so") {
                system_libraries.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        let runner = RunnerContext {
            protocol_version: 1,
            run_id: format!("{}-{}", context.run_id.0, android_runner::random_hex(8)?),
            token: android_runner::random_hex(32)?,
            serial,
            adb: settings.sdk_root.join("platform-tools/adb"),
            readelf: self.llvm()?.join("bin/llvm-readelf"),
            ndk_libraries: self
                .system_dir()?
                .parent()
                .context("NDK library root missing")?
                .to_path_buf(),
            system_libraries,
            target: target(&settings.abi)?.to_owned(),
            local_root: local_root.clone(),
            report_dir: context.report_dir.clone(),
            project_root: context.snapshot_root.join(&context.project_relative),
            data: Vec::new(),
            timeout_seconds: 60,
        };
        self.runner = Some(runner.clone());
        let registration_path = registration_path(&context.report_dir);
        android_runner::write_context(&context.report_dir.join("android-session.json"), &runner)?;
        save_registration(
            &registration_path,
            &AndroidRegistration {
                version: 1,
                storage_root: storage.to_path_buf(),
                owner_state: "intent".into(),
                runner: runner.clone(),
                settings: settings.clone(),
                toolchain: context.toolchain.clone(),
                owned: settings.avd.is_some(),
                owned_stop_pending: settings.avd.is_some(),
                guest_cleanup_pending: false,
                bootstrap: "not_started".into(),
                generation: android_runner::random_hex(16)?,
                guard: None,
                emulator: None,
                anchor: None,
            },
        )?;
        self.registration = Some(registration_path);
        if settings.avd.is_some() {
            ensure!(
                android_runner::adb(&runner, &["get-state".into()], Duration::from_secs(5)).is_err(),
                "configured owned Emulator serial is already active; use android.serial to borrow"
            );
            self.launch_owned()?;
        }
        let deadline = Instant::now() + Duration::from_secs(settings.boot_timeout_seconds);
        loop {
            ensure!(!cancelled().load(Ordering::Relaxed), "Android startup cancelled");
            if let Some(emulator) = &mut self.emulator {
                ensure!(
                    !child_exited(emulator)?,
                    "Android Emulator exited before boot completion"
                );
            }
            let boot = android_runner::adb(
                &runner,
                &["shell".into(), "-T".into(), "getprop sys.boot_completed".into()],
                Duration::from_secs(3),
            );
            let ready = boot.is_ok_and(|bytes| String::from_utf8_lossy(&bytes).trim() == "1")
                && android_runner::adb(
                    &runner,
                    &["shell".into(), "-T".into(), "printf rs-infra-shell-ready".into()],
                    Duration::from_secs(3),
                )
                .is_ok_and(|bytes| bytes == b"rs-infra-shell-ready");
            if ready {
                break;
            }
            ensure!(Instant::now() < deadline, "Android boot/shell readiness timeout");
            thread::sleep(Duration::from_millis(100));
        }
        let properties = |name: &str| -> Result<String> {
            Ok(String::from_utf8(android_runner::adb(
                &runner,
                &["shell".into(), "-T".into(), format!("getprop {name}")],
                Duration::from_secs(5),
            )?)?
            .trim()
            .to_owned())
        };
        let device_abi = properties("ro.product.cpu.abi")?;
        let device_api: u32 = properties("ro.build.version.sdk")?
            .parse()
            .context("invalid device API")?;
        ensure!(
            device_abi == settings.abi,
            "Android device ABI {device_abi} does not match {}",
            settings.abi
        );
        ensure!(
            device_api >= settings.api,
            "Android device API {device_api} is below compilation API {}",
            settings.api
        );
        let features = android_runner::adb(&runner, &["features".into()], Duration::from_secs(5))?;
        ensure!(
            String::from_utf8_lossy(&features)
                .split([',', '\n', '\r'])
                .any(|feature| feature == "shell_v2"),
            "Android adb shell_v2 required"
        );
        android_runner::adb_expected(
            &runner,
            &["shell".into(), "-T".into(), "exit 9".into()],
            Duration::from_secs(5),
            9,
        )?;
        self.build_helper(&runner, &context.toolchain)?;
        let remote = android_runner::device_root(&runner);
        let bootstrap = format!(
            "trap '' HUP; umask 077; test ! -L /data/local/tmp/rs-infra-platform && mkdir -p /data/local/tmp/rs-infra-platform && mkdir {remote} && (set -C; printf '%s' {} > {remote}/.owner) && sync",
            runner.token
        );
        update_registration(self.registration.as_ref().context("registration missing")?, |value| {
            value.guest_cleanup_pending = true;
            value.bootstrap = "root_intent".into();
        })?;
        self.remote_created = true;
        android_runner::adb(
            &runner,
            &["shell".into(), "-T".into(), bootstrap],
            Duration::from_secs(10),
        )?;
        update_registration(self.registration.as_ref().context("registration missing")?, |value| {
            value.bootstrap = "root_created".into()
        })?;
        self.install_helper()?;
        Ok(EnvironmentInfo {
            platform: Platform::Android,
            target: runner.target,
            os_version: format!("Android API {device_api}"),
            filesystem: "Android native shell filesystem (/data/local/tmp); APK permissions not exercised".into(),
            versions: BTreeMap::from([
                ("ndk".into(), settings.ndk_version),
                ("abi".into(), device_abi),
                ("serial".into(), runner.serial),
                ("compile_api".into(), settings.api.to_string()),
            ]),
        })
    }

    fn execute(&mut self, suite: &SuiteSpec, context: &SessionContext) -> Result<Vec<ExecutionResult>> {
        let settings = self.settings()?.clone();
        let mut runner = self.runner.clone().context("Android not started")?;
        ensure!(
            (1..=86400).contains(&suite.timeout_seconds),
            "Android suite timeout must be in 1..=86400 seconds"
        );
        ensure!(
            suite.target.as_ref().is_none_or(|value| value == &runner.target),
            "suite target does not match configured Android ABI target {}",
            runner.target
        );
        for requirement in &suite.requirements {
            ensure!(
                ["native-shell", "writable-tmpdir"].contains(&requirement.as_str()),
                "unsupported Android requirement {requirement}; cross-target doctests are unsupported"
            );
        }
        runner.timeout_seconds = suite.timeout_seconds;
        runner.data = suite.data.clone();
        let context_path = runner.local_root.join("cargo-context.json");
        android_runner::write_context(&context_path, &runner)?;
        let executable = std::env::current_exe()?.canonicalize()?;
        let mut env = BTreeMap::from([
            (
                OsString::from("RS_INFRA_ANDROID_CONTEXT"),
                context_path.into_os_string(),
            ),
            (OsString::from("RUSTUP_TOOLCHAIN"), OsString::from(&context.toolchain)),
            (
                OsString::from("CARGO_TARGET_DIR"),
                context.host_run_dir.join("target-android").into_os_string(),
            ),
        ]);
        let target_key = runner.target.replace('-', "_");
        env.insert(
            OsString::from(format!("CARGO_TARGET_{}_LINKER", target_key.to_ascii_uppercase())),
            self.linker(false)?.into_os_string(),
        );
        // Cargo string runners split on spaces. Use its TOML array override for paths
        // containing spaces.
        let runner_array = toml::Value::Array(vec![
            toml::Value::String(executable.to_string_lossy().into_owned()),
            toml::Value::String("android-runner".into()),
        ])
        .to_string();
        let cc = self.linker(false)?;
        let cxx = self.linker(true)?;
        let ar = self.llvm()?.join("bin/llvm-ar");
        for suffix in [runner.target.clone(), target_key.clone()] {
            for (key, value) in [("CC", &cc), ("CXX", &cxx), ("AR", &ar)] {
                env.insert(OsString::from(format!("{key}_{suffix}")), value.as_os_str().to_owned());
            }
        }
        let mut results = Vec::new();
        let execution = (|| -> Result<()> {
            for (index, command) in suite.commands.iter().enumerate() {
                let mut argv = command.argv.clone();
                ensure!(
                    argv.first()
                        .is_some_and(|program| program.rsplit(['/', '\\']).next() == Some("cargo")),
                    "Android suites require Cargo commands"
                );
                ensure!(
                    !argv.iter().any(|arg| [
                        "--doc",
                        "--target",
                        "--target-dir",
                        "--config",
                        "--manifest-path",
                        "--build-dir"
                    ]
                    .iter()
                    .any(|flag| arg == flag || arg.starts_with(&format!("{flag}=")))),
                    "Android suite must not override target/configuration or request unsupported doctests"
                );
                if argv.iter().any(|arg| arg == "test") {
                    ensure!(
                        argv.iter().any(|arg| [
                            "--lib",
                            "--tests",
                            "--test",
                            "--bins",
                            "--bin",
                            "--examples",
                            "--example",
                            "--all-targets"
                        ]
                        .iter()
                        .any(|selector| arg == selector || arg.starts_with(&format!("{selector}=")))),
                        "Android cargo test needs explicit --lib/--tests/--test selection; cross-target doctests unsupported"
                    );
                }
                argv[0] = settings.cargo.clone();
                let split = argv.iter().position(|arg| arg == "--").unwrap_or(argv.len());
                argv.splice(
                    split..split,
                    [
                        "--target".into(),
                        runner.target.clone(),
                        "--target-dir".into(),
                        context
                            .host_run_dir
                            .join("target-android")
                            .to_string_lossy()
                            .into_owned(),
                        "--config".into(),
                        format!("target.{}.runner={runner_array}", runner.target),
                    ],
                );
                let cwd = runner.project_root.join(&command.cwd).canonicalize()?;
                ensure!(
                    cwd.starts_with(&context.snapshot_root),
                    "Android command cwd escaped source snapshot"
                );
                let previous: std::collections::BTreeSet<_> = fs::read_dir(&context.report_dir)?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<std::io::Result<_>>()?;
                let result = process::run(
                    &ProcessSpec {
                        argv,
                        cwd,
                        env: env.clone(),
                        timeout: Duration::from_secs(suite.timeout_seconds),
                        stdout: context.report_dir.join(format!("{}-{index}.stdout.log", suite.id)),
                        stderr: context.report_dir.join(format!("{}-{index}.stderr.log", suite.id)),
                    },
                    cancelled(),
                )?;
                if result.timed_out || result.cancelled {
                    self.cancel_invocations()?;
                }
                let mut runner_errors = Vec::new();
                let mut device_results = Vec::new();
                for entry in fs::read_dir(&context.report_dir)? {
                    let path = entry?.path();
                    if previous.contains(&path) {
                        continue;
                    }
                    if path.extension().is_some_and(|extension| extension == "error") {
                        runner_errors.push(fs::read_to_string(path)?);
                    } else if path.extension().is_some_and(|extension| extension == "json")
                        && path.file_stem().is_some_and(|name| {
                            name.to_string_lossy().strip_prefix("android-").is_some_and(|suffix| {
                                suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
                            })
                        })
                    {
                        device_results.push(path);
                    }
                }
                let category = if !runner_errors.is_empty() {
                    "runner_infrastructure_error"
                } else if result.timed_out || result.cancelled {
                    "host_timeout_or_cancel"
                } else if !device_results.is_empty() {
                    "device_test_result"
                } else if result.exit_code != Some(0) {
                    "cargo_compilation_or_command_error"
                } else {
                    "compile_only"
                };
                fs::write(
                    context.report_dir.join(format!("android-command-{index}.json")),
                    serde_json::to_vec_pretty(
                        &serde_json::json!({"category":category,"cargo_exit_code":result.exit_code,"device_result_files":device_results,"runner_errors":runner_errors}),
                    )?,
                )?;
                ensure!(
                    runner_errors.is_empty(),
                    "Android runner infrastructure failure: {}",
                    runner_errors.join("; ")
                );
                let failed = result.exit_code != Some(0) || result.timed_out || result.cancelled;
                results.push(result);
                if failed {
                    break;
                }
            }
            Ok(())
        })();
        execution.map_err(|error| crate::report::with_completed_results(error, &results))?;
        Ok(results)
    }

    fn cleanup(&mut self, _context: &SessionContext) -> Result<CleanupReport> {
        if !self.remote_created {
            return Ok(CleanupReport::default());
        }
        if let Some(path) = &self.registration
            && read_registration(path)?.bootstrap != "ready"
        {
            self.install_helper()?;
        }
        self.cancel_invocations()?;
        let runner = self.runner.as_ref().context("Android session missing")?;
        match android_runner::helper(runner, "clean", None, Duration::from_secs(30)) {
            Ok(()) => {
                self.remote_created = false;
                if let Some(path) = &self.registration {
                    update_registration(path, |value| {
                        value.guest_cleanup_pending = false;
                        value.bootstrap = "cleaned".into();
                    })?;
                }
                fs::write(
                    runner.report_dir.join("android-session.cleaned"),
                    b"device cleanup confirmed\n",
                )?;
                Ok(CleanupReport::default())
            }
            Err(error) => Ok(CleanupReport {
                remaining: vec![PathBuf::from(android_runner::device_root(runner))],
                errors: vec![format!("Android cleanup pending: {error:#}")],
                ..CleanupReport::default()
            }),
        }
    }

    fn stop(&mut self) -> Result<()> {
        let cancellation = self.cancel_invocations();
        let stopped = if let Some(path) = &self.registration {
            stop_registration(path, self.emulator.as_mut())
        } else {
            Ok(())
        };
        if let Some(mut guard) = self.emulator.take()
            && child_exited(&guard).unwrap_or(false)
        {
            let _ = guard.wait();
        }
        self.lease.take();
        cancellation?;
        stopped
    }
}

fn target(abi: &str) -> Result<&'static str> {
    match abi {
        "x86_64" => Ok("x86_64-linux-android"),
        "arm64-v8a" => Ok("aarch64-linux-android"),
        "armeabi-v7a" => Ok("armv7-linux-androideabi"),
        "x86" => Ok("i686-linux-android"),
        _ => bail!("unsupported Android ABI {abi}"),
    }
}

/// Cancels registered device tests and stops provably owned Emulator instances,
/// preserving guest and host artifacts and keeping borrowed devices running.
/// Unconfirmed cancellation on borrowed devices or unknown owned process
/// identities return errors so durable recovery obligations remain pending.
pub(crate) fn recover_stop(report_dir: &Path, _table: Option<&toml::Value>) -> Result<()> {
    if !report_dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(report_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = registration_path(&entry.path());
        if path.exists() {
            let registration = read_registration(&path)?;
            let value = toml::Value::try_from(&registration.settings)?;
            let mut backend = AndroidBackend::from_table(Some(&value))?;
            backend.runner = Some(registration.runner.clone());
            backend.remote_created = registration.bootstrap != "cleaned";
            backend.lease = Some(Lease::acquire(
                &registration.storage_root,
                &format!("android:{}", registration.runner.serial),
            )?);
            let cancellation = backend.cancel_invocations();
            let stopped = stop_registration(&path, None);
            // Confirmed owned shutdown also proves its tests stopped when the
            // transport disappeared before cancellation could acknowledge it.
            if registration.owned && stopped.is_ok() {
                continue;
            }
            match (cancellation, stopped) {
                (Ok(()), Ok(())) => {}
                (Err(cancel), Err(stop)) => {
                    bail!("Android test cancellation: {cancel:#}; environment stop: {stop:#}")
                }
                (Err(error), _) | (_, Err(error)) => return Err(error),
            }
        }
    }
    Ok(())
}

/// Recovers registered guest cleanup and then independently confirms owned
/// stop. A recorded owned AVD is relaunched through the same gated owner as
/// normal runs. Borrowed devices are never started or stopped by this path.
pub(crate) fn recover_cleanup(report_dir: &Path, _table: Option<&toml::Value>) -> Result<()> {
    if !report_dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(report_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = registration_path(&entry.path());
        if !path.exists() {
            continue;
        }
        stop_registration(&path, None)?;
        let mut registration = read_registration(&path)?;
        if !registration.guest_cleanup_pending {
            continue;
        }
        let scratch = entry
            .path()
            .join(format!("android-recovery-{}", android_runner::random_hex(8)?));
        fs::create_dir(&scratch)?;
        registration = update_registration(&path, |value| value.runner.local_root = scratch)?;
        let value = toml::Value::try_from(&registration.settings)?;
        let mut backend = AndroidBackend::from_table(Some(&value))?;
        backend.runner = Some(registration.runner.clone());
        backend.registration = Some(path.clone());
        backend.remote_created = true;
        backend.lease = Some(Lease::acquire(
            &registration.storage_root,
            &format!("android:{}", registration.runner.serial),
        )?);
        let cleaned = (|| -> Result<()> {
            if registration.owned {
                ensure!(
                    android_runner::adb(&registration.runner, &["get-state".into()], Duration::from_secs(5)).is_err(),
                    "recorded owned serial is occupied by an unowned environment"
                );
                backend.launch_owned()?;
            }
            let deadline = Instant::now() + Duration::from_secs(registration.settings.boot_timeout_seconds);
            loop {
                if android_runner::adb(
                    &registration.runner,
                    &["shell".into(), "-T".into(), "printf rs-infra-shell-ready".into()],
                    Duration::from_secs(3),
                )
                .is_ok_and(|value| value == b"rs-infra-shell-ready")
                {
                    break;
                }
                ensure!(Instant::now() < deadline, "registered Android device reconnect timeout");
                thread::sleep(Duration::from_millis(100));
            }
            let remote = android_runner::device_root(&registration.runner);
            let state = format!(
                "if test ! -e {remote}; then printf absent; elif test ! -L {remote} && test ! -L {remote}/.owner && test \"$(cat {remote}/.owner)\" = {}; then printf owned; else printf unowned; fi",
                registration.runner.token
            );
            let status = android_runner::adb(
                &registration.runner,
                &["shell".into(), "-T".into(), state],
                Duration::from_secs(10),
            )?;
            if status == b"absent" {
                update_registration(&path, |value| {
                    value.guest_cleanup_pending = false;
                    value.bootstrap = "cleaned".into();
                })?;
                backend.remote_created = false;
                return Ok(());
            }
            ensure!(
                status == b"owned",
                "registered Android root has no matching owner marker; cleanup refused"
            );
            let report = backend.cleanup(&SessionContext {
                run_id: crate::model::RunId(registration.runner.run_id.clone()),
                host_run_dir: registration.runner.local_root.clone(),
                snapshot_root: registration.runner.project_root.clone(),
                project_relative: PathBuf::new(),
                report_dir: registration.runner.report_dir.clone(),
                toolchain: registration.toolchain.clone(),
                cleanup: crate::model::CleanupPolicy::Enabled,
            })?;
            ensure!(report.errors.is_empty(), "{}", report.errors.join("; "));
            Ok(())
        })();
        let stopped = backend.stop();
        match (cleaned, stopped) {
            (Ok(()), Ok(())) => {}
            (Err(clean), Err(stop)) => {
                bail!("Android guest cleanup: {clean:#}; environment stop: {stop:#}")
            }
            (Err(error), _) | (_, Err(error)) => return Err(error),
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    pid: u32,
    start: String,
    host: String,
    boot: Option<String>,
}
impl Identity {
    #[cfg(unix)]
    fn current(pid: u32) -> Result<Self> {
        Ok(Self {
            pid,
            start: process_identity::process_start(pid).context("exact Android owner creation identity unavailable")?,
            host: process_identity::host_id().context("Android owner host identity unavailable")?,
            boot: process_identity::boot_id(),
        })
    }
    fn alive(&self) -> Option<bool> {
        process_identity::owner_alive(Some(&self.host), self.boot.as_deref(), self.pid, Some(&self.start))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AndroidRegistration {
    version: u32,
    storage_root: PathBuf,
    owner_state: String,
    runner: RunnerContext,
    settings: AndroidConfig,
    toolchain: String,
    owned: bool,
    owned_stop_pending: bool,
    guest_cleanup_pending: bool,
    bootstrap: String,
    generation: String,
    guard: Option<Identity>,
    emulator: Option<Identity>,
    anchor: Option<Identity>,
}

fn registration_path(directory: &Path) -> PathBuf {
    directory.join("android-environment.json")
}
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
}
fn read_registration(path: &Path) -> Result<AndroidRegistration> {
    let mut file = private_options()
        .open(path)
        .context("open private Android environment registration")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= 1024 * 1024,
        "invalid Android environment registration"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0 && metadata.uid() == unsafe { libc::geteuid() },
            "Android registration must be private and locally owned"
        );
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let value: AndroidRegistration = serde_json::from_slice(&bytes)?;
    ensure!(
        value.version == 1 && android_runner::valid_id(&value.runner.run_id),
        "invalid Android environment registration version/identity"
    );
    Ok(value)
}
fn save_registration(path: &Path, registration: &AndroidRegistration) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", android_runner::random_hex(8)?));
    let mut file = private_options().create_new(true).open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(registration)?)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(path.parent().context("registration parent missing")?)?.sync_all()?;
    Ok(())
}
fn update_registration(path: &Path, change: impl FnOnce(&mut AndroidRegistration)) -> Result<AndroidRegistration> {
    let lock = private_options()
        .create(true)
        .truncate(false)
        .open(path.with_extension("record-lock"))?;
    lock.lock_exclusive()?;
    let mut value = read_registration(path)?;
    change(&mut value);
    save_registration(path, &value)?;
    Ok(value)
}
fn stop_request(path: &Path, registration: &AndroidRegistration) -> Result<()> {
    let mut file = private_options()
        .create(true)
        .truncate(true)
        .open(path.with_extension("stop"))?;
    file.write_all(registration.generation.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn child_exited(child: &Child) -> Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: WNOWAIT observes our unreaped child while preserving its PID and
    // group identity.
    ensure!(
        unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WNOHANG | libc::WEXITED | libc::WNOWAIT,
            )
        } == 0,
        "observe owned Android process: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { info.si_pid() } != 0)
}

#[cfg(not(unix))]
fn child_exited(_child: &Child) -> Result<bool> {
    bail!("Android owner requires a Unix host")
}
#[cfg(unix)]
fn signal_child_group(child: &Child, signal: i32) -> Result<()> {
    // SAFETY: this child remains unreaped until every group operation and
    // confirmation completes.
    if unsafe { libc::kill(-(child.id() as i32), signal) } != 0 {
        let error = std::io::Error::last_os_error();
        ensure!(
            error.raw_os_error() == Some(libc::ESRCH),
            "signal owned Android group: {error}"
        );
    }
    Ok(())
}
#[cfg(unix)]
fn finish_emulator(child: &mut Child, runner: &RunnerContext) -> Result<bool> {
    let mut forced = false;
    if !child_exited(child)? {
        if android_runner::adb(
            runner,
            &["shell".into(), "-T".into(), "sync".into()],
            Duration::from_secs(15),
        )
        .is_err()
        {
            forced = true;
        }
        let _ = android_runner::adb(runner, &["emu".into(), "kill".into()], Duration::from_secs(10));
        let deadline = Instant::now() + Duration::from_secs(15);
        while !child_exited(child)? && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if !child_exited(child)? {
            forced = true;
            signal_child_group(child, libc::SIGTERM)?;
            thread::sleep(Duration::from_millis(100));
        }
    }
    signal_child_group(child, libc::SIGKILL)?;
    process::wait_unix_group_stopped(child.id() as i32, Duration::from_secs(10))?;
    child.wait()?;
    Ok(forced)
}

/// Internal owner/child launcher. Both stages wait for a private pipe gate
/// before launching the next stage; exact identities are durable before
/// Emulator exec. A lifetime anchor kills only its own reserved group if its
/// owner disappears. Returns failures for registration, launch, identity, or
/// confirmed-stop failures.
pub(crate) fn emulator_owner(path: &Path, child_mode: bool) -> Result<i32> {
    #[cfg(not(unix))]
    {
        let _ = (path, child_mode);
        bail!("Android owner requires a Unix host");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let registration = read_registration(path)?;
        ensure!(registration.owned, "borrowed Android environments cannot run an owner");
        if child_mode {
            let mut gate = [0_u8; 1];
            if std::io::stdin().read_exact(&mut gate).is_err() || gate != *b"G" {
                return Ok(0);
            }
            // The anchor inherits only the lifetime pipe. It performs raw async-signal-safe
            // operations after fork, retaining this group's identity until the last kill.
            let anchor = unsafe { libc::fork() };
            ensure!(anchor >= 0, "fork Android lifetime anchor");
            if anchor == 0 {
                unsafe {
                    libc::close(1);
                    libc::close(2);
                    let mut byte = 0_u8;
                    while libc::read(0, (&mut byte as *mut u8).cast(), 1) > 0 {}
                    libc::kill(-libc::getpgrp(), libc::SIGKILL);
                    libc::_exit(71);
                }
            }
            let anchor_identity = Identity::current(anchor as u32)?;
            update_registration(path, |value| value.anchor = Some(anchor_identity))?;
            let mut command = emulator_command(&registration)?;
            command.stdin(Stdio::null());
            return Err(command.exec()).context("exec registered Android Emulator");
        }
        let lease = private_options()
            .create(true)
            .truncate(false)
            .open(path.with_extension("guard-lease"))?;
        lease.try_lock_exclusive().context("Android owner lease is active")?;
        let identity = Identity::current(std::process::id())?;
        update_registration(path, |value| {
            value.guard = Some(identity);
            value.owner_state = "owner_ready".into();
            value.owned_stop_pending = true;
        })?;
        let mut gate = [0_u8; 1];
        if std::io::stdin().read_exact(&mut gate).is_err() || gate != *b"G" {
            update_registration(path, |value| value.owned_stop_pending = false)?;
            return Ok(0);
        }
        let mut child = Command::new(std::env::current_exe()?)
            .args(["android-owner", "--context"])
            .arg(path)
            .arg("--child")
            .stdin(Stdio::piped())
            .stdout(File::create(
                registration.runner.report_dir.join("emulator.stdout.log"),
            )?)
            .stderr(File::create(
                registration.runner.report_dir.join("emulator.stderr.log"),
            )?)
            .process_group(0)
            .spawn()?;
        let identity = Identity::current(child.id())?;
        update_registration(path, |value| {
            value.emulator = Some(identity);
            value.anchor = None;
        })?;
        child
            .stdin
            .as_mut()
            .context("Android child gate missing")?
            .write_all(b"G")?;
        let (eof_tx, eof_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut bytes);
            let _ = eof_tx.send(());
        });
        loop {
            if child_exited(&child)?
                || eof_rx.try_recv().is_ok()
                || fs::read_to_string(path.with_extension("stop")).is_ok_and(|value| value == registration.generation)
            {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let forced = finish_emulator(&mut child, &registration.runner)?;
        update_registration(path, |value| {
            value.owned_stop_pending = false;
            value.owner_state = "stopped".into();
        })?;
        ensure!(!forced, "owned Android shutdown required force or failed device sync");
        Ok(0)
    }
}

#[cfg(unix)]
fn emulator_command(registration: &AndroidRegistration) -> Result<Command> {
    let settings = &registration.settings;
    let avd = settings.avd.as_deref().context("registered AVD missing")?;
    let mut command = Command::new(settings.sdk_root.join("emulator/emulator"));
    command
        .args([
            "-avd",
            avd,
            "-port",
            &settings.emulator_port.to_string(),
            "-no-window",
            "-no-audio",
            "-no-snapshot",
            "-accel",
            "on",
            "-gpu",
            "software",
        ])
        .env(
            "ANDROID_AVD_HOME",
            settings
                .avd_home
                .clone()
                .or_else(|| std::env::var_os("ANDROID_AVD_HOME").map(PathBuf::from))
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".android/avd")))
                .context("registered AVD home missing")?,
        );
    Ok(command)
}

fn stop_registration(path: &Path, mut handle: Option<&mut Child>) -> Result<()> {
    let registration = read_registration(path)?;
    if !registration.owned {
        return Ok(());
    }
    stop_request(path, &registration)?;
    if let Some(child) = handle.as_deref_mut() {
        drop(child.stdin.take());
    }
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let current = read_registration(path)?;
        let mut alive = false;
        for identity in [&current.guard, &current.emulator, &current.anchor]
            .into_iter()
            .flatten()
        {
            match identity.alive() {
                Some(value) => alive |= value,
                None => bail!("owned Android process identity cannot be established; stop remains pending"),
            }
        }
        if !alive {
            #[cfg(unix)]
            if let Some(identity) = &current.emulator {
                process::wait_unix_group_stopped(identity.pid as i32, Duration::ZERO)?;
            }
            update_registration(path, |value| {
                value.owned_stop_pending = false;
                value.owner_state = "stopped".into();
            })?;
            if let Some(child) = handle.as_deref_mut() {
                let status = child.wait()?;
                ensure!(status.success(), "Android owner reported shutdown failure: {status}");
            }
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "owned Android environment stop unconfirmed; owned_stop_pending retained"
        );
        thread::sleep(Duration::from_millis(20));
    }
}
