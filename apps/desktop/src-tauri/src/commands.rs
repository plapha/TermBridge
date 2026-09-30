//! 真实 Tauri v2 命令实现：profile CRUD、host 管理、会话生命周期。
//!
//! 依赖 crates/app（termbridge）暴露的 config / client / server，
//! 协议帧使用 termbridge-protocol。所有秘密（GUI 确认的指纹、记住的密码）
//! 密码存入系统凭据库；指纹优先存 keyring，缺失时回退到 known_hosts.json。

use std::net::SocketAddr;
use std::sync::{Arc, atomic::Ordering};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use termbridge::client::{AuthMethod, Client, ClientConfig, InputBuffer, InputRecovery};
use termbridge::config::{
    self, host_path, known_hosts_path, profiles_path, read_json, save_json, HostConfig, Profile,
    Profiles,
};
use termbridge::i18n::wire_message;
use termbridge::tr;
use termbridge_protocol::{Event, Request, Response, SessionInfo, MAX_INPUT_CHUNK};
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
            return Err(ipc(tr!("An existing fingerprint record differs; refusing to overwrite it", "已有指纹记录不同，拒绝覆盖")));
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
    offset: u64,
    data_b64: String,
}

#[derive(Serialize, Clone)]
struct SnapshotBeginEv {
    session_id: String,
    offset: u64,
    rows: u16,
    cols: u16,
}

#[derive(Serialize, Clone)]
struct SnapshotChunkEv {
    session_id: String,
    data_b64: String,
}

#[derive(Serialize, Clone)]
struct SnapshotEndEv {
    session_id: String,
}

#[derive(Serialize, Clone)]
struct ResizedEv {
    session_id: String,
    rows: u16,
    cols: u16,
}

#[derive(Serialize, Clone)]
struct InputRejectedEv {
    session_id: String,
    code: String,
    message: String,
}

