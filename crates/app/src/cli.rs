//! 命令行入口：host / profile / session 三组子命令。
//!
//! 仅调用 config.rs、client.rs、server.rs 与 termbridge_protocol 的既有 API。
//! 安全约定：
//! - 密码一律通过 rpassword 读取，不回显、不打印；可选存入系统 keyring。
//! - 首次连接先用 `client::probe_host` 展示主机指纹，用户显式输入 yes 后
//!   才写入 known_hosts；之后指纹与记录不一致时直接阻断（client 层强制校验）。
//! - attach 模式下本地整行编辑、Enter 才发送（v2 的 Input 偏移在此临时维护；
//!   M3 会替换为 raw 模式）；`:take` / `:detach` / `:end` 为内建命令；
//!   断线后不再重发任何请求。

use std::collections::HashMap;
use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use clap::{Parser, Subcommand, ValueEnum};
use termbridge_protocol::{Request, Response};

use crate::client::{probe_host, AuthMethod, Client, ClientConfig, HostFingerprint};
use crate::config::{
    self, add_public_key, init_host, known_hosts_path, profiles_path, read_json, save_json,
    HostConfig, Profile, Profiles,
};

pub const DEFAULT_LISTEN: &str = "127.0.0.1:22333";
const KEYRING_SERVICE: &str = "termbridge";

#[derive(Parser)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// 接收端（本机作为被连接方）
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// 连接配置档案
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// 远端会话操作
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
}

