#![cfg(windows)]
mod common;
use common::*;
use std::time::Duration;
use termbridge::{client, config, server};
use termbridge_protocol::{Request, Response};
use uuid::Uuid;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test]
async fn cli_raw_attach_round_trip() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let base = std::env::temp_dir();
    let dir_nested = base.join(format!("termbridge-raw-nested-{}", Uuid::new_v4()));
    let dir_host = base.join(format!("termbridge-raw-host-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir_nested).unwrap();
    std::fs::create_dir_all(&dir_host).unwrap();

    // 嵌套 CLI 使用的私钥；host2 用对应公钥认证。
    let key =
        russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap();
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

    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_termbridge"));
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
        "& '{}' --lang en session attach -p p2 --session-id {}\r",
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
            "Attached to",
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
            "left raw mode",
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
