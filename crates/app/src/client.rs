//! SSH 连接端：连接 TermBridge 服务端（termbridge-v2 subsystem）。
//!
//! 不执行任何 shell 命令；所有交互均通过 subsystem 帧协议完成。
//! 主机指纹校验为严格匹配（SHA256，OpenSSH 格式 `SHA256:<base64>`）。
//!
//! 输入与请求分离：[`Client::input_sender`] 返回可克隆的发送句柄，
//! 可在另一个请求等待响应期间继续发送 `Input` 帧。

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use russh::client::{self, Handle};
use russh::keys::HashAlg;
use russh::keys::PublicKeyOrCertificate;
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use russh::{ChannelMsg, Disconnect};
use termbridge_protocol::{decode, encode, Event, Frame, Request, Response, MAX_FRAME, SUBSYSTEM};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use uuid::Uuid;

/// 输入确认窗口；未确认字节绝不淘汰，否则无法安全地按偏移重发。
pub const MAX_UNACKED_INPUT_BYTES: usize = 256 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum InputRecovery {
    None,
    Consumed,
    Retry {
        offset: u64,
        data: Vec<u8>,
        delay: Duration,
    },
    Reattach {
        input_base: u64,
    },
}

#[derive(Debug)]
pub struct InputBuffer {
    acked: u64,
    next: u64,
    unacked: VecDeque<u8>,
    rewind_pending: Option<u64>,
}

impl InputBuffer {
    pub fn new(base: u64) -> Self {
        Self {
            acked: base,
            next: base,
            unacked: VecDeque::new(),
            rewind_pending: None,
        }
    }

    pub fn acked(&self) -> u64 {
        self.acked
    }
    pub fn next(&self) -> u64 {
        self.next
    }
    pub fn available(&self) -> usize {
        MAX_UNACKED_INPUT_BYTES - self.unacked.len()
    }

    /// 满时拒绝新输入，不改动任何偏移或已有未确认字节。
    pub fn queue(&mut self, data: &[u8]) -> Option<u64> {
        if data.is_empty()
            || data.len() > MAX_UNACKED_INPUT_BYTES.saturating_sub(self.unacked.len())
        {
            return None;
        }
        let at = self.next;
        self.next = self.next.checked_add(data.len() as u64)?;
        self.unacked.extend(data);
        Some(at)
    }

    pub fn ack(&mut self, offset: u64) {
        let target = offset.min(self.next);
        if target <= self.acked {
            return;
        }
        let consumed = (target - self.acked) as usize;
        self.unacked.drain(..consumed);
        self.acked = target;
        if self.rewind_pending.is_some_and(|at| target > at) {
            self.rewind_pending = None;
        }
    }

    pub fn rejected(&mut self, code: &str, offset: u64) -> InputRecovery {
        match code {
            "not_controller" => {
                self.ack(offset);
                InputRecovery::Consumed
            }
            "busy" | "input_gap" => {
                if offset < self.acked || offset > self.next {
                    // 缺失区间已无法还原：绝不能猜测其内容并盲目重发。
                    let base = self.next;
                    self.acked = base;
                    self.unacked.clear();
                    self.rewind_pending = None;
                    return InputRecovery::Reattach { input_base: base };
                }
                if code == "input_gap" && self.rewind_pending.is_some_and(|at| at <= offset) {
                    return InputRecovery::None;
                }
                self.rewind_pending = Some(offset);
                InputRecovery::Retry {
                    offset,
                    data: self
                        .unacked
                        .iter()
                        .skip((offset - self.acked) as usize)
                        .copied()
                        .collect(),
                    delay: if code == "busy" {
                        Duration::from_millis(50)
                    } else {
                        Duration::ZERO
                    },
                }
            }
            _ => InputRecovery::None,
        }
    }

    pub fn align_attach(&mut self, remote_next: u64) {
        if remote_next > self.next {
            self.acked = remote_next;
            self.next = remote_next;
            self.unacked.clear();
            self.rewind_pending = None;
        } else {
            self.ack(remote_next);
        }
    }
}

#[cfg(test)]
mod input_buffer_tests {
    use super::*;

    #[test]
    fn busy_rewinds_and_ack_drops_prefix() {
        let mut b = InputBuffer::new(0);
        assert_eq!(b.queue(b"abc"), Some(0));
        assert_eq!(b.queue(b"def"), Some(3));
        b.ack(3);
        assert_eq!(
            b.rejected("busy", 3),
            InputRecovery::Retry {
                offset: 3,
                data: b"def".to_vec(),
                delay: Duration::from_millis(50)
            }
        );
        b.ack(6);
        assert_eq!(b.acked(), 6);
    }

