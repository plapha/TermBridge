//! Agent（脚本）接口的核心：不经过真实终端，也能向会话发送输入、读取屏幕和输出。
//!
//! 本模块只有两类内容：
//! - 纯逻辑：按键名转字节、终端输出转纯文本、用 vt100 渲染屏幕；
//! - 会话操作：附着、按偏移可靠地发送输入、等待输出（空闲 / 正则 / 超时）。
//!
//! 约束（与 AGENTS.md 一致）：输入按偏移确认，状态未知的输入不重发；所有缓冲有上限；
//! 不记录终端原始输入输出。命令行在 `cli` 里把这些结果按 `--json` 或文本输出。

use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use base64::Engine as _;
use regex::Regex;
use serde::Serialize;
use termbridge_protocol::{Event, Request, Response, SessionInfo, MAX_INPUT_CHUNK};
use uuid::Uuid;

use crate::client::{Client, EventLoss, InputBuffer, InputRecovery};

/// 累计输出的上限；超出时丢弃最早的部分并标记 `truncated`。
pub const MAX_COLLECTED_OUTPUT: usize = 4 * 1024 * 1024;
/// 等待输入确认的时限。
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// 等待快照的时限。
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
/// 不等待条件时，事件静默多久算读完。
const SETTLE_QUIET: Duration = Duration::from_millis(150);
const SETTLE_MAX: Duration = Duration::from_secs(3);
/// 用正则匹配时只看最近这么多字节的输出。
const MATCH_WINDOW: usize = 64 * 1024;

/// 带稳定错误码的错误；`--json` 下以 `{"error":{"code":...}}` 输出。
#[derive(Debug)]
pub struct CodedError {
    pub code: String,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

impl std::fmt::Display for CodedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CodedError {}

pub fn coded(code: &str, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(CodedError {
        code: code.to_string(),
        message: message.into(),
        data: None,
    })
}

/// 同 [`coded`]，并附带结构化细节（例如待确认的主机指纹）。
pub fn coded_with(
    code: &str,
    message: impl Into<String>,
    data: serde_json::Value,
) -> anyhow::Error {
    anyhow::Error::new(CodedError {
        code: code.to_string(),
        message: message.into(),
        data: Some(data),
    })
}

/* ---------- 按键名 → 字节 ---------- */

/// 把按键名转成发送给终端的字节。支持 enter、tab、esc、backspace、space、方向键、
/// home/end/pageup/pagedown/delete/insert、f1–f12、`ctrl-<字母>`、`alt-<字符>`。
pub fn key_bytes(name: &str) -> Option<Vec<u8>> {
    let lower = name.trim().to_ascii_lowercase();
    let fixed: Option<&[u8]> = match lower.as_str() {
        "enter" | "return" => Some(b"\r"),
        "tab" => Some(b"\t"),
        "esc" | "escape" => Some(b"\x1b"),
        "backspace" => Some(b"\x7f"),
        "space" => Some(b" "),
        "up" => Some(b"\x1b[A"),
        "down" => Some(b"\x1b[B"),
        "right" => Some(b"\x1b[C"),
        "left" => Some(b"\x1b[D"),
        "home" => Some(b"\x1b[H"),
        "end" => Some(b"\x1b[F"),
        "pageup" | "pgup" => Some(b"\x1b[5~"),
        "pagedown" | "pgdn" => Some(b"\x1b[6~"),
        "delete" | "del" => Some(b"\x1b[3~"),
        "insert" | "ins" => Some(b"\x1b[2~"),
        "f1" => Some(b"\x1bOP"),
        "f2" => Some(b"\x1bOQ"),
        "f3" => Some(b"\x1bOR"),
        "f4" => Some(b"\x1bOS"),
        "f5" => Some(b"\x1b[15~"),
        "f6" => Some(b"\x1b[17~"),
        "f7" => Some(b"\x1b[18~"),
        "f8" => Some(b"\x1b[19~"),
        "f9" => Some(b"\x1b[20~"),
        "f10" => Some(b"\x1b[21~"),
        "f11" => Some(b"\x1b[23~"),
        "f12" => Some(b"\x1b[24~"),
        _ => None,
    };
    if let Some(bytes) = fixed {
        return Some(bytes.to_vec());
    }
    if let Some(rest) = lower.strip_prefix("ctrl-") {
        let mut chars = rest.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return None;
        };
        return match c {
            'a'..='z' => Some(vec![c as u8 - b'a' + 1]),
            '[' => Some(vec![0x1b]),
            '\\' => Some(vec![0x1c]),
            ']' => Some(vec![0x1d]),
            '^' => Some(vec![0x1e]),
            '_' => Some(vec![0x1f]),
            _ => None,
        };
    }
    if let Some(rest) = lower.strip_prefix("alt-") {
        let mut chars = rest.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return None;
        };
        let mut out = vec![0x1b];
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        return Some(out);
    }
    None
}

