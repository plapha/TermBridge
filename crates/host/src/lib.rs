//! 持久化的每用户终端会话（v2：raw 双向透传 + 偏移式订阅）。
//!
//! 关键不变式：
//! - PTY 读线程只做「喂 vt100 + 追加输出环」，锁内不做任何 I/O（I4）。
//! - 输入按 `(stream_id, offset)` 恰好一次写入；观察者输入一律拒绝（I1/I5）。
//! - 所有缓冲区有上限（I6）。

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use termbridge_protocol::{Event, SessionInfo, MAX_INPUT_CHUNK, MAX_OUTPUT_CHUNK};
use tokio::sync::{broadcast, watch};
use uuid::Uuid;

pub const DEFAULT_CONTROL_GRACE: Duration = Duration::from_secs(60);
const OUTPUT_RING_BYTES: usize = 4 * 1024 * 1024;
const MAX_INPUT_QUEUE_BYTES: usize = 1024 * 1024;
const MAX_INPUT_STREAMS: usize = 64;
const MAX_ROWS: u16 = 500;
const MAX_COLS: u16 = 1000;
const EVENT_CHANNEL_CAPACITY: usize = 1024;
const MAX_SCROLLBACK_LINES: usize = 5000;
const MAX_SCROLLBACK_BYTES: usize = 8 * 1024 * 1024;
const SNAPSHOT_HISTORY_BYTES: usize = 256 * 1024;

pub struct SessionManager {
    inner: Arc<Inner>,
}

struct Inner {
    sessions: Mutex<HashMap<Uuid, Arc<Session>>>,
    control_grace: Duration,
    #[cfg(windows)]
    job: WinJob,
}

// The per-user host owns a kill-on-close job: Windows logoff, forced host
// termination and ordinary shutdown all close this handle, killing PTY children.
#[cfg(windows)]
struct WinJob(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl WinJob {
    fn new() -> Result<Self> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let handle = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                std::os::windows::io::AsRawHandle::as_raw_handle(&handle),
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of_val(&info) as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(handle))
    }
    fn assign(&self, process: std::os::windows::io::RawHandle) -> Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        let ok = unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), process) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

/* ---------- 输出环与快照 ---------- */

#[derive(Debug, Default)]
struct OutputLog {
    start: u64,
    end: u64,
    buf: VecDeque<u8>,
}

impl OutputLog {
    fn append(&mut self, data: &[u8]) {
        self.buf.extend(data);
        self.end = self.end.saturating_add(data.len() as u64);
        if self.buf.len() > OUTPUT_RING_BYTES {
            let drop = self.buf.len() - OUTPUT_RING_BYTES;
            self.buf.drain(..drop);
            self.start = self.start.saturating_add(drop as u64);
        }
    }

    fn read(&self, from: u64, max: usize) -> Vec<u8> {
        debug_assert!(from >= self.start && from <= self.end);
        let skip = (from - self.start) as usize;
        let take = ((self.end - from) as usize).min(max);
        self.buf.iter().skip(skip).take(take).copied().collect()
    }
}

struct Scrollback {
    lines: VecDeque<String>,
    bytes: usize,
    partial: Vec<u8>,
}

