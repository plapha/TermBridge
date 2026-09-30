//! CLI raw 模式（M3）：本地终端进入 raw，按键原样透传给接收端。
//!
//! 本地转义键为 Ctrl+]（0x1d）：
//! - `d` 断开；`e` 结束会话（3 秒内再按一次确认）；`t` 接管；
//! - 再按一次 Ctrl+] 发送字面的 0x1d；其他键取消。
//!
//! 终端模式恢复：guard 的 `Drop` + panic hook（正常退出、出错、panic 都恢复）。

#[cfg(not(windows))]
use std::io::Read;
use std::io::{IsTerminal, Write};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use termbridge_protocol::{Event, Request, Response, MAX_INPUT_CHUNK};
use uuid::Uuid;

use super::expect_response;
use crate::client::{Client, InputBuffer, InputRecovery};
use crate::tr;

/// 状态行 `\r\n[文本]`：raw 模式下换行需要显式回车；文本按当前语言选择。
macro_rules! note {
    ($($t:tt)*) => {
        eprintln!("\r\n[{}]", tr!($($t)*))
    };
}

const ESCAPE: u8 = 0x1d;

enum InputEvent {
    Data(Vec<u8>),
    Detach,
    End,
    Take,
    Eof,
}

pub async fn attach_raw(client: &mut Client, session_id: Option<Uuid>) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!(tr!(
            "session attach needs a real terminal; piped input is not supported",
            "session attach 需要真实终端；不支持管道输入"
        ));
    }
    let session_id = resolve_session(client, session_id).await?;
    let stream_id = Uuid::new_v4();
    let resp = expect_response(
        client
            .request(Request::Attach {
                session_id,
                stream_id,
                input_base: 0,
                resume_from: None,
            })
            .await?,
        "attach",
    )?;
    let Response::Attached {
        session,
        has_control,
        input_next,
        ..
    } = resp
    else {
        bail!(tr!(
            "attach returned an unexpected response",
            "attach 返回了意外响应"
        ));
    };

    let mut guard = RawModeGuard::enable()?;
    let mut is_controller = has_control;
    let mut input_buffer = InputBuffer::new(input_next);
    let mut displayed: Option<u64> = None;
    let mut pending_snapshot: Option<u64> = None;
    let mut end_armed: Option<Instant> = None;

    eprintln!(
        "{}",
        tr!(
            "Attached to {} ({}). Ctrl+] d detach | Ctrl+] e end (asks to confirm) | Ctrl+] t take control | Ctrl+] Ctrl+] send a literal Ctrl+]",
            "已附加到 {}（{}）。Ctrl+] d 断开 | Ctrl+] e 结束（需确认） | Ctrl+] t 接管 | Ctrl+] Ctrl+] 发送字面 Ctrl+]",
            session.id,
            session.title
        )
    );
    if !is_controller {
        eprintln!(
            "{}",
            tr!(
                "Observer mode: keystrokes are not sent; press Ctrl+] t to take control.",
                "当前为观察模式：按键不会发送；按 Ctrl+] t 接管。"
            )
        );
    }

    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<InputEvent>(64);
    std::thread::spawn(move || input_thread(input_tx));
    let (size_tx, mut size_rx) = tokio::sync::mpsc::channel::<(u16, u16)>(8);
    spawn_size_watcher(size_tx);
    if is_controller {
        send_local_size(client, session_id).await;
    }

    let result: Result<()> = loop {
        tokio::select! {
            ev = client.recv_event() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(_) => {
                        note!("event buffer overflowed; reattaching from the displayed offset", "事件缓冲已溢出，按已显示偏移重新挂接");
                        match reattach(client, session_id, stream_id,
                            input_buffer.acked(), displayed).await {
                            Ok((resumed, next)) => {
                                input_buffer.align_attach(next);
                                if !resumed { displayed = None; pending_snapshot = None; }
                                continue;
                            }
                            Err(e) => break Err(e),
                        }
                    }
                };
                let Some(ev) = ev else {
                    note!("connection lost; requests are not resent", "连接已断开，不再重发请求");
                    break Ok(());
                };
                match ev {
                    Event::SnapshotBegin { session_id: sid, offset: snap_offset, .. } if sid == session_id => {
                        pending_snapshot = Some(snap_offset);
                        displayed = None;
                        write_stdout(b"\x1b[0m\x1b[2J\x1b[3J\x1b[H").with_context(|| tr!("Failed to write terminal output", "终端输出失败"))?;
                    }
                    Event::SnapshotChunk { session_id: sid, data_b64 } if sid == session_id => {
                        if let Ok(data) = base64::engine::general_purpose::STANDARD.decode(&data_b64) {
                            write_stdout(&data).with_context(|| tr!("Failed to write terminal output", "终端输出失败"))?;
                        }
                    }
                    Event::SnapshotEnd { session_id: sid } if sid == session_id => {
                        displayed = pending_snapshot.take();
                    }
                    Event::Output { session_id: sid, offset: out_offset, data_b64 } if sid == session_id => {
                        let Ok(data) = base64::engine::general_purpose::STANDARD.decode(&data_b64) else {
                            continue;
                        };
                        match displayed {
                            None => {}
                            Some(current) if out_offset > current => {
                                note!("output gap detected; reattaching", "检测到输出缺口，重新挂接");
                                match reattach(client, session_id, stream_id, input_buffer.next(), Some(current)).await {
                                    Ok((resumed, next_input)) => {
                                        input_buffer.align_attach(next_input);
                                        if resumed {
                                            displayed = Some(current);
                                        } else {
                                            displayed = None;
                                            pending_snapshot = None;
                                        }
                                    }
                                    Err(e) => break Err(e),
                                }
                            }
                            Some(current) => {
                                let end = out_offset + data.len() as u64;
                                if end > current {
                                    let skip = (current - out_offset) as usize;
                                    write_stdout(&data[skip..]).with_context(|| tr!("Failed to write terminal output", "终端输出失败"))?;
                                    displayed = Some(end);
                                }
                            }
                        }
                    }
                    Event::InputAck { session_id: sid, stream_id: st, offset: at }
                        if sid == session_id && st == stream_id => input_buffer.ack(at),
                    Event::InputRejected { session_id: sid, stream_id: st, code, offset: at, .. }
                        if sid == session_id && st == stream_id => {
                        match input_buffer.rejected(&code, at) {
                            InputRecovery::Consumed => note!("observer mode: keystrokes not sent", "观察模式，按键未发送"),
                            InputRecovery::Retry { offset, data, delay } => {
                                if !delay.is_zero() { tokio::time::sleep(delay).await; }
                                for (index, chunk) in data.chunks(MAX_INPUT_CHUNK).enumerate() {
                                    if client.send_input(session_id, stream_id,
                                        offset + (index * MAX_INPUT_CHUNK) as u64, chunk).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            InputRecovery::Reattach { input_base } => {
                                note!("input state unknown; discarding unacknowledged bytes and reattaching", "输入状态未知，丢弃未确认字节并重新挂接");
                                let (_, next) = reattach(client, session_id, stream_id,
                                    input_base, displayed).await?;
                                input_buffer.align_attach(next);
                            }
                            InputRecovery::None => {}
                        }
                    }
                    Event::Resized { session_id: sid, rows, cols } if sid == session_id => {
                        note!("remote size: {cols}x{rows}", "远端尺寸: {cols}x{rows}");
                    }
                    Event::ControlChanged { session_id: sid, controller } if sid == session_id => {
                        is_controller = controller == Some(stream_id);
                        let name = controller
                            .map(|c| c.to_string()[..8].to_string())
                            .unwrap_or_else(|| tr!("none", "无"));
                        note!("control changed: {name}", "控制权变更: {name}");
                        if is_controller {
                            send_local_size(client, session_id).await;
                        }
                    }
                    Event::Ended { session_id: sid } if sid == session_id => {
                        note!("session ended", "会话已结束");
                        break Ok(());
                    }
                    _ => {}
                }
            }
            input = input_rx.recv() => match input {
                Some(InputEvent::Data(bytes)) => {
                    if !is_controller {
                        note!("observer mode: keystrokes not sent; Ctrl+] t to take control", "观察模式，按键未发送；Ctrl+] t 接管");
                        continue;
                    }
                    for chunk in bytes.chunks(MAX_INPUT_CHUNK) {
                        let Some(at) = input_buffer.queue(chunk) else {
                            eprintln!(
                                "\r\n[{}]\x07",
                                tr!(
                                    "unacknowledged input buffer is full; keystroke rejected",
                                    "未确认输入缓冲已满，拒绝本地按键"
                                )
                            );
                            break;
                        };
                        if client.send_input(session_id, stream_id, at, chunk).await.is_err() {
                            note!("connection lost; input of unknown state will not be blindly resent", "连接已断开，状态未知的输入不会盲目重发");
                            break;
                        }
                    }
                }
                Some(InputEvent::Detach) => {
                    match client.request(Request::Detach { session_id }).await {
                        Ok(resp) => {
                            if let Err(e) = expect_response(resp, "detach") {
                                note!("not detached: {e}", "未脱离: {e}");
                            }
                        }
                        Err(_) => note!("connection lost; detached locally", "连接已断开，本地脱离"),
                    }
                    break Ok(());
                }
                Some(InputEvent::End) => {
                    let now = Instant::now();
                    let armed = end_armed
                        .map(|at| now.duration_since(at) < Duration::from_secs(3))
                        .unwrap_or(false);
                    if armed {
                        match client.request(Request::End { session_id }).await {
                            Ok(resp) => {
                                if let Err(e) = expect_response(resp, "end") {
                                    note!("not ended: {e}", "未结束: {e}");
                                }
                            }
                            Err(_) => note!("connection lost; end was not sent", "连接已断开，未发送 end"),
                        }
                        break Ok(());
                    }
                    end_armed = Some(now);
                    note!("press Ctrl+] e again within 3 seconds to confirm ending the session", "3 秒内再按一次 Ctrl+] e 确认结束会话");
                }
                Some(InputEvent::Take) => match client.request(Request::TakeControl { session_id }).await {
                    Ok(resp) => {
                        if let Err(e) = expect_response(resp, "take") {
                            note!("control not taken: {e}", "未获取: {e}");
                        } else {
                            note!("control taken", "已获取控制权");
                            is_controller = true;
                            send_local_size(client, session_id).await;
                        }
                    }
                    Err(_) => {
                        note!("connection lost; nothing is resent", "连接已断开，不再重发");
                        break Ok(());
                    }
                },
                Some(InputEvent::Eof) | None => break Ok(()),
            },
            size = size_rx.recv() => {
                if is_controller && size.is_some() {
                    send_local_size(client, session_id).await;
                }
            }
        }
    };

    guard.restore();
    note!("left raw mode", "已退出 raw 模式");
    result
}

