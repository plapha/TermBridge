//! 命令行入口：host / profile / session 三组子命令。
//!
//! 仅调用 config.rs、client.rs、server.rs 与 termbridge_protocol 的既有 API。
//! 安全约定：
//! - 密码一律通过 rpassword 读取，不回显、不打印；可选存入系统 keyring。
//! - 首次连接先用 `client::probe_host` 展示主机指纹，用户显式输入 yes 后
//!   才写入 known_hosts；之后指纹与记录不一致时直接阻断（client 层强制校验）。
//! - attach 进入 raw 模式：按键原样透传，Ctrl+] 为本地转义键（见 `raw` 子模块）；
//!   断线后不再重发任何请求。

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use serde_json::json;
use termbridge_protocol::{Request, Response, SessionInfo};

use crate::agent::{self, coded, coded_with, CodedError};
use crate::client::{probe_host, AuthMethod, Client, ClientConfig, HostFingerprint};
use crate::config::{
    self, add_public_key, init_host, known_hosts_path, profiles_path, read_json, save_json,
    HostConfig, Profile, Profiles,
};
use crate::i18n::Lang;
use crate::tr;

mod raw;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:22333";
const KEYRING_SERVICE: &str = "termbridge";

/// 解析命令行：先确定界面语言（`--lang` 优先），再按语言本地化帮助文本。
pub fn parse() -> Cli {
    if let Some(lang) = lang_from_args(std::env::args().skip(1)) {
        crate::i18n::set_lang(lang);
    }
    let matches = localize_command(Cli::command(), crate::i18n::lang()).get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

/// 在 clap 解析之前取出 `--lang <值>` / `--lang=<值>`，让 `--help` 与解析错误也使用该语言。
fn lang_from_args(mut args: impl Iterator<Item = String>) -> Option<Lang> {
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        let value = if arg == "--lang" {
            args.next()
        } else {
            arg.strip_prefix("--lang=").map(str::to_string)
        };
        if let Some(value) = value {
            return Lang::parse(&value);
        }
    }
    None
}

/// 源码里的帮助文本是英文；中文翻译按「命令路径」或「命令路径/参数 id」查表覆盖。
fn localize_command(cmd: clap::Command, lang: Lang) -> clap::Command {
    match lang {
        Lang::En => cmd,
        Lang::Zh => localize_zh(cmd, ""),
    }
}

fn localize_zh(mut cmd: clap::Command, path: &str) -> clap::Command {
    if let Some(about) = zh_help(path) {
        cmd = cmd.about(about);
    }
    let arg_ids: Vec<String> = cmd
        .get_arguments()
        .map(|arg| arg.get_id().to_string())
        .collect();
    for id in arg_ids {
        if let Some(help) = zh_help(&format!("{path}/{id}")) {
            cmd = cmd.mut_arg(id, |arg| arg.help(help));
        }
    }
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .collect();
    for name in names {
        let child = if path.is_empty() {
            name.clone()
        } else {
            format!("{path} {name}")
        };
        cmd = cmd.mut_subcommand(name, |sub| localize_zh(sub, &child));
    }
    cmd
}

