# TermBridge v2 开发任务书

本文档交给执行开发的代理（下称「开发者」）。复核由主代理完成。开始任何工作前，先完整读完本文档和 `AGENTS.md`。

## 0. 背景与总目标

v0.1.x 的设计是「整行提交、画面只读」：GUI 终端不接收键盘输入，只能在输入框里写一行点「发送」；CLI 也是按行发送。这个设计已被产品负责人推翻。v2 的目标：

- **P0**：Raw PTY 双向透传。Ctrl+C / Ctrl+D / Esc / 方向键 / Tab 等所有按键实时送达；Claude Code、Codex、vim、htop 等全屏交互程序完整可用；GUI 里的 xterm.js 像普通终端一样交互。
- **P1**：会话由独立的 Session Worker 进程持有，接收端重启后能重新发现会话；Windows 登录后自动后台运行；客户端断线自动重连；输入输出 ACK + 重放。
- **P2**：完整 scrollback；会话名称 / CWD / PID / 状态；更好的会话管理界面；剪贴板；文件拖放与上传下载。

### 0.1 已定的假设（负责人未明确答复，按此执行；如负责人后续改口再调整）

1. **开机自启 = 用户登录后启动**，不做登录前的 Windows 服务。在设置里提供开关，**默认关闭**，由用户手动打开。
2. **取消整行提交模式**。删除 GUI 的输入框（composer）、协议里的 `Send` / `Interrupt` 请求和命令去重（`CommandDedup`）。不保留「行模式」选项。
3. 协议不兼容 v0.1.x：新 subsystem 名为 `termbridge-v2`。版本不一致时给出清楚的错误提示，不做向下兼容。
4. 不做代码签名，不做中继 / NAT 穿透，不做多用户。

### 0.2 当前代码速览（改动前）

| 位置 | 现状 |
|---|---|
| `crates/protocol/src/lib.rs` | JSON 行帧：`Frame::{Request,Response,Event}`；`Request::Send{command_id,text}`、`Interrupt`；`Event::Output{seq,data_b64}`、`ResyncRequired`；`MAX_FRAME` 1 MiB |
| `crates/host/src/lib.rs` | `SessionManager`：portable-pty、vt100 解析、纯文本 scrollback、`broadcast` 事件（容量 1024）、`CommandDedup`、控制权按连接 `client_id`、Windows Job Object、ConPTY 的 CSI 6n（DSR）拦截与 `DSR_GRACE` |
| `crates/app/src/server.rs` | russh 服务端；每连接一个 `Worker` 顺序处理请求（`spawn_blocking`）；`forward_events` 把 broadcast 转发到通道，落后就发 `ResyncRequired` 并退出 |
| `crates/app/src/client.rs` | `Client::request(&mut self)` 单飞请求，30 s 超时；`recv_event` |
| `crates/app/src/cli.rs` | `attach_loop`：stdin 行读取线程，`:detach/:end/:take` |
| `apps/desktop/src/terminal.ts` | xterm.js 只读（`disableStdin: true`），`fit()` 是空实现，没有 FitAddon |
| `apps/desktop/src-tauri/src/commands.rs` | 每个会话一条 SSH 连接 + pump 任务；`create_session` 写死 24×80；`send_text` |
| `apps/desktop/index.html` / `main.ts` | 底部 composer 输入框和「发送」按钮 |

## 1. 工作方式（必须遵守）

1. **按里程碑推进**，每个里程碑一个分支：`v2/m1-protocol`、`v2/m2-gui-raw`……后一个里程碑可以基于前一个分支继续（叠加分支）。
2. **不要**提交或推送到 `main`，不要打标签，不要合并 PR。可以推送 `v2/*` 分支以触发三平台 CI。
3. 分三个复核点停下来等复核：
   - **复核点 A**：M1–M3 完成（P0）
   - **复核点 B**：M4–M6 完成（P1）
   - **复核点 C**：M7–M10 完成（P2）
   到达复核点后停止开发，等复核意见。复核提出的问题在对应分支上追加提交修复。
4. 每个里程碑完成后写报告 `docs/reports/M<n>.md`（模板见第 6 节），和代码一起提交。
5. 每个里程碑提交前必须在本机跑通：
   ```sh
   cargo test --workspace
   cargo check --manifest-path apps/desktop/src-tauri/Cargo.toml
   npm --prefix apps/desktop run build
   ```
   任何一条失败都不能算完成。不允许用 `#[ignore]`、删测试、放宽断言来让测试通过；确实需要调整的测试，在报告里逐条说明理由。
6. 只能修改本仓库。**不要读写** `C:\Users\admin\.wezterm-persistent` 或任何旧 WezTerm / RustDesk 项目。
7. 不要在代码、日志、报告、提交信息里写入任何令牌、密码、私钥。不要回显环境里的凭据。
8. 新增依赖只能从第 5 节的允许列表里选；列表以外的依赖需要在报告里说明理由，并等复核同意。
9. 报告必须如实：没跑的测试写「未运行」，没法在本机验证的（Linux / macOS 实机、GUI 手工操作）写「未验证」，不要写成「通过」。

## 2. 不变式（任何里程碑都不能破坏）