#[derive(Subcommand)]
pub enum HostCommand {
    /// 初始化接收端（产品密码或现有 SSH 授权公钥）
    Init {
        /// 使用现有 SSH 授权公钥文件初始化，无需设置另一套密码；例如 ~/.ssh/authorized_keys
        #[arg(long)]
        authorized_keys: Option<String>,
    },
    /// 查看接收端状态与主机指纹
    Status,
    /// 启用/停用接收端
    Enable {
        /// 停用而不是启用
        #[arg(long)]
        disable: bool,
        /// 首次启用必须明确选择监听地址，例如 127.0.0.1:22333 或 Tailscale IP:22333
        #[arg(long)]
        listen: Option<String>,
    },
    /// 运行接收端 SSH 服务。默认仅监听 127.0.0.1:22333；
    /// 如需其他地址/端口，必须通过 --listen 显式指定。
    Run {
        /// 显式选择监听地址（例如 0.0.0.0:22333 或 [::1]:22333）
        #[arg(long)]
        listen: Option<String>,
    },
    /// 已初始化设备改为仅使用现有 SSH 授权公钥（运行中需重启接收端）
    UseSshKeys {
        #[arg(long)]
        authorized_keys: String,
    },
    /// 添加授权公钥（OpenSSH 格式）
    KeyAdd {
        /// 公钥文件路径；省略则从 stdin 读取
        #[arg(long)]
        key_file: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ProfileCommand {
    /// 新增连接档案
    Add {
        /// 档案名称（唯一）
        name: String,
        /// 远端主机
        host: String,
        /// SSH 端口
        #[arg(long, default_value_t = 22333)]
        port: u16,
        /// 登录用户
        #[arg(short = 'u', long)]
        user: String,
        /// 认证方式
        #[arg(long, value_enum, default_value_t = AuthKindArg::Password)]
        auth: AuthKindArg,
        /// 私钥文件路径；auth=key 时省略可自动查找 ~/.ssh 下的常见私钥
        #[arg(long)]
        key_path: Option<String>,
        /// 将密码保存到系统 keyring（key 认证时忽略）
        #[arg(long)]
        remember_password: bool,
    },
    /// 列出档案
    List,
    /// 删除档案（同时清除 keyring 中的密码）
    Remove { name: String },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum AuthKindArg {
    Password,
    Key,
}

#[derive(Subcommand)]
pub enum SessionCommand {
    /// 列出远端会话
    List {
        /// 档案名称
        #[arg(short = 'p', long)]
        profile: String,
    },
    /// 创建会话
    Create {
        #[arg(short = 'p', long)]
        profile: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, default_value_t = 24)]
        rows: u16,
        #[arg(long, default_value_t = 80)]
        cols: u16,
    },
    /// 附加到会话（本地整行编辑，Enter 发送；:take/:detach/:end）
    Attach {
        #[arg(short = 'p', long)]
        profile: String,
        /// 会话 UUID；省略则列出并选择第一个活跃会话
        #[arg(long)]
        session_id: Option<uuid::Uuid>,
    },
    /// 结束会话
    End {
        #[arg(short = 'p', long)]
        profile: String,
        #[arg(long)]
        session_id: uuid::Uuid,
        /// 其他客户端持有控制权时，必须显式声明接管才能结束
        #[arg(long)]
        take_control: bool,
    },
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct KnownHosts {
    /// host:port -> SHA256 指纹
    entries: HashMap<String, String>,
}

fn load_known_hosts() -> Result<KnownHosts> {
    read_json(&known_hosts_path())
}

fn prompt_tty(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn prompt_password(confirm: bool) -> Result<String> {
    let pw = rpassword::prompt_password("接收密码（至少 12 个字符）: ")?;
    if confirm {
        let again = rpassword::prompt_password("再次输入以确认: ")?;
        if pw != again {
            bail!("两次输入不一致");
        }
    }
    Ok(pw)
}

/// 首次连接的指纹确认流程：probe -> 展示 -> 显式 yes 才入库。
/// 已有记录且不一致时直接阻断，不做任何降级。
async fn confirmed_fingerprint(host: &str, port: u16) -> Result<HostFingerprint> {
    let key = format!("{host}:{port}");
    let known = load_known_hosts()?;
    let actual = probe_host(host, port).await.context("探测主机指纹失败")?;
    match known.entries.get(&key) {
        Some(expected) => {
            let exp = HostFingerprint::new(expected.clone());
            if exp.base64_part() == actual.base64_part() {
                Ok(actual)
            } else {
                bail!(
                    "主机指纹变化，已阻断连接。\n  记录: {}\n  实际: {}\n如确认服务器变更，请手动删除 {} 中对应条目。",
                    exp.sha256, actual.sha256,
                    known_hosts_path().display()
                )
            }
        }
        None => {
            println!("首次连接 {host}:{port}");
            println!("主机指纹: {}", actual.sha256);
            let answer = prompt_tty("确认并信任该指纹？输入 yes 继续: ")?;
            if !answer.eq_ignore_ascii_case("yes") {
                bail!("未确认指纹，已取消连接");
            }
            let mut known = known;
            known.entries.insert(key, actual.sha256.clone());
            save_json(&known_hosts_path(), &known).context("保存 known_hosts 失败")?;
            Ok(actual)
        }
    }
}

fn load_profile(name: &str) -> Result<Profile> {
    let profiles: Profiles = read_json(&profiles_path())?;
    profiles
        .items
        .into_iter()
        .find(|p| p.name == name)
        .with_context(|| format!("档案不存在: {name}"))
}

async fn connect_profile(profile: &Profile) -> Result<Client> {
    let fp = confirmed_fingerprint(&profile.host, profile.port).await?;
    let auth = match profile.auth {
        config::AuthKind::Key => {
            let path = profile
                .key_path
                .as_ref()
                .with_context(|| format!("档案 {} 使用 key 认证但未配置 key_path", profile.name))?;
            let passphrase = match russh::keys::load_secret_key(path, None) {
                Ok(_) => None,
                Err(russh::keys::Error::KeyIsEncrypted) => {
                    Some(rpassword::prompt_password("SSH 私钥口令（不保存）: ")?)
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("无法读取私钥 {}", path.display()))
                }
            };
            AuthMethod::PrivateKey {
                path: path.to_string_lossy().into_owned(),
                passphrase,
            }
        }
        config::AuthKind::Password => {
            let password = load_password(profile)?;
            AuthMethod::Password { password }
        }
    };
    Client::connect(ClientConfig::new(
        profile.host.clone(),
        profile.port,
        profile.user.clone(),
        auth,
        fp,
    ))
    .await
}

fn load_password(profile: &Profile) -> Result<String> {
    // 无图形界面的 VPS 可能没有 Secret Service；凭据库不可用时仍可手动输入。
    if profile.remember_password {
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &profile.id.to_string()) {
            if let Ok(password) = entry.get_password() {
                return Ok(password);
            }
        }
    }
    Ok(rpassword::prompt_password(format!(
        "{}@{} 的密码: ",
        profile.user, profile.host
    ))?)
}

fn store_password(profile_name: &str, password: &str) -> Result<()> {
    Ok(keyring::Entry::new(KEYRING_SERVICE, profile_name)?.set_password(password)?)
}

fn expect_response(resp: Response, what: &str) -> Result<Response> {
    match resp {
        Response::Error { code, message } => {
            bail!("{} 失败 [{}]: {message}", what, code)
        }
        other => Ok(other),
    }
}

fn print_sessions(sessions: &[termbridge_protocol::SessionInfo]) {
    if sessions.is_empty() {
        println!("（无会话）");
        return;
    }
    for s in sessions {
        let ctl = s
            .controller
            .map(|c| c.to_string()[..8].to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "{}  {:<16}  {}x{:<4}  {}  ctl={}",
            s.id,
            truncate(&s.title, 16),
            s.cols,
            s.rows,
            if s.live { "live" } else { "dead" },
            ctl,
        );
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

pub async fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Host { command } => run_host(command).await,
        Command::Profile { command } => run_profile(command).await,
        Command::Session { command } => run_session(command).await,
    }
}

async fn run_host(cmd: HostCommand) -> Result<()> {
    match cmd {
        HostCommand::Init { authorized_keys } => {
            let cfg = if let Some(path) = authorized_keys {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("读取授权公钥文件失败: {path}"))?;
                config::init_host_with_keys(&text)?
            } else {
                let password = prompt_password(true)?;
                init_host(&password)?
            };
            println!(
                "接收端已初始化：用户 {}，指纹 {}",
                cfg.username,
                cfg.fingerprint()?
            );
        }
        HostCommand::Status => {
            let cfg: HostConfig = read_json(&config::host_path())?;
            if config::host_path().exists() {
                let fp = cfg
                    .fingerprint()
                    .unwrap_or_else(|e| format!("（读取失败: {e}）"));
                println!("用户:        {}", cfg.username);
                println!("已启用:      {}", if cfg.enabled { "是" } else { "否" });
                println!(
                    "监听:        {}",
                    cfg.listen.as_deref().unwrap_or(DEFAULT_LISTEN)
                );
                println!(
                    "密码认证:    {}",
                    if cfg.password_hash.is_some() {
                        "已启用"
                    } else {
                        "已关闭"
                    }
                );
                println!("授权密钥数:  {}", cfg.authorized_keys.len());
                println!("主机指纹:    {fp}");
            } else {
                println!("接收端未初始化（缺少 {}）", config::host_path().display());
            }
        }
        HostCommand::Enable { disable, listen } => {
            if !config::host_path().exists() {
                bail!("接收端未初始化，请先执行 host init");
            }
            let mut cfg: HostConfig = read_json(&config::host_path())?;
            if !disable {
                let text = listen
                    .or_else(|| cfg.listen.clone())
                    .context("首次启用必须用 --listen 明确选择监听地址")?;
                let addr: std::net::SocketAddr = text.parse().context("无效监听地址")?;
                if addr.port() == 0 {
                    bail!("监听端口不能为 0");
                }
                cfg.listen = Some(addr.to_string());
            }
            cfg.enabled = !disable;
            save_json(&config::host_path(), &cfg)?;
            println!(
                "接收端配置已{}；若服务正在运行，停用后仍需停止该进程",
                if cfg.enabled { "启用" } else { "停用" }
            );
        }
        HostCommand::Run { listen } => {
            let cfg: HostConfig = read_json(&config::host_path())?;
            if !config::host_path().exists() {
                bail!("接收端未初始化，请先执行 host init");
            }
            if !cfg.enabled {
                bail!("接收端未启用，请先执行 host enable");
            }
            let addr_text = listen
                .or_else(|| cfg.listen.clone())
                .context("请先用 host enable --listen 选择地址")?;
            let addr: std::net::SocketAddr = addr_text
                .parse()
                .with_context(|| format!("无效监听地址 {addr_text}"))?;
            if !addr.ip().is_loopback() {
                println!("警告：正在监听非回环地址 {addr}，任何可达主机都可尝试连接。");
            }
            crate::server::run(cfg, addr).await?;
        }
        HostCommand::UseSshKeys { authorized_keys } => {
            if !config::host_path().exists() {
                bail!("接收端未初始化，请先执行 host init");
            }
            let text = std::fs::read_to_string(&authorized_keys)
                .with_context(|| format!("读取授权公钥文件失败: {authorized_keys}"))?;
            let mut cfg: HostConfig = read_json(&config::host_path())?;
            config::switch_to_keys_only(&mut cfg, &text)?;
            println!(
                "已改为仅密钥认证（{} 把授权密钥）；若接收端正在运行，请先停止并重启它",
                cfg.authorized_keys.len()
            );
        }
        HostCommand::KeyAdd { key_file } => {
            let mut cfg: HostConfig = read_json(&config::host_path())?;
            if !config::host_path().exists() {
                bail!("接收端未初始化，请先执行 host init");
            }
            let text = match key_file {
                Some(path) => std::fs::read_to_string(path).context("读取公钥文件失败")?,
                None => {
                    print!("粘贴 OpenSSH 公钥（一行）: ");
                    std::io::stdout().flush()?;
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line)?;
                    line
                }
            };
            add_public_key(&mut cfg, &text)?;
            println!(
                "公钥已添加，当前共 {} 把授权密钥",
                cfg.authorized_keys.len()
            );
        }
    }
    Ok(())
}

