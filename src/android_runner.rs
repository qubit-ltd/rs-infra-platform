// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Cargo runner and bounded transport for the fixed Android device session.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunnerContext {
    pub(crate) protocol_version: u32,
    pub(crate) run_id: String,
    pub(crate) token: String,
    pub(crate) serial: String,
    pub(crate) adb: PathBuf,
    pub(crate) readelf: PathBuf,
    pub(crate) ndk_libraries: PathBuf,
    pub(crate) system_libraries: Vec<String>,
    pub(crate) target: String,
    pub(crate) local_root: PathBuf,
    pub(crate) report_dir: PathBuf,
    pub(crate) project_root: PathBuf,
    pub(crate) data: Vec<PathBuf>,
    pub(crate) timeout_seconds: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct DeviceResult {
    pub(crate) exit_code: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) cancelled: bool,
}

/// Executes one Cargo test artifact on the context's fixed serial. Returns the
/// actual exit code (128 + signal for signal exits); transport, deployment and
/// protocol failures are errors. Device logs and raw outcome remain in
/// report_dir.
pub(crate) fn run(argv: &[String]) -> Result<i32> {
    let path = std::env::var_os("RS_INFRA_ANDROID_CONTEXT").context("RS_INFRA_ANDROID_CONTEXT is required")?;
    let context = read_context(Path::new(&path))?;
    ensure!(!argv.is_empty(), "Android runner requires an executable");
    let invocation = random_hex(16)?;
    let local = context.local_root.join(&invocation);
    fs::create_dir(&local).context("create Android invocation directory")?;
    let outcome = run_one(&context, argv, &invocation, &local);
    if let Err(error) = &outcome {
        let _ = fs::write(
            context.report_dir.join(format!("android-{invocation}.error")),
            format!("{error:#}"),
        );
        if local.join("prepared").exists()
            && let Err(cancel_error) = helper(&context, "cancel", Some(&invocation), Duration::from_secs(15))
        {
            return Err(anyhow::anyhow!(
                "{error:#}; Android cancellation unconfirmed: {cancel_error:#}"
            ));
        }
    }
    outcome
}

fn run_one(context: &RunnerContext, argv: &[String], invocation: &str, local: &Path) -> Result<i32> {
    let binary = PathBuf::from(&argv[0])
        .canonicalize()
        .context("resolve Android test executable")?;
    validate_elf(&binary, &context.target)?;
    let libraries = libraries(context, &binary)?;
    let stage = local.join("stage");
    fs::create_dir(&stage)?;
    let work = stage.join("work");
    fs::create_dir(&work)?;
    for relative in &context.data {
        copy_data(&context.project_root, relative, &work)?;
    }
    fs::copy(&binary, stage.join("test"))?;
    fs::create_dir(stage.join("lib"))?;
    for (needed_name, source) in &libraries {
        fs::copy(source, stage.join("lib").join(needed_name))?;
    }
    fs::write(stage.join("request"), request(context.timeout_seconds, &argv[1..])?)?;
    helper(context, "prepare", Some(invocation), Duration::from_secs(15))?;
    fs::write(local.join("prepared"), b"1")?;
    let remote = format!("{}/{invocation}", device_root(context));
    for name in ["test", "request", "lib", "work"] {
        adb(
            context,
            &[
                "push".into(),
                stage.join(name).to_string_lossy().into_owned(),
                format!("{remote}/"),
            ],
            Duration::from_secs(120),
        )?;
    }
    // Verify the actual received bytes using the sync protocol, independently of
    // shell output.
    let verify = local.join("verify");
    fs::create_dir(&verify)?;
    for name in ["test", "request", "lib", "work"] {
        adb(
            context,
            &[
                "pull".into(),
                format!("{remote}/{name}"),
                verify.join(name).to_string_lossy().into_owned(),
            ],
            Duration::from_secs(120),
        )?;
    }
    ensure!(
        tree_digest(&stage)? == tree_digest(&verify)?,
        "Android upload integrity mismatch"
    );
    // Device timeout is authoritative. A host timeout triggers a separate cancel
    // request.
    let execution = helper(
        context,
        "run",
        Some(invocation),
        Duration::from_secs(context.timeout_seconds + 20),
    );
    let result_path = context.report_dir.join(format!("android-{invocation}.result"));
    let received = adb(
        context,
        &[
            "pull".into(),
            format!("{remote}/result"),
            result_path.to_string_lossy().into_owned(),
        ],
        Duration::from_secs(15),
    );
    let logs = (|| -> Result<()> {
        for (name, stderr) in [("stdout", false), ("stderr", true)] {
            let path = context.report_dir.join(format!("android-{invocation}.{name}.log"));
            adb(
                context,
                &[
                    "pull".into(),
                    format!("{remote}/{name}"),
                    path.to_string_lossy().into_owned(),
                ],
                Duration::from_secs(30),
            )?;
            let mut file = File::open(path)?;
            if stderr {
                std::io::copy(&mut file, &mut std::io::stderr().lock())?;
            } else {
                std::io::copy(&mut file, &mut std::io::stdout().lock())?;
            }
        }
        Ok(())
    })();
    if let Err(error) = execution {
        bail!(
            "ADB transport/supervisor failed; device result is not accepted as completed: {error:#}; result retrieval: {received:?}; log retrieval: {logs:?}"
        );
    }
    received.context("ADB transport: supervisor result missing")?;
    let result = parse_result(&fs::read(&result_path)?)?;
    fs::write(
        context.report_dir.join(format!("android-{invocation}.json")),
        serde_json::to_vec_pretty(&result)?,
    )?;
    logs.context("archive Android device logs")?;
    if result.timed_out {
        bail!("Android device test timed out");
    }
    if result.cancelled {
        bail!("Android device test cancelled");
    }
    Ok(result.exit_code.unwrap_or_else(|| 128 + result.signal.unwrap_or(0)))
}