async fn resolve_session(client: &mut Client, session_id: Option<Uuid>) -> Result<Uuid> {
    match session_id {
        Some(id) => Ok(id),
        None => {
            let resp = expect_response(client.request(Request::List).await?, "list")?;
            let Response::Sessions { sessions } = resp else {
                bail!(tr!(
                    "list returned an unexpected response",
                    "list 返回了意外响应"
                ));
            };
            sessions
                .into_iter()
                .find(|s| s.live)
                .map(|s| s.id)
                .with_context(|| {
                    tr!(
                        "No live session; create one with session create first",
                        "没有活跃会话，请先用 session create 创建"
                    )
                })
        }
    }
}

/// 重新挂接：优先按 `resume_from` 重放补洞，服务端决定重放还是给快照。
async fn reattach(
    client: &mut Client,
    session_id: Uuid,
    stream_id: Uuid,
    input_base: u64,
    resume_from: Option<u64>,
) -> Result<(bool, u64)> {
    let resp = expect_response(
        client
            .request(Request::Attach {
                session_id,
                stream_id,
                input_base,
                resume_from,
            })
            .await?,
        "reattach",
    )?;
    let Response::Attached {
        resumed,
        input_next,
        ..
    } = resp
    else {
        bail!(tr!(
            "reattach returned an unexpected response",
            "重新挂接返回了意外响应"
        ));
    };
    Ok((resumed, input_next))
}

