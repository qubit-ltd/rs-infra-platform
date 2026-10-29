# rs-infra-platform

[![Rust](https://img.shields.io/badge/rust-1.94%2B-blue.svg)](Cargo.toml)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![中文文档](https://img.shields.io/badge/文档-中文版-blue.svg)](README.zh_CN.md)

Test the Rust sources on your development machine before pushing them. `rs-infra-platform` snapshots current files, including uncommitted changes and relative local Cargo path dependencies, runs declared suites on native hosts or configured guests, and preserves reports while cleaning disposable build files.

The source is distributed through [GitHub](https://github.com/qubit-ltd/rs-infra-platform). The Cargo package is `qubit-infra-platform`; the executable is `rs-infra-platform`. This tool is not published on crates.io.

## Installation

Install Rust 1.94.0, Git, and the build prerequisites for your host. From this tool's source directory:

```bash
cargo +1.94.0 install --path . --locked
rs-infra-platform --help
```

Guest environments require separate preparation. `setup` prints guidance; `check` does not install Windows, SDKs, toolchains, or accept vendor licenses. See the [setup guide](doc/setup.md).

## Quick Start

Suppose you have changed a filesystem test and need to run those exact files locally. In the Rust project being tested, put its shared build toolchain in `.infra/tools/defaults.toml` (retain an existing managed file):

```toml
build_toolchain = "1.94.0"
```

Declare `.infra/platform/platform.toml`:

```toml
version = 1

[[suites]]
id = "native-tests"
platforms = ["linux", "windows", "macos"]
timeout_seconds = 1200
minimum_tests = 1
requirements = []
data = []

[[suites.commands]]
kind = "test"
argv = ["cargo", "test", "--locked", "--lib"]
cwd = "."
```

Run from that project on Linux:

```bash
rs-infra-platform doctor
rs-infra-platform plan --platform linux --suite native-tests
rs-infra-platform check --platform linux --suite native-tests --require linux
```

Use `macos` or `windows` instead on that native host. The project needs a current `Cargo.lock` for this `--locked` example and at least one runnable library test. `plan` prints JSON without creating a run or starting guests. `check` prints the suite outcome and `summary.json` path. Inspect the report's executed test counts, `snapshot_digest`, command log paths, and cleanup errors before accepting the result. A required suite containing a `test` command must run at least one test; ignored tests do not count. Required compile-only suites must complete their commands successfully and report `checked`.

The original source tree, lockfile, and `target` are preserved. Relative local path dependencies enter the snapshot with their layout intact; this does not schedule their own downstream test suites. Absolute Cargo path dependencies are rejected, including workspace, patch, and replace declarations; manifests are not rewritten to relocate them. Relative internal symlinks are preserved. A host-local absolute symlink can be relocated to a relative snapshot target only when its resolved target is already fully captured in that snapshot; external, uncaptured, foreign absolute, escaping, and cyclic link chains remain rejected. Original links are unchanged. Windows transfer rejects symlinks, and Android runtime data must contain ordinary files/directories. See the [snapshot boundaries](doc/setup.md#snapshot-path-boundaries).

## Use with Infrastructure Scripts

From a migrated project's root, update its managed bootstrap files and run the suites declared in `.infra/platform/platform.toml`:

```bash
./update-infra.sh
./.infra/bin/infra-tool.sh rs-infra-platform --project . check
```

The wrapper resolves and builds tools from their GitHub `main` revisions as needed. The project owns `.infra/platform/platform.toml`: `update-infra.sh` preserves it rather than generating the project's test policy. Keep VM and SDK settings in the separate machine file `~/.config/rs-infra/platform.toml`. Missing optional environments warn and skip; use `--require windows` when Windows tests must run.

To include platform checks in CI orchestration, add `"platform"` to the existing `tasks` array in `.infra/ci/ci.toml`. Local source development is also supported; install this tool into PATH, then run the coordinator:

```bash
cargo +1.94.0 install --path /path/to/rs-infra-platform --locked
cargo +1.94.0 run --manifest-path /path/to/rs-infra-ci/Cargo.toml -- --project /path/to/rs-local-files --only platform check
```

`--project` and `--only` precede `check`. This runs only the configured platform task; use the platform CLI directly for suite filters or `--require`. The infrastructure wrapper lets the platform tool own cleanup, including `--no-cleanup`. See [local infrastructure integration](doc/setup.md#use-the-local-ci-orchestrator) for wrapper overrides.

## Platform Capabilities

| Host | Linux suites | Windows suites | macOS suites | Android suites |
| --- | --- | --- | --- | --- |
| Linux | Native | Configured QEMU/KVM VM | Warn and skip | Configured Emulator/NDK/ADB |
| macOS | Warn and skip | Warn and skip | Native | Configured Emulator/NDK/ADB |
| Windows | Warn and skip | Native, including GitHub runners | Warn and skip | Warn and skip |

This table describes routing, not completed real-machine acceptance. Protocol tests do not prove a Windows VM or Android device ran successfully, and Linux validation does not validate a real Mac. Android native shell tests cover the configured ABI; they do not cover APK sandbox permissions or another ABI such as ARMv7.

Missing optional environments produce `WARN+SKIP`. Configured but broken environments, command failures, timeouts, and unmet test counts fail. Add `--require windows,android` when those platforms must execute. If every suite skips, a run without `--require` may exit zero and explicitly reports that no platform tests executed.

## Commands and Cleanup

```bash
rs-infra-platform --project /path/to/project doctor
rs-infra-platform --project /path/to/project plan --platform windows,android
rs-infra-platform --project /path/to/project check --suite native-tests
rs-infra-platform setup --platform windows,android
rs-infra-platform check --no-cleanup
rs-infra-platform clean --run RUN_ID
rs-infra-platform clean --stale
```

Machine settings default to `~/.config/rs-infra/platform.toml`; `RS_INFRA_PLATFORM_CONFIG` selects another file. Absolute machine paths and connection settings belong there, not in project suites.

Cleanup is enabled by default. `--cleanup` or `--no-cleanup` overrides `RS_INFRA_PLATFORM_CLEANUP`; the variable accepts `1/true/on` and `0/false/off` case-insensitively. Empty or invalid values fail unless a CLI flag overrides them. Default cleanup removes this run's snapshot, `target`, transfer files, and guest temporary files. Reports, logs, VM base images, SDK/NDK, AVDs, and Cargo download caches remain.

`--no-cleanup` retains run files and prints the explicit `clean --run` command. When every selected route is native or Android, another check with cleanup disabled can automatically reuse a compatible retained run's snapshot and host build outputs. The tool synchronizes source inputs under exclusive locks and archives each new report in `attempt-N`; historical reports remain intact. Windows VM selections allocate a fresh run, and default cleanup always uses a fresh run. Retained runs are excluded from automatic stale cleanup. Tool-started environments still stop, while borrowed environments remain running. See [cleanup and recovery](doc/setup.md#cleanup-and-recovery) for exact matching rules and ownership checks.

Deleting guest files does not guarantee the host's VM image shrinks. Guest free-space changes and host physical allocation are separate measurements; unavailable measurements remain unknown. The tool does not automatically compact disks offline.

## Learn More

- [Setup and operations](doc/setup.md) / [安装与操作指南](doc/setup.zh_CN.md)
- [中文 README](README.zh_CN.md)
- CLI reference: `rs-infra-platform <command> --help`
- Local API documentation: `cargo +1.94.0 doc --no-deps`

The new `.infra/platform/platform.toml` is independent of the older `rs-infra-verify` `.infra/ci/platform.toml` and `.rs-ci-platform.toml` formats.

## Testing

Install the pinned nightly toolchain's rustfmt component for the formatting check. Use the shared configuration in both local checks and CI:

```bash
cargo +nightly-2026-06-05 fmt --all -- --check --config-path .infra/style/rustfmt.toml
cargo +1.94.0 check --locked --all-targets --all-features
cargo +1.94.0 test --locked --all-targets --all-features
cargo +1.94.0 test --locked --doc --all-features
```

The CI workflow builds this source and tests this crate on Linux, Windows, and macOS. Its small fixture crates test the executor itself. Default tests do not start real VMs or emulators. Real environment acceptance is an explicit operation described in the [guide](doc/setup.md#real-environment-acceptance); no hosted CI result is claimed here.

## License

Licensed under Apache-2.0. See [LICENSE](LICENSE).

## Contributing

Keep project and machine configuration separate, preserve source-directory boundaries, and update both language versions when behavior changes. Run the checks above and the pinned Clippy toolchain in `.infra/tools/defaults.toml` before proposing a change.

## Author

No individual author is declared in this package's Cargo metadata.
