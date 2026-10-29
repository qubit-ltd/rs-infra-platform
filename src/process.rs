// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Controlled test processes and bounded libtest result scanning.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Seek;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;

use crate::model::ExecutionResult;

#[cfg(any(windows, test))]
#[path = "process_windows.rs"]
mod windows_process;

pub(crate) struct ProcessSpec {
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) env: BTreeMap<OsString, OsString>,
    pub(crate) timeout: Duration,
    pub(crate) stdout: PathBuf,
    pub(crate) stderr: PathBuf,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "mechanism", deny_unknown_fields)]
pub(crate) enum StopIdentity {
    Unix {
        host: String,
        boot: String,
        group: i32,
        leader: MemberIdentity,
        anchor: MemberIdentity,
    },
    Windows {
        host: String,
        job: String,
    },
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberIdentity {
    pid: i32,
    start: String,
}

#[derive(Debug)]
struct TreeStopFailed {
    identity: Option<StopIdentity>,
}

impl std::fmt::Display for TreeStopFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("process tree termination remains unconfirmed")
    }
}

impl std::error::Error for TreeStopFailed {}

/// Reports whether an execution error requires retaining files until process
/// termination is proven.
pub(crate) fn tree_stop_failed(error: &anyhow::Error) -> bool {
    error.is::<TreeStopFailed>()
}

/// Returns a serializable identity for retrying an unconfirmed tree stop, when
/// OS proof is available.
pub(crate) fn stop_identity(error: &anyhow::Error) -> Option<&StopIdentity> {
    error
        .downcast_ref::<TreeStopFailed>()
        .and_then(|failure| failure.identity.as_ref())
}

/// Retries only an owned OS identity; false means identity or live-state proof
/// is still insufficient.
pub(crate) fn retry_stop(identity: &StopIdentity) -> Result<bool> {
    controlled::retry_stop(identity)
}

#[derive(Debug)]
struct ProcessNotStarted {
    message: String,
}

impl std::fmt::Display for ProcessNotStarted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProcessNotStarted {}

#[derive(Debug)]
struct ProcessStarted;

impl std::fmt::Display for ProcessStarted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("process started before setup failed")
    }
}

impl std::error::Error for ProcessStarted {}

/// Identifies failures before user code could execute, allowing callers to
/// revoke an unsent request.
pub(crate) fn process_not_started(error: &anyhow::Error) -> bool {
    error.is::<ProcessNotStarted>()
}

fn before_spawn(error: anyhow::Error) -> anyhow::Error {
    let message = error.to_string();
    error.context(ProcessNotStarted { message })
}

fn finish_after_stop<T>(primary: Result<T>, stopped: Result<()>, identity: Option<StopIdentity>) -> Result<T> {
    match stopped {
        Ok(()) => primary,
        Err(stop) => match primary {
            Ok(_) => Err(stop
                .context(TreeStopFailed { identity })
                .context("stop controlled process tree")),
            Err(primary) => Err(primary
                .context(format!("stop controlled process tree also failed: {stop:#}"))
                .context(TreeStopFailed { identity })),
        },
    }
}

#[cfg(any(target_os = "macos", test))]
fn observed_pid_count(bytes: i32, known_absent: bool) -> Result<usize> {
    ensure!(
        bytes > 0 || (bytes == 0 && known_absent),
        "zero or failed PID enumeration cannot prove process group stopped"
    );
    ensure!(
        (bytes as usize).is_multiple_of(std::mem::size_of::<i32>()),
        "incomplete process PID enumeration"
    );
    Ok(bytes as usize / std::mem::size_of::<i32>())
}

/// Observes an already owned Unix group until every task has released its live
/// process resources. Never sends signals; returns an error when state cannot
/// be inspected or the timeout expires.
#[cfg(unix)]
pub(crate) fn wait_unix_group_stopped(group: i32, timeout: Duration) -> Result<()> {
    ensure!(group > 1, "invalid process group for termination observation");
    controlled::wait_group_stopped(group, timeout)
}

/// Runs argv directly with streaming file output and stops ordinary descendants
/// on every exit. Environment entries override inherited values. The caller
/// owns directories and log retention. Unix process groups do not contain
/// children that deliberately create a new session. Windows associates the
/// entire tree with a Job Object atomically during creation. Returns errors for
/// invalid arguments, log IO, spawn, wait, or process-tree termination
/// failures.
pub(crate) fn run(spec: &ProcessSpec, cancelled: &AtomicBool) -> Result<ExecutionResult> {
    run_inner(spec, cancelled, None, None)
}

/// Runs a trusted VS environment helper through cmd.exe /u /d /s /c with a
/// verbatim script tail. Callers must generate the script themselves; suite
/// arguments must use `run` instead.
#[cfg(windows)]
pub(crate) fn run_windows_script(
    spec: &ProcessSpec,
    script: &OsStr,
    cancelled: &AtomicBool,
) -> Result<ExecutionResult> {
    run_inner(spec, cancelled, None, Some(script))
}

/// Provides finite stdin bytes using a temporary file, so an unread pipe cannot
/// block cancellation. The temporary file is owned by this call and removed
/// automatically; errors include temporary IO.
pub(crate) fn run_with_input(spec: &ProcessSpec, cancelled: &AtomicBool, input: &[u8]) -> Result<ExecutionResult> {
    let directory = spec
        .stdout
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stdin = (|| -> Result<File> {
        let mut stdin = tempfile::tempfile_in(directory).context("create controlled process input")?;
        stdin.write_all(input).context("write controlled process input")?;
        stdin.rewind().context("rewind controlled process input")?;
        Ok(stdin)
    })()
    .map_err(before_spawn)?;
    run_inner(spec, cancelled, Some(stdin), None)
}

