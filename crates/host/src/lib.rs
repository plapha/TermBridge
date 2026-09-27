//! Persistent per-user terminal sessions. The OpenCode worker owns this crate.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use termbridge_protocol::{Event, SessionInfo, MAX_SEND_BYTES};
use tokio::sync::broadcast;
use uuid::Uuid;

const MAX_SCROLLBACK_LINES: usize = 5000;
const MAX_SCROLLBACK_BYTES: usize = 8 * 1024 * 1024;
const MAX_ROWS: u16 = 500;
const MAX_COLS: u16 = 1000;
const MAX_DEDUP_ENTRIES: usize = 4096;
const EVENT_CHANNEL_CAPACITY: usize = 1024;

pub struct SessionManager {
    inner: Arc<Inner>,
}

struct Inner {
    sessions: Mutex<HashMap<Uuid, Arc<Session>>>,
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

struct CommandDedup {
    seen: HashSet<Uuid>,
    order: VecDeque<Uuid>,
}

impl CommandDedup {
    fn new() -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    fn check_and_insert(&mut self, id: Uuid) -> bool {
        if self.seen.contains(&id) {
            return false;
        }
        self.seen.insert(id);
        self.order.push_back(id);
        while self.order.len() > MAX_DEDUP_ENTRIES {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

struct Session {
    info: Mutex<SessionInfo>,
    parser: Mutex<vt100::Parser>,
    scrollback: Mutex<Scrollback>,
    seq: AtomicU64,
    attached: Mutex<HashSet<Uuid>>,
    dedup: Mutex<CommandDedup>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    events: broadcast::Sender<Event>,
    control: Mutex<TerminalControl>,
    control_ready: Condvar,
}

#[derive(Default)]
struct TerminalControl {
    activated: bool,
    pending_dsr: Option<Vec<u8>>,
}

impl Session {
    fn snapshot(&self) -> (String, u64) {
        let parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        let screen_bytes = screen.state_formatted();
        let seq = self.seq.load(Ordering::SeqCst);
        let (rows, _) = screen.size();
        let mut bytes = self
            .scrollback
            .lock()
            .unwrap()
            .older_history(rows as usize, 256 * 1024);
        // Keep history in xterm scrollback and then reconstruct the authoritative
        // visible screen. CSI 2J clears only the display, not the scrollback.
        bytes.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H");
        bytes.extend_from_slice(&screen_bytes);
        (B64.encode(bytes), seq)
    }

    fn broadcast(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

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
        Ok(Self {
            inner: Arc::new(Inner {
                sessions: Mutex::new(HashMap::new()),
                #[cfg(windows)]
                job: WinJob::new()?,
            }),
        })
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let sessions = self.inner.sessions.lock().unwrap();
        sessions
            .values()
            .map(|s| s.info.lock().unwrap().clone())
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
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow!("failed to clone pty reader: {e}"))?;

        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);

        let id = Uuid::new_v4();
        let info = SessionInfo {
            id,
            title,
            created_unix_ms: now_unix_ms(),
            live: true,
            rows,
            cols,
            controller: None,
        };

        let session = Arc::new(Session {
            info: Mutex::new(info.clone()),
            parser: Mutex::new(vt100::Parser::new(rows, cols, 0)),
            scrollback: Mutex::new(Scrollback::new()),
            seq: AtomicU64::new(0),
            attached: Mutex::new(HashSet::new()),
            dedup: Mutex::new(CommandDedup::new()),
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
            events: events.clone(),
            control: Mutex::new(TerminalControl::default()),
            control_ready: Condvar::new(),
        });

        self.inner
            .sessions
            .lock()
            .unwrap()
            .insert(id, Arc::clone(&session));

        // Reader thread: authoritative screen state + scrollback + output events.
        {
            let session = Arc::clone(&session);
            std::thread::spawn(move || reader_loop(&session, &mut reader));
        }
        // Waiter thread: detect child exit, mark dead, broadcast Ended.
        {
            let session = Arc::clone(&session);
            std::thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
                session.info.lock().unwrap().live = false;
                session.broadcast(Event::Ended { session_id: id });
            });
        }