impl Scrollback {
    fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            partial: Vec::new(),
        }
    }

    fn push_bytes(&mut self, data: &[u8]) {
        for byte in data {
            if *byte == b'\n' {
                let line = String::from_utf8_lossy(&self.partial).into_owned();
                self.partial.clear();
                self.push_line(&line);
            } else {
                self.partial.push(*byte);
                // A single unterminated line must not grow without bound.
                if self.partial.len() > 16 * 1024 {
                    self.partial.drain(..self.partial.len() - 16 * 1024);
                }
            }
        }
    }

    fn push_line(&mut self, line: &str) {
        let line = line.trim_end_matches('\r');
        let cost = line.len() + 2;
        self.lines.push_back(line.to_string());
        self.bytes += cost;
        while self.lines.len() > MAX_SCROLLBACK_LINES || self.bytes > MAX_SCROLLBACK_BYTES {
            if let Some(front) = self.lines.pop_front() {
                self.bytes = self.bytes.saturating_sub(front.len() + 2);
            } else {
                break;
            }
        }
    }

    fn older_history(&self, visible_rows: usize, limit: usize) -> Vec<u8> {
        let older = self.lines.len().saturating_sub(visible_rows);
        let mut chosen = Vec::new();
        let mut bytes = 0;
        for line in self.lines.iter().take(older).rev() {
            if bytes + line.len() + 2 > limit {
                break;
            }
            bytes += line.len() + 2;
            chosen.push(line);
        }
        let mut out = Vec::with_capacity(bytes);
        for line in chosen.into_iter().rev() {
            out.extend_from_slice(line.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out
    }
}

/// vt100 不跟踪、需要接收端自己扫描记录的模式与查询。
#[derive(Default)]
struct StreamTracker {
    scan: ScanState,
    params: Vec<u8>,
    focus_reporting: bool,
    cursor_style: Option<Vec<u8>>,
}

#[derive(Default, PartialEq)]
enum ScanState {
    #[default]
    Ground,
    Esc,
    Csi,
    Osc,
}

enum Query {
    CursorPosition,
    DeviceAttributes,
}

impl StreamTracker {
    fn scan(&mut self, data: &[u8]) -> Vec<Query> {
        let mut queries = Vec::new();
        for &b in data {
            match self.scan {
                ScanState::Ground => {
                    if b == 0x1b {
                        self.scan = ScanState::Esc;
                    }
                }
                ScanState::Esc => match b {
                    b'[' => {
                        self.scan = ScanState::Csi;
                        self.params.clear();
                    }
                    b']' => self.scan = ScanState::Osc,
                    0x1b => {}
                    _ => self.scan = ScanState::Ground,
                },
                ScanState::Csi => match b {
                    0x1b => {
                        self.scan = ScanState::Esc;
                        self.params.clear();
                    }
                    0x40..=0x7e => {
                        self.finish_csi(b, &mut queries);
                        self.scan = ScanState::Ground;
                        self.params.clear();
                    }
                    0x20..=0x3f => {
                        if self.params.len() < 64 {
                            self.params.push(b);
                        }
                    }
                    _ => {
                        self.scan = ScanState::Ground;
                        self.params.clear();
                    }
                },
                ScanState::Osc => match b {
                    0x07 => self.scan = ScanState::Ground,
                    0x1b => self.scan = ScanState::Esc,
                    _ => {}
                },
            }
        }
        queries
    }

    fn finish_csi(&mut self, final_byte: u8, queries: &mut Vec<Query>) {
        match final_byte {
            b'h' | b'l' => {
                if self.params.first() == Some(&b'?') {
                    for num in parse_csi_numbers(&self.params[1..]) {
                        if num == 1004 {
                            self.focus_reporting = final_byte == b'h';
                        }
                    }
                }
            }
            b'q' => {
                let mut seq = Vec::with_capacity(self.params.len() + 3);
                seq.extend_from_slice(b"\x1b[");
                seq.extend_from_slice(&self.params);
                seq.push(b'q');
                self.cursor_style = Some(seq);
            }
            b'n' => {
                if self.params == b"6" {
                    queries.push(Query::CursorPosition);
                }
            }
            b'c' => {
                if self.params.is_empty() || self.params == b"0" {
                    queries.push(Query::DeviceAttributes);
                }
            }
            _ => {}
        }
    }
}

fn parse_csi_numbers(params: &[u8]) -> Vec<u16> {
    params
        .split(|b| *b == b';')
        .filter_map(|part| std::str::from_utf8(part).ok()?.parse().ok())
        .collect()
}

struct Snapshot {
    rows: u16,
    cols: u16,
    data: Vec<u8>,
}

struct TerminalState {
    parser: vt100::Parser,
    output: OutputLog,
    scrollback: Scrollback,
    tracker: StreamTracker,
}

impl TerminalState {
    fn snapshot(&self) -> Snapshot {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let mut data = Vec::with_capacity(8192);
        data.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[3J\x1b[H");
        data.extend_from_slice(
            &self
                .scrollback
                .older_history(rows as usize, SNAPSHOT_HISTORY_BYTES),
        );
        if screen.alternate_screen() {
            data.extend_from_slice(b"\x1b[?1049h");
        }
        data.extend_from_slice(&screen.state_formatted());
        if self.tracker.focus_reporting {
            data.extend_from_slice(b"\x1b[?1004h");
        }
        if let Some(style) = &self.tracker.cursor_style {
            data.extend_from_slice(style);
        }
        Snapshot { rows, cols, data }
    }
}

/* ---------- 输入队列与控制权 ---------- */

struct InputQueue {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
}

struct InputQueueState {
    queue: Mutex<InputQueue>,
    ready: Condvar,
}

impl InputQueueState {
    fn new() -> Self {
        Self {
            queue: Mutex::new(InputQueue {
                chunks: VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    /// 非阻塞入队；队列满或已关闭时返回 false（调用方回 busy / 丢弃）。
    fn try_push(&self, data: Vec<u8>) -> bool {
        let mut q = self.queue.lock().unwrap();
        if q.closed || q.bytes + data.len() > MAX_INPUT_QUEUE_BYTES {
            return false;
        }
        q.bytes += data.len();
        q.chunks.push_back(data);
        drop(q);
        self.ready.notify_one();
        true
    }

    fn pop_blocking(&self) -> Option<Vec<u8>> {
        let mut q = self.queue.lock().unwrap();
        loop {
            if let Some(chunk) = q.chunks.pop_front() {
                q.bytes -= chunk.len();
                return Some(chunk);
            }
            if q.closed {
                return None;
            }
            q = self.ready.wait(q).unwrap();
        }
    }

    fn close(&self) {
        let mut q = self.queue.lock().unwrap();
        q.closed = true;
        q.chunks.clear();
        q.bytes = 0;
        drop(q);
        self.ready.notify_all();
    }
}

struct InputStream {
    next: u64,
    last_seen: Instant,
}

#[derive(Default)]
struct ControlState {
    controller: Option<Uuid>,
    attached: std::collections::HashSet<Uuid>,
    grace_deadline: Option<Instant>,
}

pub enum InputOutcome {
    Ack {
        next: u64,
    },
    Rejected {
        code: &'static str,
        message: String,
        next: u64,
    },
}

/* ---------- Session ---------- */

struct Session {
    id: Uuid,
    title: Mutex<String>,
    created_unix_ms: u64,
    dims: Mutex<(u16, u16)>,
    live: AtomicBool,
    state: Mutex<TerminalState>,
    output_signal: watch::Sender<OutputSignal>,
    output_done: AtomicBool,
    input_streams: Mutex<HashMap<Uuid, InputStream>>,
    input_queue: InputQueueState,
    pty_writer: Mutex<Option<Box<dyn Write + Send>>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    control: Mutex<ControlState>,
    events: broadcast::Sender<Event>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct OutputSignal {
    end: u64,
    done: bool,
}

impl Session {
    fn info(&self) -> SessionInfo {
        let (rows, cols) = *self.dims.lock().unwrap();
        SessionInfo {
            id: self.id,
            title: self.title.lock().unwrap().clone(),
            created_unix_ms: self.created_unix_ms,
            live: self.live.load(Ordering::SeqCst),
            rows,
            cols,
            controller: self.control.lock().unwrap().controller,
        }
    }

    fn broadcast(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn controller_online(&self) -> bool {
        let control = self.control.lock().unwrap();
        match control.controller {
            Some(stream) => control.attached.contains(&stream),
            None => false,
        }
    }

    fn expire_control_grace(&self) {
        let mut control = self.control.lock().unwrap();
        let expired = match (control.controller, control.grace_deadline) {
            (Some(stream), Some(deadline))
                if !control.attached.contains(&stream) && Instant::now() >= deadline =>
            {
                control.controller = None;
                control.grace_deadline = None;
                true
            }
            _ => false,
        };
        drop(control);
        if expired {
            self.broadcast(Event::ControlChanged {
                session_id: self.id,
                controller: None,
            });
        }
    }

    fn close_input(&self) {
        self.input_queue.close();
    }

    fn close_pty(&self) {
        // 先从锁里取出再 drop，避免持锁调用可能阻塞的 ClosePseudoConsole。
        self.close_input();
        let writer = self.pty_writer.lock().unwrap().take();
        let master = self.master.lock().unwrap().take();
        drop(writer);
        drop(master);
    }

    fn resize_pty_only(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .lock()
            .unwrap()
            .as_ref()
            .ok_or_else(|| anyhow!("session is not live"))?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow!("pty resize failed: {e}"))
    }

    fn jiggle_resize(&self) {
        let (rows, cols) = *self.dims.lock().unwrap();
        if cols <= 2 || !self.live.load(Ordering::SeqCst) {
            return;
        }
        if self.resize_pty_only(rows, cols - 1).is_err() {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.parser.screen_mut().set_size(rows, cols - 1);
        }
        std::thread::sleep(Duration::from_millis(120));
        if self.resize_pty_only(rows, cols).is_err() {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.parser.screen_mut().set_size(rows, cols);
        }
    }
}

/* ---------- 订阅 ---------- */

pub struct Subscription {
    session: Arc<Session>,
    pos: u64,
    need_snapshot: bool,
    signal: watch::Receiver<OutputSignal>,
}

pub enum Chunk {
    Output {
        offset: u64,
        data: Vec<u8>,
    },
    Snapshot {
        offset: u64,
        rows: u16,
        cols: u16,
        data: Vec<u8>,
    },
    Ended,
}

impl Subscription {
    pub fn pos(&self) -> u64 {
        self.pos
    }

    /// true 表示按 `resume_from` 重放；false 表示接下来先发快照。
    pub fn is_resumed(&self) -> bool {
        !self.need_snapshot
    }

    /// 下一个要让客户端看到的块。落后于输出环时自动转为快照。
    pub async fn next(&mut self) -> Chunk {
        loop {
            if let Some(chunk) = self.try_next() {
                return chunk;
            }
            if self.session.output_done.load(Ordering::SeqCst)
                && self.pos >= self.session.state.lock().unwrap().output.end
            {
                return Chunk::Ended;
            }
            if self.signal.changed().await.is_err() {
                return Chunk::Ended;
            }
        }
    }

    fn try_next(&mut self) -> Option<Chunk> {
        let state = self.session.state.lock().unwrap();
        if self.need_snapshot || self.pos < state.output.start {
            let snapshot = state.snapshot();
            self.pos = state.output.end;
            self.need_snapshot = false;
            return Some(Chunk::Snapshot {
                offset: self.pos,
                rows: snapshot.rows,
                cols: snapshot.cols,
                data: snapshot.data,
            });
        }
        if self.pos < state.output.end {
            let data = state.output.read(self.pos, MAX_OUTPUT_CHUNK);
            let offset = self.pos;
            self.pos += data.len() as u64;
            return Some(Chunk::Output { offset, data });
        }
        None
    }
}

fn subscription_for(session: &Arc<Session>, from: Option<u64>) -> Subscription {
    let mut subscription = Subscription {
        session: Arc::clone(session),
        pos: 0,
        need_snapshot: true,
        signal: session.output_signal.subscribe(),
    };
    if let Some(from) = from {
        let state = session.state.lock().unwrap();
        if state.output.start <= from && from <= state.output.end {
            subscription.pos = from;
            subscription.need_snapshot = false;
        }
    }
    subscription
}

/// 控制者以快照挂接、且当前处于备用屏时，抖动一次尺寸迫使全屏程序重绘（3.3）。
fn start_resize_jiggle(session: &Arc<Session>) {
    let alternate = session
        .state
        .lock()
        .map(|state| state.parser.screen().alternate_screen())
        .unwrap_or(false);
    if !alternate {
        return;
    }
    let session = Arc::clone(session);
    std::thread::spawn(move || {
        // 等快照生成并开始发送之后再抖动，避免重绘输出落在快照 offset 之前。
        std::thread::sleep(Duration::from_millis(120));
        session.jiggle_resize();
    });
}

/* ---------- SessionManager ---------- */

fn validate_dims(rows: u16, cols: u16) -> Result<()> {
    if rows < 2 || cols < 2 || rows > MAX_ROWS || cols > MAX_COLS {
        bail!(
            "invalid terminal dimensions {}x{} (need 2..={} rows, 2..={} cols)",
            rows,
            cols,
            MAX_ROWS,
            MAX_COLS
        );
    }
    Ok(())
}

fn shell_command() -> CommandBuilder {
    #[cfg(windows)]
    {
        let mut cmd = CommandBuilder::new("powershell.exe");
        cmd.arg("-NoLogo");
        cmd
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        CommandBuilder::new(shell)
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl SessionManager {
    pub fn new() -> Result<Self> {
        Self::with_control_grace(DEFAULT_CONTROL_GRACE)
    }

    pub fn with_control_grace(grace: Duration) -> Result<Self> {
        let inner = Arc::new(Inner {
            sessions: Mutex::new(HashMap::new()),
            control_grace: grace,
            #[cfg(windows)]
            job: WinJob::new()?,
        });
        let weak = Arc::downgrade(&inner);
        let interval = (grace / 4).clamp(Duration::from_millis(20), Duration::from_secs(1));
        std::thread::spawn(move || loop {
            std::thread::sleep(interval);
            let Some(inner) = weak.upgrade() else {
                break;
            };
            let sessions: Vec<Arc<Session>> =
                inner.sessions.lock().unwrap().values().cloned().collect();
            for session in sessions {
                session.expire_control_grace();
            }
        });
        Ok(Self { inner })
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        self.inner
            .sessions
            .lock()
            .unwrap()
            .values()
            .map(|s| s.info())
            .collect()
    }

    pub fn create(&self, title: String, rows: u16, cols: u16) -> Result<SessionInfo> {
        validate_dims(rows, cols)?;

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut child = pair.slave.spawn_command(shell_command())?;
        #[cfg(windows)]
        {
            let process = child
                .as_raw_handle()
                .ok_or_else(|| anyhow!("child has no process handle"))?;
            if let Err(err) = self.inner.job.assign(process) {
                let _ = child.kill();
                return Err(
                    err.context("cannot bind terminal process to kill-on-close Windows job")
                );
            }
        }
        let killer = child.clone_killer();
        // Drop the slave handle so the pty can report EOF when the child exits.
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow!("failed to take pty writer: {e}"))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow!("failed to clone pty reader: {e}"))?;

        let session = self.spawn_session(
            title,
            rows,
            cols,
            writer,
            reader,
            Some(pair.master),
            Some(killer),
        );
        let id = session.id;
        let inner = Arc::downgrade(&self.inner);
        // 进程退出后释放 PTY；读线程排空剩余输出后自行广播 Ended。
        let waiter_session = Arc::clone(&session);
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
            waiter_session.live.store(false, Ordering::SeqCst);
            waiter_session.close_pty();
            if let Some(inner) = inner.upgrade() {
                inner.sessions.lock().unwrap().remove(&id);
            }
        });

        Ok(session.info())
    }

    fn spawn_session(
        &self,
        title: String,
        rows: u16,
        cols: u16,
        writer: Box<dyn Write + Send>,
        reader: Box<dyn Read + Send>,
        master: Option<Box<dyn MasterPty + Send>>,
        killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    ) -> Arc<Session> {
        let id = Uuid::new_v4();
        let (output_signal, _) = watch::channel(OutputSignal {
            end: 0,
            done: false,
        });
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let session = Arc::new(Session {
            id,
            title: Mutex::new(title),
            created_unix_ms: now_unix_ms(),
            dims: Mutex::new((rows, cols)),
            live: AtomicBool::new(true),
            state: Mutex::new(TerminalState {
                parser: vt100::Parser::new(rows, cols, 0),
                output: OutputLog::default(),
                scrollback: Scrollback::new(),
                tracker: StreamTracker::default(),
            }),
            output_signal,
            output_done: AtomicBool::new(false),
            input_streams: Mutex::new(HashMap::new()),
            input_queue: InputQueueState::new(),
            pty_writer: Mutex::new(Some(writer)),
            master: Mutex::new(master),
            killer: Mutex::new(killer),
            control: Mutex::new(ControlState::default()),
            events,
        });

        self.inner
            .sessions
            .lock()
            .unwrap()
            .insert(id, Arc::clone(&session));

        {
            let session = Arc::clone(&session);
            std::thread::spawn(move || writer_loop(&session));
        }
        {
            let session = Arc::clone(&session);
            std::thread::spawn(move || {
                let mut reader = reader;
                reader_loop(&session, reader.as_mut());
            });
        }
        session
    }

    pub fn attach(
        &self,
        session_id: Uuid,
        stream_id: Uuid,
        input_base: u64,
        resume_from: Option<u64>,
    ) -> Result<Attachment> {
        let session = self.get(session_id)?;
        let (has_control, gained, controller) = {
            let mut control = session.control.lock().unwrap();
            control.attached.insert(stream_id);
            let gained = match control.controller {
                Some(current) if current == stream_id => {
                    control.grace_deadline = None;
                    true
                }
                Some(_) => false,
                None => {
                    control.controller = Some(stream_id);
                    control.grace_deadline = None;
                    true
                }
            };
            (
                control.controller == Some(stream_id),
                gained,
                control.controller,
            )
        };
        let input_next = {
            let mut streams = session.input_streams.lock().unwrap();
            if !streams.contains_key(&stream_id) && streams.len() >= MAX_INPUT_STREAMS {
                let victim = streams
                    .iter()
                    .filter(|(id, _)| Some(**id) != controller)
                    .min_by_key(|(_, entry)| entry.last_seen)
                    .map(|(id, _)| *id)
                    .or_else(|| {
                        streams
                            .iter()
                            .min_by_key(|(_, entry)| entry.last_seen)
                            .map(|(id, _)| *id)
                    });
                if let Some(victim) = victim {
                    streams.remove(&victim);
                }
            }
            let entry = streams.entry(stream_id).or_insert_with(|| InputStream {
                next: input_base,
                last_seen: Instant::now(),
            });
            entry.last_seen = Instant::now();
            entry.next
        };

        let subscription = subscription_for(&session, resume_from);
        let resumed = subscription.is_resumed();
        if !resumed && has_control {
            // 备用屏 + 快照挂接：做一次尺寸抖动，迫使全屏程序完整重绘（3.3）。
            start_resize_jiggle(&session);
        }
        let events = session.events.subscribe();
        if gained {
            session.broadcast(Event::ControlChanged {
                session_id,
                controller: Some(stream_id),
            });
        }
        Ok(Attachment {
            session: session.info(),
            has_control,
            input_next,
            resumed,
            subscription,
            events,
        })
    }

    /// 游标式订阅：`from` 在输出环内则重放，否则由订阅先发快照。
    pub fn subscribe(&self, session_id: Uuid, from: Option<u64>) -> Result<Subscription> {
        let session = self.get(session_id)?;
        Ok(subscription_for(&session, from))
    }

    pub fn detach(&self, session_id: Uuid, stream_id: Uuid) -> Result<()> {
        let session = self.get(session_id)?;
        let mut control = session.control.lock().unwrap();
        if !control.attached.remove(&stream_id) {
            bail!("stream is not attached to this session");
        }
        if control.controller == Some(stream_id) {
            // 保留控制权一个宽限期；由回收线程在到期后广播释放。
            control.grace_deadline = Some(Instant::now() + self.inner.control_grace);
        }
        Ok(())
    }

    pub fn take_control(&self, session_id: Uuid, stream_id: Uuid) -> Result<()> {
        let session = self.get(session_id)?;
        let changed = {
            let mut control = session.control.lock().unwrap();
            if !control.attached.contains(&stream_id) {
                bail!("stream is not attached to this session");
            }
            let changed = control.controller != Some(stream_id);
            control.controller = Some(stream_id);
            control.grace_deadline = None;
            changed
        };
        if changed {
            session.broadcast(Event::ControlChanged {
                session_id,
                controller: Some(stream_id),
            });
        }
        Ok(())
    }

    pub fn input(
        &self,
        session_id: Uuid,
        stream_id: Uuid,
        offset: u64,
        data: &[u8],
    ) -> InputOutcome {
        let session = match self.inner.sessions.lock().unwrap().get(&session_id) {
            Some(session) => Arc::clone(session),
            None => {
                return InputOutcome::Rejected {
                    code: "session_not_found",
                    message: "会话不存在或已结束".into(),
                    next: 0,
                }
            }
        };
        let next_of = |session: &Session| {
            session
                .input_streams
                .lock()
                .map(|streams| streams.get(&stream_id).map(|entry| entry.next).unwrap_or(0))
                .unwrap_or(0)
        };
        if !session.live.load(Ordering::SeqCst) {
            return InputOutcome::Rejected {
                code: "session_not_live",
                message: "会话已结束".into(),
                next: next_of(&session),
            };
        }
        let n = data.len();
        if n == 0 || n > MAX_INPUT_CHUNK {
            return InputOutcome::Rejected {
                code: "invalid_input",
                message: format!("输入长度必须在 1..={MAX_INPUT_CHUNK} 字节之间"),
                next: next_of(&session),
            };
        }
        let is_controller = {
            let control = session.control.lock().unwrap();
            control.controller == Some(stream_id) && control.attached.contains(&stream_id)
        };
        if !is_controller {
            return InputOutcome::Rejected {
                code: "not_controller",
                message: "只有控制者可以输入".into(),
                next: next_of(&session),
            };
        }
        let mut streams = session.input_streams.lock().unwrap();
        let Some(entry) = streams.get_mut(&stream_id) else {
            return InputOutcome::Rejected {
                code: "not_controller",
                message: "该输入流不在会话中".into(),
                next: 0,
            };
        };
        entry.last_seen = Instant::now();
        let next = entry.next;
        if offset.saturating_add(n as u64) <= next {
            return InputOutcome::Ack { next };
        }
        if offset > next {
            return InputOutcome::Rejected {
                code: "input_gap",
                message: format!("输入缺口：期望偏移 {next}，收到 {offset}"),
                next,
            };
        }
        let skip = (next - offset) as usize;
        if !session.input_queue.try_push(data[skip..].to_vec()) {
            return InputOutcome::Rejected {
                code: "busy",
                message: "输入队列已满，请稍后按偏移重发".into(),
                next,
            };
        }
        entry.next = offset + n as u64;
        InputOutcome::Ack { next: entry.next }
    }

    pub fn resize(&self, session_id: Uuid, stream_id: Uuid, rows: u16, cols: u16) -> Result<()> {
        validate_dims(rows, cols)?;
        let session = self.get(session_id)?;
        self.require_controller(&session, stream_id)?;
        session.resize_pty_only(rows, cols)?;
        *session.dims.lock().unwrap() = (rows, cols);
        session
            .state
            .lock()
            .unwrap()
            .parser
            .screen_mut()
            .set_size(rows, cols);
        session.broadcast(Event::Resized {
            session_id,
            rows,
            cols,
        });
        Ok(())
    }

    pub fn end_for(&self, session_id: Uuid, stream_id: Uuid) -> Result<()> {
        let session = self.get(session_id)?;
        self.require_controller(&session, stream_id)?;
        self.end(session_id)
    }

    pub fn end(&self, session_id: Uuid) -> Result<()> {
        let session = {
            let mut sessions = self.inner.sessions.lock().unwrap();
            sessions.remove(&session_id)
        }
        .ok_or_else(|| anyhow!("session not found or already ended"))?;
        Self::kill(&session);
        Ok(())
    }

    /// Kill all remaining children. Call on host process shutdown to avoid orphans.
    pub fn shutdown(&self) {
        let sessions: Vec<Arc<Session>> = {
            let mut map = self.inner.sessions.lock().unwrap();
            map.drain().map(|(_, s)| s).collect()
        };
        for session in sessions {
            Self::kill(&session);
        }
    }

    fn kill(session: &Session) {
        if let Some(killer) = session.killer.lock().unwrap().as_mut() {
            let _ = killer.kill();
        }
        session.live.store(false, Ordering::SeqCst);
        // 关闭输入队列，写线程退出；读取端随 PTY 关闭自然 EOF。
        session.close_input();
    }

    fn get(&self, session_id: Uuid) -> Result<Arc<Session>> {
        self.inner
            .sessions
            .lock()
            .unwrap()
            .get(&session_id)
            .map(Arc::clone)
            .ok_or_else(|| anyhow!("session not found or already ended"))
    }

    fn require_controller(&self, session: &Session, stream_id: Uuid) -> Result<()> {
        let control = session.control.lock().unwrap();
        if control.controller != Some(stream_id) || !control.attached.contains(&stream_id) {
            bail!("stream is not the controlling client of this session");
        }
        Ok(())
    }
}

impl Drop for SessionManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub struct Attachment {
    pub session: SessionInfo,
    pub has_control: bool,
    pub input_next: u64,
    pub resumed: bool,
    pub subscription: Subscription,
    pub events: broadcast::Receiver<Event>,
}

fn writer_loop(session: &Session) {
    loop {
        let Some(chunk) = session.input_queue.pop_blocking() else {
            break;
        };
        let mut guard = session.pty_writer.lock().unwrap();
        let Some(writer) = guard.as_mut() else {
            break;
        };
        if writer
            .write_all(&chunk)
            .and_then(|_| writer.flush())
            .is_err()
        {
            drop(guard);
            session.close_input();
            break;
        }
    }
}

fn reader_loop(session: &Session, reader: &mut dyn Read) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let (end, replies) = {
                    let mut state = session.state.lock().unwrap();
                    state.parser.process(&buf[..n]);
                    state.output.append(&buf[..n]);
                    state.scrollback.push_bytes(&buf[..n]);
                    let queries = state.tracker.scan(&buf[..n]);
                    let mut replies = Vec::new();
                    for query in queries {
                        match query {
                            Query::CursorPosition => {
                                let (row, col) = state.parser.screen().cursor_position();
                                replies.push(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
                            }
                            Query::DeviceAttributes => {
                                replies.push(b"\x1b[?1;2c".to_vec());
                            }
                        }
                    }
                    (state.output.end, replies)
                };
                let _ = session
                    .output_signal
                    .send(OutputSignal { end, done: false });
                // 没有在线控制者时由接收端代答，否则原样转发给控制者的终端。
                if !replies.is_empty() && !session.controller_online() {
                    for reply in replies {
                        let _ = session.input_queue.try_push(reply);
                    }
                }
            }
            Err(_) => break,
        }
    }
    session.output_done.store(true, Ordering::SeqCst);
    let end = session.state.lock().unwrap().output.end;
    let _ = session.output_signal.send(OutputSignal { end, done: true });
    session.broadcast(Event::Ended {
        session_id: session.id,
    });
}

/* ---------- 测试 ---------- */

#[cfg(test)]
mod tests {
    use super::*;

    struct SinkWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for SinkWriter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.buf.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct ChannelReader {
        rx: std::sync::mpsc::Receiver<Vec<u8>>,
        buf: VecDeque<u8>,
    }

    impl Read for ChannelReader {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            while self.buf.is_empty() {
                match self.rx.recv() {
                    Ok(chunk) => self.buf.extend(chunk),
                    Err(_) => return Ok(0),
                }
            }
            let n = out.len().min(self.buf.len());
            for (i, byte) in self.buf.drain(..n).enumerate() {
                out[i] = byte;
            }
            Ok(n)
        }
    }

    struct FakeSession {
        session: Arc<Session>,
        feed: std::sync::mpsc::Sender<Vec<u8>>,
        written: Arc<Mutex<Vec<u8>>>,
    }

    fn fake_session(mgr: &SessionManager) -> FakeSession {
        let (feed, rx) = std::sync::mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let session = mgr.spawn_session(
            "test".into(),
            24,
            80,
            Box::new(SinkWriter {
                buf: written.clone(),
            }),
            Box::new(ChannelReader {
                rx,
                buf: VecDeque::new(),
            }),
            None,
            None,
        );
        FakeSession {
            session,
            feed,
            written,
        }
    }

    fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        predicate()
    }

    fn written_bytes(fake: &FakeSession) -> Vec<u8> {
        fake.written.lock().unwrap().clone()
    }

    /// 等 ConPTY 启动查询（CSI 6n，4 字节）被接收端代答且 shell 已有输出，
    /// 再挂接控制者，避免查询在挂接后被转发而无人应答。
    /// Unix PTY 没有启动查询，只要出现任何输出（提示符）即可。
    fn wait_shell_ready(session: &Session) {
        let deadline = Instant::now() + Duration::from_secs(10);
        #[cfg(windows)]
        let threshold = 4;
        #[cfg(not(windows))]
        let threshold = 0;
        while Instant::now() < deadline {
            if session.state.lock().unwrap().output.end > threshold {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    async fn collect_output_until(
        subscription: &mut Subscription,
        needle: &str,
        timeout: Duration,
    ) -> (bool, String) {
        let mut out: Vec<u8> = Vec::new();
        let found = tokio::time::timeout(timeout, async {
            loop {
                match subscription.next().await {
                    Chunk::Output { data, .. } | Chunk::Snapshot { data, .. } => {
                        out.extend_from_slice(&data);
                        if String::from_utf8_lossy(&out).contains(needle) {
                            break true;
                        }
                    }
                    Chunk::Ended => break false,
                }
            }
        })
        .await
        .unwrap_or(false);
        (found, String::from_utf8_lossy(&out).into_owned())
    }

    #[tokio::test]
    async fn input_offsets_are_exactly_once() {
        let mgr = SessionManager::new().unwrap();
        let fake = fake_session(&mgr);
        let stream = Uuid::new_v4();
        let attachment = mgr.attach(fake.session.id, stream, 0, None).unwrap();
        assert!(attachment.has_control);
        assert_eq!(attachment.input_next, 0);

        assert!(matches!(
            mgr.input(fake.session.id, stream, 0, b"hello"),
            InputOutcome::Ack { next: 5 }
        ));
        // 整段重复：丢弃，仍回 ACK。
        assert!(matches!(
            mgr.input(fake.session.id, stream, 0, b"hello"),
            InputOutcome::Ack { next: 5 }
        ));
        // 部分重叠：只写入新后缀 "!!"。
        assert!(matches!(
            mgr.input(fake.session.id, stream, 3, b"lo!!"),
            InputOutcome::Ack { next: 7 }
        ));
        // 完全落在已确认范围之内。
        assert!(matches!(
            mgr.input(fake.session.id, stream, 2, b"XYZ"),
            InputOutcome::Ack { next: 7 }
        ));
        // 缺口被拒绝，next 不前进。
        match mgr.input(fake.session.id, stream, 10, b"abc") {
            InputOutcome::Rejected { code, next, .. } => {
                assert_eq!(code, "input_gap");
                assert_eq!(next, 7);
            }
            _ => panic!("gap was not rejected"),
        }
        assert!(matches!(
            mgr.input(fake.session.id, stream, 7, b"!"),
            InputOutcome::Ack { next: 8 }
        ));
        assert!(wait_until(Duration::from_secs(2), || written_bytes(&fake)
            == b"hello!!!"));
    }

    #[tokio::test]
    async fn observer_input_rejected() {
        let mgr = SessionManager::new().unwrap();
        let fake = fake_session(&mgr);
        let controller = Uuid::new_v4();
        let observer = Uuid::new_v4();
        assert!(
            mgr.attach(fake.session.id, controller, 0, None)
                .unwrap()
                .has_control
        );
        assert!(
            !mgr.attach(fake.session.id, observer, 0, None)
                .unwrap()
                .has_control
        );

        match mgr.input(fake.session.id, observer, 0, b"rm -rf /") {
            InputOutcome::Rejected { code, .. } => assert_eq!(code, "not_controller"),
            _ => panic!("observer input was accepted"),
        }
        std::thread::sleep(Duration::from_millis(100));
        assert!(written_bytes(&fake).is_empty());
        // 控制者仍然可以输入。
        assert!(matches!(
            mgr.input(fake.session.id, controller, 0, b"ok"),
            InputOutcome::Ack { next: 2 }
        ));
        assert!(wait_until(Duration::from_secs(2), || written_bytes(&fake) == b"ok"));
    }

    #[tokio::test]
    async fn input_stream_table_evicts_oldest_observer() {
        let mgr = SessionManager::new().unwrap();
        let fake = fake_session(&mgr);
        let controller = Uuid::new_v4();
        mgr.attach(fake.session.id, controller, 0, None).unwrap();
        let mut observers = Vec::new();
        for _ in 0..MAX_INPUT_STREAMS - 1 {
            let stream = Uuid::new_v4();
            mgr.attach(fake.session.id, stream, 0, None).unwrap();
            observers.push(stream);
        }
        // 表已满：再挂接一个新流，最久未用的观察者条目被淘汰。
        let newcomer = Uuid::new_v4();
        mgr.attach(fake.session.id, newcomer, 0, None).unwrap();
        // 被淘汰的流以 input_base=7 重挂时会被当作新流。
        let attachment = mgr.attach(fake.session.id, observers[0], 7, None).unwrap();
        assert_eq!(attachment.input_next, 7);
        // 控制者条目不会被淘汰：>64 个新流之后仍保留原偏移。
        for _ in 0..MAX_INPUT_STREAMS + 2 {
            mgr.attach(fake.session.id, Uuid::new_v4(), 0, None)
                .unwrap();
        }
        let attachment = mgr.attach(fake.session.id, controller, 7, None).unwrap();
        assert_eq!(attachment.input_next, 0);
    }

    #[tokio::test]
    async fn slow_subscriber_gets_snapshot_not_block() {
        let mgr = SessionManager::new().unwrap();
        let fake = fake_session(&mgr);
        let fast_stream = Uuid::new_v4();
        let slow_stream = Uuid::new_v4();
        let mut fast = mgr
            .attach(fake.session.id, fast_stream, 0, Some(0))
            .unwrap()
            .subscription;
        let mut slow = mgr
            .attach(fake.session.id, slow_stream, 0, Some(0))
            .unwrap()
            .subscription;

        // 5 MiB 输出，超过 4 MiB 输出环。
        const CHUNK: usize = 4096;
        const TOTAL_CHUNKS: usize = 5 * 1024 * 1024 / CHUNK;
        let total = (CHUNK * TOTAL_CHUNKS) as u64;
        let feed = fake.feed.clone();
        let feeder = std::thread::spawn(move || {
            for _ in 0..TOTAL_CHUNKS {
                if feed.send(vec![b'a'; CHUNK]).is_err() {
                    return;
                }
            }
        });

        // fast 订阅者持续读取；读线程不能被 slow 订阅者拖住。
        let mut seen_last = 0u64;
        let mut last_end = None;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match fast.next().await {
                    Chunk::Output { offset, data } => {
                        if let Some(prev_end) = last_end {
                            assert_eq!(offset, prev_end, "fast 订阅者偏移不连续");
                        }
                        last_end = Some(offset + data.len() as u64);
                        seen_last = offset + data.len() as u64;
                        if seen_last == total {
                            break;
                        }
                    }
                    Chunk::Snapshot { offset, data, .. } => {
                        last_end = Some(offset + data.len() as u64);
                        seen_last = offset + data.len() as u64;
                    }
                    Chunk::Ended => break,
                }
            }
        })
        .await
        .expect("fast subscriber stalled while a slow subscriber idled");
        feeder.join().unwrap();
        assert_eq!(seen_last, total);

        // 慢订阅者此时落后于输出环：先收到快照，随后是连续实时输出。
        let first = slow.next().await;
        let snapshot_offset = match first {
            Chunk::Snapshot { offset, .. } => offset,
            _ => panic!("slow subscriber did not receive a snapshot"),
        };
        assert!(snapshot_offset > 0);
        fake.feed.send(b"tail".to_vec()).unwrap();
        let mut got_tail = Vec::new();
        let mut expected = snapshot_offset;
        tokio::time::timeout(Duration::from_secs(5), async {
            while got_tail.len() < 4 {
                match slow.next().await {
                    Chunk::Output { offset, data } => {
                        assert_eq!(offset, expected, "慢订阅者恢复后偏移不连续");
                        expected += data.len() as u64;
                        got_tail.extend_from_slice(&data);
                    }
                    Chunk::Snapshot { offset, data, .. } => {
                        expected = offset + data.len() as u64;
                    }
                    Chunk::Ended => break,
                }
            }
        })
        .await
        .expect("slow subscriber did not catch up after the snapshot");
        assert_eq!(got_tail, b"tail");
    }

    #[tokio::test]
    async fn resume_replays_from_offset() {
        let mgr = SessionManager::new().unwrap();
        let fake = fake_session(&mgr);
        let stream = Uuid::new_v4();
        let mut first = mgr
            .attach(fake.session.id, stream, 0, Some(0))
            .unwrap()
            .subscription;
        let prefix: Vec<u8> = (0..1000u32).map(|i| b'0' + (i % 10) as u8).collect();
        fake.feed.send(prefix.clone()).unwrap();
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while received.len() < prefix.len() {
                match first.next().await {
                    Chunk::Output { data, .. } => received.extend_from_slice(&data),
                    Chunk::Snapshot { data, .. } => received.extend_from_slice(&data),
                    Chunk::Ended => break,
                }
            }
        })
        .await
        .expect("first subscriber did not receive the prefix");
        assert_eq!(received, prefix);
        let resume_point = prefix.len() as u64;
        drop(first);

        let suffix: Vec<u8> = b"suffix-bytes-after-the-resume-point".to_vec();
        fake.feed.send(suffix.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let attachment = mgr
            .attach(fake.session.id, stream, 0, Some(resume_point))
            .unwrap();
        assert!(attachment.resumed, "expected replay instead of snapshot");
        let mut second = attachment.subscription;
        let mut replayed = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while replayed.len() < suffix.len() {
                match second.next().await {
                    Chunk::Output { offset, data } => {
                        assert_eq!(offset, resume_point + replayed.len() as u64);
                        replayed.extend_from_slice(&data);
                    }
                    Chunk::Snapshot { .. } => panic!("unexpected snapshot after resume"),
                    Chunk::Ended => break,
                }
            }
        })
        .await
        .expect("resumed subscriber did not replay the suffix");
        assert_eq!(replayed, suffix);
    }

    #[tokio::test]
    async fn snapshot_marks_alternate_screen_and_modes() {
        let mgr = SessionManager::new().unwrap();
        let fake = fake_session(&mgr);
        let payload = b"\x1b[?1049h\x1b[?1004h\x1b[2 qhi".to_vec();
        fake.feed.send(payload.clone()).unwrap();
        assert!(wait_until(Duration::from_secs(5), || {
            fake.session.state.lock().unwrap().output.end == payload.len() as u64
        }));

        let mut subscription = mgr
            .attach(fake.session.id, Uuid::new_v4(), 0, None)
            .unwrap()
            .subscription;
        let snapshot = match subscription.next().await {
            Chunk::Snapshot { data, .. } => data,
            _ => panic!("first chunk was not a snapshot"),
        };
        let text = String::from_utf8_lossy(&snapshot).into_owned();
        assert!(
            text.contains("\x1b[?1049h"),
            "alternate screen marker missing"
        );
        assert!(text.contains("\x1b[?1004h"), "focus reporting not restored");
        assert!(text.contains("\x1b[2 q"), "cursor style not restored");
        assert!(text.contains("hi"), "screen contents missing");
    }

    #[tokio::test]
    async fn controller_grace_period() {
        let mgr = SessionManager::with_control_grace(Duration::from_millis(200)).unwrap();
        let fake = fake_session(&mgr);
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let third = Uuid::new_v4();

        assert!(
            mgr.attach(fake.session.id, first, 0, None)
                .unwrap()
                .has_control
        );
        mgr.detach(fake.session.id, first).unwrap();
        // 宽限期内其他 stream 挂接不会自动获得控制权。
        assert!(
            !mgr.attach(fake.session.id, second, 0, None)
                .unwrap()
                .has_control
        );
        // 同一 stream 重新挂接立即恢复控制。
        assert!(
            mgr.attach(fake.session.id, first, 0, None)
                .unwrap()
                .has_control
        );
        mgr.detach(fake.session.id, first).unwrap();
        // 宽限期结束后释放控制权，下一次挂接可取得。
        assert!(wait_until(Duration::from_secs(5), || {
            mgr.list()[0].controller.is_none()
        }));
        assert!(
            mgr.attach(fake.session.id, third, 0, None)
                .unwrap()
                .has_control
        );
    }

    #[tokio::test]
    async fn ctrl_c_interrupts() {
        let mgr = SessionManager::new().unwrap();
        let info = mgr.create("test".into(), 24, 80).unwrap();
        let session = mgr.get(info.id).unwrap();
        wait_shell_ready(&session);
        let stream = Uuid::new_v4();
        let attachment = mgr.attach(info.id, stream, 0, None).unwrap();
        assert!(attachment.has_control);
        let mut subscription = attachment.subscription;

        #[cfg(windows)]
        let long_running = "ping -t 127.0.0.1";
        #[cfg(not(windows))]
        let long_running = "sleep 100";
        // "TTL" 只出现在 ping 的回复里，而不是命令行回显里（中英文区域设置均适用）。
        #[cfg(windows)]
        let running_marker = "TTL";
        #[cfg(not(windows))]
        let running_marker = long_running;
        let mut offset = 0u64;
        let command = format!("{long_running}\r");
        assert!(matches!(
            mgr.input(info.id, stream, offset, command.as_bytes()),
            InputOutcome::Ack { .. }
        ));
        offset += command.len() as u64;

        let (started, seen) =
            collect_output_until(&mut subscription, running_marker, Duration::from_secs(15)).await;
        assert!(started, "long-running command did not start: {seen}");

        assert!(matches!(
            mgr.input(info.id, stream, offset, b"\x03"),
            InputOutcome::Ack { .. }
        ));
        offset += 1;

        #[cfg(windows)]
        let marker_command = "Write-Output (\"TB_CTRLC_\" + \"OK731\")\r";
        #[cfg(not(windows))]
        let marker_command = "printf 'TB_CTRLC_%s\\n' OK731\r";
        // 命令文本本身不含连续标记（括号/百分号拆开），只有执行结果才会出现。
        // Windows 控制台在 Ctrl+C 时会清空尚未处理的输入，因此允许重发几次。
        let mut found = false;
        let mut output = String::new();
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert!(matches!(
                mgr.input(info.id, stream, offset, marker_command.as_bytes()),
                InputOutcome::Ack { .. }
            ));
            offset += marker_command.len() as u64;
            let (ok, seen) =
                collect_output_until(&mut subscription, "TB_CTRLC_OK731", Duration::from_secs(5))
                    .await;
            output.push_str(&seen);
            if ok {
                found = true;
                break;
            }
        }
        assert!(found, "shell did not execute after Ctrl+C: {output}");
        mgr.end(info.id).unwrap();
    }

    #[tokio::test]
    async fn shell_exit_produces_ended_and_reaps_session() {
        let mgr = SessionManager::new().unwrap();
        let info = mgr.create("exit".into(), 24, 80).unwrap();
        let session = mgr.get(info.id).unwrap();
        wait_shell_ready(&session);
        let stream = Uuid::new_v4();
        let mut subscription = mgr.attach(info.id, stream, 0, None).unwrap().subscription;
        let command = "exit\r";
        assert!(matches!(
            mgr.input(info.id, stream, 0, command.as_bytes()),
            InputOutcome::Ack { .. }
        ));
        let ended = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if matches!(subscription.next().await, Chunk::Ended) {
                    break;
                }
            }
        })
        .await;
        assert!(ended.is_ok(), "no Ended after the shell exited");
        assert!(wait_until(Duration::from_secs(5), || mgr.list().is_empty()));
        assert!(mgr.attach(info.id, stream, 0, None).is_err());
    }
}