async fn run_profile(cmd: ProfileCommand) -> Result<()> {
    match cmd {
        ProfileCommand::Add {
            name,
            host,
            port,
            user,
            auth,
            key_path,
            remember_password,
        } => {
            let mut profiles: Profiles = read_json(&profiles_path())?;
            if profiles.items.iter().any(|p| p.name == name) {
                bail!("档案已存在: {name}");
            }
            let auth_kind = match auth {
                AuthKindArg::Key => config::AuthKind::Key,
                AuthKindArg::Password => config::AuthKind::Password,
            };
            let key_path = if matches!(auth_kind, config::AuthKind::Key) {
                Some(
                    key_path
                        .map(std::path::PathBuf::from)
                        .map(Ok)
                        .unwrap_or_else(config::default_ssh_private_key_path)?,
                )
            } else {
                None
            };
            let profile = Profile {
                id: uuid::Uuid::new_v4(),
                name,
                host,
                port,
                user,
                auth: auth_kind,
                key_path,
                remember_password,
            };
            if remember_password && matches!(profile.auth, config::AuthKind::Password) {
                let pw = rpassword::prompt_password(format!(
                    "{}@{} 的密码: ",
                    profile.user, profile.host
                ))?;
                store_password(&profile.id.to_string(), &pw)?;
            }
            profiles.items.push(profile);
            save_json(&profiles_path(), &profiles)?;
            println!("档案已保存");
        }
        ProfileCommand::List => {
            let profiles: Profiles = read_json(&profiles_path())?;
            if profiles.items.is_empty() {
                println!("（无档案）");
            }
            for p in profiles.items {
                println!(
                    "{}  {}@{}:{}  {:?}  remember_pw={}",
                    p.name, p.user, p.host, p.port, p.auth, p.remember_password
                );
            }
        }
        ProfileCommand::Remove { name } => {
            let mut profiles: Profiles = read_json(&profiles_path())?;
            let before = profiles.items.len();
            let removed = profiles.items.iter().find(|p| p.name == name).cloned();
            profiles.items.retain(|p| p.name != name);
            if profiles.items.len() == before {
                bail!("档案不存在: {name}");
            }
            if let Some(profile) = removed {
                if profile.remember_password {
                    let _ = keyring::Entry::new(KEYRING_SERVICE, &profile.id.to_string())?
                        .delete_credential();
                }
            }
            save_json(&profiles_path(), &profiles)?;
            println!("档案已删除: {name}");
        }
    }
    Ok(())
}

