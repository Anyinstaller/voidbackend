# voidbackend

**Anyinstaller Protocol (AIP) v1 官方后端 / Official AIP v1 backend**

`voidbackend` 是 Anyinstaller 的安装执行器。它由 stub 从私有会话目录启动，通过本机命名管道接收 client 的设置，安全解压 `payload.zip`，以事务方式提交安装目录，并创建当前用户的开始菜单与桌面快捷方式。

`voidbackend` is the installation executor for Anyinstaller. The stub launches
it from a private session directory. It receives client settings over a local
named pipe, securely extracts `payload.zip`, commits the installation
transactionally, and creates per-user Start Menu and desktop shortcuts.

- 平台 / Platform: Windows 10/11 x64
- 协议 / Protocol: AIP v1, `protocol = 1`
- 产物 / Artifact: `backend.exe`
- 许可证 / License: MPL-2.0

AIP 规范 / AIP specification: <https://github.com/Anyinstaller/aip/blob/main/docs/zh-CN/index.md>

## 功能 / Features

- AIP v1 的 4 字节小端长度前缀 + UTF-8 TOML 帧。
- `setInstallLocation`、`setCreateStartMenuFolder`、`setCreateDesktopShortcut`、`startInstall`、`installProgress`、`installFinished` 完整消息流。
- 实时发送 `installProgress`：稳定阶段、单调百分比和可选 `current` / `total` 字节进度。
- “后发有效”：首次 `startInstall` 前最后一个合法设置生效；非法设置会使对应字段失效，后续合法设置可清除该失效状态。
- 命名管道使用 `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION` 打开。
- 30 秒连接超时；重叠 I/O；部分帧 30 秒超时；单帧上限 1 MiB。
- 严格校验安装路径、环境变量、受保护目录和 reparse point。
- 在目标父目录下创建 128 位随机、仅当前用户与 SYSTEM 可访问的私有暂存目录。
- 安全解压 ZIP：CRC、实际大小、压缩比、条目类型、路径、设备名、ADS、重复项和大小限制。
- 使用临时文件与旧快捷方式备份，原子式提升快捷方式；失败时回滚。
- 使用 `SHGetKnownFolderPath`、`IShellLinkW`、`IPersistFile` 创建快捷方式。
- 可用时将 `config.icon` 安全复制为安装目录内的内部图标文件并用于快捷方式；失败则回退目标 EXE 图标。
- 不联网、不执行 shell、不启动 payload 中的程序或脚本。
- 启动时调用 `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32)`，并静态链接 MSVC CRT，不产生 VCRUNTIME sidecar。
- 发布配置启用 CFG，并默认使用 MSVC 的 ASLR、DEP、`/GS` 与高熵 VA。

## 构建 / Build

要求 / Requirements:

- Windows 10/11 x64
- Rust 1.95+ with the MSVC toolchain

```powershell
cargo build --release
```

产物 / Artifact:

```text
target/release/backend.exe
```

`.cargo/config.toml` 为 `x86_64-pc-windows-msvc` 启用 Control Flow Guard。发布构建还启用了 LTO、`panic = "abort"`、溢出检查和符号剥离。

## 运行方式 / Runtime contract

stub 必须在 AIP 会话目录中启动后端，且只传递本会话的管道名：

```text
backend.exe --aip-pipe-name aip-<32-hex-session-id>-backend
```

后端要求：

- 当前工作目录是包根目录。
- 包根目录中有普通文件 `config.toml` 和 `payload.zip`。
- 包根目录和两个文件均不是 reparse point。
- 管道名符合 `aip-<32-to-128-hex-session-id>-backend`。

未知参数会被忽略；重复或格式错误的 `--aip-pipe-name` 会被拒绝。

后端行为：

1. 校验 `config.toml`，并从 `[defaults]` 初始化会话状态。
2. 在 30 秒内连接私有命名管道。
3. 按顺序处理设置消息，首次 `startInstall` 冻结状态并开始安装。
4. 安装期间发送零次或多次 `installProgress`，覆盖校验、解压、提交和快捷方式阶段。
5. 无论成功或失败，只发送一次 `installFinished`，然后退出。
6. 已向 client 报告 `installFinished` 的正常协议会话以 0 退出，即使 `success = false`；配置、帧或管道等未完成协议的故障使用非零退出码。

## 安装事务 / Installation transaction

1. 展开 `%VAR%`，规范化并校验安装位置。
2. 检查从卷根到目标路径的每一级 reparse point。
3. 在目标父目录下创建随机私有暂存目录。
4. 解压并逐项校验 payload。
5. 递归复查暂存树，并定位快捷方式目标。
6. 将已有安装目录移动到同级的随机备份目录。
7. 将暂存目录重命名为正式安装目录。
8. 恢复安装目录的 DACL 继承。
9. 创建或替换快捷方式。
10. 删除旧安装备份；任一步失败则回滚新目录与快捷方式。

> **更新语义 / Update semantics:** 安装目录按整体替换处理，不与旧目录合并。旧目录只在新安装和快捷方式全部成功后删除。AIP 不规定用户数据保留策略；需要保留用户数据的安装包应把数据放在安装目录之外，或由后续专门的 backend 策略实现。

