# 复核点 A 修复报告

日期：2026-09-29。分支：`v2/review-a`（基于 `v2/m3-cli-raw`）。只在本地提交，未推送，未做 M4 重连逻辑。

## 1. 逐项核实、修复与测试

| 编号 | 核实与改动 | 对应自动化测试 / 验证 | 状态 |
|---|---|---|---|
| A4 | 原 lib 单元测试用路径推断 CLI exe、锁中 panic 可毒化后续测试；原 Unix 运行标记会匹配回显；Windows 会继承忽略 Ctrl+C 的进程属性。CLI 测试移至 integration target 并使用 Cargo bin 路径；锁容忍毒化；修改运行标记；在创建 shell 前清除继承的 Ctrl+C 忽略属性，并把 M6 worker 注意事项写入计划。 | `cli_raw_attach_round_trip`、`local_two_ends_raw_input_resume_and_key_auth`、`ctrl_c_interrupts`（Windows 本机）。非 Windows 分支待 CI。 | 已修；跨平台待验证 |
| A1 | `Handler::data` 原来在回调里 `await handle.data`；改为直接 `session.data`，独立转发任务仍使用 Handle。 | `input_ack_survives_sustained_output`（持续输出时 50 个 ACK 和 List 均限时返回）。 | 已修 |
| A2 | host 原先先判控制者且拒绝不推进；CLI/GUI 原偏移与拒绝不同步、GUI 还淘汰旧未确认字节。改为重复/缺口优先、观察者消费偏移但绝不写 PTY；已知流 Attach 取 max；共享有界 `InputBuffer` 实现 ACK、busy/gap 重发与去重、未知状态丢弃对齐；CLI 已知观察者不发送；同步修订计划 3.1。 | `observer_consumes_offset_without_writing_then_can_take_control`、`gap_precedes_not_controller`、`input_stream_table_evicts_oldest_observer`、`observer_input_then_take_control_keeps_offset_usable`、`busy_rewinds_and_ack_drops_prefix`、`gap_deduplicates_and_unknown_range_reattaches`、`full_buffer_rejects_without_dropping_or_advancing`。 | 已修；GUI 操作待验 |
| A3 | 捕获阶段监听确实 `stopPropagation`；删除该监听，非 Windows 只在 xterm 的 key handler 里取消浏览器默认动作并继续交给 xterm；Windows 保持原生 WebView2 设置。 | `npm --prefix apps/desktop run build`；真实快捷键见人工矩阵。 | 已修；GUI 待验 |
| B1 | 原尺寸抖动睡前记录旧尺寸并在睡后覆盖新 Resize；增加尺寸代数，同锁串行化 PTY 与 parser 更新，发现真实 Resize 后不恢复旧值。 | `resize_during_jiggle_preserves_controller_dimensions`。 | 已修 |
| B2 | 原 16 ms 是每次接收重新计时；改为首字节绝对截止时间与 256 KiB 上限。 | `continuous_small_chunks_flush_from_first_byte_deadline`（桌面 Rust 单测）。 | 已修；流式 GUI 观感待验 |
| B3 | Windows 原 stdin 使用 `Read` 且零长视为 EOF；改用无唤醒掩码的 `ReadConsoleW`，跨读取流式解码 UTF-16 代理对。 | `utf16_stream_handles_split_surrogates_and_ctrl_z`；真实按键待人工验。 | 已修；真实控制台待验 |
| B4 | 原在线挂接仅以 stream 为键，旧 Worker 会清理新连接；host 挂接引入令牌，detach 只接受匹配令牌；Worker 同会话重挂先解旧流。 | `stale_attachment_token_cannot_detach_replacement`、`replacement_connection_and_stream_survive_old_worker_cleanup`、`controller_grace_period`。 | 已修 |
| C1 | 前端原每次 onInput 独立 invoke；改为每会话有界单队列，串行 await invoke、每批至多 16 KiB；失败不盲重发。 | `npm --prefix apps/desktop run build`；真实快速按键/粘贴顺序待人工验。 | 已修；GUI 待验 |
| C2 | 原请求期间事件缓冲溢出会静默 pop_front；现溢出置标志，下一次 `recv_event` 明确返回错误并清除旧缓冲，CLI/GUI 重新挂接。 | `overflow_is_reported_before_any_remaining_event`、app lib/CLI 集成测试。 | 已修 |
| C3 | 原任意 CSI q 都会保存；现只记录数字参数且中间字节恰为一个空格的 DECSCUSR。 | `xtversion_query_is_not_recorded_as_cursor_style`、`snapshot_marks_alternate_screen_and_modes`。 | 已修 |
| D1 | Linux Ctrl+V 原被当粘贴；现在 Linux 只用 Ctrl+Shift+V，Windows/macOS 保持各自组合键。 | 前端 build；Linux 实机待验。 | 已修；手工待验 |
| D2 | 原 CPR 读取整块解析后的光标；按查询完成的位置分段喂 parser 后应答。 | `cursor_query_replies_at_query_position_within_chunk`。 | 已修 |
| D3 | 原 stdout 写错误被忽略、Windows 控制台可能拒绝非法 UTF-8；控制台输出改为有界尾部流式 UTF-8 解码，非法字节替换，错误向调用者传播；重定向 stdout 保持原字节。 | `utf8_stdout_decoder_preserves_split_tail_and_replaces_invalid`、`cli_raw_attach_round_trip`。 | 已修；真实非法字节终端待验 |
| D4 | 原没有 Ctrl+Break / 关闭窗口恢复处理；Windows 安装退出控制回调恢复模式后交给系统默认处理。 | `console_exit_handler_only_targets_break_and_close`；实际关窗待验。 | 已修；真实控制台待验 |
| D5 | 原 `iter().skip()` 顺序跳过；改 `VecDeque::range()`。 | `output_log_range_reads_wrapped_window`。 | 已修 |
| D6 | CLI 退出后独立 stdin 线程可能再吃一个按键；任务书允许作为已知问题，不改 stdin 线程生命周期。 | 无。 | 未修（已知问题） |

