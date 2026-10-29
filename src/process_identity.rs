// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Conservative host and process identity checks for abandoned-run recovery.

/// Reads the stable local machine identity; unavailable or empty identity is
/// `None`.
pub(crate) fn host_id() -> Option<String> {
    platform::host_id()
}

/// Reads an exact kernel boot-session identity where supported, otherwise
/// `None`. Windows deliberately provides no wall-clock/uptime approximation.
pub(crate) fn boot_id() -> Option<String> {
    platform::boot_id()
}

/// Reads the process creation identity, returning `None` if unavailable or
/// exited.
pub(crate) fn process_start(pid: u32) -> Option<String> {
    match platform::observe(pid) {
        Observation::Alive(start) => Some(start),
        Observation::Dead | Observation::Unknown => None,
    }
}

/// Checks the original owner using local machine and exact process creation
/// identity. `Some(false)` proves exit, PID reuse, or a different known boot
/// session; `Some(true)` requires a matching creation identity and live OS
/// process state. Foreign/unknown hosts, denied OS queries, or a live PID
/// without a recorded creation identity yield `None`.
/// Callers must also retain the run lease while acting on this point-in-time
/// result.
pub(crate) fn owner_alive(
    recorded_host: Option<&str>,
    recorded_boot: Option<&str>,
    pid: u32,
    recorded_start: Option<&str>,
) -> Option<bool> {
    let recorded_host = recorded_host.filter(|value| !value.is_empty())?;
    if host_id().as_deref()? != recorded_host {
        return None;
    }
    if let (Some(recorded), Some(current)) = (recorded_boot, boot_id())
        && !recorded.is_empty()
        && recorded != current
    {
        return Some(false);
    }
    match platform::observe(pid) {
        Observation::Dead => Some(false),
        Observation::Alive(current) => recorded_start
            .filter(|value| !value.is_empty())
            .map(|recorded| recorded == current),
        Observation::Unknown => None,
    }
}

enum Observation {
    Alive(String),
    Dead,
    Unknown,
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;
    use std::io;

    use super::Observation;

    pub(super) fn host_id() -> Option<String> {
        fs::read_to_string("/etc/machine-id")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    pub(super) fn boot_id() -> Option<String> {
        fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    pub(super) fn observe(pid: u32) -> Observation {
        let Ok(native_pid) = i32::try_from(pid) else {
            return Observation::Unknown;
        };
        if native_pid <= 0 {
            return Observation::Unknown;
        }
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => parse_stat(&stat),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // hidepid can conceal a live process: only ESRCH proves absence.
                // SAFETY: a positive PID and signal 0 inspect existence without signaling.
                if unsafe { libc::kill(native_pid, 0) } == -1
                    && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    Observation::Dead
                } else {
                    Observation::Unknown
                }
            }
            Err(_) => Observation::Unknown,
        }
    }

    fn parse_stat(stat: &str) -> Observation {
        // comm can contain whitespace and closing parentheses; fields follow the last
        // `) `.
        let Some((_, fields)) = stat.rsplit_once(") ") else {
            return Observation::Unknown;
        };
        let mut fields = fields.split_whitespace();
        let Some(state) = fields.next() else {
            return Observation::Unknown;
        };
        let Some(start) = fields.nth(18).filter(|value| value.parse::<u64>().is_ok()) else {
            return Observation::Unknown;
        };
        if matches!(state, "Z" | "X" | "x") {
            Observation::Dead
        } else {
            Observation::Alive(start.to_owned())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::parse_stat;
        use crate::process_identity::Observation;

        #[test]
        fn test_linux_stat_keeps_exact_start_with_parentheses_in_name() {
            let stat = format!("123 (worker ) with spaces) S {} 987654321012345", "0 ".repeat(18));
            assert!(matches!(parse_stat(&stat), Observation::Alive(start) if start == "987654321012345"));
        }

        #[test]
        fn test_linux_zombie_is_dead_and_malformed_stat_is_unknown() {
            let stat = format!("123 (worker) Z {} 42", "0 ".repeat(18));
            assert!(matches!(parse_stat(&stat), Observation::Dead));
            assert!(matches!(parse_stat("123 (truncated) S 0"), Observation::Unknown));
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::io;
    use std::mem::MaybeUninit;
    use std::mem::size_of;
    use std::process::Command;
    use std::ptr;

    use super::Observation;

    pub(super) fn host_id() -> Option<String> {
        // Use the system utility by absolute path; IOPlatformUUID is the hardware UUID.
        let output = Command::new("/usr/sbin/ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let output = String::from_utf8(output.stdout).ok()?;
        output.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            if key.trim() != "\"IOPlatformUUID\"" {
                return None;
            }
            let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
            (!value.is_empty()).then(|| value.to_owned())
        })
    }

    pub(super) fn boot_id() -> Option<String> {
        let mut bytes = [0_u8; 128];
        let mut length = bytes.len();
        // SAFETY: the output allocation matches length and this read passes no new
        // value.
        let result = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                bytes.as_mut_ptr().cast(),
                &mut length,
                ptr::null_mut(),
                0,
            )
        };
        if result != 0 || length == 0 || length > bytes.len() {
            return None;
        }
        let value = std::str::from_utf8(&bytes[..length]).ok()?.trim_end_matches('\0');
        (!value.is_empty()).then(|| value.to_owned())
    }

    pub(super) fn observe(pid: u32) -> Observation {
        let Ok(native_pid) = i32::try_from(pid) else {
            return Observation::Unknown;
        };
        if native_pid <= 0 {
            return Observation::Unknown;
        }
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        // SAFETY: PROC_PIDTBSDINFO writes proc_bsdinfo and its exact size is supplied.
        let count = unsafe {
            libc::proc_pidinfo(
                native_pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size_of::<libc::proc_bsdinfo>() as i32,
            )
        };
        if count != size_of::<libc::proc_bsdinfo>() as i32 {
            return if count <= 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                Observation::Dead
            } else {
                Observation::Unknown
            };
        }
        // SAFETY: the successful call initialized the complete structure.
        let info = unsafe { info.assume_init() };
        if info.pbi_pid != pid {
            return Observation::Unknown;
        }
        if info.pbi_status == libc::SZOMB {
            return Observation::Dead;
        }
        Observation::Alive(format!("{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec))
    }
}