/* ---------- 终端输出 → 纯文本 ---------- */

#[derive(Clone, Copy, PartialEq, Eq)]
enum Esc {
    Text,
    Escape,
    Csi,
    /// OSC / DCS 等以 BEL 或 ST（ESC \）结束的串。
    Str,
    StrEscape,
    /// `ESC ( B` 之类：再吞一个字符。
    Charset,
}

/// 尽力而为的纯文本化：去掉转义序列，处理 `\r` 覆盖、退格和行内擦除。
/// 它用于“这条命令输出了什么”；权威的当前画面请用 [`Screen`]。
pub fn plain_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<String> = Vec::new();
    let mut line: Vec<char> = Vec::new();
    let mut col = 0usize;
    let mut state = Esc::Text;
    let mut params = String::new();
    for c in text.chars() {
        match state {
            Esc::Text => match c {
                '\x1b' => state = Esc::Escape,
                '\n' => {
                    lines.push(line.iter().collect::<String>().trim_end().to_string());
                    line.clear();
                    col = 0;
                }
                '\r' => col = 0,
                '\x08' => col = col.saturating_sub(1),
                '\t' => {
                    let next = (col / 8 + 1) * 8;
                    while col < next {
                        put(&mut line, &mut col, ' ');
                    }
                }
                c if c.is_control() => {}
                c => put(&mut line, &mut col, c),
            },
            Esc::Escape => {
                state = match c {
                    '[' => {
                        params.clear();
                        Esc::Csi
                    }
                    ']' | 'P' | 'X' | '^' | '_' => Esc::Str,
                    '(' | ')' | '*' | '+' | '#' => Esc::Charset,
                    _ => Esc::Text,
                };
            }
            Esc::Csi => {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    apply_csi(c, &params, &mut line, &mut col);
                    state = Esc::Text;
                } else {
                    params.push(c);
                }
            }
            Esc::Str => match c {
                '\x07' => state = Esc::Text,
                '\x1b' => state = Esc::StrEscape,
                _ => {}
            },
            Esc::StrEscape => state = if c == '\\' { Esc::Text } else { Esc::Str },
            Esc::Charset => state = Esc::Text,
        }
    }
    lines.push(line.iter().collect::<String>().trim_end().to_string());
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn put(line: &mut Vec<char>, col: &mut usize, c: char) {
    if *col < line.len() {
        line[*col] = c;
    } else {
        while line.len() < *col {
            line.push(' ');
        }
        line.push(c);
    }
    *col += 1;
}

fn apply_csi(final_byte: char, params: &str, line: &mut Vec<char>, col: &mut usize) {
    let n = params
        .split(';')
        .next()
        .and_then(|p| p.parse::<usize>().ok());
    match final_byte {
        // K：擦除行内容（0 = 光标到行尾，2 = 整行）。
        'K' => match n.unwrap_or(0) {
            0 => line.truncate(*col),
            2 => line.clear(),
            _ => {}
        },
        'C' => {
            for _ in 0..n.unwrap_or(1).min(1000) {
                if *col >= line.len() {
                    line.push(' ');
                }
                *col += 1;
            }
        }
        'D' => *col = col.saturating_sub(n.unwrap_or(1)),
        _ => {}
    }
}

