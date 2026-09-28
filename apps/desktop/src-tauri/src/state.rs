//! 桌面壳共享状态：会话表、主机服务任务句柄。
//!
//! 连接共享：所有 GUI 调用共享同一个 `AppState`（Tauri managed state），
//! 每个远端会话对应一个 `Arc<Session>`，pump 任务独占 SSH 客户端。
//! v2：每个标签页固定一个 `stream_id`；输入偏移与已确认偏移在这里维护。

use std::collections::HashMap;
use std::sync::{atomic::AtomicBool, Mutex};

use serde::Serialize;
use termbridge::client::InputSender;
use termbridge_protocol::Request;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use uuid::Uuid;

pub type RespResult = Result<termbridge_protocol::Response, String>;

/// 发给 pump 任务的控制命令。
pub enum PumpCmd {
    Request(Request, oneshot::Sender<RespResult>),
    /// 原始终端输入字节；由 pump 切块并按偏移发送。
    Input(Vec<u8>),
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
    /// 本标签页固定的输入流 ID；重挂时保持不变。
    pub stream_id: Uuid,
    /// 下一个待发送的输入偏移。
    pub input_offset: Mutex<u64>,
    /// 接收端已确认的输入偏移。
    pub input_acked: Mutex<u64>,
    /// 与请求通道共享写半边的输入句柄，不等待响应。
    pub input_sender: InputSender,
}

impl Session {
    pub fn info(&self) -> SessionMeta {
        self.meta.lock().unwrap().clone()
    }

    pub fn set_meta(&self, f: impl FnOnce(&mut SessionMeta)) {
        let mut meta = self.meta.lock().unwrap();
        f(&mut meta);
    }

    pub fn next_input_offset(&self, len: usize) -> u64 {
        let mut offset = self.input_offset.lock().unwrap();
        let at = *offset;
        *offset += len as u64;
        at
    }

    pub fn ack_input(&self, next: u64) {
        let mut acked = self.input_acked.lock().unwrap();
        if next > *acked {
            *acked = next;
        }
    }

    pub fn is_controller(&self, controller: Option<Uuid>) -> bool {
        controller == Some(self.stream_id)
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
