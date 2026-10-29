// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Standalone, dependency-free Android process supervisor, embedded by the
//! host.

use std::env;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
    fn signal(number: i32, handler: usize) -> usize;
    fn waitid(kind: i32, pid: u32, info: *mut std::ffi::c_void, options: i32) -> i32;
    fn fork() -> i32;
    fn getrlimit(resource: i32, limit: *mut RLimit) -> i32;
    fn close(fd: i32) -> i32;
    fn read(fd: i32, buffer: *mut std::ffi::c_void, length: usize) -> isize;
    fn getpgrp() -> i32;
    fn _exit(status: i32) -> !;
}

fn main() {
    if let Err(error) = dispatch() {
        eprintln!("android supervisor: {error}");
        std::process::exit(70);
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 100 && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
fn hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn safe_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid("directory is a symlink or not a directory"));
    }
    Ok(())
}
fn locked(path: &Path, exclusive: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    // SAFETY: the descriptor belongs to a live File; LOCK_NB avoids waiting on
    // active runs.
    if unsafe { flock(file.as_raw_fd(), if exclusive { 2 | 4 } else { 1 | 4 }) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}
fn owner(root: &Path, token: &str) -> io::Result<()> {
    safe_directory(root)?;
    let marker = root.join(".owner");
    if fs::symlink_metadata(&marker)?.file_type().is_symlink() || fs::read_to_string(marker)? != token {
        return Err(invalid("owner token mismatch"));
    }
    Ok(())
}
fn dispatch() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 3 || args.len() > 4 {
        return Err(invalid("expected operation run-id token [invocation]"));
    }
    let op = &args[0];
    let id = &args[1];
    let token = &args[2];
    if !valid_id(id) || !hex(token, 64) {
        return Err(invalid("invalid run identity"));
    }
    #[cfg(supervisor_host_test)]
    let base = PathBuf::from(env::var_os("RS_INFRA_ANDROID_DEVICE_BASE").ok_or_else(|| invalid("test root missing"))?);
    #[cfg(not(supervisor_host_test))]
    let base = PathBuf::from("/data/local/tmp/rs-infra-platform");
    safe_directory(&base)?;
    let root = base.join(id);
    safe_directory(&root)?;
    if op == "init" {
        if root.join(".owner").exists() {
            owner(&root, token)?;
            File::open(root.join(".owner"))?.sync_all()?;
            File::open(&root)?.sync_all()?;
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join(".owner"))?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        File::open(&root)?.sync_all()?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        return Ok(());
    }
    owner(&root, token)?;
    let _root_lock = locked(&root.join(".lease"), op == "clean")?;
    if op == "clean" {
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && (entry.path().join("execution-intent").exists() || entry.path().join("group").exists())
                && !safe_completion(&entry.path())?
            {
                return Err(invalid(
                    "incomplete supervisor result; cleanup forbidden until process stop is confirmed",
                ));
            }
        }
        // remove_dir_all does not follow symlinks and uses fd-relative recursion on
        // Unix.
        return fs::remove_dir_all(&root);
    }
    let invocation = args
        .get(3)
        .filter(|v| hex(v, 32))
        .ok_or_else(|| invalid("invalid invocation identity"))?;
    let directory = root.join(invocation);
    if op == "prepare" {
        fs::create_dir(&directory)?;
        for name in ["work", "tmp", "lib"] {
            fs::create_dir(directory.join(name))?;
        }
        return Ok(());
    }
    safe_directory(&directory)?;
    if op == "cancel" {
        File::create(directory.join("cancel"))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match locked(&directory.join(".lease"), true) {
                Ok(_lease) => {
                    if (directory.join("execution-intent").exists() || directory.join("group").exists())
                        && !safe_completion(&directory)?
                    {
                        return Err(invalid(
                            "supervisor lost before completion; process-group cancellation cannot be confirmed",
                        ));
                    }
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(invalid("cancel acknowledgement timed out; cleanup forbidden"));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    if op != "run" {
        return Err(invalid("unknown operation"));
    }
    let _invocation_lock = locked(&directory.join(".lease"), true)?;
    if directory.join("result").exists() {
        return Err(invalid("invocation already completed"));
    }
    let (timeout, arguments) = request(&directory.join("request"))?;
    let binary = directory.join("test");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))?;
    let stdout = File::create(directory.join("stdout"))?;
    let stderr = File::create(directory.join("stderr"))?;
    // adbd sends SIGHUP to the direct shell on transport loss. The supervisor
    // must outlive that connection so device timeout/cancel remains authoritative.
    // SAFETY: SIG_IGN is the platform-defined handler value 1 for SIGHUP.
    unsafe {
        signal(1, 1);
    }
    fs::write(directory.join("boot"), fs::read("/proc/sys/kernel/random/boot_id")?)?;
    let intent = File::create(directory.join("execution-intent"))?;
    intent.sync_all()?;
    let mut limits = RLimit { current: 0, maximum: 0 };
    // RLIMIT_NOFILE=7 and rlim_t=unsigned long are fixed by Linux/Bionic's resource
    // ABI.
    if unsafe { getrlimit(7, &mut limits) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let descriptor_limit = i32::try_from(limits.current).map_err(|_| invalid("descriptor limit out of range"))?;
    let (lifetime_read, lifetime_write) = UnixStream::pair()?;
    let read_fd = lifetime_read.as_raw_fd();
    let write_fd = lifetime_write.as_raw_fd();
    let mut command = Command::new(&binary);
    command
        .args(arguments)
        .current_dir(directory.join("work"))
        .env("TMPDIR", directory.join("tmp"))
        .env("LD_LIBRARY_PATH", directory.join("lib"))
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0);
    // SAFETY: resetting a signal handler is async-signal-safe in the child before
    // exec.
    unsafe {
        command.pre_exec(move || {
            signal(1, 0);
            let anchor = fork();
            if anchor < 0 {
                return Err(io::Error::last_os_error());
            }
            if anchor == 0 {
                // Includes Rust's post-fork exec-error pipe, which did not exist
                // when pre_exec was registered. Retaining it would deadlock spawn.
                for fd in 1..descriptor_limit {
                    if fd != read_fd {
                        close(fd);
                    }
                }
                let mut byte = 0_u8;
                while read(read_fd, (&mut byte as *mut u8).cast(), 1) > 0 {}
                kill(-getpgrp(), 9);
                _exit(71);
            }
            close(read_fd);
            close(write_fd);
            Ok(())
        });
    }
    let child = command.spawn();
    drop(lifetime_read);
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            fs::write(directory.join("result.tmp"), format!("RSPAD001\nspawn\n{error}\n"))?;
            fs::rename(directory.join("result.tmp"), directory.join("result"))?;
            return Ok(());
        }
    };
    let group = child.id() as i32;
    let mut guard = GroupGuard(Some(group));
    fs::write(directory.join("group"), group.to_string())?;
    let started = Instant::now();
    let mut timed_out = false;
    let mut cancelled = false;
    loop {
        if child_exited(group as u32)? {
            break;
        }
        cancelled = directory.join("cancel").exists();
        timed_out = started.elapsed() >= timeout;
        if cancelled || timed_out {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    // The group is owned by this invocation. Ordinary descendants are stopped on
    // success too. SAFETY: negative child pid addresses the dedicated process
    // group created before exec.
    stop_group(group, 15)?;
    thread::sleep(Duration::from_millis(100));
    // SAFETY: the same owned process group may contain descendants after its leader
    // exits.
    stop_group(group, 9)?;
    wait_group_stopped(group)?;
    let mut stopped = File::create(directory.join("group-stopped"))?;
    stopped.write_all(b"no live group members\n")?;
    stopped.sync_all()?;
    guard.0 = None;
    let status = child.wait()?;
    drop(lifetime_write);
    let result = format!(
        "RSPAD001\nexit\n{}\n{}\n{}\n{}\n",
        status.code().unwrap_or(-1),
        status.signal().unwrap_or(0),
        u8::from(timed_out),
        u8::from(cancelled)
    );
    let mut file = File::create(directory.join("result.tmp"))?;
    file.write_all(result.as_bytes())?;
    file.sync_all()?;
    fs::rename(directory.join("result.tmp"), directory.join("result"))?;
    Ok(())
}

struct GroupGuard(Option<i32>);
impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            let _ = stop_group(group, 9);
        }
    }
}

