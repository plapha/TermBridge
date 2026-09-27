# TermBridge — 双向远程终端（开发版）

这是独立于 WezTerm/RustDesk 的新项目。安装本产品的两端可通过**可达的 IP/主机名**通信；普通局域网或 Tailscale 网络均可，程序不调用 RustDesk，也不需要系统 OpenSSH 服务。首版没有中继和 NAT 穿透，不兼容通用 SSH 服务器。桌面包包含 GUI 与 `termbridge` CLI sidecar；无桌面的 Linux VPS 可单独安装 CLI。接收端默认关闭，必须先初始化并明确选择监听地址。

## 已实现

- Rust `termbridge-protocol`、`termbridge-host`、`termbridge`：SSH 主机密钥、可选的 Argon2 产品密码或**只用现有 SSH 授权公钥**、首次指纹确认与后续锁定、错误认证限速、会话 ID 和 PTY、画面快照与序号、单控制器多观察者、命令 UUID 去重、显式发送、分离/结束。
- Windows 使用 kill-on-close Job Object；用户注销、进程意外终止时，接收进程持有的 Job 句柄关闭会杀掉它启动的终端进程。Windows ConPTY 的光标报告延迟到首次“发送”后处理。
- CLI 提供 host/profile/session 操作；Tauri GUI 提供连接配置、首次指纹确认、密码/密钥、会话标签、只读终端与接收状态。窗口关闭时一律隐藏到托盘，也可点击顶栏“隐藏到托盘”；托盘图标可找回窗口，菜单可明确停止本 GUI 接收端并退出。
- Tauri sidecar 与三平台构建工作流。桌面包内 CLI 由 `scripts/prepare_sidecar.py` 在打包前为当前目标平台构建。

## 开发与运行

依赖 Rust、Node.js、各平台 Tauri 构建依赖（Linux 另需 WebKitGTK 4.1）。

```text
cargo test --workspace
cargo build -p termbridge
cd apps/desktop
npm ci
npm run build
npm run build:windows                 # Windows；Linux 用 build:linux，macOS 用 build:macos
```

打包脚本先复制当前平台的 CLI 到 Tauri sidecar 目录；构建产物在 `apps/desktop/src-tauri/target/release/bundle/`。Linux VPS 不需构建 GUI：`cargo build -p termbridge --release` 即可得到 CLI。

### 接收端（设备 A）

```text
termbridge host init --authorized-keys <本机 SSH authorized_keys 的绝对路径> # 推荐：只用现有公钥，无额外产品密码
# 或：termbridge host init                     # 旧方式：交互设置产品专用密码
termbridge host status                         # 核对指纹、密码是否启用、授权密钥数
termbridge host enable --listen 100.x.y.z:22333 # 显式选择 Tailscale IP；也可用 LAN IP
termbridge host run                            # 前台运行，Ctrl+C 结束全部远端终端
```

仅本机测试可用 `127.0.0.1:22333`；不要在不可信网络上无意使用 `0.0.0.0:22333`。GUI 初始化时可选“复用现有 SSH 授权公钥”，路径留空则读取当前用户的 `~/.ssh/authorized_keys`；CLI 须显式提供 `--authorized-keys` 文件路径。带 `from=`、`command=` 等 SSH 选项限制的条目会被拒绝，避免静默放宽访问限制。初始化后 `host key-add --key-file <公钥文件>` 可增添单把公钥；已经用产品密码初始化的设备，可先停接收，再执行 `termbridge host use-ssh-keys --authorized-keys <文件>`（GUI 也有“改为仅 SSH 密钥”入口），导入成功后原产品密码认证会被关闭。已经运行的接收进程（包括单独启动的 CLI 服务）需重启才能加载新认证配置。CLI 的 `host enable --disable` 只保存配置；已运行的前台进程仍需停止。GUI 中“停止接收”会向本 GUI 启动的接收任务发送停止信号，结束其终端进程。

Linux 无桌面 VPS 可以为当前用户创建 `~/.config/systemd/user/termbridge.service`，设置 `ExecStart=/绝对路径/termbridge host run`、`Restart=on-failure`、`WantedBy=default.target`，再执行 `systemctl --user enable --now termbridge`。服务与普通 CLI 使用同一用户配置；重启后只会启动新的接收端，旧会话不会复活。用户级服务的存活范围取决于系统是否启用 linger。

### 连接端（设备 B）

```text
termbridge profile add my-vps 100.x.y.z --port 22333 --user <对端接收用户> --auth key
# 若本机已有 ~/.ssh/id_ed25519、id_ecdsa 或 id_rsa，私钥路径可自动查找；也可加 --key-path <绝对路径>
# 对端仍用产品密码时，省略 --auth key 会在连接时提示输入该产品密码
termbridge session list -p my-vps
termbridge session create -p my-vps --title work
termbridge session attach -p my-vps --session-id <会话 UUID>
```

首次连接需核对并输入 `yes` 才会记录**本产品接收端**的主机指纹（不同于系统 OpenSSH 服务的主机密钥）；指纹变化会拒绝连接。加密 SSH 私钥会在连接时提示输入本次口令，不保存口令也不复制私钥。**系统 SSH 账户密码不会被直接读取或验证**：本产品不依赖系统 OpenSSH 服务，不能把它的密码认证当作内置接收端认证；若不使用 SSH 密钥，可继续选择产品专用密码。CLI 本地整行编辑，按 Enter 后才发送；在 `session attach` 中使用 `:take` 明确接管输入、`:detach` 分离、`:end` 结束远端进程。独立启动一个新 CLI 进程无法替其他客户端“分离”，因此不提供无效的 `session detach` 子命令。`session end -p my-vps --session-id <UUID>` 对无人控制的会话可直接结束；已有控制器时需明确加 `--take-control`。GUI 终端画面从不直连键盘，只有点击“发送”按钮才传送文本。首版拒绝换行和控制字符，不支持 vim 等全屏交互程序。

## 边界及尚需验收

- Windows 本机已编译并验证协议/会话测试以及接收—连接—发送—分离—重附着—画面恢复；**没有执行真实用户注销测试**。
- Linux/macOS 的源码与 CI 构建流程已配置，但尚未在这两种系统上运行互连测试，不能将 CI 文件等同于通过验收。
- GUI 虽已通过前端及 Rust 编译，首次连接、托盘、GUI/CLI 跨端会话操作尚需真人实际操作验收。
- 不做公网中继、NAT 穿透、旧配置迁移、旧会话恢复；设备重启后所有旧进程失效。接收端会话只存内存，重启后 `list` 不包含旧 ID，重新附着会明确报错，不会自动创建替代终端。
- 密码放入系统凭据库是可选的；Linux 若未提供桌面 Secret Service，保存密码可能失败，本次连接仍继续、以后可手动输入。GUI 的主机指纹优先保存至凭据库；凭据库不可用时回退到权限受限的 `known_hosts.json`。认证密码只以 Argon2 哈希存于接收端配置，主机私钥只留在接收用户配置目录。
