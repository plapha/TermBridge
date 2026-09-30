//! Agent 接口（`--json`、`--password-stdin`、`--trust-fingerprint`、`session send/read`）的端到端测试：
//! 接收端在本进程里运行，CLI 作为子进程被驱动，和 Agent 的用法一致。
#![cfg(unix)]

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;
use termbridge::{config, server};
use uuid::Uuid;

const PASSWORD: &str = "Agent-Interface-Test-Password-2026!";

/// 运行 CLI（始终 `--json`），返回解析后的 stdout 与进程是否成功。
async fn run(args: &[&str], stdin: Option<&str>) -> (Value, bool) {
    let exe = env!("CARGO_BIN_EXE_termbridge").to_string();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let stdin = stdin.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let mut child = Command::new(exe)
            .arg("--json")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            let mut pipe = child.stdin.take().unwrap();
            if let Some(text) = stdin {
                pipe.write_all(text.as_bytes()).unwrap();
            }
        }
        let output = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let value: Value = serde_json::from_str(text.trim())
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {text:?}"));
        (value, output.status.success())
    })
    .await
    .unwrap()
}

fn code(value: &Value) -> &str {
    value["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn agent_can_create_send_read_wait_and_end() {
    let dir = std::env::temp_dir().join(format!("termbridge-agent-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &dir);
    if std::env::var_os("USER").is_none() {
        std::env::set_var("USER", "agent-test");
    }
    let host = config::init_host(PASSWORD).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let (stop, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(server::run_on_listener(host.clone(), listener, async {
        let _ = rx.await;
    }));
    let pw = format!("{PASSWORD}\n");
    let pw = Some(pw.as_str());

    let (v, ok) = run(
        &[
            "profile",
            "add",
            "p",
            "127.0.0.1",
            "--port",
            &port,
            "-u",
            &host.username,
            "--auth",
            "password",
        ],
        None,
    )
    .await;
    assert!(ok, "{v}");

    // 未知主机：不提示，返回待核对的指纹；指纹不符则拒绝；带正确指纹才信任。
    let fingerprint = host.fingerprint().unwrap();
    let (v, ok) = run(&["session", "list", "-p", "p", "--password-stdin"], pw).await;
    assert!(!ok);
    assert_eq!(code(&v), "fingerprint_untrusted", "{v}");
    assert_eq!(v["error"]["details"]["fingerprint"], fingerprint.as_str());
    let (v, _) = run(
        &[
            "session",
            "list",
            "-p",
            "p",
            "--password-stdin",
            "--trust-fingerprint",
            "SHA256:wrong",
        ],
        pw,
    )
    .await;
    assert_eq!(code(&v), "fingerprint_mismatch", "{v}");

    // 缺少密码时不提示（否则会挂起），而是报错。
    let (v, _) = run(
        &[
            "session",
            "list",
            "-p",
            "p",
            "--trust-fingerprint",
            &fingerprint,
        ],
        None,
    )
    .await;
    assert_eq!(code(&v), "prompt_required", "{v}");
    let (v, ok) = run(
        &[
            "session",
            "list",
            "-p",
            "p",
            "--password-stdin",
            "--trust-fingerprint",
            &fingerprint,
        ],
        pw,
    )
    .await;
    assert!(ok, "{v}");
    assert_eq!(v["sessions"].as_array().unwrap().len(), 0);
    // 指纹已记录，之后不必再带参数。
    let (v, ok) = run(&["session", "list", "-p", "p", "--password-stdin"], pw).await;
    assert!(ok, "{v}");
    // 密码错误是连接失败。
    let (v, ok) = run(
        &["session", "list", "-p", "p", "--password-stdin"],
        Some("wrong-password\n"),
    )
    .await;
    assert!(!ok);
    assert_eq!(code(&v), "connect_failed", "{v}");

    // 新开一个标签页（会话）。
    let (v, ok) = run(
        &[
            "session",
            "create",
            "-p",
            "p",
            "--title",
            "agent",
            "--password-stdin",
        ],
        pw,
    )
    .await;
    assert!(ok, "{v}");
    let sid = v["session"]["id"].as_str().unwrap().to_string();
    let common = |extra: &[&str]| {
        let mut args = vec!["session".to_string()];
        args.extend(extra.iter().map(|s| s.to_string()));
        args.extend(["-p", "p", "--session-id", &sid, "--password-stdin"].map(String::from));
        args
    };
    async fn go(args: Vec<String>, pw: Option<&str>) -> (Value, bool) {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run(&refs, pw).await
    }

    // 发送命令并等到空闲：输出、屏幕、偏移都在一次调用里返回。
    let (v, ok) = go(
        common(&[
            "send",
            "--take-control",
            "--text",
            "echo AGENT_$((40+2))",
            "--enter",
        ]),
        pw,
    )
    .await;
    assert!(ok, "{v}");
    assert_eq!(v["reason"], "idle", "{v}");
    assert!(v["output"].as_str().unwrap().contains("AGENT_42"), "{v}");
    let screen: Vec<&str> = v["screen"]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert!(screen.iter().any(|l| *l == "AGENT_42"), "{screen:?}");
    assert_eq!(v["sent_bytes"], 21);
    let offset = v["offset"].as_u64().unwrap();

    // 另一个客户端持有控制权时必须显式接管。
    let (v, ok) = go(common(&["send", "--text", "echo nope", "--enter"]), pw).await;
    assert!(!ok);
    assert_eq!(code(&v), "not_controller", "{v}");

    // 等待正则（按行匹配）：命令睡一会儿再输出，不能提前返回。
    let (v, ok) = go(
        common(&[
            "send",
            "--take-control",
            "--text",
            "sleep 1; echo DONE_$((6*7))",
            "--enter",
            "--wait-for",
            "^DONE_42$",
            "--timeout",
            "15",
        ]),
        pw,
    )
    .await;
    assert!(ok, "{v}");
    assert_eq!(v["reason"], "match", "{v}");

    // 读取当前屏幕，以及某个偏移之后的新输出（不含之前的内容）。
    let (v, _) = go(common(&["read"]), pw).await;
    assert_eq!(v["reason"], "immediate", "{v}");
    assert!(v["screen"]["lines"].to_string().contains("DONE_42"), "{v}");
    let (v, _) = go(common(&["read", "--since", &offset.to_string()]), pw).await;
    assert!(v["screen"].is_null(), "{v}");
    assert_eq!(v["since_unavailable"], false);
    let text = v["output"].as_str().unwrap();
    assert!(
        text.contains("DONE_42") && !text.contains("AGENT_42"),
        "{text:?}"
    );

    // 按键：Ctrl+C 打断长命令。
    let (_, ok) = go(
        common(&[
            "send",
            "--take-control",
            "--text",
            "sleep 60",
            "--enter",
            "--no-wait",
        ]),
        pw,
    )
    .await;
    assert!(ok);
    let (v, ok) = go(
        common(&[
            "send",
            "--take-control",
            "--key",
            "ctrl-c",
            "--wait-for",
            "\\^C",
        ]),
        pw,
    )
    .await;
    assert!(ok, "{v}");
    assert_eq!(v["reason"], "match", "{v}");

    // 参数错误在连接之前就被拒绝；不存在的会话有明确错误码。
    let (v, _) = go(common(&["send", "--key", "no-such-key"]), pw).await;
    assert_eq!(code(&v), "invalid_args");
    let (v, _) = go(common(&["send"]), pw).await;
    assert_eq!(code(&v), "invalid_args");
    let (v, _) = run(
        &[
            "session",
            "read",
            "-p",
            "p",
            "--session-id",
            &Uuid::new_v4().to_string(),
            "--password-stdin",
        ],
        pw,
    )
    .await;
    assert!(!v["ok"].as_bool().unwrap());

    // 结束会话。
    let (v, ok) = go(common(&["end", "--take-control"]), pw).await;
    assert!(ok, "{v}");
    let (v, _) = run(&["session", "list", "-p", "p", "--password-stdin"], pw).await;
    assert!(
        v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["live"] == false)
            || v["sessions"].as_array().unwrap().is_empty(),
        "{v}"
    );

    let _ = stop.send(());
    server.await.unwrap().unwrap();
}
