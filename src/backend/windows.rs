// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! QEMU/KVM Windows lifecycle and fixed-command SSH guest transport.

use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
#[cfg(unix)]
use std::io::BufRead;
#[cfg(unix)]
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use cap_fs_ext::DirExt;
use cap_fs_ext::FollowSymlinks;
use cap_fs_ext::OpenOptionsFollowExt;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use crate::backend::Backend;
use crate::guest::CHUNK;
use crate::guest::MAX_FRAME;
use crate::guest::Request;
use crate::guest::Response;
use crate::guest::VERSION;
use crate::lease::Lease;
use crate::model::Capability;
use crate::model::CleanupPolicy;
use crate::model::CleanupReport;
use crate::model::EnvironmentInfo;
use crate::model::ExecutionResult;
use crate::model::SessionContext;
use crate::model::SuiteSpec;
use crate::process;
use crate::process::ProcessSpec;
use crate::run_store::secure_dir;
use crate::transfer;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    qemu: PathBuf,
    image: PathBuf,
    qmp_socket: PathBuf,
    ssh_alias: String,
    guest_root: String,
    #[serde(default = "default_ssh")]
    ssh: PathBuf,
    #[serde(default = "default_memory")]
    memory_mib: u32,
    #[serde(default = "default_cpus")]
    cpus: u16,
    #[serde(default = "default_port")]
    ssh_port: u16,
    #[serde(default = "default_startup")]
    startup_timeout_seconds: u64,
    #[serde(default = "default_shutdown")]
    shutdown_timeout_seconds: u64,
}
fn default_ssh() -> PathBuf {
    "ssh".into()
}
fn default_memory() -> u32 {
    4096
}
fn default_cpus() -> u16 {
    2
}
fn default_port() -> u16 {
    2222
}
fn default_startup() -> u64 {
    180
}
fn default_shutdown() -> u64 {
    60
}

/// Owns only QEMU children it created; a connected preexisting instance is
/// borrowed.
pub(crate) struct WindowsBackend {
    settings: Option<Settings>,
    child: Option<Child>,
    lease: Option<Lease>,
    token: String,
    sequence: AtomicU64,
    context: Option<SessionContext>,
    prepared: bool,
    owned: Option<OwnedVm>,
    owned_stop_pending: bool,
}

impl WindowsBackend {
    /// Parses the strict local `[windows]` schema; absent configuration remains
    /// unconfigured. Invalid aliases, resource limits, socket locations, or
    /// unknown keys return errors.
    pub(crate) fn from_table(table: Option<&toml::Value>) -> Result<Self> {
        let settings: Option<Settings> = table
            .cloned()
            .map(toml::Value::try_into)
            .transpose()
            .context("invalid [windows] configuration")?;
        if let Some(settings) = &settings {
            ensure!(
                !settings.ssh_alias.is_empty()
                    && settings
                        .ssh_alias
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
                    && !settings.ssh_alias.starts_with('-'),
                "invalid Windows SSH host alias"
            );
            ensure!(
                settings.qemu.is_absolute() && settings.image.is_absolute() && settings.qmp_socket.is_absolute(),
                "Windows image and QMP socket must be absolute paths"
            );
            ensure!(
                settings
                    .qmp_socket
                    .file_name()
                    .is_some_and(|name| name == "windows.qmp")
                    && !settings.qmp_socket.to_string_lossy().contains(',')
                    && settings.qmp_socket.as_os_str().len() < 100,
                "QMP socket must be named windows.qmp in a dedicated tool directory"
            );
            ensure!(
                settings.memory_mib >= 512
                    && settings.cpus > 0
                    && settings.ssh_port > 0
                    && settings.startup_timeout_seconds > 0
                    && settings.shutdown_timeout_seconds > 0,
                "invalid Windows resource or timeout setting"
            );
            ensure!(
                !settings.guest_root.is_empty() && !settings.guest_root.contains('\0'),
                "invalid configured guest root"
            );
        }
        Ok(Self {
            settings,
            child: None,
            lease: None,
            token: String::new(),
            sequence: AtomicU64::new(0),
            context: None,
            prepared: false,
            owned: None,
            owned_stop_pending: false,
        })
    }

    fn acquire_vm(&mut self, context: &SessionContext) -> Result<Settings> {
        ensure!(self.context.is_none(), "Windows backend already started");
        let settings = self.settings.as_ref().context("Windows is unconfigured")?.clone();
        let parent = settings.qmp_socket.parent().context("QMP socket parent")?;
        let socket_dir = secure_dir(parent, true)?;
        const MARKER: &str = ".rs-infra-platform-qmp";
        match crate::run_store::read_nofollow(&socket_dir, MARKER) {
            Ok(bytes) => ensure!(bytes == b"windows-qmp-v1\n", "QMP directory marker mismatch"),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                ensure!(
                    socket_dir.entries()?.next().is_none(),
                    "refusing nonempty unmarked QMP directory"
                );
                let mut marker = socket_dir.open_with(
                    MARKER,
                    cap_std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .follow(FollowSymlinks::No),
                )?;
                marker.write_all(b"windows-qmp-v1\n")?;
            }
            Err(error) => return Err(error),
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        self.lease = Some(Lease::acquire(parent, "windows:qmp")?);
        self.context = Some(context.clone());
        if self.token.is_empty() {
            self.token = random_token()?;
        }
        Ok(settings)
    }

