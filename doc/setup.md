# Setup and Operations

[中文](setup.zh_CN.md) · [README](../README.md)

This guide applies to `qubit-infra-platform` 0.1.0, distributed as source through GitHub and not published on crates.io. It is for developers who need to run a changed Rust project's tests before committing or pushing. Start with the native workflow, then prepare only the guests your project needs. Configuration examples describe interfaces; they are not evidence of completed real-machine acceptance.

## Run the Current Working Files

Build and install this tool from its source root:

```bash
cargo +1.94.0 install --path . --locked
rs-infra-platform --help
```

Use the complete native suite in the [README](../README.md#quick-start), then run these commands from the project being tested:

```bash
rs-infra-platform doctor
rs-infra-platform plan --platform linux --suite native-tests
rs-infra-platform check --platform linux --suite native-tests --require linux
```

Replace `linux` with your native `macos` or `windows` host. The project must contain `.infra/tools/defaults.toml` with its shared `build_toolchain` and `.infra/platform/platform.toml` with suites. Commands use that toolchain via `RUSTUP_TOOLCHAIN`; do not put a separate `cargo +toolchain` in `argv`. Install the shared toolchain yourself before checking.

The tool snapshots tracked files, modifications, and untracked nonignored files. It recursively discovers relative Cargo path dependencies and preserves their layout. `.git` and `target` are excluded. Declare necessary ignored inputs in `suite.data`; paths must remain project-relative and cannot traverse outside the project. A changing source tree can cause snapshot retries and then failure.

Builds and lockfile changes occur inside the snapshot. The small [native fixture](../tests/fixtures/native-smoke/src/lib.rs) is a runnable executor test input; the project's own suites still determine which application tests execute.

## Snapshot Path Boundaries

Use relative Cargo dependency paths such as `../rs-redact`. The snapshot preserves that sibling layout, including recursively discovered dependencies. Absolute `path` values are rejected in normal, development, build, workspace, target-specific, `patch`, and `replace` dependency declarations; workspace members/default-members and `package.workspace` must also be relative. For these Cargo paths, Windows drive-qualified forms (including drive-relative forms), UNC paths, and rooted Windows paths are rejected even on Unix. On Unix, dependency paths and symlink targets containing backslashes are also rejected to avoid different guest interpretations; use `/` for relative paths. Windows hosts can use native relative backslashes. The tool does not rewrite the original manifest or translate absolute Cargo dependency paths into snapshot paths. Existing relative `rs-local-files` dependencies follow the supported route.

Relative internal symlinks can be preserved only when every component of the full link chain resolves within the included snapshot inputs. For a host-local absolute link, the tool first resolves its complete source-side chain. The resolved destination must lie within a captured source root and be a file or implicit directory represented in the snapshot manifest; only then is the snapshot copy rewritten to a relative target. For example, a root-level `rs-json` alias back to its own `rs-json` source root becomes `.` and is recorded once rather than recursively traversed. This permits internal source aliases during native execution and host-side Android compilation without reaching back into the original workspace.

External or uncaptured resolved destinations, foreign absolute-path syntax, escaping traversal, and cyclic link chains remain rejected. Relative links and relocated targets must pass the same complete-chain and snapshot-membership checks, including intermediate targets; relative link-chain expansion is capped at 40 steps. A target's presence in the original workspace alone is insufficient, and the original link is never modified. Include necessary ignored inputs explicitly with `data` when they meet the same path rules; excluded `.git` and `target` trees cannot serve as link targets. This restricted link relocation does not enable absolute Cargo dependency paths.

Guest transport can impose stricter limits than snapshot creation. Windows transfer rejects symbolic links instead of dereferencing or silently changing them. Android `suite.data` upload rejects symbolic links in data paths and directory trees, including safe internal links; use ordinary runtime data files/directories. If snapshot validation rejects a dependency or link, review the declared path and use a supported relative layout rather than relying on access to the original workspace.

## Use the Local CI Orchestrator

The updated local `rs-infra-ci` can call this tool directly from PATH. Publication is unnecessary for this source workflow:

```bash
cargo +1.94.0 install --path /path/to/rs-infra-platform --locked
cargo +1.94.0 run --manifest-path /path/to/rs-infra-ci/Cargo.toml -- --project /path/to/rs-local-files --only platform plan
cargo +1.94.0 run --manifest-path /path/to/rs-infra-ci/Cargo.toml -- --project /path/to/rs-local-files --only platform check
```

Use your updated source checkout paths. The project needs `"platform"` in its `.infra/ci/ci.toml` `tasks` array and its own `.infra/platform/platform.toml`; preserve its other tasks. The local `rs-local-files` integration already supplies these. Both `--project` and `--only` are coordinator-level options placed before `plan` or `check`. Plan shows the selected platform task; check invokes `rs-infra-platform --project PROJECT check`. The platform CLI supplies detailed environment planning, suite selection, and `--require` when needed.

If the coordinator has `RS_INFRA_BIN_DIR` set, it resolves the platform executable from that directory instead of PATH. Point it to the directory containing your locally built or installed executable. The updated `.infra/bin/infra-tool.sh` bootstrap entry also accepts `rs-infra-platform`; its runtime wrapper checks `RS_INFRA_BIN_DIR` for an executable override before requesting a dynamic tool. For platform invocations it skips the older general build-artifact cleanup so that this tool's registered lifecycle and `--no-cleanup` policy remain in control.

For regular project use, update the managed bootstrap and invoke its tool wrapper:

```bash
./update-infra.sh
./.infra/bin/infra-tool.sh rs-infra-platform --project . check
```

The wrapper resolves tools from their GitHub `main` revisions and caches their builds; no crates.io release is required. The project owns `.infra/platform/platform.toml`, and the updater preserves it. Declare each suite's platforms there and keep machine-specific VM/SDK settings in `~/.config/rs-infra/platform.toml`. A normal `check` selects all declared suites; `--platform` and `--suite` narrow that selection. Missing optional environments warn and skip. Append `--require windows` to fail if Windows cannot run rather than accepting a skip.

Both wrapper and direct execution use default cleanup. `--no-cleanup` or `RS_INFRA_PLATFORM_CLEANUP=0` retains disposable run files; owned environments are still stopped.

## Separate Project and Machine Settings

Project `.infra/platform/platform.toml` uses `version = 1` and `[[suites]]`. Each suite has `id`, `platforms`, `timeout_seconds`, `minimum_tests`, and commands. Optional `target` fixes a Rust target; `requirements` and `data` default to empty arrays. Each command has `kind` (`check`, `build`, or `test`), an argument array `argv`, and a relative `cwd`. Compile-only suites require successful command execution and report `checked`; test counts do not apply. For suites containing a `test` command, `--require` raises `minimum_tests` to at least one actual test. Test counts come from test output, so use supported Rust test summaries rather than assuming exit zero proves tests ran.

Requirement names depend on the execution backend:

| Backend | Supported `requirements` |
| --- | --- |
| Native Linux/macOS/Windows | `native-shell`, `writable-tmpdir`, `ntfs`, `msvc`, `symlinks`, `long-paths` |
| Windows VM guest | `ntfs`, `msvc`, `symlinks`, `long-paths` |
| Android | `native-shell`, `writable-tmpdir` |

Native execution checks these conditions before suite commands: a working system shell, actual write/read/rename in the owned temporary directory, an NTFS execution filesystem, successful MSVC Rust compilation/linking on Windows, working file symlinks, or write/read under a path longer than 260 characters, respectively. Native also accepts `symlink` and `long-path` as aliases; prefer the plural spellings above. A supported name can still fail on the current host: for example, `msvc` requires Windows with an MSVC Rust host target, and `ntfs` requires the actual execution volume to be NTFS. Unknown names and unmet conditions fail instead of being ignored. Use only requirements valid for every selected backend, or split platform-specific requirements into separate suites. The Windows and Android sections below use their own backend names; a native Windows runner uses the native row.

The machine file defaults to `~/.config/rs-infra/platform.toml`, also on Windows (under the user profile). Select another file explicitly:

```bash
export RS_INFRA_PLATFORM_CONFIG=/absolute/path/to/machine.toml
```

In Windows PowerShell use `$env:RS_INFRA_PLATFORM_CONFIG = 'C:\path\to\machine.toml'` instead.

An absent default file is allowed; an explicitly selected missing file is an error. Unknown fields are rejected. This native-only machine example is sufficient on Linux; replace the absolute storage path for your account:

```toml
[storage]
root = "/home/dev/.cache/rs-infra/platform"

[native]
cargo = "cargo"
```

`storage.root` must be absolute and dedicated to this tool. Do not point it at a project, SDK, VM image directory, or an existing directory of unrelated data: ownership checks reject unsafe stores. By default, Linux uses `${XDG_CACHE_HOME}/rs-infra/platform` when the variable is absolute, otherwise `~/.cache/rs-infra/platform`; macOS uses `~/Library/Caches/rs-infra/platform`; Windows uses `%LOCALAPPDATA%/rs-infra/platform` (or the profile's `AppData/Local` fallback).

## Prepare a Windows VM on Linux

1. Provide your own legitimately licensed installation media and install Windows manually into a dedicated qcow2 disk. The tool neither downloads installation media nor performs unattended installation. Its runtime uses a q35/KVM machine, an IDE disk, e1000 networking, and a localhost SSH port forward; prepare the disk for that hardware.
2. Install QEMU and provide usable KVM permissions to the account running the tool. Checking for `/dev/kvm` alone is insufficient; the backend actually opens KVM and creates a VM handle during its capability probe. Ensure the host has sufficient memory and disk capacity.
3. In Windows, install an MSVC Rust toolchain matching the project's shared version, Visual Studio C++ build tools, and a Windows SDK. The guest locates the installation through `vswhere`/`VsDevCmd` and checks actual Rust linking. Follow the [Rust MSVC prerequisites](https://rust-lang.github.io/rustup/installation/windows-msvc.html) for the vendor installation choices.
4. Install and configure OpenSSH Server, key authentication, and the guest account according to [Microsoft's OpenSSH instructions](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse). Verify the host key separately and put it in the host's `known_hosts`; the transport requires `StrictHostKeyChecking=yes` and noninteractive authentication.
5. Build this same tool source for Windows and put `rs-infra-platform.exe` in the SSH account's PATH. Fixed `rs-infra-platform guest` invocations exchange versioned JSON on stdin/stdout. Keep shell startup messages off stdout. Host and guest must speak the same protocol.
6. Give the guest account a dedicated local NTFS execution root, not an SMB/host shared folder. Set `RS_INFRA_PLATFORM_GUEST_ROOT` in the environment seen by SSH, or use `%LOCALAPPDATA%/rs-infra-platform/guest`. The host's `guest_root` records that matching location; it does not install or configure the guest environment for you.

Example host machine settings (append to the native configuration and replace paths):

```toml
[windows]
qemu = "/usr/bin/qemu-system-x86_64"
image = "/home/dev/VMs/rs-infra-windows.qcow2"
qmp_socket = "/home/dev/.cache/rs-infra/windows-control/windows.qmp"
ssh_alias = "rs-infra-windows"
guest_root = 'C:\Users\tester\AppData\Local\rs-infra-platform\guest'
ssh = "ssh"
memory_mib = 4096
cpus = 2
ssh_port = 2222
startup_timeout_seconds = 180
shutdown_timeout_seconds = 60
```

All fields through `guest_root` are required. Remaining values show defaults. Use an absolute QEMU path and image path. The QMP filename must be `windows.qmp`, with a path shorter than 100 bytes and no commas. Its parent must be dedicated and initially empty; the tool establishes a `.rs-infra-platform-qmp` marker, sets private `0700` permissions, and locks the selected image. A nonempty unmarked directory is rejected. The SSH alias accepts ASCII letters, digits, `.`, `_`, and `-`, and cannot start with `-`.

The matching `~/.ssh/config` entry can be:

```sshconfig
Host rs-infra-windows
    HostName 127.0.0.1
    Port 2222
    User tester
    IdentityFile ~/.ssh/rs-infra-windows
    IdentitiesOnly yes
```

The alias's port must match `ssh_port`. Register the known host key only after verifying it against the guest. Guest suites can request `ntfs`, `msvc`, `symlinks`, and `long-paths`; enable Windows Developer Mode or the appropriate symbolic-link privilege when needed, and configure long-path support. These requirements are checked rather than silently skipped. NTFS and a working MSVC build environment are baseline guest conditions.

Run `rs-infra-platform doctor`, review `plan --platform windows`, then use `check --platform windows --require windows`. `doctor` checks host prerequisites without booting a guest; guest NTFS/MSVC/space and suite requirements are checked when the guest connects. A configured image that is missing or an unusable KVM setup is an error. The first actual SSH connection and Windows test run remain separate acceptance steps.

An existing responsive configured QMP instance is borrowed; otherwise the tool starts its own QEMU process. It stops only its own process. Do not concurrently mutate the same guest disk outside the tool's lease.

## Prepare Android

The Android workflow cross-compiles on the host, then runs the resulting Rust test executable through ADB in a dedicated `/data/local/tmp` directory. It does not build an APK or perform instrumentation tests.

Prepare a Java runtime, Android command-line tools, SDK platform-tools, Emulator, NDK, and a system image matching the intended ABI. The following pinned Linux example matches the environment selected during development; these are example inputs, not auto-installed tool dependencies:

```bash
export ANDROID_HOME="$HOME/Android/Sdk"
export ANDROID_AVD_HOME="$HOME/Android/avd"
export PATH="$ANDROID_HOME/cmdline-tools/22.0/bin:$ANDROID_HOME/platform-tools:$ANDROID_HOME/emulator:$PATH"
sdkmanager --sdk_root="$ANDROID_HOME" 'platform-tools' 'emulator' 'ndk;27.3.13750724' 'system-images;android-35;default;x86_64'
```

Review and accept the requested vendor license interactively only when authorized. Avoid piping blanket acceptance into all license prompts. Development installation of these four selected components was authorized; that does not accept licenses on another user's machine. See the [vendor package manager documentation](https://developer.android.com/tools/sdkmanager) for package installation and license handling. The example uses the selected command-line tools' `sdkmanager`, not an assumption about the latest vendor CLI.

Create a dedicated AVD and install the matching Rust target:

```bash
avdmanager create avd --name rs-infra-api35-x86_64 --package 'system-images;android-35;default;x86_64'
rustup target add --toolchain 1.94.0 x86_64-linux-android
emulator -accel-check
```

Use the actual shared toolchain when it differs. If `avdmanager` asks about a custom hardware profile, select according to your intended test environment. The development host selected API 35, AOSP x86_64, 2 cores, 2048 MiB RAM, serial `emulator-5580`, and an isolated persistent AVD directory. This is environment preparation, not proof that suites have passed.

Linux hosts need working KVM access. macOS uses its native hypervisor and host-compatible images; Apple Silicon needs an arm64 image and `aarch64-linux-android`, while an x86_64 Linux example cannot be copied unchanged. See [Android's acceleration guidance](https://developer.android.com/studio/run/emulator-acceleration). Linux-native and macOS-native suites each execute only on their actual host.

Append this machine configuration for a running, borrowed emulator (replace paths):

```toml
[android]
sdk_root = "/home/dev/Android/Sdk"
ndk_version = "27.3.13750724"
api = 35
abi = "x86_64"
serial = "emulator-5580"
avd_home = "/home/dev/Android/avd"
emulator_port = 5580
boot_timeout_seconds = 180
cargo = "cargo"
```

`sdk_root` and `ndk_version` are required. Select exactly one of `serial` (borrow a device) and `avd` (start a dedicated AVD). To let the tool start the example AVD, replace `serial` with `avd = "rs-infra-api35-x86_64"`; do not keep both. API defaults to 35 and currently accepts 21 through 35. ABI defaults to `x86_64` on Linux and `arm64-v8a` on Apple Silicon. Other displayed values are defaults, except `avd_home`, whose default comes from `ANDROID_AVD_HOME` or `~/.android/avd`. `sdk_root` and an explicit `avd_home` must be absolute. The tool starts an owned emulator with acceleration and without snapshot loading/saving; it does not wipe persistent AVD data.

Use `adb -s emulator-5580 shell getprop sys.boot_completed` and `adb -s emulator-5580 shell getprop ro.product.cpu.abi` to inspect the explicitly selected emulator. A connected device still must satisfy configured API, ABI, shell readiness, and suite requirements. The tool shuts down emulators it starts; it does not shut down a borrowed serial.

The README's desktop suite does not select Android. Append this Android suite to the project's `.infra/platform/platform.toml` before checking it:

```toml
[[suites]]
id = "android-native"
platforms = ["android"]
timeout_seconds = 1200
minimum_tests = 1
requirements = ["native-shell", "writable-tmpdir"]
data = []

[[suites.commands]]
kind = "test"
argv = ["cargo", "test", "--locked", "--lib"]
cwd = "."
```

Then run `rs-infra-platform doctor`, `rs-infra-platform plan --platform android`, and `rs-infra-platform check --platform android --suite android-native --require android`. Inspect the resulting command logs and actual test count before accepting Android coverage.

The runner injects the target runner and NDK linker for the process; it does not write the user's `.cargo/config.toml`. Leave `target` unspecified to use the configured ABI, or set the matching Rust target in the suite. Android Cargo `argv` must not override `--target`, `--target-dir`, `--config`, or `--manifest-path`. The device's API must be at least the configured compilation API and its ABI must match. Cross-target doctests are unsupported, so `cargo test` needs an explicit selector such as `--lib`, `--tests`, or `--test NAME`; `--doc` fails rather than being silently omitted. ARMv7 compile checks remain compile-only; an x86_64 emulator cannot validate ARMv7 runtime behavior.

## Read Outcomes and Reports

`plan` reports selected suites, route, and capability without launching environments or cleaning past runs. `doctor` diagnoses configuration and installed prerequisites; `setup --platform windows,android` prints preparation guidance. None performs an installation.

After `check`, follow the printed report path. The first execution keeps `reports/RUN_ID/summary.json` and per-command stdout/stderr separately from `runs/RUN_ID`. Reusing that retained run preserves the original report and writes the next execution to `reports/RUN_ID/attempt-N/summary.json` with its own command logs. The summary includes the source snapshot digest, environment information, suite test counts, command exit/timeout/cancellation results, retained paths, and cleanup errors. Keep these artifacts with the change being reviewed. Recovery files such as `windows-session.json` contain scoped session tokens; keep the report directory private and review artifacts before publishing them.

| Observation | Meaning and next action |
| --- | --- |
| `WARN+SKIP` | Optional environment is missing or route unsupported. Prepare it or require it explicitly. |
| `infrastructure_failed` | Configured environment, transport, startup, or execution preparation failed. Inspect logs and fix it. |
| `failed` | A command failed, timed out, was cancelled, or did not meet actual test counts. Inspect the command logs. |
| `checked` | Commands succeeded without executed tests, where the suite permits it. This is not runtime test coverage. |
| `passed` | The suite executed counted tests and met its checks. Inspect cleanup separately. |
| `no platform tests executed` | No selected suite executed counted tests. An optional all-skipped run may return zero. |

`--require windows,android` makes missing/skipped selected platforms fail. Their selected compile-only suites must complete successfully; suites containing a `test` command must execute at least one actual test. Unknown suite IDs, unknown platform names, an excluded required platform, invalid machine settings, and unsafe paths fail before useful execution. A test success followed by necessary cleanup failure still produces a nonzero overall result. Test and cleanup failures remain separate in the report.

## Cleanup and Recovery

The policy is resolved before starting execution:

1. Explicit `--cleanup` or `--no-cleanup` (mutually exclusive).
2. `RS_INFRA_PLATFORM_CLEANUP`: `1/true/on` or `0/false/off`, case-insensitive.
3. Cleanup enabled by default.

Empty, whitespace-padded, or other values are invalid unless an explicit CLI flag overrides the environment. For example:

```bash
RS_INFRA_PLATFORM_CLEANUP=0 rs-infra-platform check
RS_INFRA_PLATFORM_CLEANUP=0 rs-infra-platform check --cleanup
rs-infra-platform check --no-cleanup
rs-infra-platform clean --run RUN_ID
rs-infra-platform clean --stale
```

Default cleanup removes the run's host snapshot, build `target`, temporary and transfer files, and reachable owned guest test directories. Reports and archived logs, VM installation disks, SDK/NDK, system images, AVD persistent data, and Cargo registry/git download caches remain. Original project sources, `Cargo.lock`, and `target` remain outside cleanup.

Run records and ownership markers control removal. Cleaners acquire an exclusive lease, check registration and ownership, and reject arbitrary paths, live runs, or unverifiable active owners. `clean --run` accepts a registered run ID, never a filesystem path. It can remove an explicitly retained run. `clean --stale` and the next `check` scan only registered automatic-cleanup runs and leave retained runs alone. Recovery checks machine identity and exact process-creation identity on Linux, macOS, and Windows, with boot-session identity where available. Foreign machines, denied queries, or unknown identity remain conservatively preserved; a PID or old timestamp alone is insufficient.

With cleanup disabled by `--no-cleanup` or the environment policy, the tool retains run files and excludes them from automatic stale deletion. It still stops tool-started VMs/emulators; borrowed environments remain running. If every selected route is native or Android, a later check with cleanup disabled automatically looks for a compatible stopped retained run without pending cleanup. Matching covers the canonical project path, shared toolchain, host OS/architecture, selected platforms/routes/targets, native Cargo program, and Android machine settings. A project, target, toolchain, or backend configuration change therefore starts a separate run.

Reuse holds both the matching-configuration lease and the run lease. It synchronizes current inputs into the owned snapshot, removes inputs that disappeared from the source manifest, and preserves host `target`/Android build outputs for Cargo's incremental work. The original source tree remains untouched. The run ID stays the same; new summaries and logs go into `attempt-N`, so earlier reports remain inspectable. A new input colliding with an unregistered retained file, changed source layout, unsafe markers, or synchronization failure produces an error rather than blindly replacing retained files; rerun with `--cleanup` for a fresh snapshot after reviewing the error.

Selections containing a Windows VM or an unsupported route do not use this automatic reuse path. Windows guest directories are retained under `--no-cleanup` for explicit recovery but are not incrementally reused across runs. Default enabled cleanup always creates a fresh run and removes its build outputs afterward, trading incremental compilation speed for lower disposable disk usage. Download caches remain reusable under either policy.

Reports are written before cleanup. SIGINT/SIGTERM trigger attempted finalization; SIGKILL, power loss, or disconnection can leave artifacts. Recovery uses persisted ownership to stop tool-created environments and leaves borrowed environments running. If the owner identity cannot be established, shutdown remains pending rather than acting on an unverified process or VM. Guest cleanup errors must also be treated as pending work. Keep the report, restore the guest connection, and retry cleanup for the registered run. Do not interpret removal of host files as evidence that unreachable guest files were removed; inspect remaining paths and cleanup errors. Disabling file cleanup does not disable stopping an environment the tool created.

Deleting guest files can increase guest free space without decreasing qcow2/AVD physical allocation on the host. Discard/TRIM depends on the guest filesystem, virtual controller, image, and host filesystem. The backend records Windows `fsutil` diagnostics without claiming a complete discard chain; it does not automatically run offline compaction. `guest_available_delta` and `host_reclaimed_bytes` are separate optional measurements; a null value means unknown, not zero or successful reclamation. Windows environment information can include `guest_available_bytes`, `host_image_allocated_bytes` (allocated blocks × 512), and `discard_trim`; these observations do not by themselves establish a cleanup delta or physical reclamation.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| No suite selected / missing config | Use `--project` for the Rust project and inspect `.infra/platform/platform.toml`; do not reuse the legacy verify format. |
| Toolchain unavailable | Read `.infra/tools/defaults.toml`, install its exact version and targets; remove embedded `+toolchain` arguments. |
| Snapshot or store rejected | Keep the state root outside source roots, use a dedicated directory, and inspect reported unsafe links or changing inputs. |
| Windows SSH failure | Verify alias/port, known host key, noninteractive key authentication, runner PATH, and guest protocol version. |
| Windows filesystem/compile failure | Confirm execution is on local NTFS and that MSVC, SDK, symlink privileges, and long paths satisfy the suite. |
| Emulator unavailable | Inspect SDK/NDK/AVD paths, acceleration, serial selection, ABI, API, boot readiness, and free space. |
| Zero counted tests | Check filters/features and `kind = "test"`; ignored tests and compile-only commands are not executed coverage. |
| Cleanup failed | Preserve the original failure and report, inspect remaining paths and ownership, restore guest access, then explicitly clean the registered run. |

## Real Environment Acceptance

Real tests are ignored by default and need an explicit invocation on a prepared host:

```bash
export RS_INFRA_PLATFORM_E2E_CONFIG=/absolute/path/to/prepared-machine.toml
# On a Linux host:
cargo +1.94.0 test --test platform_e2e_tests linux_native -- --ignored --exact
# On a macOS host:
cargo +1.94.0 test --test platform_e2e_tests macos_native -- --ignored --exact
# On a supported, prepared host:
cargo +1.94.0 test --test platform_e2e_tests windows_vm -- --ignored --exact
cargo +1.94.0 test --test platform_e2e_tests android_emulator -- --ignored --exact
```

`RS_INFRA_PLATFORM_E2E_CONFIG` selects an already prepared machine file for these acceptance tests. Without it, native tests use built-in native defaults; guest tests read the default machine file. The explicit `--ignored --exact` invocation opts into execution. Tests create this crate's own temporary executor fixture, exercise real file operations, and inspect reports and cleanup; they do not run another project's downstream suite.

Run VM and emulator acceptance serially. An absent image or device is not a passed test. Record run IDs, source digests, actual commands/counts, logs, filesystem evidence of cleanup, and host/guest space measurements. Also exercise command failure, timeout, retention, CLI cleanup override, interruption recovery, and guest disconnect. Native macOS acceptance needs a real Mac or explicitly authorized runner; routing and protocol tests alone are insufficient. The repository CI only runs this crate's native checks and executor fixtures; it does not provision these guests.

Return to the [README](../README.md) or [中文指南](setup.zh_CN.md).