## 2. 偏离与核实范围

- A4 开工时工作区已有未提交的 A4 草稿和未跟踪的 `.review-a-git/`。先按要求单独提交任务书，再核对、修正并提交 A4；未跟踪目录保持原状，未纳入任何提交。
- 原复核所列旧 CI run 状态已读证：M1 的 macOS job、M3 的 Windows job 为失败；本轮未推送，**新分支 CI：待推送**。旧 CI 的具体失败根因按原代码路径核对，本机只验证了 Windows 修复后行为，不能推断 macOS/Linux 已通过。
- C2 GUI 后端拿不到“前端确已显示”的精确偏移；发生事件溢出时用 `resume_from: None` 请求完整快照，而非猜测偏移，避免跳过未显示输出。CLI 则用已显示偏移重挂。这是对任务书“按 resume_from”建议的安全性偏离。
- B1 测试 mock 在 Unix 使用与 `pid_t` 等价的 `i32` 签名，避免为测试新增 host 的直接 `libc` 依赖。
- D3 在 Windows **控制台**把无效 UTF-8 替换为 U+FFFD；stdout 重定向时继续写原字节，避免改变管道语义。
- B4 令牌只约束挂接清理，协议输入本身仍按 stream_id 鉴权；同一 stream_id 的旧连接在尚未关闭时主动发 Input 的权限隔离不在本任务范围，后续设计需复核。

## 3. 本机命令真实输出摘要

最终代码提交后连续执行三次 `cargo test --workspace`，每次退出码都是 **0**；每次输出摘要均为：app lib `16 passed; 0 failed`，CLI integration `1 passed; 0 failed`，host `16 passed; 0 failed`，protocol `6 passed; 0 failed`，其余 bin/doc test `0 passed; 0 failed`。三次均提示 `tests/common/mod.rs` 的 `next_offset` 未使用（警告，不是失败）。

`cargo check --manifest-path apps/desktop/src-tauri/Cargo.toml`：退出码 **0**，`Finished dev profile`；警告 `SessionState::Connecting` 未使用。

`npm --prefix apps/desktop run build`：退出码 **0**，`tsc --noEmit && vite build`，`17 modules transformed`，`built in 557ms`。这只是本机 Windows 检查/构建，不代表三平台 CI。

**CI：待推送**（遵照负责人本次要求，不执行 `git push`；没有新分支 run 链接）。

## 4. 人工验收矩阵

以下真实桌面操作均**未验证**，交负责人在 Windows GUI/CLI 和 Linux/macOS 实机执行；自动化测试或编译不能替代人工结论。

| 场景 | 状态 / 需观察 |
|---|---|
| PowerShell | 未验证：方向键历史、Tab、Ctrl+C 中断持续命令、Esc、Ctrl+R。 |
| Claude Code | 未验证：输入、斜杠菜单、Esc、Ctrl+C、多行粘贴、流式输出。 |
| Codex CLI | 未验证：同上。 |
| vim | 未验证：插入、退出、方向键、缩放重绘。 |
| 窗口缩放 | 未验证：提示符/全屏程序正确重排。 |
| 断开后重挂运行中的 vim | 未验证：快照完整恢复。 |
| 两个客户端 | 未验证：观察者按键提示、接管后正常输入和尺寸。 |
| 快捷键 | 未验证：F5 / Ctrl+R / Ctrl+F / Ctrl+W 不触发浏览器且送达终端。 |
| 中文输入法 | 未验证：组字、上屏、中文/emoji 宽度。 |
| 剪贴板 | 未验证：选中复制、无选区 ^C、Ctrl+Shift+V / Ctrl+V 粘贴；Linux vim Ctrl+V 列选择。 |
| 刷屏时 Ctrl+C | 未验证：前台 GUI 连接不断、按键立即生效。 |
| Windows CLI Ctrl+Z、Ctrl+Break、关窗 | 未验证：按键到达远端、终端模式恢复。 |

## 5. 待复核与已知风险

- 负责人推送后需核对 Windows/Linux/macOS 的测试和桌面构建；目前不能声称三平台全绿。
- D6 保留为已知问题。A3/B2/C1 及 D1/D3/D4 的真实桌面行为仍待人工验收。
- C2 的 GUI 溢出恢复会使用完整快照（较重但安全）；M4 的跨连接自动重连逻辑未实施。