fn read_context(path: &Path) -> Result<RunnerContext> {
    let metadata = fs::symlink_metadata(path).context("read private Android context")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Android context must be a private regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions and only reads this process's effective
        // uid.
        ensure!(
            metadata.permissions().mode() & 0o077 == 0 && metadata.uid() == unsafe { libc::geteuid() },
            "Android context must be private and owned by the current user"
        );
    }
    ensure!(metadata.len() <= 1024 * 1024, "Android context exceeds 1 MiB");
    let context: RunnerContext = serde_json::from_slice(&fs::read(path)?).context("decode Android context")?;
    ensure!(
        context.protocol_version == 1,
        "Android context protocol version mismatch"
    );
    ensure!(
        valid_id(&context.run_id) && is_hex(&context.token, 64),
        "Android context identity invalid"
    );
    ensure!(valid_serial(&context.serial), "Android serial invalid");
    ensure!(
        (1..=86400).contains(&context.timeout_seconds),
        "Android timeout out of range"
    );
    for path in [
        &context.adb,
        &context.readelf,
        &context.ndk_libraries,
        &context.local_root,
        &context.report_dir,
        &context.project_root,
    ] {
        ensure!(path.is_absolute(), "Android context paths must be absolute");
    }
    for path in &context.data {
        validate_relative(path)?;
    }
    Ok(context)
}

pub(crate) fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 100 && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}
pub(crate) fn valid_serial(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}
fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
pub(crate) fn device_root(context: &RunnerContext) -> String {
    format!("/data/local/tmp/rs-infra-platform/{}", context.run_id)
}

/// Creates a random, non-shell-significant invocation or lease identifier.
pub(crate) fn random_hex(length: usize) -> Result<String> {
    let mut bytes = vec![0; length];
    File::open("/dev/urandom")
        .context("open OS random source")?
        .read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Writes secrets atomically to a newly created owner-only context file.
pub(crate) fn write_context(path: &Path, context: &RunnerContext) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).context("create private Android context")?;
    file.write_all(&serde_json::to_vec(context)?)?;
    file.sync_all()?;
    Ok(())
}

/// Runs an adb control command with explicit serial and bounded output/time.
/// No user-controlled test argv enters its remote shell command.
pub(crate) fn adb(context: &RunnerContext, args: &[String], timeout: Duration) -> Result<Vec<u8>> {
    adb_expected(context, args, timeout, 0)
}

