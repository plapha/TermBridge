# TermBridge

[English](README.md) | **简体中文**

TermBridge 是一个跨平台的远程终端工具。终端会话运行在接收端，客户端断开后会话继续运行，重新连接时恢复当前画面。

每台安装了 TermBridge 的设备都可以同时作为接收端和连接端。传输层使用 SSH，接收端内置基于 [russh](https://github.com/Eugeny/russh) 的 SSH 服务，适用于局域网、Tailscale 等可以直接访问的网络。

> 当前版本 0.2.0，处于开发阶段，详见[开发状态](#开发状态)。

## 功能

- 会话在接收端持续运行，客户端断开或网络中断不影响会话，重新附着时恢复画面并继续接收输出
- 按键实时发送，支持 vim、htop、less 等全屏程序
- 同一会话可以被多个客户端同时查看，同一时间只有一个客户端可以输入
- 支持 SSH 公钥认证和独立密码认证，首次连接时确认主机指纹
- 提供桌面图形界面和 `termbridge` 命令行工具，均支持英文和简体中文（见[界面语言](#界面语言)）
- 支持 Windows、Linux、macOS

TermBridge 使用自定义的 SSH 子系统协议（`termbridge-v2`），不能与标准 SSH 客户端或服务端互通，也不提供中继和 NAT 穿透。

## 安装

从 [Releases](https://github.com/plapha/TermBridge/releases) 下载对应平台的安装包：

| 平台 | 安装包 |
|---|---|
| Windows 10/11 x64 | `TermBridge_<版本>_x64-setup.exe` |
| Debian / Ubuntu x86_64 | `TermBridge_<版本>_amd64.deb` |
| macOS（Apple Silicon / Intel） | `TermBridge_<版本>_universal.dmg` |

安装包内附带同版本的 `termbridge` 命令行工具。0.2 与 0.1.x 的协议不兼容，连接双方都需要升级到 0.2。

安装包未做代码签名。Windows 出现 SmartScreen 提示时，选择「更多信息」→「仍要运行」。macOS 首次打开时右键点击应用并选择「打开」；如果提示应用已损坏，执行：

```sh
xattr -dr com.apple.quarantine /Applications/TermBridge.app
```

## 基本概念

| 术语 | 说明 |
|---|---|
| 接收端 | 被连接的一方，负责创建和保存会话，默认不启用 |
| 连接端 | 发起连接的一方，使用图形界面或命令行 |
| 连接配置 | 连接端保存的地址、端口、用户名和认证方式（profile） |
| 会话 | 接收端上的一个终端，以 UUID 标识。Windows 上为 PowerShell，其他系统为用户的默认 shell |
| 控制权 | 会话的输入权限。首个附着的客户端获得控制权，其他客户端需要接管后才能输入 |
| 主机指纹 | 接收端 SSH 主机密钥的 SHA256 指纹。首次连接时确认，之后不一致即拒绝连接 |

## 使用

以下以 A 作为接收端、B 作为连接端为例。

### 启用接收端（A）

图形界面：在「本应用接收端」面板中选择认证方式并初始化，设置监听地址和端口后点击「启用」。

命令行：

```sh
termbridge host init --authorized-keys ~/.ssh/authorized_keys   # 使用 SSH 公钥认证（推荐）
termbridge host init                                            # 或使用独立密码认证，至少 12 个字符

termbridge host status                             # 查看主机指纹、登录用户名和认证方式
termbridge host enable --listen 100.64.0.5:22333   # 设置监听地址
termbridge host run                                # 前台运行，Ctrl+C 停止
```

- 登录用户名为初始化时的系统用户名，可通过 `host status` 查看。
- 监听地址需要显式指定。仅在本机测试时可使用 `127.0.0.1:22333`，不建议在不可信网络中监听 `0.0.0.0`。
- `authorized_keys` 中带有 `from=`、`command=` 等选项的条目不受支持，导入时会报错。
- 修改认证配置后需要重启接收端。

### 连接（B）

图形界面：在「连接配置」中新建配置，填写主机、端口、用户名和认证方式。保存后点击「新建终端」创建会话，或点击「已有终端」附着到正在运行的会话。首次连接时会显示主机指纹，请与 A 上 `host status` 的输出核对后再确认。

会话标签上的「接管输入」用于获取控制权，「分离」断开当前客户端并保留会话，「终止」结束会话。

命令行：

```sh
termbridge profile add a-box 100.64.0.5 --port 22333 -u <用户名> --auth key
termbridge session create -p a-box --title build
termbridge session list   -p a-box
termbridge session attach -p a-box --session-id <UUID>
termbridge session end    -p a-box --session-id <UUID>
```

- `--auth key` 使用 SSH 私钥，默认依次查找 `~/.ssh/id_ed25519`、`id_ecdsa`、`id_rsa`，可通过 `--key-path` 指定。省略 `--auth` 时使用独立密码，连接时输入；加 `--remember-password` 可保存到系统凭据库。
- 首次连接时会显示主机指纹，输入 `yes` 确认。
- `session attach` 省略 `--session-id` 时附着到第一个运行中的会话。
- 会话正被其他客户端控制时，`session end` 需要加 `--take-control`。

### 命令行附着

`session attach` 会将本地终端切换到 raw 模式，按键原样发送到远端，退出时恢复终端设置。该命令需要在交互式终端中运行，不支持管道输入。

本地快捷键以 Ctrl+] 为前缀，前缀后按其他键则取消：

| 按键 | 功能 |
|---|---|
| Ctrl+] d | 分离，远端会话继续运行 |
| Ctrl+] t | 接管控制权 |
| Ctrl+] e | 结束远端会话，需在 3 秒内再按一次 Ctrl+] e 确认 |
| Ctrl+] Ctrl+] | 向远端发送 Ctrl+] |

