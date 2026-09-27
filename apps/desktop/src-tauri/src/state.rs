//! 桌面壳共享状态：会话表、主机服务任务句柄。
//!
//! 连接共享：所有 GUI 调用共享同一个 `AppState`（Tauri managed state），
//! 每个远端会话对应一个 `Arc<Session>`，pump 任务独占 SSH 客户端。

use std::collections::HashMap;
use std::sync::{Mutex, atomic::AtomicBool};

use serde::Serialize;
use termbridge_protocol::Request;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub type RespResult = Result<termbridge_protocol::Response, String>;

/// 发给 pump 任务的控制命令。
pub enum PumpCmd {
    Request(Request, oneshot::Sender<RespResult>),
    Close(oneshot::Sender<()>),
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Connecting,
    Attached,
    Detached,
    Closed,
    Error,
}

/// 与前端 types.ts 的 SessionInfo 对齐。
#[derive(Clone, Debug, Serialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub profile_id: String,
    pub profile_name: String,
    pub state: SessionState,
    pub controller: Option<String>,
    pub is_controller: bool,
    pub online: bool,
}

pub struct Session {
    pub meta: Mutex<SessionMeta>,
    pub tx: mpsc::Sender<PumpCmd>,
    pub last_seq: Mutex<u64>,
    pub client_id: Mutex<Option<uuid::Uuid>>,
}

impl Session {
    pub fn info(&self) -> SessionMeta {
        self.meta.lock().unwrap().clone()
    }
    pub fn set_meta(&self, f: impl FnOnce(&mut SessionMeta)) {
        let mut meta = self.meta.lock().unwrap();
        f(&mut meta);
    }
}

#[derive(Default)]
pub struct AppState {
    pub sessions: Mutex<HashMap<String, std::sync::Arc<Session>>>,
    /// 接收端（host server）后台任务的句柄；None 表示未启动。
    pub host_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    pub host_stop: Mutex<Option<oneshot::Sender<()>>>,
    pub host_running: AtomicBool,
}
