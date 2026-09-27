//! 真实 Tauri v2 命令实现：profile CRUD、host 管理、会话生命周期。
//!
//! 依赖 crates/app（termbridge）暴露的 config / client / server，
//! 协议帧使用 termbridge-protocol。所有秘密（GUI 确认的指纹、记住的密码）
//! 密码存入系统凭据库；指纹优先存 keyring，缺失时回退到 known_hosts.json。

use std::net::SocketAddr;
use std::sync::{Arc, atomic::Ordering};

use serde::{Deserialize, Serialize};
use termbridge::client::{AuthMethod, Client, ClientConfig};
use termbridge::config::{
    self, host_path, known_hosts_path, profiles_path, read_json, save_json, HostConfig, Profile,
    Profiles,
};
use termbridge_protocol::{Event, Request, Response, SessionInfo, MAX_SEND_BYTES};
use tauri::{Emitter, Manager};
use uuid::Uuid;

use crate::state::{AppState, PumpCmd, Session, SessionMeta, SessionState};

#[derive(Debug, Serialize)]
pub struct IpcError {
    pub message: String,
}

fn ipc<E: std::fmt::Display>(e: E) -> IpcError {
    IpcError { message: e.to_string() }
}

type IpcResult<T> = Result<T, IpcError>;

/* ---------- keyring（系统凭据库） ---------- */

const SERVICE: &str = "termbridge";

fn keyring_entry(name: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, name).map_err(|e| e.to_string())
}