    #[test]
    fn gap_deduplicates_and_unknown_range_reattaches() {
        let mut b = InputBuffer::new(0);
        b.queue(b"abc");
        assert!(matches!(
            b.rejected("input_gap", 0),
            InputRecovery::Retry { .. }
        ));
        assert_eq!(b.rejected("input_gap", 0), InputRecovery::None);
        b.ack(2);
        assert_eq!(
            b.rejected("input_gap", 0),
            InputRecovery::Reattach { input_base: 3 }
        );
    }

    #[test]
    fn full_buffer_rejects_without_dropping_or_advancing() {
        let mut b = InputBuffer::new(0);
        assert_eq!(b.queue(&vec![42; MAX_UNACKED_INPUT_BYTES]), Some(0));
        assert_eq!(b.queue(b"x"), None);
        assert_eq!(b.next(), MAX_UNACKED_INPUT_BYTES as u64);
        assert!(
            matches!(b.rejected("busy", 0), InputRecovery::Retry { data, .. }
            if data.len() == MAX_UNACKED_INPUT_BYTES)
        );
    }
}

/// OpenSSH 风格的 SHA256 主机指纹，例如 `SHA256:abcdef...`（base64，无 padding）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostFingerprint {
    pub sha256: String,
}

impl HostFingerprint {
    pub fn new(sha256: impl Into<String>) -> Self {
        Self {
            sha256: sha256.into(),
        }
    }

    /// 去掉可选的 `SHA256:` 前缀，返回 base64 部分。
    pub fn base64_part(&self) -> &str {
        let s = self.sha256.as_str();
        s.strip_prefix("SHA256:")
            .or_else(|| s.strip_prefix("sha256:"))
            .unwrap_or(s)
    }
}

/// 认证方式：密码或本地私钥文件。
#[derive(Clone)]
pub enum AuthMethod {
    Password {
        password: String,
    },
    PrivateKey {
        path: String,
        passphrase: Option<String>,
    },
}

/// 连接配置。`expected_fingerprint` 为必填，严格匹配失败即拒绝连接。
pub struct ClientConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthMethod,
    pub expected_fingerprint: HostFingerprint,
}

impl ClientConfig {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        auth: AuthMethod,
        expected_fingerprint: HostFingerprint,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            user: user.into(),
            auth,
            expected_fingerprint,
        }
    }
}

/// 仅探测主机指纹：完成 SSH 密钥交换后立即拒绝，不进行任何认证。
/// 返回服务端实际指纹，供上层展示并确认后再调用 [`Client::connect`]。
pub async fn probe_host(host: &str, port: u16) -> Result<HostFingerprint> {
    let seen: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
    let verifier = HostKeyVerifier {
        expected: None,
        seen: seen.clone(),
    };
    let config = Arc::new(client::Config::default());
    let addrs = (host.to_string(), port);
    // 期望的失败：check_server_key 恒返回 false，此处 Err 属于预期。
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        client::connect(config, addrs, verifier),
    )
    .await;
    let result = seen.lock().unwrap().clone();
    match result {
        Some(fp) => Ok(HostFingerprint { sha256: fp }),
        None => {
            bail!("probe: host key exchange failed or was rejected before fingerprint was seen")
        }
    }
}

type SharedWriter = Arc<tokio::sync::Mutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>;

/// 只发送 `Input` 帧的句柄；与请求通道共享底层 SSH channel 写半边。
/// 不等待响应，可在请求等待响应期间并发使用。
#[derive(Clone)]
pub struct InputSender {
    writer: SharedWriter,
    disconnected: Arc<AtomicBool>,
}

impl InputSender {
    pub async fn send_input(
        &self,
        session_id: Uuid,
        stream_id: Uuid,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        if self.disconnected.load(Ordering::SeqCst) {
            bail!("disconnected");
        }
        let frame = Frame::Input {
            session_id,
            stream_id,
            offset,
            data_b64: B64.encode(data),
        };
        let line = encode(&frame).context("encode input failed")?;
        if line.len() > MAX_FRAME {
            bail!("input frame too large: {} > {MAX_FRAME}", line.len());
        }
        let mut writer = self.writer.lock().await;
        if let Err(e) = async {
            writer.write_all(&line).await?;
            writer.flush().await
        }
        .await
        {
            self.disconnected.store(true, Ordering::SeqCst);
            bail!("input delivery uncertain; connection closed: {e}");
        }
        Ok(())
    }

    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }
}

/// 请求 ID 到响应的关联由内部事件循环完成；事件异步推送给调用者。
pub struct Client {
    sender: InputSender,
    handle: Handle<HostKeyVerifier>,
    loop_rx: mpsc::Receiver<LoopMsg>,
    event_buf: EventBuffer,
    resp_buf: VecDeque<(Uuid, Response)>,
    disconnected: Arc<AtomicBool>,
}

