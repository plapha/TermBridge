use crate::config::{host_key_path, HostConfig};
use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use russh::{
    server::{self, Msg, Server as _, Session},
    Channel, ChannelId, ChannelOpenFailure,
};
use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use termbridge_host::{Attachment, Chunk, InputOutcome, SessionManager, Subscription};
use termbridge_protocol::{
    decode, encode, Event, Frame, Request, Response, MAX_FRAME, MAX_SNAPSHOT_CHUNK, SUBSYSTEM,
};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

struct Shared {
    config: HostConfig,
    manager: Arc<SessionManager>,
    failed: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    stopped: AtomicBool,
}

/// 单个连接最多排队的未处理请求；超出视为滥用并断开。
const MAX_PENDING_REQUESTS: usize = 256;

struct HostServer {
    shared: Arc<Shared>,
    peer: Option<IpAddr>,
    channel: Option<ChannelId>,
    handle: Option<server::Handle>,
    input: Vec<u8>,
    // subsystem 建立后才有；请求交给连接自己的工作任务按序执行，
    // SSH 回调本身不做任何可能阻塞的事。
    requests: Option<mpsc::Sender<(Uuid, Request)>>,
}

impl HostServer {
    fn new(shared: Arc<Shared>, peer: Option<IpAddr>) -> Self {
        Self {
            shared,
            peer,
            channel: None,
            handle: None,
            input: Vec::new(),
            requests: None,
        }
    }
    fn reject_attempt(&self) {
        if let Some(ip) = self.peer {
            if let Ok(mut failures) = self.shared.failed.lock() {
                let entry = failures.entry(ip).or_insert((0, Instant::now()));
                if entry.1.elapsed() > Duration::from_secs(600) {
                    *entry = (0, Instant::now());
                }
                entry.0 = entry.0.saturating_add(1);
            }
        }
    }
    fn blocked(&self) -> bool {
        self.peer
            .and_then(|ip| {
                self.shared
                    .failed
                    .lock()
                    .ok()
                    .and_then(|m| m.get(&ip).copied())
            })
            .map(|(count, since)| count >= 5 && since.elapsed() < Duration::from_secs(600))
            .unwrap_or(false)
    }
}

/// 一个连接的请求处理者。关闭请求队列后它处理完已排队的请求、
/// 解除挂接并结束；被丢弃时同样会解除挂接（控制权进入宽限期）。
struct Worker {
    shared: Arc<Shared>,
    channel: ChannelId,
    handle: server::Handle,
    attached: HashMap<Uuid, (Uuid, u64)>,
    tasks: HashMap<Uuid, tokio::task::JoinHandle<()>>,
}

impl Worker {
    async fn run(mut self, mut requests: mpsc::Receiver<(Uuid, Request)>) {
        while let Some((id, req)) = requests.recv().await {
            if self.request(id, req).await.is_err() {
                break;
            }
        }
    }

    async fn respond(&self, id: Uuid, body: Response) -> Result<(), ()> {
        let line = encode(&Frame::Response { id, body }).map_err(|_| ())?;
        self.handle.data(self.channel, line).await.map_err(|_| ())
    }

    async fn error(&self, id: Uuid, code: &str, message: impl Into<String>) -> Result<(), ()> {
        self.respond(
            id,
            Response::Error {
                code: code.into(),
                message: message.into(),
            },
        )
        .await
    }

    fn stream_for(&self, session_id: Uuid) -> Option<Uuid> {
        self.attached.get(&session_id).map(|(stream, _)| *stream)
    }