fn stop_group(group: i32, signal: i32) -> io::Result<()> {
    // SAFETY: group was created for this child by process_group(0) before exec.
    if unsafe { kill(-group, signal) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(3) {
            return Err(error);
        } // ESRCH: group already empty.
    }
    Ok(())
}

fn request(path: &Path) -> io::Result<(Duration, Vec<String>)> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > 4 * 1024 * 1024 {
        return Err(invalid("request exceeds 4 MiB"));
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    let mut cursor = bytes.as_slice();
    if take(&mut cursor, 8)? != b"RSPAD001" {
        return Err(invalid("protocol version mismatch"));
    }
    let timeout = u64::from_le_bytes(take(&mut cursor, 8)?.try_into().map_err(|_| invalid("timeout"))?);
    if timeout == 0 || timeout > 86_400_000 {
        return Err(invalid("timeout out of range"));
    }
    let count = u32::from_le_bytes(take(&mut cursor, 4)?.try_into().map_err(|_| invalid("argc"))?);
    if count > 1024 {
        return Err(invalid("too many arguments"));
    }
    let mut args = Vec::new();
    for _ in 0..count {
        let length = u32::from_le_bytes(
            take(&mut cursor, 4)?
                .try_into()
                .map_err(|_| invalid("argument length"))?,
        ) as usize;
        if length > 1024 * 1024 {
            return Err(invalid("argument too large"));
        }
        let value = take(&mut cursor, length)?;
        if value.contains(&0) {
            return Err(invalid("NUL in argument"));
        }
        args.push(String::from_utf8(value.to_vec()).map_err(|_| invalid("argument is not UTF-8"))?);
    }
    if !cursor.is_empty() {
        return Err(invalid("trailing protocol bytes"));
    }
    Ok((Duration::from_millis(timeout), args))
}
fn take<'a>(cursor: &mut &'a [u8], length: usize) -> io::Result<&'a [u8]> {
    if length > cursor.len() {
        return Err(invalid("truncated request"));
    }
    let (head, tail) = cursor.split_at(length);
    *cursor = tail;
    Ok(head)
}