/// Executes a fixed control canary whose nonzero remote exit must survive adb.
/// A mismatched status is a transport/capability failure rather than a test
/// result.
pub(crate) fn adb_expected(
    context: &RunnerContext,
    args: &[String],
    timeout: Duration,
    expected: i32,
) -> Result<Vec<u8>> {
    let stem = random_hex(8)?;
    let stdout = context.local_root.join(format!("adb-{stem}.out"));
    let stderr = context.local_root.join(format!("adb-{stem}.err"));
    let mut child = Command::new(&context.adb)
        .args(["-s", &context.serial])
        .args(args)
        .stdin(Stdio::null())
        .stdout(File::create(&stdout)?)
        .stderr(File::create(&stderr)?)
        .spawn()
        .context("start fixed-serial adb")?;
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "ADB transport timeout for {}",
                args.first().map_or("command", String::as_str)
            );
        }
        thread::sleep(Duration::from_millis(10));
    };
    ensure!(
        fs::metadata(&stdout)?.len() <= 4 * 1024 * 1024 && fs::metadata(&stderr)?.len() <= 4 * 1024 * 1024,
        "ADB control output exceeds bound"
    );
    ensure!(
        status.code() == Some(expected),
        "ADB transport/control failed ({status}): {}",
        String::from_utf8_lossy(&fs::read(stderr)?)
    );
    Ok(fs::read(stdout)?)
}

pub(crate) fn helper(
    context: &RunnerContext,
    operation: &str,
    invocation: Option<&str>,
    timeout: Duration,
) -> Result<()> {
    ensure!(
        ["init", "prepare", "run", "cancel", "clean"].contains(&operation),
        "invalid supervisor operation"
    );
    ensure!(
        valid_id(&context.run_id) && is_hex(&context.token, 64),
        "invalid supervisor identity"
    );
    ensure!(
        invocation.is_none_or(|value| is_hex(value, 32)),
        "invalid supervisor invocation"
    );
    let mut command = format!(
        "{}/supervisor {operation} {} {}",
        device_root(context),
        context.run_id,
        context.token
    );
    if let Some(invocation) = invocation {
        command.push(' ');
        command.push_str(invocation);
    }
    adb(context, &["shell".into(), "-T".into(), command], timeout)?;
    Ok(())
}

fn request(timeout: u64, args: &[String]) -> Result<Vec<u8>> {
    ensure!(args.len() <= 1024, "too many Android test arguments");
    let mut bytes = b"RSPAD001".to_vec();
    bytes.extend_from_slice(&(timeout * 1000).to_le_bytes());
    bytes.extend_from_slice(&(args.len() as u32).to_le_bytes());
    for arg in args {
        ensure!(
            !arg.as_bytes().contains(&0) && arg.len() <= 1024 * 1024,
            "invalid Android test argument"
        );
        bytes.extend_from_slice(&(arg.len() as u32).to_le_bytes());
        bytes.extend_from_slice(arg.as_bytes());
    }
    ensure!(bytes.len() <= 4 * 1024 * 1024, "Android request exceeds 4 MiB");
    Ok(bytes)
}

fn parse_result(bytes: &[u8]) -> Result<DeviceResult> {
    ensure!(bytes.len() <= 4096, "Android result exceeds bound");
    let value = std::str::from_utf8(bytes).context("Android result is not UTF-8")?;
    let fields: Vec<_> = value.lines().collect();
    ensure!(fields.first() == Some(&"RSPAD001"), "Android result protocol mismatch");
    if fields.get(1) == Some(&"spawn") {
        bail!("Android device spawn failed: {}", fields.get(2).unwrap_or(&"unknown"));
    }
    ensure!(fields.len() == 6 && fields[1] == "exit", "Android result is incomplete");
    let code = fields[2].parse::<i32>().context("invalid Android exit code")?;
    let signal = fields[3].parse::<i32>().context("invalid Android signal")?;
    ensure!(
        (-1..=255).contains(&code) && (0..=64).contains(&signal) && ((code == -1) != (signal == 0)),
        "inconsistent Android exit/signal"
    );
    ensure!(
        ["0", "1"].contains(&fields[4]) && ["0", "1"].contains(&fields[5]),
        "invalid Android result flags"
    );
    Ok(DeviceResult {
        exit_code: (code >= 0).then_some(code),
        signal: (signal > 0).then_some(signal),
        timed_out: fields[4] == "1",
        cancelled: fields[5] == "1",
    })
}

fn validate_elf(path: &Path, target: &str) -> Result<()> {
    let mut header = [0_u8; 20];
    File::open(path)?
        .read_exact(&mut header)
        .context("read Android ELF header")?;
    ensure!(
        &header[..4] == b"\x7fELF" && header[5] == 1,
        "Android executable is not a little-endian ELF"
    );
    let expected = match target {
        "x86_64-linux-android" => (2, 62),
        "aarch64-linux-android" => (2, 183),
        "armv7-linux-androideabi" => (1, 40),
        "i686-linux-android" => (1, 3),
        _ => bail!("unsupported Android target {target}"),
    };
    ensure!(
        (header[4], u16::from_le_bytes([header[18], header[19]])) == expected,
        "Android ELF ABI does not match {target}"
    );
    Ok(())
}