| # | 不变式 |
|---|---|
| I1 | 只有控制者的输入会写进 PTY。观察者发来的 `Input` 一律拒绝（`InputRejected{code:"not_controller"}`），不能静默写入。 |
| I2 | 主机指纹严格锁定。自动重连必须使用已锁定的指纹；指纹不一致时停止重连并报错，**绝不**自动弹出「是否信任新指纹」或自动更新。 |
| I3 | 不记录密码、私钥口令、终端原始输入输出、剪贴板内容、文件内容。错误信息里也不能带这些数据。 |
| I4 | PTY 读线程永远不会因为写锁、网络、客户端慢而阻塞。它只做：读 PTY → 在短临界区里喂 vt100 并追加到输出环 → 唤醒等待者。 |
| I5 | 输入恰好一次：同一个输入偏移的字节不会写入 PTY 两次。重发只能通过偏移机制进行；状态未知的输入要丢弃并提示用户，不能盲目重发。 |
| I6 | 所有缓冲区有上限：输出环、输入队列、快照分块、每会话的输入流表、帧大小、待处理请求数。 |
| I7 | 每条 SSH 连接仍然只有一个通道。文件传输使用单独的 SSH 连接（见 M9）。 |
| I8 | 开机自启默认关闭。 |
| I9 | Session Worker 的本地 IPC 只允许当前用户访问（M5）。 |

## 3. 协议 v2（M1 定义，后续里程碑只能追加字段）

后续里程碑追加字段时一律用 `#[serde(default)]` 或 `Option`，不改已有字段含义。

```rust
pub const SUBSYSTEM: &str = "termbridge-v2";
pub const MAX_FRAME: usize = 1024 * 1024;          // 单帧 JSON（含换行）上限，不变
pub const MAX_INPUT_CHUNK: usize = 16 * 1024;      // 单个 Input 帧的原始字节上限
pub const MAX_OUTPUT_CHUNK: usize = 64 * 1024;     // 单个 Output 事件的原始字节上限
pub const MAX_SNAPSHOT_CHUNK: usize = 256 * 1024;  // 单个 SnapshotChunk 的原始字节上限

pub enum Frame {
    Request { id: Uuid, body: Request },
    Response { id: Uuid, body: Response },
    Event { body: Event },
    /// 客户端 → 接收端，没有 Response；结果通过 InputAck / InputRejected 事件返回。
    Input { session_id: Uuid, stream_id: Uuid, offset: u64, data_b64: String },
}

pub enum Request {
    List,
    Create { title: String, rows: u16, cols: u16 },
    /// stream_id：客户端为「这个标签页看这个会话」生成的 UUID，重连时保持不变。
    /// input_base：客户端认为接收端已确认的输入偏移（首次为 0）。
    /// resume_from：客户端已完整显示到的输出偏移；None 表示首次挂接。
    Attach { session_id: Uuid, stream_id: Uuid, input_base: u64, resume_from: Option<u64> },
    Detach { session_id: Uuid },
    TakeControl { session_id: Uuid },
    Resize { session_id: Uuid, rows: u16, cols: u16 },
    End { session_id: Uuid },
}
// 删除：Send、Interrupt（Ctrl+C 就是输入字节 0x03）

pub enum Response {
    Sessions { sessions: Vec<SessionInfo> },
    Created { session: SessionInfo },
    /// input_next：接收端对这个 stream 期望的下一个输入偏移。
    /// resumed=true：接下来从 resume_from 开始补发 Output，客户端保留现有画面；
    /// resumed=false：接下来会先收到 SnapshotBegin…SnapshotEnd，客户端需重置画面。
    Attached { session: SessionInfo, has_control: bool, input_next: u64, resumed: bool },
    Accepted,
    Error { code: String, message: String },
}

pub enum Event {
    /// offset：data 第一个字节在该会话输出流里的偏移（会话创建时为 0）。
    Output { session_id: Uuid, offset: u64, data_b64: String },
    /// 快照：客户端收到 Begin 时 reset 终端并 resize 到 rows×cols，
    /// 依次写入 Chunk，End 之后的 Output 从 offset 开始。
    SnapshotBegin { session_id: Uuid, offset: u64, rows: u16, cols: u16 },
    SnapshotChunk { session_id: Uuid, data_b64: String },
    SnapshotEnd { session_id: Uuid },
    /// 接收端已接收（写入队列）到 offset（不含）为止的输入。
    InputAck { session_id: Uuid, stream_id: Uuid, offset: u64 },
    InputRejected { session_id: Uuid, stream_id: Uuid, offset: u64, code: String, message: String },
    Resized { session_id: Uuid, rows: u16, cols: u16 },
    ControlChanged { session_id: Uuid, controller: Option<Uuid> }, // controller 现在是 stream_id
    Ended { session_id: Uuid },
}
// 删除：ResyncRequired（落后时接收端直接推送快照，见 3.2）
```

`SessionInfo.controller` 的含义改为控制者的 `stream_id`。

### 3.1 输入偏移（恰好一次）

- 接收端为每个会话保存 `HashMap<stream_id, InputStream { next: u64, last_seen: Instant }>`，每会话最多 64 个，超出时淘汰最久未用的非控制者条目。
- 收到 `Input{offset, data}`（`n = data.len()`，必须 `1 ≤ n ≤ MAX_INPUT_CHUNK`）：
  - 不是控制者 → `InputRejected{not_controller}`；
  - `offset + n <= next` → 整段重复，丢弃，照常回 `InputAck{next}`；
  - `offset > next` → 有缺口，`InputRejected{input_gap}`，不写入；
  - 否则写入 `data[(next - offset)..]`，`next = offset + n`，回 `InputAck{next}`。
