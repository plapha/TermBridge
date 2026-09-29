# 复核点 A（M1–M3）复核结论与修复任务书

复核对象：`v2/m1-protocol`（cce8d22、9eb796a）、`v2/m2-gui-raw`（4a65514、08f7ab8）、`v2/m3-cli-raw`（181bf07、6b76d5f）。
复核方式：逐文件读代码 + 本机 Windows 实跑 `cargo test --workspace` + 查看已推送分支的 GitHub Actions 结果 + 对照 russh 0.63.3 / Rust std 源码核实关键假设。

## 0. 结论

**不通过，需修复后才能进入 M4。**

整体架构方向正确，大部分核心设计实现得好（见第 4 节），但有 4 个会直接导致「卡死 / 按键永久失效 / 快捷键失效」的阻塞问题，以及报告中「全部测试通过」与实际不符（CI 两个平台失败、本机也失败）。

---

## 1. 测试与 CI：报告结论不成立

报告称 `cargo test --workspace` 全部通过（protocol 6 / host 9 / app 7）。实际：

| 环境 | 结果 | 根因 |
|---|---|---|
| CI m1 分支 macOS（run 36434249118） | `ctrl_c_interrupts` 失败：`shell did not execute after Ctrl+C` | 测试缺陷：非 Windows 的 `running_marker = long_running`（`crates/host/src/lib.rs:1653`），匹配到的是**命令回显**而不是 `sleep` 已在运行的证据。`^C` 在 zsh fork 出 `sleep` 之前到达，于是 `sleep 100` 没被打断。 |
| CI m3 分支 Windows（run 36434248113） | `cli_raw_attach_round_trip` 失败：`CLI 可执行文件不存在 …\target\debug\termbridge.exe`；随后 `local_two_ends_raw_input_resume_and_key_auth` 以 `PoisonError` 失败（`crates/app/src/lib.rs:215`）；host 测试因 cargo 在第一个失败的测试二进制后停止而**根本没跑** | ① lib 单元测试不会触发构建 bin target，干净环境下 exe 不存在（`crates/app/src/lib.rs:19`、`:598`）；② 该测试在持有 `ENV_LOCK` 时 panic，把锁毒化，连带另一个测试失败（`:215`、`:533`）。 |
| 复核者本机 Windows | `ctrl_c_interrupts` 4/4 失败（`ping -t` 从未被中断） | **产品缺陷**（不只是测试问题），见 A4-④。已用临时补丁验证：在进程开头调用 `SetConsoleCtrlHandler(None, 0)` 后该测试 0.73 s 通过；补丁已还原。 |

要求：今后报告里的「通过」必须附 CI run 链接（三平台），本机结果只能作为补充。

---

## 2. 问题清单（按严重度）

### A. 阻塞级（必须修，修完才能开 M4）

#### A1. SSH 处理器内 `await handle.data()` —— 大量输出时按键会让整个连接死锁
- 位置：`crates/app/src/server.rs:451` 起的 `Handler::data()`，对 `Frame::Input` 处理完后用 `handle.data(channel, line).await` 回 `InputAck/InputRejected`。
- 原因（已核对 russh 0.63.3 源码）：
  - `Handle` 背后是容量为 `event_buffer_size`（默认 **10**）的有界 mpsc（`server/mod.rs:121`、`:1070`），`Handle::data` 是 `sender.send(...).await`；
  - 会话主循环在**同一个任务里** `reply(...).await` 内联执行 handler（`server/session.rs:738`），只有在没有待写数据时才去排空这个 mpsc（`:770`）；
  - 转发任务（`forward_session`）在大量输出时会持续把 10 格塞满 → handler 里的 `send().await` 等主循环排空 → 主循环在等 handler 返回 → **死锁**。
  - 典型触发：`cat 大文件` / 编译刷屏时按 Ctrl+C —— 恰恰是最需要 Ctrl+C 的时刻。
- 修复：handler 里改用参数 `session: &mut Session` 的 `session.data(channel, …)`（直接写入会话自己的发送缓冲，不经过 10 格队列、不 await）。同时全文检查：**任何** handler 回调里都不得 `await` 同一连接的 `Handle`（`:104`、`:277` 在独立任务里，不受影响）。
- 回归测试（必须新增）：会话里跑持续刷屏的命令（Windows：`while($true){'x'*200}`；其他：`yes xxxxxxxx…`），挂接后连续发 50 个 `Input` 帧，断言每个都在 5 s 内收到 `InputAck`，且随后一个普通请求（如 `List`）也能在 5 s 内返回。