fn libraries(context: &RunnerContext, binary: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let ndk_root = context
        .ndk_libraries
        .canonicalize()
        .context("resolve Android NDK runtime directory")?;
    let mut pending = vec![binary.to_path_buf()];
    let mut seen = BTreeSet::new();
    let mut libraries = BTreeMap::new();
    while let Some(path) = pending.pop() {
        let output = Command::new(&context.readelf)
            .arg("--dynamic")
            .arg(&path)
            .output()
            .context("inspect Android DT_NEEDED")?;
        ensure!(output.status.success(), "readelf failed for {}", path.display());
        ensure!(
            output.stdout.len() <= 4 * 1024 * 1024,
            "ELF dependency output exceeds bound"
        );
        for line in String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.contains("(NEEDED)"))
        {
            let name = line
                .split('[')
                .nth(1)
                .and_then(|value| value.split(']').next())
                .context("malformed DT_NEEDED entry")?;
            ensure!(
                !name.is_empty() && Path::new(name).components().count() == 1 && !name.contains(['/', '\\']),
                "invalid shared library name"
            );
            if context.system_libraries.iter().any(|system| system == name) {
                continue;
            }
            let parent = binary.parent().context("Android executable parent missing")?;
            let library = [
                path.parent().context("ELF dependency directory missing")?.join(name),
                parent.join(name),
                parent.join("deps").join(name),
                ndk_root.join(name),
            ]
            .into_iter()
            .find(|candidate| candidate.is_file())
            .with_context(|| format!("Android dynamic library missing: {name}"))?;
            let library = library.canonicalize()?;
            if let Some(existing) = libraries.get(name) {
                ensure!(
                    existing == &library,
                    "conflicting Android DT_NEEDED resolution for {name}"
                );
                continue;
            }
            ensure!(seen.insert(name.to_owned()), "duplicate Android dependency resolution");
            // NDK API-level .so files are linker stubs, never deployable runtimes.
            ensure!(
                !(library.starts_with(&ndk_root)
                    && library
                        .parent()
                        .and_then(Path::file_name)
                        .is_some_and(|name| name.to_string_lossy().bytes().all(|byte| byte.is_ascii_digit()))),
                "refuse NDK system stub deployment: {}",
                library.display()
            );
            validate_elf(&library, &context.target)?;
            libraries.insert(name.to_owned(), library.clone());
            pending.push(library);
        }
    }
    Ok(libraries)
}

fn validate_relative(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "Android data paths must be nonempty relative paths without traversal"
    );
    Ok(())
}
fn copy_data(root: &Path, relative: &Path, destination: &Path) -> Result<()> {
    validate_relative(relative)?;
    let source = root.join(relative);
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        ensure!(
            !fs::symlink_metadata(&current)?.file_type().is_symlink(),
            "Android data symlinks are unsupported: {}",
            current.display()
        );
    }
    let metadata = fs::symlink_metadata(&source)?;
    let target = destination.join(relative);
    if metadata.is_dir() {
        fs::create_dir_all(&target)?;
        for entry in fs::read_dir(&source)? {
            copy_data(root, &relative.join(entry?.file_name()), destination)?;
        }
    } else {
        ensure!(metadata.is_file(), "Android data is not a regular file");
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, target)?;
    }
    Ok(())
}
fn tree_digest(root: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    fn visit(root: &Path, relative: &Path, output: &mut Vec<(PathBuf, Vec<u8>)>) -> Result<()> {
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(!metadata.file_type().is_symlink(), "symlink in Android transfer");
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                visit(root, &relative.join(entry?.file_name()), output)?;
            }
        } else {
            ensure!(metadata.is_file(), "non-file in Android transfer");
            let mut file = File::open(path)?;
            let mut hash = Sha256::new();
            let mut buffer = [0; 65536];
            loop {
                let length = file.read(&mut buffer)?;
                if length == 0 {
                    break;
                }
                hash.update(&buffer[..length]);
            }
            output.push((relative.to_path_buf(), hash.finalize().to_vec()));
        }
        Ok(())
    }
    let mut entries = Vec::new();
    visit(root, Path::new(""), &mut entries)?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(entries)
}