    fn connect(&mut self, context: &SessionContext, cleanup_only: bool) -> Result<Option<EnvironmentInfo>> {
        let settings = self.acquire_vm(context)?;
        if self.owned_stop_pending {
            match self.owned_alive()? {
                Some(true) => {}
                Some(false) => {
                    self.owned_stop_pending = false;
                    self.persist_session(context)?;
                    self.remove_stale_owned_socket(&settings.qmp_socket)?;
                }
                None => bail!("owned Windows VM identity is unknown; shutdown remains pending"),
            }
        }
        if settings.qmp_socket.exists() {
            if !self.owned_stop_pending {
                qmp(&settings.qmp_socket, "query-status")?;
            }
        } else if !self.owned_stop_pending {
            let stdout = File::create(context.report_dir.join("windows-qemu.stdout"))?;
            let stderr = File::create(context.report_dir.join("windows-qemu.stderr"))?;
            let token = random_token()?;
            let uuid = format!(
                "{}-{}-{}-{}-{}",
                &token[..8],
                &token[8..12],
                &token[12..16],
                &token[16..20],
                &token[20..32]
            );
            self.owned = Some(OwnedVm {
                host_id: crate::process_identity::host_id(),
                boot_id: crate::process_identity::boot_id(),
                pid: None,
                process_start: None,
                uuid: uuid.clone(),
            });
            self.owned_stop_pending = true;
            // A crash between spawn and recording PID/start is explicitly unknown, never
            // borrowed.
            self.persist_session(context)?;
            let block = json!({"driver":"qcow2","node-name":"disk","discard":"unmap","file":{"driver":"file","filename":settings.image}}).to_string();
            let spawned = Command::new(&settings.qemu)
                .args([
                    "-uuid",
                    &uuid,
                    "-machine",
                    "q35",
                    "-accel",
                    "kvm",
                    "-m",
                    &settings.memory_mib.to_string(),
                    "-smp",
                    &settings.cpus.to_string(),
                    "-display",
                    "none",
                    "-blockdev",
                    &block,
                    "-device",
                    "ide-hd,drive=disk",
                    "-netdev",
                    &format!("user,id=net0,hostfwd=tcp:127.0.0.1:{}-:22", settings.ssh_port),
                    "-device",
                    "e1000,netdev=net0",
                    "-qmp",
                    &format!("unix:{},server=on,wait=off", settings.qmp_socket.display()),
                ])
                .stdin(Stdio::null())
                .stdout(stdout)
                .stderr(stderr)
                .spawn();
            let child = match spawned {
                Ok(child) => child,
                Err(error) => {
                    self.owned_stop_pending = false;
                    self.persist_session(context)?;
                    return Err(error).context("start owned QEMU/KVM instance");
                }
            };
            let pid = child.id();
            self.child = Some(child);
            if let Some(owned) = &mut self.owned {
                owned.pid = Some(pid);
                owned.process_start = crate::process_identity::process_start(pid);
            }
            self.persist_session(context)?;
        }
        let start = Instant::now();
        let environment: Option<EnvironmentInfo> = loop {
            ensure!(
                !crate::executor::cancelled().load(Ordering::Relaxed),
                "Windows startup cancelled"
            );
            if let Some(child) = &mut self.child {
                ensure!(
                    child.try_wait()?.is_none(),
                    "owned QEMU exited before guest became ready"
                );
            }
            if self.owned_stop_pending
                && let Err(error) = qmp(&settings.qmp_socket, "query-status")
            {
                ensure!(
                    start.elapsed() < Duration::from_secs(settings.startup_timeout_seconds),
                    "owned QEMU QMP startup deadline exceeded: {error:#}"
                );
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let request = self.request(if cleanup_only { "status" } else { "probe" }, context);
            match self.rpc(&request, context, Duration::from_secs(35), crate::executor::cancelled()) {
                Ok(payload) => {
                    if cleanup_only {
                        ensure!(
                            payload["registered"] == true,
                            "guest did not confirm authenticated registration"
                        );
                        break None;
                    }
                    break Some(serde_json::from_value(payload).context("invalid Windows environment")?);
                }
                Err(error) if start.elapsed() >= Duration::from_secs(settings.startup_timeout_seconds) => {
                    return Err(error).context("Windows guest startup deadline exceeded");
                }
                Err(_) => std::thread::sleep(Duration::from_millis(250)),
            }
        };
        #[cfg(unix)]
        let environment = environment.map(|mut environment| {
            if let Some(bytes) = allocated_bytes(&settings.image) {
                environment
                    .versions
                    .insert("host_image_allocated_bytes".into(), bytes.to_string());
            }
            environment
        });
        Ok(environment)
    }

    fn owned_alive(&self) -> Result<Option<bool>> {
        let owned = self
            .owned
            .as_ref()
            .context("owned Windows VM registration is missing")?;
        let Some(pid) = owned.pid else {
            return Ok(None);
        };
        Ok(crate::process_identity::owner_alive(
            owned.host_id.as_deref(),
            owned.boot_id.as_deref(),
            pid,
            owned.process_start.as_deref(),
        ))
    }

    fn remove_stale_owned_socket(&self, path: &Path) -> Result<()> {
        #[cfg(not(unix))]
        let _ = path;
        self.lease
            .as_ref()
            .context("owned QMP cleanup requires VM lease")?
            .validate()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            match fs::symlink_metadata(path) {
                Ok(metadata) => {
                    ensure!(
                        metadata.file_type().is_socket(),
                        "refusing to unlink non-socket QMP path"
                    );
                    match std::os::unix::net::UnixStream::connect(path) {
                        Ok(_) => bail!(
                            "original owned VM exited but QMP socket is active; refusing to unlink another instance"
                        ),
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                            ) => {}
                        Err(error) => {
                            return Err(error).context("cannot establish stale QMP socket");
                        }
                    }
                    let parent = secure_dir(path.parent().context("QMP parent")?, false)?;
                    parent.remove_file(path.file_name().context("QMP filename")?)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn stopped(&mut self) -> Result<()> {
        self.owned_stop_pending = false;
        if let Some(context) = self.context.clone() {
            self.persist_session(&context)?;
        }
        self.lease.take();
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn stop_recovered(&mut self) -> Result<()> {
        use std::os::fd::AsRawFd;
        use std::os::fd::FromRawFd;
        use std::os::fd::OwnedFd;
        self.lease
            .as_ref()
            .context("recovered shutdown requires VM lease")?
            .validate()?;
        match self.owned_alive()? {
            Some(false) => return self.stopped(),
            None => bail!("owned Windows VM identity is unknown; shutdown remains pending"),
            Some(true) => {}
        }
        let owned = self.owned.as_ref().context("missing owned VM registration")?;
        let pid = owned.pid.context("missing owned VM PID")?;
        // SAFETY: pidfd_open returns a new owned descriptor for the observed process.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        ensure!(
            raw >= 0,
            "cannot obtain stable owned QEMU process handle: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: the successful syscall returned a new descriptor owned by this
        // function.
        let process = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        // Recheck after opening: the handle never signals a later reused PID.
        match self.owned_alive()? {
            Some(false) => return self.stopped(),
            Some(true) => {}
            None => bail!("owned QEMU creation identity is unknown after opening process handle"),
        }
        let settings = self.settings.as_ref().context("Windows settings unavailable")?.clone();
        let uuid = qmp(&settings.qmp_socket, "query-uuid")?;
        ensure!(
            uuid["UUID"].as_str() == Some(owned.uuid.as_str()),
            "owned QMP instance UUID mismatch; shutdown remains pending"
        );
        let exited = || -> Result<bool> {
            let mut descriptor = libc::pollfd {
                fd: process.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: this pollfd points to our retained process descriptor.
            let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
            ensure!(
                result >= 0,
                "poll owned QEMU handle: {}",
                std::io::Error::last_os_error()
            );
            Ok(result > 0 && descriptor.revents & libc::POLLIN != 0)
        };
        if qmp(&settings.qmp_socket, "system_powerdown").is_ok() {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(settings.shutdown_timeout_seconds) {
                if exited()? {
                    return self.stopped();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = qmp(&settings.qmp_socket, "quit");
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            if exited()? {
                self.stopped()?;
                bail!("recovered Windows normal shutdown unconfirmed; QEMU forcibly stopped");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: pidfd_send_signal targets only the process held by this stable
        // handle.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                process.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        ensure!(
            result == 0,
            "terminate recovered owned QEMU: {}",
            std::io::Error::last_os_error()
        );
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            if exited()? {
                self.stopped()?;
                bail!("recovered Windows normal shutdown failed; owned QEMU required termination");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        bail!("owned QEMU termination not confirmed; shutdown remains pending")
    }

    #[cfg(not(target_os = "linux"))]
    fn stop_recovered(&mut self) -> Result<()> {
        bail!("owned Windows VM recovery requires Linux process identity support")
    }

    fn persist_session(&self, context: &SessionContext) -> Result<()> {
        let settings = self.settings.as_ref().context("Windows is unconfigured")?;
        let session = Session {
            context: context.clone(),
            token: self.token.clone(),
            image: settings.image.clone(),
            qmp_socket: settings.qmp_socket.clone(),
            ssh_alias: settings.ssh_alias.clone(),
            guest_pending: self.prepared,
            owned_stop_pending: self.owned_stop_pending,
            owned: self.owned.clone(),
        };
        let directory = secure_dir(&context.report_dir, false)?;
        let temporary = format!("windows-session-{}.tmp", self.request("probe", context).request_id);
        let mut file = directory
            .open_with(
                &temporary,
                cap_std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .follow(FollowSymlinks::No),
            )?
            .into_std();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&serde_json::to_vec(&session)?)?;
        file.sync_all()?;
        drop(file);
        directory.rename(&temporary, &directory, "windows-session.json")?;
        Ok(())
    }

    fn request(&self, op: &str, context: &SessionContext) -> Request {
        Request {
            protocol_version: VERSION,
            request_id: format!(
                "r-{:x}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                self.sequence.fetch_add(1, Ordering::Relaxed)
            ),
            run_id: guest_run_id(context),
            lease_token: self.token.clone(),
            op: op.into(),
            action: None,
            manifest: None,
            path: None,
            offset: None,
            data: None,
            suite: None,
            project_relative: None,
            toolchain: None,
            cleanup: None,
        }
    }

    fn rpc(
        &self,
        request: &Request,
        context: &SessionContext,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<Value> {
        let input = request_frame(request)?;
        self.rpc_frame(request, context, timeout, cancelled, &input)
    }

    fn rpc_frame(
        &self,
        request: &Request,
        context: &SessionContext,
        timeout: Duration,
        cancelled: &AtomicBool,
        input: &[u8],
    ) -> Result<Value> {
        let settings = self.settings.as_ref().context("Windows is unconfigured")?;
        let spec = ProcessSpec {
            argv: vec![
                settings.ssh.to_string_lossy().into_owned(),
                "-T".into(),
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "StrictHostKeyChecking=yes".into(),
                "-o".into(),
                "ConnectTimeout=5".into(),
                settings.ssh_alias.clone(),
                "rs-infra-platform guest".into(),
            ],
            cwd: context.report_dir.clone(),
            env: BTreeMap::new(),
            timeout,
            stdout: context.report_dir.join(format!("windows-{}.json", request.request_id)),
            stderr: context
                .report_dir
                .join(format!("windows-{}.ssh.log", request.request_id)),
        };
        let execution = process::run_with_input(&spec, cancelled, input).context("Windows SSH transport")?;
        ensure!(
            execution.exit_code == Some(0) && !execution.cancelled && !execution.timed_out,
            "Windows SSH transport failed (exit={:?}, timeout={}, cancelled={}); cleanup may be pending",
            execution.exit_code,
            execution.timed_out,
            execution.cancelled
        );
        ensure!(
            fs::metadata(&spec.stdout)?.len() <= MAX_FRAME,
            "guest response exceeds frame limit"
        );
        let response: Response =
            serde_json::from_reader(File::open(&spec.stdout)?).context("invalid guest protocol response")?;
        ensure!(
            response.protocol_version == VERSION
                && response.request_id == request.request_id
                && response.run_id == request.run_id,
            "guest protocol identity or version mismatch"
        );
        ensure!(
            response.ok,
            "guest operation failed: {}",
            response.error.as_deref().unwrap_or("missing error")
        );
        Ok(response.payload)
    }

    fn begin_upload(&mut self, entries: &[transfer::Entry], context: &SessionContext) -> Result<()> {
        ensure!(!self.prepared, "guest prepare transaction already exists");
        let mut request = self.request("prepare", context);
        request.action = Some("start".into());
        request.manifest = Some(entries.to_vec());
        request.cleanup = Some(context.cleanup);
        let input = request_frame(&request)?;
        ensure!(
            !crate::executor::cancelled().load(Ordering::Relaxed),
            "Windows prepare cancelled before transmission"
        );
        self.prepared = true; // After local validation, a lost reply may hide registered artifacts.
        if let Err(error) = self.persist_session(context) {
            self.prepared = false;
            self.persist_session(context)
                .with_context(|| format!("{error:#}; failed to revoke unsent guest prepare intent"))?;
            return Err(error);
        }
        if let Err(error) = self.rpc_frame(
            &request,
            context,
            Duration::from_secs(30),
            &AtomicBool::new(false),
            &input,
        ) {
            if process::process_not_started(&error) {
                self.prepared = false;
                self.persist_session(context)
                    .with_context(|| format!("{error:#}; failed to persist definitely unsent guest request"))?;
            }
            return Err(error);
        }
        Ok(())
    }

    fn upload(&mut self, context: &SessionContext) -> Result<()> {
        let entries = transfer::manifest(&context.snapshot_root)?;
        self.begin_upload(&entries, context)?;
        for entry in &entries {
            if entry.directory {
                continue;
            }
            let mut file = File::open(context.snapshot_root.join(&entry.path))?;
            let mut offset = 0;
            loop {
                let mut bytes = vec![0; CHUNK];
                let length = file.read(&mut bytes)?;
                if length == 0 {
                    break;
                }
                bytes.truncate(length);
                let mut request = self.request("prepare", context);
                request.action = Some("chunk".into());
                request.path = Some(entry.path.clone());
                request.offset = Some(offset);
                request.data = Some(bytes);
                self.rpc(&request, context, Duration::from_secs(30), crate::executor::cancelled())?;
                offset += length as u64;
            }
        }
        let mut request = self.request("prepare", context);
        request.action = Some("seal".into());
        self.rpc(&request, context, Duration::from_secs(60), crate::executor::cancelled())?;
        Ok(())
    }

    fn fetch_log(&self, remote: &Path, context: &SessionContext) -> Result<PathBuf> {
        let name = remote
            .to_str()
            .and_then(|name| name.rsplit(['/', '\\']).next())
            .context("invalid guest log filename")?;
        ensure!(!name.contains(['/', '\\']), "invalid guest log filename");
        let local = context.report_dir.join(format!("windows-{name}"));
        let mut file = File::create(&local)?;
        let mut offset = 0;
        loop {
            let mut request = self.request("read", context);
            request.path = Some(name.into());
            request.offset = Some(offset);
            let payload = self.rpc(&request, context, Duration::from_secs(30), &AtomicBool::new(false))?;
            let data: Vec<u8> = serde_json::from_value(payload["data"].clone()).context("invalid guest log chunk")?;
            ensure!(data.len() <= CHUNK, "guest log chunk exceeds limit");
            file.write_all(&data)?;
            offset += data.len() as u64;
            if payload["eof"] == true {
                break;
            }
            ensure!(!data.is_empty(), "guest log transfer made no progress");
        }
        file.sync_all()?;
        Ok(local)
    }
}

impl Backend for WindowsBackend {
    fn probe(&self) -> Result<Capability> {
        let Some(settings) = &self.settings else {
            return Ok(Capability::Unconfigured(
                "configure a Windows VM, SSH host alias and guest runner".into(),
            ));
        };
        if !cfg!(target_os = "linux") {
            return Ok(Capability::Unsupported(
                "Windows VM backend requires a Linux KVM host".into(),
            ));
        }
        if !settings.image.is_file() {
            return Ok(Capability::Broken(format!(
                "Windows VM image not found: {}",
                settings.image.display()
            )));
        }
        if !settings.qemu.is_file() {
            return Ok(Capability::Broken(format!(
                "QEMU program not found: {}",
                settings.qemu.display()
            )));
        }
        match kvm_probe() {
            Ok(()) => Ok(Capability::Ready),
            Err(error) => Ok(Capability::Broken(format!("KVM initialization failed: {error:#}"))),
        }
    }

    fn start(&mut self, context: &SessionContext) -> Result<EnvironmentInfo> {
        let environment = self.connect(context, false)?.context("missing execution environment")?;
        self.persist_session(context)?;
        let actual_root = environment
            .versions
            .get("guest_root")
            .context("guest did not report its configured root")?;
        ensure!(
            actual_root
                .replace('/', "\\")
                .trim_end_matches('\\')
                .eq_ignore_ascii_case(
                    self.settings
                        .as_ref()
                        .context("Windows is unconfigured")?
                        .guest_root
                        .replace('/', "\\")
                        .trim_end_matches('\\')
                ),
            "configured Windows guest_root differs from guest registration root"
        );
        self.upload(context)?;
        Ok(environment)
    }

    fn execute(&mut self, suite: &SuiteSpec, context: &SessionContext) -> Result<Vec<ExecutionResult>> {
        let mut request = self.request("execute", context);
        request.suite = Some(suite.clone());
        request.toolchain = Some(context.toolchain.clone());
        request.project_relative = Some(
            context
                .project_relative
                .to_str()
                .context("non-UTF-8 project layout")?
                .replace('\\', "/"),
        );
        let timeout = Duration::from_secs(
            suite
                .timeout_seconds
                .saturating_mul(suite.commands.len() as u64)
                .saturating_add(60),
        );
        let payload = match self.rpc(&request, context, timeout, crate::executor::cancelled()) {
            Ok(payload) => payload,
            Err(error) => {
                let cancel = self.request("cancel", context);
                let _ = self.rpc(&cancel, context, Duration::from_secs(15), &AtomicBool::new(false));
                return Err(error);
            }
        };
        let mut results: Vec<ExecutionResult> =
            serde_json::from_value(payload["results"].clone()).context("missing structured guest execution results")?;
        let infrastructure_error = match payload.get("infrastructure_error") {
            Some(value) => Some(value.as_str().context("invalid guest infrastructure error")?.to_owned()),
            None => None,
        };
        ensure!(
            !results.is_empty() || infrastructure_error.is_some(),
            "guest returned no execution results"
        );
        let archive = (|| -> Result<()> {
            for result in &mut results {
                result.stdout = self.fetch_log(&result.stdout, context)?;
                result.stderr = self.fetch_log(&result.stderr, context)?;
            }
            Ok(())
        })();
        archive.map_err(|error| crate::report::with_completed_results(error, &results))?;
        if let Some(error) = infrastructure_error {
            return Err(crate::report::with_completed_results(
                anyhow::anyhow!("guest infrastructure failure: {error}"),
                &results,
            ));
        }
        Ok(results)
    }

    fn cleanup(&mut self, context: &SessionContext) -> Result<CleanupReport> {
        if !self.prepared {
            return Ok(CleanupReport::default());
        }
        if context.cleanup == CleanupPolicy::Disabled {
            return Ok(CleanupReport {
                remaining: vec![PathBuf::from(format!("windows:{}", guest_run_id(context)))],
                ..CleanupReport::default()
            });
        }
        let request = self.request("cleanup", context);
        let before = self
            .settings
            .as_ref()
            .and_then(|settings| allocated_bytes(&settings.image));
        match self.rpc(&request, context, Duration::from_secs(60), &AtomicBool::new(false)) {
            Ok(value) => {
                let mut report: CleanupReport =
                    serde_json::from_value(value).context("invalid guest cleanup report")?;
                if report.errors.is_empty()
                    && let (Some(before), Some(after)) = (
                        before,
                        self.settings
                            .as_ref()
                            .and_then(|settings| allocated_bytes(&settings.image)),
                    )
                {
                    report.host_reclaimed_bytes = Some(before.saturating_sub(after));
                }
                if report.errors.is_empty() {
                    self.prepared = false;
                    self.persist_session(context)?;
                }
                Ok(report)
            }
            Err(error) => Ok(CleanupReport {
                remaining: vec![PathBuf::from(format!("windows:{}", guest_run_id(context)))],
                errors: vec![format!("Windows guest cleanup-pending: {error:#}")],
                ..CleanupReport::default()
            }),
        }
    }

    fn stop(&mut self) -> Result<()> {
        let Some(child) = self.child.take() else {
            if self.owned_stop_pending {
                return self.stop_recovered();
            }
            self.lease.take();
            return Ok(());
        };
        let mut child = StopGuard { child };
        let settings = self.settings.as_ref().context("missing owned Windows settings")?;
        let powerdown = qmp(&settings.qmp_socket, "system_powerdown");
        let start = Instant::now();
        if powerdown.is_ok() {
            while start.elapsed() < Duration::from_secs(settings.shutdown_timeout_seconds) {
                if child.child.try_wait()?.is_some() {
                    self.stopped()?;
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = qmp(&settings.qmp_socket, "quit");
        let forced = Instant::now();
        while forced.elapsed() < Duration::from_secs(3) {
            if child.child.try_wait()?.is_some() {
                self.stopped()?;
                bail!("Windows normal shutdown was not confirmed; QEMU forcibly stopped");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        child
            .child
            .kill()
            .context("terminate owned QEMU after failed shutdown")?;
        child.child.wait().context("reap owned QEMU")?;
        self.stopped()?;
        bail!("Windows normal shutdown failed; owned QEMU required termination")
    }
}

struct StopGuard {
    child: Child,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        // Also runs on an unexpected polling or QMP error after ownership was taken.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for WindowsBackend {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.stop();
        }
    }
}

#[cfg(target_os = "linux")]
fn kvm_probe() -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    let kvm = fs::OpenOptions::new().read(true).write(true).open("/dev/kvm")?;
    // SAFETY: KVM_CREATE_VM takes an integer machine type and returns a fresh owned
    // fd.
    let descriptor = unsafe { libc::ioctl(kvm.as_raw_fd(), 0xae01, 0) };
    ensure!(descriptor >= 0, "KVM_CREATE_VM: {}", std::io::Error::last_os_error());
    // SAFETY: this successful ioctl returned a new fd, owned by this probe only.
    drop(unsafe { OwnedFd::from_raw_fd(descriptor) });
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn kvm_probe() -> Result<()> {
    bail!("KVM requires Linux")
}

fn request_frame(request: &Request) -> Result<Vec<u8>> {
    let input = serde_json::to_vec(request).context("serialize guest request")?;
    ensure!(input.len() as u64 <= MAX_FRAME, "guest request exceeds frame limit");
    Ok(input)
}

#[cfg(unix)]
fn allocated_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path)
        .ok()
        .map(|metadata| metadata.blocks().saturating_mul(512))
}
#[cfg(not(unix))]
fn allocated_bytes(_path: &Path) -> Option<u64> {
    None
}

fn guest_run_id(context: &SessionContext) -> String {
    let entry = context.report_dir.file_name().unwrap_or_default().to_string_lossy();
    let suffix = Sha256::digest(entry.as_bytes());
    format!(
        "{}-{}",
        context.run_id.0,
        suffix[..8].iter().map(|byte| format!("{byte:02x}")).collect::<String>()
    )
}

fn random_token() -> Result<String> {
    #[cfg(unix)]
    {
        let mut bytes = [0; 32];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }
    #[cfg(not(unix))]
    {
        bail!("Windows VM coordinator requires Linux")
    }
}

#[cfg(unix)]
fn qmp(path: &Path, operation: &str) -> Result<Value> {
    use std::os::unix::net::UnixStream;
    let stream = UnixStream::connect(path).context("connect owned QMP socket")?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let greeting = qmp_frame(&mut reader)?;
    ensure!(greeting.get("QMP").is_some(), "missing QMP greeting");
    let mut writer = stream;
    for (id, execute) in [("capabilities", "qmp_capabilities"), ("command", operation)] {
        writeln!(writer, "{}", json!({"execute":execute,"id":id}))?;
        for _ in 0..128 {
            let frame = qmp_frame(&mut reader)?;
            if frame.get("event").is_some() {
                continue;
            }
            ensure!(frame["id"] == id, "QMP response identity mismatch");
            ensure!(frame.get("error").is_none(), "QMP command error: {frame}");
            if id == "command" {
                return frame.get("return").cloned().context("missing QMP return");
            }
            break;
        }
    }
    bail!("missing QMP command response")
}

#[cfg(unix)]
fn qmp_frame(reader: &mut BufReader<std::os::unix::net::UnixStream>) -> Result<Value> {
    let mut bytes = Vec::new();
    let count = reader.take(64 * 1024 + 1).read_until(b'\n', &mut bytes)?;
    ensure!(
        count > 0 && count <= 64 * 1024 && bytes.last() == Some(&b'\n'),
        "missing or oversized QMP frame"
    );
    serde_json::from_slice(&bytes).context("invalid QMP JSON frame")
}
#[cfg(not(unix))]
fn qmp(_path: &Path, _operation: &str) -> Result<Value> {
    bail!("QMP Unix sockets require a Linux host")
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Session {
    context: SessionContext,
    token: String,
    image: PathBuf,
    qmp_socket: PathBuf,
    ssh_alias: String,
    #[serde(alias = "pending")]
    guest_pending: bool,
    #[serde(default)]
    owned_stop_pending: bool,
    #[serde(default)]
    owned: Option<OwnedVm>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OwnedVm {
    host_id: Option<String>,
    boot_id: Option<String>,
    pid: Option<u32>,
    process_start: Option<String>,
    uuid: String,
}

/// Stops only registered owned VMs; guest files and tokens remain retained.
/// Unknown identities and unconfirmed shutdown return errors without clearing
/// pending state.
pub(crate) fn recover_stop(report_dir: &Path, table: Option<&toml::Value>) -> Result<()> {
    recover_sessions(report_dir, table, false)
}

/// Recovers registered remote runs before local artifacts are reclaimed.
/// Reads durable entry reports, verifies the current VM identity, and shuts
/// down only QEMU instances created by this recovery. Connection or cleanup
/// failures return errors.
pub(crate) fn recover_cleanup(report_dir: &Path, table: Option<&toml::Value>) -> Result<()> {
    recover_sessions(report_dir, table, true)
}

fn recover_sessions(report_dir: &Path, table: Option<&toml::Value>, cleanup_guest: bool) -> Result<()> {
    let directory = secure_dir(report_dir, false)?;
    for entry in directory.entries()? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("entry-") {
            continue;
        }
        let child = directory.open_dir_nofollow(entry.file_name())?;
        let bytes = match crate::run_store::read_nofollow(&child, "windows-session.json") {
            Ok(bytes) => bytes,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let session: Session = serde_json::from_slice(&bytes).context("invalid durable Windows session")?;
        if !session.owned_stop_pending && (!cleanup_guest || !session.guest_pending) {
            continue;
        }
        let mut backend = WindowsBackend::from_table(table)?;
        let settings = backend
            .settings
            .as_ref()
            .context("Windows cleanup-pending requires [windows] configuration")?;
        ensure!(
            settings.image == session.image
                && settings.qmp_socket == session.qmp_socket
                && settings.ssh_alias == session.ssh_alias,
            "Windows cleanup session machine identity differs from configuration"
        );
        let mut context = session.context;
        context.report_dir = report_dir.join(entry.file_name());
        if cleanup_guest {
            context.cleanup = CleanupPolicy::Enabled;
        }
        backend.token = session.token;
        backend.prepared = session.guest_pending;
        backend.owned_stop_pending = session.owned_stop_pending;
        backend.owned = session.owned;
        let recovery = (|| {
            if cleanup_guest && backend.prepared {
                backend.connect(&context, true)?;
                let cleanup = backend.cleanup(&context)?;
                ensure!(
                    cleanup.errors.is_empty(),
                    "Windows cleanup-pending: {}",
                    cleanup.errors.join("; ")
                );
            } else {
                backend.acquire_vm(&context)?;
            }
            Ok(())
        })();
        let stop = backend.stop();
        recovery?;
        stop?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use serde_json::json;

    use super::WindowsBackend;
    use super::qmp;
    use crate::backend::Backend;
    use crate::model::Capability;
    use crate::model::CleanupPolicy;
    use crate::model::CommandKind;
    use crate::model::CommandSpec;
    use crate::model::RunId;
    use crate::model::SessionContext;
    use crate::model::SuiteSpec;

    fn fixture(root: &Path, script: &str) -> (WindowsBackend, SessionContext) {
        fs::write(root.join("base image.qcow2"), "disposable test image").expect("image fixture");
        let ssh = root.join("fake ssh");
        fs::write(&ssh, format!("#!/usr/bin/python3\n{script}")).expect("fake SSH");
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).expect("executable SSH");
        let value = toml::Value::try_from(json!({"qemu":"/usr/bin/qemu-system-x86_64","image":root.join("base image.qcow2"),"qmp_socket":root.join("qmp/windows.qmp"),"ssh_alias":"windows-test","guest_root":"C:\\infra","ssh":ssh})).expect("settings");
        let mut backend = WindowsBackend::from_table(Some(&value)).expect("backend");
        backend.token = "00".repeat(32);
        let context = SessionContext {
            run_id: RunId("aabb".into()),
            host_run_dir: root.into(),
            snapshot_root: root.join("snapshot"),
            project_relative: "project".into(),
            report_dir: root.into(),
            toolchain: "stable".into(),
            cleanup: CleanupPolicy::Enabled,
        };
        (backend, context)
    }

    #[test]
    fn test_unconfigured_differs_from_invalid_config() {
        assert!(matches!(
            WindowsBackend::from_table(None)
                .expect("unconfigured")
                .probe()
                .expect("probe"),
            Capability::Unconfigured(_)
        ));
        let root = tempfile::tempdir().expect("fixture");
        let (mut backend, _) = fixture(root.path(), "");
        backend.settings.as_mut().expect("settings").image = root.path().join("missing-image");
        let mut settings = toml::Value::try_from(json!({"qemu":"/bin/true","image":"/missing","qmp_socket":"/tmp/qmp/windows.qmp","ssh_alias":"-oProxyCommand=bad","guest_root":"C:\\infra"})).expect("settings");
        assert!(WindowsBackend::from_table(Some(&settings)).is_err());
        settings["ssh_alias"] = "safe".into();
        settings
            .as_table_mut()
            .expect("table")
            .insert("typo".into(), true.into());
        assert!(WindowsBackend::from_table(Some(&settings)).is_err());
        assert!(matches!(
            backend.probe().expect("configured probe"),
            Capability::Broken(_)
        ));
    }

    #[test]
    fn test_ssh_keeps_untrusted_suite_argv_in_stdin_and_preserves_exit_nine() {
        let root = tempfile::tempdir().expect("fixture");
        let script = r#"import sys,json,pathlib
r=json.load(sys.stdin)
with open('captured.jsonl','a') as f: f.write(json.dumps({'argv':sys.argv[1:],'request':r})+'\n')
p={'results':[{'exit_code':9,'timed_out':False,'cancelled':False,'tests_passed':0,'tests_failed':1,'tests_ignored':0,'stdout':'C:\\reports\\case.stdout','stderr':'C:\\reports\\case.stderr'}]} if r['op']=='execute' else {'data':list(b'log {"ok":true}'), 'eof':True}
print(json.dumps({'protocol_version':1,'request_id':r['request_id'],'run_id':r['run_id'],'ok':True,'payload':p,'error':None}))
"#;
        let (mut backend, context) = fixture(root.path(), script);
        let malicious = "space ' \" ; $(touch bad)";
        let suite = SuiteSpec {
            id: "suite".into(),
            platforms: vec![],
            target: None,
            commands: vec![CommandSpec {
                kind: CommandKind::Test,
                argv: vec!["cargo".into(), malicious.into()],
                cwd: ".".into(),
            }],
            timeout_seconds: 1,
            minimum_tests: 1,
            requirements: vec![],
            data: vec![],
        };
        let results = backend.execute(&suite, &context).expect("execute structured response");
        assert_eq!(results[0].exit_code, Some(9));
        let capture = fs::read_to_string(root.path().join("captured.jsonl")).expect("captured SSH");
        let first: serde_json::Value =
            serde_json::from_str(capture.lines().next().expect("capture line")).expect("capture JSON");
        assert_eq!(
            first["argv"].as_array().expect("argv").last().expect("fixed command"),
            "rs-infra-platform guest"
        );
        assert!(!first["argv"].to_string().contains("touch bad"));
        assert_eq!(first["request"]["suite"]["commands"][0]["argv"][1], malicious);
        assert_eq!(
            fs::read_to_string(&results[0].stdout).expect("downloaded log"),
            "log {\"ok\":true}"
        );
    }

    #[test]
    fn test_ssh_failure_and_protocol_mismatch_are_infrastructure_errors() {
        for script in ["import sys\nsys.exit(9)\n", "print('{}')\n"] {
            let root = tempfile::tempdir().expect("fixture");
            let (backend, context) = fixture(root.path(), script);
            let error = backend
                .rpc(
                    &backend.request("probe", &context),
                    &context,
                    Duration::from_secs(1),
                    &AtomicBool::new(false),
                )
                .expect_err("transport or protocol must fail");
            assert!(format!("{error:#}").contains(if script.contains("exit") {
                "transport failed"
            } else {
                "invalid guest protocol response"
            }));
        }
    }

    #[test]
    fn test_qmp_handles_events_and_validates_response_identity() {
        for wrong_id in [false, true] {
            let root = tempfile::tempdir().expect("QMP fixture");
            let socket = root.path().join("qmp");
            let listener = UnixListener::bind(&socket).expect("QMP socket");
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("QMP connection");
                writeln!(stream, "{}", json!({"QMP":{"version":{}}})).expect("greeting");
                let mut reader = BufReader::new(stream.try_clone().expect("reader"));
                let mut line = String::new();
                reader.read_line(&mut line).expect("capabilities");
                writeln!(stream, "{}", json!({"event":"READY"})).expect("event");
                writeln!(stream, "{}", json!({"return":{},"id":"capabilities"})).expect("capabilities response");
                line.clear();
                reader.read_line(&mut line).expect("command");
                writeln!(
                    stream,
                    "{}",
                    json!({"return":{},"id":if wrong_id {"other"} else {"command"}})
                )
                .expect("command response");
            });
            let result = qmp(&socket, "system_powerdown");
            assert_eq!(result.is_err(), wrong_id);
            server.join().expect("QMP server");
        }
    }

    #[test]
    fn test_borrowed_stop_never_contacts_qmp() {
        let root = tempfile::tempdir().expect("fixture");
        let (mut backend, _) = fixture(root.path(), "");
        let socket = root.path().join("qmp/windows.qmp");
        fs::create_dir_all(socket.parent().expect("parent")).expect("socket directory");
        let listener = UnixListener::bind(&socket).expect("borrowed socket");
        listener.set_nonblocking(true).expect("nonblocking listener");
        backend.stop().expect("borrowed stop");
        assert_eq!(
            listener.accept().expect_err("must not shut borrowed VM down").kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn test_vm_lease_contention_prevents_start() {
        let root = tempfile::tempdir().expect("fixture");
        let (mut backend, context) = fixture(root.path(), "");
        let parent = root.path().join("qmp");
        fs::create_dir_all(&parent).expect("QMP root");
        fs::write(parent.join(".rs-infra-platform-qmp"), "windows-qmp-v1\n").expect("QMP owner marker");
        let _lease = crate::lease::Lease::acquire(&parent, "windows:qmp").expect("first coordinator");
        let error = backend.start(&context).expect_err("second coordinator must be refused");
        assert!(format!("{error:#}").contains("lease already held"));
    }

    #[test]
    fn test_powerdown_ack_without_exit_is_shutdown_failure() {
        let root = tempfile::tempdir().expect("fixture");
        let (mut backend, _) = fixture(root.path(), "");
        let settings = backend.settings.as_mut().expect("settings");
        settings.shutdown_timeout_seconds = 1;
        fs::create_dir_all(settings.qmp_socket.parent().expect("QMP parent")).expect("QMP root");
        let listener = UnixListener::bind(&settings.qmp_socket).expect("QMP socket");
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("QMP connection");
                writeln!(stream, "{}", json!({"QMP":{}})).expect("greeting");
                let mut reader = BufReader::new(stream.try_clone().expect("reader"));
                let mut line = String::new();
                reader.read_line(&mut line).expect("capabilities");
                writeln!(stream, "{}", json!({"return":{},"id":"capabilities"})).expect("capabilities response");
                line.clear();
                reader.read_line(&mut line).expect("operation");
                writeln!(stream, "{}", json!({"return":{},"id":"command"})).expect("operation response");
            }
        });
        backend.child = Some(
            std::process::Command::new("/usr/bin/python3")
                .args(["-c", "import time;time.sleep(30)"])
                .spawn()
                .expect("owned VM stand-in"),
        );
        let error = backend.stop().expect_err("an ACK does not establish guest shutdown");
        assert!(format!("{error:#}").contains("normal shutdown failed"));
        assert!(backend.child.is_none());
        server.join().expect("QMP server");
    }

    #[test]
    fn test_remote_recovery_uses_durable_token_and_preserves_borrowed_vm() {
        let root = tempfile::tempdir().expect("recovery fixture");
        let reports = root.path().join("reports");
        let entry = reports.join("entry-0");
        fs::create_dir_all(&entry).expect("entry report directory");
        let script = r#"import sys,json
r=json.load(sys.stdin)
p={'registered':True} if r['op']=='status' else {'remaining':[],'errors':[],'host_reclaimed_bytes':None,'guest_available_delta':None}
with open('recovery-requests.jsonl','a') as f: f.write(json.dumps(r)+'\n')
print(json.dumps({'protocol_version':1,'request_id':r['request_id'],'run_id':r['run_id'],'ok':r['op']!='probe','payload':p,'error':'MSVC unavailable; disk full' if r['op']=='probe' else None}))
"#;
        let (mut backend, mut context) = fixture(root.path(), script);
        context.host_run_dir = root.path().join("already-cleaned-host-run");
        context.report_dir = entry.clone();
        backend.prepared = true;
        backend.persist_session(&context).expect("durable remote session");
        backend.settings.as_mut().expect("settings").startup_timeout_seconds = 1;
        let settings = backend.settings.as_ref().expect("settings");
        let table = toml::Value::try_from(settings).expect("recovery settings");
        let socket = settings.qmp_socket.clone();
        fs::create_dir_all(socket.parent().expect("QMP directory")).expect("QMP directory");
        fs::write(
            socket.parent().expect("parent").join(".rs-infra-platform-qmp"),
            "windows-qmp-v1\n",
        )
        .expect("QMP marker");
        let listener = UnixListener::bind(&socket).expect("borrowed QMP");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("QMP query");
            writeln!(stream, "{}", json!({"QMP":{}})).expect("greeting");
            let mut reader = BufReader::new(stream.try_clone().expect("reader"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("capabilities");
            writeln!(stream, "{}", json!({"return":{},"id":"capabilities"})).expect("capabilities response");
            line.clear();
            reader.read_line(&mut line).expect("query status");
            writeln!(stream, "{}", json!({"return":{"status":"running"},"id":"command"})).expect("status response");
            listener.set_nonblocking(true).expect("nonblocking listener");
            listener
        });
        super::recover_cleanup(&reports, Some(&table)).expect("remote cleanup recovery after host artifacts vanished");
        let listener = server.join().expect("QMP server");
        assert_eq!(
            listener.accept().expect_err("borrowed VM must remain running").kind(),
            std::io::ErrorKind::WouldBlock
        );
        let session: serde_json::Value =
            serde_json::from_slice(&fs::read(entry.join("windows-session.json")).expect("durable session"))
                .expect("session JSON");
        assert_eq!(session["guest_pending"], false);
        let requests = fs::read_to_string(entry.join("recovery-requests.jsonl")).expect("requests");
        for line in requests.lines() {
            let request: serde_json::Value = serde_json::from_str(line).expect("request JSON");
            assert_eq!(request["lease_token"], "00".repeat(32));
        }
    }

    #[test]
    fn test_recovery_stops_owned_vm_even_after_guest_cleanup_was_durable() {
        let root = tempfile::tempdir().expect("ownership recovery fixture");
        let reports = root.path().join("reports");
        let entry = reports.join("entry-0");
        fs::create_dir_all(&entry).expect("entry reports");
        let (backend, mut context) = fixture(root.path(), "import sys;sys.exit(9)");
        context.report_dir = entry.clone();
        backend.persist_session(&context).expect("session");
        let mut child = std::process::Command::new("/usr/bin/python3")
            .args(["-c", "import time;time.sleep(30)"])
            .spawn()
            .expect("owned VM fixture");
        let pid = child.id();
        let mut session: serde_json::Value =
            serde_json::from_slice(&fs::read(entry.join("windows-session.json")).expect("session bytes"))
                .expect("session JSON");
        session.as_object_mut().expect("session object").remove("pending");
        session["guest_pending"] = false.into();
        session["owned_stop_pending"] = true.into();
        session["owned"] = json!({"host_id":crate::process_identity::host_id(),"boot_id":crate::process_identity::boot_id(),"pid":pid,"process_start":crate::process_identity::process_start(pid),"uuid":"12345678-1234-1234-1234-123456789abc"});
        fs::write(
            entry.join("windows-session.json"),
            serde_json::to_vec(&session).expect("serialize session"),
        )
        .expect("owned session");
        let settings = backend.settings.as_ref().expect("settings");
        let table = toml::Value::try_from(settings).expect("settings table");
        let parent = settings.qmp_socket.parent().expect("QMP parent");
        fs::create_dir_all(parent).expect("QMP directory");
        fs::write(parent.join(".rs-infra-platform-qmp"), "windows-qmp-v1\n").expect("QMP owner marker");
        let listener = UnixListener::bind(&settings.qmp_socket).expect("owned QMP");
        listener.set_nonblocking(true).expect("nonblocking listener");
        let server = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let mut operations = Vec::new();
            while start.elapsed() < Duration::from_secs(4) && operations.len() < 2 {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("QMP accept: {error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .expect("QMP deadline");
                writeln!(stream, "{}", json!({"QMP":{}})).expect("greeting");
                let mut reader = BufReader::new(stream.try_clone().expect("reader"));
                let mut line = String::new();
                reader.read_line(&mut line).expect("capabilities");
                writeln!(stream, "{}", json!({"return":{},"id":"capabilities"})).expect("capabilities response");
                line.clear();
                reader.read_line(&mut line).expect("QMP operation");
                let operation: serde_json::Value = serde_json::from_str(&line).expect("QMP request");
                let execute = operation["execute"].as_str().expect("execute");
                operations.push(execute.to_owned());
                let payload = if execute == "query-uuid" {
                    json!({"UUID":"12345678-1234-1234-1234-123456789abc"})
                } else {
                    json!({})
                };
                writeln!(stream, "{}", json!({"return":payload,"id":"command"})).expect("QMP operation response");
                if execute == "system_powerdown" {
                    // SAFETY: pid names the child created by this test and still held above.
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                }
            }
            operations
        });
        let recovery = super::recover_cleanup(&reports, Some(&table));
        let _ = child.kill();
        let _ = child.wait();
        let operations = server.join().expect("QMP server");
        recovery.expect("owned VM must be recovered even when guest_pending is false");
        assert_eq!(operations, ["query-uuid", "system_powerdown"]);
        let session: serde_json::Value =
            serde_json::from_slice(&fs::read(entry.join("windows-session.json")).expect("updated session"))
                .expect("session JSON");
        assert_eq!(session["owned_stop_pending"], false);
    }

    #[test]
    fn test_unknown_owned_identity_keeps_shutdown_pending_without_connecting() {
        let root = tempfile::tempdir().expect("unknown ownership fixture");
        let reports = root.path().join("reports");
        let entry = reports.join("entry-0");
        fs::create_dir_all(&entry).expect("entry");
        let (mut backend, mut context) = fixture(root.path(), "import sys;sys.exit(9)");
        context.report_dir = entry.clone();
        backend.owned_stop_pending = true;
        backend.owned = Some(super::OwnedVm {
            host_id: crate::process_identity::host_id(),
            boot_id: crate::process_identity::boot_id(),
            pid: None,
            process_start: None,
            uuid: "12345678-1234-1234-1234-123456789abc".into(),
        });
        backend.persist_session(&context).expect("unknown ownership intent");
        let table = toml::Value::try_from(backend.settings.as_ref().expect("settings")).expect("settings table");
        let error = super::recover_cleanup(&reports, Some(&table))
            .expect_err("unknown spawned process must not become borrowed or complete");
        assert!(format!("{error:#}").contains("identity is unknown"));
        let session: serde_json::Value =
            serde_json::from_slice(&fs::read(entry.join("windows-session.json")).expect("preserved session"))
                .expect("session JSON");
        assert_eq!(session["owned_stop_pending"], true);
    }

    #[test]
    #[ignore = "subprocess fixture is killed to model controller SIGKILL"]
    fn test_vm_owner_crash_fixture() {
        let Some(root) = std::env::var_os("RS_INFRA_WINDOWS_CRASH_FIXTURE") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let (mut backend, mut context) = fixture(
            &root,
            r#"import json,sys
r=json.load(sys.stdin)
print(json.dumps({'protocol_version':1,'request_id':r['request_id'],'run_id':r['run_id'],'ok':True,'payload':{'registered':True},'error':None}))
"#,
        );
        context.report_dir = root.join("reports/entry-0");
        fs::create_dir_all(&context.report_dir).expect("entry reports");
        backend.settings.as_mut().expect("settings").qemu = root.join("fake-qemu");
        backend.connect(&context, true).expect("owned fake QEMU startup");
        fs::write(root.join("controller-ready"), b"ready").expect("controller ready");
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    #[test]
    fn test_controller_sigkill_preserves_owned_vm_shutdown_registration() {
        let root = tempfile::tempdir().expect("controller crash fixture");
        let qemu = root.path().join("fake-qemu");
        fs::write(
            &qemu,
            r#"#!/usr/bin/python3
import sys,json,socket,pathlib
args=sys.argv[1:]
path=args[args.index('-qmp')+1][5:].split(',')[0]
uuid=args[args.index('-uuid')+1]
s=socket.socket(socket.AF_UNIX);s.bind(path);s.listen()
stopping=False
while not stopping:
 c,_=s.accept();f=c.makefile('rwb',buffering=0)
 f.write((json.dumps({'QMP':{}})+'\n').encode())
 json.loads(f.readline());f.write((json.dumps({'return':{},'id':'capabilities'})+'\n').encode())
 r=json.loads(f.readline());op=r['execute']
 payload={'UUID':uuid} if op=='query-uuid' else {'status':'running'}
 f.write((json.dumps({'return':payload,'id':'command'})+'\n').encode())
 c.close();stopping=op in ['system_powerdown','quit']
s.close();pathlib.Path(path).unlink()
"#,
        )
        .expect("fake QEMU");
        fs::set_permissions(&qemu, fs::Permissions::from_mode(0o700)).expect("executable QEMU");
        let mut controller = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "backend::windows::tests::test_vm_owner_crash_fixture",
                "--ignored",
            ])
            .env("RS_INFRA_WINDOWS_CRASH_FIXTURE", root.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("controller fixture");
        let started = std::time::Instant::now();
        while !root.path().join("controller-ready").exists() && started.elapsed() < Duration::from_secs(5) {
            if controller.try_wait().expect("poll controller").is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let ready = root.path().join("controller-ready").exists();
        controller.kill().expect("SIGKILL controller");
        controller.wait().expect("reap controller");
        assert!(ready, "controller must have persisted its real spawned child identity");
        let entry = root.path().join("reports/entry-0");
        let mut session: serde_json::Value =
            serde_json::from_slice(&fs::read(entry.join("windows-session.json")).expect("durable ownership"))
                .expect("session JSON");
        assert_eq!(session["owned_stop_pending"], true);
        assert!(
            session["owned"]["process_start"]
                .as_str()
                .is_some_and(|start| !start.is_empty())
        );
        session["guest_pending"] = true.into();
        session["context"]["cleanup"] = "disabled".into();
        fs::write(
            entry.join("windows-session.json"),
            serde_json::to_vec(&session).expect("retained session"),
        )
        .expect("persist retained session");
        fs::write(root.path().join("retained-guest-input"), "retain source and target").expect("retained fixture");
        let original_token = session["token"].clone();
        let (mut backend, _) = fixture(root.path(), "import sys;sys.exit(9)");
        backend.settings.as_mut().expect("settings").qemu = qemu;
        let table = toml::Value::try_from(backend.settings.as_ref().expect("settings")).expect("settings table");
        let recovery = super::recover_stop(&root.path().join("reports"), Some(&table));
        if recovery.is_err()
            && let Some(pid) = session["owned"]["pid"].as_u64()
        {
            // SAFETY: this failure cleanup targets only the recorded fake-QEMU test child.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        recovery.expect("SIGKILL recovery must stop original owned QEMU without a live Child handle");
        let session: serde_json::Value =
            serde_json::from_slice(&fs::read(entry.join("windows-session.json")).expect("recovered session"))
                .expect("session JSON");
        if session["owned_stop_pending"] != false
            && let Some(pid) = session["owned"]["pid"].as_u64()
        {
            // SAFETY: dispose only this test's fake-QEMU process after a failing assertion.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        assert_eq!(session["owned_stop_pending"], false);
        assert_eq!(session["guest_pending"], true);
        assert_eq!(session["token"], original_token);
        assert_eq!(
            fs::read_to_string(root.path().join("retained-guest-input")).expect("retained input"),
            "retain source and target"
        );
    }

    #[test]
    fn test_oversized_prepare_is_not_sent_and_leaves_no_guest_pending() {
        let root = tempfile::tempdir().expect("local frame rejection fixture");
        let (mut backend, context) = fixture(
            root.path(),
            "import pathlib;pathlib.Path('ssh-invoked').write_text('unexpected transport')",
        );
        backend.persist_session(&context).expect("initial session");
        let entries: Vec<_> = (0..20_000)
            .map(|index| crate::transfer::Entry {
                path: format!("{}-{index}", "a".repeat(200)),
                directory: true,
                size: 0,
                digest: String::new(),
            })
            .collect();
        crate::transfer::validate_manifest(&entries).expect("portable valid manifest");
        let error = backend
            .begin_upload(&entries, &context)
            .expect_err("request must exceed the 4 MiB frame limit");
        assert!(format!("{error:#}").contains("frame limit"));
        assert!(
            !root.path().join("ssh-invoked").exists(),
            "local validation must happen before SSH starts"
        );
        assert!(
            !backend.prepared,
            "a definitely unsent request must not mark guest artifacts pending"
        );
        let session: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("windows-session.json")).expect("durable session"))
                .expect("session JSON");
        assert_eq!(session["guest_pending"], false);
    }

    #[test]
    fn test_prepare_spawn_failure_revokes_durable_guest_pending() {
        let root = tempfile::tempdir().expect("unsent spawn failure fixture");
        let (mut backend, context) = fixture(root.path(), "");
        backend.settings.as_mut().expect("settings").ssh = root.path().join("missing-ssh");
        backend.persist_session(&context).expect("initial session");
        let error = backend
            .begin_upload(&[], &context)
            .expect_err("missing SSH must fail before executing transport");
        assert!(crate::process::process_not_started(&error));
        assert!(!backend.prepared);
        let session: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("windows-session.json")).expect("durable unsent state"))
                .expect("session JSON");
        assert_eq!(session["guest_pending"], false);
    }

    #[test]
    fn test_host_preserves_partial_guest_results_and_archives_logs_before_infra_error() {
        let root = tempfile::tempdir().expect("partial guest response fixture");
        let script = r#"import sys,json
r=json.load(sys.stdin)
p={'results':[{'exit_code':0,'timed_out':False,'cancelled':False,'tests_passed':1,'tests_failed':0,'tests_ignored':0,'stdout':'C:\\reports\\first.stdout','stderr':'C:\\reports\\first.stderr'}],'infrastructure_error':'spawn failed: missing guest program'} if r['op']=='execute' else {'data':list(b'first command log'),'eof':True}
print(json.dumps({'protocol_version':1,'request_id':r['request_id'],'run_id':r['run_id'],'ok':True,'payload':p,'error':None}))
"#;
        let (mut backend, context) = fixture(root.path(), script);
        let suite = SuiteSpec {
            id: "partial".into(),
            platforms: vec![],
            target: None,
            commands: vec![
                CommandSpec {
                    kind: CommandKind::Test,
                    argv: vec!["test-program".into()],
                    cwd: ".".into(),
                },
                CommandSpec {
                    kind: CommandKind::Test,
                    argv: vec!["missing-program".into()],
                    cwd: ".".into(),
                },
            ],
            timeout_seconds: 10,
            minimum_tests: 1,
            requirements: vec![],
            data: vec![],
        };
        let error = backend
            .execute(&suite, &context)
            .expect_err("partial protocol payload must remain an infrastructure failure");
        let completed = crate::report::completed_results(&error).expect("host error must retain completed results");
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].tests_passed, 1);
        assert_eq!(
            fs::read_to_string(&completed[0].stdout).expect("archived stdout"),
            "first command log"
        );
        assert!(format!("{error:#}").contains("missing guest program"));
    }
}