fn fingerprint_key(host: &str, port: u16) -> String {
    format!("host-fingerprint:{host}:{port}")
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct KnownHosts {
    #[serde(default)]
    entries: std::collections::HashMap<String, String>,
}

fn store_confirmed_fingerprint(host: &str, port: u16, fp: &str) -> IpcResult<()> {
    if keyring_entry(&fingerprint_key(host, port))
        .and_then(|e| e.set_password(fp).map_err(|e| e.to_string())).is_ok() {
        return Ok(());
    }
    // 无 Secret Service 的 Linux 桌面也须能保存主机指纹；指纹不是密码。
    let mut known: KnownHosts = read_json(&known_hosts_path()).map_err(ipc)?;
    let key = format!("{host}:{port}");
    if let Some(previous) = known.entries.get(&key) {
        if termbridge::client::HostFingerprint::new(previous.clone()).base64_part()
            != termbridge::client::HostFingerprint::new(fp.to_owned()).base64_part() {
            return Err(ipc("已有指纹记录不同，拒绝覆盖"));
        }
    }
    known.entries.insert(key, fp.to_owned());
    save_json(&known_hosts_path(), &known).map_err(ipc)
}

fn confirmed_fingerprint(host: &str, port: u16) -> Option<String> {
    keyring_entry(&fingerprint_key(host, port))
        .ok()
        .and_then(|e| e.get_password().ok())
}

/// GUI 与 CLI 共用指纹文件；GUI 首选系统凭据库，缺失时写入此文件。
fn known_hosts_entry(host: &str, port: u16) -> Option<String> {
    let known: KnownHosts = read_json(&known_hosts_path()).ok()?;
    known.entries.get(&format!("{host}:{port}")).cloned()
}

/* ---------- DTO（与 types.ts 对齐） ---------- */

#[derive(Serialize)]
pub struct ProfileDto {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: String,
    pub key_path: Option<String>,
    pub remember_password: bool,
}

fn front_profile(p: &Profile) -> ProfileDto {
    let auth = match p.auth {
        config::AuthKind::Password => "password",
        config::AuthKind::Key => "key",
    };
    ProfileDto {
        id: p.id.to_string(),
        name: p.name.clone(),
        host: p.host.clone(),
        port: p.port,
        user: p.user.clone(),
        auth: auth.to_string(),
        key_path: p.key_path.as_ref().map(|x| x.to_string_lossy().into_owned()),
        remember_password: p.remember_password,
    }
}

#[derive(Deserialize)]
pub struct ProfileDraft {
    pub id: Option<String>,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: String,
    pub key_path: Option<String>,
}

#[derive(Serialize)]
pub struct HostStatus {
    pub enabled: bool,
    pub bind_addr: String,
    pub fingerprint: Option<String>,
    pub controller: Option<String>,
    pub port: Option<u16>,
    pub password_enabled: bool,
    pub authorized_key_count: usize,
}

#[derive(Serialize)]
pub struct FingerprintDto {
    pub fingerprint: String,
    pub trusted: bool,
}

#[derive(Serialize, Clone)]
struct OutputEv {
    session_id: String,
    seq: u64,
    data_b64: String,
}

#[derive(Serialize, Clone)]
struct ClosedEv {
    session_id: String,
    reason: Option<String>,
}

#[derive(Serialize, Clone)]
struct ResyncEv {
    session_id: String,
}

fn to_front_info(info: &SessionInfo, meta: &SessionMeta) -> SessionMeta {
    SessionMeta {
        session_id: info.id.to_string(),
        profile_id: meta.profile_id.clone(),
        profile_name: meta.profile_name.clone(),
        state: meta.state.clone(),
        controller: info.controller.map(|c| c.to_string()),
        is_controller: meta.is_controller,
        online: meta.online && info.live,
    }
}

/* ---------- profile CRUD ---------- */

#[tauri::command]
pub fn list_profiles() -> IpcResult<Vec<ProfileDto>> {
    let profiles: Profiles = read_json(&profiles_path()).map_err(ipc)?;
    Ok(profiles.items.iter().map(front_profile).collect())
}

#[tauri::command]
pub fn save_profile(draft: ProfileDraft) -> IpcResult<ProfileDto> {
    let auth = match draft.auth.as_str() {
        "password" => config::AuthKind::Password,
        "key" => config::AuthKind::Key,
        other => return Err(ipc(format!("auth `{other}` 不受支持（仅 password / key）"))),
    };
    if draft.port == 0 || draft.host.trim().is_empty() || draft.user.trim().is_empty() || draft.name.trim().is_empty() {
        return Err(ipc("配置名称、主机、用户及有效端口均为必填"));
    }
    let key_path = if matches!(auth, config::AuthKind::Key) {
        Some(match draft.key_path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(path) => std::path::PathBuf::from(path),
            None => config::default_ssh_private_key_path().map_err(ipc)?,
        })
    } else { None };
    let mut profiles: Profiles = read_json(&profiles_path()).map_err(ipc)?;
    let editing_id = draft.id.as_deref().map(Uuid::parse_str).transpose().map_err(ipc)?;
    if profiles.items.iter().any(|p| p.name == draft.name && Some(p.id) != editing_id) {
        return Err(ipc("连接配置名称已存在"));
    }
    let mut clear_password = None;
    let profile = match editing_id {
        Some(id) => {
            let existing = profiles
                .items
                .iter_mut()
                .find(|p| p.id == id)
                .ok_or_else(|| ipc("档案不存在"))?;
            // 主机、端口、用户或认证方式变化时，不能复用旧目标的已记住密码。
            let identity_changed = existing.host != draft.host || existing.port != draft.port
                || existing.user != draft.user
                || matches!(existing.auth, config::AuthKind::Password) != matches!(auth, config::AuthKind::Password);
            if identity_changed {
                if existing.remember_password { clear_password = Some(existing.id.to_string()); }
                existing.remember_password = false;
            }
            existing.name = draft.name.clone();
            existing.host = draft.host.clone();
            existing.port = draft.port;
            existing.user = draft.user.clone();
            existing.auth = auth.clone();
            existing.key_path = key_path.clone();
            existing.clone()
        }
        None => Profile {
            id: Uuid::new_v4(),
            name: draft.name.clone(),
            host: draft.host.clone(),
            port: draft.port,
            user: draft.user.clone(),
            auth: auth.clone(),
            key_path,
            remember_password: false,
        },
    };
    if let Some(pos) = profiles.items.iter().position(|p| p.id == profile.id) {
        profiles.items[pos] = profile.clone();
    } else {
        profiles.items.push(profile.clone());
    }
    save_json(&profiles_path(), &profiles).map_err(ipc)?;
    if let Some(id) = clear_password {
        if let Ok(entry) = keyring_entry(&id) { let _ = entry.delete_credential(); }
    }
    Ok(front_profile(&profile))
}

