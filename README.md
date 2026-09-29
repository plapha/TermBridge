# TermBridge

TermBridge 用来在自己的几台电脑之间开远程终端。终端跑在被连的那台机器上，连接断了它也不会停，下次连上还能接着看。装了 TermBridge 的机器既可以去连别人，也可以开放给别人连。

连接用的是 TermBridge 自带的 SSH 服务（基于 [russh](https://github.com/Eugeny/russh)），不需要系统装 OpenSSH，也不依赖 RustDesk 之类的软件。它没有中继，也不做 NAT 穿透，两台机器之间要能直接访问，比如在同一个局域网里，或者都在 Tailscale 上。

支持 Windows、Linux 和 macOS。桌面版是 Tauri 做的图形界面，里面附带同版本的 `termbridge` 命令行工具；没有桌面的服务器可以只用命令行工具。

现在是 0.2.0，还在开发中。哪些测过、哪些还没测，写在最后的[现状](#现状)里。

## 用途

典型的用法是在远端开几个 shell，跑构建、跑长任务、看日志，或者跑 Claude Code、Codex 这类交互程序，中途断开，换一台电脑再接上。

从 0.2.0 开始，按键是实时发过去的，Ctrl+C、Esc、方向键、Tab 都会直接送到远端，所以 vim、htop、less 这些全屏程序也能正常用。0.1.x 那种输完一整行再提交的方式已经去掉了。

TermBridge 不是通用的 SSH 工具：它连不了普通的 sshd，`ssh` 命令也连不上 TermBridge 的接收端。

## 下载

在 [Releases](https://github.com/plapha/TermBridge/releases) 下载，都是 64 位：

- Windows 10/11：`TermBridge_<版本>_x64-setup.exe`
- Debian / Ubuntu：`TermBridge_<版本>_amd64.deb`
- macOS：`TermBridge_<版本>_universal.dmg`，Apple Silicon 和 Intel 都能用

0.2 和 0.1.x 的协议不兼容，两边都得装 0.2。

安装包没有签名。Windows 上被 SmartScreen 拦下时，点「更多信息」，再点「仍要运行」。macOS 第一次打开要右键点应用选「打开」；如果提示应用「已损坏」，运行：

```sh
xattr -dr com.apple.quarantine /Applications/TermBridge.app
```

## 几个说法

被连的一方叫接收端，发起连接的一方叫连接端。接收端默认是关着的，要先初始化、指定监听地址，才会开始接受连接。连接端可以把地址、端口、用户名和认证方式存成一个连接档案（profile），以后按名字连。

接收端上的每个终端叫一个会话，有自己的 UUID。Windows 上开的是 PowerShell，其他系统开用户的默认 shell。一个连接可以同时开多个会话。

同一个会话可以有好几个客户端同时看，但只有一个能输入，这就是控制权。先连上的拿到控制权，其他人想输入要先「接管」。

主机指纹是接收端 SSH 主机密钥的 SHA256 指纹。第一次连接时要你自己核对，确认后会记下来，以后对不上就拒绝连接。

## 快速上手

下面假设从 B 机器连到 A 机器。

### 在 A 上开接收端

用 GUI 的话，在右侧「本应用接收端」面板里初始化，填好监听地址，然后启用。

用命令行：

```sh
# 两种初始化方式选一种
termbridge host init --authorized-keys ~/.ssh/authorized_keys   # 用现有的 SSH 公钥登录（推荐）
termbridge host init                                            # 设一个 TermBridge 专用密码，至少 12 个字符

termbridge host status                             # 查看指纹、登录用户名和认证方式
termbridge host enable --listen 100.64.0.5:22333   # 填本机的 Tailscale 或局域网 IP
termbridge host run                                # 前台运行，Ctrl+C 停止，本机所有会话会一起结束
```

登录用户名就是初始化时的系统用户名，`host status` 里能看到。监听地址必须写明，只在本机测试的话用 `127.0.0.1:22333`，不可信的网络上别用 `0.0.0.0`。

如果 `authorized_keys` 里有带 `from=`、`command=` 这类限制选项的行，导入会直接报错，免得原来的访问限制被悄悄去掉。

改了认证配置，要重启接收端才生效。

### 在 B 上连 A

用 GUI 的话，新建一个连接，填地址、端口、用户名和认证方式。第一次连会显示 A 的指纹，和 A 上 `host status` 显示的对一下，一样再确认。之后就可以新建会话，在标签页里直接打字。

用命令行：

```sh
termbridge profile add a-box 100.64.0.5 --port 22333 -u <A 上的用户名> --auth key
termbridge session create -p a-box --title build
termbridge session list   -p a-box
termbridge session attach -p a-box --session-id <UUID>
```

`--auth key` 表示用 SSH 私钥，默认依次找 `~/.ssh/id_ed25519`、`id_ecdsa`、`id_rsa`，也可以用 `--key-path` 指定。不加 `--auth` 就是用 TermBridge 专用密码，连接时输入；加上 `--remember-password` 会存进系统凭据库。

第一次连接时命令行会显示指纹，输入 `yes` 才会记住。`session attach` 不带 `--session-id` 时，会进入第一个还在运行的会话。

`session attach` 会把本地终端切到 raw 模式，按键原样发给远端，退出时恢复原来的终端设置，出错退出也一样。所以它只能在真正的终端里用，不能接管道。本地操作都是先按 Ctrl+]，再按一个键：

- `d`：断开，远端终端继续运行
- `t`：接管控制权
- `e`：结束远端终端，3 秒内再按一次确认
- 再按一次 Ctrl+]：给远端发一个 Ctrl+]

