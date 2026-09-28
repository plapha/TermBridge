# TermBridge

在两台电脑之间开远程终端。每台装了 TermBridge 的设备既可以连别人，也可以被别人连。终端跑在被连的那台机器上，客户端断开后终端照常运行，重新连上时能看到当前画面。

- 只要对方的 IP 或主机名能访问就能用，局域网和 Tailscale 都可以。没有中继，也不做 NAT 穿透。
- 接收端自带 SSH 服务（基于 [russh](https://github.com/Eugeny/russh)），不需要系统里的 OpenSSH，也不依赖 RustDesk 之类的软件。
- 支持 Windows、Linux、macOS。桌面版是一个 Tauri 图形界面，里面带着同版本的 `termbridge` 命令行工具。没有桌面环境的服务器可以只装命令行工具。

> 当前版本 0.1.1，开发阶段。已经验证了什么、还没验证什么，见文末[现状](#现状)。

## 它适合做什么，不适合做什么

适合在远端机器上开几个 shell，跑命令、看输出、中途断开、换一台设备接着看，比如跑构建、跑长任务、查日志。

不适合交互式全屏程序。输入是**整行提交**的：在输入框里写好一行，点「发送」（命令行模式下按 Enter）才会送到远端。按键不会实时转发，所以 vim、top、less 这类程序用不了。这样设计是为了避免误触和重复执行（见[安全设计](#安全设计)）。

它也不是通用 SSH 客户端或服务端：不能用 TermBridge 连普通的 sshd，也不能用 `ssh` 命令连 TermBridge 的接收端。

## 基本概念

| 名称 | 含义 |
|---|---|
| 接收端 | 被连接的一方。它在指定地址上监听，负责创建和保管终端。默认关闭，需要手动初始化并选择监听地址。 |
| 连接端 | 发起连接的一方。可以用 GUI，也可以用 `termbridge profile` / `session` 命令。 |
| 连接档案（profile） | 连接端保存的一条连接配置：地址、端口、用户名、认证方式。 |
| 会话 | 接收端上的一个终端（Windows 上是 PowerShell，其他系统是用户的默认 shell）。每个会话有一个 UUID。一个连接可以同时开多个会话。 |
| 控制权 | 同一个会话可以被多个客户端同时观看，但只有一个客户端能输入。第一个接入的客户端拿到控制权，其他客户端需要明确「接管」才能输入。 |
| 主机指纹 | 接收端 SSH 主机密钥的 SHA256 指纹。第一次连接时由你核对确认，之后只要不一致就拒绝连接。 |

## 快速上手

下面以 A 机器被连、B 机器去连为例。两台机器都可以用 GUI 或命令行完成。

### 1. 在 A 上开启接收端

**GUI**：打开 TermBridge，在右侧「本应用接收端」面板里初始化，填写监听地址，然后启用。

**命令行**：

```sh
# 初始化，二选一：
termbridge host init --authorized-keys ~/.ssh/authorized_keys   # 推荐：复用现有的 SSH 公钥，不用另设密码
termbridge host init                                            # 或者：设置一个 TermBridge 专用密码（至少 12 个字符）

termbridge host status                          # 查看主机指纹、登录用户名、认证方式
termbridge host enable --listen 100.64.0.5:22333   # 选择监听地址，例如本机的 Tailscale IP 或局域网 IP
termbridge host run                             # 在前台运行；按 Ctrl+C 停止，同时结束这台机器上的所有会话
```

注意：

- 登录用户名是初始化时当前系统用户的名字，可以用 `host status` 查看。
- 监听地址需要明确指定。只在本机测试时用 `127.0.0.1:22333`。在不可信的网络上不要用 `0.0.0.0`。
- `--authorized-keys` 会拒绝带 `from=`、`command=` 等限制选项的条目，免得悄悄放宽原本的访问限制。
- 修改认证配置后，要重启正在运行的接收端才会生效。

### 2. 在 B 上连接 A

**GUI**：新建连接，填写地址、端口、用户名和认证方式。第一次连接会显示 A 的主机指纹，请和 A 上 `host status` 显示的指纹核对，一致再确认。之后可以新建会话、打开标签页，在输入框里写命令并点「发送」。

**命令行**：

```sh
termbridge profile add a-box 100.64.0.5 --port 22333 -u <A 上的用户名> --auth key
#   --auth key：使用 SSH 私钥。默认自动查找 ~/.ssh/id_ed25519、id_ecdsa、id_rsa，也可以用 --key-path 指定
#   不加 --auth：使用 TermBridge 专用密码，连接时输入；加 --remember-password 会保存到系统凭据库

termbridge session create -p a-box --title build
termbridge session list   -p a-box
termbridge session attach -p a-box --session-id <UUID>   # 不写 --session-id 会进入第一个还在运行的会话
```

第一次连接时，命令行会显示指纹，输入 `yes` 才会记住它。

进入 `session attach` 之后，每输入一行按 Enter 发送。下面几条是内置命令：

| 输入 | 作用 |
|---|---|
| `:take` | 接管控制权 |
| `:detach` | 断开当前客户端，远端终端继续运行 |
| `:end` | 结束远端终端 |

没有人控制的会话，可以直接用 `termbridge session end -p a-box --session-id <UUID>` 结束。如果其他客户端正在控制这个会话，需要加 `--take-control`。

## 会话的生命周期

- **断开不影响终端**：客户端断开、退出或网络中断时，远端终端照常运行。重新连接后，会先收到一份当前画面，再接着收新的输出。
- **终端自己退出时会被清理**：在终端里执行 `exit`，或者程序正常结束，会话会马上从列表里移除，占用的终端资源和滚动历史也会释放。已连接的客户端会收到「会话结束」的通知。
- **接收端停止时，所有会话一起结束**：包括按 Ctrl+C、在 GUI 里「停止接收」、从托盘退出。在 Windows 上，终端里启动的子进程会通过 Job Object 一起被结束；即使接收进程意外退出也是如此。Linux 和 macOS 上目前只会结束 shell 本身，shell 在后台启动的进程可能会留下来。
- **会话只保存在内存里**：接收端或机器重启后，原来的会话都没有了，也不会自动重建。用旧的 UUID 重新接入会直接报错。

## 安全设计

- **认证**：推荐使用现有的 SSH 公钥。也可以用 TermBridge 专用密码，接收端只保存它的 Argon2 哈希。TermBridge 不读取、也不校验系统账户的登录密码。同一个 IP 连续失败 5 次后，10 分钟内会被拒绝连接。
- **主机指纹锁定**：第一次连接时由你手动确认指纹，之后指纹一变就拒绝连接，不会自动信任新指纹。这里的主机密钥是 TermBridge 自己的，和系统 OpenSSH 的主机密钥不是同一个。
- **不会重复执行**：每次发送都带一个唯一 ID，接收端会拒绝重复的 ID。如果一次发送的结果不确定（超时或断线），客户端会报错，**不会自动重发**。
- **输入需要明确提交**：GUI 的终端画面是只读的，键盘输入不会直接进入远端，只有点「发送」才会提交一行。首版不允许发送换行和控制字符。
- **本地保存的内容**：密码和 GUI 记住的指纹优先存进系统凭据库（Windows 凭据管理器、macOS 钥匙串、Linux Secret Service）。凭据库不可用时，指纹会存到权限受限的 `known_hosts.json`，而密码会在每次连接时询问。私钥只从原来的位置读取，不会被复制；私钥的口令不会保存。日志里不会记录密码，也不会记录终端的原始输出。

配置目录：Windows 是 `%LOCALAPPDATA%\TermBridge\`，Linux 和 macOS 是 `$XDG_CONFIG_HOME/termbridge/`（默认 `~/.config/termbridge/`）。其中 `host.json` 和 `host_key` 属于接收端，`profiles.json` 属于连接端。

## 在无桌面 Linux 上长期运行

只需要命令行工具：

```sh
cargo build -p termbridge --release     # 生成 target/release/termbridge
```

用 systemd 用户服务运行。创建 `~/.config/systemd/user/termbridge.service`：

```ini
[Unit]
Description=TermBridge host

[Service]
ExecStart=/绝对路径/termbridge host run
Restart=on-failure

[Install]
WantedBy=default.target
```

```sh
systemctl --user enable --now termbridge
loginctl enable-linger $USER    # 用户没有登录时也保持运行
```

服务重启后会启动一个新的接收端，之前的会话不会恢复。

## GUI 说明

- 关闭窗口不会退出程序，只会隐藏到托盘，点托盘图标可以找回窗口。要彻底退出，请用托盘菜单里的「退出 TermBridge」；退出时会停止本机接收端，并结束它上面的所有会话。
- GUI 不会开机自启，启动时也不会自动开启接收端。机器重启后，需要重新打开 GUI 并启用接收；如果需要无人值守，请参考上一节用命令行加系统服务运行。

## 从源码构建

需要 Rust stable、Node.js 22（与 CI 一致）、Python 3，以及 [Tauri 2 的系统依赖](https://v2.tauri.app/start/prerequisites/)。Linux 还需要 WebKitGTK 4.1。

```sh
cargo test --workspace            # 协议、会话管理、本机两端互连测试
cargo build -p termbridge         # 只构建命令行工具

cd apps/desktop
npm ci
npm run build:windows             # 或 build:linux（deb）、build:macos（dmg）
```

打包脚本会先用 `scripts/prepare_sidecar.py` 为当前平台编译命令行工具，放进 Tauri 的 sidecar 目录，然后打包。安装包输出在 `apps/desktop/src-tauri/target/release/bundle/`。`.github/workflows/build.yml` 会在三个平台上测试并打包。

## 代码结构

```
crates/protocol   连接端与接收端之间的消息格式（每行一个 JSON，通过 SSH subsystem termbridge-v1 传输）
crates/host       终端进程（portable-pty）、会话表、画面快照（vt100）、控制权、防重复发送
crates/app        SSH 接收端和连接端、配置与认证、termbridge 命令行工具
apps/desktop      Tauri 2 图形界面（TypeScript + xterm.js 只读显示）
scripts           打包辅助脚本
```

## 现状

已验证：

- 在 Windows 本机上跑通了从接收端到连接端的完整流程：连接、发送、断开、重新接入、恢复画面，以及结束会话、终端退出后自动清理、拒绝同一连接上的第二个通道。
- GitHub Actions 在 Windows、Linux、macOS 上都能编译通过，单元测试也都通过。

还没有验证：

- Linux 和 macOS 之间的实机互连。
- Windows 用户注销后，终端是否真的会被结束。
- GUI 的人工操作流程，包括首次确认指纹、托盘、GUI 和命令行交替操作同一个会话。

已知限制：不支持全屏交互程序；客户端还不能发送 Ctrl+C（协议里有中断请求，但 GUI 和命令行都没有提供这个入口），卡住的命令只能结束整个会话；没有中继和 NAT 穿透；会话在重启后不保留；Linux 和 macOS 上结束会话时不会结束后台子进程。

## 许可证

MIT