    async fn blocking<F>(&self, id: Uuid, f: F) -> Result<(), ()>
    where
        F: FnOnce(&SessionManager) -> Result<Response> + Send + 'static,
    {
        let manager = self.shared.manager.clone();
        let outcome = tokio::task::spawn_blocking(move || f(&manager))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("request handler failed: {e}")));
        let body = outcome.unwrap_or_else(|e| Response::Error {
            code: "request_failed".into(),
            message: e.to_string(),
        });
        self.respond(id, body).await
    }

    async fn request(&mut self, id: Uuid, req: Request) -> Result<(), ()> {
        if self.shared.stopped.load(Ordering::SeqCst) {
            return self.error(id, "host_stopped", "host stopped").await;
        }
        match req {
            Request::List => {
                self.blocking(id, |m| Ok(Response::Sessions { sessions: m.list() }))
                    .await
            }
            Request::Create { title, rows, cols } => {
                self.blocking(id, move |m| {
                    m.create(title, rows, cols)
                        .map(|session| Response::Created { session })
                })
                .await
            }
            Request::Attach {
                session_id,
                stream_id,
                input_base,
                resume_from,
            } => {
                self.handle_attach(id, session_id, stream_id, input_base, resume_from)
                    .await
            }
            Request::Detach { session_id } => {
                let Some(stream_id) = self.stream_for(session_id) else {
                    return self
                        .error(id, "not_attached", "not attached to this session")
                        .await;
                };
                let (_, token) = self.attached.remove(&session_id).unwrap();
                if let Some(task) = self.tasks.remove(&session_id) {
                    task.abort();
                }
                self.blocking(id, move |m| {
                    m.detach(session_id, stream_id, token)
                        .map(|_| Response::Accepted)
                })
                .await
            }
            Request::TakeControl { session_id } => {
                let Some(stream_id) = self.stream_for(session_id) else {
                    return self
                        .error(id, "not_attached", "not attached to this session")
                        .await;
                };
                self.blocking(id, move |m| {
                    m.take_control(session_id, stream_id)
                        .map(|_| Response::Accepted)
                })
                .await
            }
            Request::Resize {
                session_id,
                rows,
                cols,
            } => {
                let Some(stream_id) = self.stream_for(session_id) else {
                    return self
                        .error(id, "not_attached", "not attached to this session")
                        .await;
                };
                self.blocking(id, move |m| {
                    m.resize(session_id, stream_id, rows, cols)
                        .map(|_| Response::Accepted)
                })
                .await
            }
            Request::End { session_id } => {
                let Some(stream_id) = self.stream_for(session_id) else {
                    return self
                        .error(id, "not_attached", "not attached to this session")
                        .await;
                };
                let result = self
                    .blocking(id, move |m| {
                        m.end_for(session_id, stream_id).map(|_| Response::Accepted)
                    })
                    .await;
                if let Some(task) = self.tasks.remove(&session_id) {
                    task.abort();
                }
                self.attached.remove(&session_id);
                result
            }
        }
    }

    async fn handle_attach(
        &mut self,
        id: Uuid,
        session_id: Uuid,
        stream_id: Uuid,
        input_base: u64,
        resume_from: Option<u64>,
    ) -> Result<(), ()> {
        let manager = self.shared.manager.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            manager.attach(session_id, stream_id, input_base, resume_from)
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("request handler failed: {e}")));
        let attachment: Attachment = match outcome {
            Ok(attachment) => attachment,
            Err(e) => return self.error(id, "attach_failed", e.to_string()).await,
        };
        // 新挂接已得到新令牌；旧流（即使是不同 stream）必须解除，
        // 同一 stream 的旧令牌清理则由 host 安全忽略。
        if let Some(task) = self.tasks.remove(&session_id) {
            task.abort();
        }
        if let Some((old_stream, old_token)) = self.attached.remove(&session_id) {
            let _ = self
                .shared
                .manager
                .detach(session_id, old_stream, old_token);
        }
        self.attached
            .insert(session_id, (stream_id, attachment.token));
        let body = Response::Attached {
            session: attachment.session.clone(),
            has_control: attachment.has_control,
            input_next: attachment.input_next,
            resumed: attachment.resumed,
        };
        // 先发挂接响应，再启动转发任务：之后的快照/重放都排在响应之后。
        self.respond(id, body).await?;
        let task = tokio::spawn(forward_session(
            self.handle.clone(),
            self.channel,
            session_id,
            attachment.subscription,
            attachment.events,
        ));
        self.tasks.insert(session_id, task);
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        for (session_id, (stream_id, token)) in self.attached.drain() {
            let _ = self.shared.manager.detach(session_id, stream_id, token);
        }
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
    }
}

async fn send_event(
    handle: &server::Handle,
    channel: ChannelId,
    event: Event,
) -> std::result::Result<(), ()> {
    let line = encode(&Frame::Event { body: event }).map_err(|_| ())?;
    handle.data(channel, line).await.map_err(|_| ())
}

