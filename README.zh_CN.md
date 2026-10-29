# rs-infra-platform

[![Rust](https://img.shields.io/badge/rust-1.94%2B-blue.svg)](Cargo.toml)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![English](https://img.shields.io/badge/docs-English-blue.svg)](README.md)

修改 Rust 代码后，无须先推送远端，就能用当前工作区验证平台行为。`rs-infra-platform` 把未提交修改和使用相对路径的本地 Cargo path 依赖一起制作成源码快照，按项目声明的 suite 在本机或已配置客体执行测试，保留报告，并清理本次构建产生的文件。

源码通过 [GitHub](https://github.com/qubit-ltd/rs-infra-platform) 分发。Cargo 包名为 `qubit-infra-platform`，命令名为 `rs-infra-platform`，目前没有发布到 crates.io。

## 安装

先准备 Rust 1.94.0、Git 及宿主平台的构建依赖，在本工具源码目录执行：

```bash
cargo +1.94.0 install --path . --locked
rs-infra-platform --help
```

客体环境需要单独准备。`setup` 输出准备说明；日常 `check` 不安装 Windows、SDK 或工具链，也不会代替用户接受许可证。具体步骤见[安装与操作指南](doc/setup.zh_CN.md)。

## 快速开始

假设你刚改过文件系统测试，需要直接验证这些尚未提交的文件。在待测 Rust 项目的 `.infra/tools/defaults.toml` 中声明共享构建工具链；已有受管理文件时保留现有配置：

```toml
build_toolchain = "1.94.0"
```

再创建 `.infra/platform/platform.toml`：

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

在 Linux 上进入待测项目，执行：

```bash
rs-infra-platform doctor
rs-infra-platform plan --platform linux --suite native-tests
rs-infra-platform check --platform linux --suite native-tests --require linux
```

在 macOS 或 Windows 本机运行时，将平台参数分别换成 `macos` 或 `windows`。示例使用 `--locked`，因此项目需要已有且最新的 `Cargo.lock`，并至少包含一个可运行的库测试。`plan` 只输出 JSON，不创建运行记录或启动客体；`check` 输出 suite 结果和 `summary.json` 位置。验收时查看报告中的实际测试数、`snapshot_digest`、命令日志与清理错误。包含 `test` 命令的必需 suite 至少执行一个测试，忽略的测试不计入执行数量。必需的纯编译 suite 必须成功完成命令，并报告 `checked`。

源码、原始锁文件和原工作区 `target` 不受构建影响。相对 path 依赖按原布局进入快照，用于构建当前项目，不会追加依赖或下游项目的测试任务。绝对 Cargo path 依赖会被拒绝，包括 workspace、patch、replace 声明；工具不会重写 manifest 来迁移路径。相对内部符号链接可以保留；本机绝对链接只有解析后的目标已完整纳入快照时，才会在快照中重定位为相对目标。外部、未捕获、外来绝对路径、逃逸或循环链路仍会拒绝，原链接不改。Windows 传输不接受符号链接，Android 运行数据仅接受普通文件和目录。详见[快照路径边界](doc/setup.zh_CN.md#快照路径边界)。

## 通过基础设施脚本运行

在已迁移项目的根目录更新受管理的引导文件，再按 `.infra/platform/platform.toml` 执行测试：

```bash
./update-infra.sh
./.infra/bin/infra-tool.sh rs-infra-platform --project . check
```

包装脚本按需从 GitHub 解析各工具的 `main` 提交并构建。项目自行维护 `.infra/platform/platform.toml`，`update-infra.sh` 会保留这份测试配置；VM、SDK 等机器信息另写入 `~/.config/rs-infra/platform.toml`。可选环境缺失时告警跳过；要保证 Windows 测试实际执行，可追加 `--require windows`。

若要由 CI 编排器调用平台检查，在 `.infra/ci/ci.toml` 原有的 `tasks` 数组中加入 `"platform"`。也可以继续使用本地源码开发方式，先把平台工具安装到 PATH，再运行编排器：

```bash
cargo +1.94.0 install --path /path/to/rs-infra-platform --locked
cargo +1.94.0 run --manifest-path /path/to/rs-infra-ci/Cargo.toml -- --project /path/to/rs-local-files --only platform check
```

`--project` 和 `--only` 放在 `check` 前。此命令仅执行已配置的 platform 任务；需要筛选 suite 或指定 `--require` 时直接调用平台 CLI。包装脚本将清理交给平台工具，保留 `--no-cleanup` 的语义。本地二进制覆盖方式见[本地 CI 编排接入](doc/setup.zh_CN.md#接入本地-ci-编排器)。

## 平台能力

| 宿主 | Linux suite | Windows suite | macOS suite | Android suite |
| --- | --- | --- | --- | --- |
| Linux | 本机 | 已配置的 QEMU/KVM VM | 告警并跳过 | 已配置的 Emulator/NDK/ADB |
| macOS | 告警并跳过 | 告警并跳过 | 本机 | 已配置的 Emulator/NDK/ADB |
| Windows | 告警并跳过 | 本机，包括 GitHub runner | 告警并跳过 | 告警并跳过 |

表格说明执行路由，不代表已完成实机验收。协议测试通过不能证明 Windows VM 或 Android 设备已实际运行；Linux 上的验证也不能代替 Mac 实机验证。Android 原生 shell 测试只覆盖配置的 ABI，不覆盖 APK 沙箱权限，也不能替代 ARMv7 等其他 ABI 的运行验证。

可选环境未配置时输出 `WARN+SKIP`；已配置环境损坏、命令失败、超时或测试数量不足时失败。必须执行的平台用 `--require windows,android` 声明。未指定 require 且全部跳过时，命令允许返回零，但会明确显示没有执行任何平台测试。

## 常用命令与清理

```bash
rs-infra-platform --project /path/to/project doctor
rs-infra-platform --project /path/to/project plan --platform windows,android
rs-infra-platform --project /path/to/project check --suite native-tests
rs-infra-platform setup --platform windows,android
rs-infra-platform check --no-cleanup
rs-infra-platform clean --run RUN_ID
rs-infra-platform clean --stale
```

本机配置默认读取 `~/.config/rs-infra/platform.toml`，也可通过 `RS_INFRA_PLATFORM_CONFIG` 指定文件。绝对机器路径和连接信息写入本机配置，项目 suite 不保存这些信息。

默认启用清理。命令行 `--cleanup`、`--no-cleanup` 的优先级高于 `RS_INFRA_PLATFORM_CLEANUP`；环境变量接受 `1/true/on` 与 `0/false/off`，不区分大小写。没有命令行覆盖时，空值或非法值会报错。默认清理本次快照、`target`、传输文件和客体临时文件；报告、日志、VM 基础镜像、SDK/NDK、AVD 与 Cargo 下载缓存继续保留。

`--no-cleanup` 保留运行文件，并输出 `clean --run` 命令。选中的执行路由全部为本机或 Android 时，再次禁用清理的检查可以自动复用兼容保留运行的快照和宿主构建产物；工具在独占锁下同步源码，把新报告归档到 `attempt-N`，保留历史报告。选中 Windows VM 时创建新运行，默认启用清理时也始终创建新运行。自动 stale 回收不会删除主动保留的文件；自建环境仍会关闭，借用环境保持运行。精确匹配规则和所有权校验见[清理与恢复](doc/setup.zh_CN.md#清理与恢复)。

客体删除文件后，宿主的虚拟磁盘镜像未必缩小。客体可用空间变化和宿主实际分配空间是两种指标；无法测量时保持未知。工具不自动执行离线磁盘压缩。

## 延伸阅读

- [安装与操作指南](doc/setup.zh_CN.md) / [Setup and operations](doc/setup.md)
- [English README](README.md)
- 命令参考：`rs-infra-platform <command> --help`
- 本地 API 文档：`cargo +1.94.0 doc --no-deps`

新的 `.infra/platform/platform.toml` 独立于旧 `rs-infra-verify` 的 `.infra/ci/platform.toml` 和 `.rs-ci-platform.toml`，不改变旧格式的含义。

## 测试

格式检查需要安装固定 nightly 工具链的 rustfmt 组件。本地与 CI 均显式使用共享配置：

```bash
cargo +nightly-2026-06-05 fmt --all -- --check --config-path .infra/style/rustfmt.toml
cargo +1.94.0 check --locked --all-targets --all-features
cargo +1.94.0 test --locked --all-targets --all-features
cargo +1.94.0 test --locked --doc --all-features
```

CI 工作流从当前源码构建工具，在 Linux、Windows、macOS 上只验证本 crate。小型 fixture crate 用于测试执行器自身。默认测试不启动真实 VM 或模拟器；实机验收需要[显式执行](doc/setup.zh_CN.md#真实环境验收)，本文不宣称已有远端 CI 成功结果。

## 许可证

采用 Apache-2.0，见 [LICENSE](LICENSE)。

## 贡献

保持项目配置与本机配置分离，维护源目录边界，行为变化时同步更新两种语言文档。提出变更前运行上述检查，以及 `.infra/tools/defaults.toml` 中固定工具链的 Clippy。

## 作者

当前包的 Cargo 元数据未声明个人作者。