// Bionic NDK27 asm-generic/siginfo.h fixes siginfo_t at 128 bytes and puts
// si_signo at offset zero on every supported Android ABI. No union offsets are
// used.
#[repr(C, align(16))]
struct SigInfo([u8; 128]);
fn child_exited(pid: u32) -> io::Result<bool> {
    let mut info = SigInfo([0; 128]);
    // SAFETY: aligned, correctly sized siginfo storage; WNOWAIT keeps our child
    // unreaped.
    if unsafe { waitid(1, pid, info.0.as_mut_ptr().cast(), 4 | 1 | 0x01000000) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(i32::from_ne_bytes(info.0[..4].try_into().map_err(|_| invalid("siginfo signo"))?) != 0)
}
fn wait_group_stopped(group: i32) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut live = false;
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
                continue;
            }
            let stat = match fs::read_to_string(entry.path().join("stat")) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let fields = stat
                .rsplit_once(')')
                .ok_or_else(|| invalid("malformed process stat"))?
                .1
                .split_whitespace()
                .collect::<Vec<_>>();
            if fields.get(2).and_then(|value| value.parse::<i32>().ok()) != Some(group) {
                continue;
            }
            let tasks = match fs::read_dir(entry.path().join("task")) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for task in tasks {
                let stat = match fs::read_to_string(task?.path().join("stat")) {
                    Ok(value) => value,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                if !matches!(
                    stat.rsplit_once(')')
                        .and_then(|(_, fields)| fields.split_whitespace().next()),
                    Some("Z" | "X" | "x")
                ) {
                    live = true;
                }
            }
        }
        if !live {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(invalid("live process-group members remain; completion forbidden"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}
fn safe_completion(directory: &Path) -> io::Result<bool> {
    if directory.join("result").exists() || directory.join("group-stopped").exists() {
        return Ok(true);
    }
    let original = match fs::read(directory.join("boot")) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let current = fs::read("/proc/sys/kernel/random/boot_id")?;
    Ok(!original.is_empty() && !current.is_empty() && original != current)
}

#[repr(C)]
struct RLimit {
    current: usize,
    maximum: usize,
}