enum LoopMsg {
    Response { id: Uuid, body: Response },
    Event(Event),
    Closed,
}

/// 客户端缓存的未消费事件上限；超出时丢弃最旧的。
const MAX_BUFFERED_EVENTS: usize = 4096;

#[derive(Debug)]
pub struct EventLoss;

impl std::fmt::Display for EventLoss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "buffered events were lost; reattach required")
    }
}
impl std::error::Error for EventLoss {}

#[derive(Default)]
struct EventBuffer {
    events: VecDeque<Event>,
    lost: bool,
}

impl EventBuffer {
    fn push(&mut self, event: Event) {
        if self.events.len() >= MAX_BUFFERED_EVENTS {
            self.events.pop_front();
            self.lost = true;
        }
        self.events.push_back(event);
    }

    fn take(&mut self) -> std::result::Result<Option<Event>, EventLoss> {
        if self.lost {
            self.clear();
            return Err(EventLoss);
        }
        Ok(self.events.pop_front())
    }

    fn clear(&mut self) {
        self.events.clear();
        self.lost = false;
    }
}

#[cfg(test)]
mod event_buffer_tests {
    use super::*;

    #[test]
    fn overflow_is_reported_before_any_remaining_event() {
        let mut buffer = EventBuffer::default();
        for _ in 0..=MAX_BUFFERED_EVENTS {
            buffer.push(Event::Ended {
                session_id: Uuid::nil(),
            });
        }
        assert!(buffer.take().is_err());
        assert!(buffer.take().unwrap().is_none());
    }
}

struct HostKeyVerifier {
    expected: Option<String>,
    seen: Arc<std::sync::Mutex<Option<String>>>,
}

impl client::Handler for HostKeyVerifier {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = server_public_key else {
            bail!("host certificates are not supported by this product");
        };
        let fp = fingerprint_sha256(key);
        *self.seen.lock().unwrap() = Some(fp.clone());
        match &self.expected {
            // probe 模式：只记录指纹并拒绝，绝不进入认证阶段。
            None => Ok(false),
            Some(expected) => {
                let exp = HostFingerprint::new(expected.clone());
                if HostFingerprint::new(fp.clone()).base64_part() == exp.base64_part() {
                    Ok(true)
                } else {
                    bail!(
                        "host fingerprint mismatch: expected {}, got {}",
                        exp.sha256,
                        fp
                    )
                }
            }
        }
    }
}

