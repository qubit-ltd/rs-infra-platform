# 安装与操作指南

[English](setup.md) · [README](../README.zh_CN.md)

本指南适用于通过 GitHub 分发源码的 `qubit-infra-platform` 0.1.0，目前没有 crates.io 发布版本，面向需要在提交或推送前验证 Rust 工作区修改的开发者。建议先跑通本机测试，再准备项目确实需要的客体。下文的配置样例说明接口，不代表已经完成相应平台的实机验收。

## 用当前工作区运行测试

在本工具源码目录构建并安装：

```bash
cargo +1.94.0 install --path . --locked
rs-infra-platform --help
```

先采用 [README 中完整的本机 suite](../README.zh_CN.md#快速开始)，再进入待测项目执行：

```bash
rs-infra-platform doctor
rs-infra-platform plan --platform linux --suite native-tests
rs-infra-platform check --platform linux --suite native-tests --require linux
```

macOS 或 Windows 本机分别改用 `macos` 或 `windows`。项目需要 `.infra/tools/defaults.toml` 中的共享 `build_toolchain`，以及 `.infra/platform/platform.toml` 中的 suite。命令通过 `RUSTUP_TOOLCHAIN` 统一选择版本，不要在 `argv` 内另写 `cargo +toolchain`。执行前自行安装共享配置指定的精确工具链。

源码快照包含已跟踪文件、当前修改和未被忽略的未跟踪文件。工具递归发现相对 Cargo path 依赖并保持布局，默认排除 `.git` 与 `target`。必须包含的忽略文件通过 `suite.data` 显式声明，路径必须位于项目内。制快照期间源码持续变化会重试，仍不一致则失败。

构建和锁文件变更发生在快照里。仓库中的[本机 fixture](../tests/fixtures/native-smoke/src/lib.rs) 是验证执行器的可运行输入；实际应用测试范围仍由项目自己的 suite 决定。

## 快照路径边界

Cargo 依赖使用 `../rs-redact` 等相对路径时，快照保留兄弟目录布局，并递归纳入依赖。普通、开发、构建、workspace、target 专用、`patch`、`replace` 依赖声明中的绝对 `path` 都会被拒绝；workspace members/default-members 和 `package.workspace` 也必须使用相对路径。这些 Cargo 路径即使在 Unix 上也不接受带盘符的 Windows 形式（含盘符相对形式）、UNC 路径和 Windows 根路径。Unix 上的依赖路径和链接目标若含反斜杠，同样会因客体解释可能不同而拒绝，相对路径请使用 `/`；Windows 宿主仍支持本机相对路径中的反斜杠。工具不会重写原 manifest，也不会自动把绝对 Cargo 依赖路径转换为快照路径。现有 `rs-local-files` 的相对依赖属于支持范围。

相对内部符号链接只有在完整链路逐组件解析后始终位于已纳入的快照输入中，才会保留。对于本机绝对链接，工具先在源端解析完整链路；解析后的目标必须位于已捕获的源码根中，并且是快照清单中的文件或隐含目录，才会把快照副本中的目标重写为相对路径。例如，`rs-json` 源码根目录中的同名别名指回该根时，会变成 `.`，只记录一次，不递归遍历。这样本机执行和 Android 宿主编译可以使用内部源码别名，而不会绕回原工作区。

解析后位于外部或未捕获的目标、外来绝对路径语法、逃逸和循环链路仍会拒绝。相对链接与重定位后的目标必须通过同样的完整链路和快照成员检查，中间目标也不例外；相对链接链路最多展开 40 步。目标在原工作区存在并不足够，原链接始终不改。必要的忽略输入可在符合路径规则时通过 `data` 显式加入，但 `.git`、`target` 等排除目录不能作为链接目标。这项受限链接重定位不代表支持绝对 Cargo 依赖路径。

客体传输可能比快照制作更严格。Windows 不接受符号链接，不会解引用或静默改变其语义；Android `suite.data` 上传也拒绝数据路径和目录树中的符号链接，即使是内部安全链接，运行数据也应使用普通文件和目录。依赖或链接校验失败时，应检查声明路径并采用支持的相对布局，不依赖回到原工作区读取文件。

## 接入本地 CI 编排器

更新后的本地 `rs-infra-ci` 可以直接调用 PATH 中的平台工具，无须先发布远端仓库：

```bash
cargo +1.94.0 install --path /path/to/rs-infra-platform --locked
cargo +1.94.0 run --manifest-path /path/to/rs-infra-ci/Cargo.toml -- --project /path/to/rs-local-files --only platform plan
cargo +1.94.0 run --manifest-path /path/to/rs-infra-ci/Cargo.toml -- --project /path/to/rs-local-files --only platform check
```

路径应指向本次更新后的源码 checkout。项目 `.infra/ci/ci.toml` 的 `tasks` 数组需要包含 `"platform"`，同时具有自己的 `.infra/platform/platform.toml`；加入任务时保留已有任务。本次本地 `rs-local-files` 接入已提供这些配置。`--project`、`--only` 都是编排器顶层参数，放在 `plan` 或 `check` 前。Plan 显示选中的 platform 任务；check 调用 `rs-infra-platform --project PROJECT check`。需要查看具体环境计划、筛选 suite 或指定 `--require` 时直接使用平台 CLI。

编排器设置了 `RS_INFRA_BIN_DIR` 时，从该目录解析平台命令，覆盖 PATH；请使其指向本地构建或安装的可执行文件目录。更新后的 `.infra/bin/infra-tool.sh` 引导入口也已接纳 `rs-infra-platform`，其运行时包装器会先检查 `RS_INFRA_BIN_DIR` 中的可执行覆盖，再请求动态工具。调用平台工具时跳过旧的通用构建产物清理，由平台工具自身的登记运行和 `--no-cleanup` 策略管理生命周期。

日常使用时，在项目根目录更新引导文件，再运行工具包装脚本：

```bash
./update-infra.sh
./.infra/bin/infra-tool.sh rs-infra-platform --project . check
```

包装脚本从 GitHub 解析工具的 `main` 提交并复用构建缓存，不依赖 crates.io 发布。`.infra/platform/platform.toml` 属于项目自己的测试配置，更新器会保留它；每个 suite 的目标平台在这里声明，VM、SDK 等机器信息则放在 `~/.config/rs-infra/platform.toml`。不带筛选参数的 `check` 选择全部已声明 suite；`--platform`、`--suite` 可缩小范围。可选环境未准备时告警跳过；追加 `--require windows` 后，Windows 无法运行就会失败，不能以跳过代替验收。

包装脚本和直接调用均默认清理本次运行文件。`--no-cleanup` 或 `RS_INFRA_PLATFORM_CLEANUP=0` 可以保留中间产物，自建环境仍会停止。

## 分清项目配置和本机配置

项目文件 `.infra/platform/platform.toml` 使用 `version = 1` 与 `[[suites]]`。每个 suite 声明 `id`、`platforms`、`timeout_seconds`、`minimum_tests` 和命令；可选的 `target` 固定 Rust target，`requirements`、`data` 默认空数组。每条命令包含 `kind`（`check`、`build` 或 `test`）、参数数组 `argv` 和相对路径 `cwd`。纯编译 suite 要求命令执行成功并报告 `checked`，不检查测试计数。包含 `test` 命令的 suite 在指定 `--require` 时，会将 `minimum_tests` 提高到至少一个实际测试。测试数量来自测试输出，因此需要可识别的 Rust 测试汇总，不能仅凭退出码零判断跑过测试。

`requirements` 的名称按执行后端区分：

| 后端 | 支持的 `requirements` |
| --- | --- |
| Linux/macOS/Windows 本机 | `native-shell`、`writable-tmpdir`、`ntfs`、`msvc`、`symlinks`、`long-paths` |
| Windows VM 客体 | `ntfs`、`msvc`、`symlinks`、`long-paths` |
| Android | `native-shell`、`writable-tmpdir` |

本机执行在 suite 命令之前实际探测这些条件，依次对应系统 shell 可运行、专用临时目录可写入/读取/重命名、执行目录位于 NTFS、Windows 上的 MSVC Rust 编译链接成功、文件符号链接可创建并读取，以及超过 260 字符的路径可写入并读取。本机后端也接受 `symlink`、`long-path` 别名，建议使用表中的复数写法。名称受支持不代表当前宿主满足条件，例如 `msvc` 需要 Windows 与 MSVC Rust host target，`ntfs` 需要执行目录所在文件系统确实为 NTFS。未知名称或条件不满足都会失败，不会忽略。跨后端共用 suite 时应选择各后端都支持的条件，否则拆成平台专用 suite。下文 Windows 客体和 Android 使用各自的名称；Windows 原生 runner 属于本机一行。

本机配置默认读取 `~/.config/rs-infra/platform.toml`；Windows 同样使用用户目录下的此路径。也可显式指定：

```bash
export RS_INFRA_PLATFORM_CONFIG=/absolute/path/to/machine.toml
```

Windows PowerShell 改用 `$env:RS_INFRA_PLATFORM_CONFIG = 'C:\path\to\machine.toml'`。

默认文件不存在时允许使用默认值；显式指定的文件不存在则报错。未知字段会被拒绝。Linux 上只跑本机测试时可以使用以下配置，将绝对路径改成自己的目录：

```toml
[storage]
root = "/home/dev/.cache/rs-infra/platform"

[native]
cargo = "cargo"
```

`storage.root` 必须是本工具专用的绝对目录。不要指向源码、SDK、VM 镜像目录或含有无关数据的现有目录；所有权校验会拒绝不安全的存储位置。Linux 默认使用绝对的 `${XDG_CACHE_HOME}/rs-infra/platform`，未设置有效绝对路径时用 `~/.cache/rs-infra/platform`；macOS 默认 `~/Library/Caches/rs-infra/platform`；Windows 默认 `%LOCALAPPDATA%/rs-infra/platform`，缺少该变量时使用用户目录下的 `AppData/Local`。

## 在 Linux 上准备 Windows VM

1. 自行提供有合法使用许可的安装介质，手工把 Windows 安装到独立的 qcow2 磁盘。工具不下载介质，也不执行无人值守安装。运行时使用 q35/KVM、IDE 磁盘、e1000 网卡及本机 SSH 端口转发，首次安装时应使镜像兼容这些硬件。
2. 安装 QEMU，并让执行工具的用户具备可用的 KVM 权限。仅有 `/dev/kvm` 不足以证明可用；后端探测会实际打开 KVM 并创建 VM 句柄。同时预留内存与磁盘空间。
3. 在 Windows 安装匹配共享版本的 MSVC Rust 工具链、Visual Studio C++ 构建工具和 Windows SDK。客体通过 `vswhere`、`VsDevCmd` 查找环境，并实际编译链接 Rust 程序。安装选项参考 [Rust 的 MSVC 前置条件](https://rust-lang.github.io/rustup/installation/windows-msvc.html)。
4. 按 [Microsoft OpenSSH 文档](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse) 配置 SSH Server、密钥认证和客体账户。单独核对主机密钥后加入宿主 `known_hosts`；传输要求 `StrictHostKeyChecking=yes`，且不能依赖交互输入密码。
5. 将同一份工具源码构建成 Windows 可执行文件，把 `rs-infra-platform.exe` 放入 SSH 账户的 PATH。宿主通过固定的 `rs-infra-platform guest` 命令和标准输入输出交换版本化 JSON；shell 启动信息不能污染 stdout。两端需要兼容的协议版本。
6. 为客体账户准备本地 NTFS 上的独立执行根目录，不使用 SMB 或宿主共享目录。在 SSH 可见环境设置 `RS_INFRA_PLATFORM_GUEST_ROOT`，或使用默认 `%LOCALAPPDATA%/rs-infra-platform/guest`。宿主的 `guest_root` 填写同一位置；该字段不会代替用户配置客体环境。

将以下内容追加到本机配置，并替换路径：

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

从 `qemu` 到 `guest_root` 的字段必填，其余展示的是默认值。QEMU 和镜像应使用绝对路径。QMP 文件必须名为 `windows.qmp`，路径短于 100 字节且不含逗号；其父目录必须专用、初始为空。工具建立 `.rs-infra-platform-qmp` 标记、设置私有 `0700` 权限，并通过租约串行使用指定镜像；非空且无标记的目录会被拒绝。SSH 别名仅允许 ASCII 字母、数字、`.`、`_`、`-`，且不能以 `-` 开头。

对应的 `~/.ssh/config` 可以是：

```sshconfig
Host rs-infra-windows
    HostName 127.0.0.1
    Port 2222
    User tester
    IdentityFile ~/.ssh/rs-infra-windows
    IdentitiesOnly yes
```

别名端口需要与 `ssh_port` 一致，首次信任主机密钥前先与客体核对。suite 可以声明 `ntfs`、`msvc`、`symlinks`、`long-paths` 条件；需要符号链接时开启 Windows 开发者模式或授予相应权限，需要长路径时配置系统支持。必要条件不满足会失败，不会静默跳过。NTFS 和可用的 MSVC 构建环境是 Windows 客体的基础要求。

先运行 `doctor`，检查 `plan --platform windows`，再执行 `check --platform windows --require windows`。`doctor` 不启动客体，只检查宿主前置条件；客体连接后才检查 NTFS、MSVC、空间和 suite 条件。镜像已配置却不存在、KVM 不可用均属于错误；实际 SSH 连接与 Windows 测试需要另行验收。

已存在且 QMP 响应正常的实例视为借用实例，否则工具自行启动 QEMU。工具只关闭自己启动的进程。不要在租约之外同时修改同一客体磁盘。

## 准备 Android

Android 流程先在宿主交叉编译，再通过 ADB 把 Rust 测试程序放入专用 `/data/local/tmp` 目录执行。它不生成 APK，也不执行 instrumentation 测试。

准备 Java、Android 命令行工具、SDK platform-tools、Emulator、NDK 和匹配 ABI 的系统镜像。以下 Linux 示例沿用开发环境选择的组件版本，仅作为配置输入，不代表工具会自动安装：

```bash
export ANDROID_HOME="$HOME/Android/Sdk"
export ANDROID_AVD_HOME="$HOME/Android/avd"
export PATH="$ANDROID_HOME/cmdline-tools/22.0/bin:$ANDROID_HOME/platform-tools:$ANDROID_HOME/emulator:$PATH"
sdkmanager --sdk_root="$ANDROID_HOME" 'platform-tools' 'emulator' 'ndk;27.3.13750724' 'system-images;android-35;default;x86_64'
```

阅读安装所需条款，在得到授权后交互接受对应许可，不要向全部许可提示批量输入同意。开发环境中这四个指定组件已获得安装授权，不代表其他机器的用户也已接受许可。包安装与许可证处理可参考[官方包管理说明](https://developer.android.com/tools/sdkmanager)。示例使用选定命令行工具中的 `sdkmanager`，不假定它是厂商最新 CLI。

创建专用 AVD 并安装匹配的 Rust target：

```bash
avdmanager create avd --name rs-infra-api35-x86_64 --package 'system-images;android-35;default;x86_64'
rustup target add --toolchain 1.94.0 x86_64-linux-android
emulator -accel-check
```

共享工具链不同则替换为项目实际版本。`avdmanager` 询问自定义硬件配置时，按目标测试环境选择。开发宿主选择 API 35、AOSP x86_64、2 核、2048 MiB 内存，独立持久化 AVD 目录与 `emulator-5580` 序列号；环境准备完成并不等于 suite 已通过。

Linux 需要可用的 KVM 权限；macOS 使用本机 hypervisor 和适合宿主的镜像。Apple Silicon 应使用 arm64 镜像与 `aarch64-linux-android`，不能原样照搬 Linux x86_64 示例。加速条件见 [Android 官方说明](https://developer.android.com/studio/run/emulator-acceleration)。Linux 和 macOS 本机 suite 也必须分别在对应系统实际运行。

借用已运行模拟器时，在本机文件中追加：

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

`sdk_root`、`ndk_version` 必填。`serial` 表示借用设备，`avd` 表示启动专用 AVD，二者必须且只能选择一个。要由工具启动上述 AVD，把 `serial` 换成 `avd = "rs-infra-api35-x86_64"`，不要同时保留。API 默认 35，当前支持 21 至 35；Linux ABI 默认 `x86_64`，Apple Silicon 默认 `arm64-v8a`。除 `avd_home` 外其余显示值均为默认值；`avd_home` 默认取 `ANDROID_AVD_HOME`，否则为 `~/.android/avd`。`sdk_root` 和显式 `avd_home` 均须为绝对路径。工具自行启动模拟器时启用加速、禁用快照加载与保存，不会清空持久化 AVD 数据。

用 `adb -s emulator-5580 shell getprop sys.boot_completed` 与 `adb -s emulator-5580 shell getprop ro.product.cpu.abi` 检查指定设备。设备已连接仍需满足 API、ABI、shell 就绪和 suite 条件。自行启动的模拟器结束时关闭，借用设备不会关闭。

README 中的桌面 suite 没有选中 Android。检查前，在项目 `.infra/platform/platform.toml` 中追加：

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

随后运行 `rs-infra-platform doctor`、`rs-infra-platform plan --platform android`、`rs-infra-platform check --platform android --suite android-native --require android`。按报告检查命令日志与实际测试数，再判断是否获得 Android 运行覆盖。

runner 只为本次进程注入 target runner 与 NDK linker，不修改用户 `.cargo/config.toml`。suite 不填 `target` 时采用配置的 ABI，也可填写匹配的 Rust target。Android Cargo `argv` 不得自行覆盖 `--target`、`--target-dir`、`--config`、`--manifest-path`。设备 API 不能低于编译 API，ABI 必须一致。跨目标 doctest 当前不支持，因此 `cargo test` 必须明确选择 `--lib`、`--tests` 或 `--test NAME` 等测试目标；`--doc` 会报错，不会被静默省略。ARMv7 编译检查仍应单独记为纯编译验证，x86_64 模拟器不能证明 ARMv7 运行正确。

## 查看结果和报告

`plan` 输出 suite、路由与能力，不启动环境或清理历史运行；`doctor` 诊断配置和前置条件；`setup --platform windows,android` 输出准备指引。三个命令均不执行安装。

执行 `check` 后按终端给出的路径查看报告。首次执行把 `reports/RUN_ID/summary.json`、逐条命令的 stdout/stderr 与可清理的 `runs/RUN_ID` 分开保存；复用保留运行时不覆盖原报告，而是写入 `reports/RUN_ID/attempt-N/summary.json` 与本次命令日志。汇总包含源码摘要、环境信息、测试数、退出码、超时与取消结果、保留位置和清理错误。建议将这些证据与待审查的变更一起保存。`windows-session.json` 等恢复文件包含受限会话令牌，应保持报告目录私有，公开分享前检查产物内容。

| 观察到的结果 | 含义与下一步 |
| --- | --- |
| `WARN+SKIP` | 可选环境未配置或执行方式不支持；准备环境，或显式 require。 |
| `infrastructure_failed` | 已配置环境、传输、启动或执行准备失败；检查日志并修复。 |
| `failed` | 命令失败、超时、取消或实际测试数不足；检查对应命令日志。 |
| `checked` | 命令成功且 suite 允许零测试；不能算作运行测试覆盖。 |
| `passed` | 实际执行的测试数满足要求；仍需分别检查清理结果。 |
| `no platform tests executed` | 没有 suite 跑出可计数测试；可选 suite 全跳过时可能退出零。 |

`--require windows,android` 使选中平台的缺失和跳过变成失败。选中的纯编译 suite 必须成功完成命令，包含 `test` 命令的 suite 则至少执行一个实际测试。未知 suite、未知平台、被筛选条件排除的 required 平台、无效本机配置和越界路径都会报错。测试成功但必要清理失败时整体仍返回非零；报告分别保留测试与清理结果。

## 清理与恢复

启动环境前按以下顺序解析清理策略：

1. 命令行 `--cleanup` 或 `--no-cleanup`，两者互斥。
2. `RS_INFRA_PLATFORM_CLEANUP`，接受 `1/true/on` 或 `0/false/off`，不区分大小写。
3. 默认启用清理。

没有命令行覆盖时，空值、带空白的值和其他非法内容均报错。例如：

```bash
RS_INFRA_PLATFORM_CLEANUP=0 rs-infra-platform check
RS_INFRA_PLATFORM_CLEANUP=0 rs-infra-platform check --cleanup
rs-infra-platform check --no-cleanup
rs-infra-platform clean --run RUN_ID
rs-infra-platform clean --stale
```

默认删除本次宿主快照、构建 `target`、临时及传输文件，以及可连接的客体中本工具登记的测试目录。报告与归档日志、VM 安装磁盘、SDK/NDK、系统镜像、AVD 持久数据、Cargo registry/git 下载缓存继续保留。原项目源码、`Cargo.lock` 与 `target` 不在清理范围内。

清理依赖运行记录和所有权标记，并先获取独占租约。工具校验登记与归属，不接受任意路径，不删除仍在运行或无法确认失活的活动运行。`clean --run` 参数是登记的运行 ID，不是文件路径，也可用于回收主动保留的运行。`clean --stale` 和下次 `check` 只扫描登记过、启用自动清理的运行，跳过主动保留项。Linux、macOS、Windows 通过机器身份和精确进程创建身份判断原进程，具备启动会话标识时也参与核对。记录来自其他机器、查询被拒绝或身份不明时仍保守保留，不能仅凭 PID 或时间过旧回收。

通过 `--no-cleanup` 或环境变量禁用清理后，工具保留运行文件，并排除自动 stale 删除；自建 VM/模拟器仍会关闭，借用环境继续运行。选中路由全部为本机或 Android 时，后续同样禁用清理的检查会自动寻找兼容、已停止且没有待清理事项的保留运行。匹配条件包括规范化项目路径、共享工具链、宿主 OS/架构、选中平台/路由/target、native Cargo 程序与 Android 本机设置。项目、target、工具链或后端设置变化会使用另一运行。

复用时同时持有配置组合租约与运行租约，把当前源码同步到本工具的快照；源清单中已删除的输入也从快照移除，保留宿主 `target` 和 Android 构建产物用于 Cargo 增量构建。原工作区不受影响。运行 ID 不变，新汇总与日志进入 `attempt-N`，旧报告仍可查看。新输入与未登记的保留文件冲突、源码布局变化、标记异常或同步失败时明确报错，不会盲目覆盖保留文件；检查错误后可改用 `--cleanup` 创建全新快照。

选中 Windows VM 或不支持的路由时不走这条自动复用途径。`--no-cleanup` 保留的 Windows 客体目录用于显式恢复，不会跨运行进行增量复用。默认启用清理时始终创建新运行，结束后删除其构建产物，以增量编译速度换取较少的临时磁盘占用；两种策略都保留下载缓存。

报告在清理前落盘。SIGINT/SIGTERM 会触发尽力收尾；SIGKILL、掉电或失联可能留下文件。恢复流程依据持久化所有权停止工具自建环境，借用环境继续运行；无法确认原所有者身份时保留待关闭状态，不操作未经核验的进程或 VM。客体清理报错同样应视为待处理事项：保留报告，恢复连接后对登记运行重试清理。宿主文件已删除不代表失联客体已清完，应核对剩余路径和清理错误。禁用文件清理不等于禁用自建环境的停止流程。

客体文件删除后，可用空间可能增加，但 qcow2/AVD 在宿主上的实际占用未必减小。discard/TRIM 是否生效取决于客体文件系统、虚拟控制器、镜像与宿主文件系统。后端记录 Windows 的 `fsutil` 诊断，不宣称完整 discard 链路已验证，也不会自动离线压缩。`guest_available_delta` 与 `host_reclaimed_bytes` 是独立的可选测量值；`null` 表示未知，不表示零或已完成物理回收。Windows 环境信息可包含 `guest_available_bytes`、`host_image_allocated_bytes`（已分配块数 × 512）和 `discard_trim`，这些观察本身不能证明清理差值或物理空间已释放。

## 按症状排障

| 症状 | 检查顺序 |
| --- | --- |
| 没选中 suite 或找不到配置 | 用 `--project` 指向待测 Rust 项目，检查 `.infra/platform/platform.toml`，不要套用旧 verify 格式。 |
| 工具链不可用 | 读取共享 defaults，安装精确版本和 target，移除命令内的 `+toolchain`。 |
| 快照或存储目录被拒绝 | 让状态目录位于源码根之外，使用专用目录，检查报错中的链接或持续变动输入。 |
| Windows SSH 失败 | 依次检查别名/端口、主机密钥、免交互认证、runner PATH 和协议版本。 |
| Windows 文件系统或编译失败 | 确认执行目录位于本地 NTFS，MSVC/SDK、符号链接权限和长路径满足 suite。 |
| 模拟器不可用 | 检查 SDK/NDK/AVD 路径、加速能力、serial、ABI、API、启动就绪与空间。 |
| 实际测试数为零 | 检查过滤条件、features 与 `kind = "test"`；忽略测试和纯编译不算运行覆盖。 |
| 清理失败 | 保留原始错误和报告，核对剩余路径及归属，恢复客体连接，再显式清理登记运行。 |

## 真实环境验收

真实环境测试默认忽略，需要在准备好的宿主显式执行：

```bash
export RS_INFRA_PLATFORM_E2E_CONFIG=/absolute/path/to/prepared-machine.toml
# Linux 宿主：
cargo +1.94.0 test --test platform_e2e_tests linux_native -- --ignored --exact
# macOS 宿主：
cargo +1.94.0 test --test platform_e2e_tests macos_native -- --ignored --exact
# 已准备好的受支持宿主：
cargo +1.94.0 test --test platform_e2e_tests windows_vm -- --ignored --exact
cargo +1.94.0 test --test platform_e2e_tests android_emulator -- --ignored --exact
```

`RS_INFRA_PLATFORM_E2E_CONFIG` 为验收测试选择已准备好的本机文件；未设置时，本机测试采用内置默认配置，客体测试读取默认本机文件。`--ignored --exact` 明确启用对应测试。这些测试会创建本 crate 自己的临时执行器 fixture，执行真实文件操作并检查报告与清理，不是其他项目的下游 suite。

VM 与模拟器验收串行运行；缺少镜像或设备不能算作通过。记录 run ID、源码摘要、实际命令和用例数、日志、目录清理证据及宿主/客体空间变化，并覆盖失败、超时、保留、命令行覆盖、中断恢复和客体断连。macOS 原生验收需要真实 Mac 或明确授权的 runner，路由与协议测试不能代替。仓库自身 CI 只验证本 crate 的原生逻辑与执行器 fixture，不负责准备这些客体。

返回 [README](../README.zh_CN.md) 或 [English guide](setup.md)。