#[tauri::command]
pub fn remove_profile(id: String) -> IpcResult<()> {
    let uid = Uuid::parse_str(&id).map_err(ipc)?;
    let mut profiles: Profiles = read_json(&profiles_path()).map_err(ipc)?;
    let before = profiles.items.len();
    profiles.items.retain(|p| p.id != uid);
    if profiles.items.len() == before {
        return Err(ipc("档案不存在"));
    }
    save_json(&profiles_path(), &profiles).map_err(ipc)?;
    // 尽力删除记住的密码；凭据库不可用时忽略。
    if let Ok(entry) = keyring_entry(&id.to_string()) {
        let _ = entry.delete_credential();
    }
    Ok(())
}

#[tauri::command]
pub fn store_profile_password(profile_id: String, password: String) -> IpcResult<()> {
    let uid = Uuid::parse_str(&profile_id).map_err(ipc)?;
    let mut profiles: Profiles = read_json(&profiles_path()).map_err(ipc)?;
    let profile = profiles.items.iter_mut().find(|p| p.id == uid).ok_or_else(|| ipc("档案不存在"))?;
    if !matches!(profile.auth, config::AuthKind::Password) || password.is_empty() {
        return Err(ipc("仅密码认证档案可保存非空密码"));
    }
    keyring_entry(&profile_id.clone())
        .and_then(|e| e.set_password(&password).map_err(|e| e.to_string()))
        .map_err(ipc)?;
    profile.remember_password = true;
    save_json(&profiles_path(), &profiles).map_err(ipc)
}

/* ---------- host 管理 ---------- */

fn read_host_config() -> IpcResult<Option<HostConfig>> {
    if !host_path().exists() {
        return Ok(None);
    }
    read_json(&host_path()).map(Some).map_err(ipc)
}

fn host_status_inner(app: &tauri::AppHandle) -> IpcResult<HostStatus> {
    let config = read_host_config()?;
    let listen = config.as_ref().and_then(|c| c.listen.clone());
    let port = listen.as_deref().and_then(|s| s.rsplit(':').next().and_then(|p| p.parse().ok()));
    let state: tauri::State<AppState> = app.state();
    let running = state.host_running.load(Ordering::SeqCst);
    Ok(HostStatus {
        enabled: running,
        bind_addr: listen.unwrap_or_default(),
        fingerprint: None,
        controller: None,
        port,
        password_enabled: config.as_ref().is_some_and(|c| c.password_hash.is_some()),
        authorized_key_count: config.as_ref().map_or(0, |c| c.authorized_keys.len()),
    })
}

fn host_fingerprint() -> Option<String> {
    read_host_config().ok().flatten().and_then(|c| c.fingerprint().ok())
}

#[tauri::command]
pub fn host_status(app: tauri::AppHandle) -> IpcResult<HostStatus> {
    let mut status = host_status_inner(&app)?;
    status.fingerprint = host_fingerprint();
    Ok(status)
}

/// 用户显式选择密码或复用 SSH 授权公钥；两者不会混用或隐式放开认证。
#[tauri::command]
pub fn init_host(app: tauri::AppHandle, password: Option<String>, authorized_keys_path: Option<String>) -> IpcResult<HostStatus> {
    match (password, authorized_keys_path) {
        (Some(password), None) => { config::init_host(&password).map_err(ipc)?; }
        (None, Some(path)) => {
            let path = if path.trim().is_empty() {
                config::default_ssh_authorized_keys_path().map_err(ipc)?
            } else { std::path::PathBuf::from(path.trim()) };
            let text = std::fs::read_to_string(&path).map_err(ipc)?;
            config::init_host_with_keys(&text).map_err(ipc)?;
        }
        _ => return Err(ipc("请只选择一种初始化方式：产品密码或现有 SSH 授权公钥")),
    }
    host_status(app)
}

/// 已初始化主机改用 SSH 密钥；GUI 接收任务运行中必须先明确停止。
#[tauri::command]
pub fn switch_host_to_keys(app: tauri::AppHandle, authorized_keys_path: String) -> IpcResult<HostStatus> {
    let state: tauri::State<AppState> = app.state();
    if state.host_running.load(Ordering::SeqCst) {
        return Err(ipc("请先点击“停止本应用接收”，再切换认证方式；运行中的会话不会被偷偷结束"));
    }
    let mut config = read_host_config()?.ok_or_else(|| ipc("接收端未初始化"))?;
    let path = if authorized_keys_path.trim().is_empty() {
        config::default_ssh_authorized_keys_path().map_err(ipc)?
    } else { std::path::PathBuf::from(authorized_keys_path.trim()) };
    let text = std::fs::read_to_string(&path).map_err(ipc)?;
    config::switch_to_keys_only(&mut config, &text).map_err(ipc)?;
    host_status(app)
}