#### A2. 输入偏移在任意一次拒绝后永久失步（CLI 与 GUI 都有）
- 位置：
  - 服务端判定：`crates/host/src/lib.rs:958`（`input()`）—— `not_controller` / `busy` / `input_gap` 都不推进 `next`；
  - CLI：`crates/app/src/cli/raw.rs:163` 不论是否被拒都 `offset += bytes.len()`；非控制者时也照样发送；`:134` 收到 `InputRejected` 只提示；
  - GUI：`apps/desktop/src-tauri/src/commands.rs:713`（`forward_event`）收到 `InputRejected` 只转给前端提示，`send_input`（`:1042`）的本地偏移从不回拨。
- 后果：观察者按过一次键、或一次 `busy`、或 `Ctrl+] t` 接管前按过键 → 此后本地偏移 > 服务端 `next`，**之后所有输入都被判 `input_gap`，终端再也打不进字**，只能断开重连。
- 修复（含协议语义修订，需同步改 `docs/dev-plan-v2.md` 3.1 节）：

  **服务端 `input()` 判定顺序改为：**
  1. 会话不存在 / 已结束 / 长度非法 → 按现状拒绝；
  2. `offset + len <= next` → 重复，`Ack{next}`；
  3. `offset > next` → `InputRejected{input_gap, offset: next}`，`next` 不变；
  4. **不是控制者** → **消费并丢弃**：`next = offset + len`，不写 PTY（I1 仍成立），回 `InputRejected{not_controller, offset: 新 next}`；
  5. 队列满 → `InputRejected{busy, offset: next}`，`next` 不变；
  6. 正常写入，`Ack`。
  - 无输入流条目的非控制者（被 LRU 淘汰）按「新建条目、`next = offset`」处理后走第 4 步，而不是固定回 `next: 0`（现 `:1010` 附近）。
  - `attach` 时已有条目：`next = max(next, input_base)`。

  **客户端（CLI 与 GUI 用同一套规则，建议抽到 `crates/app/src/client.rs` 的一个小结构里供 CLI 复用；GUI 在 `state.rs` 同样实现）：**
  - 维护 `acked`、`next_offset` 与未确认缓冲 `unacked`（覆盖 `[acked, next_offset)`）；
  - `InputAck{offset}` → `acked = offset`，丢弃之前的缓冲；
  - `InputRejected{not_controller, offset R}` → 这段已被服务端消费：`acked = R`，丢弃对应缓冲，提示「观察模式，按键未发送」；
  - `InputRejected{busy | input_gap, offset R}` 且 `R` 在缓冲范围内 → 从 `R` 起重发缓冲里 `[R, next_offset)`（busy 退避 50 ms，gap 立即）；用一个 `rewind_pending: Option<u64>` 去重，同一个 `R` 的后续 gap 拒绝忽略（在途帧都会被判 gap，这是预期的）；
  - `R` 早于缓冲起点（无法重发）→ 丢弃缺失部分，用同一 `stream_id`、`input_base = next_offset` 重新 `Attach` 对齐，并提示；
  - 其他错误码（`session_not_live` 等）不重发；
  - **未确认缓冲满时拒绝本地新输入（提示/响铃），不再丢最旧条目**（`apps/desktop/src-tauri/src/state.rs:94`、`:101` 的 drop-oldest 会破坏 M4 的恰好一次重发）。CLI 也要有同样的缓冲（M4 重连本来就需要）。
  - 已知自己是观察者时，CLI 与 GUI 都不应发送输入（GUI 已做到；CLI 要补）。
- 测试（必须新增）：
  - host 单元：观察者发 `[0,5)` → `not_controller` 且 `next==5`；接管后从 5 发送 → 写入 PTY；
  - host 单元：gap 优先于 not_controller；
  - app 集成：观察者连接先打字、再 `TakeControl`、再打字 → 后者的字节出现在输出里；
  - 客户端重发逻辑的纯函数单元测试（busy 回拨、gap 去重、缓冲满拒绝）。