#[cfg(windows)]
mod platform {
    use std::ptr;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
    use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    use windows_sys::Win32::System::Registry::RRF_RT_REG_SZ;
    use windows_sys::Win32::System::Registry::RRF_SUBKEY_WOW6464KEY;
    use windows_sys::Win32::System::Registry::RegGetValueW;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
    use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    use super::Observation;

    pub(super) fn host_id() -> Option<String> {
        let key: Vec<u16> = "SOFTWARE\\Microsoft\\Cryptography\0".encode_utf16().collect();
        let name: Vec<u16> = "MachineGuid\0".encode_utf16().collect();
        let mut buffer = [0_u16; 128];
        let mut bytes = std::mem::size_of_val(&buffer) as u32;
        // SAFETY: null-terminated UTF-16 names and a correctly sized writable buffer.
        let result = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
                ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if result != ERROR_SUCCESS || bytes as usize > std::mem::size_of_val(&buffer) {
            return None;
        }
        let end = buffer.iter().position(|value| *value == 0)?;
        let value = String::from_utf16(&buffer[..end]).ok()?;
        (!value.is_empty()).then_some(value)
    }

    pub(super) fn boot_id() -> Option<String> {
        // Windows has no public stable per-boot UUID API. Exact process creation time
        // still permits recovery without guessing a boot time from adjustable clocks.
        None
    }

    pub(super) fn observe(pid: u32) -> Observation {
        if pid == 0 {
            return Observation::Unknown;
        }
        // SAFETY: OpenProcess accepts a numeric PID and returns an owned handle.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            // pid zero was excluded; invalid parameter for these valid flags means absent
            // PID.
            return if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER {
                Observation::Dead
            } else {
                Observation::Unknown
            };
        }
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: retain the same process handle for creation identity and exit status.
        let observation = unsafe {
            if GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) == 0 {
                Observation::Unknown
            } else {
                match WaitForSingleObject(handle, 0) {
                    WAIT_OBJECT_0 => Observation::Dead,
                    WAIT_TIMEOUT => Observation::Alive(
                        ((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)).to_string(),
                    ),
                    _ => Observation::Unknown,
                }
            }
        };
        // SAFETY: this handle was returned successfully by OpenProcess, and is no
        // longer used.
        unsafe { CloseHandle(handle) };
        observation
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::Observation;
    pub(super) fn host_id() -> Option<String> {
        None
    }
    pub(super) fn boot_id() -> Option<String> {
        None
    }
    pub(super) fn observe(_pid: u32) -> Observation {
        Observation::Unknown
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::process::Command;
    use std::process::Stdio;

    use super::boot_id;
    use super::host_id;
    use super::owner_alive;
    use super::process_start;

    #[test]
    fn test_foreign_or_unknown_host_remains_unknown() {
        assert_eq!(owner_alive(None, None, std::process::id(), None), None);
        assert_eq!(
            owner_alive(Some("foreign-host-fixture"), None, std::process::id(), None),
            None
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    fn test_self_is_alive_and_reused_identity_is_dead() {
        let host = host_id().expect("host identity");
        let boot = boot_id();
        let pid = std::process::id();
        let start = process_start(pid).expect("current process start identity");
        assert_eq!(owner_alive(Some(&host), boot.as_deref(), pid, Some(&start)), Some(true));
        assert_eq!(
            owner_alive(Some(&host), boot.as_deref(), pid, Some("different-start")),
            Some(false)
        );
        assert_eq!(owner_alive(Some(&host), boot.as_deref(), pid, None), None);
        assert_eq!(owner_alive(Some(&host), None, pid, Some(&start)), Some(true));
        assert_eq!(owner_alive(Some(&host), boot.as_deref(), 0, Some(&start)), None);
        if boot.is_some() {
            assert_eq!(
                owner_alive(Some(&host), Some("previous-boot-fixture"), pid, Some(&start)),
                Some(false)
            );
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    fn test_exited_child_is_dead() {
        let host = host_id().expect("host identity");
        let boot = boot_id();
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--ignored", "test_process_fixture_wait"])
            .env("RS_INFRA_IDENTITY_TEST_FIXTURE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn identity fixture");
        let pid = child.id();
        let start = process_start(pid).expect("child process start identity");
        assert_eq!(owner_alive(Some(&host), boot.as_deref(), pid, Some(&start)), Some(true));
        drop(child.stdin.take());
        assert!(child.wait().expect("wait fixture").success());
        assert_eq!(
            owner_alive(Some(&host), boot.as_deref(), pid, Some(&start)),
            Some(false)
        );
    }

    #[test]
    #[ignore = "subprocess fixture waits for its parent to close stdin"]
    fn test_process_fixture_wait() {
        if std::env::var_os("RS_INFRA_IDENTITY_TEST_FIXTURE").is_none() {
            return;
        }
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input).expect("read fixture stdin");
    }
}