- `Attach` 时：已知这个 stream → `input_next = 已知 next`；未知 → 以客户端给的 `input_base` 建立条目，`input_next = input_base`。
- 客户端保存 `acked` 和尚未确认的字节。重连后：丢掉偏移 `< input_next` 的部分，从 `input_next` 开始重发剩下的。
- **断线期间用户新敲的键不排队**：GUI 显示「重连中」，丢弃新输入。只重发断线前已发出但未确认的字节。
- 写入走每会话独立的**写线程**：`Input` 处理把字节放进有界队列（上限 1 MiB 待写字节）后立即回 ACK；队列满时回 `InputRejected{busy}`，`next` 不前进，客户端稍后按偏移重发。ConPTY 写阻塞只会阻塞写线程，不影响读线程（I4）。

### 3.2 输出环、重放与流控

- 每个会话一个输出环 `OutputLog { start: u64, end: u64, buf: VecDeque<u8> }`，容量 4 MiB，超出丢最旧的字节（`start` 前进）。
- PTY 读线程在**同一把锁**里完成「喂 vt100 + 追加输出环」，这样快照和它对应的偏移是一致的。锁内不做任何 I/O。然后通过 `tokio::sync::watch<u64>`（或 `Notify`）唤醒转发任务。
- **不再用 `broadcast` 转发输出**。每个 (连接, 会话) 的转发任务持有自己的游标 `pos`：
  1. 等待 `end > pos`；
  2. 加锁：若 `pos < start`（落后太多，数据已被覆盖）→ 生成快照，按 SnapshotBegin/Chunk/End 发送，`pos = 快照 offset`；否则复制 `min(end - pos, MAX_OUTPUT_CHUNK)` 字节；
  3. 解锁后 `handle.data(...).await` 发送（SSH 窗口满时自然等待，这就是背压），`pos` 前进。
  慢客户端只会让自己落后，最终收到一次快照；不会拖慢 PTY，也不会影响其他客户端。
- `Attach` 的顺序保证：先发 `Attached` 响应，再由转发任务发快照或重放，最后是实时输出。它们要经过同一条有序路径发出。
  - `resume_from = Some(x)` 且 `start <= x <= end` → `resumed = true`，从 `x` 开始发 Output；
  - 其他情况 → `resumed = false`，先发快照。
  - 首次挂接一律用快照，不从 0 重放：历史输出是在不同窗口尺寸下产生的，原样重放会错位。
- 控制类事件（`ControlChanged`、`Resized`、`Ended`）频率很低，可以继续用 broadcast 单独发送。`Ended` 必须在该会话剩余输出发完之后才发出。

### 3.3 快照内容

vt100 0.16 的 `state_formatted()` 只包含当前屏幕内容和几种输入模式（键盘应用模式、光标应用模式、括号粘贴、鼠标）。**它不会输出 `?1049h`**，所以 vim 运行时拿到的快照会被当成主屏写进客户端。快照按下面的顺序组装：

1. `\x1b[0m\x1b[2J\x1b[3J\x1b[H`
2. 历史行（M4 前沿用现有的纯文本 scrollback；M4 改为带颜色）
3. 若 `screen.alternate_screen()` 为真：写入 `\x1b[?1049h`
4. `state_formatted()`
5. vt100 不跟踪、需要接收端自己从输出流里扫描记录并补发的模式：至少 `?1004`（焦点事件）和光标形状 `CSI Ps SP q`。

已知的降级：进入备用屏之前的主屏可见内容无法恢复（退出 vim 后主屏只剩历史，shell 会重画提示符）。滚动区域（DECSTBM）vt100 不对外暴露。因此：**控制者通过快照挂接、且当前处于备用屏时，接收端做一次尺寸抖动**（先 resize 成 `cols-1`，再恢复原尺寸），迫使全屏程序完整重绘。必须用 vim 和 htop 实测这一点。

### 3.4 终端查询的应答

- 控制者在线时，接收端**不再代答** CSI 6n 等查询，原样转发给客户端，由控制者的 xterm.js 应答（xterm.js 的 `onData` 会产生应答字节，作为正常输入发回）。
- 没有在线控制者时（例如刚创建、ConPTY 启动时发 CSI 6n 并阻塞等待），由接收端用 vt100 的光标位置代答 CSI 6n，并用 `\x1b[?1;2c` 代答 DA1（`CSI c` / `CSI 0 c`）。其他查询（OSC 10/11 颜色、XTVERSION 等）不代答。
- 删除 `DSR_GRACE` 以及「发送前先写挂起的 DSR 应答」的逻辑。
- 观察者的 xterm.js 也会对查询产生应答：GUI / CLI 在非控制者状态下不能把 `onData` 发出去（接收端也会按 I1 拒绝）。
- 已知竞态：查询刚转发出去，控制者就断线了，这次查询会没人应答。可以接受，写进报告的已知问题。

### 3.5 尺寸