/* ---------- 屏幕渲染 ---------- */

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ScreenView {
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub alternate_screen: bool,
    /// 每行一项，去掉行尾空白和末尾的空行。
    pub lines: Vec<String>,
}

/// 按接收端给出的快照和后续输出，在本地还原终端画面。
pub struct Screen {
    parser: vt100::Parser,
}

impl Screen {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows.max(2), cols.max(2), 0),
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows.max(2), cols.max(2));
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.parser.process(data);
    }

    pub fn view(&self) -> ScreenView {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let mut lines: Vec<String> = screen
            .rows(0, cols)
            .map(|row| row.trim_end().to_string())
            .collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        ScreenView {
            rows,
            cols,
            cursor_row,
            cursor_col,
            alternate_screen: screen.alternate_screen(),
            lines,
        }
    }

    pub fn text(&self) -> String {
        self.view().lines.join("\n")
    }
}

/* ---------- 观察：快照 + 输出的累积 ---------- */

/// 收到事件后对调用者有意义的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    SnapshotDone,
    Output,
    Ended,
}

/// 累积一个会话的快照与输出，并维护本地屏幕。
pub struct Collector {
    session_id: Uuid,
    screen: Option<Screen>,
    snapshot_buf: Vec<u8>,
    snapshot_rows: u16,
    snapshot_cols: u16,
    /// 已看到的输出末尾偏移（下一个期望的 Output 偏移）。
    pub end_offset: Option<u64>,
    output: Vec<u8>,
    pub truncated: bool,
    /// 输出偏移出现缺口（丢了数据）。
    pub gap: bool,
    pub ended: bool,
    pub got_snapshot: bool,
    pub last_output_at: Instant,
}

impl Collector {
    pub fn new(session_id: Uuid, resume_from: Option<u64>) -> Self {
        Self {
            session_id,
            screen: None,
            snapshot_buf: Vec::new(),
            snapshot_rows: 24,
            snapshot_cols: 80,
            end_offset: resume_from,
            output: Vec::new(),
            truncated: false,
            gap: false,
            ended: false,
            got_snapshot: false,
            last_output_at: Instant::now(),
        }
    }