#### A3. GUI 捕获阶段拦截吞掉 Ctrl+R / Ctrl+W / Ctrl+P 等终端按键
- 位置：`apps/desktop/src/terminal.ts:84-89` 在**所有平台**给容器挂了捕获阶段的 keydown 监听；`:182-190` 的 `blockBrowserAccelerators` 做了 `preventDefault()` + `stopPropagation()`。
- 后果：事件在捕获阶段就被截断，xterm 的 textarea 收不到 → bash/PowerShell 的 Ctrl+R 反向搜索、Ctrl+W 删词、Ctrl+P 上一条历史、F5 等**全部失效**；Ctrl+0/±/= 也发不出去。注释写「非 Windows」，实际没有平台判断。
- 修复：
  - 删除容器上的捕获监听；
  - Windows 已用 `SetAreBrowserAcceleratorKeysEnabled(false)`（`apps/desktop/src-tauri/src/lib.rs:106`），不需要前端拦截；
  - 其他平台如仍需防止缩放，在 `attachCustomKeyEventHandler`（`handleKey`）里对**确实会触发 WebView 行为且 xterm 不处理**的组合键只调用 `event.preventDefault()`，返回 `true` 让 xterm 继续处理；xterm 对自己产生数据的按键会自行取消默认行为。
- 验收：GUI 里 bash/PowerShell 中 Ctrl+R 进入反向搜索、Ctrl+W 删词、F5 不刷新页面、`showkey -a`/`cat -v` 能看到 `^R ^W ^P`。

#### A4. 测试基础设施与 Ctrl+C 继承问题
1. `cli_raw_attach_round_trip` 移到 `crates/app/tests/cli_raw.rs`，用 `env!("CARGO_BIN_EXE_termbridge")` 取可执行文件路径（cargo 会保证先构建 bin）。它依赖的测试辅助（`drain_until_answering`、`QueryResponder` 等）放到 `tests/common/mod.rs`，或在 lib 里加 `#[doc(hidden)] pub mod test_support`（只在测试中使用）。
2. 所有 `ENV_LOCK.lock().unwrap()` 改为 `lock().unwrap_or_else(|e| e.into_inner())`，一个测试失败不连坐其他测试。
3. `ctrl_c_interrupts`：非 Windows 改为执行 `printf 'TB_RUN_%s\n' GO; sleep 100`，等输出里出现**执行产生的** `TB_RUN_GO`（回显里只有 `TB_RUN_%s`，不会误匹配），再等 300 ms 后发 `\x03`。
4. **产品修复**：Windows 下「忽略 Ctrl+C」属性（`SetConsoleCtrlHandler(NULL, TRUE)` 或 `CREATE_NEW_PROCESS_GROUP` 产生）会被子进程继承。接收端如果由 GUI、计划任务、服务、或任何本身忽略 Ctrl+C 的父进程拉起，**所有会话里的 Ctrl+C 都失效**（M5/M6 的自启与 session-worker 必然踩到）。修复：Windows 下在创建任何 shell 之前调用一次 `SetConsoleCtrlHandler(None, 0)`（建议放在 `SessionManager::new`；`windows-sys` 加 `Win32_System_Console` feature，已在允许列表内）。M6 的 session-worker 创建时同样要做，并写进 M6 任务。
5. 修完后必须推送分支，三平台 CI 全绿，报告附 run 链接。

### B. 高优先级（本轮一起修）

#### B1. 尺寸抖动会覆盖控制者随后发来的 Resize
- 位置：`crates/host/src/lib.rs:515`（`jiggle_resize`）：先读 dims，`cols-1`，睡 120 ms，然后恢复为**睡眠前读到的旧 cols**。
- 后果：GUI/CLI 挂接后会立即发一次 `Resize`，它很可能落在这 120 ms 窗口里，随后被抖动的「恢复」覆盖 → PTY 与 vt100 尺寸回到旧值，但 `dims` 记的是新值，全屏程序排版错乱。
- 修复：给会话加一个尺寸代数（`resize()` 每次 +1）。抖动开始记下代数；恢复前在同一把锁里比较，若期间发生过 `resize()` 则**跳过恢复**（控制者的 resize 已经触发了重绘）；否则恢复到当前 `dims`。
- 测试：备用屏会话、控制者以快照挂接后立即 `resize(30, 100)`，等 300 ms，断言 `dims`、vt100 屏幕尺寸均为 (30,100)。

#### B2. GUI 输出合并在「持续细流」下会卡住显示
- 位置：`apps/desktop/src-tauri/src/commands.rs:538`（`output_emitter`）：只在**空闲** 16 ms 或累计 256 KiB 时才 emit。
- 后果：输出间隔持续小于 16 ms（LLM 流式输出、`ping`、编译日志）时一直不刷新，直到攒满 256 KiB，Claude Code / Codex 的流式输出会一卡一卡甚至长时间不动。
- 修复：截止时间从**第一个待发字节**起算（最多 16 ms），而不是从最后一个字节起算；256 KiB 上限保留。把「是否该 flush」写成可单测的纯函数并加测试。