/// 游标式转发：输出走订阅（落后自动快照），控制类事件走 broadcast。
/// 控制事件先进入本地待发队列、在 select 之外发送，避免取消导致事件丢失。
async fn forward_session(
    handle: server::Handle,
    channel: ChannelId,
    session_id: Uuid,
    mut subscription: Subscription,
    mut control: broadcast::Receiver<Event>,
) {
    let mut pending: VecDeque<Event> = VecDeque::new();
    loop {
        if let Some(event) = pending.pop_front() {
            if send_event(&handle, channel, event).await.is_err() {
                break;
            }
            continue;
        }
        loop {
            match control.try_recv() {
                Ok(event @ (Event::ControlChanged { .. } | Event::Resized { .. })) => {
                    pending.push_back(event)
                }
                Ok(_) => {}
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Closed) => break,
            }
        }
        if let Some(event) = pending.pop_front() {
            if send_event(&handle, channel, event).await.is_err() {
                break;
            }
            continue;
        }
        tokio::select! {
            chunk = subscription.next() => match chunk {
                Chunk::Output { offset, data } => {
                    let event = Event::Output {
                        session_id,
                        offset,
                        data_b64: B64.encode(&data),
                    };
                    if send_event(&handle, channel, event).await.is_err() {
                        break;
                    }
                }
                Chunk::Snapshot { offset, rows, cols, data } => {
                    let begin = Event::SnapshotBegin { session_id, offset, rows, cols };
                    if send_event(&handle, channel, begin).await.is_err() {
                        break;
                    }
                    let mut ok = true;
                    for part in data.chunks(MAX_SNAPSHOT_CHUNK) {
                        let frame = Event::SnapshotChunk {
                            session_id,
                            data_b64: B64.encode(part),
                        };
                        if send_event(&handle, channel, frame).await.is_err() {
                            ok = false;
                            break;
                        }
                    }
                    if !ok || send_event(&handle, channel, Event::SnapshotEnd { session_id }).await.is_err() {
                        break;
                    }
                }
                Chunk::Ended => {
                    let _ = send_event(&handle, channel, Event::Ended { session_id }).await;
                    break;
                }
            },
            event = control.recv() => match event {
                Ok(event @ (Event::ControlChanged { .. } | Event::Resized { .. })) => {
                    pending.push_back(event)
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {}
            },
        }
    }
}

impl server::Server for HostServer {
    type Handler = Self;
    fn new_client(&mut self, peer: Option<SocketAddr>) -> Self {
        Self::new(self.shared.clone(), peer.map(|x| x.ip()))
    }
    fn handle_session_error(&mut self, _error: <Self::Handler as server::Handler>::Error) {
        // Never log credentials or raw terminal output.
    }
}