    pub fn handle(&mut self, event: Event) -> Effect {
        match event {
            Event::SnapshotBegin {
                session_id,
                offset,
                rows,
                cols,
            } if session_id == self.session_id => {
                self.snapshot_buf.clear();
                self.snapshot_rows = rows;
                self.snapshot_cols = cols;
                self.end_offset = Some(offset);
                // 新快照取代此前累积的一切。
                self.output.clear();
                Effect::None
            }
            Event::SnapshotChunk {
                session_id,
                data_b64,
            } if session_id == self.session_id => {
                if let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data_b64) {
                    if self.snapshot_buf.len() + data.len() <= MAX_COLLECTED_OUTPUT {
                        self.snapshot_buf.extend_from_slice(&data);
                    }
                }
                Effect::None
            }
            Event::SnapshotEnd { session_id } if session_id == self.session_id => {
                let mut screen = Screen::new(self.snapshot_rows, self.snapshot_cols);
                screen.feed(&self.snapshot_buf);
                self.snapshot_buf = Vec::new();
                self.screen = Some(screen);
                self.got_snapshot = true;
                Effect::SnapshotDone
            }
            Event::Output {
                session_id,
                offset,
                data_b64,
            } if session_id == self.session_id => {
                let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data_b64) else {
                    return Effect::None;
                };
                let mut data = data.as_slice();
                let mut offset = offset;
                if let Some(expected) = self.end_offset {
                    if offset > expected {
                        self.gap = true;
                    } else if offset < expected {
                        // 重放与已见部分重叠：只取新的那段。
                        let skip = (expected - offset) as usize;
                        if skip >= data.len() {
                            return Effect::None;
                        }
                        data = &data[skip..];
                        offset = expected;
                    }
                }
                self.end_offset = Some(offset + data.len() as u64);
                if let Some(screen) = &mut self.screen {
                    screen.feed(data);
                }
                self.output.extend_from_slice(data);
                if self.output.len() > MAX_COLLECTED_OUTPUT {
                    let drop = self.output.len() - MAX_COLLECTED_OUTPUT;
                    self.output.drain(..drop);
                    self.truncated = true;
                }
                self.last_output_at = Instant::now();
                Effect::Output
            }
            Event::Resized {
                session_id,
                rows,
                cols,
            } if session_id == self.session_id => {
                if let Some(screen) = &mut self.screen {
                    screen.resize(rows, cols);
                }
                Effect::None
            }
            Event::Ended { session_id } if session_id == self.session_id => {
                self.ended = true;
                Effect::Ended
            }
            _ => Effect::None,
        }
    }

    pub fn screen_view(&self) -> Option<ScreenView> {
        self.screen.as_ref().map(Screen::view)
    }

    /// 累积输出的纯文本。
    pub fn output_text(&self) -> String {
        plain_text(&self.output)
    }

    fn matches(&self, re: &Regex) -> bool {
        let tail = &self.output[self.output.len().saturating_sub(MATCH_WINDOW)..];
        if re.is_match(&plain_text(tail)) {
            return true;
        }
        self.screen
            .as_ref()
            .is_some_and(|screen| re.is_match(&screen.text()))
    }
}

/* ---------- 等待条件 ---------- */

#[derive(Clone, Debug)]
pub struct WaitSpec {
    /// 这么久没有新输出就返回。
    pub idle: Option<Duration>,
    /// 输出（或屏幕）匹配该正则就返回。
    pub until: Option<Regex>,
    pub timeout: Duration,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// 没有设置等待条件，读完当前内容就返回。
    Immediate,
    Idle,
    Match,
    Timeout,
    Ended,
}

/// 读取事件直到 `done` 返回 true、超时或连接结束。
async fn pump(
    client: &mut Client,
    collector: &mut Collector,
    deadline: Instant,
    mut done: impl FnMut(&Collector, Effect) -> bool,
) -> Result<bool> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        match tokio::time::timeout(remaining, client.recv_event()).await {
            Err(_) => return Ok(false),
            Ok(Err(EventLoss)) => {
                return Err(coded(
                    "event_loss",
                    "output events were lost because this client read too slowly; run the command again",
                ))
            }
            Ok(Ok(None)) => {
                return Err(coded("disconnected", "the connection to the host was closed"))
            }
            Ok(Ok(Some(event))) => {
                let effect = collector.handle(event);
                if done(collector, effect) {
                    return Ok(true);
                }
            }
        }
    }
}