fn zh_help(key: &str) -> Option<&'static str> {
    Some(match key {
        "" => "TermBridge：基于 SSH 的远程终端（接收端、连接配置与会话）",
        "/lang" => "界面语言（en 或 zh）；默认取 TERMBRIDGE_LANG 或系统语言",
        "/json" => "机器可读的 JSON 输出；不会提示输入（缺少时直接报错），消息固定为英文",
        "/trust_fingerprint" => "信任指纹等于该值的未知主机（不会覆盖已变化的指纹）",
        "/password_stdin" => "从标准输入的第一行读取密码或私钥口令",
        "host" => "接收端（本机作为被连接方）",
        "host init" => "初始化接收端（产品密码或现有 SSH 授权公钥）",
        "host init/authorized_keys" => {
            "使用现有 SSH 授权公钥文件初始化，无需设置另一套密码；例如 ~/.ssh/authorized_keys"
        }
        "host status" => "查看接收端状态与主机指纹",
        "host enable" => "启用/停用接收端",
        "host enable/disable" => "停用而不是启用",
        "host enable/listen" => {
            "首次启用必须明确选择监听地址，例如 127.0.0.1:22333 或 Tailscale IP:22333"
        }
        "host run" => {
            "运行接收端 SSH 服务。默认仅监听 127.0.0.1:22333；如需其他地址/端口，必须通过 --listen 显式指定。"
        }
        "host run/listen" => "显式选择监听地址（例如 0.0.0.0:22333 或 [::1]:22333）",
        "host use-ssh-keys" => "已初始化设备改为仅使用现有 SSH 授权公钥（运行中需重启接收端）",
        "host key-add" => "添加授权公钥（OpenSSH 格式）",
        "host key-add/key_file" => "公钥文件路径；省略则从 stdin 读取",
        "profile" => "连接配置档案",
        "profile add" => "新增连接档案",
        "profile add/name" => "档案名称（唯一）",
        "profile add/host" => "远端主机",
        "profile add/port" => "SSH 端口",
        "profile add/user" => "登录用户",
        "profile add/auth" => "认证方式",
        "profile add/key_path" => "私钥文件路径；auth=key 时省略可自动查找 ~/.ssh 下的常见私钥",
        "profile add/remember_password" => "将密码保存到系统 keyring（key 认证时忽略）",
        "profile list" => "列出档案",
        "profile remove" => "删除档案（同时清除 keyring 中的密码）",
        "session" => "远端会话操作",
        "session list" => "列出远端会话",
        "session list/profile"
        | "session create/profile"
        | "session attach/profile"
        | "session end/profile"
        | "session send/profile"
        | "session read/profile" => "档案名称",
        "session create" => "创建会话",
        "session attach" => "附加到会话（raw 模式；Ctrl+] 为本地转义键）",
        "session attach/session_id" => "会话 UUID；省略则列出并选择第一个活跃会话",
        "session end" => "结束会话",
        "session end/take_control" => "其他客户端持有控制权时，必须显式声明接管才能结束",
        "session send" => "无需终端向会话输入内容，然后等待并返回输出",
        "session send/text" => "要输入的文本，原样发送（不含换行，除非加 --enter）",
        "session send/enter" => "在文本后按回车",
        "session send/keys" => "文本之后依次按下的按键名（enter、tab、esc、ctrl-c、up、f5、alt-x 等）",
        "session send/take_control" => "其他客户端持有控制权时，必须带上它才能发送",
        "session send/no_wait" => "输入被确认后立即返回，不等待输出",
        "session send/wait_idle" | "session read/wait_idle" => {
            "连续这么多毫秒没有新输出就返回（send 默认等待 500）"
        }
        "session send/wait_for" | "session read/wait_for" => "输出或屏幕匹配该正则表达式时返回（^ 和 $ 按行匹配）",
        "session send/timeout" | "session read/timeout" => {
            "等待超过这么多秒就放弃（结果的 reason 为 timeout）"
        }
        "session read" => "无需终端读取会话的屏幕，或某个偏移之后的输出",
        "session read/since" => "返回该偏移之后产生的输出（取自之前结果里的 offset）",
        _ => return None,
    })
}

/// TermBridge: remote terminal over SSH (host, profiles and sessions)
#[derive(Parser)]
pub struct Cli {
    /// Interface language (en or zh); defaults to TERMBRIDGE_LANG or the system locale
    #[arg(long, global = true, value_enum)]
    pub lang: Option<LangArg>,
    /// Machine-readable JSON output; never prompts (errors instead) and uses English messages
    #[arg(long, global = true)]
    pub json: bool,
    /// Trust an unknown host whose fingerprint equals this value (never overrides a changed fingerprint)
    #[arg(long, global = true, value_name = "SHA256")]
    pub trust_fingerprint: Option<String>,
    /// Read the password or key passphrase from the first line of stdin
    #[arg(long, global = true)]
    pub password_stdin: bool,
    #[command(subcommand)]
    pub command: Command,
}

/// `--lang` 的取值；帮助与运行时消息的语言见 `crate::i18n`。
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum LangArg {
    En,
    Zh,
}

