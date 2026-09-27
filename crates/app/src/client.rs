//! SSH 连接端：连接 TermBridge 服务端（termbridge-v1 subsystem）。
//!
//! 不执行任何 shell 命令；所有交互均通过 subsystem 帧协议完成。
//! 主机指纹校验为严格匹配（SHA256，OpenSSH 格式 `SHA256:<base64>`）。

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use russh::client::{self, Handle};
use russh::keys::HashAlg;
use russh::keys::PublicKeyOrCertificate;
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use russh::{ChannelMsg, Disconnect};
use termbridge_protocol::{decode, encode, Event, Frame, Request, Response, MAX_FRAME, SUBSYSTEM};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use uuid::Uuid;

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

/// 请求 ID 到响应的关联由内部事件循环完成；事件异步推送给调用者。
pub struct Client {
    writer: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    handle: Handle<HostKeyVerifier>,
    loop_rx: mpsc::Receiver<LoopMsg>,
    event_buf: VecDeque<Event>,
    resp_buf: VecDeque<(Uuid, Response)>,
    disconnected: bool,
}

enum LoopMsg {
    Response { id: Uuid, body: Response },
    Event(Event),
    Closed,
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
    /// 连接并认证，然后打开 termbridge-v1 subsystem。
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

        Ok(Self {
            writer: Box::new(writer),
            handle,
            loop_rx,
            event_buf: VecDeque::new(),
            resp_buf: VecDeque::new(),
            disconnected: false,
        })
    }

    /// 发送一个请求并等待按 UUID 关联的响应。单飞（&mut self），不自动重试。
    pub async fn request(&mut self, req: Request) -> Result<Response> {
        if self.disconnected {
            bail!("disconnected");
        }
        let id = Uuid::new_v4();
        let frame = encode(&Frame::Request { id, body: req }).context("encode request failed")?;
        if frame.len() > MAX_FRAME {
            bail!("request too large: {} > {MAX_FRAME}", frame.len());
        }
        if let Err(e) = async {
            self.writer.write_all(&frame).await?;
            self.writer.flush().await
        }
        .await
        {
            self.disconnected = true;
            bail!("request delivery uncertain; connection closed, never resend automatically: {e}");
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
                    Some(LoopMsg::Event(e)) => {
                        if self.event_buf.len() >= 1024 {
                            let session_id = match &e {
                                Event::Output { session_id, .. }
                                | Event::Ended { session_id }
                                | Event::ControlChanged { session_id, .. }
                                | Event::ResyncRequired { session_id } => *session_id,
                            };
                            self.event_buf.clear();
                            self.event_buf
                                .push_back(Event::ResyncRequired { session_id });
                        } else {
                            self.event_buf.push_back(e);
                        }
                    }
                    Some(LoopMsg::Closed) | None => return None,
                }
            }
        })
        .await;
        match result {
            Ok(Some(response)) => Ok(response),
            _ => {
                self.disconnected = true;
                bail!("request result unknown after timeout/disconnect; never resend automatically")
            }
        }
    }

    /// 异步接收服务端推送的事件。断线后返回 None（此后不再自动重试）。
    pub async fn recv_event(&mut self) -> Option<Event> {
        if let Some(e) = self.event_buf.pop_front() {
            return Some(e);
        }
        loop {
            match self.loop_rx.recv().await {
                Some(LoopMsg::Event(e)) => return Some(e),
                Some(LoopMsg::Response { id, body }) => self.resp_buf.push_back((id, body)),
                Some(LoopMsg::Closed) | None => {
                    self.disconnected = true;
                    return None;
                }
            }
        }
    }

    pub fn is_disconnected(&self) -> bool {
        self.disconnected
    }

    /// 主动断开并通知。之后 request 会失败，recv_event 返回 None。
    pub async fn disconnect(&mut self) -> Result<()> {
        self.disconnected = true;
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
                        Ok(Frame::Request { .. }) | Err(_) => {
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