#[tauri::command]
pub async fn set_host_enabled(app: tauri::AppHandle, enabled: bool, bind_addr: String) -> IpcResult<HostStatus> {
    let mut config = read_host_config()?
        .ok_or_else(|| ipc("接收端尚未初始化：请先选择密码或 SSH 授权公钥"))?;
    if enabled && bind_addr.trim().is_empty() { return Err(ipc("必须明确选择监听地址")); }
    // 先校验并占用新地址：地址错误或端口已占用时，不停掉旧接收端及其终端。
    let listener = if enabled {
        config.fingerprint().map_err(ipc)?;
        let addr: SocketAddr = bind_addr.parse().map_err(ipc)?;
        if addr.port() == 0 { return Err(ipc("监听端口不能为 0")); }
        let state: tauri::State<AppState> = app.state();
        if state.host_running.load(Ordering::SeqCst) && config.listen.as_deref() == Some(addr.to_string().as_str()) {
            return host_status(app);
        }
        Some((addr, tokio::net::TcpListener::bind(addr).await.map_err(ipc)?))
    } else { None };
    stop_host_task(&app).await;
    if let Some((addr, listener)) = listener {
        config.listen = Some(addr.to_string());
        config.enabled = true;
        start_host_task(&app, listener, config.clone()).await?;
        if let Err(err) = save_json(&host_path(), &config) {
            stop_host_task(&app).await;
            return Err(ipc(err));
        }
    } else {
        config.enabled = false;
        save_json(&host_path(), &config).map_err(ipc)?;
    }
    host_status(app)
}

async fn stop_host_task(app: &tauri::AppHandle) {
    let state: tauri::State<AppState> = app.state();
    let stop = state.host_stop.lock().unwrap().take();
    let task = state.host_task.lock().unwrap().take();
    if let Some(stop) = stop { let _ = stop.send(()); }
    if let Some(mut task) = task {
        if tokio::time::timeout(std::time::Duration::from_secs(5), &mut task).await.is_err() {
            task.abort();
        }
    }
    state.host_running.store(false, Ordering::SeqCst);
}

async fn start_host_task(app: &tauri::AppHandle, listener: tokio::net::TcpListener, config: HostConfig) -> IpcResult<()> {
    config.fingerprint().map_err(ipc)?;
    let (stop, rx) = tokio::sync::oneshot::channel();
    let app_for_task = app.clone();
    let state: tauri::State<AppState> = app.state();
    state.host_running.store(true, Ordering::SeqCst);
    let handle = tauri::async_runtime::spawn(async move {
        let _ = termbridge::server::run_on_listener(config, listener, async { let _ = rx.await; }).await;
        app_for_task.state::<AppState>().host_running.store(false, Ordering::SeqCst);
    });
    *state.host_stop.lock().unwrap() = Some(stop);
    *state.host_task.lock().unwrap() = Some(handle);
    Ok(())
}

/// 托盘选择退出时，停止监听；退出导致 Windows Job 句柄关闭并终止子终端。
pub fn stop_host_persist(app: &tauri::AppHandle) {
    let state: tauri::State<AppState> = app.state();
    // 没有本 GUI 的接收任务时不要修改共享配置：另行启动的 CLI 用户服务不受影响。
    let stop = state.host_stop.lock().unwrap().take();
    if let Some(stop) = stop {
        let _ = stop.send(());
        if let Ok(Some(mut config)) = read_host_config() {
            config.enabled = false;
            let _ = save_json(&host_path(), &config);
        }
    }
}

/* ---------- 指纹确认（绝无 TOFU 自动接受） ---------- */

/// 仅探测：返回服务端实际指纹，供 GUI 展示；不进行认证、不落任何信任记录。
#[tauri::command]
pub async fn probe_host(host: String, port: u16) -> IpcResult<FingerprintDto> {
    let fp = termbridge::client::probe_host(&host, port).await.map_err(ipc)?;
    let pins = [confirmed_fingerprint(&host, port), known_hosts_entry(&host, port)];
    for pinned in pins.iter().flatten() {
        if termbridge::client::HostFingerprint::new(pinned.clone()).base64_part() != fp.base64_part() {
            return Err(ipc(format!("主机指纹变化，已阻断连接。已有记录: {pinned}；当前: {}", fp.sha256)));
        }
    }
    Ok(FingerprintDto { fingerprint: fp.sha256, trusted: pins.iter().any(Option::is_some) })
}