- 只有控制者能 `Resize`。尺寸变化后广播 `Resized`。
- 观察者把 xterm.js 设为会话的 rows×cols（不 fit 窗口），超出部分出现滚动条或留白。
- 获得控制权（挂接即控制或 TakeControl 成功）后，客户端立即把自己 fit 后的尺寸发一次 `Resize`。
- `Create` 的尺寸由客户端实际 fit 结果决定，不再写死 24×80。

## 4. 里程碑

每个里程碑列出「可编辑范围」。范围以外的文件原则上不动；确实需要动的，在报告里说明。

---

### M1 协议 v2 与接收端核心

**可编辑**：`crates/protocol/**`、`crates/host/**`、`crates/app/src/server.rs`、`crates/app/src/client.rs`、`crates/app/src/lib.rs`（集成测试）；为保持编译需要对 `crates/app/src/cli.rs`、`apps/desktop/src-tauri/src/*.rs`、`apps/desktop/src/*.ts` 做最小适配。

**要做**：

1. 按第 3 节改协议，补齐 encode/decode 测试（每种 Frame/Event 往返；超过 `MAX_FRAME` 被拒绝）。
2. `crates/host`：
   - 输出环、游标式订阅 API，例如 `fn subscribe(&self, session_id, from: Option<u64>) -> Subscription`，由它决定重放还是快照；
   - 写线程 + 有界输入队列；
   - 输入流表与偏移判定（3.1）；
   - 控制权改为按 `stream_id`。控制者所在连接断开后保留控制权 60 秒：期间其他客户端挂接不会自动拿到控制权，但可以显式 TakeControl；同一 `stream_id` 重新挂接即恢复控制；60 秒后释放；
   - 查询应答策略（3.4）、快照组装（3.3）；
   - 删除 `CommandDedup`、`send`、`interrupt`、`DSR_GRACE`。
3. `server.rs`：
   - 解析 `Frame::Input`，交给会话，**不经过**顺序请求队列（输入不能被慢请求挡住）。但 `data()` 回调里不能做阻塞操作：只做校验和入队，入队用非阻塞 `try_send`；
   - 转发任务改为游标式；
   - 连接断开时对所有挂接执行 detach（保留控制权宽限期）。
4. `client.rs`：
   - 新增 `send_input(&self, session_id, stream_id, offset, data)`，不等待响应，并且在另一个请求等待响应期间也能发送。可以用 `Arc<Mutex<Channel写半边>>`，也可以用独立发送任务；
   - `recv_event` 能收到新事件。
5. 最小适配：CLI 和 GUI 暂时把「发送一行」实现成 `Input(text + "\r")`，并自己维护偏移。M2/M3 会替换掉。

**自动化测试（必须新增）**：

- `input_offsets_are_exactly_once`：重复段丢弃、部分重叠只写新部分、缺口被拒绝，用 PTY 回显或假写端验证写入的字节。
- `observer_input_rejected`。
- `slow_subscriber_gets_snapshot_not_block`：一个订阅者不读，持续产生大量输出（超过环容量），PTY 读线程不阻塞；另一个正常订阅者持续收到连续偏移；慢订阅者恢复读取后先收到快照，再收到连续输出。
- `resume_replays_from_offset`：订阅到偏移 x 后断开，再以 `resume_from = x` 订阅，拿到的字节与连续订阅完全一致。
- `snapshot_marks_alternate_screen`：喂入 `\x1b[?1049h` 和一些内容后，快照包含 `?1049h`。
- `controller_grace_period`：断开后 60 秒内（测试里把宽限期做成可注入的参数）同一 stream 重挂恢复控制；其他 stream 挂接不自动获得控制。
- `ctrl_c_interrupts`：发送一个长时间运行的命令（Windows 用 `ping -t 127.0.0.1`，其他系统用 `sleep 100`），再发 `\x03`，shell 回到提示符。
- 改写 `crates/app/src/lib.rs` 里的集成测试，使用 v2 流程：挂接、Input、Output 偏移连续、断开后 resume。

**完成标准**：上面的测试在 Windows 本机通过；CI 三平台通过。

---

### M2 GUI 原生终端交互

**可编辑**：`apps/desktop/src/**`、`apps/desktop/index.html`、`apps/desktop/package.json`、`package-lock.json`、`apps/desktop/src-tauri/src/**`、`apps/desktop/src-tauri/Cargo.toml`、`apps/desktop/src-tauri/tauri.conf.json`、`apps/desktop/src-tauri/capabilities/**`。

**要做**：

1. 删除 composer 输入框和「发送」按钮，以及 `send_text` 命令。
2. `terminal.ts`：
   - `disableStdin: false`；接入 `@xterm/addon-fit`（配合 `ResizeObserver`，100 ms 防抖）和 `@xterm/addon-unicode11`（设 `term.unicode.activeVersion = '11'`）；
   - `onData` / `onBinary` → 仅控制者发送；非控制者按键在状态栏提示「观察模式，点「接管」后可输入」。
