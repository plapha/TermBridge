use base64::Engine as _;
use std::time::Duration;
use termbridge::client;
use termbridge_protocol::Event;
use uuid::Uuid;

/// 输出偏移跟踪：验证事件偏移连续、无重复、无缺口。
pub struct OutputTracker {
    pub expected: Option<u64>,
    pub text: String,
}

impl OutputTracker {
    pub fn new() -> Self {
        Self {
            expected: None,
            text: String::new(),
        }
    }

    pub fn apply(&mut self, offset: u64, data: &[u8]) {
        let end = offset + data.len() as u64;
        match self.expected {
            None => self.expected = Some(end),
            Some(prev) => {
                assert_eq!(offset, prev, "输出偏移不连续");
                self.expected = Some(end);
            }
        }
        self.text.push_str(&String::from_utf8_lossy(data));
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.text.contains(needle)
    }

    pub fn next_offset(&self) -> u64 {
        self.expected.unwrap_or(0)
    }
}

pub async fn read_snapshot(client: &mut client::Client, session_id: Uuid) -> (u64, String) {
    let mut offset = None;
    let mut data: Vec<u8> = Vec::new();
    loop {
        match client.recv_event().await.expect("unexpected event loss") {
            Some(Event::SnapshotBegin {
                session_id: sid,
                offset: o,
                ..
            }) if sid == session_id => offset = Some(o),
            Some(Event::SnapshotChunk {
                session_id: sid,
                data_b64,
            }) if sid == session_id => data.extend_from_slice(
                &base64::engine::general_purpose::STANDARD
                    .decode(data_b64)
                    .unwrap(),
            ),
            Some(Event::SnapshotEnd { session_id: sid }) if sid == session_id => break,
            Some(_) => {}
            None => panic!("disconnected while reading the snapshot"),
        }
    }
    (
        offset.expect("snapshot begin was never received"),
        String::from_utf8_lossy(&data).into_owned(),
    )
}

/// 模拟 xterm.js：控制器在线时由客户端应答终端的查询。
/// 只处理 CSI 6n（光标位置）与 DA1（设备属性），其余查询不代答。
struct QueryResponder {
    scanned: Vec<u8>,
    consumed: usize,
}

impl QueryResponder {
    pub fn new() -> Self {
        Self {
            scanned: Vec::new(),
            consumed: 0,
        }
    }

    fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.scanned.extend_from_slice(data);
        let mut replies = Vec::new();
        loop {
            let rest = &self.scanned[self.consumed..];
            let candidates: [(&[u8], &[u8]); 3] = [
                (b"\x1b[6n", b"\x1b[1;1R"),
                (b"\x1b[0c", b"\x1b[?1;2c"),
                (b"\x1b[c", b"\x1b[?1;2c"),
            ];
            let mut earliest: Option<(usize, usize, &[u8])> = None;
            for (pattern, reply) in candidates {
                if let Some(pos) = rest.windows(pattern.len()).position(|w| w == pattern) {
                    let end = pos + pattern.len();
                    if earliest.map(|(p, _, _)| pos < p).unwrap_or(true) {
                        earliest = Some((pos, end, reply));
                    }
                }
            }
            match earliest {
                Some((_, end, reply)) => {
                    self.consumed += end;
                    replies.push(reply.to_vec());
                }
                None => break,
            }
        }
        replies
    }
}

pub async fn drain_until(
    client: &mut client::Client,
    session_id: Uuid,
    tracker: &mut OutputTracker,
    needle: &str,
    timeout: Duration,
) -> bool {
    tokio::time::timeout(timeout, async {
        loop {
            match client.recv_event().await.expect("unexpected event loss") {
                Some(Event::Output {
                    session_id: sid,
                    offset,
                    data_b64,
                }) if sid == session_id => {
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(data_b64)
                        .unwrap();
                    tracker.apply(offset, &data);
                    if tracker.contains(needle) {
                        break true;
                    }
                }
                Some(_) => {}
                None => break false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// 与 `drain_until` 相同，但像 xterm.js 一样应答终端的启动查询。
pub async fn drain_until_answering(
    client: &mut client::Client,
    session_id: Uuid,
    stream_id: Uuid,
    tracker: &mut OutputTracker,
    needle: &str,
    timeout: Duration,
    input_offset: &mut u64,
) -> bool {
    let mut responder = QueryResponder::new();
    tokio::time::timeout(timeout, async {
        loop {
            match client.recv_event().await.expect("unexpected event loss") {
                Some(Event::Output {
                    session_id: sid,
                    offset,
                    data_b64,
                }) if sid == session_id => {
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(data_b64)
                        .unwrap();
                    tracker.apply(offset, &data);
                    for reply in responder.push(&data) {
                        client
                            .send_input(session_id, stream_id, *input_offset, &reply)
                            .await
                            .unwrap();
                        *input_offset += reply.len() as u64;
                    }
                    if tracker.contains(needle) {
                        break true;
                    }
                }
                Some(_) => {}
                None => break false,
            }
        }
    })
    .await
    .unwrap_or(false)
}