#[derive(Serialize, Clone)]
struct ClosedEv {
    session_id: String,
    reason: Option<String>,
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
        other => return Err(ipc(tr!("auth `{other}` is not supported (only password / key)", "auth `{other}` 不受支持（仅 password / key）"))),
    };
    if draft.port == 0 || draft.host.trim().is_empty() || draft.user.trim().is_empty() || draft.name.trim().is_empty() {
        return Err(ipc(tr!("Profile name, host, user and a valid port are all required", "配置名称、主机、用户及有效端口均为必填")));
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
        return Err(ipc(tr!("A profile with this name already exists", "连接配置名称已存在")));
    }
    let mut clear_password = None;
    let profile = match editing_id {
        Some(id) => {
            let existing = profiles
                .items
                .iter_mut()
                .find(|p| p.id == id)
                .ok_or_else(|| ipc(tr!("Profile not found", "档案不存在")))?;
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
        return Err(ipc(tr!("Profile not found", "档案不存在")));
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
    let profile = profiles.items.iter_mut().find(|p| p.id == uid).ok_or_else(|| ipc(tr!("Profile not found", "档案不存在")))?;
    if !matches!(profile.auth, config::AuthKind::Password) || password.is_empty() {
        return Err(ipc(tr!("Only password-authenticated profiles can store a non-empty password", "仅密码认证档案可保存非空密码")));
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
        _ => return Err(ipc(tr!("Choose exactly one initialization method: a separate password or existing SSH authorized keys", "请只选择一种初始化方式：产品密码或现有 SSH 授权公钥"))),
    }
    host_status(app)
}

/// 已初始化主机改用 SSH 密钥；GUI 接收任务运行中必须先明确停止。
#[tauri::command]
pub fn switch_host_to_keys(app: tauri::AppHandle, authorized_keys_path: String) -> IpcResult<HostStatus> {
    let state: tauri::State<AppState> = app.state();
    if state.host_running.load(Ordering::SeqCst) {
        return Err(ipc(tr!("Stop the local receiver first, then switch the authentication method; running sessions are never ended silently", "请先点击“停止本应用接收”，再切换认证方式；运行中的会话不会被偷偷结束")));
    }
    let mut config = read_host_config()?.ok_or_else(|| ipc(tr!("The host is not initialized", "接收端未初始化")))?;
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
        .ok_or_else(|| ipc(tr!("The host is not initialized yet: choose a password or SSH authorized keys first", "接收端尚未初始化：请先选择密码或 SSH 授权公钥")))?;
    if enabled && bind_addr.trim().is_empty() { return Err(ipc(tr!("A listen address must be chosen explicitly", "必须明确选择监听地址"))); }
    // 先校验并占用新地址：地址错误或端口已占用时，不停掉旧接收端及其终端。
    let listener = if enabled {
        config.fingerprint().map_err(ipc)?;
        let addr: SocketAddr = bind_addr.parse().map_err(ipc)?;
        if addr.port() == 0 { return Err(ipc(tr!("The listen port cannot be 0", "监听端口不能为 0"))); }
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
            return Err(ipc(tr!("Host fingerprint changed; connection blocked. Recorded: {pinned}; current: {}", "主机指纹变化，已阻断连接。已有记录: {pinned}；当前: {}", fp.sha256)));
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
        return Err(ipc(tr!(
            "Fingerprint mismatch: shown {}, actual {}; probe again",
            "指纹不一致：展示 {}，实际 {}；请重新探测",
            shown.sha256,
            actual.sha256
        )));
    }
    for pinned in [confirmed_fingerprint(&host, port), known_hosts_entry(&host, port)].into_iter().flatten() {
        if termbridge::client::HostFingerprint::new(pinned).base64_part() != actual.base64_part() {
            return Err(ipc(tr!("Host fingerprint changed; connection blocked; a stored record cannot be replaced by re-confirming", "主机指纹变化，已阻断连接；不能通过重新确认覆盖旧记录")));
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
        .ok_or_else(|| ipc(tr!("Session not found: {session_id}", "会话不存在: {session_id}")))
}

async fn pump_request(session: &Session, req: Request) -> IpcResult<Response> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    session
        .tx
        .send(PumpCmd::Request(req, tx))
        .await
        .map_err(|_| ipc(tr!("The session pump has exited", "会话 pump 已退出")))?;
    let response = rx.await.map_err(|_| ipc(tr!("The session pump has exited", "会话 pump 已退出")))?.map_err(ipc)?;
    match response {
        Response::Error { code, message } => Err(ipc(format!("{code}: {}", wire_message(&code, &message)))),
        ok => Ok(ok),
    }
}

/// 发给输出合并任务的前端事件；Output 保留原始字节，在合并任务里统一编码。
enum FrontEvent {
    Output {
        session_id: String,
        offset: u64,
        data: Vec<u8>,
    },
    SnapshotBegin(SnapshotBeginEv),
    SnapshotChunk(SnapshotChunkEv),
    SnapshotEnd(SnapshotEndEv),
    Resized(ResizedEv),
    ControlChanged {
        session_id: String,
        controller: Option<String>,
        is_controller: bool,
    },
    InputRejected(InputRejectedEv),
    Closed(ClosedEv),
}

const MAX_COALESCE: usize = 256 * 1024;
const COALESCE_WINDOW: std::time::Duration = std::time::Duration::from_millis(16);

fn should_flush_output(first: std::time::Instant, now: std::time::Instant, len: usize) -> bool {
    now.duration_since(first) >= COALESCE_WINDOW || len >= MAX_COALESCE
}

/// 输出合并：从第一字节起最多等待 16ms，或达到 256 KiB 就 emit。
async fn output_emitter(app: tauri::AppHandle, mut rx: tokio::sync::mpsc::Receiver<FrontEvent>) {
    let mut pending: Option<(String, u64, Vec<u8>)> = None;
    let mut first_at: Option<std::time::Instant> = None;
    fn flush(app: &tauri::AppHandle, pending: &mut Option<(String, u64, Vec<u8>)>) {
        if let Some((session_id, offset, data)) = pending.take() {
            let _ = app.emit(
                "session_output",
                OutputEv {
                    session_id,
                    offset,
                    data_b64: B64.encode(&data),
                },
            );
        }
    }
    loop {
        let event = if let Some(first) = first_at {
            match tokio::time::timeout_at(
                tokio::time::Instant::from_std(first + COALESCE_WINDOW), rx.recv()).await {
                Ok(event) => event,
                Err(_) => {
                    flush(&app, &mut pending);
                    first_at = None;
                    continue;
                }
            }
        } else { rx.recv().await };
        let Some(event) = event else {
            break;
        };
        match event {
            FrontEvent::Output {
                session_id,
                offset,
                data,
            } => match &mut pending {
                Some((sid, base, buf))
                    if *sid == session_id
                        && *base + buf.len() as u64 == offset
                        && buf.len() < MAX_COALESCE =>
                {
                    buf.extend_from_slice(&data);
                }
                _ => {
                    flush(&app, &mut pending);
                    first_at = Some(std::time::Instant::now());
                    pending = Some((session_id, offset, data));
                }
            },
            other => {
                flush(&app, &mut pending);
                first_at = None;
                match other {
                    FrontEvent::SnapshotBegin(ev) => {
                        let _ = app.emit("session_snapshot_begin", ev);
                    }
                    FrontEvent::SnapshotChunk(ev) => {
                        let _ = app.emit("session_snapshot_chunk", ev);
                    }
                    FrontEvent::SnapshotEnd(ev) => {
                        let _ = app.emit("session_snapshot_end", ev);
                    }
                    FrontEvent::Resized(ev) => {
                        let _ = app.emit("session_resized", ev);
                    }
                    FrontEvent::ControlChanged {
                        session_id,
                        controller,
                        is_controller,
                    } => {
                        let _ = app.emit(
                            "session_control_changed",
                            serde_json::json!({
                                "session_id": session_id,
                                "controller": controller,
                                "is_controller": is_controller,
                            }),
                        );
                    }
                    FrontEvent::InputRejected(ev) => {
                        let _ = app.emit("session_input_rejected", ev);
                    }
                    FrontEvent::Closed(ev) => {
                        let _ = app.emit("session_closed", ev);
                    }
                    FrontEvent::Output { .. } => {}
                }
            }
        }
        if let Some(first) = first_at {
            if pending.as_ref().is_some_and(|(_, _, data)|
                should_flush_output(first, std::time::Instant::now(), data.len())) {
                flush(&app, &mut pending);
                first_at = None;
            }
        }
    }
    flush(&app, &mut pending);
}

#[cfg(test)]
mod output_flush_tests {
    use super::*;

    #[test]
    fn continuous_small_chunks_flush_from_first_byte_deadline() {
        let first = std::time::Instant::now();
        for ms in [5, 10, 15] {
            assert!(!should_flush_output(first, first + std::time::Duration::from_millis(ms), 1024));
        }
        assert!(should_flush_output(first, first + COALESCE_WINDOW, 1024));
        assert!(should_flush_output(first, first, MAX_COALESCE));
    }
}

fn spawn_pump(
    client: Client,
    app: tauri::AppHandle,
    session: Arc<Session>,
    session_id: Uuid,
    rx: tokio::sync::mpsc::Receiver<PumpCmd>,
) {
    let (emit_tx, emit_rx) = tokio::sync::mpsc::channel::<FrontEvent>(256);
    tauri::async_runtime::spawn(output_emitter(app.clone(), emit_rx));
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
                    Ok(Some(e)) => forward_event(&emit_tx, &session, &mut client, &e).await,
                    Err(_) => {
                        // 前端实际已绘制的偏移无法由后端证明；全量快照比猜测 resume_from 安全。
                        let base = session.input_buffer.lock().unwrap().acked();
                        match client.request(Request::Attach {
                            session_id, stream_id: session.stream_id,
                            input_base: base, resume_from: None,
                        }).await {
                            Ok(Response::Attached { input_next, has_control, .. }) => {
                                session.input_buffer.lock().unwrap().align_attach(input_next);
                                session.set_meta(|m| m.is_controller = has_control);
                            }
                            _ => {
                                session.set_meta(|m| m.state = SessionState::Error);
                                let _ = emit_tx.send(FrontEvent::Closed(ClosedEv {
                                    session_id: session_id.to_string(),
                                    reason: Some("event_loss_reattach_failed".into()),
                                })).await;
                                break;
                            }
                        }
                    }
                    Ok(None) => {
                        session.set_meta(|m| {
                            m.online = false;
                            m.state = SessionState::Error;
                        });
                        let _ = emit_tx.send(FrontEvent::Closed(ClosedEv {
                            session_id: session_id.to_string(),
                            reason: Some("connection_lost".into()),
                        })).await;
                        break;
                    }
                },
            }
        }
    });
}