/// GUI 显式确认：后端重新 probe 并核对前端展示的指纹，再写入受信任记录。
#[tauri::command]
pub async fn confirm_host_fingerprint(
    host: String,
    port: u16,
    fingerprint: String,
) -> IpcResult<FingerprintDto> {
    let actual = termbridge::client::probe_host(&host, port).await.map_err(ipc)?;
    let shown = termbridge::client::HostFingerprint::new(fingerprint);
    if actual.base64_part() != shown.base64_part() {
        return Err(ipc(format!(
            "指纹不一致：展示 {}，实际 {}；请重新探测",
            shown.sha256, actual.sha256
        )));
    }
    for pinned in [confirmed_fingerprint(&host, port), known_hosts_entry(&host, port)].into_iter().flatten() {
        if termbridge::client::HostFingerprint::new(pinned).base64_part() != actual.base64_part() {
            return Err(ipc("主机指纹变化，已阻断连接；不能通过重新确认覆盖旧记录"));
        }
    }
    store_confirmed_fingerprint(&host, port, &actual.sha256)?;
    Ok(FingerprintDto { fingerprint: actual.sha256, trusted: true })
}

/* ---------- 会话生命周期 ---------- */

fn find_session(state: &tauri::State<AppState>, session_id: &str) -> IpcResult<Arc<Session>> {
    state
        .sessions
        .lock()
        .unwrap()
        .get(session_id)
        .cloned()
        .ok_or_else(|| ipc(format!("会话不存在: {session_id}")))
}

async fn pump_request(session: &Session, req: Request) -> IpcResult<Response> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    session
        .tx
        .send(PumpCmd::Request(req, tx))
        .await
        .map_err(|_| ipc("会话 pump 已退出"))?;
    let response = rx.await.map_err(|_| ipc("会话 pump 已退出"))?.map_err(ipc)?;
    match response {
        Response::Error { code, message } => Err(ipc(format!("{code}: {message}"))),
        ok => Ok(ok),
    }
}

fn spawn_pump(
    client: Client,
    app: tauri::AppHandle,
    session: Arc<Session>,
    session_id: String,
    rx: tokio::sync::mpsc::Receiver<PumpCmd>,
) {
    tauri::async_runtime::spawn(async move {
        let mut client = client;
        let mut rx = rx;
        loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(PumpCmd::Request(req, resp_tx)) => {
                        let res = client.request(req).await;
                        let _ = resp_tx.send(res.map_err(|e| e.to_string()));
                    }
                    Some(PumpCmd::Close(ack)) => {
                        let _ = client.disconnect().await;
                        let _ = ack.send(());
                        break;
                    }
                    None => break,
                },
                ev = client.recv_event() => match ev {
                    Some(e) => forward_event(&app, &session, &e),
                    None => {
                        session.set_meta(|m| {
                            m.online = false;
                            m.state = SessionState::Error;
                        });
                        let _ = app.emit("session_closed", ClosedEv {
                            session_id: session_id.clone(),
                            reason: Some("connection_lost".into()),
                        });
                        break;
                    }
                },
            }
        }
    });
}

fn forward_event(app: &tauri::AppHandle, session: &Arc<Session>, event: &Event) {
    match event {
        Event::Output { session_id, seq, data_b64 } => {
            let _ = app.emit("session_output", OutputEv {
                session_id: session_id.to_string(),
                seq: *seq,
                data_b64: data_b64.clone(),
            });
        }
        Event::Ended { session_id } => {
            session.set_meta(|m| { m.state = SessionState::Closed; m.online = false; m.is_controller = false; });
            let _ = app.emit("session_closed", ClosedEv {
                session_id: session_id.to_string(),
                reason: Some("ended".into()),
            });
        }
        Event::ControlChanged { session_id, controller } => {
            let mine = *session.client_id.lock().unwrap();
            session.set_meta(|meta| {
                meta.controller = controller.map(|c| c.to_string());
                meta.is_controller = mine.is_some() && *controller == mine;
            });
            let _ = app.emit("session_control_changed", serde_json::json!({
                "session_id": session_id.to_string(),
                "controller": controller.map(|c| c.to_string()),
                "is_controller": session.info().is_controller,
            }));
        }
        Event::ResyncRequired { session_id } => {
            let _ = app.emit("session_resync_required", ResyncEv {
                session_id: session_id.to_string(),
            });
        }
    }
}