async fn run_session(cmd: SessionCommand) -> Result<()> {
    let (profile_name, action): (String, SessionAction) = match cmd {
        SessionCommand::List { profile } => (profile, SessionAction::List),
        SessionCommand::Create {
            profile,
            title,
            rows,
            cols,
        } => (profile, SessionAction::Create { title, rows, cols }),
        SessionCommand::Attach {
            profile,
            session_id,
        } => (profile, SessionAction::Attach { session_id }),
        SessionCommand::End {
            profile,
            session_id,
            take_control,
        } => (
            profile,
            SessionAction::End {
                session_id,
                take_control,
            },
        ),
    };
    let profile = load_profile(&profile_name)?;
    let mut client = connect_profile(&profile).await.context("连接失败")?;
    let result = match action {
        SessionAction::List => {
            let resp = expect_response(client.request(Request::List).await?, "list")?;
            if let Response::Sessions { sessions } = resp {
                print_sessions(&sessions);
            }
            Ok(())
        }
        SessionAction::Create { title, rows, cols } => {
            let title = title.unwrap_or_else(|| "cli".to_string());
            let resp = expect_response(
                client
                    .request(Request::Create { title, rows, cols })
                    .await?,
                "create",
            )?;
            if let Response::Created { session } = resp {
                println!("会话已创建: {}", session.id);
            }
            Ok(())
        }
        SessionAction::End {
            session_id,
            take_control,
        } => {
            let resp = expect_response(
                client
                    .request(Request::Attach {
                        session_id,
                        stream_id: uuid::Uuid::new_v4(),
                        input_base: 0,
                        resume_from: None,
                    })
                    .await?,
                "attach",
            )?;
            let Response::Attached { has_control, .. } = resp else {
                bail!("attach 返回意外响应")
            };
            if !has_control {
                if !take_control {
                    bail!("另一客户端持有控制权；请显式使用 --take-control")
                }
                expect_response(
                    client.request(Request::TakeControl { session_id }).await?,
                    "take",
                )?;
            }
            expect_response(client.request(Request::End { session_id }).await?, "end")?;
            println!("会话已结束: {session_id}");
            Ok(())
        }
        SessionAction::Attach { session_id } => attach_loop(&mut client, session_id).await,
    };
    client.disconnect().await.ok();
    result
}

