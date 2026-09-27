# TermBridge 桌面端

Tauri v2 + Vite + TypeScript GUI；与同一套 Rust CLI/接收端共享 `crates/app`、`crates/host`、`crates/protocol`。只读终端画面不会转发键盘数据；编辑框显式“发送”才提交。首次指纹确认、可复用现有 SSH 公私钥（含加密私钥口令）、已有会话附着及 GUI 接收端托盘模式均由真实后端命令处理。窗口关闭或顶栏按钮会隐藏到托盘，必须从托盘菜单明确退出。源代码根目录的 README 记录运行方式、边界及当前验收进度。

在本目录运行 `npm ci && npm run build` 仅构建前端；`npm run build:windows` 会先构建 CLI sidecar，再构建 Windows 安装包。Linux 换用 `npm run build:linux`，macOS 换用 `npm run build:macos`。请勿把 `npm run build` 的前端静态输出误认为完整安装包。