## 快捷方式目标选择 / Shortcut target selection

只有当 `createStartMenuFolder = true` 或 `createDesktopShortcut = true` 时才需要选择目标。规则按顺序执行：

1. payload 中恰有一个 `.exe` 的文件名主干与净化后的 `name` 不区分大小写相同。
2. payload 根目录恰有一个 `.exe`。
3. payload 总共恰有一个 `.exe`。
4. 否则安装失败，并报告候选列表。

目标必须是安装目录内已存在、非 reparse point 的普通文件。参数固定为空，工作目录固定为安装目录。

如果 `config.icon` 是有效、未超过 64 MiB 且位于包根目录内的包内相对路径，backend 会把图标复制到安装目录中的 `.aip-icon-<random>.<ext>`，并将其用于快捷方式。图标缺失、路径非法、文件过大或复制失败时，回退到目标可执行文件的第 0 个图标，不中断安装。

## 安全限制 / Security limits

| 项目 / Item | 限制 / Limit |
| --- | --- |
| AIP frame | 1 MiB |
| `config.toml` | 64 KiB, UTF-8 without BOM |
| Install path | 32,767 UTF-16 code units |
| ZIP entries | 100,000 |
| Uncompressed total | 8 GiB |
| Single uncompressed file | 4 GiB |
| Compression ratio | 100:1 |
| Payload file | 8 GiB compressed |
| Pipe connect timeout | 30 s |
| Pipe write / partial frame timeout | 30 s |
| App name | 128 Unicode characters |
| Shortcut icon | 64 MiB |

安装位置拒绝：

- 盘符根目录；
- `%SystemRoot%` 及其子目录；
- `%ProgramFiles%`、`%ProgramFiles(x86)%`、`%ProgramData%` 根目录；
- 用户配置目录、桌面、开始菜单根目录；
- 整个 AIP `Anyinstaller\Sessions` 会话目录树及其祖先/后代重叠；
- UNC、网络、设备、相对、盘符相对路径；
- `..`、ADS、NUL、控制字符、设备名、结尾点或空格；
- 任一级 reparse point / junction / symbolic link。

## AIP 合规要点 / AIP compliance

| AIP 要求 | 实现 |
| --- | --- |
| 角色由可执行文件确定，只接收管道名 | `backend.exe` + `--aip-pipe-name` |
| 不直接连接 client | 仅连接 stub 提供的私有管道 |
| 严格帧格式与上限 | `frame.rs`, `pipe.rs` |
| 后发有效与首次 `startInstall` | `message.rs` |
| 实时 `installProgress` | `install.rs`, `payload.rs`, `app.rs` |
| 只发送一次 `installFinished` | `app.rs` |
| 本地盘符绝对安装路径 | `path.rs` |
| 受保护目录与 reparse point 检查 | `path.rs`, `winutil.rs` |
| 随机私有暂存目录 | `acl.rs`, `install.rs` |
| ZIP 路径/炸弹/链接/CRC 防护 | `payload.rs` |
| 已知文件夹 + Shell Link 快捷方式 | `shortcut.rs` |
| 配置图标复用与安全回退 | `install.rs`, `shortcut.rs` |
| 不联网、不执行 shell 或 payload | 全仓库无相关调用 |
| 安全 DLL 搜索与无运行库 sidecar | `SetDefaultDllDirectories` + `+crt-static` |
| 安全二进制配置 | `.cargo/config.toml`, release profile |

stub 负责 Authenticode、组件哈希、会话目录、管道 DACL、子进程 PID/会话校验和 Job Object；这些不是后端的重复职责。

## 测试 / Tests

```powershell
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
cargo build --release
```

测试覆盖：

- 配置、帧、消息顺序和错误状态；
- 安装路径与包路径校验；
- 私有目录 DACL；
- ZIP 正常解压、CRC/路径/链接/压缩比拒绝；
- 已有安装的事务替换；
- Shell Link 实际创建；
- 命名管道完整 AIP 会话，包括一次写入多帧。

## 目录 / Layout

| 路径 | 用途 |
| --- | --- |
| `src/app.rs` | 会话入口与 `installFinished` |
| `src/cli.rs` | AIP 启动参数与管道名校验 |
| `src/config.rs` | `config.toml` |
| `src/frame.rs` | AIP 帧 |
| `src/message.rs` | 消息状态机 |
| `src/pipe.rs` | 重叠 I/O 命名管道客户端 |
| `src/path.rs` | 安装/包路径与名称净化 |
| `src/payload.rs` | 安全 ZIP 解压 |
| `src/install.rs` | 暂存、提交、回滚 |
| `src/acl.rs` | 私有 DACL 与继承恢复 |
| `src/log.rs` | 不因 stderr 缺失而 panic 的诊断输出 |
| `src/shortcut.rs` | 已知文件夹与 Shell Link |
| `src/winutil.rs` | Windows 路径与随机数工具 |
| `tests/protocol.rs` | 完整端到端管道测试 |