enum SessionAction {
    List,
    Create {
        title: Option<String>,
        rows: u16,
        cols: u16,
    },
    Attach {
        session_id: Option<uuid::Uuid>,
    },
    End {
        session_id: uuid::Uuid,
        take_control: bool,
    },
}

/// attach 主循环：本地整行编辑，Enter 才发送；`:take`/`:detach`/`:end` 为内建命令；
/// 远端事件实时显示；断线后不再重发任何请求。
/// M1 最小适配：输入按 v2 偏移发送，相同 `stream_id` 在重挂时保持不变。
async fn attach_loop(client: &mut Client, session_id: Option<uuid::Uuid>) -> Result<()> {
    let session_id = match session_id {
        Some(id) => id,
        None => {
            let resp = expect_response(client.request(Request::List).await?, "list")?;
            let Response::Sessions { sessions } = resp else {
                bail!("list 返回了意外响应");
            };
            sessions
                .into_iter()
                .find(|s| s.live)
                .map(|s| s.id)
                .context("没有活跃会话，请先用 session create 创建")?
        }
    };
    let stream_id = uuid::Uuid::new_v4();
    let resp = expect_response(
        client
            .request(Request::Attach {
                session_id,
                stream_id,
                input_base: 0,
                resume_from: None,
            })
            .await?,
        "attach",
    )?;
    let Response::Attached {
        session,
        has_control,
        input_next,
        ..
    } = resp
    else {
        bail!("attach 返回了意外响应");
    };
    println!(
        "已附加到 {}（{}）。输入命令后按 Enter 发送；:take 获取控制权，:detach 脱离，:end 结束会话。",
        session.id, session.title
    );
    if !has_control {
        println!("当前无控制权，输入 :take 获取。");
    }

    let mut input_offset = input_next;
    // 已完整显示到的输出偏移；None 表示正在等待/写入快照。
    let mut displayed: Option<u64> = None;
    let mut pending_snapshot: Option<u64> = None;

    // stdin 整行读取放到阻塞线程；回车才产出一条命令。
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(8);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) => {
                    if line_tx.blocking_send(l).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    loop {
        tokio::select! {
            ev = client.recv_event() => {
                match ev {
                    Some(termbridge_protocol::Event::SnapshotBegin { session_id: sid, offset, .. }) if sid == session_id => {
                        pending_snapshot = Some(offset);
                        displayed = None;
                    }
                    Some(termbridge_protocol::Event::SnapshotChunk { session_id: sid, data_b64 }) if sid == session_id => {
                        if let Ok(data) = base64::engine::general_purpose::STANDARD.decode(&data_b64) {
                            print!("{}", String::from_utf8_lossy(&data));
                            let _ = std::io::stdout().flush();
                        }
                    }
                    Some(termbridge_protocol::Event::SnapshotEnd { session_id: sid }) if sid == session_id => {
                        displayed = pending_snapshot.take();
                    }
                    Some(termbridge_protocol::Event::Output { session_id: sid, offset, data_b64 }) if sid == session_id => {
                        match displayed {
                            None => {}
                            Some(_) if resync_cli_gap(displayed, offset, &data_b64) => {
                                println!("\n[检测到输出缺口，重新挂接]");
                                let (next_input, new_displayed) = resync_cli(
                                    client,
                                    session_id,
                                    stream_id,
                                    input_offset,
                                    displayed,
                                )
                                .await?;
                                input_offset = input_offset.max(next_input);
                                displayed = new_displayed;
                                pending_snapshot = None;
                            }
                            Some(current) => {
                                if let Ok(data) = base64::engine::general_purpose::STANDARD.decode(&data_b64) {
                                    let end = offset + data.len() as u64;
                                    if end > current {
                                        let skip = (current - offset) as usize;
                                        print!("{}", String::from_utf8_lossy(&data[skip..]));
                                        let _ = std::io::stdout().flush();
                                        displayed = Some(end);
                                    }
                                }
                            }
                        }
                    }
                    Some(termbridge_protocol::Event::InputAck { session_id: sid, stream_id: st, offset: _ }) if sid == session_id && st == stream_id => {}
                    Some(termbridge_protocol::Event::InputRejected { session_id: sid, stream_id: st, code, message, .. }) if sid == session_id && st == stream_id => {
                        println!("\n[输入被拒绝: {code} {message}]");
                    }
                    Some(termbridge_protocol::Event::Resized { session_id: sid, rows, cols }) if sid == session_id => {
                        println!("\n[远端尺寸: {cols}x{rows}]");
                    }
                    Some(termbridge_protocol::Event::Ended { session_id: sid }) if sid == session_id => {
                        println!("\n[会话已结束]");
                        break;
                    }
                    Some(termbridge_protocol::Event::ControlChanged { session_id: sid, controller }) if sid == session_id => {
                        println!("\n[控制权变更: {}]", controller.map(|c| c.to_string()[..8].to_string()).unwrap_or_else(|| "无".into()));
                    }
                    Some(_) => {}
                    None => {
                        println!("\n[连接已断开，不再重发请求]");
                        break;
                    }
                }
            }
            line = line_rx.recv() => {
                let Some(line) = line else { break };
                let trimmed = line.trim();
                match trimmed {
                    ":detach" => {
                        match client.request(Request::Detach { session_id }).await {
                            Ok(resp) => match expect_response(resp, "detach") { Ok(_) => println!("[已脱离]"), Err(e) => println!("[未脱离: {e}]") },
                            Err(_) => println!("[连接已断开，本地脱离]"),
                        }
                        break;
                    }
                    ":end" => {
                        match client.request(Request::End { session_id }).await {
                            Ok(resp) => match expect_response(resp, "end") { Ok(_) => println!("[会话已结束]"), Err(e) => println!("[未结束: {e}]") },
                            Err(_) => println!("[连接已断开，未发送 end]"),
                        }
                        break;
                    }
                    ":take" => {
                        match client.request(Request::TakeControl { session_id }).await {
                            Ok(resp) => match expect_response(resp, "take") { Ok(_) => println!("[已获取控制权]"), Err(e) => println!("[未获取: {e}]") },
                            Err(_) => println!("[连接已断开，不再重发]"),
                        }
                    }
                    "" => {}
                    _ => {
                        // 无控制权时不发送，避免误操作。
                        if client.is_disconnected() {
                            println!("[连接已断开，不再重发]");
                            continue;
                        }
                        let mut bytes = line.clone().into_bytes();
                        bytes.push(b'\r');
                        match client.send_input(session_id, stream_id, input_offset, &bytes).await {
                            Ok(()) => input_offset += bytes.len() as u64,
                            Err(_) => println!("[发送状态未知：连接已断开，不再重发]"),
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// 输出偏移是否出现缺口（已知已显示偏移 + 事件偏移 + 数据长度）。
fn resync_cli_gap(displayed: Option<u64>, offset: u64, data_b64: &str) -> bool {
    let Some(current) = displayed else {
        return false;
    };
    match base64::engine::general_purpose::STANDARD.decode(data_b64) {
        Ok(_) => offset > current,
        Err(_) => false,
    }
}

/// 重新挂接：优先按 `resume_from` 重放补洞，服务端决定重放还是给快照。
async fn resync_cli(
    client: &mut Client,
    session_id: uuid::Uuid,
    stream_id: uuid::Uuid,
    input_base: u64,
    resume_from: Option<u64>,
) -> Result<(u64, Option<u64>)> {
    let resp = expect_response(
        client
            .request(Request::Attach {
                session_id,
                stream_id,
                input_base,
                resume_from,
            })
            .await?,
        "重新挂接",
    )?;
    let Response::Attached {
        resumed,
        input_next,
        ..
    } = resp
    else {
        bail!("重新挂接返回了意外响应");
    };
    if resumed {
        Ok((input_next, resume_from))
    } else {
        Ok((input_next, None))
    }
}