/// 仅控制者发送本地实际尺寸（Unix 由 SIGWINCH 触发，Windows 每 250ms 轮询）。
async fn send_local_size(client: &mut Client, session_id: Uuid) {
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        if rows >= 2 && cols >= 2 {
            let _ = client
                .request(Request::Resize {
                    session_id,
                    rows,
                    cols,
                })
                .await;
        }
    }
}

#[cfg(windows)]
#[derive(Default)]
struct Utf8OutputDecoder {
    pending: Vec<u8>,
}

#[cfg(windows)]
impl Utf8OutputDecoder {
    fn decode(&mut self, data: &[u8]) -> String {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(data);
        let mut result = String::new();
        let mut at = 0;
        while at < bytes.len() {
            match std::str::from_utf8(&bytes[at..]) {
                Ok(valid) => {
                    result.push_str(valid);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    result.push_str(std::str::from_utf8(&bytes[at..at + valid]).unwrap());
                    at += valid;
                    match error.error_len() {
                        Some(len) => {
                            result.push(char::REPLACEMENT_CHARACTER);
                            at += len;
                        }
                        None => {
                            self.pending.extend_from_slice(&bytes[at..]);
                            break;
                        }
                    }
                }
            }
        }
        result
    }
}

#[cfg(windows)]
static OUTPUT_DECODER: std::sync::OnceLock<Mutex<Utf8OutputDecoder>> = std::sync::OnceLock::new();