#[tauri::command]
pub fn list_sessions(app: tauri::AppHandle) -> IpcResult<Vec<SessionMeta>> {
    let state: tauri::State<AppState> = app.state();
    let sessions = state.sessions.lock().unwrap();
    Ok(sessions.values().map(|s| s.info()).collect())
}

fn parse_host_fingerprint(profile: &Profile, password_missing_hint: bool) -> IpcResult<String> {
    let _ = password_missing_hint;
    let gui = confirmed_fingerprint(&profile.host, profile.port);
    let cli = known_hosts_entry(&profile.host, profile.port);
    if let (Some(a), Some(b)) = (&gui, &cli) {
        if termbridge::client::HostFingerprint::new(a.clone()).base64_part() != termbridge::client::HostFingerprint::new(b.clone()).base64_part() {
            return Err(ipc("GUI 和 CLI 的指纹记录冲突，已阻断连接"));
        }
    }
    if let Some(fp) = gui.or(cli) { return Ok(fp); }
    Err(ipc(format!(
        "首次连接 {}:{} 需要先在界面确认主机指纹（probe_host / confirm_host_fingerprint），不自动信任",
        profile.host, profile.port
    )))
}

/// 只返回是否需要 SSH 私钥口令；私钥内容与口令均不返回给前端。
#[tauri::command]
pub fn key_passphrase_required(profile_id: String) -> IpcResult<bool> {
    let id = Uuid::parse_str(&profile_id).map_err(ipc)?;
    let profiles: Profiles = read_json(&profiles_path()).map_err(ipc)?;
    let profile = profiles.items.into_iter().find(|p| p.id == id).ok_or_else(|| ipc("档案不存在"))?;
    if !matches!(profile.auth, config::AuthKind::Key) { return Err(ipc("该配置未使用 SSH 私钥")); }
    let path = profile.key_path.as_ref().ok_or_else(|| ipc("档案未配置私钥路径"))?;
    config::key_passphrase_required(path).map_err(ipc)
}

fn auth_method(profile: &Profile, password: Option<String>) -> IpcResult<AuthMethod> {
    match profile.auth {
        config::AuthKind::Password => {
            let password = match password {
                Some(p) if !p.is_empty() => p,
                _ => {
                    let remembered = profile
                        .remember_password
                        .then(|| {
                            keyring_entry(&profile.id.to_string())
                                .ok()
                                .and_then(|e| e.get_password().ok())
                        })
                        .flatten();
                    remembered.ok_or_else(|| ipc("缺少连接密码：请在界面输入密码或先保存（不可隐式成功）"))?
                }
            };
            Ok(AuthMethod::Password { password })
        }
        config::AuthKind::Key => {
            let path = profile.key_path.as_ref().ok_or_else(|| ipc("档案未配置私钥路径"))?;
            let passphrase = password.filter(|p| !p.is_empty());
            config::validate_private_key(path, passphrase.as_deref()).map_err(ipc)?;
            Ok(AuthMethod::PrivateKey {
                path: path.to_string_lossy().into_owned(),
                passphrase,
            })
        }
    }
}