3. 新增 Tauri 命令 `send_input(session_id, data: Vec<u8> 或 base64)`。Rust 侧给每个标签页维护 `stream_id`、输入偏移和未确认缓冲，大于 `MAX_INPUT_CHUNK` 的数据要切块。**输入不能排在 pump 里正在等响应的请求后面。**
4. 输出：pump 把连续的 Output 合并后再 emit（例如每 16 ms 或累计 256 KiB 一次），前端用 `term.write(Uint8Array)`。前端检查偏移连续性：出现重叠就跳过重复字节；出现缺口就触发重新挂接。
5. 快照事件：SnapshotBegin → `term.reset()` 并 `resize(cols, rows)`，写入各 Chunk，End 之后恢复正常。
6. 剪贴板和快捷键（`attachCustomKeyEventHandler`）：
   - 有选区时 Ctrl+C 复制，并**不**发送 ^C；没有选区时发送 ^C；
   - Ctrl+Shift+C 复制，Ctrl+Shift+V 和 Ctrl+V 粘贴；
   - 粘贴必须走 `term.paste(text)`，这样远端开启括号粘贴时会自动加括号；
   - macOS 用 Cmd+C / Cmd+V，并设置 `macOptionIsMeta: true`；
   - 读写剪贴板用 `tauri-plugin-clipboard-manager` 或 `navigator.clipboard`，两者任选，要实测在 Tauri 里可用。
7. 屏蔽 WebView 的浏览器快捷键：Windows 上通过 `with_webview` 取得 `ICoreWebView2Settings3`，调用 `SetAreBrowserAcceleratorKeysEnabled(false)`。这会屏蔽 F5 / Ctrl+R 刷新、Ctrl+F、Ctrl+P、Ctrl+±缩放等，但不影响输入框里的复制粘贴。其他平台至少在终端获得焦点时 `preventDefault` 掉 Ctrl+R / F5 / Ctrl+W。
8. 创建会话时使用实际 fit 出的行列数；获得控制权后立即发送 Resize；观察者按会话尺寸显示（3.5）。
9. 中文输入法（IME）组字要正常，中文和 emoji 宽度显示正确。

**手工验收矩阵（本机 Windows 必须做，写进报告）**：

| 场景 | 预期 |
|---|---|
| PowerShell：方向键翻历史、Tab 补全、Ctrl+C 中断 `ping -t`、Esc 清行 | 与 Windows Terminal 表现一致 |
| Claude Code（`claude`）：输入、斜杠菜单、Esc 中断、Ctrl+C 退出、多行粘贴 | 正常，粘贴不会被逐行执行 |
| Codex CLI（`codex`）：同上 | 正常 |
| vim 或 nvim（若本机未安装，写「未安装未测」）：插入、`:wq`、方向键 | 正常，退出后画面恢复 |
| 窗口缩放 | 远端程序随之重排 |
| 断开后重新挂接一个正在运行 vim 的会话 | 画面正确（经过尺寸抖动重绘） |
| 两个客户端：一个控制、一个观察，观察者接管 | 控制权切换，尺寸跟随新控制者 |
| 在终端里按 F5 / Ctrl+R / Ctrl+F | 不刷新页面，按键送给远端程序 |
| 中文输入法输入 | 组字正常 |

Linux / macOS 的同样矩阵（加上 htop、less、tmux）由复核方或负责人执行，报告里标「未验证」。

---

### M3 CLI Raw 模式

**可编辑**：`crates/app/src/cli.rs`、`crates/app/Cargo.toml`，以及为此新增的 `crates/app/src/cli/*.rs`（如果拆分文件）。

**要做**：

1. `session attach` 进入 raw 模式。Unix 用 `crossterm::terminal::enable_raw_mode`。Windows 上把控制台输入模式设为 `ENABLE_VIRTUAL_TERMINAL_INPUT` 并去掉 `ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT`，输出模式开 `ENABLE_VIRTUAL_TERMINAL_PROCESSING`。之后直接从 stdin 读原始字节，不要用 crossterm 的事件 API 再拼字节（那样会丢失原始序列）。
2. 转义键 **Ctrl+]**（0x1d）后接一个键：
   - `d`：断开；
   - `e`：结束会话（需要确认）；
   - `t`：接管；
   - 再按一次 Ctrl+]：发送一个字面的 0x1d；
   - 其他键：取消。
   进入 attach 时在 stderr 打印一行提示。
3. 退出时恢复终端模式，包括正常退出、出错、panic（用 guard 的 `Drop` 加 panic hook）。
4. 窗口尺寸：Unix 监听 SIGWINCH；Windows 每 250 ms 轮询一次尺寸，变化时发送 Resize（仅控制者）。
5. 删除 `:detach` / `:end` / `:take` 行命令和按行发送的逻辑。stdin 不是 TTY 时报错退出，不支持管道输入。
6. 输出直接写 stdout（原始字节），快照事件照 SnapshotBegin 的做法先清屏。

**验收**：

- 用 CLI 在本机回环连接中完成 M2 矩阵里的 PowerShell、Claude Code、Codex 三项；
- Ctrl+] d 后终端模式完全恢复（回显、行编辑正常）；
- 进程被 Ctrl+Break 杀掉后终端模式能否恢复，如实记录。

**复核点 A**：M1–M3 完成后停止，提交报告，等待复核。

---

### M4 自动重连、重放、完整 scrollback

**可编辑**：`crates/host/**`、`crates/app/src/{client,cli,server}.rs`、`apps/desktop/src/**`、`apps/desktop/src-tauri/src/**`。

**要做**：