fn write_stdout(data: &[u8]) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    #[cfg(windows)]
    if stdout.is_terminal() {
        // WriteConsoleW 拒绝非法 UTF-8：跨事件保留不完整尾部，非法字节替换。
        let text = OUTPUT_DECODER
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .decode(data);
        let mut out = stdout.lock();
        out.write_all(text.as_bytes())?;
        return out.flush();
    }
    let mut out = stdout.lock();
    out.write_all(data)?;
    out.flush()
}

/// Ctrl+] 转义的逐字节状态机（跨读取分片保持状态）。
struct EscapeFilter {
    escape: bool,
}

enum FilterAction {
    Forward(u8),
    Event(InputEvent),
    None,
}

impl EscapeFilter {
    fn new() -> Self {
        Self { escape: false }
    }

    fn push(&mut self, byte: u8) -> FilterAction {
        if self.escape {
            self.escape = false;
            match byte {
                b'd' => FilterAction::Event(InputEvent::Detach),
                b'e' => FilterAction::Event(InputEvent::End),
                b't' => FilterAction::Event(InputEvent::Take),
                ESCAPE => FilterAction::Forward(ESCAPE),
                _ => FilterAction::None,
            }
        } else if byte == ESCAPE {
            self.escape = true;
            FilterAction::None
        } else {
            FilterAction::Forward(byte)
        }
    }
}

#[cfg(windows)]
#[derive(Default)]
struct Utf16InputDecoder {
    high: Option<u16>,
}

#[cfg(windows)]
impl Utf16InputDecoder {
    fn decode(&mut self, units: &[u16]) -> Vec<u8> {
        let mut text = String::new();
        for &unit in units {
            if let Some(high) = self.high.take() {
                if (0xdc00..=0xdfff).contains(&unit) {
                    let scalar =
                        0x10000 + (((high as u32 - 0xd800) << 10) | (unit as u32 - 0xdc00));
                    text.push(char::from_u32(scalar).unwrap());
                    continue;
                }
                text.push(char::REPLACEMENT_CHARACTER);
            }
            if (0xd800..=0xdbff).contains(&unit) {
                self.high = Some(unit);
            } else if (0xdc00..=0xdfff).contains(&unit) {
                text.push(char::REPLACEMENT_CHARACTER);
            } else {
                text.push(char::from_u32(unit as u32).unwrap());
            }
        }
        text.into_bytes()
    }
}