#[tauri::command]
pub async fn create_session(
    app: tauri::AppHandle,
    profile_id: String,
    password: Option<String>,
) -> IpcResult<serde_json::Value> {
    let profiles: Profiles = read_json(&profiles_path()).map_err(ipc)?;
    let uid = Uuid::parse_str(&profile_id).map_err(ipc)?;
    let profile = profiles
        .items
        .into_iter()
        .find(|p| p.id == uid)
        .ok_or_else(|| ipc("档案不存在"))?;

    let fp = parse_host_fingerprint(&profile, false)?;
    let auth = auth_method(&profile, password)?;
    let cfg = ClientConfig::new(
        profile.host.clone(),
        profile.port,
        profile.user.clone(),
        auth,
        termbridge::client::HostFingerprint::new(fp),
    );
    let mut client = Client::connect(cfg).await.map_err(ipc)?;

    // 创建远端会话（Create 不等于 Attach；attach 由前端显式调用）。
    let resp = client
        .request(Request::Create { title: profile.name.clone(), rows: 24, cols: 80 })
        .await
        .map_err(ipc)?;
    let remote = match resp {
        Response::Created { session } => session,
        Response::Error { code, message } => return Err(ipc(format!("{code}: {message}"))),
        other => return Err(ipc(format!("意外的服务端响应: {other:?}"))),
    };

    let sid = remote.id.to_string();
    let meta = SessionMeta {
        session_id: sid.clone(),
        profile_id: profile.id.to_string(),
        profile_name: profile.name.clone(),
        state: SessionState::Detached,
        controller: remote.controller.map(|c| c.to_string()),
        is_controller: false,
        online: true,
    };
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let session = Arc::new(Session { meta: std::sync::Mutex::new(meta), tx, last_seq: std::sync::Mutex::new(0), client_id: std::sync::Mutex::new(None) });
    spawn_pump(client, app.clone(), session.clone(), sid.clone(), rx);
    {
        let state: tauri::State<AppState> = app.state();
        state.sessions.lock().unwrap().insert(sid, session.clone());
    }
    Ok(serde_json::json!({ "session": session.info() }))
}

/// 在另一连接端创建的会话也可通过配置列出并附着，不会偷偷创建替代进程。
#[tauri::command]
pub async fn list_remote_sessions(profile_id: String, password: Option<String>) -> IpcResult<Vec<SessionInfo>> {
    let profiles: Profiles = read_json(&profiles_path()).map_err(ipc)?;
    let id = Uuid::parse_str(&profile_id).map_err(ipc)?;
    let profile = profiles.items.into_iter().find(|p| p.id == id).ok_or_else(|| ipc("档案不存在"))?;
    let fp = parse_host_fingerprint(&profile, false)?;
    let auth = auth_method(&profile, password)?;
    let mut client = Client::connect(ClientConfig::new(&profile.host, profile.port, &profile.user,
        auth, termbridge::client::HostFingerprint::new(fp))).await.map_err(ipc)?;
    let result = client.request(Request::List).await.map_err(ipc)?;
    match result {
        Response::Sessions { sessions } => Ok(sessions),
        Response::Error { code, message } => Err(ipc(format!("{code}: {message}"))),
        _ => Err(ipc("意外的列表响应")),
    }
}

#[tauri::command]
pub async fn connect_existing_session(
    app: tauri::AppHandle, profile_id: String, session_id: String, password: Option<String>,
) -> IpcResult<serde_json::Value> {
    let profiles: Profiles = read_json(&profiles_path()).map_err(ipc)?;
    let id = Uuid::parse_str(&profile_id).map_err(ipc)?;
    let sid = Uuid::parse_str(&session_id).map_err(ipc)?;
    let profile = profiles.items.into_iter().find(|p| p.id == id).ok_or_else(|| ipc("档案不存在"))?;
    let fp = parse_host_fingerprint(&profile, false)?;
    let auth = auth_method(&profile, password)?;
    let mut client = Client::connect(ClientConfig::new(&profile.host, profile.port, &profile.user,
        auth, termbridge::client::HostFingerprint::new(fp))).await.map_err(ipc)?;
    let result = client.request(Request::Attach { session_id: sid }).await.map_err(ipc)?;
    let (remote, screen_b64, seq, has_control, client_id) = match result {
        Response::Attached { session, screen_b64, seq, has_control, client_id } => (session, screen_b64, seq, has_control, client_id),
        Response::Error { code, message } => return Err(ipc(format!("{code}: {message}"))),
        _ => return Err(ipc("意外的附着响应")),
    };
    let sid_text = sid.to_string();
    let meta = SessionMeta {
        session_id: sid_text.clone(), profile_id: profile.id.to_string(), profile_name: profile.name,
        state: SessionState::Attached, controller: remote.controller.map(|c| c.to_string()),
        is_controller: has_control, online: remote.live,
    };
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let session = Arc::new(Session { meta: std::sync::Mutex::new(meta), tx, last_seq: std::sync::Mutex::new(seq), client_id: std::sync::Mutex::new(Some(client_id)) });
    spawn_pump(client, app.clone(), session.clone(), sid_text.clone(), rx);
    let state: tauri::State<AppState> = app.state();
    if let Some(old) = state.sessions.lock().unwrap().insert(sid_text, session.clone()) {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let _ = old.tx.try_send(PumpCmd::Close(tx));
    }
    Ok(serde_json::json!({ "session": to_front_info(&remote, &session.info()), "screen_b64": screen_b64, "seq": seq }))
}