impl server::Handler for HostServer {
    type Error = russh::Error;
    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        if self.shared.stopped.load(Ordering::SeqCst) || self.blocked() {
            return Ok(server::Auth::reject());
        }
        // argon2 校验耗时明显，不能占着异步线程。
        let shared = self.shared.clone();
        let (user, password) = (user.to_owned(), password.to_owned());
        let ok =
            tokio::task::spawn_blocking(move || shared.config.verify_password(&user, &password))
                .await
                .unwrap_or(false);
        if ok {
            Ok(server::Auth::Accept)
        } else {
            self.reject_attempt();
            Ok(server::Auth::reject())
        }
    }
    async fn auth_publickey(
        &mut self,
        user: &str,
        key: &russh::keys::PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        if self.shared.stopped.load(Ordering::SeqCst) || self.blocked() {
            return Ok(server::Auth::reject());
        }
        if self.shared.config.verify_key(user, key) {
            Ok(server::Auth::Accept)
        } else {
            self.reject_attempt();
            Ok(server::Auth::reject())
        }
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // 每个连接只服务一个通道；多开的明确拒绝，让客户端立刻得到失败。
        if self.channel.is_some() {
            reply.reject(ChannelOpenFailure::ResourceShortage).await;
            return Ok(());
        }
        self.channel = Some(channel.id());
        reply.accept().await;
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.channel == Some(channel) && name == SUBSYSTEM && self.requests.is_none() {
            let (tx, rx) = mpsc::channel(MAX_PENDING_REQUESTS);
            let worker = Worker {
                shared: self.shared.clone(),
                channel,
                handle: session.handle(),
                attached: HashMap::new(),
                tasks: HashMap::new(),
            };
            tokio::spawn(worker.run(rx));
            self.requests = Some(tx);
            self.handle = Some(session.handle());
            session.channel_success(channel)?;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(requests) = self
            .requests
            .as_ref()
            .filter(|_| self.channel == Some(channel))
        else {
            return Err(russh::Error::Disconnect);
        };
        if self.input.len().saturating_add(data.len()) > MAX_FRAME {
            return Err(russh::Error::Disconnect);
        }
        self.input.extend_from_slice(data);
        while let Some(end) = self.input.iter().position(|&b| b == b'\n') {
            let line: Vec<_> = self.input.drain(..=end).collect();
            match decode(&line) {
                Ok(Frame::Request { id, body }) => {
                    requests
                        .try_send((id, body))
                        .map_err(|_| russh::Error::Disconnect)?;
                }
                Ok(Frame::Input {
                    session_id,
                    stream_id,
                    offset,
                    data_b64,
                }) => {
                    // 输入不经过顺序请求队列：校验 + 非阻塞入队，立即回 ACK。
                    let outcome = match B64.decode(&data_b64) {
                        Ok(data) => {
                            if self.shared.stopped.load(Ordering::SeqCst) {
                                InputOutcome::Rejected {
                                    code: "host_stopped",
                                    message: "host stopped".into(),
                                    next: 0,
                                }
                            } else {
                                self.shared
                                    .manager
                                    .input(session_id, stream_id, offset, &data)
                            }
                        }
                        Err(_) => InputOutcome::Rejected {
                            code: "invalid_data",
                            message: "input data is not valid base64".into(),
                            next: 0,
                        },
                    };
                    let event = match outcome {
                        InputOutcome::Ack { next } => Event::InputAck {
                            session_id,
                            stream_id,
                            offset: next,
                        },
                        InputOutcome::Rejected {
                            code,
                            message,
                            next,
                        } => Event::InputRejected {
                            session_id,
                            stream_id,
                            offset: next,
                            code: code.into(),
                            message,
                        },
                    };
                    let line = encode(&Frame::Event { body: event })
                        .map_err(|_| russh::Error::Disconnect)?;
                    session.data(channel, line)?;
                }
                Ok(_) | Err(_) => return Err(russh::Error::Disconnect),
            }
        }
        Ok(())
    }
    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.channel == Some(channel) {
            // 关闭队列即通知工作任务收尾并解除挂接（控制权进入宽限期）。
            self.requests = None;
            self.channel = None;
            self.handle = None;
        }
        Ok(())
    }
}

/// CLI 前台运行；Ctrl+C 触发受控关闭并结束所有终端。
pub async fn run(config: HostConfig, listen: SocketAddr) -> Result<()> {
    let socket = TcpListener::bind(listen)
        .await
        .with_context(|| crate::tr!("Cannot listen on {listen}", "无法监听 {listen}"))?;
    println!(
        "{}",
        crate::tr!(
            "Host listening on {listen} (Ctrl+C stops it and ends all sessions)",
            "接收端监听 {listen}（Ctrl+C 停止并结束会话）"
        )
    );
    run_on_listener(config, socket, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

/// GUI 在保存“已启用”前先绑定 socket，再以停止信号结束接收端。
pub async fn run_on_listener<F>(config: HostConfig, socket: TcpListener, stop: F) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send,
{
    let key = russh::keys::load_secret_key(host_key_path(), None)
        .with_context(|| crate::tr!("Failed to load the host key", "加载接收端主机密钥失败"))?;
    let ssh = Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::from_secs(2),
        max_auth_attempts: 5,
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });
    let shared = Arc::new(Shared {
        config,
        manager: Arc::new(SessionManager::new()?),
        failed: Mutex::new(HashMap::new()),
        stopped: AtomicBool::new(false),
    });
    let mut host = HostServer::new(shared.clone(), None);
    let result = tokio::select! {
        result = host.run_on_socket(ssh, &socket) => result.map_err(anyhow::Error::from),
        _ = stop => Ok(()),
    };
    shared.stopped.store(true, Ordering::SeqCst);
    shared.manager.shutdown();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_auth_failures_block_peer() {
        let shared = Arc::new(Shared {
            config: HostConfig::default(),
            manager: Arc::new(SessionManager::new().unwrap()),
            failed: Mutex::new(HashMap::new()),
            stopped: AtomicBool::new(false),
        });
        let host = HostServer::new(shared, Some("127.0.0.1".parse().unwrap()));
        for _ in 0..4 {
            host.reject_attempt();
            assert!(!host.blocked());
        }
        host.reject_attempt();
        assert!(host.blocked());
    }
}