/// 直接读控制台 UTF-16（无 Ctrl+Z 唤醒掩码）；std::io::Stdin 会吞尾部 0x1A。
#[cfg(windows)]
fn read_console_utf8(
    decoder: &mut Utf16InputDecoder,
    buf: &mut [u8; 8192],
) -> std::io::Result<usize> {
    use windows_sys::Win32::System::Console::{GetStdHandle, ReadConsoleW, STD_INPUT_HANDLE};
    let mut wide = [0u16; 2048];
    let mut count = 0u32;
    let ok = unsafe {
        ReadConsoleW(
            GetStdHandle(STD_INPUT_HANDLE),
            wide.as_mut_ptr().cast(),
            wide.len() as u32,
            &mut count,
            std::ptr::null(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let bytes = decoder.decode(&wide[..count as usize]);
    buf[..bytes.len()].copy_from_slice(&bytes);
    Ok(bytes.len())
}

/// stdin 线程：直接读原始字节；仅拦截本地 Ctrl+] 转义。
fn input_thread(tx: tokio::sync::mpsc::Sender<InputEvent>) {
    #[cfg(not(windows))]
    let mut stdin = std::io::stdin();
    #[cfg(windows)]
    let mut decoder = Utf16InputDecoder::default();
    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    let mut filter = EscapeFilter::new();
    loop {
        #[cfg(windows)]
        let read = read_console_utf8(&mut decoder, &mut buf);
        #[cfg(not(windows))]
        let read = stdin.read(&mut buf);
        match read {
            Ok(0) => {
                #[cfg(windows)]
                {
                    continue;
                } // 不把控制台的零长度读取误判为 Ctrl+Z 后的 EOF。
                #[cfg(not(windows))]
                {
                    flush_pending(&tx, &mut pending);
                    let _ = tx.blocking_send(InputEvent::Eof);
                    return;
                }
            }
            Ok(n) => {
                for &byte in &buf[..n] {
                    match filter.push(byte) {
                        FilterAction::Forward(byte) => pending.push(byte),
                        FilterAction::Event(event) => {
                            flush_pending(&tx, &mut pending);
                            if tx.blocking_send(event).is_err() {
                                return;
                            }
                        }
                        FilterAction::None => {}
                    }
                }
                flush_pending(&tx, &mut pending);
            }
            Err(_) => {
                flush_pending(&tx, &mut pending);
                let _ = tx.blocking_send(InputEvent::Eof);
                return;
            }
        }
    }
}

fn flush_pending(tx: &tokio::sync::mpsc::Sender<InputEvent>, pending: &mut Vec<u8>) {
    if !pending.is_empty() {
        let _ = tx.blocking_send(InputEvent::Data(std::mem::take(pending)));
    }
}

/// 尺寸变化监听：Unix 用 SIGWINCH，Windows 每 250ms 轮询。
fn spawn_size_watcher(tx: tokio::sync::mpsc::Sender<(u16, u16)>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sig = match signal(SignalKind::window_change()) {
            Ok(sig) => sig,
            Err(_) => return,
        };
        tokio::spawn(async move {
            while sig.recv().await.is_some() {
                if let Ok(size) = crossterm::terminal::size() {
                    if tx.send(size).await.is_err() {
                        return;
                    }
                }
            }
        });
    }
    #[cfg(windows)]
    {
        tokio::spawn(async move {
            let mut last = crossterm::terminal::size().ok();
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let current = crossterm::terminal::size().ok();
                if current != last {
                    last = current;
                    if let Some(size) = current {
                        if tx.send(size).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = tx;
    }
}

/* ---------- 终端模式 guard ---------- */

static RESTORE_STATE: Mutex<Option<RestoreState>> = Mutex::new(None);

#[derive(Clone, Copy)]
enum RestoreState {
    #[cfg(unix)]
    Unix,
    #[cfg(windows)]
    Windows { input: u32, output: u32 },
}

fn restore_terminal(state: RestoreState) {
    match state {
        #[cfg(unix)]
        RestoreState::Unix => {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        #[cfg(windows)]
        RestoreState::Windows { input, output } => unsafe {
            use windows_sys::Win32::System::Console::{
                GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
            };
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), input);
            SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), output);
        },
    }
}

#[cfg(windows)]
fn is_terminal_exit_event(kind: u32) -> bool {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT};
    kind == CTRL_BREAK_EVENT || kind == CTRL_CLOSE_EVENT
}

#[cfg(windows)]
unsafe extern "system" fn restore_on_console_exit(kind: u32) -> i32 {
    if !is_terminal_exit_event(kind) {
        return 0;
    }
    if let Ok(mut state) = RESTORE_STATE.lock() {
        if let Some(state) = state.take() {
            restore_terminal(state);
        }
    }
    // 不吞掉 Ctrl+Break / 关窗；让系统默认处理器继续完成退出。
    0
}

fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Ok(mut state) = RESTORE_STATE.lock() {
                if let Some(state) = state.take() {
                    restore_terminal(state);
                }
            }
            previous(info);
        }));
    });
}

struct RawModeGuard {
    active: bool,
}