#[tauri::command]
pub async fn attach_session(
    app: tauri::AppHandle,
    session_id: String,
) -> IpcResult<serde_json::Value> {
    let state: tauri::State<AppState> = app.state();
    let session = find_session(&state, &session_id)?;
    let sid = Uuid::parse_str(&session_id).map_err(ipc)?;
    let resp = pump_request(&session, Request::Attach { session_id: sid }).await?;
    match resp {
        Response::Attached { session: remote, screen_b64, seq, has_control, client_id } => {
            *session.client_id.lock().unwrap() = Some(client_id);
            session.set_meta(|m| {
                m.state = SessionState::Attached;
                m.is_controller = has_control;
                m.online = true;
            });
            *session.last_seq.lock().unwrap() = seq;
            let info = to_front_info(&remote, &session.info());
            Ok(serde_json::json!({ "screen_b64": screen_b64, "seq": seq, "session": info }))
        }
        Response::Error { code, message } => Err(ipc(format!("{code}: {message}"))),
        other => Err(ipc(format!("意外的服务端响应: {other:?}"))),
    }
}

#[tauri::command]
pub async fn detach_session(
    app: tauri::AppHandle,
    session_id: String,
) -> IpcResult<serde_json::Value> {
    let state: tauri::State<AppState> = app.state();
    let session = find_session(&state, &session_id)?;
    let sid = Uuid::parse_str(&session_id).map_err(ipc)?;
    pump_request(&session, Request::Detach { session_id: sid }).await?;
    session.set_meta(|m| {
        m.state = SessionState::Detached;
        m.is_controller = false;
    });
    Ok(serde_json::json!(session.info()))
}

#[tauri::command]
pub async fn end_session(
    app: tauri::AppHandle,
    session_id: String,
) -> IpcResult<serde_json::Value> {
    let state: tauri::State<AppState> = app.state();
    let session = find_session(&state, &session_id)?;
    let sid = Uuid::parse_str(&session_id).map_err(ipc)?;
    pump_request(&session, Request::End { session_id: sid }).await?;
    session.set_meta(|m| {
        m.state = SessionState::Closed;
        m.is_controller = false;
    });
    let info = session.info();
    // 结束后关闭 pump 并从会话表移除。
    let (tx, rx) = tokio::sync::oneshot::channel();
    if session.tx.send(PumpCmd::Close(tx)).await.is_ok() {
        let _ = rx.await;
    }
    state.sessions.lock().unwrap().remove(&session_id);
    Ok(serde_json::json!(info))
}

#[tauri::command]
pub async fn send_text(app: tauri::AppHandle, session_id: String, text: String) -> IpcResult<()> {
    if text.len() > MAX_SEND_BYTES {
        return Err(ipc(format!("文本超过上限 {MAX_SEND_BYTES} 字节")));
    }
    let state: tauri::State<AppState> = app.state();
    let session = find_session(&state, &session_id)?;
    let sid = Uuid::parse_str(&session_id).map_err(ipc)?;
    pump_request(&session, Request::Send {
        session_id: sid,
        command_id: Uuid::new_v4(),
        text,
    })
    .await
    .map(|_| ())
}

#[tauri::command]
pub async fn take_control(
    app: tauri::AppHandle,
    session_id: String,
) -> IpcResult<serde_json::Value> {
    let state: tauri::State<AppState> = app.state();
    let session = find_session(&state, &session_id)?;
    let sid = Uuid::parse_str(&session_id).map_err(ipc)?;
    pump_request(&session, Request::TakeControl { session_id: sid }).await?;
    // 服务端 Accepted 不回传连接端 UUID，只能本地推断为已取得控制权。
    session.set_meta(|m| {
        m.is_controller = true;
        m.online = true;
    });
    Ok(serde_json::json!(session.info()))
}
