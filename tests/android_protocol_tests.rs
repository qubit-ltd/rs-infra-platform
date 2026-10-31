// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Android runner contracts exercised through the installed command boundary.

use std::fs;
use std::process::Command;

use tempfile::tempdir;

#[test]
fn test_runner_requires_private_context() {
    let output = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"))
        .args(["android-runner", "/missing/test"])
        .env_remove("RS_INFRA_ANDROID_CONTEXT")
        .output()
        .expect("run Android runner");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("RS_INFRA_ANDROID_CONTEXT"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn test_runner_rejects_public_context_before_using_adb() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempdir().expect("create context directory");
    let path = root.path().join("context.json");
    fs::write(&path, b"{}").expect("write context");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("set public permissions");
    let output = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"))
        .args(["android-runner", "/missing/test"])
        .env("RS_INFRA_ANDROID_CONTEXT", &path)
        .output()
        .expect("run Android runner");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("private"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
mod protocol {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::path::PathBuf;
    use std::process::Command;
    use std::process::Output;
    use std::sync::OnceLock;

    use serde_json::json;
    use tempfile::TempDir;
    use tempfile::tempdir;

    static HELPER: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    fn helper_binary() -> &'static Path {
        &HELPER
            .get_or_init(|| {
                let root = tempdir().expect("supervisor build root");
                let source = root.path().join("supervisor.rs");
                fs::write(&source, include_str!("../src/android_device.rs")).expect("write embedded supervisor");
                let binary = root.path().join("supervisor");
                let status = Command::new("rustc")
                    .args(["--edition=2024", "--cfg", "supervisor_host_test", "-O"])
                    .arg(&source)
                    .arg("-o")
                    .arg(&binary)
                    .status()
                    .expect("compile actual supervisor");
                assert!(
                    status.success(),
                    "embedded supervisor must compile without crate dependencies"
                );
                (root, binary)
            })
            .1
    }
    fn executable(path: &Path, source: &str) {
        fs::write(path, source).expect("write controlled executable");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("set executable mode");
    }

    struct Fixture {
        root: TempDir,
        context: PathBuf,
        adb_log: PathBuf,
    }
    impl Fixture {
        fn new(mode: &str) -> Self {
            let root = tempdir().expect("fixture root");
            let device = root.path().join("device");
            fs::create_dir(&device).expect("device base");
            fs::create_dir(device.join("test-run")).expect("device run");
            let token = "a".repeat(64);
            let status = Command::new(helper_binary())
                .args(["init", "test-run", &token])
                .env("RS_INFRA_ANDROID_DEVICE_BASE", &device)
                .status()
                .expect("initialize real supervisor");
            assert!(status.success());
            for name in ["local", "reports", "project", "ndk"] {
                fs::create_dir(root.path().join(name)).expect("fixture directories");
            }
            fs::create_dir_all(root.path().join("project/assets/sub")).expect("data parent");
            fs::write(root.path().join("project/assets/sub/value.txt"), "layout-preserved").expect("data file");
            let adb = root.path().join("fake adb");
            let adb_log = root.path().join("adb.jsonl");
            executable(
                &adb,
                r#"#!/usr/bin/env python3
import os,sys,json,pathlib,shlex,shutil,subprocess
args=sys.argv[1:]
with open(os.environ['FIXTURE_LOG'],'a') as log: log.write(json.dumps(args)+'\n')
if args==['version']: print('Android Debug Bridge version fake'); sys.exit(0)
assert args[:2] == ['-s','emulator-5580'], repr(args)
args=args[2:]
base=pathlib.Path(os.environ['FIXTURE_DEVICE'])
def local(path):
    prefix='/data/local/tmp/rs-infra-platform/'
    assert path.startswith(prefix),repr(path)
    return base/path[len(prefix):]
mode=os.environ.get('FIXTURE_MODE','ok')
if mode=='disconnected': sys.exit(1)
if args[0]=='features': print('shell_v2'); sys.exit(0)
if args[0]=='get-state':
    if mode.startswith('owned-'):
        try:
            pid=int((base.parent/'emulator.pid').read_text())
            stat=pathlib.Path('/proc/'+str(pid)+'/stat').read_text()
            assert stat.rsplit(')',1)[1].split()[0] not in ['Z','X','x']
        except Exception: sys.exit(1)
    print('device'); sys.exit(0)
if args[0]=='emu':
    assert mode.startswith('owned-') and args[1]=='kill'
    import signal
    os.kill(int((base.parent/'emulator.pid').read_text()),signal.SIGTERM);sys.exit(0)
if args[0]=='shell':
    assert args[1]=='-T'
    command=args[2]
    if command.startswith("trap '' HUP;"):
        words=shlex.split(command); pos=words.index('>'); marker=local(words[pos+1].rstrip(')')); root=marker.parent
        root.mkdir(); marker.write_text(words[pos-1]); sys.exit(0)
    if command=='sync': sys.exit(0)
    if command.startswith('test ! -L /data/local/tmp/rs-infra-platform/'):
        words=shlex.split(command); root=local(words[3]); assert (root/'.owner').read_text()==words[-1]; sys.exit(0)
    if command.startswith('if test ! -e '):
        words=shlex.split(command); root=local(words[4].rstrip(';'))
        print('owned' if root.exists() else 'absent',end='');sys.exit(0)
    if command=='exit 9': sys.exit(9)
    if command=='getprop sys.boot_completed': print('0' if mode in ['boot-timeout','owned-startup-block'] else '1'); sys.exit(0)
    if command=='printf rs-infra-shell-ready':
        if mode=='shell-not-ready': sys.exit(1)
        print('rs-infra-shell-ready',end=''); sys.exit(0)
    if command=='getprop ro.product.cpu.abi': print('arm64-v8a' if mode=='abi-mismatch' else 'x86_64'); sys.exit(0)
    if command=='getprop ro.build.version.sdk': print('21' if mode=='api-too-low' else '35'); sys.exit(0)
    if command.startswith('test ! -L '): local(command.split()[-1]).mkdir(); sys.exit(0)
    if command.startswith('chmod 700 '): sys.exit(1 if mode=='bootstrap-chmod-fail' else 0)
    words=shlex.split(args[2]); op=words[1]
    assert words[0].startswith('/data/local/tmp/rs-infra-platform/') and words[0].endswith('/supervisor')
    if mode=='bootstrap-init-fail' and op=='init': sys.exit(1)
    if mode=='transport' and op=='run': sys.exit(1)
    if mode=='clean-fail' and op=='clean': sys.exit(1)
    env=dict(os.environ,RS_INFRA_ANDROID_DEVICE_BASE=str(base))
    status=subprocess.call([os.environ['FIXTURE_HELPER']]+words[1:],env=env)
    if mode=='completed-transport' and op=='run': sys.exit(1)
    sys.exit(status)
if args[0]=='push':
    if mode=='upload-fail': sys.exit(1)
    if mode=='bootstrap-push-fail' and pathlib.Path(args[1]).name=='supervisor': sys.exit(1)
    source=pathlib.Path(args[1]); dest=local(args[2])
    if args[2].endswith('/'): dest=dest/source.name
elif args[0]=='pull':
    source=local(args[1]); dest=pathlib.Path(args[2])
else: raise AssertionError(args)
if not source.exists(): sys.exit(1)
if source.is_dir(): shutil.copytree(source,dest,dirs_exist_ok=True)
else: shutil.copyfile(source,dest)
"#,
            );
            let readelf = root.path().join("readelf");
            executable(
                &readelf,
                if mode == "missing-library" {
                    "#!/usr/bin/env python3\nprint(' 0x01 (NEEDED) Shared library: [libmissing.so]')\n"
                } else {
                    "#!/usr/bin/env python3\n"
                },
            );
            let context = root.path().join("context.json");
            fs::write(&context, serde_json::to_vec(&json!({
                "protocol_version":1,"run_id":"test-run","token":token,"serial":"emulator-5580",
                "adb":adb,"readelf":readelf,"ndk_libraries":root.path().join("ndk"),"system_libraries":[],
                "target":"x86_64-linux-android","local_root":root.path().join("local"),"report_dir":root.path().join("reports"),
                "project_root":root.path().join("project"),"data":["assets"],"timeout_seconds":1
            })).expect("encode context")).expect("write context");
            fs::set_permissions(&context, fs::Permissions::from_mode(0o600)).expect("private context");
            fs::write(root.path().join("mode"), mode).expect("fixture mode");
            Self { root, context, adb_log }
        }
        fn command(&self, script: &str, args: &[&str]) -> Command {
            let mut command = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"));
            command
                .args(["android-runner", "/bin/sh", "-c", script, "fixture"])
                .args(args)
                .env("RS_INFRA_ANDROID_CONTEXT", &self.context)
                .env("FIXTURE_LOG", &self.adb_log)
                .env("FIXTURE_DEVICE", self.root.path().join("device"))
                .env("FIXTURE_HELPER", helper_binary())
                .env(
                    "FIXTURE_MODE",
                    fs::read_to_string(self.root.path().join("mode")).expect("mode"),
                );
            command
        }
        fn run(&self, script: &str, args: &[&str]) -> Output {
            self.command(script, args)
                .output()
                .expect("invoke runner through actual CLI")
        }
    }

    #[test]
    fn test_fixed_serial_and_malicious_argv_preserve_boundaries_and_data_layout() {
        let fixture = Fixture::new("ok");
        let args = [
            "space value",
            "'single'",
            "\"double\"",
            "$(touch escaped)",
            "`touch escaped2`",
            ";touch escaped3",
            "line\nbreak",
            "",
            "中文",
            "-leading",
        ];
        let output = fixture.run("cat assets/sub/value.txt; printf '\\n'; printf '<%s>\\n' \"$@\"", &args);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let expected = format!(
            "layout-preserved\n{}",
            args.iter().map(|arg| format!("<{arg}>\n")).collect::<String>()
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
        let log = fs::read_to_string(&fixture.adb_log).expect("ADB log");
        for line in log.lines() {
            let argv: Vec<String> = serde_json::from_str(line).expect("ADB argv JSON");
            assert_eq!(&argv[..2], &["-s", "emulator-5580"]);
            if argv[2] == "shell" {
                assert!(
                    !argv[4].contains("touch"),
                    "test arguments must only travel in request bytes"
                );
            }
            assert_ne!(argv[2], "emu", "borrowed device must remain running");
        }
    }

    #[test]
    fn test_exit_one_and_nine_are_forwarded_without_transport_confusion() {
        for code in [1, 9] {
            let fixture = Fixture::new("ok");
            let output = fixture.run(&format!("exit {code}"), &[]);
            assert_eq!(
                output.status.code(),
                Some(code),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let records: Vec<_> = fs::read_dir(fixture.root.path().join("reports"))
                .expect("reports")
                .map(|entry| entry.expect("record").path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .collect();
            assert_eq!(records.len(), 1);
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(&records[0]).expect("record bytes")).expect("record JSON");
            assert_eq!(value["exit_code"], code);
            assert_eq!(value["signal"], serde_json::Value::Null);
        }
    }

    #[test]
    fn test_signal_remains_a_separate_persistent_field() {
        let fixture = Fixture::new("ok");
        let output = fixture.run("kill -TERM $$", &[]);
        assert_eq!(output.status.code(), Some(143));
        let record = fs::read_dir(fixture.root.path().join("reports"))
            .expect("reports")
            .map(|entry| entry.expect("record").path())
            .find(|path| path.extension().is_some_and(|ext| ext == "json"))
            .expect("signal record");
        let value: serde_json::Value = serde_json::from_slice(&fs::read(record).expect("bytes")).expect("JSON");
        assert_eq!(value["signal"], 15);
        assert_eq!(value["exit_code"], serde_json::Value::Null);
    }

    #[test]
    fn test_transport_upload_and_missing_library_have_explicit_failures() {
        for (mode, expected) in [
            ("transport", "transport/supervisor"),
            ("upload-fail", "transport/control"),
            ("missing-library", "dynamic library missing"),
        ] {
            let fixture = Fixture::new(mode);
            let output = fixture.run("exit 0", &[]);
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(expected),
                "{mode}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn test_device_timeout_stops_ordinary_descendant_before_return() {
        let fixture = Fixture::new("ok");
        let output = fixture.run(
            "(while :; do printf alive >> ../tmp/heartbeat; sleep 0.02; done) & wait",
            &[],
        );
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
        let directory = fs::read_dir(fixture.root.path().join("device/test-run"))
            .expect("device tree")
            .map(|entry| entry.expect("entry").path())
            .find(|path| path.is_dir())
            .expect("invocation");
        let heartbeat = directory.join("tmp/heartbeat");
        let first = fs::metadata(&heartbeat).expect("heartbeat").len();
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert_eq!(
            fs::metadata(heartbeat).expect("retained heartbeat").len(),
            first,
            "device process group must stop before timeout returns"
        );
    }

    #[test]
    fn test_supervisor_rejects_wrong_owner_and_active_cleanup() {
        let fixture = Fixture::new("ok");
        let device = fixture.root.path().join("device");
        let output = Command::new(helper_binary())
            .args(["clean", "test-run", &"b".repeat(64)])
            .env("RS_INFRA_ANDROID_DEVICE_BASE", &device)
            .output()
            .expect("wrong owner cleanup");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("owner token mismatch"));
        assert!(device.join("test-run/.owner").exists());
    }

    #[test]
    fn test_active_cleanup_is_rejected_then_cancel_stops_and_unlocks_device() {
        let fixture = Fixture::new("ok");
        let mut context: serde_json::Value =
            serde_json::from_slice(&fs::read(&fixture.context).expect("context")).expect("JSON");
        context["timeout_seconds"] = json!(30);
        fs::write(&fixture.context, serde_json::to_vec(&context).expect("encode")).expect("long timeout");
        let mut runner = fixture
            .command(
                "(while :; do printf alive >> ../tmp/heartbeat; sleep 0.02; done) & wait",
                &[],
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start live runner");
        let device = fixture.root.path().join("device");
        let root = device.join("test-run");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let invocation = loop {
            let found = fs::read_dir(&root)
                .expect("root")
                .map(|entry| entry.expect("entry").path())
                .find(|path| path.join("tmp/heartbeat").exists());
            if let Some(path) = found {
                break path;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "runner should start child before fixture deadline"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let token = "a".repeat(64);
        let clean = Command::new(helper_binary())
            .args(["clean", "test-run", &token])
            .env("RS_INFRA_ANDROID_DEVICE_BASE", &device)
            .output()
            .expect("active cleanup");
        assert!(!clean.status.success(), "cleanup must hold exclusive root lock");
        let id = invocation.file_name().expect("id").to_str().expect("UTF8");
        let cancel = Command::new(helper_binary())
            .args(["cancel", "test-run", &token, id])
            .env("RS_INFRA_ANDROID_DEVICE_BASE", &device)
            .output()
            .expect("cancel helper");
        assert!(cancel.status.success(), "{}", String::from_utf8_lossy(&cancel.stderr));
        assert!(!runner.wait().expect("runner terminated").success());
        let heartbeat = invocation.join("tmp/heartbeat");
        let first = fs::metadata(&heartbeat).expect("heartbeat").len();
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert_eq!(fs::metadata(&heartbeat).expect("heartbeat retained").len(), first);
        let clean = Command::new(helper_binary())
            .args(["clean", "test-run", &token])
            .env("RS_INFRA_ANDROID_DEVICE_BASE", &device)
            .output()
            .expect("stopped cleanup");
        assert!(clean.status.success(), "{}", String::from_utf8_lossy(&clean.stderr));
        assert!(
            !root.exists(),
            "registered device directory should be removed only after stop"
        );
    }
    fn backend_fixture(mode: &str) -> Fixture {
        let fixture = Fixture::new(mode);
        let root = fixture.root.path();
        let sdk = root.join("sdk");
        let llvm = sdk.join("ndk/27.3.13750724/toolchains/llvm/prebuilt/linux-x86_64");
        fs::create_dir_all(sdk.join("platform-tools")).expect("fake platform tools");
        fs::create_dir_all(llvm.join("bin")).expect("fake NDK bin");
        fs::create_dir_all(llvm.join("sysroot/usr/lib/x86_64-linux-android/35")).expect("fake API libraries");
        fs::write(
            sdk.join("ndk/27.3.13750724/source.properties"),
            "Pkg.Revision = 27.3.13750724\n",
        )
        .expect("NDK version");
        fs::copy(root.join("fake adb"), sdk.join("platform-tools/adb")).expect("ADB executable");
        for name in [
            "x86_64-linux-android35-clang",
            "x86_64-linux-android35-clang++",
            "llvm-ar",
            "llvm-readelf",
        ] {
            fs::copy(root.join("readelf"), llvm.join("bin").join(name)).expect("NDK tool");
        }
        let bin = root.join("bin");
        fs::create_dir(&bin).expect("host fixture tools");
        executable(
            &bin.join("rustc"),
            r#"#!/usr/bin/env python3
import sys,os,shutil
shutil.copyfile(os.environ['FIXTURE_HELPER'],sys.argv[sys.argv.index('-o')+1])
"#,
        );
        executable(
            &bin.join("cargo"),
            r#"#!/usr/bin/env python3
import sys,os,tomllib,subprocess
args=sys.argv[1:]
assert args[args.index('--target')+1]=='x86_64-linux-android'
assert os.environ['CC_x86_64_linux_android'].endswith('x86_64-linux-android35-clang')
config=tomllib.loads(args[args.index('--config')+1])
runner=config['target']['x86_64-linux-android']['runner']
script="printf 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\\n'"
sys.exit(subprocess.call(runner+['/bin/sh','-c',script]))
"#,
        );
        let project = root.join("project");
        for name in ["src", ".infra/tools", ".infra/platform"] {
            fs::create_dir_all(project.join(name)).expect("project directories");
        }
        fs::write(
            project.join("Cargo.toml"),
            "[package]\nname=\"android-fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
        )
        .expect("Cargo manifest");
        fs::write(project.join("src/lib.rs"), "").expect("source");
        fs::write(
            project.join(".infra/tools/defaults.toml"),
            "build_toolchain=\"1.94.0\"\n",
        )
        .expect("toolchain");
        fs::write(project.join(".infra/platform/platform.toml"), "version=1\n[[suites]]\nid=\"android\"\nplatforms=[\"android\"]\nminimum_tests=1\ntimeout_seconds=30\n[[suites.commands]]\nkind=\"test\"\nargv=[\"cargo\",\"test\",\"--lib\"]\ncwd=\".\"\n").expect("suite");
        fs::write(root.join("machine.toml"), format!("[storage]\nroot={:?}\n[android]\nsdk_root={sdk:?}\nndk_version=\"27.3.13750724\"\nserial=\"emulator-5580\"\nboot_timeout_seconds=1\n", root.join("store"))).expect("machine settings");
        fixture
    }
    fn backend_command(fixture: &Fixture, args: &[&str]) -> Output {
        let mut command = fixture.command("", &[]);
        // The base fixture's environment is shared, but its runner argv is replaced.
        command = {
            let mut c = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"));
            c.envs(
                command
                    .get_envs()
                    .filter_map(|(key, value)| value.map(|value| (key, value))),
            );
            c
        };
        let path = std::env::join_paths(
            std::iter::once(fixture.root.path().join("bin"))
                .chain(std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))),
        )
        .expect("fixture PATH");
        command
            .arg("--project")
            .arg(fixture.root.path().join("project"))
            .args(args)
            .env("PATH", path)
            .env("RS_INFRA_PLATFORM_CONFIG", fixture.root.path().join("machine.toml"))
            .env_remove("RS_INFRA_PLATFORM_CLEANUP")
            .output()
            .expect("run backend through CLI")
    }
    fn backend_report(fixture: &Fixture) -> serde_json::Value {
        let entry = fs::read_dir(fixture.root.path().join("store/reports"))
            .expect("reports")
            .next()
            .expect("run")
            .expect("entry");
        serde_json::from_slice(&fs::read(entry.path().join("summary.json")).expect("summary")).expect("summary JSON")
    }

    #[test]
    fn test_backend_boot_shell_abi_api_failures_are_infrastructure_errors() {
        for (mode, expected) in [
            ("boot-timeout", "readiness timeout"),
            ("shell-not-ready", "readiness timeout"),
            ("abi-mismatch", "device ABI"),
            ("api-too-low", "device API"),
        ] {
            let fixture = backend_fixture(mode);
            let output = backend_command(&fixture, &["check", "--platform", "android", "--require", "android"]);
            assert!(!output.status.success(), "{mode}");
            let summary = backend_report(&fixture);
            assert_eq!(
                summary["suites"][0]["status"], "infrastructure_failed",
                "{mode}: {summary}"
            );
            assert!(
                summary["suites"][0]["errors"].to_string().contains(expected),
                "{mode}: {summary}"
            );
        }
    }

    #[test]
    fn test_backend_cleanup_failure_is_not_success_and_borrowed_device_never_stops() {
        let fixture = backend_fixture("clean-fail");
        let output = backend_command(&fixture, &["check", "--platform", "android", "--require", "android"]);
        assert!(!output.status.success());
        let summary = backend_report(&fixture);
        assert_eq!(summary["suites"][0]["status"], "passed", "{summary}");
        assert!(
            !summary["cleanup"]["errors"]
                .as_array()
                .expect("cleanup errors")
                .is_empty(),
            "{summary}"
        );
        assert!(
            !summary["cleanup"]["remaining"]
                .as_array()
                .expect("pending device")
                .is_empty()
        );
        let log = fs::read_to_string(&fixture.adb_log).expect("ADB log");
        for line in log.lines() {
            let args: Vec<String> = serde_json::from_str(line).expect("ADB argv");
            assert!(
                args.get(2).is_none_or(|arg| arg != "emu"),
                "borrowed device cannot be killed"
            );
        }
    }
    #[test]
    fn test_private_context_rejects_unknown_protocol_and_shell_significant_serial() {
        for (key, value, expected) in [
            ("protocol_version", json!(2), "protocol version"),
            ("serial", json!("emulator-5580;touch escaped"), "serial invalid"),
        ] {
            let fixture = Fixture::new("ok");
            let mut context: serde_json::Value =
                serde_json::from_slice(&fs::read(&fixture.context).expect("context")).expect("JSON");
            context[key] = value;
            fs::write(&fixture.context, serde_json::to_vec(&context).expect("encode")).expect("write invalid context");
            let output = fixture.run("exit 0", &[]);
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(expected),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !fixture.adb_log.exists(),
                "invalid context must fail before any transport call"
            );
        }
    }

    #[test]
    fn test_backend_no_cleanup_and_pending_cleanup_can_be_reclaimed_later() {
        for mode in ["ok", "clean-fail"] {
            let fixture = backend_fixture(mode);
            let args = if mode == "ok" {
                vec!["check", "--platform", "android", "--no-cleanup"]
            } else {
                vec!["check", "--platform", "android"]
            };
            let output = backend_command(&fixture, &args);
            assert_eq!(
                output.status.success(),
                mode == "ok",
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let summary = backend_report(&fixture);
            let id = summary["run_id"].as_str().expect("run id");
            let device = fixture.root.path().join("device");
            assert_eq!(
                fs::read_dir(&device).expect("device roots").count(),
                2,
                "registered run must be retained or pending"
            );
            fs::write(fixture.root.path().join("mode"), "ok").expect("transport recovered");
            let output = backend_command(&fixture, &["clean", "--run", id]);
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(
                fs::read_dir(device).expect("remaining roots").count(),
                1,
                "only unrelated fixture run must remain"
            );
        }
    }

    fn interrupted_borrowed_recovery(disconnect_first: bool, historical: bool) {
        let fixture = backend_fixture("ok");
        let initial = backend_command(&fixture, &["check", "--platform", "android", "--no-cleanup"]);
        assert!(initial.status.success(), "{}", String::from_utf8_lossy(&initial.stderr));
        let summary = backend_report(&fixture);
        let id = summary["run_id"].as_str().expect("run id");
        let retained = Path::new(summary["retained_path"].as_str().expect("retained run"));
        let reports = fixture.root.path().join("store/reports").join(id);
        let mut registration: serde_json::Value =
            serde_json::from_slice(&fs::read(reports.join("entry-0/android-environment.json")).expect("registration"))
                .expect("registration JSON");
        if historical {
            let entry = reports.join("attempt-1/entry-0");
            let local = retained.join("historical-android");
            fs::create_dir_all(&entry).expect("historical report");
            fs::create_dir(&local).expect("historical invocation root");
            registration["runner"]["local_root"] = json!(local);
            registration["runner"]["report_dir"] = json!(entry);
            fs::write(
                entry.join("android-environment.json"),
                serde_json::to_vec(&registration).expect("encode registration"),
            )
            .expect("historical registration");
            fs::set_permissions(
                entry.join("android-environment.json"),
                fs::Permissions::from_mode(0o600),
            )
            .expect("private historical registration");
        }
        let mut context = registration["runner"].clone();
        context["timeout_seconds"] = json!(60);
        fs::write(&fixture.context, serde_json::to_vec(&context).expect("encode context")).expect("active context");
        let mut runner = fixture
            .command(
                "(while :; do printf alive >> ../tmp/heartbeat; sleep 0.02; done) & wait",
                &[],
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("active runner");
        let device = fixture.root.path().join("device");
        let remote = device.join(context["run_id"].as_str().expect("device run"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let invocation = loop {
            if let Some(path) = fs::read_dir(&remote)
                .expect("device root")
                .map(|entry| entry.expect("entry").path())
                .find(|path| path.join("tmp/heartbeat").exists())
            {
                break path;
            }
            assert!(std::time::Instant::now() < deadline, "device test should start");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        runner.kill().expect("interrupt host runner only");
        runner.wait().expect("reap interrupted runner");
        let heartbeat = invocation.join("tmp/heartbeat");
        let before = fs::metadata(&heartbeat).expect("active heartbeat").len();
        let heartbeat_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let heartbeat_survived = loop {
            if fs::metadata(&heartbeat).is_ok_and(|metadata| metadata.len() > before) {
                break true;
            }
            if std::time::Instant::now() >= heartbeat_deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let record_path = fixture.root.path().join("store/records").join(format!("{id}.json"));
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&record_path).expect("record")).expect("record JSON");
        record["state"] = json!("cleanup-pending");
        record["pending_guest"] = json!([remote]);
        fs::write(&record_path, serde_json::to_vec(&record).expect("encode record"))
            .expect("interrupted durable record");
        let host_sentinel = retained.join("preserved-host-file");
        let guest_sentinel = remote.join("preserved-guest-file");
        fs::write(&host_sentinel, b"host retained").expect("host sentinel");
        fs::write(&guest_sentinel, b"guest retained").expect("guest sentinel");
        let disconnected = if disconnect_first {
            fs::write(fixture.root.path().join("mode"), "disconnected").expect("disconnect transport");
            let output = backend_command(&fixture, &["clean", "--stale"]);
            let pending: serde_json::Value =
                serde_json::from_slice(&fs::read(&record_path).expect("pending record")).expect("pending JSON");
            Some((output.status.success(), pending))
        } else {
            None
        };
        fs::write(fixture.root.path().join("mode"), "ok").expect("restore connection");
        let recovered = backend_command(&fixture, &["clean", "--stale"]);
        let after: serde_json::Value =
            serde_json::from_slice(&fs::read(&record_path).expect("recovered record")).expect("recovered JSON");
        let confirmed = invocation.join("group-stopped").is_file();
        // Always stop this generated device test even when the regression is RED.
        let rescue = Command::new(helper_binary())
            .args([
                "cancel",
                context["run_id"].as_str().expect("run"),
                context["token"].as_str().expect("token"),
                invocation
                    .file_name()
                    .expect("invocation")
                    .to_str()
                    .expect("UTF8 invocation"),
            ])
            .env("RS_INFRA_ANDROID_DEVICE_BASE", &device)
            .output()
            .expect("fixture cancellation");
        assert!(rescue.status.success(), "{}", String::from_utf8_lossy(&rescue.stderr));
        assert!(
            heartbeat_survived,
            "device test should survive host runner interruption"
        );
        if let Some((success, pending)) = disconnected {
            assert!(!success, "disconnected recovery cannot prove test stop");
            assert_eq!(pending["state"], "cleanup-pending");
            assert_eq!(pending["pending_guest"], record["pending_guest"]);
        }
        assert!(
            recovered.status.success(),
            "{}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert!(
            confirmed,
            "recovery must confirm the real supervisor stopped the active process group"
        );
        assert_eq!(after["state"], "retained");
        assert_eq!(after["pending_guest"], json!([]));
        let stopped_size = fs::metadata(&heartbeat).expect("heartbeat after cancel").len();
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert_eq!(fs::metadata(&heartbeat).expect("stopped heartbeat").len(), stopped_size);
        assert_eq!(fs::read(&host_sentinel).expect("host file retained"), b"host retained");
        assert_eq!(
            fs::read(&guest_sentinel).expect("guest file retained"),
            b"guest retained"
        );
        let calls = fs::read_to_string(&fixture.adb_log).expect("ADB log");
        assert!(
            !calls.lines().any(|line| {
                let args: Vec<String> = serde_json::from_str(line).expect("ADB argv");
                args.get(2).is_some_and(|arg| arg == "emu") || args.iter().any(|arg| arg.contains("/supervisor clean "))
            }),
            "borrowed stop-only recovery must preserve the device and files"
        );
    }

    #[test]
    fn test_borrowed_retained_recovery_cancels_interrupted_active_device_test() {
        interrupted_borrowed_recovery(false, false);
    }

    #[test]
    fn test_borrowed_retained_historical_recovery_preserves_pending_until_reconnected() {
        interrupted_borrowed_recovery(true, true);
    }

    #[test]
    fn test_backend_fixed_target_and_doctest_requests_fail_explicitly() {
        for (change, expected) in [("target", "suite target"), ("doctest", "doctests")] {
            let fixture = backend_fixture("ok");
            let path = fixture.root.path().join("project/.infra/platform/platform.toml");
            let text = fs::read_to_string(&path).expect("suite");
            let text = if change == "target" {
                text.replace("minimum_tests=1", "minimum_tests=1\ntarget=\"aarch64-linux-android\"")
            } else {
                text.replace("\"--lib\"", "\"--doc\"")
            };
            fs::write(path, text).expect("changed suite");
            let output = backend_command(&fixture, &["check", "--platform", "android"]);
            assert!(!output.status.success());
            let report = backend_report(&fixture);
            assert!(report["suites"][0]["errors"].to_string().contains(expected), "{report}");
        }
    }
    #[test]
    fn test_standard_avd_names_with_underscores_reach_tool_probe() {
        let fixture = backend_fixture("ok");
        let path = fixture.root.path().join("machine.toml");
        let text = fs::read_to_string(&path)
            .expect("machine")
            .replace("serial=\"emulator-5580\"", "avd=\"rs-infra-api35-x86_64\"");
        fs::write(path, text).expect("owned AVD settings");
        let output = backend_command(&fixture, &["doctor"]);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains("emulator missing"),
            "legal AVD name must reach configured tool probe: {text}"
        );
    }
    #[test]
    fn test_available_device_logs_survive_transport_failure_after_test_completion() {
        let fixture = Fixture::new("completed-transport");
        let output = fixture.run("printf archived-device-log; exit 9", &[]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("transport/supervisor"));
        let files: Vec<_> = fs::read_dir(fixture.root.path().join("reports"))
            .expect("reports")
            .map(|entry| entry.expect("entry").path())
            .collect();
        let stdout = files
            .iter()
            .find(|path| path.to_string_lossy().ends_with(".stdout.log"))
            .expect("device logs must be archived even if adb status fails");
        assert_eq!(fs::read_to_string(stdout).expect("log"), "archived-device-log");
    }
    #[test]
    fn test_dynamic_library_alias_is_deployed_under_dt_needed_name() {
        let fixture = Fixture::new("ok");
        let root = fixture.root.path();
        let binary = root.join("aliased-test");
        fs::copy("/bin/sh", &binary).expect("test binary");
        let actual = root.join("libfixture.so.1");
        fs::copy("/bin/sh", &actual).expect("fixture ELF");
        std::os::unix::fs::symlink("libfixture.so.1", root.join("libfixture.so")).expect("needed alias");
        executable(
            &root.join("readelf"),
            "#!/usr/bin/env python3\nimport sys\nif sys.argv[-1].endswith('aliased-test'): print('0x01 (NEEDED) Shared library: [libfixture.so]')\n",
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"));
        let base = fixture.command("", &[]);
        command.envs(
            base.get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        );
        let output = command
            .arg("android-runner")
            .arg(&binary)
            .args(["-c", "test -f ../lib/libfixture.so"])
            .output()
            .expect("run aliased ELF");
        assert!(
            output.status.success(),
            "DT_NEEDED alias must survive canonicalization: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn test_each_command_records_only_its_own_device_invocations() {
        let fixture = backend_fixture("ok");
        let path = fixture.root.path().join("project/.infra/platform/platform.toml");
        let mut text = fs::read_to_string(&path).expect("suite");
        text.push_str("\n[[suites.commands]]\nkind=\"check\"\nargv=[\"cargo\",\"check\"]\ncwd=\".\"\n");
        fs::write(path, text).expect("second command");
        let cargo = fixture.root.path().join("bin/cargo");
        let text = fs::read_to_string(&cargo).expect("fake cargo").replace(
            "args=sys.argv[1:]",
            "args=sys.argv[1:]\nif args[0]=='check': sys.exit(0)",
        );
        executable(&cargo, &text);
        let output = backend_command(&fixture, &["check", "--platform", "android"]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let run = fs::read_dir(fixture.root.path().join("store/reports"))
            .expect("reports")
            .next()
            .expect("run")
            .expect("entry")
            .path();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(run.join("entry-0/android-command-1.json")).expect("second record"))
                .expect("JSON");
        assert_eq!(value["category"], "compile_only");
        assert_eq!(value["device_result_files"].as_array().expect("results").len(), 0);
    }

    #[test]
    fn test_partial_supervisor_bootstrap_is_recoverable_after_transport_returns() {
        for mode in ["bootstrap-push-fail", "bootstrap-chmod-fail", "bootstrap-init-fail"] {
            let fixture = backend_fixture(mode);
            let output = backend_command(&fixture, &["check", "--platform", "android"]);
            assert!(!output.status.success(), "{mode}");
            let report = backend_report(&fixture);
            fs::write(fixture.root.path().join("mode"), "ok").expect("restore transport");
            let output = backend_command(&fixture, &["clean", "--run", report["run_id"].as_str().expect("id")]);
            assert!(
                output.status.success(),
                "bootstrap stage must recover {mode}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                fs::read_dir(fixture.root.path().join("device"))
                    .expect("device roots")
                    .count(),
                1
            );
        }
    }
    #[test]
    fn test_supervisor_keeps_exited_leader_until_group_stop_is_confirmed() {
        let fixture = Fixture::new("ok");
        let mut child = fixture
            .command("exit 0", &[])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("runner");
        let root = fixture.root.path().join("device/test-run");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let invocation = loop {
            if let Some(path) = fs::read_dir(&root)
                .expect("root")
                .map(|entry| entry.expect("entry").path())
                .find(|path| path.join("group").exists())
            {
                break path;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        let pid = fs::read_to_string(invocation.join("group")).expect("group pid");
        while !invocation.join("result").exists() {
            assert!(
                Path::new(&format!("/proc/{}", pid.trim())).exists() || invocation.join("group-stopped").exists(),
                "leader identity released before process-group stop confirmation"
            );
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(child.wait().expect("runner stopped").success());
        assert!(invocation.join("group-stopped").exists());
    }
    fn owned_fixture(mode: &str) -> Fixture {
        let fixture = backend_fixture(mode);
        let root = fixture.root.path();
        let sdk = root.join("sdk");
        fs::create_dir_all(sdk.join("emulator")).expect("emulator directory");
        executable(
            &sdk.join("emulator/emulator"),
            r#"#!/usr/bin/env python3
import os,sys,time,pathlib,json
if '-version' in sys.argv or '-accel-check' in sys.argv: sys.exit(0)
root=pathlib.Path(os.environ['FIXTURE_DEVICE']).parent
record=max((root/'store/reports').glob('**/android-environment.json'),key=lambda p:p.stat().st_mtime)
data=json.loads(record.read_text())
assert data['owned'] and data['owned_stop_pending']
assert data['guard']['start'] and data['emulator']['pid']==os.getpid() and data['anchor']['start']
assert os.getpgrp()==os.getpid()
(root/'emulator-observed-registration.json').write_text(json.dumps(data))
(root/'emulator.pid').write_text(str(os.getpid()))
while True: time.sleep(.02)
"#,
        );
        fs::create_dir_all(sdk.join("system-images/test")).expect("image");
        fs::write(
            sdk.join("system-images/test/source.properties"),
            "AndroidVersion.ApiLevel=35\n",
        )
        .expect("image API");
        let avd = root.join("avd");
        fs::create_dir_all(avd.join("fixture_x86_64.avd")).expect("AVD");
        fs::write(
            avd.join("fixture_x86_64.avd/config.ini"),
            "abi.type=x86_64\nimage.sysdir.1=system-images/test/\n",
        )
        .expect("AVD config");
        let machine = root.join("machine.toml");
        let text = fs::read_to_string(&machine)
            .expect("machine")
            .replace(
                "serial=\"emulator-5580\"",
                &format!("avd=\"fixture_x86_64\"\navd_home={avd:?}"),
            )
            .replace("boot_timeout_seconds=1", "boot_timeout_seconds=30");
        fs::write(machine, text).expect("owned settings");
        fixture
    }
    fn owned_child(fixture: &Fixture) -> std::process::Child {
        let base = fixture.command("", &[]);
        let mut command = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"));
        command.envs(
            base.get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        );
        let path = std::env::join_paths(
            std::iter::once(fixture.root.path().join("bin"))
                .chain(std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))),
        )
        .expect("PATH");
        command
            .arg("--project")
            .arg(fixture.root.path().join("project"))
            .args(["check", "--platform", "android", "--no-cleanup"])
            .env("PATH", path)
            .env("RS_INFRA_PLATFORM_CONFIG", fixture.root.path().join("machine.toml"))
            .env_remove("RS_INFRA_PLATFORM_CLEANUP")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("owned coordinator")
    }
    fn wait_owned(fixture: &Fixture) -> PathBuf {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !fixture.root.path().join("emulator.pid").exists() {
            assert!(std::time::Instant::now() < deadline, "owned Emulator launch deadline");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let run = fs::read_dir(fixture.root.path().join("store/reports"))
            .expect("reports")
            .next()
            .expect("run")
            .expect("entry")
            .path();
        run.join("entry-0/android-environment.json")
    }
    fn pid_live(pid: u64) -> bool {
        fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .map(|(_, fields)| fields.split_whitespace().next().unwrap_or("").to_owned())
            })
            .is_some_and(|state| !matches!(state.as_str(), "Z" | "X" | "x"))
    }
    #[test]
    fn test_owned_intent_precedes_emulator_exec_and_parent_crash_stops_without_deleting() {
        let fixture = owned_fixture("owned-startup-block");
        let mut parent = owned_child(&fixture);
        let path = wait_owned(&fixture);
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).expect("registration")).expect("JSON");
        assert!(value["owned_stop_pending"].as_bool().expect("pending"));
        assert_eq!(value["bootstrap"], "not_started");
        // A guest-cleaned receipt must never suppress the independent owner-stop state.
        fs::write(
            path.parent().expect("entry").join("android-session.cleaned"),
            "guest already cleaned",
        )
        .expect("receipt");
        parent.kill().expect("kill coordinator only");
        parent.wait().expect("reap coordinator");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).expect("registration")).expect("JSON");
            if value["owned_stop_pending"] == false && !pid_live(value["emulator"]["pid"].as_u64().expect("pid")) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "owner EOF must stop the exact Emulator"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let store = fixture.root.path().join("store");
        assert_eq!(fs::read_dir(store.join("runs")).expect("retained artifacts").count(), 1);
        assert!(fixture.root.path().join("device/test-run/.owner").exists());
    }
    #[test]
    fn test_killed_owned_guard_anchor_stops_only_its_registered_group() {
        let fixture = owned_fixture("owned-startup-block");
        let mut parent = owned_child(&fixture);
        let path = wait_owned(&fixture);
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).expect("registration")).expect("JSON");
        let guard = value["guard"]["pid"].as_u64().expect("guard pid") as i32;
        assert_eq!(unsafe { libc::kill(guard, libc::SIGKILL) }, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while pid_live(value["emulator"]["pid"].as_u64().expect("pid")) {
            assert!(
                std::time::Instant::now() < deadline,
                "anchor must stop group on owner loss"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!parent.wait().expect("coordinator failure").success());
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).expect("registration")).expect("JSON");
        assert_eq!(value["owned_stop_pending"], false);
        assert!(fixture.root.path().join("device/test-run/.owner").exists());
    }
    #[test]
    fn test_owned_cleanup_relaunch_uses_same_gated_process_group_and_stops_again() {
        let fixture = owned_fixture("owned-ok");
        let output = backend_command(&fixture, &["check", "--platform", "android", "--no-cleanup"]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let summary = backend_report(&fixture);
        let id = summary["run_id"].as_str().expect("id");
        let output = backend_command(&fixture, &["clean", "--run", id]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let value: serde_json::Value = serde_json::from_slice(
            &fs::read(fixture.root.path().join("emulator-observed-registration.json")).expect("launch proof"),
        )
        .expect("JSON");
        assert!(!pid_live(value["emulator"]["pid"].as_u64().expect("pid")));
        assert_eq!(
            fs::read_dir(fixture.root.path().join("device"))
                .expect("device roots")
                .count(),
            1
        );
    }
    #[test]
    fn test_conflicting_dt_needed_alias_sources_are_rejected_before_deployment() {
        let fixture = Fixture::new("ok");
        let root = fixture.root.path();
        fs::create_dir_all(root.join("nested/A")).expect("library root");
        for relative in ["aliased-test", "libdep.so", "nested/A/libdep.so", "nested/A/liba.so"] {
            fs::copy("/bin/sh", root.join(relative)).expect("fixture ELF");
        }
        std::os::unix::fs::symlink("nested/A/liba.so", root.join("liba.so")).expect("alias");
        executable(
            &root.join("readelf"),
            "#!/usr/bin/env python3\nimport sys\np=sys.argv[-1]\nif p.endswith('aliased-test'): print('0x01 (NEEDED) Shared library: [liba.so]\\n0x01 (NEEDED) Shared library: [libdep.so]')\nif p.endswith('nested/A/liba.so'): print('0x01 (NEEDED) Shared library: [libdep.so]')\n",
        );
        let base = fixture.command("", &[]);
        let mut command = Command::new(env!("CARGO_BIN_EXE_rs-infra-platform"));
        command.envs(
            base.get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        );
        let output = command
            .arg("android-runner")
            .arg(root.join("aliased-test"))
            .args(["-c", "exit 0"])
            .output()
            .expect("runner");
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("conflicting Android DT_NEEDED"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.adb_log.exists());
    }

    #[test]
    fn test_device_supervisor_kill_anchor_stops_group_and_preserves_unconfirmed_run() {
        let fixture = Fixture::new("ok");
        let mut context: serde_json::Value =
            serde_json::from_slice(&fs::read(&fixture.context).expect("context")).expect("JSON");
        context["timeout_seconds"] = json!(30);
        fs::write(&fixture.context, serde_json::to_vec(&context).expect("encode")).expect("long timeout");
        let mut runner = fixture
            .command(
                "(while :; do printf alive >> ../tmp/heartbeat; sleep 0.02; done) & wait",
                &[],
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("runner");
        let root = fixture.root.path().join("device/test-run");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let invocation = loop {
            if let Some(path) = fs::read_dir(&root)
                .expect("root")
                .map(|entry| entry.expect("entry").path())
                .find(|path| path.join("tmp/heartbeat").exists() && path.join("group").exists())
            {
                break path;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let group: u64 = fs::read_to_string(invocation.join("group"))
            .expect("group")
            .parse()
            .expect("pid");
        let stat = fs::read_to_string(format!("/proc/{group}/stat")).expect("child stat");
        let supervisor: i32 = stat
            .rsplit_once(')')
            .expect("fields")
            .1
            .split_whitespace()
            .nth(1)
            .expect("ppid")
            .parse()
            .expect("supervisor pid");
        assert_eq!(unsafe { libc::kill(supervisor, libc::SIGKILL) }, 0);
        while pid_live(group) {
            assert!(
                std::time::Instant::now() < deadline,
                "anchor should stop child after supervisor SIGKILL"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!runner.wait().expect("runner failed").success());
        let first = fs::metadata(invocation.join("tmp/heartbeat")).expect("heartbeat").len();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(
            fs::metadata(invocation.join("tmp/heartbeat"))
                .expect("retained heartbeat")
                .len(),
            first
        );
        let output = Command::new(helper_binary())
            .args(["clean", "test-run", &"a".repeat(64)])
            .env("RS_INFRA_ANDROID_DEVICE_BASE", fixture.root.path().join("device"))
            .output()
            .expect("conservative cleanup");
        assert!(!output.status.success());
        assert!(root.exists(), "missing completion cannot be declared cleaned");
    }
    #[test]
    fn test_owned_shutdown_flushes_guest_files_before_emulator_exit() {
        let fixture = owned_fixture("owned-ok");
        let output = backend_command(&fixture, &["check", "--platform", "android", "--no-cleanup"]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let calls: Vec<Vec<String>> = fs::read_to_string(&fixture.adb_log)
            .expect("log")
            .lines()
            .map(|line| serde_json::from_str(line).expect("argv"))
            .collect();
        let sync = calls
            .iter()
            .position(|args| {
                args.get(2).is_some_and(|value| value == "shell") && args.get(4).is_some_and(|value| value == "sync")
            })
            .expect("device files must be flushed before stopping VM");
        let stop = calls
            .iter()
            .position(|args| args.get(2).is_some_and(|value| value == "emu"))
            .expect("owned stop");
        assert!(sync < stop);
    }
    #[test]
    fn test_prior_device_results_survive_later_deployment_failure() {
        let fixture = backend_fixture("ok");
        let path = fixture.root.path().join("project/.infra/platform/platform.toml");
        let mut text = fs::read_to_string(&path).expect("suite");
        text.push_str("\n[[suites.commands]]\nkind=\"test\"\nargv=[\"cargo\",\"test\",\"--lib\",\"--test\",\"second\"]\ncwd=\".\"\n");
        fs::write(path, text).expect("second command");
        let cargo = fixture.root.path().join("bin/cargo");
        let text = fs::read_to_string(&cargo).expect("fake Cargo").replace(
            "args=sys.argv[1:]",
            "args=sys.argv[1:]\nif 'second' in args: os.environ['FIXTURE_MODE']='upload-fail'",
        );
        executable(&cargo, &text);
        let output = backend_command(&fixture, &["check", "--platform", "android"]);
        assert!(!output.status.success());
        let report = backend_report(&fixture);
        assert_eq!(report["suites"][0]["status"], "infrastructure_failed");
        assert_eq!(report["suites"][0]["tests_passed"], 1, "{report}");
        assert_eq!(
            report["suites"][0]["commands"]
                .as_array()
                .expect("completed commands")
                .len(),
            1
        );
    }
}
