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

    #[tokio::test]
    async fn local_two_ends_pin_send_detach_reconnect() {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("termbridge-integration-{}", Uuid::new_v4()));
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
        let Response::Attached {
            has_control: true, ..
        } = first
            .request(Request::Attach { session_id: id })
            .await
            .unwrap()
        else {
            panic!("attach rejected")
        };
        #[cfg(windows)]
        let command = "Write-Output TB_INTEGRATION_731";
        #[cfg(not(windows))]
        let command = "echo TB_INTEGRATION_731";
        assert!(matches!(
            first
                .request(Request::Send {
                    session_id: id,
                    command_id: Uuid::new_v4(),
                    text: command.into()
                })
                .await
                .unwrap(),
            Response::Accepted
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(Event::Output { data_b64, .. }) = first.recv_event().await {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data_b64)
                        .unwrap();
                    if String::from_utf8_lossy(&bytes).contains("TB_INTEGRATION_731") {
                        break;
                    }
                }
            }
        })
        .await
        .expect("the terminal produced no output");
        assert!(matches!(
            first
                .request(Request::Detach { session_id: id })
                .await
                .unwrap(),
            Response::Accepted
        ));
        first.disconnect().await.unwrap();
        let mut second = client::Client::connect(make_config(fp, password))
            .await
            .unwrap();
        let Response::Attached { screen_b64, .. } = second
            .request(Request::Attach { session_id: id })
            .await
            .unwrap()
        else {
            panic!("reattach rejected")
        };
        let snapshot = base64::engine::general_purpose::STANDARD
            .decode(screen_b64)
            .unwrap();
        assert!(
            String::from_utf8_lossy(&snapshot).contains("TB_INTEGRATION_731"),
            "snapshot did not recover the screen"
        );
        assert!(matches!(
            second
                .request(Request::End { session_id: id })
                .await
                .unwrap(),
            Response::Accepted
        ));
        let _ = stop.send(());
        task.await.unwrap().unwrap();
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
        let _ = std::fs::remove_dir_all(&dir);

        // 密钥专用接收端：复用一把现有 SSH 公私钥，不设置任何产品密码。
        let key_dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("termbridge-key-integration-{}", Uuid::new_v4()));
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
}