/// 按等待条件继续收集输出。
///
/// 没有等待条件时：`settle` 为 true 则读完已经在路上的事件（快照之后的重放或紧随的输出）
/// 再返回，否则立即返回。
pub async fn wait_for(
    client: &mut Client,
    collector: &mut Collector,
    spec: Option<&WaitSpec>,
    settle: bool,
) -> Result<Reason> {
    let Some(spec) = spec else {
        if !settle {
            return Ok(Reason::Immediate);
        }
        let start = Instant::now();
        let hard = start + SETTLE_MAX;
        loop {
            let quiet_until = Instant::now() + SETTLE_QUIET;
            let got = pump(client, collector, quiet_until.min(hard), |_, _| true).await?;
            if !got || Instant::now() >= hard {
                break;
            }
        }
        return Ok(if collector.ended {
            Reason::Ended
        } else {
            Reason::Immediate
        });
    };
    let deadline = Instant::now() + spec.timeout;
    collector.last_output_at = Instant::now();
    loop {
        if collector.ended {
            return Ok(Reason::Ended);
        }
        if let Some(re) = &spec.until {
            if collector.matches(re) {
                return Ok(Reason::Match);
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(Reason::Timeout);
        }
        let idle_at = spec.idle.map(|idle| collector.last_output_at + idle);
        if let Some(at) = idle_at {
            if now >= at {
                return Ok(Reason::Idle);
            }
        }
        let wake = idle_at.map_or(deadline, |at| at.min(deadline));
        pump(client, collector, wake, |_, effect| {
            matches!(effect, Effect::Output | Effect::Ended)
        })
        .await?;
    }
}

/* ---------- 会话操作 ---------- */

pub struct Attachment {
    pub session: SessionInfo,
    pub stream_id: Uuid,
    pub has_control: bool,
    pub input_next: u64,
    pub resumed: bool,
}

/// 附着到会话；`resume_from` 为 `Some` 时优先按偏移重放，否则先收到快照。
pub async fn attach(
    client: &mut Client,
    session_id: Uuid,
    resume_from: Option<u64>,
) -> Result<Attachment> {
    let stream_id = Uuid::new_v4();
    let response = client
        .request(Request::Attach {
            session_id,
            stream_id,
            input_base: 0,
            resume_from,
        })
        .await?;
    match response {
        Response::Attached {
            session,
            has_control,
            input_next,
            resumed,
        } => Ok(Attachment {
            session,
            stream_id,
            has_control,
            input_next,
            resumed,
        }),
        Response::Error { code, message } => {
            Err(coded(&code, crate::i18n::wire_message(&code, &message)))
        }
        _ => bail!("attach returned an unexpected response"),
    }
}

/// 等到快照收完（首次附着或重放越界时服务端会先发快照）。
pub async fn wait_snapshot(client: &mut Client, collector: &mut Collector) -> Result<()> {
    let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
    let done = pump(client, collector, deadline, |_, effect| {
        effect == Effect::SnapshotDone
    })
    .await?;
    if done {
        Ok(())
    } else {
        Err(coded(
            "snapshot_timeout",
            "the host did not send the screen snapshot in time",
        ))
    }
}

/// 取得控制权；已是控制者时什么也不做。其他客户端持有控制权而没有 `take_control` 时报错。
pub async fn ensure_control(
    client: &mut Client,
    session_id: Uuid,
    has_control: bool,
    take_control: bool,
) -> Result<()> {
    if has_control {
        return Ok(());
    }
    if !take_control {
        return Err(coded(
            "not_controller",
            "another client holds control of this session; pass --take-control to take it over",
        ));
    }
    match client.request(Request::TakeControl { session_id }).await? {
        Response::Accepted => Ok(()),
        Response::Error { code, message } => {
            Err(coded(&code, crate::i18n::wire_message(&code, &message)))
        }
        _ => bail!("take control returned an unexpected response"),
    }
}

/// 发送输入并等到接收端确认。
///
/// 被拒绝的输入只在能确定内容时才重发（busy / input_gap）；状态未知（断线、确认超时、
/// 区间丢失）一律报错，绝不盲目重发。返回确认后的输入偏移。
pub async fn send_input(
    client: &mut Client,
    collector: &mut Collector,
    session_id: Uuid,
    stream_id: Uuid,
    input_next: u64,
    data: &[u8],
) -> Result<u64> {
    let mut buffer = InputBuffer::new(input_next);
    for chunk in data.chunks(MAX_INPUT_CHUNK) {
        let Some(at) = buffer.queue(chunk) else {
            return Err(coded("input_buffer_full", "too much unacknowledged input"));
        };
        client
            .send_input(session_id, stream_id, at, chunk)
            .await
            .map_err(|e| coded("input_state_unknown", format!("{e}")))?;
    }
    let target = buffer.next();
    let deadline = Instant::now() + ACK_TIMEOUT;
    while buffer.acked() < target {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = match tokio::time::timeout(remaining, client.recv_event()).await {
            Err(_) => {
                return Err(coded(
                    "input_unconfirmed",
                    "the host did not acknowledge the input in time; whether it was delivered is unknown, so it was not resent",
                ))
            }
            Ok(Err(EventLoss)) => {
                return Err(coded(
                    "input_state_unknown",
                    "events were lost before the input was acknowledged; its delivery state is unknown",
                ))
            }
            Ok(Ok(None)) => {
                return Err(coded(
                    "input_state_unknown",
                    "the connection closed before the input was acknowledged; its delivery state is unknown",
                ))
            }
            Ok(Ok(Some(event))) => event,
        };
        match event {
            Event::InputAck {
                session_id: sid,
                stream_id: st,
                offset,
            } if sid == session_id && st == stream_id => buffer.ack(offset),
            Event::InputRejected {
                session_id: sid,
                stream_id: st,
                offset,
                code,
                message,
            } if sid == session_id && st == stream_id => {
                match buffer.rejected(&code, offset) {
                    InputRecovery::Consumed => {
                        return Err(coded(
                            "not_controller",
                            "this client lost control of the session; the input was not delivered",
                        ))
                    }
                    InputRecovery::Retry {
                        offset,
                        data,
                        delay,
                    } => {
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        for (index, chunk) in data.chunks(MAX_INPUT_CHUNK).enumerate() {
                            client
                                .send_input(
                                    session_id,
                                    stream_id,
                                    offset + (index * MAX_INPUT_CHUNK) as u64,
                                    chunk,
                                )
                                .await
                                .map_err(|e| coded("input_state_unknown", format!("{e}")))?;
                        }
                    }
                    InputRecovery::Reattach { .. } => return Err(coded(
                        "input_state_unknown",
                        "the host lost track of part of the input; it was discarded and not resent",
                    )),
                    InputRecovery::None => {
                        // input_gap 的重复通知已经在重发中；其余错误码是确定的失败。
                        if code != "input_gap" {
                            return Err(coded(&code, crate::i18n::wire_message(&code, &message)));
                        }
                    }
                }
            }
            other => {
                collector.handle(other);
            }
        }
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_map_to_bytes() {
        assert_eq!(key_bytes("enter"), Some(b"\r".to_vec()));
        assert_eq!(key_bytes("Ctrl-C"), Some(vec![3]));
        assert_eq!(key_bytes("ctrl-z"), Some(vec![26]));
        assert_eq!(key_bytes("ctrl-]"), Some(vec![0x1d]));
        assert_eq!(key_bytes("up"), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes("f5"), Some(b"\x1b[15~".to_vec()));
        assert_eq!(key_bytes("alt-x"), Some(b"\x1bx".to_vec()));
        assert_eq!(key_bytes("ctrl-"), None);
        assert_eq!(key_bytes("ctrl-ab"), None);
        assert_eq!(key_bytes("hyper"), None);
    }

    #[test]
    fn plain_text_strips_escapes_and_honors_carriage_returns() {
        assert_eq!(plain_text(b"\x1b[31mred\x1b[0m\r\nok\r\n"), "red\nok");
        // 进度条：\r 回到行首覆盖。
        assert_eq!(plain_text(b"10%\r50%\r100%\r\ndone\r\n"), "100%\ndone");
        // 行内擦除与退格。
        assert_eq!(plain_text(b"abcdef\r\x1b[3C\x1b[K\r\n"), "abc");
        assert_eq!(plain_text(b"ab\x08c\r\n"), "ac");
        // OSC 标题、bracketed paste 开关、字符集切换都不应泄漏到文本里。
        assert_eq!(
            plain_text(b"\x1b]0;title\x07\x1b[?2004hprompt$ \x1b(B"),
            "prompt$"
        );
        assert_eq!(plain_text(b"\x1b]0;t\x1b\\x"), "x");
        // 多字节字符原样保留；制表符按字符列对齐（不计全角宽度，属于尽力而为）。
        assert_eq!(plain_text("中文ok".as_bytes()), "中文ok");
        assert_eq!(plain_text(b"ab\tc"), "ab      c");
        assert_eq!(plain_text(b""), "");
    }

    #[test]
    fn screen_renders_text_and_cursor() {
        let mut screen = Screen::new(5, 20);
        screen.feed(b"hello\r\nworld");
        let view = screen.view();
        assert_eq!(view.lines, vec!["hello", "world"]);
        assert_eq!((view.cursor_row, view.cursor_col), (1, 5));
        assert!(!view.alternate_screen);
        screen.feed(b"\x1b[2J\x1b[Hfresh");
        assert_eq!(screen.text(), "fresh");
        screen.resize(3, 10);
        assert_eq!(screen.view().rows, 3);
    }

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    #[test]
    fn collector_builds_screen_from_snapshot_and_follows_output() {
        let sid = Uuid::new_v4();
        let mut c = Collector::new(sid, None);
        c.handle(Event::SnapshotBegin {
            session_id: sid,
            offset: 100,
            rows: 4,
            cols: 20,
        });
        c.handle(Event::SnapshotChunk {
            session_id: sid,
            data_b64: b64(b"$ ls\r\nfile"),
        });
        assert_eq!(
            c.handle(Event::SnapshotEnd { session_id: sid }),
            Effect::SnapshotDone
        );
        assert_eq!(c.screen_view().unwrap().lines, vec!["$ ls", "file"]);
        assert_eq!(c.end_offset, Some(100));

        c.handle(Event::Output {
            session_id: sid,
            offset: 100,
            data_b64: b64(b"_a\r\nnext"),
        });
        assert_eq!(c.end_offset, Some(108));
        assert_eq!(c.output_text(), "_a\nnext");
        assert_eq!(
            c.screen_view().unwrap().lines,
            vec!["$ ls", "file_a", "next"]
        );

        // 与已见部分重叠的重放只取新的一段；缺口会被标记。
        c.handle(Event::Output {
            session_id: sid,
            offset: 104,
            data_b64: b64(b"\r\nnextXY"),
        });
        assert_eq!(c.end_offset, Some(112));
        assert!(!c.gap);
        c.handle(Event::Output {
            session_id: sid,
            offset: 200,
            data_b64: b64(b"z"),
        });
        assert!(c.gap);

        // 别的会话的事件被忽略。
        let before = c.end_offset;
        c.handle(Event::Output {
            session_id: Uuid::new_v4(),
            offset: before.unwrap(),
            data_b64: b64(b"zzz"),
        });
        assert_eq!(c.end_offset, before);
        assert_eq!(c.handle(Event::Ended { session_id: sid }), Effect::Ended);
        assert!(c.ended);
    }

    #[test]
    fn collector_caps_collected_output() {
        let sid = Uuid::new_v4();
        let mut c = Collector::new(sid, Some(0));
        let chunk = vec![b'a'; 64 * 1024];
        let mut offset = 0u64;
        for _ in 0..80 {
            c.handle(Event::Output {
                session_id: sid,
                offset,
                data_b64: b64(&chunk),
            });
            offset += chunk.len() as u64;
        }
        assert!(c.truncated);
        assert!(c.output.len() <= MAX_COLLECTED_OUTPUT);
    }

    #[test]
    fn regex_matches_output_tail_or_screen() {
        let sid = Uuid::new_v4();
        let mut c = Collector::new(sid, None);
        c.handle(Event::SnapshotBegin {
            session_id: sid,
            offset: 0,
            rows: 3,
            cols: 20,
        });
        c.handle(Event::SnapshotEnd { session_id: sid });
        c.handle(Event::Output {
            session_id: sid,
            offset: 0,
            data_b64: b64(b"build \x1b[32mOK\x1b[0m\r\n"),
        });
        assert!(c.matches(&Regex::new("build OK").unwrap()));
        assert!(!c.matches(&Regex::new("FAILED").unwrap()));
    }
}
