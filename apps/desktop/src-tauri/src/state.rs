//! 桌面壳共享状态：会话表、主机服务任务句柄。
//!
//! 连接共享：所有 GUI 调用共享同一个 `AppState`（Tauri managed state），
//! 每个远端会话对应一个 `Arc<Session>`，pump 任务独占 SSH 客户端。
//! v2：每个标签页固定一个 `stream_id`；输入偏移与已确认偏移在这里维护。

use std::collections::{HashMap, VecDeque};
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
    /// 串行化并发输入：保证偏移分配与帧发送顺序一致。
    pub input_gate: tokio::sync::Mutex<()>,
    /// 已发出但未被 InputAck 确认的输入（按偏移）；给 M4 重连重发用。
    pub input_unacked: Mutex<VecDeque<(u64, Vec<u8>)>>,
}

/// 未确认输入缓冲上限；超出时丢弃最旧的条目（会在报告里注明）。
pub const MAX_UNACKED_INPUT_BYTES: usize = 256 * 1024;

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
        {
            let mut acked = self.input_acked.lock().unwrap();
            if next > *acked {
                *acked = next;
            }
        }
        let mut unacked = self.input_unacked.lock().unwrap();
        while let Some((offset, bytes)) = unacked.front() {
            if offset + bytes.len() as u64 <= next {
                unacked.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn record_input(&self, offset: u64, bytes: &[u8]) {
        let mut unacked = self.input_unacked.lock().unwrap();
        unacked.push_back((offset, bytes.to_vec()));
        let mut total: usize = unacked.iter().map(|(_, bytes)| bytes.len()).sum();
        while total > MAX_UNACKED_INPUT_BYTES {
            match unacked.pop_front() {
                Some((_, dropped)) => total = total.saturating_sub(dropped.len()),
                None => break,
            }
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
