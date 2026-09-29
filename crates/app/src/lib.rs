pub mod cli;
pub mod client;
pub mod config;
pub mod server;

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::time::Duration;
    use termbridge_protocol::{Event, Request, Response};
    use uuid::Uuid;

    /// 这些测试通过 LOCALAPPDATA 切换配置目录；串行化避免互相干扰。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 输出偏移跟踪：验证事件偏移连续、无重复、无缺口。
    struct OutputTracker {
        expected: Option<u64>,
        text: String,
    }

    impl OutputTracker {
        fn new() -> Self {
            Self {
                expected: None,
                text: String::new(),
            }
        }

        fn apply(&mut self, offset: u64, data: &[u8]) {
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

        fn contains(&self, needle: &str) -> bool {
            self.text.contains(needle)
        }

        fn next_offset(&self) -> u64 {
            self.expected.unwrap_or(0)
        }
    }

    async fn read_snapshot(client: &mut client::Client, session_id: Uuid) -> (u64, String) {
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
        fn new() -> Self {
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

    async fn drain_until(
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
    async fn drain_until_answering(
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

    #[tokio::test]
    async fn local_two_ends_raw_input_resume_and_key_auth() {
        let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("termbridge-integration-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(windows)]
        std::env::set_var("LOCALAPPDATA", &dir);
        #[cfg(not(windows))]
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let password = "Integration-Only-Password-2026!";
        let host = config::init_host(password).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server::run_on_listener(host.clone(), listener, async {
            let _ = rx.await;
        }));
        let fp = client::probe_host("127.0.0.1", port).await.unwrap();
        assert_eq!(fp.sha256, host.fingerprint().unwrap());
        let make_config = |pin: client::HostFingerprint, pw: &str| {
            client::ClientConfig::new(
                "127.0.0.1",
                port,
                host.username.clone(),
                client::AuthMethod::Password {
                    password: pw.to_string(),
                },
                pin,
            )
        };
        assert!(client::Client::connect(make_config(
            client::HostFingerprint::new("SHA256:untrusted"),
            password
        ))
        .await
        .is_err());
        assert!(
            client::Client::connect(make_config(fp.clone(), "bad-password"))
                .await
                .is_err()
        );

        let mut first = client::Client::connect(make_config(fp.clone(), password))
            .await
            .unwrap();
        let Response::Created { session } = first
            .request(Request::Create {
                title: "test".into(),
                rows: 24,
                cols: 80,
            })
            .await
            .unwrap()
        else {
            panic!("create rejected")
        };
        let id = session.id;
        // 同一连接只服务一个通道：多开必须立即被拒绝，不能让客户端挂住。
        let extra = tokio::time::timeout(Duration::from_secs(5), first.open_extra_channel()).await;
        assert!(
            matches!(extra, Ok(Err(_))),
            "a second channel on one connection was not rejected"
        );

        let stream_id = Uuid::new_v4();
        let Response::Attached {
            has_control,
            input_next,
            resumed,
            ..
        } = first
            .request(Request::Attach {
                session_id: id,
                stream_id,
                input_base: 0,
                resume_from: None,
            })
            .await
            .unwrap()
        else {
            panic!("attach rejected")
        };
        assert!(has_control, "first attach should control the session");
        assert!(!resumed, "first attach must start from a snapshot");
        assert_eq!(input_next, 0);
        let (snapshot_offset, snapshot) = read_snapshot(&mut first, id).await;
        assert!(!snapshot.is_empty());

        let mut tracker = OutputTracker::new();
        tracker.expected = Some(snapshot_offset);
        #[cfg(windows)]
        let command = "Write-Output TB_INTEGRATION_731\r";
        #[cfg(not(windows))]
        let command = "echo TB_INTEGRATION_731\r";
        first
            .send_input(id, stream_id, 0, command.as_bytes())
            .await
            .unwrap();
        let mut input_offset = command.len() as u64;
        let mut responder = QueryResponder::new();
        // 在同一循环里等待 ack 与输出，避免丢弃任一事件；同时像 xterm.js 一样应答查询。
        let mut acked = false;
        let received = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match first.recv_event().await.expect("unexpected event loss") {
                    Some(Event::InputAck {
                        session_id: sid,
                        stream_id: st,
                        offset,
                    }) if sid == id && st == stream_id => {
                        if offset == command.len() as u64 {
                            acked = true;
                        }
                    }
                    Some(Event::Output {
                        session_id: sid,
                        offset,
                        data_b64,
                    }) if sid == id => {
                        let data = base64::engine::general_purpose::STANDARD
                            .decode(data_b64)
                            .unwrap();
                        tracker.apply(offset, &data);
                        for reply in responder.push(&data) {
                            first
                                .send_input(id, stream_id, input_offset, &reply)
                                .await
                                .unwrap();
                            input_offset += reply.len() as u64;
                        }
                        if acked && tracker.contains("TB_INTEGRATION_731") {
                            break true;
                        }
                    }
                    Some(_) => {}
                    None => break false,
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(acked, "no input ack");
        assert!(
            received,
            "the terminal produced no output: {}",
            tracker.text
        );
        let resume_point = tracker.next_offset();

        assert!(matches!(
            first
                .request(Request::Detach { session_id: id })
                .await
                .unwrap(),
            Response::Accepted
        ));
        first.disconnect().await.unwrap();

        // 重连：同一 stream 恢复控制，resume_from 从已显示偏移重放。
        let mut second = client::Client::connect(make_config(fp, password))
            .await
            .unwrap();
        let Response::Attached {
            has_control: has_control2,
            resumed: resumed2,
            input_next: input_next2,
            ..
        } = second
            .request(Request::Attach {
                session_id: id,
                stream_id,
                input_base: input_offset,
                resume_from: Some(resume_point),
            })
            .await
            .unwrap()
        else {
            panic!("reattach rejected")
        };
        assert!(
            has_control2,
            "same stream should retain control after grace"
        );
        assert!(resumed2, "resume_from inside the output ring must replay");
        // input_next 包含命令与客户端应答查询的字节。
        assert_eq!(input_next2, input_offset);

        #[cfg(windows)]
        let second_command = "Write-Output TB_RESUME_952\r";
        #[cfg(not(windows))]
        let second_command = "echo TB_RESUME_952\r";
        second
            .send_input(id, stream_id, input_next2, second_command.as_bytes())
            .await
            .unwrap();
        let mut tracker2 = OutputTracker::new();
        tracker2.expected = Some(resume_point);
        assert!(
            drain_until(
                &mut second,
                id,
                &mut tracker2,
                "TB_RESUME_952",
                Duration::from_secs(15)
            )
            .await,
            "no output after resume: {}",
            tracker2.text
        );
        // OutputTracker::apply 已断言重放与实时输出的偏移连续，无需再比较长度。
        assert!(tracker2.next_offset() > resume_point);
        assert!(matches!(
            second
                .request(Request::End { session_id: id })
                .await
                .unwrap(),
            Response::Accepted
        ));
        let _ = stop.send(());
        task.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        // 迁移必须先完整校验授权文件；无效条目不能关闭仍在使用的密码。
        let migration_key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let migration_public = migration_key.public_key().to_openssh().unwrap();
        let mut migrated = host.clone();
        let old_key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let old_public = old_key.public_key().to_openssh().unwrap();
        migrated.authorized_keys.push(old_public.clone());
        assert!(config::switch_to_keys_only(
            &mut migrated,
            &format!("from=\"192.0.2.1\" {migration_public}")
        )
        .is_err());
        assert!(migrated.password_hash.is_some());
        assert_eq!(migrated.authorized_keys, vec![old_public]);
        config::switch_to_keys_only(&mut migrated, &migration_public).unwrap();
        assert!(migrated.password_hash.is_none());
        assert_eq!(migrated.authorized_keys, vec![migration_public]);
        let saved: config::HostConfig = config::read_json(&config::host_path()).unwrap();
        assert!(saved.password_hash.is_none());

        // 密钥专用接收端：复用一把现有 SSH 公私钥，不设置任何产品密码。
        let key_dir =
            std::env::temp_dir().join(format!("termbridge-key-integration-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&key_dir).unwrap();
        #[cfg(windows)]
        std::env::set_var("LOCALAPPDATA", &key_dir);
        #[cfg(not(windows))]
        std::env::set_var("XDG_CONFIG_HOME", &key_dir);
        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let public = key.public_key().to_openssh().unwrap();
        let private_path = key_dir.join("existing-ssh-key");
        std::fs::write(
            &private_path,
            key.to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&private_path, std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let key_host = config::init_host_with_keys(&public).unwrap();
        assert!(key_host.password_hash.is_none());
        assert_eq!(key_host.authorized_keys.len(), 1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server::run_on_listener(key_host.clone(), listener, async {
            let _ = rx.await;
        }));
        let fp = client::probe_host("127.0.0.1", port).await.unwrap();
        assert!(client::Client::connect(client::ClientConfig::new(
            "127.0.0.1",
            port,
            key_host.username.clone(),
            client::AuthMethod::Password {
                password: password.into()
            },
            fp.clone(),
        ))
        .await
        .is_err());
        let mut key_client = client::Client::connect(client::ClientConfig::new(
            "127.0.0.1",
            port,
            key_host.username,
            client::AuthMethod::PrivateKey {
                path: private_path.to_string_lossy().into_owned(),
                passphrase: None,
            },
            fp,
        ))
        .await
        .unwrap();
        assert!(matches!(
            key_client.request(Request::List).await.unwrap(),
            Response::Sessions { .. }
        ));
        key_client.disconnect().await.unwrap();
        let _ = stop.send(());
        task.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&key_dir);
    }

    #[tokio::test]
    async fn input_ack_survives_sustained_output() {
        let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("termbridge-flood-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(windows)]
        std::env::set_var("LOCALAPPDATA", &dir);
        #[cfg(not(windows))]
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let secret = Uuid::new_v4().to_string();
        let host = config::init_host(&secret).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(server::run_on_listener(host.clone(), listener, async {
            let _ = rx.await;
        }));
        let fp = client::probe_host("127.0.0.1", port).await.unwrap();
        let mut client = client::Client::connect(client::ClientConfig::new(
            "127.0.0.1",
            port,
            host.username,
            client::AuthMethod::Password { password: secret },
            fp,
        ))
        .await
        .unwrap();
        let Response::Created { session } = client
            .request(Request::Create {
                title: "flood".into(),
                rows: 24,
                cols: 80,
            })
            .await
            .unwrap()
        else {
            panic!("create failed")
        };
        let sid = session.id;
        let stream = Uuid::new_v4();
        assert!(matches!(
            client
                .request(Request::Attach {
                    session_id: sid,
                    stream_id: stream,
                    input_base: 0,
                    resume_from: None,
                })
                .await
                .unwrap(),
            Response::Attached {
                has_control: true,
                ..
            }
        ));
        let _ = read_snapshot(&mut client, sid).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        #[cfg(windows)]
        let command = b"while($true){'x'*200}\r".as_slice();
        #[cfg(not(windows))]
        let command =
            b"yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r".as_slice();
        client.send_input(sid, stream, 0, command).await.unwrap();
        let mut next = command.len() as u64;
        let mut outputs = 0;
        let mut responder = QueryResponder::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while outputs < 10 {
                match client.recv_event().await.expect("unexpected event loss") {
                    Some(Event::Output {
                        session_id,
                        data_b64,
                        ..
                    }) if session_id == sid => {
                        outputs += 1;
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(data_b64)
                            .unwrap();
                        for reply in responder.push(&bytes) {
                            client.send_input(sid, stream, next, &reply).await.unwrap();
                            next += reply.len() as u64;
                        }
                    }
                    Some(_) => {}
                    None => panic!("disconnected during output"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("command did not produce sustained output: {outputs} output events")
        });
        // 每次发送后等 ACK：旧的 handler 在已满的 Handle 队列里等待自身排空会卡死。
        for _ in 0..50 {
            client.send_input(sid, stream, next, b"z").await.unwrap();
            next += 1;
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match client.recv_event().await.expect("unexpected event loss") {
                        Some(Event::InputAck {
                            session_id,
                            stream_id,
                            offset,
                        }) if session_id == sid && stream_id == stream && offset == next => break,
                        Some(Event::InputRejected { code, .. }) => panic!("input rejected: {code}"),
                        Some(_) => {}
                        None => panic!("disconnected while awaiting ACK"),
                    }
                }
            })
            .await
            .expect("InputAck stalled under output pressure");
        }
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), client.request(Request::List))
                .await
                .expect("List stalled under output pressure")
                .unwrap(),
            Response::Sessions { .. }
        ));
        client.send_input(sid, stream, next, b"\x03").await.unwrap();
        client.disconnect().await.unwrap();
        let _ = stop.send(());
        server.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
    #[tokio::test]
    async fn observer_input_then_take_control_keeps_offset_usable() {
        let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("termbridge-observer-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(windows)]
        std::env::set_var("LOCALAPPDATA", &dir);
        #[cfg(not(windows))]
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let secret = Uuid::new_v4().to_string();
        let host = config::init_host(&secret).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(server::run_on_listener(host.clone(), listener, async {
            let _ = rx.await;
        }));
        let fp = client::probe_host("127.0.0.1", port).await.unwrap();
        let connect = || {
            client::ClientConfig::new(
                "127.0.0.1",
                port,
                host.username.clone(),
                client::AuthMethod::Password {
                    password: secret.clone(),
                },
                fp.clone(),
            )
        };
        let mut first = client::Client::connect(connect()).await.unwrap();
        let mut second = client::Client::connect(connect()).await.unwrap();
        let Response::Created { session } = first
            .request(Request::Create {
                title: "observer".into(),
                rows: 24,
                cols: 80,
            })
            .await
            .unwrap()
        else {
            panic!("create failed")
        };
        let sid = session.id;
        first
            .request(Request::Attach {
                session_id: sid,
                stream_id: Uuid::new_v4(),
                input_base: 0,
                resume_from: None,
            })
            .await
            .unwrap();
        let observer = Uuid::new_v4();
        assert!(matches!(
            second
                .request(Request::Attach {
                    session_id: sid,
                    stream_id: observer,
                    input_base: 0,
                    resume_from: None,
                })
                .await
                .unwrap(),
            Response::Attached {
                has_control: false,
                ..
            }
        ));
        let _ = read_snapshot(&mut second, sid).await;
        second.send_input(sid, observer, 0, b"nope").await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match second.recv_event().await.expect("unexpected event loss") {
                    Some(Event::InputRejected {
                        session_id,
                        stream_id,
                        offset: 4,
                        code,
                        ..
                    }) if session_id == sid
                        && stream_id == observer
                        && code == "not_controller" =>
                    {
                        break
                    }
                    Some(_) => {}
                    None => panic!("observer disconnected"),
                }
            }
        })
        .await
        .expect("observer rejection was not returned");
        assert!(matches!(
            second
                .request(Request::TakeControl { session_id: sid })
                .await
                .unwrap(),
            Response::Accepted
        ));
        #[cfg(windows)]
        let command = b"Write-Output ('TAKE_' + 'YES')\r".as_slice();
        #[cfg(not(windows))]
        let command = b"printf 'TAKE_%s\n' YES\r".as_slice();
        second.send_input(sid, observer, 4, command).await.unwrap();
        let mut next = 4 + command.len() as u64;
        let mut output = OutputTracker::new();
        assert!(
            drain_until_answering(
                &mut second,
                sid,
                observer,
                &mut output,
                "TAKE_YES",
                Duration::from_secs(15),
                &mut next
            )
            .await,
            "input after taking control did not execute"
        );
        second.disconnect().await.unwrap();
        first.disconnect().await.unwrap();
        let _ = stop.send(());
        server.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
    #[tokio::test]
    async fn replacement_connection_and_stream_survive_old_worker_cleanup() {
        let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("termbridge-attachment-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(windows)]
        std::env::set_var("LOCALAPPDATA", &dir);
        #[cfg(not(windows))]
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let secret = Uuid::new_v4().to_string();
        let host = config::init_host(&secret).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(server::run_on_listener(host.clone(), listener, async {
            let _ = rx.await;
        }));
        let fp = client::probe_host("127.0.0.1", port).await.unwrap();
        let connect = || {
            client::ClientConfig::new(
                "127.0.0.1",
                port,
                host.username.clone(),
                client::AuthMethod::Password {
                    password: secret.clone(),
                },
                fp.clone(),
            )
        };
        let mut first = client::Client::connect(connect()).await.unwrap();
        let mut second = client::Client::connect(connect()).await.unwrap();
        let Response::Created { session } = first
            .request(Request::Create {
                title: "token".into(),
                rows: 24,
                cols: 80,
            })
            .await
            .unwrap()
        else {
            panic!("create failed")
        };
        let sid = session.id;
        let stream1 = Uuid::new_v4();
        first
            .request(Request::Attach {
                session_id: sid,
                stream_id: stream1,
                input_base: 0,
                resume_from: None,
            })
            .await
            .unwrap();
        assert!(matches!(
            second
                .request(Request::Attach {
                    session_id: sid,
                    stream_id: stream1,
                    input_base: 0,
                    resume_from: Some(0)
                })
                .await
                .unwrap(),
            Response::Attached {
                has_control: true,
                ..
            }
        ));
        first.disconnect().await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        second.send_input(sid, stream1, 0, b"x").await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match second.recv_event().await.expect("unexpected event loss") {
                    Some(Event::InputAck {
                        session_id,
                        stream_id,
                        offset: 1,
                    }) if session_id == sid && stream_id == stream1 => break,
                    Some(Event::InputRejected { code, .. }) => {
                        panic!("replacement rejected: {code}")
                    }
                    Some(_) => {}
                    None => panic!("replacement disconnected"),
                }
            }
        })
        .await
        .expect("replacement was detached by old Worker");
        let stream2 = Uuid::new_v4();
        second
            .request(Request::Attach {
                session_id: sid,
                stream_id: stream2,
                input_base: 0,
                resume_from: Some(0),
            })
            .await
            .unwrap();
        second.send_input(sid, stream1, 1, b"y").await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match second.recv_event().await.expect("unexpected event loss") {
                    Some(Event::InputRejected {
                        session_id,
                        stream_id,
                        code,
                        ..
                    }) if session_id == sid && stream_id == stream1 && code == "not_controller" => {
                        break
                    }
                    Some(Event::InputAck {
                        session_id,
                        stream_id,
                        ..
                    }) if session_id == sid && stream_id == stream1 => {
                        panic!("old stream still online")
                    }
                    Some(_) => {}
                    None => panic!("disconnected"),
                }
            }
        })
        .await
        .expect("old stream was not detached");
        assert!(matches!(
            second
                .request(Request::TakeControl { session_id: sid })
                .await
                .unwrap(),
            Response::Accepted
        ));
        second.send_input(sid, stream2, 0, b"z").await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match second.recv_event().await.expect("unexpected event loss") {
                    Some(Event::InputAck {
                        session_id,
                        stream_id,
                        offset: 1,
                    }) if session_id == sid && stream_id == stream2 => break,
                    Some(_) => {}
                    None => panic!("disconnected"),
                }
            }
        })
        .await
        .expect("new stream was not usable");
        second.disconnect().await.unwrap();
        let _ = stop.send(());
        server.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