        Ok(info)
    }

    pub fn attach(&self, session_id: Uuid, client_id: Uuid) -> Result<Attachment> {
        let session = self.get(session_id)?;
        let mut attached = session.attached.lock().unwrap();
        let info = session.info.lock().unwrap();
        if !info.live {
            bail!("session is not live");
        }
        attached.insert(client_id);
        let gained = info.controller.is_none();
        drop(info);
        let mut info = session.info.lock().unwrap();
        if gained {
            info.controller = Some(client_id);
        }
        let has_control = info.controller == Some(client_id);
        let info = info.clone();
        drop(attached);
        if gained {
            session.broadcast(Event::ControlChanged {
                session_id,
                controller: Some(client_id),
            });
        }

        // Subscribe before snapshot so no output can be lost between the two.
        let events = session.events.subscribe();
        let (screen_b64, seq) = session.snapshot();
        if screen_b64.len() + 1024 > termbridge_protocol::MAX_FRAME {
            let _ = self.detach(session_id, client_id);
            bail!("screen snapshot exceeds protocol frame limit");
        }
        Ok(Attachment {
            session: info,
            screen_b64,
            seq,
            has_control,
            events,
        })
    }

    pub fn detach(&self, session_id: Uuid, client_id: Uuid) -> Result<()> {
        let session = self.get(session_id)?;
        let was_attached = session.attached.lock().unwrap().remove(&client_id);
        if !was_attached {
            bail!("client is not attached to this session");
        }
        let released = session.info.lock().unwrap().controller == Some(client_id);
        if released {
            session.info.lock().unwrap().controller = None;
            session.broadcast(Event::ControlChanged {
                session_id,
                controller: None,
            });
        }
        Ok(())
    }

    pub fn take_control(&self, session_id: Uuid, client_id: Uuid) -> Result<()> {
        let session = self.get(session_id)?;
        if !session.attached.lock().unwrap().contains(&client_id) {
            bail!("client is not attached to this session");
        }
        session.info.lock().unwrap().controller = Some(client_id);
        session.broadcast(Event::ControlChanged {
            session_id,
            controller: Some(client_id),
        });
        Ok(())
    }

