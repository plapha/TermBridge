use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SUBSYSTEM: &str = "termbridge-v1";
pub const MAX_FRAME: usize = 1024 * 1024;
pub const MAX_SEND_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: Uuid,
    pub title: String,
    pub created_unix_ms: u64,
    pub live: bool,
    pub rows: u16,
    pub cols: u16,
    pub controller: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    List,
    Create {
        title: String,
        rows: u16,
        cols: u16,
    },
    Attach {
        session_id: Uuid,
    },
    Detach {
        session_id: Uuid,
    },
    TakeControl {
        session_id: Uuid,
    },
    Send {
        session_id: Uuid,
        command_id: Uuid,
        text: String,
    },
    Resize {
        session_id: Uuid,
        rows: u16,
        cols: u16,
    },
    Interrupt {
        session_id: Uuid,
    },
    End {
        session_id: Uuid,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Sessions {
        sessions: Vec<SessionInfo>,
    },
    Created {
        session: SessionInfo,
    },
    Attached {
        session: SessionInfo,
        screen_b64: String,
        seq: u64,
        has_control: bool,
        client_id: Uuid,
    },
    Accepted,
    Error {
        code: String,
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Output {
        session_id: Uuid,
        seq: u64,
        data_b64: String,
    },
    Ended {
        session_id: Uuid,
    },
    ResyncRequired {
        session_id: Uuid,
    },
    ControlChanged {
        session_id: Uuid,
        controller: Option<Uuid>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frame {
    Request { id: Uuid, body: Request },
    Response { id: Uuid, body: Response },
    Event { body: Event },
}

pub fn encode(frame: &Frame) -> Result<Vec<u8>, serde_json::Error> {
    let mut line = serde_json::to_vec(frame)?;
    line.push(b'\n');
    Ok(line)
}

pub fn decode(line: &[u8]) -> Result<Frame, serde_json::Error> {
    serde_json::from_slice(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip_and_delimiter() {
        let frame = Frame::Request {
            id: Uuid::new_v4(),
            body: Request::Send {
                session_id: Uuid::new_v4(),
                command_id: Uuid::new_v4(),
                text: "你好\nworld".into(),
            },
        };
        let bytes = encode(&frame).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert!(matches!(
            decode(&bytes).unwrap(),
            Frame::Request {
                body: Request::Send { .. },
                ..
            }
        ));
    }
}