async fn forward_event(
    tx: &tokio::sync::mpsc::Sender<FrontEvent>,
    session: &Arc<Session>,
    client: &mut Client,
    event: &Event,
) {
    let front = match event {
        Event::Output { session_id, offset, data_b64 } => {
            let Ok(data) = B64.decode(data_b64) else {
                return;
            };
            FrontEvent::Output {
                session_id: session_id.to_string(),
                offset: *offset,
                data,
            }
        }
        Event::SnapshotBegin { session_id, offset, rows, cols } => {
            FrontEvent::SnapshotBegin(SnapshotBeginEv {
                session_id: session_id.to_string(),
                offset: *offset,
                rows: *rows,
                cols: *cols,
            })
        }
        Event::SnapshotChunk { session_id, data_b64 } => {
            FrontEvent::SnapshotChunk(SnapshotChunkEv {
                session_id: session_id.to_string(),
                data_b64: data_b64.clone(),
            })
        }
        Event::SnapshotEnd { session_id } => FrontEvent::SnapshotEnd(SnapshotEndEv {
            session_id: session_id.to_string(),
        }),
        Event::InputAck { stream_id, offset, .. } => {
            if *stream_id == session.stream_id {
                session.input_buffer.lock().unwrap().ack(*offset);
            }
            return;
        }
        Event::InputRejected { session_id, stream_id, offset, code, message } => {
            if *stream_id != session.stream_id { return; }
            let recovery = session.input_buffer.lock().unwrap().rejected(code, *offset);
            match recovery {
                InputRecovery::Retry { offset, data, delay } => {
                    let _gate = session.input_gate.lock().await;
                    if !delay.is_zero() { tokio::time::sleep(delay).await; }
                    for (index, chunk) in data.chunks(MAX_INPUT_CHUNK).enumerate() {
                        if session.input_sender.send_input(*session_id, *stream_id,
                            offset + (index * MAX_INPUT_CHUNK) as u64, chunk).await.is_err() {
                            break;
                        }
                    }
                    return;
                }
                InputRecovery::Reattach { input_base } => {
                    // 缺失字节不再存在，保持同一流并显式让服务端对齐，不能猜测后重发。
                    let result = client.request(Request::Attach {
                        session_id: *session_id, stream_id: *stream_id,
                        input_base, resume_from: None,
                    }).await;
                    if let Ok(Response::Attached { input_next, .. }) = result {
                        session.input_buffer.lock().unwrap().align_attach(input_next);
                    }
                }
                InputRecovery::Consumed | InputRecovery::None => {}
            }
            FrontEvent::InputRejected(InputRejectedEv {
                session_id: session_id.to_string(),
                code: code.clone(), message: wire_message(code, message),
            })
        }
        Event::Resized { session_id, rows, cols } => FrontEvent::Resized(ResizedEv {
            session_id: session_id.to_string(),
            rows: *rows,
            cols: *cols,
        }),
        Event::Ended { session_id } => {
            session.set_meta(|m| {
                m.state = SessionState::Closed;
                m.online = false;
                m.is_controller = false;
            });
            FrontEvent::Closed(ClosedEv {
                session_id: session_id.to_string(),
                reason: Some("ended".into()),
            })
        }
        Event::ControlChanged { session_id, controller } => {
            let mine = session.is_controller(*controller);
            session.set_meta(|meta| {
                meta.controller = controller.map(|c| c.to_string());
                meta.is_controller = mine;
            });
            FrontEvent::ControlChanged {
                session_id: session_id.to_string(),
                controller: controller.map(|c| c.to_string()),
                is_controller: mine,
            }
        }
    };
    let _ = tx.send(front).await;
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
            return Err(ipc(tr!("The GUI and CLI fingerprint records conflict; connection blocked", "GUI 和 CLI 的指纹记录冲突，已阻断连接")));
        }
    }
    if let Some(fp) = gui.or(cli) { return Ok(fp); }
    Err(ipc(tr!(
        "First connection to {}:{} requires confirming the host fingerprint in the UI first (probe_host / confirm_host_fingerprint); it is never trusted automatically",
        "首次连接 {}:{} 需要先在界面确认主机指纹（probe_host / confirm_host_fingerprint），不自动信任",
        profile.host,
        profile.port
    )))
}