没人控制的会话可以直接结束：

```sh
termbridge session end -p a-box --session-id <UUID>
```

如果别的客户端正控制着这个会话，要加 `--take-control`。

## 断开、退出和重启

客户端断开、退出或者网络断了，远端终端都照常运行。重新连上时会先收到一份当前画面，再接着收新的输出。现在断线后不会自动重连，得自己重新 attach，自动重连在开发计划里。

在终端里执行 `exit`，或者程序自己结束，会话会马上从列表里去掉，占用的资源和滚动历史也一起释放，连着的客户端会收到会话结束的通知。

接收端停下时，上面的会话会全部结束，不管是按了 Ctrl+C、在 GUI 里点了「停止接收」，还是从托盘退出。Windows 上，终端里启动的子进程会通过 Job Object 一起结束，接收端进程意外崩溃时也是这样。Linux 和 macOS 目前只结束 shell 本身，shell 放到后台的进程可能会留下来。

会话只存在内存里。接收端或者机器重启后，之前的会话就没了，也不会重建，拿旧的 UUID 去连会直接报错。

## 安全

认证推荐用现有的 SSH 公钥。用专用密码的话，接收端只存它的 Argon2 哈希。TermBridge 不读取、也不校验系统账户的密码。同一个 IP 连续失败 5 次，之后 10 分钟内会拒绝它的连接。

指纹确认过一次就固定下来，之后一变就拒绝连接，不会自动信任新指纹。这里的主机密钥是 TermBridge 自己的，跟系统 OpenSSH 的主机密钥没有关系。

输入按字节偏移编号，接收端确认收到后，客户端才把它从缓冲里删掉。重复的字节会被丢弃，中间有缺口时客户端从确认点重发。如果因为断线之类的原因没法确定对方收到了多少，客户端会丢掉没确认的按键并提示你，而不是盲目重发，免得同一条命令被执行两次。

没有控制权的客户端，按键不会写进终端，客户端会提示先接管。

密码和 GUI 记住的指纹优先存进系统凭据库（Windows 凭据管理器、macOS 钥匙串、Linux Secret Service）。凭据库用不了时，指纹存到权限受限的 `known_hosts.json`，密码则每次连接时现问。私钥只从原来的位置读，不会复制，私钥口令也不保存。日志里不记密码，也不记终端的原始输出。

