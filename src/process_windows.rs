// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Windows command encoding and atomic process containment.

use std::collections::BTreeMap;

use anyhow::Result;
use anyhow::ensure;

type Environment = BTreeMap<Vec<u16>, (Vec<u16>, Vec<u16>)>;

fn job_termination_proven(active: Option<u32>) -> bool {
    active == Some(0)
}

fn command_line(executable: &[u16], arguments: &[Vec<u16>], raw_tail: Option<&[u16]>) -> Result<Vec<u16>> {
    ensure!(
        !executable.contains(&0) && !executable.contains(&34),
        "invalid executable name"
    );
    let mut result = vec![34];
    result.extend(executable);
    result.push(34);
    for argument in arguments {
        ensure!(!argument.contains(&0), "NUL in Windows process argument");
        if raw_tail.is_some() {
            ensure!(
                !argument.is_empty() && argument.iter().all(|unit| (33..=126).contains(unit) && *unit != 34),
                "invalid trusted cmd option"
            );
            result.push(32);
            result.extend(argument);
            continue;
        }
        result.extend([32, 34]);
        let mut slashes = 0;
        for &unit in argument {
            if unit == 92 {
                slashes += 1;
                continue;
            }
            result.extend(std::iter::repeat_n(
                92,
                if unit == 34 { 2 * slashes + 1 } else { slashes },
            ));
            result.push(unit);
            slashes = 0;
        }
        result.extend(std::iter::repeat_n(92, slashes * 2));
        result.push(34);
    }
    if let Some(tail) = raw_tail {
        ensure!(!tail.contains(&0), "NUL in trusted Windows script");
        result.push(32);
        result.extend(tail);
    }
    result.push(0);
    ensure!(
        result.len() <= 32767,
        "Windows process command line exceeds 32767 UTF16 units"
    );
    Ok(result)
}

fn environment_values(entries: &[(Vec<u16>, Vec<u16>)]) -> Result<Environment> {
    let mut values = BTreeMap::new();
    for (key, value) in entries {
        ensure!(
            !key.is_empty() && key.iter().all(|unit| (1..=127).contains(unit)) && !value.contains(&0),
            "Windows environment names must be nonempty ASCII without NUL; values must not contain NUL"
        );
        let hidden_drive = key.len() == 3 && key[0] == 61 && (key[1] as u8).is_ascii_alphabetic() && key[2] == 58;
        ensure!(!key.contains(&61) || hidden_drive, "invalid Windows environment name");
        let folded = key
            .iter()
            .map(|unit| (*unit as u8).to_ascii_uppercase() as u16)
            .collect();
        values.insert(folded, (key.clone(), value.clone()));
    }
    Ok(values)
}

fn environment_block(values: &Environment) -> Vec<u16> {
    let mut block = Vec::new();
    for (key, value) in values.values() {
        block.extend(key);
        block.push(61);
        block.extend(value);
        block.push(0);
    }
    block.push(0);
    if values.is_empty() {
        block.push(0);
    }
    block
}

#[cfg(windows)]
pub(super) use native::Process;
#[cfg(windows)]
pub(super) use native::retry_stop;

#[cfg(windows)]
mod native {
    use std::ffi::OsStr;
    use std::ffi::OsString;
    use std::fs::File;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawHandle;
    use std::os::windows::io::OwnedHandle;
    use std::os::windows::process::ExitStatusExt;
    use std::path::Path;
    use std::path::PathBuf;
    use std::process::ExitStatus;
    use std::ptr::null;
    use std::ptr::null_mut;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