impl From<LangArg> for Lang {
    fn from(arg: LangArg) -> Self {
        match arg {
            LangArg::En => Lang::En,
            LangArg::Zh => Lang::Zh,
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Host: run this machine as the side being connected to
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Connection profiles
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Operate on remote sessions
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
}

#[derive(Subcommand)]
pub enum HostCommand {
    /// Initialize the host (separate password or existing SSH authorized keys)
    Init {
        /// Initialize from an existing SSH authorized keys file instead of setting a separate password, e.g. ~/.ssh/authorized_keys
        #[arg(long)]
        authorized_keys: Option<String>,
    },
    /// Show the host status and fingerprint
    Status,
    /// Enable or disable the host
    Enable {
        /// Disable instead of enabling
        #[arg(long)]
        disable: bool,
        /// Listen address; the first enable must choose one explicitly, e.g. 127.0.0.1:22333 or a Tailscale IP:22333
        #[arg(long)]
        listen: Option<String>,
    },
    /// Run the host SSH service. Listens on 127.0.0.1:22333 by default;
    /// use --listen to choose another address or port.
    Run {
        /// Listen address, chosen explicitly (e.g. 0.0.0.0:22333 or [::1]:22333)
        #[arg(long)]
        listen: Option<String>,
    },
    /// Switch an initialized host to existing SSH authorized keys only (restart a running host afterwards)
    UseSshKeys {
        #[arg(long)]
        authorized_keys: String,
    },
    /// Add an authorized public key (OpenSSH format)
    KeyAdd {
        /// Public key file; read from stdin if omitted
        #[arg(long)]
        key_file: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ProfileCommand {
    /// Add a connection profile
    Add {
        /// Profile name (unique)
        name: String,
        /// Remote host
        host: String,
        /// SSH port
        #[arg(long, default_value_t = 22333)]
        port: u16,
        /// Login user
        #[arg(short = 'u', long)]
        user: String,
        /// Authentication method
        #[arg(long, value_enum, default_value_t = AuthKindArg::Password)]
        auth: AuthKindArg,
        /// Private key path; with auth=key it is found automatically under ~/.ssh when omitted
        #[arg(long)]
        key_path: Option<String>,
        /// Store the password in the system keyring (ignored for key authentication)
        #[arg(long)]
        remember_password: bool,
    },
    /// List profiles
    List,
    /// Remove a profile (and its password in the keyring)
    Remove { name: String },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum AuthKindArg {
    Password,
    Key,
}

#[derive(Subcommand)]
pub enum SessionCommand {
    /// List remote sessions
    List {
        /// Profile name
        #[arg(short = 'p', long)]
        profile: String,
    },
    /// Create a session
    Create {
        /// Profile name
        #[arg(short = 'p', long)]
        profile: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, default_value_t = 24)]
        rows: u16,
        #[arg(long, default_value_t = 80)]
        cols: u16,
    },
    /// Attach to a session (raw mode; Ctrl+] is the local escape key)
    Attach {
        /// Profile name
        #[arg(short = 'p', long)]
        profile: String,
        /// Session UUID; the first live session is used when omitted
        #[arg(long)]
        session_id: Option<uuid::Uuid>,
    },
    /// End a session
    End {
        /// Profile name
        #[arg(short = 'p', long)]
        profile: String,
        #[arg(long)]
        session_id: uuid::Uuid,
        /// Required to end a session while another client holds control
        #[arg(long)]
        take_control: bool,
    },
    /// Type into a session without a terminal, then wait for and return the output
    Send {
        /// Profile name
        #[arg(short = 'p', long)]
        profile: String,
        #[arg(long)]
        session_id: uuid::Uuid,
        /// Text to type, sent literally (no newline unless --enter)
        #[arg(long)]
        text: Option<String>,
        /// Press Enter after the text
        #[arg(long)]
        enter: bool,
        /// Named keys sent after the text, in order (enter, tab, esc, ctrl-c, up, f5, alt-x, ...)
        #[arg(long = "key", value_name = "KEY")]
        keys: Vec<String>,
        /// Required to send while another client holds control
        #[arg(long)]
        take_control: bool,
        /// Return as soon as the input is acknowledged instead of waiting for output
        #[arg(long)]
        no_wait: bool,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Read a session's screen, or the output since an offset, without a terminal
    Read {
        /// Profile name
        #[arg(short = 'p', long)]
        profile: String,
        #[arg(long)]
        session_id: uuid::Uuid,
        /// Return the output produced after this offset (the `offset` of an earlier result)
        #[arg(long)]
        since: Option<u64>,
        #[command(flatten)]
        wait: WaitArgs,
    },
}

/// `session send` / `session read` 的等待条件；满足任意一个就返回。
#[derive(clap::Args, Clone, Debug)]
pub struct WaitArgs {
    /// Return once there has been no new output for this many milliseconds (send waits 500 by default)
    #[arg(long, value_name = "MS")]
    wait_idle: Option<u64>,
    /// Return once the output or the screen matches this regular expression (^ and $ match per line)
    #[arg(long, value_name = "REGEX")]
    wait_for: Option<String>,
    /// Give up waiting after this many seconds (the result then has reason "timeout")
    #[arg(long, default_value_t = 30, value_name = "SECONDS")]
    timeout: u64,
}

/// 全局选项（`--json` / `--trust-fingerprint` / `--password-stdin`），由 `run_cli` 在分发前设置。
#[derive(Default)]
struct Options {
    json: bool,
    trust_fingerprint: Option<String>,
    password_stdin: bool,
}

static OPTIONS: OnceLock<Options> = OnceLock::new();

fn options() -> &'static Options {
    OPTIONS.get_or_init(Options::default)
}

fn emit(value: serde_json::Value) {
    println!("{value}");
}

/// 出错时的统一输出：`--json` 下是 `{"ok":false,"error":{"code":...,"message":...}}`（写到 stdout），
/// 否则沿用原来的 `Error: ...` 到 stderr。
pub fn report_error(err: &anyhow::Error) {
    if options().json {
        let coded = err.chain().find_map(|e| e.downcast_ref::<CodedError>());
        let mut error = json!({
            "code": coded.map_or("error", |c| c.code.as_str()),
            "message": format!("{err:#}"),
        });
        if let Some(data) = coded.and_then(|c| c.data.clone()) {
            error["details"] = data;
        }
        emit(json!({ "ok": false, "error": error }));
    } else {
        eprintln!("Error: {err:?}");
    }
}

/// 读取一个秘密（密码或私钥口令）：`--password-stdin` 读 stdin 首行；`--json` 下不提示而是报错；
/// 否则在终端里提示（不回显）。
fn read_secret(prompt: String) -> Result<String> {
    let opts = options();
    if opts.password_stdin {
        static SECRET: OnceLock<Option<String>> = OnceLock::new();
        let secret = SECRET.get_or_init(|| {
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).ok()?;
            Some(line.trim_end_matches(['\r', '\n']).to_string())
        });
        return secret.clone().filter(|s| !s.is_empty()).ok_or_else(|| {
            coded(
                "prompt_required",
                "--password-stdin was given but stdin has no password on its first line",
            )
        });
    }
    if opts.json {
        return Err(coded(
            "prompt_required",
            "a password or key passphrase is required; pass --password-stdin and pipe it in",
        ));
    }
    Ok(rpassword::prompt_password(prompt)?)
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
    let pw = read_secret(tr!(
        "Host password (at least 12 characters): ",
        "接收密码（至少 12 个字符）: "
    ))?;
    if confirm && !options().password_stdin {
        let again =
            rpassword::prompt_password(tr!("Enter it again to confirm: ", "再次输入以确认: "))?;
        if pw != again {
            bail!(tr!("The two entries do not match", "两次输入不一致"));
        }
    }
    Ok(pw)
}

/// 首次连接的指纹确认流程：probe -> 展示 -> 显式 yes 才入库。
/// 已有记录且不一致时直接阻断，不做任何降级。
async fn confirmed_fingerprint(host: &str, port: u16) -> Result<HostFingerprint> {
    let key = format!("{host}:{port}");
    let known = load_known_hosts()?;
    let actual = probe_host(host, port)
        .await
        .with_context(|| tr!("Failed to probe the host fingerprint", "探测主机指纹失败"))?;
    match known.entries.get(&key) {
        Some(expected) => {
            let exp = HostFingerprint::new(expected.clone());
            if exp.base64_part() == actual.base64_part() {
                Ok(actual)
            } else {
                Err(coded_with(
                    "fingerprint_changed",
                    tr!(
                        "Host fingerprint changed; connection blocked.\n  Recorded: {}\n  Actual:   {}\nIf the server was legitimately changed, delete the matching entry in {} by hand.",
                        "主机指纹变化，已阻断连接。\n  记录: {}\n  实际: {}\n如确认服务器变更，请手动删除 {} 中对应条目。",
                        exp.sha256,
                        actual.sha256,
                        known_hosts_path().display()
                    ),
                    json!({ "recorded": exp.sha256, "actual": actual.sha256 }),
                ))
            }
        }
        None => {
            let opts = options();
            if let Some(pinned) = &opts.trust_fingerprint {
                // 调用者已经通过可信渠道拿到了指纹：只有与实际一致才信任并记录。
                if HostFingerprint::new(pinned.clone()).base64_part() != actual.base64_part() {
                    return Err(coded_with(
                        "fingerprint_mismatch",
                        tr!(
                            "The host fingerprint {} does not match --trust-fingerprint {}",
                            "主机指纹 {} 与 --trust-fingerprint {} 不一致",
                            actual.sha256,
                            pinned
                        ),
                        json!({ "actual": actual.sha256 }),
                    ));
                }
            } else if opts.json {
                return Err(coded_with(
                    "fingerprint_untrusted",
                    format!(
                        "first connection to {key}: the host fingerprint {} is not trusted yet; verify it through a trusted channel and pass --trust-fingerprint",
                        actual.sha256
                    ),
                    json!({ "host": host, "port": port, "fingerprint": actual.sha256 }),
                ));
            } else {
                println!(
                    "{}",
                    tr!(
                        "First connection to {host}:{port}",
                        "首次连接 {host}:{port}"
                    )
                );
                println!(
                    "{}",
                    tr!("Host fingerprint: {}", "主机指纹: {}", actual.sha256)
                );
                let answer = prompt_tty(&tr!(
                    "Trust this fingerprint? Type yes to continue: ",
                    "确认并信任该指纹？输入 yes 继续: "
                ))?;
                if !answer.eq_ignore_ascii_case("yes") {
                    bail!(tr!(
                        "Fingerprint not confirmed; connection cancelled",
                        "未确认指纹，已取消连接"
                    ));
                }
            }
            let mut known = known;
            known.entries.insert(key, actual.sha256.clone());
            save_json(&known_hosts_path(), &known)
                .with_context(|| tr!("Failed to save known_hosts", "保存 known_hosts 失败"))?;
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
        .ok_or_else(|| {
            coded(
                "profile_not_found",
                tr!("Profile not found: {name}", "档案不存在: {name}"),
            )
        })
}

async fn connect_profile(profile: &Profile) -> Result<Client> {
    let fp = confirmed_fingerprint(&profile.host, profile.port).await?;
    let auth = match profile.auth {
        config::AuthKind::Key => {
            let path = profile.key_path.as_ref().with_context(|| {
                tr!(
                    "Profile {} uses key authentication but has no key_path",
                    "档案 {} 使用 key 认证但未配置 key_path",
                    profile.name
                )
            })?;
            let passphrase = match russh::keys::load_secret_key(path, None) {
                Ok(_) => None,
                Err(russh::keys::Error::KeyIsEncrypted) => Some(read_secret(tr!(
                    "SSH private key passphrase (not saved): ",
                    "SSH 私钥口令（不保存）: "
                ))?),
                Err(e) => {
                    return Err(e).with_context(|| {
                        tr!(
                            "Cannot read private key {}",
                            "无法读取私钥 {}",
                            path.display()
                        )
                    })
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
    .map_err(|e| {
        coded(
            "connect_failed",
            format!("{}: {e:#}", tr!("Connection failed", "连接失败")),
        )
    })
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
    read_secret(tr!(
        "Password for {}@{}: ",
        "{}@{} 的密码: ",
        profile.user,
        profile.host
    ))
}

fn store_password(profile_name: &str, password: &str) -> Result<()> {
    Ok(keyring::Entry::new(KEYRING_SERVICE, profile_name)?.set_password(password)?)
}

fn expect_response(resp: Response, what: &str) -> Result<Response> {
    match resp {
        Response::Error { code, message } => {
            let message = crate::i18n::wire_message(&code, &message);
            Err(coded(
                &code,
                tr!(
                    "{what} failed [{code}]: {message}",
                    "{what} 失败 [{code}]: {message}"
                ),
            ))
        }
        other => Ok(other),
    }
}

fn print_sessions(sessions: &[termbridge_protocol::SessionInfo]) {
    if sessions.is_empty() {
        println!("{}", tr!("(no sessions)", "（无会话）"));
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
    let _ = OPTIONS.set(Options {
        json: cli.json,
        trust_fingerprint: cli.trust_fingerprint.clone(),
        password_stdin: cli.password_stdin,
    });
    if let Some(lang) = cli.lang {
        crate::i18n::set_lang(lang.into());
    } else if cli.json {
        // 机器可读输出里的消息保持英文，不随系统语言变化。
        crate::i18n::set_lang(Lang::En);
    }
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
                let text = std::fs::read_to_string(&path).with_context(|| {
                    tr!(
                        "Failed to read the authorized keys file: {path}",
                        "读取授权公钥文件失败: {path}"
                    )
                })?;
                config::init_host_with_keys(&text)?
            } else {
                let password = prompt_password(true)?;
                init_host(&password)?
            };
            println!(
                "{}",
                tr!(
                    "Host initialized: user {}, fingerprint {}",
                    "接收端已初始化：用户 {}，指纹 {}",
                    cfg.username,
                    cfg.fingerprint()?
                )
            );
        }
        HostCommand::Status => {
            let cfg: HostConfig = read_json(&config::host_path())?;
            if options().json {
                let initialized = config::host_path().exists();
                emit(json!({
                    "ok": true,
                    "initialized": initialized,
                    "username": initialized.then(|| cfg.username.clone()),
                    "enabled": cfg.enabled,
                    "listen": cfg.listen.as_deref().unwrap_or(DEFAULT_LISTEN),
                    "password_auth": cfg.password_hash.is_some(),
                    "authorized_keys": cfg.authorized_keys.len(),
                    "fingerprint": if initialized { cfg.fingerprint().ok() } else { None },
                }));
                return Ok(());
            }
            if config::host_path().exists() {
                let fp = cfg
                    .fingerprint()
                    .unwrap_or_else(|e| tr!("(read failed: {e})", "（读取失败: {e}）"));
                let yes = tr!("yes", "是");
                let no = tr!("no", "否");
                println!(
                    "{}{}",
                    tr!("User:            ", "用户:        "),
                    cfg.username
                );
                println!(
                    "{}{}",
                    tr!("Enabled:         ", "已启用:      "),
                    if cfg.enabled { &yes } else { &no }
                );
                println!(
                    "{}{}",
                    tr!("Listen:          ", "监听:        "),
                    cfg.listen.as_deref().unwrap_or(DEFAULT_LISTEN)
                );
                println!(
                    "{}{}",
                    tr!("Password auth:   ", "密码认证:    "),
                    if cfg.password_hash.is_some() {
                        tr!("enabled", "已启用")
                    } else {
                        tr!("disabled", "已关闭")
                    }
                );
                println!(
                    "{}{}",
                    tr!("Authorized keys: ", "授权密钥数:  "),
                    cfg.authorized_keys.len()
                );
                println!("{}{fp}", tr!("Fingerprint:     ", "主机指纹:    "));
            } else {
                println!(
                    "{}",
                    tr!(
                        "The host is not initialized (missing {})",
                        "接收端未初始化（缺少 {}）",
                        config::host_path().display()
                    )
                );
            }
        }
        HostCommand::Enable { disable, listen } => {
            if !config::host_path().exists() {
                bail!(tr!(
                    "The host is not initialized; run host init first",
                    "接收端未初始化，请先执行 host init"
                ));
            }
            let mut cfg: HostConfig = read_json(&config::host_path())?;
            if !disable {
                let text = listen.or_else(|| cfg.listen.clone()).with_context(|| {
                    tr!(
                        "The first enable needs an explicit listen address via --listen",
                        "首次启用必须用 --listen 明确选择监听地址"
                    )
                })?;
                let addr: std::net::SocketAddr = text
                    .parse()
                    .with_context(|| tr!("Invalid listen address", "无效监听地址"))?;
                if addr.port() == 0 {
                    bail!(tr!("The listen port cannot be 0", "监听端口不能为 0"));
                }
                cfg.listen = Some(addr.to_string());
            }
            cfg.enabled = !disable;
            save_json(&config::host_path(), &cfg)?;
            let state = if cfg.enabled {
                tr!("enabled", "启用")
            } else {
                tr!("disabled", "停用")
            };
            println!(
                "{}",
                tr!(
                    "Host configuration {}; if the service is running, it still has to be stopped after disabling",
                    "接收端配置已{}；若服务正在运行，停用后仍需停止该进程",
                    state
                )
            );
        }
        HostCommand::Run { listen } => {
            let cfg: HostConfig = read_json(&config::host_path())?;
            if !config::host_path().exists() {
                bail!(tr!(
                    "The host is not initialized; run host init first",
                    "接收端未初始化，请先执行 host init"
                ));
            }
            if !cfg.enabled {
                bail!(tr!(
                    "The host is not enabled; run host enable first",
                    "接收端未启用，请先执行 host enable"
                ));
            }
            let addr_text = listen.or_else(|| cfg.listen.clone()).with_context(|| {
                tr!(
                    "Choose an address first with host enable --listen",
                    "请先用 host enable --listen 选择地址"
                )
            })?;
            let addr: std::net::SocketAddr = addr_text.parse().with_context(|| {
                tr!(
                    "Invalid listen address {addr_text}",
                    "无效监听地址 {addr_text}"
                )
            })?;
            if !addr.ip().is_loopback() {
                println!(
                    "{}",
                    tr!(
                        "Warning: listening on the non-loopback address {addr}; any reachable machine can try to connect.",
                        "警告：正在监听非回环地址 {addr}，任何可达主机都可尝试连接。"
                    )
                );
            }
            crate::server::run(cfg, addr).await?;
        }
        HostCommand::UseSshKeys { authorized_keys } => {
            if !config::host_path().exists() {
                bail!(tr!(
                    "The host is not initialized; run host init first",
                    "接收端未初始化，请先执行 host init"
                ));
            }
            let text = std::fs::read_to_string(&authorized_keys).with_context(|| {
                tr!(
                    "Failed to read the authorized keys file: {authorized_keys}",
                    "读取授权公钥文件失败: {authorized_keys}"
                )
            })?;
            let mut cfg: HostConfig = read_json(&config::host_path())?;
            config::switch_to_keys_only(&mut cfg, &text)?;
            println!(
                "{}",
                tr!(
                    "Switched to key-only authentication ({} authorized keys); if the host is running, stop and restart it",
                    "已改为仅密钥认证（{} 把授权密钥）；若接收端正在运行，请先停止并重启它",
                    cfg.authorized_keys.len()
                )
            );
        }
        HostCommand::KeyAdd { key_file } => {
            let mut cfg: HostConfig = read_json(&config::host_path())?;
            if !config::host_path().exists() {
                bail!(tr!(
                    "The host is not initialized; run host init first",
                    "接收端未初始化，请先执行 host init"
                ));
            }
            let text = match key_file {
                Some(path) => std::fs::read_to_string(path).with_context(|| {
                    tr!("Failed to read the public key file", "读取公钥文件失败")
                })?,
                None => {
                    print!(
                        "{}",
                        tr!(
                            "Paste an OpenSSH public key (one line): ",
                            "粘贴 OpenSSH 公钥（一行）: "
                        )
                    );
                    std::io::stdout().flush()?;
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line)?;
                    line
                }
            };
            add_public_key(&mut cfg, &text)?;
            println!(
                "{}",
                tr!(
                    "Public key added; {} authorized keys in total",
                    "公钥已添加，当前共 {} 把授权密钥",
                    cfg.authorized_keys.len()
                )
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
                bail!(tr!("Profile already exists: {name}", "档案已存在: {name}"));
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
                let pw = read_secret(tr!(
                    "Password for {}@{}: ",
                    "{}@{} 的密码: ",
                    profile.user,
                    profile.host
                ))?;
                store_password(&profile.id.to_string(), &pw)?;
            }
            profiles.items.push(profile);
            save_json(&profiles_path(), &profiles)?;
            if options().json {
                let saved = profiles.items.last().map(|p| p.name.clone());
                emit(json!({ "ok": true, "profile": saved }));
            } else {
                println!("{}", tr!("Profile saved", "档案已保存"));
            }
        }
        ProfileCommand::List => {
            let profiles: Profiles = read_json(&profiles_path())?;
            if options().json {
                let items: Vec<_> = profiles
                    .items
                    .iter()
                    .map(|p| {
                        json!({
                            "name": p.name,
                            "host": p.host,
                            "port": p.port,
                            "user": p.user,
                            "auth": format!("{:?}", p.auth).to_lowercase(),
                            "key_path": p.key_path,
                            "remember_password": p.remember_password,
                        })
                    })
                    .collect();
                emit(json!({ "ok": true, "profiles": items }));
                return Ok(());
            }
            if profiles.items.is_empty() {
                println!("{}", tr!("(no profiles)", "（无档案）"));
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
                bail!(tr!("Profile not found: {name}", "档案不存在: {name}"));
            }
            if let Some(profile) = removed {
                if profile.remember_password {
                    let _ = keyring::Entry::new(KEYRING_SERVICE, &profile.id.to_string())?
                        .delete_credential();
                }
            }
            save_json(&profiles_path(), &profiles)?;
            if options().json {
                emit(json!({ "ok": true, "removed": name }));
            } else {
                println!("{}", tr!("Profile removed: {name}", "档案已删除: {name}"));
            }
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
        SessionCommand::Send {
            profile,
            session_id,
            text,
            enter,
            keys,
            take_control,
            no_wait,
            wait,
        } => {
            // 连接之前先校验参数：按键名写错不该白白建立一次连接。
            let mut data = text.unwrap_or_default().into_bytes();
            if enter {
                data.push(b'\r');
            }
            for key in &keys {
                let bytes = agent::key_bytes(key)
                    .ok_or_else(|| coded("invalid_args", format!("unknown key name: {key}")))?;
                data.extend_from_slice(&bytes);
            }
            if data.is_empty() {
                return Err(coded(
                    "invalid_args",
                    "nothing to send: pass --text, --enter or --key",
                ));
            }
            let spec = wait_spec(&wait, Some(500), !no_wait)?;
            (
                profile,
                SessionAction::Send {
                    session_id,
                    data,
                    take_control,
                    spec,
                    settle: !no_wait,
                },
            )
        }
        SessionCommand::Read {
            profile,
            session_id,
            since,
            wait,
        } => {
            let spec = wait_spec(&wait, None, true)?;
            (
                profile,
                SessionAction::Read {
                    session_id,
                    since,
                    spec,
                },
            )
        }
    };
    if matches!(action, SessionAction::Attach { .. }) && !std::io::stdin().is_terminal() {
        bail!(tr!(
            "session attach needs a real terminal; piped input is not supported",
            "session attach 需要真实终端；不支持管道输入"
        ));
    }
    let profile = load_profile(&profile_name)?;
    let mut client = connect_profile(&profile).await?;
    let result = match action {
        SessionAction::List => {
            let resp = expect_response(client.request(Request::List).await?, "list")?;
            if let Response::Sessions { sessions } = resp {
                if options().json {
                    emit(json!({ "ok": true, "sessions": sessions }));
                } else {
                    print_sessions(&sessions);
                }
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
                if options().json {
                    emit(json!({ "ok": true, "session": session }));
                } else {
                    println!(
                        "{}",
                        tr!("Session created: {}", "会话已创建: {}", session.id)
                    );
                }
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
                bail!(tr!(
                    "attach returned an unexpected response",
                    "attach 返回意外响应"
                ))
            };
            if !has_control {
                if !take_control {
                    return Err(coded(
                        "not_controller",
                        tr!(
                            "Another client holds control; pass --take-control explicitly",
                            "另一客户端持有控制权；请显式使用 --take-control"
                        ),
                    ));
                }
                expect_response(
                    client.request(Request::TakeControl { session_id }).await?,
                    "take",
                )?;
            }
            expect_response(client.request(Request::End { session_id }).await?, "end")?;
            if options().json {
                emit(json!({ "ok": true, "session_id": session_id }));
            } else {
                println!(
                    "{}",
                    tr!("Session ended: {session_id}", "会话已结束: {session_id}")
                );
            }
            Ok(())
        }
        SessionAction::Attach { session_id } => raw::attach_raw(&mut client, session_id).await,
        SessionAction::Send {
            session_id,
            data,
            take_control,
            spec,
            settle,
        } => {
            agent_send(
                &mut client,
                session_id,
                &data,
                take_control,
                spec.as_ref(),
                settle,
            )
            .await
        }
        SessionAction::Read {
            session_id,
            since,
            spec,
        } => agent_read(&mut client, session_id, since, spec.as_ref()).await,
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
    Send {
        session_id: uuid::Uuid,
        data: Vec<u8>,
        take_control: bool,
        spec: Option<agent::WaitSpec>,
        settle: bool,
    },
    Read {
        session_id: uuid::Uuid,
        since: Option<u64>,
        spec: Option<agent::WaitSpec>,
    },
}

/// 把等待参数变成等待条件。`default_idle` 是没写任何条件时的默认空闲时长，`enabled` 为 false
/// 表示不等待。
fn wait_spec(
    args: &WaitArgs,
    default_idle: Option<u64>,
    enabled: bool,
) -> Result<Option<agent::WaitSpec>> {
    if !enabled {
        return Ok(None);
    }
    let idle = args.wait_idle.or(if args.wait_for.is_some() {
        None
    } else {
        default_idle
    });
    if idle.is_none() && args.wait_for.is_none() {
        return Ok(None);
    }
    // 多行模式：`^` / `$` 匹配每一行的行首行尾，而不只是整段输出的首尾。
    let until = args
        .wait_for
        .as_deref()
        .map(|pattern| regex::RegexBuilder::new(pattern).multi_line(true).build())
        .transpose()
        .map_err(|e| {
            coded(
                "invalid_args",
                format!("invalid --wait-for regular expression: {e}"),
            )
        })?;
    Ok(Some(agent::WaitSpec {
        idle: idle.map(Duration::from_millis),
        until,
        timeout: Duration::from_secs(args.timeout.clamp(1, 3600)),
    }))
}

/// 发送输入（必要时先接管），等到接收端确认，再按等待条件收集输出。
async fn agent_send(
    client: &mut Client,
    session_id: uuid::Uuid,
    data: &[u8],
    take_control: bool,
    spec: Option<&agent::WaitSpec>,
    settle: bool,
) -> Result<()> {
    let mut collector = agent::Collector::new(session_id, None);
    let attachment = agent::attach(client, session_id, None).await?;
    agent::wait_snapshot(client, &mut collector).await?;
    agent::ensure_control(client, session_id, attachment.has_control, take_control).await?;
    let input_offset = agent::send_input(
        client,
        &mut collector,
        session_id,
        attachment.stream_id,
        attachment.input_next,
        data,
    )
    .await?;
    let reason = agent::wait_for(client, &mut collector, spec, settle).await?;
    let extra = json!({ "sent_bytes": data.len(), "input_offset": input_offset });
    print_observation(session_id, &attachment.session, &collector, reason, extra);
    Ok(())
}

/// 读取屏幕，或读取某个偏移之后的输出。
async fn agent_read(
    client: &mut Client,
    session_id: uuid::Uuid,
    since: Option<u64>,
    spec: Option<&agent::WaitSpec>,
) -> Result<()> {
    let mut collector = agent::Collector::new(session_id, since);
    let attachment = agent::attach(client, session_id, since).await?;
    if !attachment.resumed {
        agent::wait_snapshot(client, &mut collector).await?;
    }
    let reason = agent::wait_for(client, &mut collector, spec, true).await?;
    // 指定了 since 但接收端已经回收了那段输出，只能退回到当前画面。
    let extra = json!({ "since_unavailable": since.is_some() && !attachment.resumed });
    print_observation(session_id, &attachment.session, &collector, reason, extra);
    Ok(())
}

/// `send` / `read` 的结果：JSON 下输出完整对象；文本下输出新输出，没有则输出屏幕。
fn print_observation(
    session_id: uuid::Uuid,
    session: &SessionInfo,
    collector: &agent::Collector,
    reason: agent::Reason,
    extra: serde_json::Value,
) {
    let output = collector.output_text();
    let screen = collector.screen_view();
    if options().json {
        let mut value = json!({
            "ok": true,
            "session_id": session_id,
            "live": session.live && !collector.ended,
            "reason": reason,
            "offset": collector.end_offset,
            "output": output,
            "output_truncated": collector.truncated,
            "output_gap": collector.gap,
            "screen": screen,
        });
        if let (Some(map), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            map.extend(extra.clone());
        }
        emit(value);
    } else if !output.is_empty() {
        println!("{output}");
    } else if let Some(screen) = screen {
        println!("{}", screen.lines.join("\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> impl Iterator<Item = String> {
        list.iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn lang_flag_is_read_before_clap_runs() {
        assert_eq!(
            lang_from_args(args(&["--lang", "zh", "host"])),
            Some(Lang::Zh)
        );
        assert_eq!(lang_from_args(args(&["host", "--lang=en"])), Some(Lang::En));
        assert_eq!(lang_from_args(args(&["host", "status"])), None);
        // `--` ends option parsing; an unknown value is left for clap to reject.
        assert_eq!(lang_from_args(args(&["--", "--lang", "zh"])), None);
        assert_eq!(lang_from_args(args(&["--lang", "fr"])), None);
        assert_eq!(lang_from_args(args(&["--lang"])), None);
    }

    /// 每条英文帮助都必须有中文翻译，本地化后的命令树仍然合法。
    #[test]
    fn every_help_text_has_a_zh_translation() {
        fn walk(cmd: &clap::Command, path: &str, missing: &mut Vec<String>) {
            if cmd.get_about().is_some() && zh_help(path).is_none() {
                missing.push(format!("command `{path}`"));
            }
            for arg in cmd.get_arguments() {
                let key = format!("{path}/{}", arg.get_id());
                if arg.get_help().is_some() && zh_help(&key).is_none() {
                    missing.push(format!("argument `{key}`"));
                }
            }
            for sub in cmd.get_subcommands() {
                let child = if path.is_empty() {
                    sub.get_name().to_string()
                } else {
                    format!("{path} {}", sub.get_name())
                };
                walk(sub, &child, missing);
            }
        }
        let mut missing = Vec::new();
        walk(&Cli::command(), "", &mut missing);
        assert!(missing.is_empty(), "missing zh help: {missing:?}");
        localize_command(Cli::command(), Lang::Zh).debug_assert();
    }

    #[test]
    fn zh_help_is_rendered_in_chinese() {
        let mut zh = localize_command(Cli::command(), Lang::Zh);
        let help = zh.render_help().to_string();
        assert!(help.contains("接收端（本机作为被连接方）"), "{help}");
        let mut en = localize_command(Cli::command(), Lang::En);
        let help = en.render_help().to_string();
        assert!(
            help.contains("Host: run this machine as the side being connected to"),
            "{help}"
        );
    }
}