## 界面语言

命令行和桌面端支持英文和简体中文；系统语言为中文时默认使用中文，其余情况默认英文。

- 命令行：语言按以下顺序确定：`--lang en|zh`、环境变量 `TERMBRIDGE_LANG`、`LC_ALL` / `LC_MESSAGES` / `LANG`（Windows 上还会参考用户默认区域设置），最后回退到英文。`--lang` 可以写在命令行的任意位置，例如 `termbridge --lang zh host status`，同时决定 `--help` 的语言；clap 自带的固定标题（如 “Usage”“Options”）仍为英文。
- 桌面端：在顶栏的语言选择器中切换，选择会被记住；没有选择时跟随系统语言。后端返回的错误信息和托盘菜单会随之切换。
- 协议本身与语言无关：接收端发送英文诊断文本和稳定的错误码，由各客户端按错误码显示自己的译文。

## 会话生命周期

- 客户端断开、退出或网络中断时，会话继续运行。重新附着后先收到当前画面，再接收后续输出。目前不支持自动重连。
- 终端中的 shell 退出（例如执行 `exit`）后，会话立即移除并释放资源，已附着的客户端会收到结束通知。
- 接收端停止时（Ctrl+C、图形界面中点击「停止本应用接收」、从托盘退出），其上的所有会话一并结束。Windows 上通过 Job Object 结束终端内启动的全部子进程，接收端异常退出时同样生效；Linux 和 macOS 上按会话（sid）结束 shell 及其启动的全部进程，包括后台任务、`nohup` 启动的进程和忽略 SIGHUP 的进程。主动调用 `setsid` 脱离会话的守护进程不受影响，接收端被强杀（SIGKILL）时也不会清理。
- 会话仅保存在内存中，接收端或系统重启后不会恢复。

## 安全

- 推荐使用 SSH 公钥认证。使用独立密码时，接收端只保存其 Argon2 哈希，不读取也不校验系统账户密码。
- 同一 IP 连续认证失败 5 次后，10 分钟内拒绝该 IP 的连接。
- 主机指纹在首次连接时由用户确认，之后指纹变化即拒绝连接。TermBridge 使用独立的主机密钥，与系统 OpenSSH 的主机密钥无关。
- 输入按字节偏移编号并由接收端确认，重复数据会被丢弃，缺失部分从确认点重发。无法确定对端接收状态时（如断线），客户端丢弃未确认的输入并给出提示，不会自动重发。
- 只有持有控制权的客户端的输入会写入终端。
- 密码和图形界面记住的指纹优先保存在系统凭据库（Windows 凭据管理器、macOS 钥匙串、Linux Secret Service）。凭据库不可用时，指纹保存在权限受限的 `known_hosts.json` 中，密码在每次连接时输入。
- 私钥只从原路径读取，不会复制，私钥口令不会保存。日志中不记录密码和终端原始输出。