impl RawModeGuard {
    fn enable() -> Result<Self> {
        install_panic_hook();
        #[cfg(unix)]
        {
            crossterm::terminal::enable_raw_mode()
                .with_context(|| tr!("Cannot enter raw mode", "无法进入 raw 模式"))?;
            *RESTORE_STATE.lock().unwrap() = Some(RestoreState::Unix);
        }
        #[cfg(windows)]
        {
            let (input, output) = enable_windows_raw()?;
            *RESTORE_STATE.lock().unwrap() = Some(RestoreState::Windows { input, output });
            if unsafe {
                windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                    Some(restore_on_console_exit),
                    1,
                )
            } == 0
            {
                if let Some(state) = RESTORE_STATE.lock().unwrap().take() {
                    restore_terminal(state);
                }
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(Self { active: true })
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        if let Some(state) = RESTORE_STATE.lock().unwrap().take() {
            restore_terminal(state);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                Some(restore_on_console_exit),
                0,
            );
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Windows：VT 输入 + 关闭行输入/回显/行处理；输出开启 VT 处理。
#[cfg(windows)]
fn enable_windows_raw() -> Result<(u32, u32)> {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
        ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    unsafe {
        let input = GetStdHandle(STD_INPUT_HANDLE);
        let output = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut input_mode = 0u32;
        let mut output_mode = 0u32;
        if GetConsoleMode(input, &mut input_mode) == 0
            || GetConsoleMode(output, &mut output_mode) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let new_input = (input_mode
            & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        let new_output = output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | ENABLE_PROCESSED_OUTPUT;
        if SetConsoleMode(input, new_input) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if SetConsoleMode(output, new_output) == 0 {
            let _ = SetConsoleMode(input, input_mode);
            return Err(std::io::Error::last_os_error().into());
        }
        Ok((input_mode, output_mode))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn console_exit_handler_only_targets_break_and_close() {
        use windows_sys::Win32::System::Console::{
            CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT,
        };
        assert!(is_terminal_exit_event(CTRL_BREAK_EVENT));
        assert!(is_terminal_exit_event(CTRL_CLOSE_EVENT));
        assert!(!is_terminal_exit_event(CTRL_C_EVENT));
    }

    #[cfg(windows)]
    #[test]
    fn utf8_stdout_decoder_preserves_split_tail_and_replaces_invalid() {
        let mut decoder = Utf8OutputDecoder::default();
        assert_eq!(decoder.decode(&[0xe4, 0xb8]), "");
        assert_eq!(decoder.decode(&[0xad, 0xff, b'!']), "中�!");
        assert_eq!(decoder.decode(b"\x1b[2J"), "\x1b[2J");
    }

    #[cfg(windows)]
    #[test]
    fn utf16_stream_handles_split_surrogates_and_ctrl_z() {
        let mut decoder = Utf16InputDecoder::default();
        assert_eq!(decoder.decode(&[0xd83d]), b"");
        assert_eq!(decoder.decode(&[0xde00]), "😀".as_bytes());
        assert_eq!(decoder.decode(&[0x001a]), b"\x1a");
        assert_eq!(decoder.decode(&[0xd83d, b'A' as u16]), "�A".as_bytes());
    }

    #[test]
    fn plain_bytes_pass_through() {
        let mut filter = EscapeFilter::new();
        let mut forwarded = Vec::new();
        for &byte in b"hello\r\x03\x1b[A" {
            match filter.push(byte) {
                FilterAction::Forward(byte) => forwarded.push(byte),
                FilterAction::Event(_) => panic!("unexpected escape event"),
                FilterAction::None => {}
            }
        }
        assert_eq!(forwarded, b"hello\r\x03\x1b[A");
    }

    #[test]
    fn escape_commands_and_literals() {
        let mut filter = EscapeFilter::new();
        assert!(matches!(filter.push(ESCAPE), FilterAction::None));
        assert!(matches!(
            filter.push(b'd'),
            FilterAction::Event(InputEvent::Detach)
        ));

        assert!(matches!(filter.push(ESCAPE), FilterAction::None));
        assert!(matches!(
            filter.push(b'e'),
            FilterAction::Event(InputEvent::End)
        ));

        assert!(matches!(filter.push(ESCAPE), FilterAction::None));
        assert!(matches!(
            filter.push(b't'),
            FilterAction::Event(InputEvent::Take)
        ));

        // 再按一次 Ctrl+]：发送字面的 0x1d。
        assert!(matches!(filter.push(ESCAPE), FilterAction::None));
        let action = filter.push(ESCAPE);
        assert!(matches!(action, FilterAction::Forward(byte) if byte == ESCAPE));

        // 其他键取消转义：Ctrl+] 与后续键都被吞掉。
        assert!(matches!(filter.push(ESCAPE), FilterAction::None));
        assert!(matches!(filter.push(b'x'), FilterAction::None));
        assert!(matches!(filter.push(b'a'), FilterAction::Forward(b'a')));
    }
}