配置目录在 Windows 上是 `%LOCALAPPDATA%\TermBridge\`，Linux 和 macOS 上是 `$XDG_CONFIG_HOME/termbridge/`（默认 `~/.config/termbridge/`）。`host.json` 和 `host_key` 是接收端用的，`profiles.json` 是连接端用的。

## 在没有桌面的 Linux 上常驻

只需要命令行工具，从源码编译：

```sh
cargo build -p termbridge --release     # 生成 target/release/termbridge
```

然后用 systemd 用户服务跑起来。创建 `~/.config/systemd/user/termbridge.service`：

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
loginctl enable-linger $USER    # 用户没登录时也保持运行
```

服务重启后是一个新的接收端，之前的会话不会恢复。

## GUI

关掉窗口只是缩到托盘，点托盘图标能找回来。要真正退出，用托盘菜单里的「退出 TermBridge」，这会停掉本机接收端，并结束上面的所有会话。

GUI 不会开机自启，启动时也不会自动打开接收端。机器重启后要重新打开 GUI 再启用接收。需要无人值守的话，用命令行加系统服务来跑，Linux 的做法见上一节。

## 从源码构建

需要 Rust stable、Node.js 22（和 CI 一致）、Python 3，以及 [Tauri 2 的系统依赖](https://v2.tauri.app/start/prerequisites/)。Linux 还要 WebKitGTK 4.1。

```sh
cargo test --workspace            # 协议、会话管理、本机两端互连测试
cargo build -p termbridge         # 只编译命令行工具

cd apps/desktop
npm ci
npm run build:windows             # 或者 build:linux（deb）、build:macos（dmg）
```

macOS 打的是 Intel 和 Apple Silicon 的通用包，要先执行 `rustup target add aarch64-apple-darwin x86_64-apple-darwin`。

打包时会先用 `scripts/prepare_sidecar.py` 编译命令行工具，放进 Tauri 的 sidecar 目录，然后再打包。安装包在 `apps/desktop/src-tauri/target/release/bundle/` 下，macOS 在 `target/universal-apple-darwin/release/bundle/` 下。`.github/workflows/build.yml` 会在三个平台上跑测试并打包。

发新版本时，先把 `Cargo.toml`、`apps/desktop/package.json`、`apps/desktop/src-tauri/tauri.conf.json` 和 `apps/desktop/src-tauri/Cargo.toml` 里的版本号改成一样，再推一个同名标签，比如 `v0.2.1`。三个平台都构建成功后，CI 会自动建 Release 并上传安装包。标签和版本号对不上时不会发布。

## 代码结构

```
crates/protocol   连接端和接收端之间的消息格式，每行一个 JSON，走 SSH subsystem termbridge-v2
crates/host       终端进程（portable-pty）、会话表、输出环和重放、画面快照（vt100）、控制权、输入偏移
crates/app        SSH 接收端和连接端、配置和认证、termbridge 命令行
apps/desktop      Tauri 2 图形界面，TypeScript + xterm.js
scripts           打包用的脚本
```

## 现状

自动化测试在 GitHub Actions 的 Windows、Linux、macOS 上都能通过，覆盖本机两端互连下的按键收发、断开后续传、观察者接管、刷屏时按键不卡、Ctrl+C 中断、命令行 raw 模式 attach 等。

还没验证的：0.2 的 GUI 和命令行还没在真实桌面上人工验收过，包括 PowerShell、Claude Code、Codex、vim、中文输入法、剪贴板和快捷键；Linux 和 macOS 之间还没实机互连过；Windows 用户注销后终端是不是真的会被结束，也还没确认。

已知的问题和限制：

- 断线后不会自动重连
- 重新 attach 时，滚动历史只按纯文本恢复，没有颜色
- 没有中继，也不做 NAT 穿透
- 接收端重启后会话不保留
- Linux 和 macOS 上结束会话时，不会结束后台子进程
- Windows 上用命令行退出 attach 后，可能会多吞掉一个按键

## 许可证

MIT