    use anyhow::Context;
    use anyhow::Result;
    use anyhow::bail;
    use anyhow::ensure;
    use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
    use windows_sys::Win32::Foundation::DuplicateHandle;
    use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Foundation::SetLastError;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
    use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
    use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    use windows_sys::Win32::System::JobObjects::JOBOBJECT_BASIC_ACCOUNTING_INFORMATION;
    use windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
    use windows_sys::Win32::System::JobObjects::JobObjectBasicAccountingInformation;
    use windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation;
    use windows_sys::Win32::System::JobObjects::OpenJobObjectW;
    use windows_sys::Win32::System::JobObjects::QueryInformationJobObject;
    use windows_sys::Win32::System::JobObjects::SetInformationJobObject;
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;
    use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    use windows_sys::Win32::System::Registry::RRF_RT_REG_SZ;
    use windows_sys::Win32::System::Registry::RRF_SUBKEY_WOW6464KEY;
    use windows_sys::Win32::System::Registry::RegGetValueW;
    use windows_sys::Win32::System::SystemServices::JOB_OBJECT_QUERY;
    use windows_sys::Win32::System::SystemServices::JOB_OBJECT_TERMINATE;
    use windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT;
    use windows_sys::Win32::System::Threading::CreateProcessW;
    use windows_sys::Win32::System::Threading::DeleteProcThreadAttributeList;
    use windows_sys::Win32::System::Threading::EXTENDED_STARTUPINFO_PRESENT;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    use windows_sys::Win32::System::Threading::INFINITE;
    use windows_sys::Win32::System::Threading::InitializeProcThreadAttributeList;
    use windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST;
    use windows_sys::Win32::System::Threading::PROC_THREAD_ATTRIBUTE_HANDLE_LIST;
    use windows_sys::Win32::System::Threading::PROC_THREAD_ATTRIBUTE_JOB_LIST;
    use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
    use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
    use windows_sys::Win32::System::Threading::STARTUPINFOEXW;
    use windows_sys::Win32::System::Threading::UpdateProcThreadAttribute;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    use super::Environment;
    use super::command_line;
    use super::environment_block;
    use super::environment_values;
    use crate::process::ProcessSpec;
    use crate::process::StopIdentity;

    pub(crate) struct Process {
        handle: OwnedHandle,
        job: OwnedHandle,
        identity: StopIdentity,
        stopped: bool,
    }