fn run_inner(
    spec: &ProcessSpec,
    cancelled: &AtomicBool,
    stdin: Option<File>,
    raw_tail: Option<&OsStr>,
) -> Result<ExecutionResult> {
    let (stdout, stderr) = (|| -> Result<_> {
        ensure!(!spec.argv.is_empty(), "process argv must not be empty");
        ensure!(
            spec.stdout != spec.stderr,
            "stdout and stderr must use distinct log files"
        );
        let stdout = File::create(&spec.stdout).context("create process stdout log")?;
        let stderr = File::create(&spec.stderr).context("create process stderr log")?;
        Ok((stdout, stderr))
    })()
    .map_err(before_spawn)?;
    let mut result = ExecutionResult {
        exit_code: None,
        timed_out: false,
        cancelled: cancelled.load(Ordering::Relaxed),
        tests_passed: 0,
        tests_failed: 0,
        tests_ignored: 0,
        stdout: spec.stdout.clone(),
        stderr: spec.stderr.clone(),
    };
    if result.cancelled {
        return Ok(result);
    }
    let mut process = controlled::Process::spawn_spec(spec, stdin, stdout, stderr, raw_tail)
        .map_err(|error| {
            if error.is::<ProcessStarted>() || tree_stop_failed(&error) {
                error
            } else {
                before_spawn(error)
            }
        })
        .context("spawn controlled process")?;
    let state = drive_process(&mut process, cancelled, spec.timeout)?;
    result.cancelled = state.cancelled;
    result.timed_out = state.timed_out;
    let status = process.wait().context("reap controlled process")?;
    result.exit_code = status.code();
    let counts = scan_results(File::open(&spec.stdout).context("open stdout for test results")?)?;
    result.tests_passed = counts.passed;
    result.tests_failed = counts.failed;
    result.tests_ignored = counts.ignored;
    Ok(result)
}

#[derive(Default)]
struct ExecutionState {
    cancelled: bool,
    timed_out: bool,
}