1. **客户端重连**（GUI 与 CLI）：
   - SSH 断开或保活超时（客户端保活 15 s，3 次无响应判定断线）后按 1、2、4、8、16、30、30… 秒退避重连；
   - 使用已锁定的指纹（I2）；
   - 认证：密码只在内存里保留到应用退出（用 `zeroize` 在释放时清零），不写盘，除非用户原本就选了「记住密码」；私钥口令同理；
   - 重连成功后，每个标签页用原来的 `stream_id`、`input_base = acked`、`resume_from = 已显示到的偏移` 重新挂接，按 3.1 重发未确认输入。
2. **UI**：标签页显示「连接中 / 已连接 / 重连中（第 n 次）/ 已断开」。重连中禁止输入。会话已结束（`session not found`）时停止重连并提示。
3. **完整 scrollback**：
   - vt100 解析器开启 scrollback：`Parser::new(rows, cols, SCROLLBACK_LINES)`，默认 10000 行，可在 host 配置里改，上限 100000；
   - 快照里的历史改为带颜色：借助 `set_scrollback` 分页 + `rows_formatted` 导出，或其他等价方案，方案写进报告；
   - 删除旧的纯文本 `Scrollback`；
   - 快照可能超过 1 MiB，已经按 `MAX_SNAPSHOT_CHUNK` 分块；
   - GUI 的 xterm `scrollback` 与之匹配。
4. CLI 重连沿用同一套逻辑；重连期间在 stderr 提示。

**测试**：

- 集成测试：挂接 → 输出 → 在服务端强制断开连接 → 客户端重连 → `resumed = true`，输出偏移连续，没有重复也没有缺失；
- 未确认输入重发后只写入一次；
- 指纹变化时重连停止并报错（换一把主机密钥启动服务端）；
- 快照历史保留颜色（断言包含 SGR 序列）。

**手工**：GUI 连接中拔网线或断开 Tailscale 30 秒，恢复后自动接上，画面连续；期间 Claude Code 仍在远端运行。

---

### M5 Session Worker 独立进程

这是整个计划里风险最高的部分。先在报告里写设计说明（进程关系、IPC、注册表、升级），等复核确认后再写大量代码。可以先提交一个只含设计说明的报告，标注「M5 设计待确认」。

**目标**：接收端（监听 SSH 的进程，GUI 内嵌的或 `termbridge host run`）重启、崩溃或升级后，会话仍然在运行，重新启动的接收端能发现并接管它们。

**结构**：

- 新子命令 `termbridge session-worker --id <uuid> ...`（在 `--help` 里隐藏）。每个会话一个 worker 进程，持有：ConPTY / PTY、子进程、Job Object（Windows，kill-on-close，确保 worker 死亡时整棵子进程树被结束）、vt100 状态、输出环、输入流表、控制权状态。**3.1–3.5 的逻辑全部移到 worker 里**，接收端只做 SSH ↔ IPC 的转发。控制权和输入偏移存在 worker 里，所以接收端重启不会丢失。
- `crates/host` 拆成两层：会话核心（worker 内使用）和 `SessionManager` 代理（接收端内使用，通过 IPC 调用 worker）。
- 本地 IPC：
  - Windows：命名管道 `\\.\pipe\termbridge-<用户SID的短哈希>-<session uuid>`，创建时设置只允许当前用户 SID 的 DACL，加 `PIPE_REJECT_REMOTE_CLIENTS` 和 `FILE_FLAG_FIRST_PIPE_INSTANCE`。注意：默认 DACL 会给 Everyone 读权限，不能用默认值（I9）；
  - Unix：`$XDG_RUNTIME_DIR/termbridge/`（没有时用配置目录下的 `run/`），目录权限 0700，socket 0600；accept 后用 `SO_PEERCRED` / `getpeereid` 校验对端 uid。
  - IPC 帧可以复用 JSON 行格式，但要有独立的握手 `Hello { worker_version, ipc_version }`。
- 注册表：worker 启动后写 `<配置目录>/sessions/<uuid>.json`，内容包括 id、title、pid、管道名、created、worker 版本、ipc 版本，写入要原子（先写临时文件再改名）。worker 正常退出时删除自己的文件。接收端启动时扫描这些文件：连接得上且握手通过的纳入管理；pid 已经不存在的删除文件；版本不兼容的在列表里标「版本不兼容」，只允许结束（按 pid 结束进程）。
- 进程脱离：
  - Windows：`CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`（或 `DETACHED_PROCESS`）。如果父进程所在的 Job 不允许 breakaway，CreateProcess 会失败，这时要有退路（例如通过 WMI `Win32_Process.Create` 或任务计划启动），并把失败原因写清楚；
  - 接收端自身的 Job Object 不能再包含 worker（现在的 kill-on-close Job 要移到 worker 里）；
  - Unix：`setsid()`，关闭继承的 fd。
- 升级与 exe 锁：Windows 上正在运行的 exe 无法被安装程序覆盖。worker 从一份按版本复制的可执行文件启动：`<配置目录>/workers/termbridge-<version>.exe`（没有就从 sidecar 复制过去）。接收端启动时清理没有 worker 在用的旧版本副本。
- 生命周期语义变化：
  - 「停止接收」和托盘「退出 TermBridge」**不再结束会话**；
  - 托盘增加「退出并结束所有会话」；
  - shell 退出 → worker 清理并退出；
  - 不做空闲自动结束，列表里显示最后活动时间，由用户自己结束；
  - Windows 注销或重启后会话必然丢失，README 里写明。