    fn host_identity() -> Result<String> {
        let key: Vec<_> = "SOFTWARE\\Microsoft\\Cryptography\0".encode_utf16().collect();
        let name: Vec<_> = "MachineGuid\0".encode_utf16().collect();
        let mut buffer = [0_u16; 128];
        let mut bytes = std::mem::size_of_val(&buffer) as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
                null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32)).context("read Windows process owner MachineGuid");
        }
        let host = String::from_utf16(&buffer[..bytes as usize / 2])?
            .trim_end_matches('\0')
            .to_ascii_lowercase();
        ensure!(
            !host.is_empty() && host.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b'-'),
            "invalid Windows process owner MachineGuid"
        );
        Ok(host)
    }

    pub(crate) fn retry_stop(identity: &StopIdentity) -> Result<bool> {
        let StopIdentity::Windows { host, job } = identity else {
            return Ok(false);
        };
        if *host != host_identity()? {
            return Ok(false);
        }
        let Some(nonce) = job.strip_prefix("Global\\rs-infra-platform-") else {
            return Ok(false);
        };
        if nonce.len() != 64 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(false);
        }
        let name = terminated(OsStr::new(job))?;
        let handle = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE, 0, name.as_ptr()) };
        if handle.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) {
                return Ok(super::job_termination_proven(None));
            }
            return Err(error).context("reopen owned process job for stop retry");
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        stop_job(&handle)?;
        Ok(true)
    }

    fn stop_job(job: &OwnedHandle) -> Result<()> {
        if unsafe { TerminateJobObject(job.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error()).context("terminate atomic process job");
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            if unsafe {
                QueryInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error()).context("query atomic process job termination");
            }
            if super::job_termination_proven(Some(info.ActiveProcesses)) {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "atomic job processes are still alive; termination pending"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    struct Attributes {
        storage: Vec<usize>,
    }

    impl Attributes {
        fn new() -> Result<Self> {
            let mut bytes = 0;
            // SAFETY: the first call queries the exact attribute-list allocation size.
            unsafe {
                InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut bytes);
            }
            ensure!(bytes > 0, "query process attribute allocation failed");
            let mut storage = vec![0_usize; bytes.div_ceil(size_of::<usize>())];
            // SAFETY: usize storage has pointer alignment and the requested byte capacity.
            if unsafe { InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 2, 0, &mut bytes) } == 0 {
                return Err(io::Error::last_os_error()).context("initialize process attributes");
            }
            Ok(Self { storage })
        }

        fn pointer(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.storage.as_mut_ptr().cast()
        }

        fn set(&mut self, attribute: u32, handles: &[HANDLE]) -> Result<()> {
            // SAFETY: callers retain each handle array until after CreateProcessW returns.
            if unsafe {
                UpdateProcThreadAttribute(
                    self.pointer(),
                    0,
                    attribute as usize,
                    handles.as_ptr().cast(),
                    std::mem::size_of_val(handles),
                    null_mut(),
                    null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error()).context("set atomic process creation attribute");
            }
            Ok(())
        }
    }

    impl Drop for Attributes {
        fn drop(&mut self) {
            // SAFETY: the initialized list remains allocated until after deletion.
            unsafe {
                DeleteProcThreadAttributeList(self.pointer());
            }
        }
    }

    fn inherited(file: &File) -> Result<OwnedHandle> {
        let mut duplicate = null_mut();
        // SAFETY: source is borrowed; only the new handle becomes owned and
        // inheritable.
        if unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                file.as_raw_handle(),
                GetCurrentProcess(),
                &mut duplicate,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).context("duplicate controlled stdio handle");
        }
        Ok(unsafe { OwnedHandle::from_raw_handle(duplicate) })
    }

    fn terminated(value: &OsStr) -> Result<Vec<u16>> {
        let mut value: Vec<_> = value.encode_wide().collect();
        ensure!(!value.contains(&0), "NUL in Windows path");
        value.push(0);
        Ok(value)
    }

    fn executable(program: &str, cwd: &Path, environment: &Environment) -> Result<PathBuf> {
        ensure!(
            !program.is_empty() && !program.contains('\0'),
            "invalid Windows executable"
        );
        let program = Path::new(program);
        let explicit = program.is_absolute() || program.components().count() > 1;
        ensure!(
            !program.has_root() || program.is_absolute(),
            "root-relative Windows executables are unsupported"
        );
        if let Some(std::path::Component::Prefix(_)) = program.components().next() {
            ensure!(
                program.is_absolute(),
                "drive-relative Windows executables are unsupported"
            );
        }
        let mut names = vec![program.to_owned()];
        if program.extension().is_none() {
            names.push(program.with_extension("exe"));
        }
        for name in &names {
            if let Some(extension) = name.extension() {
                ensure!(
                    extension
                        .to_str()
                        .is_some_and(|value| value.eq_ignore_ascii_case("exe")),
                    "only native .exe programs are supported; use the trusted cmd script API for controlled scripts"
                );
            }
        }
        let directories: Vec<PathBuf> = if explicit {
            vec![cwd.to_owned()]
        } else {
            let (_, path) = environment
                .get(&"PATH".encode_utf16().collect::<Vec<_>>())
                .context("Windows PATH is not configured")?;
            std::env::split_paths(&OsString::from_wide(path))
                .map(|path| if path.is_absolute() { path } else { cwd.join(path) })
                .collect()
        };
        for directory in directories {
            for name in &names {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    return std::fs::canonicalize(candidate).context("resolve Windows executable");
                }
            }
        }
        bail!(
            "Windows executable was not found in controlled PATH: {}",
            program.display()
        )
    }

    impl Process {
        pub(crate) fn stop_identity(&self) -> Option<StopIdentity> {
            Some(self.identity.clone())
        }
        pub(crate) fn spawn_spec(
            spec: &ProcessSpec,
            stdin: Option<File>,
            stdout: File,
            stderr: File,
            raw_tail: Option<&OsStr>,
        ) -> Result<Self> {
            let mut entries: Vec<_> = std::env::vars_os()
                .map(|(key, value)| (key.encode_wide().collect(), value.encode_wide().collect()))
                .collect();
            entries.extend(
                spec.env
                    .iter()
                    .map(|(key, value)| (key.encode_wide().collect(), value.encode_wide().collect())),
            );
            let environment = environment_values(&entries)?;
            let environment_block = environment_block(&environment);
            let cwd = std::fs::canonicalize(&spec.cwd).context("resolve Windows process directory")?;
            let executable = executable(&spec.argv[0], &cwd, &environment)?;
            if raw_tail.is_some() {
                let trusted = spec.argv.len() == 5
                    && spec.argv[1..]
                        .iter()
                        .zip(["/u", "/d", "/s", "/c"])
                        .all(|(actual, expected)| actual.eq_ignore_ascii_case(expected));
                ensure!(
                    trusted
                        && executable.file_name().is_some_and(|name| name
                            .to_str()
                            .is_some_and(|value| value.eq_ignore_ascii_case("cmd.exe"))),
                    "trusted raw scripts require cmd.exe /u /d /s /c"
                );
            }
            let arguments: Vec<_> = spec.argv[1..]
                .iter()
                .map(|argument| argument.encode_utf16().collect())
                .collect();
            let raw_tail: Option<Vec<_>> = raw_tail.map(|tail| tail.encode_wide().collect());
            let executable_units: Vec<_> = executable.as_os_str().encode_wide().collect();
            let mut command_line = command_line(&executable_units, &arguments, raw_tail.as_deref())?;
            let application = terminated(executable.as_os_str())?;
            let cwd = terminated(cwd.as_os_str())?;
            let stdin = match stdin {
                Some(stdin) => stdin,
                None => File::open("NUL").context("open Windows null stdin")?,
            };
            let stdin = inherited(&stdin)?;
            let stdout = inherited(&stdout)?;
            let stderr = inherited(&stderr)?;
            let handles = [stdin.as_raw_handle(), stdout.as_raw_handle(), stderr.as_raw_handle()];
            let mut nonce = [0_u8; 32];
            getrandom::fill(&mut nonce)
                .map_err(|error| anyhow::anyhow!("generate process job ownership nonce: {error}"))?;
            let name = format!(
                "Global\\rs-infra-platform-{}",
                nonce.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
            );
            let identity = StopIdentity::Windows {
                host: host_identity()?,
                job: name.clone(),
            };
            let name = terminated(OsStr::new(&name))?;
            // SAFETY: the job handle is non-inheritable. A random global name permits owned
            // retries across controller sessions; a preexisting object is never reused.
            unsafe {
                SetLastError(0);
            }
            let job = unsafe { CreateJobObjectW(null(), name.as_ptr()) };
            let job_error = unsafe { GetLastError() };
            if job.is_null() {
                return Err(io::Error::last_os_error()).context("create atomic process job");
            }
            let job = unsafe { OwnedHandle::from_raw_handle(job) };
            ensure!(
                job_error != ERROR_ALREADY_EXISTS,
                "owned process job name already exists"
            );
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error()).context("configure atomic process job");
            }
            let jobs = [job.as_raw_handle()];
            let mut attributes = Attributes::new()?;
            attributes.set(PROC_THREAD_ATTRIBUTE_JOB_LIST, &jobs)?;
            attributes.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)?;
            let mut startup = STARTUPINFOEXW::default();
            startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = handles[0];
            startup.StartupInfo.hStdOutput = handles[1];
            startup.StartupInfo.hStdError = handles[2];
            startup.lpAttributeList = attributes.pointer();
            let mut info = PROCESS_INFORMATION::default();
            // SAFETY: every mutable/immutable UTF16 buffer and attribute array remains
            // alive; JOB_LIST associates containment before any user
            // instruction can execute. Only the three explicitly listed stdio
            // handles are inherited, never the job ownership handle.
            let created = unsafe {
                CreateProcessW(
                    application.as_ptr(),
                    command_line.as_mut_ptr(),
                    null(),
                    null(),
                    1,
                    CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                    environment_block.as_ptr().cast(),
                    cwd.as_ptr(),
                    &startup.StartupInfo,
                    &mut info,
                )
            };
            if created == 0 {
                return Err(io::Error::last_os_error()).context("CreateProcessW with atomic Job Object");
            }
            let handle = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
            let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
            drop(thread);
            Ok(Self {
                handle,
                job,
                identity,
                stopped: false,
            })
        }

        pub(crate) fn poll_exit(&mut self) -> Result<bool> {
            match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) } {
                WAIT_OBJECT_0 => Ok(true),
                WAIT_TIMEOUT => Ok(false),
                _ => Err(io::Error::last_os_error()).context("poll atomic Windows process"),
            }
        }

        pub(crate) fn wait(&mut self) -> Result<ExitStatus> {
            ensure!(
                unsafe { WaitForSingleObject(self.handle.as_raw_handle(), INFINITE) } == WAIT_OBJECT_0,
                "wait for atomic Windows process failed"
            );
            let mut code = 0;
            if unsafe { GetExitCodeProcess(self.handle.as_raw_handle(), &mut code) } == 0 {
                return Err(io::Error::last_os_error()).context("read Windows process exit status");
            }
            Ok(ExitStatus::from_raw(code))
        }

        pub(crate) fn stop_tree(&mut self) -> Result<()> {
            if self.stopped {
                return Ok(());
            }
            stop_job(&self.job)?;
            self.stopped = true;
            Ok(())
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.stop_tree();
        }
    }

    #[cfg(test)]
    mod tests {
        use std::collections::BTreeMap;
        use std::ffi::OsString;
        use std::fs::File;
        use std::os::windows::io::AsRawHandle;
        use std::time::Duration;

        use windows_sys::Win32::System::JobObjects::IsProcessInJob;

        use super::Process;
        use super::retry_stop;
        use crate::process::ProcessSpec;

        #[test]
        fn test_create_assigns_job_and_reopens_owned_stop_identity() {
            let root = tempfile::tempdir().expect("create atomic Job test root");
            let spec = ProcessSpec {
                argv: vec![
                    std::env::current_exe()
                        .expect("locate fixture executable")
                        .to_string_lossy()
                        .into_owned(),
                    "--exact".into(),
                    "process::tests::process_fixture".into(),
                    "--ignored".into(),
                    "--nocapture".into(),
                ],
                cwd: root.path().into(),
                env: BTreeMap::from([
                    (OsString::from("PLATFORM_PROCESS_MODE"), OsString::from("stream")),
                    (OsString::from("PLATFORM_PROCESS_ROOT"), root.path().as_os_str().into()),
                ]),
                timeout: Duration::from_secs(5),
                stdout: root.path().join("stdout"),
                stderr: root.path().join("stderr"),
            };
            let mut process = Process::spawn_spec(
                &spec,
                None,
                File::create(&spec.stdout).expect("create stdout"),
                File::create(&spec.stderr).expect("create stderr"),
                None,
            )
            .expect("create process atomically in Job");
            let mut contained = 0;
            assert_ne!(
                unsafe {
                    IsProcessInJob(
                        process.handle.as_raw_handle(),
                        process.job.as_raw_handle(),
                        &mut contained,
                    )
                },
                0
            );
            assert_ne!(
                contained, 0,
                "process must already belong to the Job returned by creation"
            );
            let identity = process.stop_identity().expect("capture named Job identity");
            assert!(retry_stop(&identity).expect("reopen and stop the owned Job"));
            process.stop_tree().expect("confirm Job members are gone");
            process.wait().expect("observe stopped process");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::command_line;
    use super::environment_block;
    use super::environment_values;

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    #[test]
    fn test_crt_command_line_preserves_quotes_empty_and_trailing_slashes() {
        let encoded = command_line(
            &wide("C:\\Program Files\\tool.exe"),
            &[wide(""), wide("a b"), wide("a\"b"), wide("tail\\")],
            None,
        )
        .expect("encode CRT argv");
        assert_eq!(
            String::from_utf16(&encoded[..encoded.len() - 1]).expect("UTF16"),
            "\"C:\\Program Files\\tool.exe\" \"\" \"a b\" \"a\\\"b\" \"tail\\\\\""
        );
    }

    #[test]
    fn test_trusted_script_tail_is_appended_verbatim() {
        let tail = wide("\"\"C:\\A B\\init.cmd\" > \"C:\\C D\\env\"\"");
        let encoded = command_line(
            &wide("cmd.exe"),
            &[wide("/u"), wide("/d"), wide("/s"), wide("/c")],
            Some(&tail),
        )
        .expect("encode trusted cmd tail");
        let expected = wide("\"cmd.exe\" /u /d /s /c ");
        assert!(encoded.starts_with(&expected));
        assert_eq!(&encoded[expected.len()..encoded.len() - 1], tail);
    }

    #[test]
    fn test_environment_overrides_are_case_insensitive_and_double_nul() {
        let entries = [
            (wide("Path"), wide("old")),
            (wide("PATH"), wide("new")),
            (wide("EMPTY"), vec![]),
        ];
        let environment = environment_values(&entries).expect("merge environment");
        let block = environment_block(&environment);
        assert_eq!(String::from_utf16(&block).expect("UTF16"), "EMPTY=\0PATH=new\0\0");
        assert_eq!(environment_block(&environment_values(&[]).expect("empty env")), [0, 0]);
    }

    #[test]
    fn test_unsupported_or_invalid_inputs_are_explicit_errors() {
        assert!(command_line(&wide("tool.exe"), &[vec![65, 0, 66]], None).is_err());
        assert!(command_line(&wide("tool.exe"), &[vec![65; 32768]], None).is_err());
        assert!(environment_values(&[(wide("invalid=name"), wide("value"))]).is_err());
        assert!(environment_values(&[(wide("中文"), wide("value"))]).is_err());
    }

    #[test]
    fn test_missing_job_name_is_not_proof_that_members_finished() {
        assert!(!super::job_termination_proven(None));
        assert!(!super::job_termination_proven(Some(1)));
        assert!(super::job_termination_proven(Some(0)));
    }
}
