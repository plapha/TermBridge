use crate::config::{host_key_path, HostConfig};
use anyhow::{Context, Result};
use russh::{
    server::{self, Msg, Server as _, Session},
    Channel, ChannelId, ChannelOpenFailure,
};
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use termbridge_host::SessionManager;
use termbridge_protocol::{decode, encode, Event, Frame, Request, Response, MAX_FRAME, SUBSYSTEM};
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
    client_id: Uuid,
    channel: Option<ChannelId>,
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
            client_id: Uuid::new_v4(),
            channel: None,
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
/// 解除挂接并结束；被丢弃时同样会解除挂接。
struct Worker {
    shared: Arc<Shared>,
    client_id: Uuid,
    channel: ChannelId,
    handle: server::Handle,
    attached: HashSet<Uuid>,
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

    async fn request(&mut self, id: Uuid, req: Request) -> Result<(), ()> {
        if self.shared.stopped.load(Ordering::SeqCst) {
            let body = Response::Error {
                code: "host_stopped".into(),
                message: "接收端已停止".into(),
            };
            return self.respond(id, body).await;
        }
        if let Request::Detach { session_id } = &req {
            self.attached.remove(session_id);
            if let Some(task) = self.tasks.remove(session_id) {
                task.abort();
            }
        }
        // 写终端、启动 shell、等待 ConPTY 光标查询都可能阻塞，放到阻塞线程池。
        let manager = self.shared.manager.clone();
        let client_id = self.client_id;
        let outcome = tokio::task::spawn_blocking(move || execute(&manager, client_id, req))
            .await
            .unwrap_or_else(|e| (Err(anyhow::anyhow!("请求处理失败：{e}")), None));
        let (result, forward) = outcome;
        let body = result.unwrap_or_else(|e| Response::Error {
            code: "request_failed".into(),
            message: e.to_string(),
        });
        let attached_id = match &body {
            Response::Attached { session, .. } => Some(session.id),
            _ => None,
        };
        if let Some(session_id) = attached_id {
            self.attached.insert(session_id);
        }
        // 先发挂接响应，再开始转发事件；期间的事件缓存在 rx 里，不会丢。
        self.respond(id, body).await?;
        if let (Some(session_id), Some(rx)) = (attached_id, forward) {
            if let Some(task) = self.tasks.remove(&session_id) {
                task.abort();
            }
            let task = tokio::spawn(forward_events(
                self.handle.clone(),
                self.channel,
                session_id,
                rx,
            ));
            self.tasks.insert(session_id, task);
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        for id in self.attached.drain() {
            let _ = self.shared.manager.detach(id, self.client_id);
        }
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
    }
}

fn execute(
    manager: &SessionManager,
    client_id: Uuid,
    req: Request,
) -> (Result<Response>, Option<broadcast::Receiver<Event>>) {
    let mut forward = None;
    let result = match req {
        Request::List => Ok(Response::Sessions {
            sessions: manager.list(),
        }),
        Request::Create { title, rows, cols } => manager
            .create(title, rows, cols)
            .map(|session| Response::Created { session }),
        Request::Attach { session_id } => manager.attach(session_id, client_id).map(|attachment| {
            forward = Some(attachment.events);
            Response::Attached {
                session: attachment.session,
                screen_b64: attachment.screen_b64,
                seq: attachment.seq,
                has_control: attachment.has_control,
                client_id,
            }
        }),
        Request::Detach { session_id } => manager
            .detach(session_id, client_id)
            .map(|_| Response::Accepted),
        Request::TakeControl { session_id } => manager
            .take_control(session_id, client_id)
            .map(|_| Response::Accepted),
        Request::Send {
            session_id,
            command_id,
            text,
        } => manager
            .send(session_id, client_id, command_id, &text)
            .map(|_| Response::Accepted),
        Request::Resize {
            session_id,
            rows,
            cols,
        } => manager
            .resize(session_id, client_id, rows, cols)
            .map(|_| Response::Accepted),
        Request::Interrupt { session_id } => manager
            .interrupt(session_id, client_id)
            .map(|_| Response::Accepted),
        Request::End { session_id } => manager
            .end_for(session_id, client_id)
            .map(|_| Response::Accepted),
    };
    (result, forward)
}

async fn forward_events(
    handle: server::Handle,
    channel: ChannelId,
    session_id: Uuid,
    mut rx: broadcast::Receiver<Event>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                let Ok(line) = encode(&Frame::Event { body: event }) else {
                    break;
                };
                if handle.data(channel, line).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // Clients reattach for an authoritative snapshot after lag.
                let e = Event::ResyncRequired { session_id };
                let Ok(line) = encode(&Frame::Event { body: e }) else {
                    break;
                };
                let _ = handle.data(channel, line).await;
                break;
            }
            Err(_) => break,
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
                client_id: self.client_id,
                channel,
                handle: session.handle(),
                attached: HashSet::new(),
                tasks: HashMap::new(),
            };
            tokio::spawn(worker.run(rx));
            self.requests = Some(tx);
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
        _session: &mut Session,
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
            let Ok(Frame::Request { id, body }) = decode(&line) else {
                return Err(russh::Error::Disconnect);
            };
            requests
                .try_send((id, body))
                .map_err(|_| russh::Error::Disconnect)?;
        }
        Ok(())
    }
    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.channel == Some(channel) {
            // 关闭队列即通知工作任务收尾并解除挂接。
            self.requests = None;
            self.channel = None;
        }
        Ok(())
    }
}

/// CLI 前台运行；Ctrl+C 触发受控关闭并结束所有终端。
pub async fn run(config: HostConfig, listen: SocketAddr) -> Result<()> {
    let socket = TcpListener::bind(listen)
        .await
        .with_context(|| format!("无法监听 {listen}"))?;
    println!("接收端监听 {listen}（Ctrl+C 停止并结束会话）");
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
    let key =
        russh::keys::load_secret_key(host_key_path(), None).context("加载接收端主机密钥失败")?;
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