#### B3. Windows CLI：单独按 Ctrl+Z 会退出，结尾的 Ctrl+Z 会被吞
- 位置：`crates/app/src/cli/raw.rs:329`（`input_thread`），`:336` 把 `Ok(0)` 当 EOF。
- 原因（已核对 Rust std `library/std/src/sys/stdio/windows.rs:407-440`）：Windows 控制台 stdin 用 `ReadConsoleW` 并设 Ctrl+Z 唤醒掩码，会把末尾的 0x1A 去掉；单独一个 Ctrl+Z 就返回 0 字节。raw 模式下这正是用户要发给远端的按键（vim/Unix shell 挂起、PowerShell 里 ^Z）。
- 修复：Windows 下不用 `std::io::stdin()`，直接调用 `ReadConsoleW`（无唤醒掩码），把 UTF-16 转 UTF-8（处理跨两次读取的代理对）；非 Windows 维持现状。
- 测试：UTF-16→UTF-8 流式解码器单元测试（含拆开的代理对、单独 0x1A）。

#### B4. 挂接记录只按 stream、不区分连接（M4 重连会踩）
- 位置：`crates/host/src/lib.rs:840`（`attach`）、`:924`（`detach`）的 `control.attached: HashSet<Uuid>`；`crates/app/src/server.rs:217`（`handle_attach`）、`:260`（`Worker` 的 `Drop`）。
- 后果：
  - 同一 `stream_id` 从新连接重挂（M4 重连的正常路径），而旧连接稍后才超时断开 → 旧 Worker 的 `Drop` 把该 stream detach，**正在使用的新连接被判离线/进入宽限**；
  - 同一连接改挂另一个 stream 时，旧 stream 永远不会 detach。
- 修复：`attach` 返回一个挂接令牌（递增 u64），`attached` 改为 `HashMap<stream_id, token>`；`detach` 必须带令牌，令牌不匹配则忽略。`handle_attach` 替换旧任务时先 detach 旧 stream（带旧令牌）。
- 测试：conn1 挂 S → conn2 挂 S → 关闭 conn1 → S 仍是在线控制者、conn2 输入被接受；同连接先挂 S1 再挂 S2 → S1 不再在线。

### C. 中优先级（本轮修）

- **C1. GUI 每个按键一个 invoke，可能乱序**：Tauri 的 async 命令并发执行，`input_gate` 只保证「拿到锁的顺序」，不保证按键到达顺序。前端改为单队列：`onData` 只往缓冲里追加，一个循环串行 `await invoke`，每次取走当前全部待发字节（≤16 KiB）。位置 `apps/desktop/src/main.ts` / `ipc.ts` 与 `commands.rs:1042`。
- **C2. `client.rs` 事件缓冲溢出静默丢最旧**：`crates/app/src/client.rs:180`、`:355`。丢了 `SnapshotChunk`/`ControlChanged` 无法察觉。改为：溢出时置标志，下一次 `recv_event` 返回明确的「已丢事件」信号，CLI/GUI 收到后按 `resume_from` 重新挂接。
- **C3. 任何 `CSI … q` 都被记为光标形状**：`crates/host/src/lib.rs:262`，未检查中间字节是否为空格，`CSI > q`（XTVERSION 查询）会被写进快照重放。只在中间字节恰为 `' '` 时记录。加测试。

### D. 低优先级（能顺手就修，否则写进已知问题）

- **D1.** `terminal.ts` 的 `handleKey`：Linux 上 Ctrl+V 也被当粘贴，挡住 vim 的 Ctrl+V 列选择。建议 Linux 只用 Ctrl+Shift+V；Windows 保留 Ctrl+V 粘贴。
- **D2.** `crates/host/src/lib.rs:1167`：代答 CPR 用的是整块处理完后的光标位置，不是查询出现时的位置。可在查询处切分喂给 parser 再应答。
- **D3.** `crates/app/src/cli/raw.rs:287`（`write_stdout`）忽略错误；Windows 控制台 stdout 拒绝非法 UTF-8，Unix 远端的非 UTF-8 字节会被整块丢掉。Windows 下改为流式解码（保留不完整尾部、非法字节替换为 U+FFFD）再写。
- **D4.** Ctrl+Break / 关窗时终端模式不恢复：Windows 下注册 `SetConsoleCtrlHandler` 处理 `CTRL_BREAK_EVENT`/`CTRL_CLOSE_EVENT`，在回调里恢复控制台模式（注意与 A4-④ 分清：那是接收端，这是 CLI 端）。
- **D5.** `crates/host/src/lib.rs:110`（`OutputLog::read`）用 `iter().skip()` 是 O(n)，改 `VecDeque::range()`。
- **D6.** CLI 退出后 stdin 线程吃一个按键：接受为已知问题即可。