    pub fn send(
        &self,
        session_id: Uuid,
        client_id: Uuid,
        command_id: Uuid,
        text: &str,
    ) -> Result<()> {
        let session = self.get(session_id)?;
        self.require_controller(&session, client_id)?;
        if !session.info.lock().unwrap().live {
            bail!("session is not live");
        }
        if text.chars().any(|c| c.is_control()) {
            bail!("a command must be one line without control characters");
        }
        let text_bytes = text.as_bytes();
        if text_bytes.len() > MAX_SEND_BYTES {
            bail!(
                "text too large: {} bytes (max {})",
                text_bytes.len(),
                MAX_SEND_BYTES
            );
        }
        if !session.dedup.lock().unwrap().check_and_insert(command_id) {
            bail!("duplicate command_id");
        }
        // Windows console shells request a cursor-position report (CSI 6n) at
        // startup. Defer that protocol reply until an explicit Send, so a
        // newly attached terminal receives no PTY input before the user acts.
        let mut control = session.control.lock().unwrap();
        #[cfg(windows)]
        if !control.activated && control.pending_dsr.is_none() {
            let (guard, _) = session
                .control_ready
                .wait_timeout(control, std::time::Duration::from_secs(2))
                .unwrap();
            control = guard;
        }
        control.activated = true;
        let mut writer = session.writer.lock().unwrap();
        if let Some(reply) = control.pending_dsr.take() {
            writer.write_all(&reply)?;
        }
        writer.write_all(text_bytes)?;
        writer.write_all(b"\r")?;
        writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, session_id: Uuid, client_id: Uuid, rows: u16, cols: u16) -> Result<()> {
        validate_dims(rows, cols)?;
        let session = self.get(session_id)?;
        self.require_controller(&session, client_id)?;
        session
            .master
            .lock()
            .unwrap()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow!("pty resize failed: {e}"))?;
        let mut info = session.info.lock().unwrap();
        info.rows = rows;
        info.cols = cols;
        session
            .parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
        Ok(())
    }

    pub fn interrupt(&self, session_id: Uuid, client_id: Uuid) -> Result<()> {
        let session = self.get(session_id)?;
        self.require_controller(&session, client_id)?;
        let mut writer = session.writer.lock().unwrap();
        writer.write_all(b"\x03")?;
        writer.flush()?;
        Ok(())
    }

    pub fn end_for(&self, session_id: Uuid, client_id: Uuid) -> Result<()> {
        let session = self.get(session_id)?;
        self.require_controller(&session, client_id)?;
        self.end(session_id)
    }

    pub fn end(&self, session_id: Uuid) -> Result<()> {
        let session = self.get(session_id)?;
        {
            let _ = session.killer.lock().unwrap().kill();
        }
        session.info.lock().unwrap().live = false;
        session.broadcast(Event::Ended { session_id });
        self.inner.sessions.lock().unwrap().remove(&session_id);
        Ok(())
    }

    /// Kill all remaining children. Call on host process shutdown to avoid orphans.
    pub fn shutdown(&self) {
        let sessions: Vec<Arc<Session>> = {
            let mut map = self.inner.sessions.lock().unwrap();
            map.drain().map(|(_, s)| s).collect()
        };
        for session in sessions {
            if let Ok(mut killer) = session.killer.lock() {
                let _ = killer.kill();
            }
            session.info.lock().unwrap().live = false;
            let id = session.info.lock().unwrap().id;
            session.broadcast(Event::Ended { session_id: id });
        }
    }

    fn get(&self, session_id: Uuid) -> Result<Arc<Session>> {
        self.inner
            .sessions
            .lock()
            .unwrap()
            .get(&session_id)
            .map(Arc::clone)
            .ok_or_else(|| anyhow!("session not found"))
    }

    fn require_controller(&self, session: &Session, client_id: Uuid) -> Result<()> {
        let info = session.info.lock().unwrap();
        if info.controller != Some(client_id) {
            bail!("client is not the controlling client of this session");
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
    pub screen_b64: String,
    pub seq: u64,
    pub has_control: bool,
    pub events: broadcast::Receiver<Event>,
}

fn reader_loop(session: &Session, reader: &mut dyn std::io::Read) {
    let mut buf = [0u8; 4096];
    #[cfg(windows)]
    let mut tail = VecDeque::with_capacity(4);
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let seq = {
                    let mut parser = session.parser.lock().unwrap();
                    parser.process(&buf[..n]);
                    session.seq.fetch_add(1, Ordering::SeqCst) + 1
                };
                session.scrollback.lock().unwrap().push_bytes(&buf[..n]);
                #[cfg(windows)]
                for byte in &buf[..n] {
                    tail.push_back(*byte);
                    if tail.len() > 4 {
                        tail.pop_front();
                    }
                    if tail.iter().copied().eq(b"\x1b[6n".iter().copied()) {
                        let (row, col) = session.parser.lock().unwrap().screen().cursor_position();
                        let reply = format!("\x1b[{};{}R", row + 1, col + 1).into_bytes();
                        let mut control = session.control.lock().unwrap();
                        if control.activated {
                            if let Ok(mut writer) = session.writer.lock() {
                                let _ = writer.write_all(&reply);
                                let _ = writer.flush();
                            }
                        } else {
                            control.pending_dsr = Some(reply);
                            session.control_ready.notify_all();
                        }
                    }
                }
                let data_b64 = B64.encode(&buf[..n]);
                session.broadcast(Event::Output {
                    session_id: session.info.lock().unwrap().id,
                    seq,
                    data_b64,
                });
            }
            Err(_) => break,
        }
    }
    session.info.lock().unwrap().live = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_attachment_controls_and_takeover_switches() {
        let mgr = SessionManager::new().unwrap();
        let info = mgr.create("test".into(), 24, 80).expect("create");
        let client_a = Uuid::new_v4();
        let client_b = Uuid::new_v4();

        let att_a = mgr.attach(info.id, client_a).expect("attach a");
        assert!(att_a.has_control);

        let att_b = mgr.attach(info.id, client_b).expect("attach b");
        assert!(!att_b.has_control);

        // Observer cannot send or resize.
        assert!(mgr
            .send(info.id, client_b, Uuid::new_v4(), "echo hi")
            .is_err());
        assert!(mgr.resize(info.id, client_b, 30, 100).is_err());

        // Explicit takeover.
        mgr.take_control(info.id, client_b).expect("take control");
        let att_b2 = mgr.attach(info.id, client_b).expect("reattach b");
        assert!(att_b2.has_control);
        assert!(mgr
            .send(info.id, client_a, Uuid::new_v4(), "echo hi")
            .is_err());

        // Controller detach releases control; next attach gains it.
        mgr.detach(info.id, client_b).expect("detach b");
        let client_c = Uuid::new_v4();
        let att_c = mgr.attach(info.id, client_c).expect("attach c");
        assert!(att_c.has_control);

        mgr.end(info.id).expect("end");
        mgr.shutdown();
    }

    #[tokio::test]
    async fn duplicate_command_id_is_rejected() {
        let mgr = SessionManager::new().unwrap();
        let info = mgr.create("test".into(), 24, 80).expect("create");
        let client = Uuid::new_v4();
        let att = mgr.attach(info.id, client).expect("attach");
        assert!(att.has_control);

        let command_id = Uuid::new_v4();
        mgr.send(info.id, client, command_id, "echo dedup-test")
            .expect("first send");
        assert!(mgr
            .send(info.id, client, command_id, "echo dedup-test")
            .is_err());
        // A fresh command_id works even with identical text.
        mgr.send(info.id, client, Uuid::new_v4(), "echo dedup-test")
            .expect("second send with new id");

        // Oversized text is rejected.
        let big = "x".repeat(MAX_SEND_BYTES + 1);
        assert!(mgr.send(info.id, client, Uuid::new_v4(), &big).is_err());

        mgr.end(info.id).expect("end");
        mgr.shutdown();
    }

    #[tokio::test]
    async fn invalid_dimensions_rejected() {
        let mgr = SessionManager::new().unwrap();
        assert!(mgr.create("bad".into(), 0, 80).is_err());
        assert!(mgr.create("bad".into(), 24, 0).is_err());
        assert!(mgr.create("bad".into(), 6000, 80).is_err());
    }
    #[tokio::test]
    async fn command_produces_output_after_explicit_send() {
        let mgr = SessionManager::new().unwrap();
        let info = mgr.create("output".into(), 24, 80).unwrap();
        let client = Uuid::new_v4();
        let mut attachment = mgr.attach(info.id, client).unwrap();
        #[cfg(windows)]
        let command = "Write-Output TB_OUTPUT_TEST_654";
        #[cfg(not(windows))]
        let command = "echo TB_OUTPUT_TEST_654";
        mgr.resize(info.id, client, 25, 81).unwrap();
        mgr.send(info.id, client, Uuid::new_v4(), command).unwrap();
        let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(Event::Output { data_b64, .. }) = attachment.events.recv().await {
                    if let Ok(data) = B64.decode(data_b64) {
                        if String::from_utf8_lossy(&data).contains("TB_OUTPUT_TEST_654") {
                            break true;
                        }
                    }
                }
            }
        })
        .await;
        assert!(found.is_ok(), "no output after command send");
        mgr.end(info.id).unwrap();
    }
}
