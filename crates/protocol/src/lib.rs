//! TermBridge v2 wire protocol.
//!
//! Frames are newline-delimited JSON. `Input` frames are not requests: they
//! carry raw terminal bytes and are acknowledged with `InputAck` /
//! `InputRejected` events so that input delivery is exactly-once by offset.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SUBSYSTEM: &str = "termbridge-v2";
/// 单帧 JSON（含换行）上限。
pub const MAX_FRAME: usize = 1024 * 1024;
/// 单个 `Input` 帧的原始字节上限。
pub const MAX_INPUT_CHUNK: usize = 16 * 1024;
/// 单个 `Output` 事件的原始字节上限。
pub const MAX_OUTPUT_CHUNK: usize = 64 * 1024;
/// 单个 `SnapshotChunk` 的原始字节上限。
pub const MAX_SNAPSHOT_CHUNK: usize = 256 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: Uuid,
    pub title: String,
    pub created_unix_ms: u64,
    pub live: bool,
    pub rows: u16,
    pub cols: u16,
    /// 当前控制者的 `stream_id`。
    pub controller: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    List,
    Create {
        title: String,
        rows: u16,
        cols: u16,
    },
    /// stream_id：客户端为「这个标签页看这个会话」生成的 UUID，重连时保持不变。
    /// input_base：客户端认为接收端已确认的输入偏移（首次为 0）。
    /// resume_from：客户端已完整显示到的输出偏移；None 表示首次挂接。
    Attach {
        session_id: Uuid,
        stream_id: Uuid,
        input_base: u64,
        resume_from: Option<u64>,
    },
    Detach {
        session_id: Uuid,
    },
    TakeControl {
        session_id: Uuid,
    },
    Resize {
        session_id: Uuid,
        rows: u16,
        cols: u16,
    },
    End {
        session_id: Uuid,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Sessions {
        sessions: Vec<SessionInfo>,
    },
    Created {
        session: SessionInfo,
    },
    /// input_next：接收端对这个 stream 期望的下一个输入偏移。
    /// resumed=true：接下来从 resume_from 开始补发 Output，客户端保留现有画面；
    /// resumed=false：接下来会先收到 SnapshotBegin…SnapshotEnd，客户端需重置画面。
    Attached {
        session: SessionInfo,
        has_control: bool,
        input_next: u64,
        resumed: bool,
    },
    Accepted,
    Error {
        code: String,
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// offset：data 第一个字节在该会话输出流里的偏移（会话创建时为 0）。
    Output {
        session_id: Uuid,
        offset: u64,
        data_b64: String,
    },
    /// 快照：客户端收到 Begin 时 reset 终端并 resize 到 rows×cols，
    /// 依次写入 Chunk，End 之后的 Output 从 offset 开始。
    SnapshotBegin {
        session_id: Uuid,
        offset: u64,
        rows: u16,
        cols: u16,
    },
    SnapshotChunk {
        session_id: Uuid,
        data_b64: String,
    },
    SnapshotEnd {
        session_id: Uuid,
    },
    /// 接收端已接收（写入队列）到 offset（不含）为止的输入。
    InputAck {
        session_id: Uuid,
        stream_id: Uuid,
        offset: u64,
    },
    InputRejected {
        session_id: Uuid,
        stream_id: Uuid,
        offset: u64,
        code: String,
        message: String,
    },
    Resized {
        session_id: Uuid,
        rows: u16,
        cols: u16,
    },
    ControlChanged {
        session_id: Uuid,
        /// 控制者的 `stream_id`。
        controller: Option<Uuid>,
    },
    Ended {
        session_id: Uuid,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frame {
    Request {
        id: Uuid,
        body: Request,
    },
    Response {
        id: Uuid,
        body: Response,
    },
    Event {
        body: Event,
    },
    /// 客户端 → 接收端，没有 Response；结果通过 InputAck / InputRejected 事件返回。
    Input {
        session_id: Uuid,
        stream_id: Uuid,
        offset: u64,
        data_b64: String,
    },
}

pub fn encode(frame: &Frame) -> Result<Vec<u8>, serde_json::Error> {
    let mut line = serde_json::to_vec(frame)?;
    line.push(b'\n');
    Ok(line)
}

pub fn decode(line: &[u8]) -> Result<Frame, serde_json::Error> {
    serde_json::from_slice(line)
}

/// 编码并校验单帧大小（含换行）不超过 [`MAX_FRAME`]。
pub fn encode_checked(frame: &Frame) -> Result<Vec<u8>, EncodeError> {
    let line = encode(frame).map_err(EncodeError::Json)?;
    if line.len() > MAX_FRAME {
        return Err(EncodeError::TooLarge { len: line.len() });
    }
    Ok(line)
}

#[derive(Debug)]
pub enum EncodeError {
    Json(serde_json::Error),
    TooLarge { len: usize },
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::Json(e) => write!(f, "encode frame failed: {e}"),
            EncodeError::TooLarge { len } => {
                write!(f, "frame too large: {len} bytes (max {MAX_FRAME})")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session() -> SessionInfo {
        SessionInfo {
            id: Uuid::new_v4(),
            title: "标题 / title".into(),
            created_unix_ms: 1_700_000_000_000,
            live: true,
            rows: 24,
            cols: 80,
            controller: Some(Uuid::new_v4()),
        }
    }

    #[test]
    fn all_requests_round_trip() {
        let id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let stream_id = Uuid::new_v4();
        let requests = vec![
            Request::List,
            Request::Create {
                title: "t".into(),
                rows: 24,
                cols: 80,
            },
            Request::Attach {
                session_id,
                stream_id,
                input_base: 4096,
                resume_from: Some(1024),
            },
            Request::Attach {
                session_id,
                stream_id,
                input_base: 0,
                resume_from: None,
            },
            Request::Detach { session_id },
            Request::TakeControl { session_id },
            Request::Resize {
                session_id,
                rows: 30,
                cols: 100,
            },
            Request::End { session_id },
        ];
        for body in requests {
            let frame = Frame::Request {
                id,
                body: body.clone(),
            };
            let bytes = encode(&frame).unwrap();
            assert_eq!(bytes.last(), Some(&b'\n'));
            assert_eq!(decode(&bytes).unwrap(), frame);
        }
    }

    #[test]
    fn all_responses_round_trip() {
        let id = Uuid::new_v4();
        let responses = vec![
            Response::Sessions {
                sessions: vec![sample_session()],
            },
            Response::Created {
                session: sample_session(),
            },
            Response::Attached {
                session: sample_session(),
                has_control: true,
                input_next: 128,
                resumed: false,
            },
            Response::Accepted,
            Response::Error {
                code: "not_controller".into(),
                message: "只读".into(),
            },
        ];
        for body in responses {
            let frame = Frame::Response {
                id,
                body: body.clone(),
            };
            let bytes = encode(&frame).unwrap();
            assert_eq!(decode(&bytes).unwrap(), frame);
        }
    }

    #[test]
    fn all_events_round_trip() {
        let session_id = Uuid::new_v4();
        let stream_id = Uuid::new_v4();
        let events = vec![
            Event::Output {
                session_id,
                offset: 65536,
                data_b64: "aGVsbG8=".into(),
            },
            Event::SnapshotBegin {
                session_id,
                offset: 128,
                rows: 24,
                cols: 80,
            },
            Event::SnapshotChunk {
                session_id,
                data_b64: "eA==".into(),
            },
            Event::SnapshotEnd { session_id },
            Event::InputAck {
                session_id,
                stream_id,
                offset: 3,
            },
            Event::InputRejected {
                session_id,
                stream_id,
                offset: 3,
                code: "busy".into(),
                message: "队列满".into(),
            },
            Event::Resized {
                session_id,
                rows: 25,
                cols: 81,
            },
            Event::ControlChanged {
                session_id,
                controller: Some(stream_id),
            },
            Event::ControlChanged {
                session_id,
                controller: None,
            },
            Event::Ended { session_id },
        ];
        for body in events {
            let frame = Frame::Event { body: body.clone() };
            let bytes = encode(&frame).unwrap();
            assert_eq!(decode(&bytes).unwrap(), frame);
        }
    }

    #[test]
    fn input_frame_round_trip() {
        let frame = Frame::Input {
            session_id: Uuid::new_v4(),
            stream_id: Uuid::new_v4(),
            offset: 42,
            data_b64: "G1tB".into(),
        };
        let bytes = encode(&frame).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(decode(&bytes).unwrap(), frame);
    }

    #[test]
    fn oversized_frame_is_rejected_by_encode_checked() {
        let frame = Frame::Event {
            body: Event::SnapshotChunk {
                session_id: Uuid::new_v4(),
                data_b64: "A".repeat(MAX_FRAME),
            },
        };
        let err = encode_checked(&frame).unwrap_err();
        assert!(matches!(err, EncodeError::TooLarge { .. }));
        // 上限内的帧正常通过。
        let ok = Frame::Event {
            body: Event::SnapshotChunk {
                session_id: Uuid::new_v4(),
                data_b64: "A".repeat(1024),
            },
        };
        assert!(encode_checked(&ok).is_ok());
    }

    #[test]
    fn unknown_fields_and_kinds_are_rejected() {
        assert!(decode(br#"{"kind":"nope"}"#).is_err());
        assert!(decode(b"not json").is_err());
    }
}