---

## 3. 依赖偏离：接受

`src-tauri` 新增 `base64`、`windows-core 0.61`、`webview2-com 0.38`（仅 Windows），理由成立；`crossterm 0.28`、`windows-sys` 的 `Win32_System_Console` 在允许列表内。

## 4. 做得好的地方（无需改动）

- `config.rs:64` 公钥带注释无法认证的修复正确（比较 `key_data()`，导入去注释），并有回归测试。
- `server.rs:282` `forward_session` 取消安全，控制类事件进 `pending` 不丢。
- 输出环 + 游标订阅 + 快照在同一把锁里生成，偏移一致；`Subscription::next/try_next` 取消安全。
- PTY 读线程不会被慢订阅者阻塞（有测试），所有缓冲有界。
- 输入写线程与 1 MiB 有界队列、恰好一次的重叠/重复判定正确（`input_offsets_are_exactly_once`）。
- 查询扫描器跨分片的状态机、OSC 不误判。
- CLI `RawModeGuard` + panic hook 的恢复路径、非 TTY 前置检查（`cli/mod.rs:573`）。
- 报告对「未验证」的标注总体诚实（GUI 矩阵如实标为未验证）。

---

## 5. 给开发代理的执行要求

1. **分支**：从 `v2/m3-cli-raw` 新建 `v2/review-a`，所有修复提交在这条分支上（不要改写已有的 m1/m2/m3 分支）；M4 以后从 `v2/review-a` 开分支。
2. **提交粒度**：每个编号一个提交，提交信息以编号开头，如 `A1: reply to Input frames via Session, not Handle`。D 类可以合并为一个提交。
3. **顺序**：A4（先让测试可信）→ A1 → A2（含改 `docs/dev-plan-v2.md` 3.1 节）→ A3 → B1–B4 → C1–C3 → D。
4. **每个 A/B/C 项都要有对应的自动化测试**（第 2 节已列出），A3、C1 属于前端行为，无法自动化的写进人工矩阵。
5. **完成标准**：
   - 三条必跑命令本机通过；
   - 推送 `v2/review-a`，GitHub Actions 三平台全绿，**报告里贴 run 链接**；
   - 连续跑 3 次 `cargo test --workspace` 均通过（排查偶发失败）。
6. **报告**：写 `docs/reports/review-A-fixes.md`，逐项列出：编号 / 改了什么 / 对应测试名 / 状态（已修、部分修、未修及原因）。不要写「全部通过」而不附输出。
7. 不得扩大范围：不做 M4 的重连逻辑本身，只把 A2/B4 的基础（缓冲、令牌）做对。

## 6. 需要负责人在真实桌面上做的 GUI 人工验收（修完 A3/B2/C1 后）

| 场景 | 看什么 |
|---|---|
| PowerShell | 方向键翻历史、Tab 补全、Ctrl+C 中断 `ping -t`、Esc 清行、Ctrl+R 反向搜索 |
| Claude Code（`claude`） | 输入、斜杠菜单、Esc 中断、Ctrl+C 退出、多行粘贴、**流式输出是否流畅** |
| Codex CLI（`codex`） | 同上 |
| vim | 插入、`:wq`、方向键、窗口缩放后重绘 |
| 窗口缩放 | 提示符与全屏程序是否正确重排 |
| 断开后重挂运行中的 vim | 画面完整恢复 |
| 两个客户端 | 一个控制一个观察；观察者打字有提示且**接管后能正常打字**（验证 A2） |
| 快捷键 | F5 / Ctrl+R / Ctrl+F / Ctrl+W 不触发浏览器行为，且都能传给终端 |
| 中文输入法 | 组字、上屏、中文/emoji 宽度 |
| 剪贴板 | 选中 Ctrl+C 复制、无选区 Ctrl+C 发 ^C、Ctrl+Shift+V / Ctrl+V 粘贴 |
| 刷屏时按 Ctrl+C | `cat` 大文件时 Ctrl+C 能立即生效、连接不卡死（验证 A1） |