fn drive_process(
    process: &mut controlled::Process,
    cancelled: &AtomicBool,
    timeout: Duration,
) -> Result<ExecutionState> {
    let mut result = ExecutionState::default();
    let start = Instant::now();
    let primary = (|| -> Result<ExecutionState> {
        loop {
            if cancelled.load(Ordering::Relaxed) {
                result.cancelled = true;
                break;
            }
            if process.poll_exit().context("poll controlled process")? {
                break;
            }
            if start.elapsed() >= timeout {
                result.timed_out = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(result)
    })();
    // Even successful parents may leave children running and holding log or target
    // files.
    let identity = process.stop_identity();
    finish_after_stop(primary, process.stop_tree(), identity)
}

#[derive(Default)]
struct Counts {
    passed: u64,
    failed: u64,
    ignored: u64,
}

fn scan_results(file: File) -> Result<Counts> {
    let mut reader = BufReader::new(file);
    let mut counts = Counts::default();
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let buffer = reader.fill_buf().context("read test result log")?;
        if buffer.is_empty() {
            if !oversized {
                add_summary(&line, &mut counts)?;
            }
            break;
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let length = newline.map_or(buffer.len(), |index| index + 1);
        if !oversized && line.len() + length <= 16 * 1024 {
            line.extend_from_slice(&buffer[..length]);
        } else {
            oversized = true;
            line.clear();
        }
        reader.consume(length);
        if newline.is_some() {
            if !oversized {
                add_summary(&line, &mut counts)?;
            }
            line.clear();
            oversized = false;
        }
    }
    Ok(counts)
}

fn add_summary(line: &[u8], total: &mut Counts) -> Result<()> {
    let Ok(line) = std::str::from_utf8(line) else {
        return Ok(());
    };
    let Some(summary) = line.trim_end().strip_prefix("test result: ") else {
        return Ok(());
    };
    let Some((status, fields)) = summary.split_once(". ") else {
        return Ok(());
    };
    if status != "ok" && status != "FAILED" {
        return Ok(());
    }
    let mut fields = fields.split(';');
    let mut counter = |suffix: &str| fields.next()?.trim().strip_suffix(suffix)?.parse::<u64>().ok();
    let (Some(passed), Some(failed), Some(ignored)) = (counter(" passed"), counter(" failed"), counter(" ignored"))
    else {
        return Ok(());
    };
    if counter(" measured").is_none() || counter(" filtered out").is_none() {
        return Ok(());
    }
    let Some(duration) = fields
        .next()
        .and_then(|field| field.trim().strip_prefix("finished in "))
        .and_then(|duration| duration.strip_suffix('s'))
        .and_then(|duration| duration.parse::<f64>().ok())
    else {
        return Ok(());
    };
    if !duration.is_finite() || duration < 0.0 || fields.next().is_some() {
        return Ok(());
    }
    total.passed = total
        .passed
        .checked_add(passed)
        .context("libtest passed count overflow")?;
    total.failed = total
        .failed
        .checked_add(failed)
        .context("libtest failed count overflow")?;
    total.ignored = total
        .ignored
        .checked_add(ignored)
        .context("libtest ignored count overflow")?;
    Ok(())
}

#[cfg(unix)]
mod controlled {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    use std::process::Child;
    use std::process::Command;
    use std::process::Stdio;
    use std::ptr::null_mut;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

    use anyhow::Context;
    use anyhow::Result;

    use super::MemberIdentity;
    use super::ProcessSpec;
    use super::StopIdentity;

    pub(super) struct Process {
        pub(super) child: Child,
        group: libc::pid_t,
        lifetime: Option<UnixStream>,
        acknowledgement: UnixStream,
        identity: Option<StopIdentity>,
        stopped: bool,
    }

    pub(super) fn retry_stop(identity: &StopIdentity) -> Result<bool> {
        let StopIdentity::Unix {
            host,
            boot,
            group,
            leader,
            anchor,
        } = identity
        else {
            return Ok(false);
        };
        if *host != host_identity()? {
            return Ok(false);
        }
        if *boot != boot_identity()? {
            return Ok(true);
        }
        if *group <= 1 || leader.pid != *group || anchor.pid <= 1 || anchor.pid == *group {
            return Ok(false);
        }
        if !group_has_live_members(*group)? {
            return Ok(true);
        }
        // Recovery does not own a continuous anchor lifetime: observation never
        // authorizes a later numeric killpg after the original watcher could
        // have reaped that anchor.
        Ok(false)
    }

    struct MemberState {
        start: String,
        group: i32,
    }

    #[cfg(any(target_os = "linux", target_os = "android", test))]
    fn process_disappeared(error: &io::Error) -> bool {
        error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
    }

    #[cfg(any(target_os = "linux", target_os = "android", test))]
    fn zombie_leader_has_threads(fields: &[&str]) -> Result<bool> {
        if !matches!(fields.first(), Some(&"Z" | &"X" | &"x")) {
            return Ok(false);
        }
        let threads: u32 = fields.get(17).context("missing process thread count")?.parse()?;
        // Linux can hide /proc/PID/task once the main thread exits even while siblings
        // live.
        Ok(threads > 1)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn member_state(pid: i32) -> Result<Option<MemberState>> {
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(error) if process_disappeared(&error) => return Ok(None),
            Err(error) => return Err(error).context("read recorded process identity"),
        };
        let (_, fields) = stat.rsplit_once(')').context("malformed recorded process stat")?;
        let fields: Vec<_> = fields.split_whitespace().collect();
        let group = fields.get(2).context("missing recorded process group")?.parse()?;
        let start = fields
            .get(19)
            .context("missing recorded process start time")?
            .to_string();
        Ok(Some(MemberState { start, group }))
    }

    #[cfg(target_os = "macos")]
    fn member_state(pid: i32) -> Result<Option<MemberState>> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let expected = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let used = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                expected,
            )
        };
        if used == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error).context("read recorded macOS process identity");
        }
        anyhow::ensure!(used == expected, "incomplete recorded macOS process identity");
        Ok(Some(MemberState {
            start: format!("{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec),
            group: info.pbi_pgid as i32,
        }))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn host_identity() -> Result<String> {
        Ok(std::fs::read_to_string("/etc/machine-id")
            .context("read process owner machine identity")?
            .trim()
            .to_owned())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn boot_identity() -> Result<String> {
        Ok(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .context("read process owner boot identity")?
            .trim()
            .to_owned())
    }

    #[cfg(target_os = "macos")]
    fn host_identity() -> Result<String> {
        let mut uuid = [0_u8; 16];
        let timeout = libc::timespec { tv_sec: 1, tv_nsec: 0 };
        if unsafe { libc::gethostuuid(uuid.as_mut_ptr(), &timeout) } != 0 {
            return Err(io::Error::last_os_error()).context("read macOS process owner UUID");
        }
        Ok(uuid.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    #[cfg(target_os = "macos")]
    fn boot_identity() -> Result<String> {
        let mut bytes = [0_u8; 128];
        let mut size = bytes.len();
        if unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error()).context("read macOS process owner boot UUID");
        }
        Ok(std::str::from_utf8(&bytes[..size])?.trim_end_matches('\0').to_owned())
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    fn member_state(_pid: i32) -> Result<Option<MemberState>> {
        anyhow::bail!("process identity unsupported on this Unix")
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    fn host_identity() -> Result<String> {
        anyhow::bail!("machine identity unsupported on this Unix")
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    fn boot_identity() -> Result<String> {
        anyhow::bail!("boot identity unsupported on this Unix")
    }

    fn recorded_identity(group: i32, anchor: i32) -> Result<StopIdentity> {
        let leader = member_state(group)?.context("process leader identity disappeared")?;
        let member = member_state(anchor)?.context("process anchor identity disappeared")?;
        anyhow::ensure!(
            member.group == group && leader.group == group,
            "process group identity changed during startup"
        );
        let host = host_identity()?;
        let boot = boot_identity()?;
        anyhow::ensure!(!host.is_empty() && !boot.is_empty(), "empty process owner identity");
        Ok(StopIdentity::Unix {
            host,
            boot,
            group,
            leader: MemberIdentity {
                pid: group,
                start: leader.start,
            },
            anchor: MemberIdentity {
                pid: anchor,
                start: member.start,
            },
        })
    }

    pub(super) fn wait_group_stopped(group: libc::pid_t, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if !group_has_live_members(group)? {
                return Ok(());
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "live process group {group} still holds resources; termination pending"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn group_has_live_members(group: libc::pid_t) -> Result<bool> {
        for entry in std::fs::read_dir("/proc").context("enumerate processes before cleanup")? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
                continue;
            }
            let stat = match std::fs::read_to_string(entry.path().join("stat")) {
                Ok(stat) => stat,
                Err(error) if process_disappeared(&error) => continue,
                Err(error) => return Err(error).context("inspect process group before cleanup"),
            };
            let Some((_, fields)) = stat.rsplit_once(')') else {
                anyhow::bail!("malformed process stat");
            };
            let fields: Vec<_> = fields.split_whitespace().collect();
            if fields.get(2).and_then(|field| field.parse::<i32>().ok()) != Some(group) {
                continue;
            }
            if zombie_leader_has_threads(&fields)? {
                return Ok(true);
            }
            // A zombie main thread can still have live sibling threads owning file handles.
            let tasks = match std::fs::read_dir(entry.path().join("task")) {
                Ok(tasks) => tasks,
                Err(error) if process_disappeared(&error) => continue,
                Err(error) => {
                    return Err(error).context("enumerate process threads before cleanup");
                }
            };
            for task in tasks {
                let stat = match std::fs::read_to_string(task?.path().join("stat")) {
                    Ok(stat) => stat,
                    Err(error) if process_disappeared(&error) => continue,
                    Err(error) => {
                        return Err(error).context("inspect process thread before cleanup");
                    }
                };
                let state = stat
                    .rsplit_once(')')
                    .and_then(|(_, fields)| fields.split_whitespace().next());
                if !matches!(state, Some("Z" | "X" | "x")) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    #[cfg(target_os = "macos")]
    fn group_has_live_members(group: libc::pid_t) -> Result<bool> {
        const PROC_PGRP_ONLY: u32 = 2;
        let mut capacity = 128;
        loop {
            let mut pids = vec![0_i32; capacity];
            let bytes = (pids.len() * std::mem::size_of::<i32>()) as i32;
            // SAFETY: the output buffer is properly aligned and sized for the requested PID
            // list.
            let used = unsafe { libc::proc_listpids(PROC_PGRP_ONLY, group as u32, pids.as_mut_ptr().cast(), bytes) };
            let known_absent = used == 0
                && unsafe { libc::kill(-group, 0) } < 0
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            let count = super::observed_pid_count(used, known_absent)?;
            if used >= bytes {
                capacity *= 2;
                anyhow::ensure!(capacity <= 1_048_576, "process group exceeds inspection limit");
                continue;
            }
            pids.truncate(count);
            for pid in pids.into_iter().filter(|pid| *pid > 0) {
                let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
                let expected = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
                // SAFETY: info is a correctly sized initialized output structure.
                let used = unsafe {
                    libc::proc_pidinfo(
                        pid,
                        libc::PROC_PIDTBSDINFO,
                        0,
                        (&mut info as *mut libc::proc_bsdinfo).cast(),
                        expected,
                    )
                };
                if used == 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ESRCH) {
                        continue;
                    }
                    return Err(error).context("inspect process before cleanup");
                }
                anyhow::ensure!(used == expected, "incomplete process state before cleanup");
                if info.pbi_pgid == group as u32 && info.pbi_status != libc::SZOMB {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    fn group_has_live_members(_group: libc::pid_t) -> Result<bool> {
        anyhow::bail!("process group termination inspection unsupported on this Unix")
    }

    impl Process {
        pub(super) fn stop_identity(&self) -> Option<StopIdentity> {
            self.identity.clone()
        }
        pub(super) fn spawn_spec(
            spec: &ProcessSpec,
            stdin: Option<File>,
            stdout: File,
            stderr: File,
            raw_tail: Option<&OsStr>,
        ) -> Result<Self> {
            anyhow::ensure!(raw_tail.is_none(), "trusted raw scripts are only supported on Windows");
            let mut command = Command::new(&spec.argv[0]);
            command
                .args(&spec.argv[1..])
                .current_dir(&spec.cwd)
                .envs(&spec.env)
                .stdin(stdin.map_or_else(Stdio::null, Stdio::from))
                .stdout(stdout)
                .stderr(stderr);
            Self::spawn(&mut command)
        }
        pub(super) fn poll_exit(&mut self) -> Result<bool> {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: only observes our unreaped child, keeping its PID/PGID reserved.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.group as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error()).context("observe process without reaping");
            }
            Ok(unsafe { info.si_pid() } != 0)
        }
        pub(super) fn wait(&mut self) -> Result<std::process::ExitStatus> {
            Ok(self.child.wait()?)
        }
        pub(super) fn spawn(command: &mut Command) -> Result<Self> {
            // Standard UnixStream creation sets CLOEXEC, including for concurrent spawns.
            let (monitor, lifetime) = UnixStream::pair().context("create process lifetime channel")?;
            let (acknowledgement, ready) = UnixStream::pair().context("create watchdog acknowledgement channel")?;
            acknowledgement.set_read_timeout(Some(Duration::from_secs(5)))?;
            let monitor_fd = monitor.as_raw_fd();
            let lifetime_fd = lifetime.as_raw_fd();
            let acknowledgement_fd = acknowledgement.as_raw_fd();
            let ready_fd = ready.as_raw_fd();
            // Evaluated before fork; the fallback close loop must not query Rust state.
            // SAFETY: sysconf has no pointer arguments or side effects on process
            // descriptors.
            let descriptor_limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
            anyhow::ensure!(
                descriptor_limit > 0 && descriptor_limit <= i32::MAX as libc::c_long,
                "cannot determine watchdog descriptor limit"
            );
            // SAFETY: the closure uses only async-signal-safe libc operations and inline
            // values. The watcher closes the std spawn error pipe before
            // signaling readiness, so spawn cannot hang waiting for an
            // inherited writer. User code starts only after readiness.
            unsafe {
                command.process_group(0).pre_exec(move || {
                    if libc::setpgid(0, 0) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let group = libc::getpid();
                    libc::close(lifetime_fd);
                    let watcher = libc::fork();
                    if watcher < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if watcher == 0 {
                        // Detach the final watcher from the target's waitpid(-1) child set.
                        let detached = libc::fork();
                        if detached < 0 {
                            let failure = 0_u8;
                            libc::write(ready_fd, (&failure as *const u8).cast(), 1);
                            libc::_exit(125);
                        }
                        if detached > 0 {
                            libc::_exit(0);
                        }
                        watchdog(monitor_fd, ready_fd, group, descriptor_limit as i32);
                    }
                    libc::close(monitor_fd);
                    libc::close(ready_fd);
                    let mut message = 0_u8;
                    let received = libc::read(acknowledgement_fd, (&mut message as *mut u8).cast(), 1);
                    libc::close(acknowledgement_fd);
                    let mut intermediate_status = 0;
                    let reaped = libc::waitpid(watcher, &mut intermediate_status, 0);
                    if received != 1 || message != 1 || reaped != watcher || intermediate_status != 0 {
                        return Err(io::Error::from_raw_os_error(libc::EIO));
                    }
                    Ok(())
                });
            }
            let child = command.spawn()?;
            let group = child.id() as libc::pid_t;
            let mut process = Self {
                child,
                group,
                lifetime: Some(lifetime),
                acknowledgement,
                identity: None,
                stopped: false,
            };
            let mut anchor = [0_u8; 4];
            process
                .complete_anchor_handshake(&mut anchor)
                .map_err(|error| error.context(super::ProcessStarted))?;
            Ok(process)
        }

        fn complete_anchor_handshake(&mut self, anchor: &mut [u8; 4]) -> Result<()> {
            let read = self
                .acknowledgement
                .read_exact(anchor)
                .context("read detached group anchor identity");
            if let Err(error) = read {
                let identity = self.stop_identity();
                return super::finish_after_stop(Err(error), self.stop_tree(), identity);
            }
            let anchor = i32::from_ne_bytes(*anchor);
            self.identity = recorded_identity(self.group, anchor).ok();
            Ok(())
        }

        pub(super) fn stop_tree(&mut self) -> Result<()> {
            if self.stopped {
                return Ok(());
            }
            // Closing the coordinator's only endpoint also works when it dies under
            // SIGKILL.
            drop(self.lifetime.take());
            let mut message = [0_u8];
            let acknowledgement = self.acknowledgement.read_exact(&mut message);
            if acknowledgement.is_ok() && message == [1] {
                wait_group_stopped(self.group, Duration::from_secs(5))?;
                self.stopped = true;
                return Ok(());
            }
            // A missing/dead watchdog is an error, but still stop the controlled tree.
            self.kill_group()?;
            acknowledgement.context("wait for process watchdog termination acknowledgement")?;
            anyhow::ensure!(message == [2], "invalid process watchdog acknowledgement");
            wait_group_stopped(self.group, Duration::from_secs(5))?;
            self.stopped = true;
            Ok(())
        }

        fn kill_group(&self) -> Result<()> {
            // SAFETY: the negative id names only the dedicated child process group.
            if unsafe { libc::kill(-self.group, libc::SIGKILL) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error).context("kill child process group");
                }
            }
            Ok(())
        }
    }

    /// This runs after fork in a multithreaded process: libc calls and plain
    /// stack data only.
    unsafe fn watchdog(monitor: i32, ready: i32, group: libc::pid_t, descriptor_limit: i32) -> ! {
        // SAFETY: all pointers refer to initialized stack data; every operation below
        // is async-signal-safe. No Rust IO, allocation, destructors, locks, or
        // unwinding occur.
        unsafe {
            let mut signals: libc::sigset_t = std::mem::zeroed();
            libc::sigfillset(&mut signals);
            libc::sigprocmask(libc::SIG_SETMASK, &signals, null_mut());
            // The anchor reserves the target PGID even if init reaps the leader after
            // controller death.
            let mut anchor_lifetime = [0_i32; 2];
            if libc::pipe(anchor_lifetime.as_mut_ptr()) != 0 {
                libc::_exit(125);
            }
            let anchor = libc::fork();
            if anchor < 0 {
                libc::_exit(125);
            }
            if anchor == 0 {
                close_inherited_descriptors(anchor_lifetime[0], -1, -1, descriptor_limit);
                let mut byte = 0_u8;
                while libc::read(anchor_lifetime[0], (&mut byte as *mut u8).cast(), 1) > 0 {}
                libc::kill(-group, libc::SIGKILL);
                libc::_exit(0);
            }
            if libc::setsid() < 0 {
                let failure = 0_u8;
                libc::write(ready, (&failure as *const u8).cast(), 1);
                libc::_exit(125);
            }
            close_inherited_descriptors(monitor, ready, anchor_lifetime[1], descriptor_limit);
            let success = 1_u8;
            if libc::write(ready, (&success as *const u8).cast(), 1) != 1 {
                libc::kill(-group, libc::SIGKILL);
                libc::_exit(125);
            }
            if libc::write(
                ready,
                (&anchor as *const libc::pid_t).cast(),
                std::mem::size_of::<libc::pid_t>(),
            ) != 4
            {
                libc::kill(-group, libc::SIGKILL);
                libc::_exit(125);
            }
            let mut byte = 0_u8;
            while libc::read(monitor, (&mut byte as *mut u8).cast(), 1) > 0 {}
            let response = if libc::kill(-group, libc::SIGKILL) == 0 {
                1_u8
            } else {
                2_u8
            };
            // No later killpg is permitted after releasing the anchor's group identity.
            let mut status = 0;
            libc::waitpid(anchor, &mut status, 0);
            libc::write(ready, (&response as *const u8).cast(), 1);
            libc::_exit(0);
        }
    }

    unsafe fn close_inherited_descriptors(monitor: i32, ready: i32, anchor: i32, descriptor_limit: i32) {
        // SAFETY: preserve exactly the two watchdog channels; all other inherited
        // descriptors, including stdout/stderr and the Rust exec handshake
        // writer, belong to other processes.
        unsafe {
            #[cfg(target_os = "linux")]
            {
                let mut start = 0_u32;
                let mut success = true;
                loop {
                    let mut next = u32::MAX;
                    for kept in [monitor, ready, anchor] {
                        if kept >= 0 && kept as u32 >= start {
                            next = next.min(kept as u32);
                        }
                    }
                    if next > start && libc::syscall(libc::SYS_close_range, start, next - 1, 0_u32) != 0 {
                        success = false;
                    }
                    if next == u32::MAX {
                        if libc::syscall(libc::SYS_close_range, u32::MAX, u32::MAX, 0_u32) != 0 {
                            success = false;
                        }
                        break;
                    }
                    start = next + 1;
                }
                if success {
                    return;
                }
            }
            for descriptor in 0..descriptor_limit {
                if descriptor != monitor && descriptor != ready && descriptor != anchor {
                    libc::close(descriptor);
                }
            }
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.stop_tree();
            let _ = self.child.kill();
            if self.stopped {
                let _ = self.child.wait();
            } else {
                let _ = self.child.try_wait();
            }
        }
    }

    #[cfg(test)]
    mod failure_tests {
        use std::net::Shutdown;
        use std::process::Command;
        use std::process::Stdio;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        use super::Process;
        use crate::process::drive_process;
        use crate::process::stop_identity;
        use crate::process::tree_stop_failed;

        fn fixture(mode: &str) -> (tempfile::TempDir, Process) {
            let root = tempfile::tempdir().expect("create process failure fixture");
            let mut command = Command::new(std::env::current_exe().expect("locate fixture executable"));
            command
                .args(["--exact", "process::tests::process_fixture", "--ignored", "--nocapture"])
                .env("PLATFORM_PROCESS_MODE", mode)
                .env("PLATFORM_PROCESS_ROOT", root.path())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            (
                root,
                Process::spawn(&mut command).expect("spawn real process for error path"),
            )
        }

        #[test]
        fn test_poll_error_preserves_primary_and_typed_stop_failure() {
            let (_root, mut process) = fixture("zero");
            process.child.wait().expect("force ECHILD after external reaping");
            process
                .acknowledgement
                .shutdown(Shutdown::Read)
                .expect("force acknowledgement failure");
            let error = drive_process(&mut process, &AtomicBool::new(false), Duration::from_secs(1))
                .err()
                .expect("poll must fail");
            assert!(
                tree_stop_failed(&error),
                "failed finally stop must retain its typed guard"
            );
            assert!(
                stop_identity(&error).is_some(),
                "available identity must survive poll failure"
            );
            let message = format!("{error:#}");
            assert!(message.contains("observe process without reaping"));
            assert!(message.contains("acknowledgement"));
        }

        #[test]
        fn test_handshake_error_preserves_primary_and_typed_stop_failure() {
            let (_root, mut process) = fixture("stream");
            process
                .acknowledgement
                .shutdown(Shutdown::Read)
                .expect("force handshake and stop acknowledgement failures");
            let error = process
                .complete_anchor_handshake(&mut [0_u8; 4])
                .expect_err("handshake must fail");
            assert!(
                tree_stop_failed(&error),
                "spawned target must not escape failed-stop guard"
            );
            assert!(stop_identity(&error).is_some());
            assert!(!crate::process::process_not_started(&error));
            assert!(format!("{error:#}").contains("read detached group anchor identity"));
        }

        #[test]
        fn test_proc_esrch_means_disappeared_without_hiding_permission_errors() {
            assert!(super::process_disappeared(&std::io::Error::from_raw_os_error(
                libc::ESRCH
            )));
            assert!(super::process_disappeared(&std::io::Error::from_raw_os_error(
                libc::ENOENT
            )));
            assert!(!super::process_disappeared(&std::io::Error::from_raw_os_error(
                libc::EACCES
            )));
        }

        #[test]
        fn test_zombie_leader_does_not_hide_remaining_threads() {
            let mut fields = vec!["0"; 20];
            fields[0] = "Z";
            fields[17] = "2";
            assert!(super::zombie_leader_has_threads(&fields).expect("parse zombie thread count"));
            fields[17] = "1";
            assert!(!super::zombie_leader_has_threads(&fields).expect("sole zombie released resources"));
        }
    }
}

#[cfg(windows)]
use windows_process as controlled;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::io::Read;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

    use tempfile::TempDir;
    use tempfile::tempdir;

    use super::ProcessSpec;
    use super::run;
    use super::run_with_input;
    use super::scan_results;

    fn spec(mode: &str) -> (TempDir, ProcessSpec) {
        let directory = tempdir().expect("create test process directory");
        let spec = ProcessSpec {
            argv: vec![
                std::env::current_exe()
                    .expect("locate test executable")
                    .to_string_lossy()
                    .into_owned(),
                "--exact".into(),
                "process::tests::process_fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
            cwd: directory.path().into(),
            env: BTreeMap::from([
                (OsString::from("PLATFORM_PROCESS_MODE"), OsString::from(mode)),
                (
                    OsString::from("PLATFORM_PROCESS_ROOT"),
                    directory.path().as_os_str().into(),
                ),
            ]),
            timeout: Duration::from_secs(5),
            stdout: directory.path().join("stdout.log"),
            stderr: directory.path().join("stderr.log"),
        };
        (directory, spec)
    }

    #[test]
    #[ignore = "executed in a controlled child process"]
    fn process_fixture() {
        let mode = std::env::var("PLATFORM_PROCESS_MODE").expect("controlled fixture mode");
        let root = std::env::var_os("PLATFORM_PROCESS_ROOT").expect("controlled fixture root");
        let root = Path::new(&root);
        match mode.as_str() {
            #[cfg(unix)]
            "wait-any" => {
                let _child = Command::new(std::env::current_exe().expect("locate test executable"))
                    .args(["--exact", "process::tests::process_fixture", "--ignored", "--nocapture"])
                    .env("PLATFORM_PROCESS_MODE", "zero")
                    .spawn()
                    .expect("spawn owned child");
                // A program is allowed to reap all of its own children without hidden waiters.
                let mut status = 0;
                while unsafe { libc::waitpid(-1, &mut status, 0) } > 0 {}
                std::process::exit(0);
            }
            "input" => {
                let mut input = Vec::new();
                std::io::stdin().read_to_end(&mut input).expect("read stdin to EOF");
                fs::write(root.join("input.bin"), input).expect("record stdin");
                std::process::exit(0);
            }
            "counts" => {
                println!(
                    "test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s"
                );
                println!(
                    "test result: FAILED. 1 passed; 2 failed; 3 ignored; 0 measured; 0 filtered out; finished in 0.00s"
                );
                std::process::exit(101);
            }
            "exit-nine" => {
                eprintln!("controlled failure");
                std::process::exit(9);
            }
            "zero" | "ignored" => {
                let ignored = if mode == "ignored" { 4 } else { 0 };
                println!(
                    "test result: ok. 0 passed; 0 failed; {ignored} ignored; 0 measured; 0 filtered out; finished in 0.00s"
                );
                std::process::exit(0);
            }
            "compile-error" => {
                eprintln!("error: could not compile fixture");
                std::process::exit(101);
            }
            "argv" => {
                let actual: Vec<_> = std::env::args().skip(5).collect();
                fs::write(
                    root.join("argv.json"),
                    serde_json::to_vec(&actual).expect("serialize argv"),
                )
                .expect("write argv");
                std::process::exit(0);
            }
            "leaf" => loop {
                fs::write(root.join("heartbeat"), b"alive").expect("write heartbeat");
                thread::sleep(Duration::from_millis(20));
            },
            "tree" | "orphan" => {
                fs::write(root.join("tree.pid"), std::process::id().to_string()).expect("record group leader");
                let mut child = Command::new(std::env::current_exe().expect("locate test executable"))
                    .args(["--exact", "process::tests::process_fixture", "--ignored", "--nocapture"])
                    .env("PLATFORM_PROCESS_MODE", "leaf")
                    .spawn()
                    .expect("spawn real descendant");
                fs::write(root.join("child.pid"), child.id().to_string()).expect("record descendant");
                println!("descendant started");
                if mode == "orphan" {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !root.join("heartbeat").exists() && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(10));
                    }
                    assert!(
                        root.join("heartbeat").exists(),
                        "real descendant must run before its parent exits"
                    );
                    std::process::exit(0);
                }
                let _ = child.wait();
            }
            "controller" => {
                let child_spec = ProcessSpec {
                    argv: vec![
                        std::env::current_exe()
                            .expect("locate test executable")
                            .to_string_lossy()
                            .into_owned(),
                        "--exact".into(),
                        "process::tests::process_fixture".into(),
                        "--ignored".into(),
                        "--nocapture".into(),
                    ],
                    cwd: root.into(),
                    env: BTreeMap::from([(OsString::from("PLATFORM_PROCESS_MODE"), OsString::from("tree"))]),
                    timeout: Duration::from_secs(30),
                    stdout: root.join("nested.stdout"),
                    stderr: root.join("nested.stderr"),
                };
                let _ = run(&child_spec, &AtomicBool::new(false));
                std::process::exit(0);
            }
            "stream" => {
                println!("stream-before-exit");
                loop {
                    thread::sleep(Duration::from_millis(20));
                }
            }
            other => panic!("unexpected fixture mode: {other}"),
        }
    }

    #[test]
    fn test_counts_real_libtest_summaries() {
        let (_directory, spec) = spec("counts");
        let result = run(&spec, &AtomicBool::new(false)).expect("run counting fixture");
        assert_eq!(
            (result.tests_passed, result.tests_failed, result.tests_ignored),
            (3, 2, 4)
        );
        assert_eq!(result.exit_code, Some(101));
    }

    #[test]
    fn test_stop_error_retains_identity_through_outer_context() {
        let identity = super::StopIdentity::Windows {
            host: "fixture-host".into(),
            job: "fixture-job".into(),
        };
        let error = anyhow::anyhow!("termination pending")
            .context(super::TreeStopFailed {
                identity: Some(identity),
            })
            .context("outer backend context");
        assert!(super::tree_stop_failed(&error));
        let identity = super::stop_identity(&error).expect("typed stop identity survives contexts");
        let encoded = serde_json::to_value(identity).expect("serialize retained identity");
        assert_eq!(encoded["job"], "fixture-job");
        assert!(!super::tree_stop_failed(&anyhow::anyhow!("unrelated execution error")));
    }

    #[cfg(unix)]
    #[test]
    fn test_waitpid_any_does_not_wait_on_watchdog() {
        let (_directory, mut spec) = spec("wait-any");
        spec.timeout = Duration::from_millis(500);
        let result = run(&spec, &AtomicBool::new(false)).expect("run wait-any fixture");
        assert!(!result.timed_out, "target must not inherit the watchdog as its child");
        assert_eq!(result.exit_code, Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn test_exit_observation_keeps_group_leader_waitable() {
        let (_directory, spec) = spec("zero");
        let mut command = Command::new(&spec.argv[0]);
        command
            .args(&spec.argv[1..])
            .envs(&spec.env)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut process = super::controlled::Process::spawn(&mut command).expect("spawn pinned leader");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !process.poll_exit().expect("observe exit") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: inspect our own child without reaping it, preserving its PID and PGID
        // identity.
        let observed = unsafe {
            libc::waitid(
                libc::P_PID,
                process.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        assert_eq!(observed, 0, "polling must leave the leader available for final wait");
        assert_eq!(unsafe { info.si_pid() }, process.child.id() as libc::pid_t);
    }

    #[cfg(unix)]
    #[test]
    fn test_unconfirmed_live_group_is_an_explicit_error() {
        use std::os::unix::process::CommandExt;
        let (_directory, spec) = spec("stream");
        let mut child = Command::new(&spec.argv[0])
            .args(&spec.argv[1..])
            .envs(&spec.env)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn live group");
        let result = super::controlled::wait_group_stopped(child.id() as libc::pid_t, Duration::ZERO);
        child.kill().expect("stop this test's live group");
        child.wait().expect("reap this test's live group");
        assert!(result.is_err(), "live group must be pending, never reported stopped");
    }

    #[cfg(unix)]
    #[test]
    fn test_retry_rejects_unknown_live_group_identity() {
        use std::os::unix::process::CommandExt;
        let (_directory, spec) = spec("stream");
        let mut child = Command::new(&spec.argv[0])
            .args(&spec.argv[1..])
            .envs(&spec.env)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn unrelated live group");
        let identity = super::StopIdentity::Unix {
            host: "unknown-host".into(),
            boot: "unknown-boot".into(),
            group: child.id() as i32,
            leader: super::MemberIdentity {
                pid: child.id() as i32,
                start: "unverified".into(),
            },
            anchor: super::MemberIdentity {
                pid: child.id() as i32,
                start: "unverified".into(),
            },
        };
        let result = super::retry_stop(&identity).expect("refuse unknown ownership");
        child.kill().expect("stop unrelated test group");
        child.wait().expect("reap unrelated test group");
        assert!(!result, "unknown identity cannot prove this live group stopped");
    }

    #[cfg(unix)]
    #[test]
    fn test_stop_identity_survives_serialization_and_retries_owned_group() {
        let (_directory, spec) = spec("stream");
        let mut command = Command::new(&spec.argv[0]);
        command
            .args(&spec.argv[1..])
            .envs(&spec.env)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut process = super::controlled::Process::spawn(&mut command).expect("spawn recoverable group");
        let identity = process
            .stop_identity()
            .expect("capture leader and anchor startup identity");
        let encoded = serde_json::to_vec(&identity).expect("serialize stop identity");
        let identity = serde_json::from_slice(&encoded).expect("deserialize stop identity");
        assert!(!super::retry_stop(&identity).expect("observe owned live group without killing"));
        assert!(!process.poll_exit().expect("observation must not kill the owned group"));
        process.stop_tree().expect("confirm owned group stopped");
        process.wait().expect("reap owned group leader");
        assert!(super::retry_stop(&identity).expect("already stopped group is idempotent"));
    }

    #[test]
    fn test_input_preserves_bytes_and_reaches_eof() {
        let (directory, spec) = spec("input");
        let input = b"{\"argv\":[\"a b\",\"$()\"]}\n\0\xff";
        let result = run_with_input(&spec, &AtomicBool::new(false), input).expect("run input fixture");
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(
            fs::read(directory.path().join("input.bin")).expect("read saved stdin"),
            input
        );
    }

    #[test]
    fn test_parser_rejects_incomplete_summaries() {
        let directory = tempdir().expect("create parser test directory");
        let log = directory.path().join("log");
        fs::write(
            &log,
            b"test result: ok. 9 passed; 0 failed; 0 ignored; compiler output\n",
        )
        .expect("write malformed log");
        let counts = scan_results(fs::File::open(log).expect("open malformed log")).expect("scan malformed log");
        assert_eq!(counts.passed, 0);
    }

    #[test]
    fn test_parser_skips_oversized_and_non_utf8_lines() {
        let directory = tempdir().expect("create parser test directory");
        let log = directory.path().join("log");
        let mut contents = vec![b'x'; 128 * 1024];
        contents.extend_from_slice(b"\n\xff\ncompiler test result: ok. 99 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n");
        contents.extend_from_slice(
            b"test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s",
        );
        fs::write(&log, contents).expect("write unstructured log");
        let counts = scan_results(fs::File::open(log).expect("open unstructured log")).expect("scan bounded lines");
        assert_eq!((counts.passed, counts.failed, counts.ignored), (2, 0, 1));
    }

    #[test]
    fn test_preexisting_cancellation_does_not_spawn() {
        let (directory, spec) = spec("input");
        let result = run(&spec, &AtomicBool::new(true)).expect("return preexisting cancellation");
        assert!(result.cancelled);
        assert_eq!(result.exit_code, None);
        assert!(!directory.path().join("input.bin").exists());
    }

    #[test]
    fn test_log_failure_prevents_spawn() {
        let (directory, mut spec) = spec("input");
        spec.stderr = directory.path().into();
        let error = run(&spec, &AtomicBool::new(false)).expect_err("stderr directory must be rejected");
        assert!(error.to_string().contains("create process stderr log"));
        assert!(super::process_not_started(&error));
        assert!(!directory.path().join("input.bin").exists());
    }

    #[test]
    fn test_spawn_failure_retains_context() {
        let (_directory, mut spec) = spec("input");
        spec.argv[0] = spec.cwd.join("missing-executable").to_string_lossy().into_owned();
        let error = run(&spec, &AtomicBool::new(false)).expect_err("missing executable must fail");
        assert!(error.to_string().contains("spawn controlled process"));
        assert!(super::process_not_started(&error));
    }

    #[test]
    fn test_zero_pid_list_requires_independent_absence_proof() {
        assert!(
            super::observed_pid_count(0, false).is_err(),
            "zero can mean a failed macOS system call"
        );
        assert_eq!(
            super::observed_pid_count(0, true).expect("independently absent group"),
            0
        );
    }

    #[test]
    fn test_input_preparation_error_is_before_spawn() {
        let (directory, mut spec) = spec("input");
        spec.stdout = directory.path().join("missing-parent/stdout");
        let error =
            run_with_input(&spec, &AtomicBool::new(false), b"request").expect_err("missing input storage parent");
        assert!(super::process_not_started(&error));
        assert!(!directory.path().join("input.bin").exists());
    }

    #[test]
    fn test_unread_large_input_does_not_block_timeout() {
        let (_directory, mut spec) = spec("stream");
        spec.timeout = Duration::from_millis(150);
        let result = run_with_input(&spec, &AtomicBool::new(false), &vec![b'x'; 4 * 1024 * 1024])
            .expect("run unread input fixture");
        assert!(result.timed_out);
    }

    #[test]
    fn test_preserves_exit_nine_and_stderr() {
        let (_directory, spec) = spec("exit-nine");
        let result = run(&spec, &AtomicBool::new(false)).expect("run failed fixture");
        assert_eq!(result.exit_code, Some(9));
        assert!(
            fs::read_to_string(&spec.stderr)
                .expect("read stderr")
                .contains("controlled failure")
        );
    }

    #[test]
    fn test_zero_and_ignored_do_not_claim_execution() {
        for (mode, ignored) in [("zero", 0), ("ignored", 4), ("compile-error", 0)] {
            let (_directory, spec) = spec(mode);
            let result = run(&spec, &AtomicBool::new(false)).expect("run empty fixture");
            assert_eq!(result.tests_passed + result.tests_failed, 0, "{mode}");
            assert_eq!(result.tests_ignored, ignored, "{mode}");
            assert_eq!(result.exit_code, Some(if mode == "compile-error" { 101 } else { 0 }));
        }
    }

    #[test]
    fn test_argv_boundaries_are_preserved() {
        let (directory, mut spec) = spec("argv");
        let arguments = [
            "space value",
            "quote\"value",
            "$(touch should-not-exist)",
            "semi;colon",
            "中文",
            "",
        ];
        // libtest rejects unknown positional arguments, so pass them after its
        // separator.
        spec.argv.push("--".into());
        spec.argv.extend(arguments.iter().map(|argument| (*argument).into()));
        let result = run(&spec, &AtomicBool::new(false)).expect("run argv fixture");
        assert_eq!(result.exit_code, Some(0));
        let actual: Vec<String> =
            serde_json::from_slice(&fs::read(directory.path().join("argv.json")).expect("read argv"))
                .expect("parse argv");
        let expected: Vec<_> = std::iter::once("--").chain(arguments).map(str::to_owned).collect();
        assert_eq!(actual, expected);
    }

    fn assert_descendant_stopped(directory: &Path) {
        let heartbeat = directory.join("heartbeat");
        thread::sleep(Duration::from_millis(80));
        let first = fs::metadata(&heartbeat)
            .expect("descendant heartbeat existed")
            .modified()
            .expect("heartbeat timestamp");
        thread::sleep(Duration::from_millis(120));
        let last = fs::metadata(&heartbeat)
            .expect("heartbeat retained")
            .modified()
            .expect("heartbeat timestamp");
        assert_eq!(first, last, "ordinary descendant must stop writing before run returns");
    }

    #[test]
    fn test_timeout_stops_real_descendant() {
        let (directory, mut spec) = spec("tree");
        spec.timeout = Duration::from_millis(400);
        let result = run(&spec, &AtomicBool::new(false)).expect("run timed out tree");
        assert!(result.timed_out);
        assert!(!result.cancelled);
        assert_descendant_stopped(directory.path());
    }

    #[test]
    fn test_cancellation_stops_real_descendant() {
        let (directory, spec) = spec("tree");
        let cancelled = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(2);
                while !directory.path().join("heartbeat").exists() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                cancelled.store(true, Ordering::Relaxed);
            });
            let result = run(&spec, &cancelled).expect("run cancelled tree");
            assert!(result.cancelled);
            assert!(!result.timed_out);
        });
        assert_descendant_stopped(directory.path());
    }

    #[test]
    fn test_successful_parent_still_stops_descendant() {
        let (directory, spec) = spec("orphan");
        let result = run(&spec, &AtomicBool::new(false)).expect("run parent exiting early");
        assert_eq!(result.exit_code, Some(0));
        assert!(directory.path().join("child.pid").exists());
        assert_descendant_stopped(directory.path());
    }

    #[cfg(unix)]
    #[test]
    fn test_controller_sigkill_stops_real_grandchild() {
        let (directory, spec) = spec("controller");
        let mut controller = Command::new(&spec.argv[0])
            .args(&spec.argv[1..])
            .envs(&spec.env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn independent controller");
        let heartbeat = directory.path().join("heartbeat");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !heartbeat.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        controller.kill().expect("SIGKILL independent controller");
        controller.wait().expect("reap killed controller");
        assert!(
            heartbeat.exists(),
            "real grandchild must run before killing its controller"
        );
        thread::sleep(Duration::from_millis(100));
        let first = fs::metadata(&heartbeat)
            .expect("heartbeat existed")
            .modified()
            .expect("heartbeat time");
        thread::sleep(Duration::from_millis(150));
        let last = fs::metadata(&heartbeat)
            .expect("heartbeat retained")
            .modified()
            .expect("heartbeat time");
        // Always stop this test's own controlled tree, including the expected RED run.
        let group: i32 = fs::read_to_string(directory.path().join("tree.pid"))
            .expect("read controlled group identity")
            .parse()
            .expect("parse controlled group identity");
        // SAFETY: this id was produced by our own nested run, whose leader set its
        // process group.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
        assert_eq!(
            first, last,
            "controller SIGKILL must stop its ordinary test grandchildren"
        );
    }

    #[test]
    fn test_stdout_is_streamed_before_exit() {
        let (_directory, spec) = spec("stream");
        let cancelled = AtomicBool::new(false);
        thread::scope(|scope| {
            let running = scope.spawn(|| run(&spec, &cancelled));
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut observed = false;
            while Instant::now() < deadline {
                if fs::read_to_string(&spec.stdout).is_ok_and(|text| text.contains("stream-before-exit")) {
                    observed = true;
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            cancelled.store(true, Ordering::Relaxed);
            assert!(
                running
                    .join()
                    .expect("join process thread")
                    .expect("run streaming fixture")
                    .cancelled
            );
            assert!(observed, "stdout must reach disk while process remains running");
        });
    }
}