- systemd：README 里的 unit 要加 `KillMode=process`，否则重启服务会把 worker 一起杀掉。

**测试**：

- 集成测试：启动接收端 → 创建会话 → 输出 → 停止接收端（worker 继续运行）→ 启动新的接收端 → `List` 能看到该会话 → resume 挂接后偏移连续，输入偏移状态保留；
- worker 被杀后，注册文件在下次扫描时被清理，子进程被 Job 结束（Windows）；
- 以另一个用户或低完整性级别访问管道被拒绝：能做就做，做不了写「未验证」，并附上 DACL 设置代码的位置；
- 测试结束时不能留下任何 worker 进程（测试夹具负责清理）。

**手工**：GUI 里开一个运行 Claude Code 的会话，托盘退出 GUI，重新打开，启用接收端，连接端能重新挂上且 Claude Code 仍在运行。安装新版本覆盖安装时不因 exe 被占用而失败。

---

### M6 Windows 登录后后台运行

**可编辑**：`apps/desktop/src-tauri/**`、`apps/desktop/src/**`、`apps/desktop/index.html`。

**要做**：

1. 使用 `tauri-plugin-autostart`（Windows 是 HKCU Run），启动参数 `--background`。设置界面加开关「登录后在后台运行接收端」，默认关（I8）。
2. 以 `--background` 启动时不显示主窗口，只有托盘；如果 `host.json` 里 `enabled = true`，自动开始监听；然后接管 M5 里已有的 worker。
3. 已有实例在运行时，再次启动应聚焦已有窗口（`tauri-plugin-single-instance`），不能起两个实例抢端口。
4. macOS / Linux 同一插件可以顺带支持，但只要求 Windows 验收。
5. 更新 README 里「不会开机自启」的描述。

**验收**：打开开关 → 注销再登录 → 托盘出现，接收端在监听，另一端能连上；关闭开关后注册表项被删除。

**复核点 B**：M4–M6 完成后停止，提交报告，等待复核。

---

### M7 会话元数据与会话管理界面

**要做**：

1. `SessionInfo` 追加字段（均用 `#[serde(default)]`）：
   - `pid`：shell 的 pid；
   - `cwd: Option<String>`；
   - `osc_title: Option<String>`：程序通过 OSC 0/2 设置的标题；
   - `attached: u32`：在线客户端数；
   - `has_live_controller: bool`；
   - `last_output_unix_ms`；
   - `status`：`running` / `exited` / `incompatible`。
2. 新请求 `Rename { session_id, title }`（仅控制者，或无人控制时任何人）。
3. CWD 获取：
   - Linux：`tcgetpgrp(master_fd)` 取前台进程组，读 `/proc/<pid>/cwd`；
   - macOS：`proc_pidinfo(PROC_PIDVNODEPATHINFO)`；
   - 全平台：解析输出里的 OSC 7（`file://host/path`）和 OSC 9;9，有值时优先；
   - Windows：PowerShell 的 `Set-Location` 不改变进程 cwd，只能靠 OSC。创建会话时用 `-NoExit -Command` 注入一个包装原有 `prompt` 函数、额外输出 OSC 9;9 的脚本，不能破坏用户 profile 里自定义的 prompt。设置里提供开关，默认开。
4. 会话管理界面：
   - 按连接档案分组列出远端会话，显示标题、CWD、PID、状态、在线人数、最后活动时间；
   - 可以重命名、挂接、结束、刷新；
   - 标签页标题优先用 `osc_title`。
5. CLI `session list` 输出这些字段。

**测试**：OSC 7 / 9;9 / 0 / 2 解析单元测试；Linux 下 cwd 获取测试（CI 上运行）；Rename 的权限测试。

---

### M8 剪贴板（OSC 52）与右键菜单

1. 远端程序通过 OSC 52 写剪贴板（`ESC ] 52 ; c ; <base64> BEL/ST`）：
   - 由**客户端**处理（xterm.js 用 `registerOscHandler(52)`，CLI 直接透传给本地终端）；
   - 只处理控制者所在的客户端；
   - 单次上限 1 MiB；
   - 设置里提供开关，默认开。
2. OSC 52 读取请求（`?`）一律拒绝，不应答。
3. 右键菜单：复制、粘贴、全选、清屏（只清本地显示）。
4. 接收端不能在快照里重放 OSC 52，否则挂接时会重复写剪贴板。检查输出环重放路径，确认只有实时和 resume 输出会触发。

---

### M9 文件传输

1. 服务端：每条连接的第一个也是唯一一个通道上，subsystem 可以是 `termbridge-v2` 或 `sftp`（I7 保持不变）。`sftp` 用 `russh-sftp` 的服务端实现，工作目录默认为用户主目录。和 shell 同一个用户、同样的权限，不额外扩大权限。
2. 客户端：`russh-sftp` 客户端，**单独开一条 SSH 连接**，使用同一套认证和指纹锁定。
3. GUI：
   - 把文件或文件夹拖到终端上 → 上传到会话当前 CWD（没有 CWD 时用主目录），可在弹窗里改目标目录；
   - 显示进度，可以取消；
   - 同名文件询问覆盖、跳过或重命名；
   - 下载：会话管理界面里提供简单的远端文件浏览（列目录、下载、上传），不做编辑；
   - 使用 Tauri 的原生拖放事件（`onDragDropEvent`）。