fn fingerprint_sha256(key: &russh::keys::PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

impl Client {
    /// 连接并认证，然后打开 termbridge-v2 subsystem。
    pub async fn connect(cfg: ClientConfig) -> Result<Self> {
        let verifier = HostKeyVerifier {
            expected: Some(cfg.expected_fingerprint.sha256.clone()),
            seen: Arc::default(),
        };
        let config = Arc::new(client::Config::default());
        let mut handle: Handle<HostKeyVerifier> = tokio::time::timeout(
            Duration::from_secs(10),
            client::connect(config, (cfg.host.as_str(), cfg.port), verifier),
        )
        .await
        .context("ssh connect timed out")?
        .context("ssh connect failed")?;

        let auth = &cfg.auth;
        let ok = match auth {
            AuthMethod::Password { password } => handle
                .authenticate_password(cfg.user.as_str(), password.as_str())
                .await
                .context("password auth failed")?
                .success(),
            AuthMethod::PrivateKey { path, passphrase } => {
                let key = load_secret_key(Path::new(path), passphrase.as_deref())
                    .with_context(|| format!("load private key {path}"))?;
                let hash = handle
                    .best_supported_rsa_hash()
                    .await
                    .context("rsa hash negotiation failed")?
                    .flatten();
                handle
                    .authenticate_publickey(
                        cfg.user.as_str(),
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    )
                    .await
                    .context("publickey auth failed")?
                    .success()
            }
        };
        if !ok {
            bail!("authentication failed for user {}", cfg.user);
        }

        let mut channel = handle
            .channel_open_session()
            .await
            .context("open ssh channel failed")?;
        channel
            .request_subsystem(true, SUBSYSTEM)
            .await
            .context("request subsystem failed")?;
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Success) => break,
                Some(ChannelMsg::Failure) => {
                    bail!("server rejected subsystem {SUBSYSTEM}")
                }
                Some(_) => continue,
                None => bail!("connection closed while starting subsystem {SUBSYSTEM}"),
            }
        }

        let writer = channel.make_writer();
        let (loop_tx, loop_rx) = mpsc::channel(64);
        tokio::spawn(reader_loop(channel, loop_tx));

        let disconnected = Arc::new(AtomicBool::new(false));
        Ok(Self {
            sender: InputSender {
                writer: Arc::new(tokio::sync::Mutex::new(Box::new(writer))),
                disconnected: disconnected.clone(),
            },
            handle,
            loop_rx,
            event_buf: EventBuffer::default(),
            resp_buf: VecDeque::new(),
            disconnected,
        })
    }

    /// 可克隆的输入发送句柄；输入不等待响应，也不占用 `&mut Client`。
    pub fn input_sender(&self) -> InputSender {
        self.sender.clone()
    }

    /// 发送一个 `Input` 帧（不等待响应）。ACK/拒绝通过事件返回。
    pub async fn send_input(
        &self,
        session_id: Uuid,
        stream_id: Uuid,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        self.sender
            .send_input(session_id, stream_id, offset, data)
            .await
    }

    /// 发送一个请求并等待按 UUID 关联的响应。单飞（&mut self），不自动重试。
    pub async fn request(&mut self, req: Request) -> Result<Response> {
        if self.is_disconnected() {
            bail!("disconnected");
        }
        let id = Uuid::new_v4();
        let frame = encode(&Frame::Request { id, body: req }).context("encode request failed")?;
        if frame.len() > MAX_FRAME {
            bail!("request too large: {} > {MAX_FRAME}", frame.len());
        }
        {
            let mut writer = self.sender.writer.lock().await;
            if let Err(e) = async {
                writer.write_all(&frame).await?;
                writer.flush().await
            }
            .await
            {
                self.disconnected.store(true, Ordering::SeqCst);
                bail!("request delivery uncertain; connection closed, never resend automatically: {e}");
            }
        }
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match self.loop_rx.recv().await {
                    Some(LoopMsg::Response { id: rid, body }) if rid == id => return Some(body),
                    Some(LoopMsg::Response { id: rid, body }) => {
                        if self.resp_buf.len() >= 64 {
                            return None;
                        }
                        self.resp_buf.push_back((rid, body));
                    }
                    Some(LoopMsg::Event(e)) => self.event_buf.push(e),
                    Some(LoopMsg::Closed) | None => return None,
                }
            }
        })
        .await;
        match result {
            Ok(Some(response)) => {
                // Attached 响应之前的旧订阅事件不能再夹在新快照/重放里。
                if matches!(response, Response::Attached { .. }) {
                    self.event_buf.clear();
                }
                Ok(response)
            }
            _ => {
                self.disconnected.store(true, Ordering::SeqCst);
                bail!("request result unknown after timeout/disconnect; never resend automatically")
            }
        }
    }

    /// 已丢事件时明确报错，调用者必须重新挂接；断线则返回 Ok(None)。
    pub async fn recv_event(&mut self) -> std::result::Result<Option<Event>, EventLoss> {
        if let Some(e) = self.event_buf.take()? {
            return Ok(Some(e));
        }
        loop {
            match self.loop_rx.recv().await {
                Some(LoopMsg::Event(e)) => return Ok(Some(e)),
                Some(LoopMsg::Response { id, body }) => self.resp_buf.push_back((id, body)),
                Some(LoopMsg::Closed) | None => {
                    self.disconnected.store(true, Ordering::SeqCst);
                    return Ok(None);
                }
            }
        }
    }

    /// 测试用：在同一连接上再开一个通道，验证接收端会立即拒绝。
    #[cfg(test)]
    pub(crate) async fn open_extra_channel(&self) -> Result<()> {
        self.handle.channel_open_session().await?;
        Ok(())
    }

    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }

    /// 主动断开并通知。之后 request 会失败，recv_event 返回 None。
    pub async fn disconnect(&mut self) -> Result<()> {
        self.disconnected.store(true, Ordering::SeqCst);
        self.handle
            .disconnect(Disconnect::ByApplication, "client closed", "en")
            .await
            .ok();
        Ok(())
    }
}

async fn reader_loop(mut channel: russh::Channel<russh::client::Msg>, tx: mpsc::Sender<LoopMsg>) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Data { data }) => {
                buf.extend_from_slice(&data);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf[..pos].to_vec();
                    buf.drain(..=pos);
                    if line.len() > MAX_FRAME {
                        let _ = tx.send(LoopMsg::Closed).await;
                        return;
                    }
                    match decode(&line) {
                        Ok(Frame::Response { id, body }) => {
                            if tx.send(LoopMsg::Response { id, body }).await.is_err() {
                                return;
                            }
                        }
                        Ok(Frame::Event { body }) => {
                            if tx.send(LoopMsg::Event(body)).await.is_err() {
                                return;
                            }
                        }
                        Ok(_) | Err(_) => {
                            let _ = tx.send(LoopMsg::Closed).await;
                            return;
                        }
                    }
                }
                if buf.len() > MAX_FRAME {
                    let _ = tx.send(LoopMsg::Closed).await;
                    return;
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    let _ = tx.send(LoopMsg::Closed).await;
}