/// 只返回是否需要 SSH 私钥口令；私钥内容与口令均不返回给前端。
#[tauri::command]
pub fn key_passphrase_required(profile_id: String) -> IpcResult<bool> {
    let id = Uuid::parse_str(&profile_id).map_err(ipc)?;
    let profiles: Profiles = read_json(&profiles_path()).map_err(ipc)?;
    let profile = profiles.items.into_iter().find(|p| p.id == id).ok_or_else(|| ipc(tr!("Profile not found", "档案不存在")))?;
    if !matches!(profile.auth, config::AuthKind::Key) { return Err(ipc(tr!("This profile does not use an SSH private key", "该配置未使用 SSH 私钥"))); }
    let path = profile.key_path.as_ref().ok_or_else(|| ipc(tr!("The profile has no private key path configured", "档案未配置私钥路径")))?;
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
                    remembered.ok_or_else(|| ipc(tr!("Missing connection password: enter it in the UI or save it first (never succeeds implicitly)", "缺少连接密码：请在界面输入密码或先保存（不可隐式成功）")))?
                }
            };
            Ok(AuthMethod::Password { password })
        }
        config::AuthKind::Key => {
            let path = profile.key_path.as_ref().ok_or_else(|| ipc(tr!("The profile has no private key path configured", "档案未配置私钥路径")))?;
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
    rows: u16,
    cols: u16,
) -> IpcResult<serde_json::Value> {
    let profiles: Profiles = read_json(&profiles_path()).map_err(ipc)?;
    let uid = Uuid::parse_str(&profile_id).map_err(ipc)?;
    let profile = profiles
        .items
        .into_iter()
        .find(|p| p.id == uid)
        .ok_or_else(|| ipc(tr!("Profile not found", "档案不存在")))?;

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
    // 尺寸来自前端实际 fit 结果，不再写死 24x80。
    let resp = client
        .request(Request::Create { title: profile.name.clone(), rows, cols })
        .await
        .map_err(ipc)?;
    let remote = match resp {
        Response::Created { session } => session,
        Response::Error { code, message } => return Err(ipc(format!("{code}: {}", wire_message(&code, &message)))),
        other => return Err(ipc(tr!("Unexpected server response: {other:?}", "意外的服务端响应: {other:?}"))),
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
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let session_uuid = remote.id;
    let session = Arc::new(Session {
        meta: std::sync::Mutex::new(meta),
        tx,
        stream_id: Uuid::new_v4(),
        input_buffer: std::sync::Mutex::new(InputBuffer::new(0)),
        input_sender: client.input_sender(),
        input_gate: tokio::sync::Mutex::new(()),
    });
    spawn_pump(client, app.clone(), session.clone(), session_uuid, rx);
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
    let profile = profiles.items.into_iter().find(|p| p.id == id).ok_or_else(|| ipc(tr!("Profile not found", "档案不存在")))?;
    let fp = parse_host_fingerprint(&profile, false)?;
    let auth = auth_method(&profile, password)?;
    let mut client = Client::connect(ClientConfig::new(&profile.host, profile.port, &profile.user,
        auth, termbridge::client::HostFingerprint::new(fp))).await.map_err(ipc)?;
    let result = client.request(Request::List).await.map_err(ipc)?;
    match result {
        Response::Sessions { sessions } => Ok(sessions),
        Response::Error { code, message } => Err(ipc(format!("{code}: {}", wire_message(&code, &message)))),
        _ => Err(ipc(tr!("Unexpected list response", "意外的列表响应"))),
    }
}

#[tauri::command]
pub async fn connect_existing_session(
    app: tauri::AppHandle, profile_id: String, session_id: String, password: Option<String>,
) -> IpcResult<serde_json::Value> {
    let profiles: Profiles = read_json(&profiles_path()).map_err(ipc)?;
    let id = Uuid::parse_str(&profile_id).map_err(ipc)?;
    let sid = Uuid::parse_str(&session_id).map_err(ipc)?;
    let profile = profiles.items.into_iter().find(|p| p.id == id).ok_or_else(|| ipc(tr!("Profile not found", "档案不存在")))?;
    let fp = parse_host_fingerprint(&profile, false)?;
    let auth = auth_method(&profile, password)?;
    let mut client = Client::connect(ClientConfig::new(&profile.host, profile.port, &profile.user,
        auth, termbridge::client::HostFingerprint::new(fp))).await.map_err(ipc)?;
    let stream_id = Uuid::new_v4();
    let result = client
        .request(Request::Attach {
            session_id: sid,
            stream_id,
            input_base: 0,
            resume_from: None,
        })
        .await
        .map_err(ipc)?;
    let (remote, has_control) = match result {
        Response::Attached { session, has_control, .. } => (session, has_control),
        Response::Error { code, message } => return Err(ipc(format!("{code}: {}", wire_message(&code, &message)))),
        _ => return Err(ipc(tr!("Unexpected attach response", "意外的附着响应"))),
    };
    let sid_text = sid.to_string();
    let meta = SessionMeta {
        session_id: sid_text.clone(), profile_id: profile.id.to_string(), profile_name: profile.name,
        state: SessionState::Attached, controller: remote.controller.map(|c| c.to_string()),
        is_controller: has_control, online: remote.live,
    };
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let session = Arc::new(Session {
        meta: std::sync::Mutex::new(meta),
        tx,
        stream_id,
        input_buffer: std::sync::Mutex::new(InputBuffer::new(0)),
        input_sender: client.input_sender(),
        input_gate: tokio::sync::Mutex::new(()),
    });
    spawn_pump(client, app.clone(), session.clone(), sid, rx);
    let state: tauri::State<AppState> = app.state();
    if let Some(old) = state.sessions.lock().unwrap().insert(sid_text, session.clone()) {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let _ = old.tx.try_send(PumpCmd::Close(tx));
    }
    Ok(serde_json::json!({ "session": to_front_info(&remote, &session.info()) }))
}

#[tauri::command]
pub async fn attach_session(
    app: tauri::AppHandle,
    session_id: String,
    resume_from: Option<u64>,
) -> IpcResult<serde_json::Value> {
    let state: tauri::State<AppState> = app.state();
    let session = find_session(&state, &session_id)?;
    let sid = Uuid::parse_str(&session_id).map_err(ipc)?;
    let input_base = session.input_buffer.lock().unwrap().acked();
    let resp = pump_request(
        &session,
        Request::Attach {
            session_id: sid,
            stream_id: session.stream_id,
            input_base,
            resume_from,
        },
    )
    .await?;
    match resp {
        Response::Attached {
            session: remote,
            has_control,
            input_next,
            resumed,
        } => {
            session.set_meta(|m| {
                m.state = SessionState::Attached;
                m.is_controller = has_control;
                m.online = true;
            });
            session.input_buffer.lock().unwrap().align_attach(input_next);
            let info = to_front_info(&remote, &session.info());
            Ok(serde_json::json!({ "session": info, "resumed": resumed, "input_next": input_next }))
        }
        Response::Error { code, message } => Err(ipc(format!("{code}: {}", wire_message(&code, &message)))),
        other => Err(ipc(tr!("Unexpected server response: {other:?}", "意外的服务端响应: {other:?}"))),
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

/// 原始终端输入：直接经 InputSender 发送，不等待响应、不排在 pump 的请求后面。
#[tauri::command]
pub async fn send_input(
    app: tauri::AppHandle,
    session_id: String,
    data: Vec<u8>,
) -> IpcResult<()> {
    if data.is_empty() {
        return Ok(());
    }
    if data.len() > 64 * MAX_INPUT_CHUNK {
        return Err(ipc(tr!("Input exceeds the per-call limit", "输入超过单次上限")));
    }
    let sid = Uuid::parse_str(&session_id).map_err(ipc)?;
    let session = {
        let state: tauri::State<AppState> = app.state();
        find_session(&state, &session_id)?
    };
    // 串行化并发输入：偏移分配与帧发送必须同序。
    let _gate = session.input_gate.lock().await;
    if data.len() > session.input_buffer.lock().unwrap().available() {
        return Err(ipc(tr!("The unacknowledged input buffer is full; new local input rejected", "未确认输入缓冲已满，拒绝本地新输入")));
    }
    for chunk in data.chunks(MAX_INPUT_CHUNK) {
        let offset = session.input_buffer.lock().unwrap().queue(chunk)
            .ok_or_else(|| ipc(tr!("The unacknowledged input buffer is full; new local input rejected", "未确认输入缓冲已满，拒绝本地新输入")))?;
        session.input_sender.send_input(sid, session.stream_id, offset, chunk)
            .await.map_err(ipc)?;
    }
    Ok(())
}

/// 控制者设置远端 PTY 尺寸；由前端 fit 结果驱动。
#[tauri::command]
pub async fn resize_session(
    app: tauri::AppHandle,
    session_id: String,
    rows: u16,
    cols: u16,
) -> IpcResult<()> {
    let state: tauri::State<AppState> = app.state();
    let session = find_session(&state, &session_id)?;
    let sid = Uuid::parse_str(&session_id).map_err(ipc)?;
    pump_request(
        &session,
        Request::Resize {
            session_id: sid,
            rows,
            cols,
        },
    )
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
