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

    /// 测试用 CLI 可执行文件（cargo test 会一并构建 bin target）。
    #[cfg(windows)]
    fn cli_binary() -> std::path::PathBuf {
        let mut path = std::env::current_exe().unwrap();
        path.pop();
        if path.file_name().map(|n| n == "deps").unwrap_or(false) {
            path.pop();
        }
        path.join("termbridge.exe")
    }

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
            match client.recv_event().await {
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
                match client.recv_event().await {
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
                match client.recv_event().await {
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
        let _env_lock = ENV_LOCK.lock().unwrap();
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
                match first.recv_event().await {
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

    /// Windows 专用端到端测试：把本产品自己的 host 会话当作“真实终端”，
    /// 验证 CLI raw 模式的字节透传、Ctrl+] d 退出与退出后的终端恢复。
    #[cfg(windows)]
    #[tokio::test]
    async fn cli_raw_attach_round_trip() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let base = std::env::temp_dir();
        let dir_nested = base.join(format!("termbridge-raw-nested-{}", Uuid::new_v4()));
        let dir_host = base.join(format!("termbridge-raw-host-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir_nested).unwrap();
        std::fs::create_dir_all(&dir_host).unwrap();

        // 嵌套 CLI 使用的私钥；host2 用对应公钥认证。
        let key =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let private_path = dir_nested.join("id_ed25519");
        std::fs::write(
            &private_path,
            key.to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();

        // host2：被嵌套 CLI 连接的接收端。
        std::env::set_var("LOCALAPPDATA", &dir_host);
        let host2 = config::init_host_with_keys(&key.public_key().to_openssh().unwrap()).unwrap();
        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port2 = listener2.local_addr().unwrap().port();
        let (stop2, rx2) = tokio::sync::oneshot::channel();
        let server2 = tokio::spawn(server::run_on_listener(host2.clone(), listener2, async {
            let _ = rx2.await;
        }));
        let fp2 = client::probe_host("127.0.0.1", port2).await.unwrap();
        assert_eq!(fp2.sha256, host2.fingerprint().unwrap());

        // host1：驱动嵌套 CLI 的会话；同时准备嵌套 CLI 使用的档案与已知指纹。
        std::env::set_var("LOCALAPPDATA", &dir_nested);
        let password = "Raw-Mode-Test-Password-2026!";
        let host1 = config::init_host(password).unwrap();
        let profiles = config::Profiles {
            items: vec![config::Profile {
                id: Uuid::new_v4(),
                name: "p2".into(),
                host: "127.0.0.1".into(),
                port: port2,
                user: host2.username.clone(),
                auth: config::AuthKind::Key,
                key_path: Some(private_path.clone()),
                remember_password: false,
            }],
        };
        config::save_json(&config::profiles_path(), &profiles).unwrap();
        let known = serde_json::json!({
            "entries": { format!("127.0.0.1:{port2}"): fp2.sha256.clone() }
        });
        std::fs::write(
            config::known_hosts_path(),
            serde_json::to_vec(&known).unwrap(),
        )
        .unwrap();
        let listener1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port1 = listener1.local_addr().unwrap().port();
        let (stop1, rx1) = tokio::sync::oneshot::channel();
        let server1 = tokio::spawn(server::run_on_listener(host1.clone(), listener1, async {
            let _ = rx1.await;
        }));

        let exe = cli_binary();
        assert!(exe.is_file(), "CLI 可执行文件不存在: {}", exe.display());

        // host2 上创建 S2（不挂接：让嵌套 CLI 成为控制者）。
        let fp2_client = client::probe_host("127.0.0.1", port2).await.unwrap();
        let mut c2 = client::Client::connect(client::ClientConfig::new(
            "127.0.0.1",
            port2,
            host2.username.clone(),
            client::AuthMethod::PrivateKey {
                path: private_path.to_string_lossy().into_owned(),
                passphrase: None,
            },
            fp2_client,
        ))
        .await
        .unwrap();
        let Response::Created { session: s2 } = c2
            .request(Request::Create {
                title: "raw-target".into(),
                rows: 30,
                cols: 100,
            })
            .await
            .unwrap()
        else {
            panic!("S2 创建失败")
        };

        // host1 上创建 S1 并作为控制者，模拟“真实终端”驱动嵌套 CLI。
        let fp1 = client::probe_host("127.0.0.1", port1).await.unwrap();
        let mut c1 = client::Client::connect(client::ClientConfig::new(
            "127.0.0.1",
            port1,
            host1.username.clone(),
            client::AuthMethod::Password {
                password: password.into(),
            },
            fp1,
        ))
        .await
        .unwrap();
        let Response::Created { session: s1 } = c1
            .request(Request::Create {
                title: "outer".into(),
                rows: 30,
                cols: 100,
            })
            .await
            .unwrap()
        else {
            panic!("S1 创建失败")
        };
        let t1 = Uuid::new_v4();
        let Response::Attached {
            has_control: true, ..
        } = c1
            .request(Request::Attach {
                session_id: s1.id,
                stream_id: t1,
                input_base: 0,
                resume_from: None,
            })
            .await
            .unwrap()
        else {
            panic!("S1 挂接失败")
        };
        let (snapshot_offset, _) = read_snapshot(&mut c1, s1.id).await;
        let mut outer_tracker = OutputTracker::new();
        outer_tracker.expected = Some(snapshot_offset);
        let mut outer_input = 0u64;

        // 在 S1 里运行嵌套 CLI attach 到 S2。
        let launch = format!(
            "& '{}' session attach -p p2 --session-id {}\r",
            exe.display(),
            s2.id
        );
        c1.send_input(s1.id, t1, outer_input, launch.as_bytes())
            .await
            .unwrap();
        outer_input += launch.len() as u64;
        assert!(
            drain_until_answering(
                &mut c1,
                s1.id,
                t1,
                &mut outer_tracker,
                "已附加到",
                Duration::from_secs(25),
                &mut outer_input
            )
            .await,
            "嵌套 CLI 未进入 raw 模式：{}",
            outer_tracker.text
        );

        // 通过 S1 的“键盘”输入，应被 raw CLI 原样转发到 S2。
        let marker_cmd = "Write-Output (\"RAW_\" + \"OK99\")\r";
        c1.send_input(s1.id, t1, outer_input, marker_cmd.as_bytes())
            .await
            .unwrap();
        outer_input += marker_cmd.len() as u64;
        tokio::time::sleep(Duration::from_secs(2)).await;

        // 独立观察 S2：输出环从 0 重放，应该包含命令执行结果。
        let e2 = Uuid::new_v4();
        let Response::Attached {
            has_control: false,
            resumed: true,
            ..
        } = c2
            .request(Request::Attach {
                session_id: s2.id,
                stream_id: e2,
                input_base: 0,
                resume_from: Some(0),
            })
            .await
            .unwrap()
        else {
            panic!("S2 观察挂接失败")
        };
        let mut inner_tracker = OutputTracker::new();
        inner_tracker.expected = Some(0);
        assert!(
            drain_until(
                &mut c2,
                s2.id,
                &mut inner_tracker,
                "RAW_OK99",
                Duration::from_secs(15)
            )
            .await,
            "raw 输入没有到达 S2：{}",
            inner_tracker.text
        );

        // Ctrl+] d 断开：CLI 退出并恢复终端。
        // 注意：这两个字节写进了 S1 的 PTY（虽然被 CLI 本地过滤），偏移要前进。
        c1.send_input(s1.id, t1, outer_input, b"\x1dd")
            .await
            .unwrap();
        outer_input += 2;
        assert!(
            drain_until(
                &mut c1,
                s1.id,
                &mut outer_tracker,
                "已退出 raw 模式",
                Duration::from_secs(15)
            )
            .await,
            "Ctrl+] d 未退出 raw 模式：{}",
            outer_tracker.text
        );

        // 等 attach 进程完全退出：它的 stdin 线程在退出前的瞬间可能仍会吃掉按键。
        tokio::time::sleep(Duration::from_millis(1200)).await;
        // 退出后 S1 的 shell 应恢复行编辑：分片标记只在执行结果里连续出现。
        let after_cmd = "Write-Output (\"AFTER_\" + \"OK88\")\r";
        c1.send_input(s1.id, t1, outer_input, after_cmd.as_bytes())
            .await
            .unwrap();
        assert!(
            drain_until(
                &mut c1,
                s1.id,
                &mut outer_tracker,
                "AFTER_OK88",
                Duration::from_secs(15)
            )
            .await,
            "raw 退出后 shell 未恢复：{}",
            outer_tracker.text
        );

        c1.disconnect().await.unwrap();
        c2.disconnect().await.unwrap();
        let _ = stop1.send(());
        let _ = stop2.send(());
        server1.await.unwrap().unwrap();
        server2.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir_nested);
        let _ = std::fs::remove_dir_all(&dir_host);
    }
}