4. CLI：`termbridge cp <本地> <档案>:<远端路径>` 和反向，支持 `-r`。
5. 大文件流式传输，内存占用有上限，不能整个读进内存。

**测试**：回环集成测试上传 / 下载一个 20 MiB 随机文件，校验 SHA-256；在路径不存在、权限不足时报错清楚；取消后不留下半截文件（用临时文件名，完成后再改名）。

---

### M10 收尾

1. README 全面更新：
   - 删除「不适合交互式全屏程序」和「输入需要明确提交」；
   - 重写「安全设计」：控制权、输入偏移恰好一次、指纹锁定、IPC 权限、OSC 52 策略、文件传输权限；
   - 更新「会话的生命周期」（worker、注销丢失、托盘退出语义）；
   - 更新 systemd 示例（`KillMode=process`）；
   - 更新「现状」。
2. 四处版本号改为 `0.2.0`。**不打标签**，发布由负责人决定。
3. `docs/reports/SUMMARY.md`：汇总所有里程碑、未验证项和已知问题。

**复核点 C**：M7–M10 完成后停止，提交报告，等待复核。

## 5. 允许新增的依赖

| 位置 | 依赖 | 用途 |
|---|---|---|
| crates/app | `crossterm` | CLI raw 模式（Unix）和尺寸读取 |
| crates/app / host | `windows-sys`（按需开启 feature） | 控制台模式、命名管道 DACL、进程创建标志、Job Object |
| crates/app / host | `libc` 或 `nix` | setsid、tcgetpgrp、SO_PEERCRED |
| crates/app | `zeroize` | 内存中的密码清零 |
| crates/app | `russh-sftp` | M9 |
| desktop 前端 | `@xterm/addon-fit`、`@xterm/addon-unicode11` | M2 |
| desktop 前端 | `@xterm/addon-webgl`（可选） | 大量输出时的渲染性能 |
| desktop Rust | `tauri-plugin-autostart`、`tauri-plugin-single-instance`、`tauri-plugin-clipboard-manager` | M6 / M2 |
| desktop Rust | `webview2-com`（仅 Windows） | M2 屏蔽浏览器快捷键 |

xterm.js 保持 5.x 版本，不升级大版本；如确需升级，在报告里说明理由。

## 6. 报告模板（`docs/reports/M<n>.md`）

```markdown
# M<n> <名称> 报告

## 1. 完成情况
- 逐条对照任务书「要做」列表：已完成 / 部分完成 / 未做，未完成的说明原因。

## 2. 提交
- 分支名；提交哈希与一句话说明（git log --oneline 输出）。

## 3. 设计要点与偏离
- 实际采用的方案；与任务书不同的地方及理由。
- 新增的依赖及理由。

## 4. 测试
- 三条必跑命令的结果（贴出最后几行输出，含通过/失败数量）。
- 新增测试列表：测试名 → 验证了什么。
- 手工验收矩阵：每一项写「通过 / 失败（现象）/ 未验证（原因）」。

## 5. 未验证项
- 哪些没法在本机验证，需要谁在什么环境验证。

## 6. 已知问题与风险
- 包括本文件中列出的「已知竞态 / 已知降级」在实现后的实际表现。

## 7. 需要复核重点看的地方
- 列出你最没把握的代码位置（文件:行号）。
```

## 7. 易错点清单（开发时对照）

1. **读线程阻塞**：任何在读线程里 `lock()` 写端、`blocking_send`、等待网络的写法都违反 I4。现有代码里读线程在处理 DSR 时会拿写锁，重构时要去掉。
2. **ConPTY 启动时的 CSI 6n**：ConPTY 启动时会查询光标位置，没有应答会卡住。无人控制时必须由接收端代答（3.4）。
3. **ConPTY 发出的 `?9001h`（win32-input-mode）/ `?1004h`**：原样透传；xterm.js 会忽略不认识的模式。不要为了「清理」而过滤输出。
4. **双重应答**：控制者在线时接收端不能再代答，否则程序会收到两份应答。
5. **vt100 快照缺陷**：备用屏、滚动区域、焦点模式，见 3.3。
6. **broadcast 的 Lagged**：旧的「落后就要求客户端重挂」逻辑已废弃，改用游标 + 自动快照。
7. **WebView2 快捷键**：F5 / Ctrl+R 会刷新整个 GUI，导致所有标签页丢失，必须屏蔽。
8. **没有 FitAddon**：现有的 `fit()` 是空实现，M2 必须真正实现。
9. **Tauri 事件洪泛**：`cat` 一个大文件时，如果每 4 KiB emit 一次会卡死界面，必须合并后再 emit。前端待写入的数据超过 8 MiB 时，丢弃本地缓冲并重新挂接拿快照。
10. **粘贴**：不要把粘贴内容当按键逐字发送，要用 `term.paste()` 并按 16 KiB 切块，保证偏移连续。
11. **Job Object breakaway**：见 M5。
12. **exe 被占用**：见 M5 的版本化副本。
13. **systemd 杀进程组**：见 M5。
14. **测试泄漏进程**：每个测试都要结束自己创建的会话和 worker；Windows 上残留的 powershell / worker 会让后续测试和安装失败。
15. **CRLF**：Windows 上 Enter 发 `\r`，由 ConPTY 处理。不要在客户端把 `\r` 转成 `\r\n`。