配置目录：Windows 为 `%LOCALAPPDATA%\TermBridge\`，Linux 和 macOS 为 `$XDG_CONFIG_HOME/termbridge/`（默认 `~/.config/termbridge/`）。其中 `host.json`、`host_key` 属于接收端，`profiles.json` 属于连接端。

## 作为系统服务运行（Linux）

无桌面环境的 Linux 可以只编译命令行工具：

```sh
cargo build -p termbridge --release     # 输出 target/release/termbridge
```

创建 `~/.config/systemd/user/termbridge.service`：

```ini
[Unit]
Description=TermBridge host

[Service]
ExecStart=/绝对路径/termbridge host run
Restart=on-failure

[Install]
WantedBy=default.target
```

启用服务：

```sh
systemctl --user enable --now termbridge
loginctl enable-linger $USER    # 用户未登录时保持运行
```

服务重启后原有会话不会恢复。

## 桌面端说明

- 关闭窗口时程序隐藏到托盘，点击托盘图标可恢复窗口。通过托盘菜单中的「退出 TermBridge」退出程序，退出时会停止本机接收端并结束其上的会话。
- 桌面端不会开机自启，启动时也不会自动启用接收端。需要无人值守运行时，请使用命令行工具配合系统服务。

## 从源码构建

依赖：Rust stable、Node.js 22、Python 3，以及 [Tauri 2 的系统依赖](https://v2.tauri.app/start/prerequisites/)（Linux 需要 WebKitGTK 4.1）。

```sh
cargo test --workspace
cargo build -p termbridge          # 仅构建命令行工具

cd apps/desktop
npm ci
npm run build:windows              # 或 build:linux、build:macos
```

macOS 构建的是 Universal 包，需要先安装两个编译目标：

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
```

打包前会由 `scripts/prepare_sidecar.py` 编译命令行工具并放入 Tauri 的 sidecar 目录。安装包输出到 `apps/desktop/src-tauri/target/release/bundle/`，macOS 输出到 `apps/desktop/src-tauri/target/universal-apple-darwin/release/bundle/`。

### 发布

将 `Cargo.toml`、`apps/desktop/package.json`、`apps/desktop/src-tauri/tauri.conf.json`、`apps/desktop/src-tauri/Cargo.toml` 中的版本号更新为一致后，推送对应的标签（如 `v0.2.1`）。CI（`.github/workflows/build.yml`）在三个平台构建成功后自动创建 Release 并上传安装包；标签与版本号不一致时不会发布。

## 项目结构

```
crates/protocol   消息格式（JSON Lines，经 SSH 子系统 termbridge-v2 传输）
crates/host       终端进程（portable-pty）、会话管理、输出缓冲与重放、画面快照（vt100）、控制权、输入偏移
crates/app        SSH 服务端与客户端、配置与认证、termbridge 命令行
apps/desktop      桌面端（Tauri 2、TypeScript、xterm.js）
scripts           构建脚本
```

## 开发状态

自动化测试在 GitHub Actions 的 Windows、Linux、macOS 上通过，覆盖按键收发、断线续传、控制权接管、大量输出时的输入、Ctrl+C 中断和命令行 raw 模式附着等场景。

尚未完成的验证：

- 0.2 桌面端和命令行的人工验收（输入法、剪贴板、快捷键、全屏程序等）
- Linux 与 macOS 之间的实机互连
- Windows 用户注销后终端进程的清理

已知限制：

- 不支持断线自动重连
- 重新附着时滚动历史仅恢复纯文本，不含颜色
- 不提供中继和 NAT 穿透
- 接收端重启后会话不保留
- Windows 上命令行退出附着后，可能会吞掉一个按键

## 许可证

MIT
