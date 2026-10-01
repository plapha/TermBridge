use anyhow::{bail, Context, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand::rng;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    Password,
    Key,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub id: Uuid,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthKind,
    pub key_path: Option<PathBuf>,
    pub remember_password: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Profiles {
    pub items: Vec<Profile>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostConfig {
    pub username: String,
    pub password_hash: Option<String>,
    pub authorized_keys: Vec<String>,
    pub listen: Option<String>,
    pub enabled: bool,
}

impl HostConfig {
    pub fn verify_password(&self, user: &str, password: &str) -> bool {
        if user != self.username {
            return false;
        }
        let Some(hash) = self.password_hash.as_ref() else {
            return false;
        };
        let Ok(hash) = PasswordHash::new(hash) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    }
    pub fn verify_key(&self, user: &str, key: &PublicKey) -> bool {
        if user != self.username {
            return false;
        }
        // 只比较密钥材料：ssh-key 的 PartialEq 会把注释也算进去，
        // 而 ssh-keygen 生成的 .pub 默认带注释。
        self.authorized_keys.iter().any(|s| {
            PublicKey::from_openssh(s)
                .map(|k| k.key_data() == key.key_data())
                .unwrap_or(false)
        })
    }
    pub fn fingerprint(&self) -> Result<String> {
        let path = host_key_path();
        let key = russh::keys::load_secret_key(path, None)?;
        Ok(key
            .public_key()
            .fingerprint(russh::keys::HashAlg::Sha256)
            .to_string())
    }
}

pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(
            std::env::var_os("LOCALAPPDATA")
                .unwrap_or_else(|| std::env::var_os("USERPROFILE").unwrap_or_default()),
        )
        .join("TermBridge")
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
            })
            .join("termbridge")
    }
}
pub fn profiles_path() -> PathBuf {
    config_dir().join("profiles.json")
}
pub fn host_path() -> PathBuf {
    config_dir().join("host.json")
}
pub fn host_key_path() -> PathBuf {
    config_dir().join("host_key")
}
pub fn known_hosts_path() -> PathBuf {
    config_dir().join("known_hosts.json")
}

fn ssh_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    Ok(PathBuf::from(home.with_context(|| {
        crate::tr!("Cannot determine the home directory", "无法确定用户主目录")
    })?)
    .join(".ssh"))
}

/// 检查私钥是否需要口令；不读取或返回私钥明文给调用方。
pub fn key_passphrase_required(path: &Path) -> Result<bool> {
    match russh::keys::load_secret_key(path, None) {
        Ok(_) => Ok(false),
        Err(russh::keys::Error::KeyIsEncrypted) => Ok(true),
        Err(e) => Err(e).with_context(|| {
            crate::tr!(
                "Cannot read private key {}",
                "无法读取私钥 {}",
                path.display()
            )
        }),
    }
}

pub fn validate_private_key(path: &Path, passphrase: Option<&str>) -> Result<()> {
    russh::keys::load_secret_key(path, passphrase).with_context(|| {
        crate::tr!(
            "Cannot read private key {}",
            "无法读取私钥 {}",
            path.display()
        )
    })?;
    Ok(())
}
/// 显式选择复用 SSH 授权密钥时的默认来源；不会自动授予访问权。
pub fn default_ssh_authorized_keys_path() -> Result<PathBuf> {
    Ok(ssh_dir()?.join("authorized_keys"))
}

/// 客户端复用已有私钥文件，只返回路径，从不复制私钥内容。
pub fn default_ssh_private_key_path() -> Result<PathBuf> {
    let dir = ssh_dir()?;
    for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
        let path = dir.join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    bail!(crate::tr!(
        "No ~/.ssh/id_ed25519, id_ecdsa or id_rsa found; pass --key-path explicitly",
        "未发现 ~/.ssh/id_ed25519、id_ecdsa 或 id_rsa；请显式提供 --key-path"
    ))
}

pub fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    if !path.exists() {
        return Ok(T::default());
    }
    serde_json::from_slice(&fs::read(path).with_context(|| format!("reading {}", path.display()))?)
        .with_context(|| format!("invalid JSON in {}", path.display()))
}

pub fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let tmp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(value)?)?;
        f.sync_all()?;
        drop(f);
        #[cfg(not(windows))]
        fs::rename(&tmp, path)?;
        #[cfg(windows)]
        {
            if path.exists() {
                use std::os::windows::ffi::OsStrExt;
                use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
                let wide = |p: &Path| {
                    p.as_os_str()
                        .encode_wide()
                        .chain(Some(0))
                        .collect::<Vec<u16>>()
                };
                let dst = wide(path);
                let src = wide(&tmp);
                let ok = unsafe {
                    ReplaceFileW(
                        dst.as_ptr(),
                        src.as_ptr(),
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                };
                if ok == 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            } else {
                fs::rename(&tmp, path)?;
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

/// 拒绝 authorized_keys 选项行（如 from=、command=），防止静默抹掉原 SSH 的访问限制。
pub fn parse_authorized_keys(text: &str) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut key = PublicKey::from_openssh(line).with_context(|| {
            crate::tr!(
                "Line {} of the authorized keys file is not supported; remove option lines or provide plain public keys",
                "授权公钥文件第 {} 行不受支持；请移除选项行或提供纯公钥",
                index + 1
            )
        })?;
        key.set_comment("");
        let normalized = key.to_openssh()?;
        if !keys.contains(&normalized) {
            keys.push(normalized);
        }
    }
    if keys.is_empty() {
        bail!(crate::tr!(
            "The authorized keys file has no valid keys; refusing to enable a host without authentication",
            "授权公钥文件没有有效密钥，拒绝启用无认证的接收端"
        ));
    }
    Ok(keys)
}

/// 兼容原来的产品专用密码初始化方式。
pub fn init_host(password: &str) -> Result<HostConfig> {
    if password.len() < 12 {
        bail!(crate::tr!(
            "The host password must be at least 12 characters",
            "接收密码至少需要 12 个字符"
        ));
    }
    init_host_inner(Some(password), Vec::new())
}

/// 显式复用用户选定的 SSH authorized_keys，完全不启用密码认证。
pub fn init_host_with_keys(authorized_keys_text: &str) -> Result<HostConfig> {
    let keys = parse_authorized_keys(authorized_keys_text)?;
    init_host_inner(None, keys)
}

fn init_host_inner(password: Option<&str>, authorized_keys: Vec<String>) -> Result<HostConfig> {
    if password.is_none() && authorized_keys.is_empty() {
        bail!(crate::tr!(
            "The host needs at least one authentication method",
            "接收端至少需要一种认证方式"
        ));
    }
    if host_path().exists() || host_key_path().exists() {
        bail!(crate::tr!(
            "The host is already initialized; refusing to overwrite the host key",
            "接收端已初始化，拒绝覆盖主机密钥"
        ));
    }
    fs::create_dir_all(config_dir())?;
    let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519)?;
    let path = host_key_path();
    let private = key.to_openssh(russh::keys::ssh_key::LineEnding::LF)?;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&path)?.write_all(private.as_bytes())?;
    let password_hash = password
        .map(|password| -> Result<String> {
            let salt = SaltString::generate(&mut rand_core::OsRng);
            Ok(Argon2::default()
                .hash_password(password.as_bytes(), &salt)
                .map_err(|e| {
                    anyhow::anyhow!(crate::tr!(
                        "Password hashing failed: {e}",
                        "密码哈希失败: {e}"
                    ))
                })?
                .to_string())
        })
        .transpose()?;
    let username = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .with_context(|| crate::tr!("Cannot determine the local username", "无法确定本机用户名"))?;
    let config = HostConfig {
        username,
        password_hash,
        authorized_keys,
        listen: None,
        enabled: false,
    };
    save_json(&host_path(), &config)?;
    Ok(config)
}

/// 将已初始化的接收端显式改为只接受 SSH 公钥；先校验新文件，再原子保存。
pub fn switch_to_keys_only(config: &mut HostConfig, text: &str) -> Result<()> {
    let keys = parse_authorized_keys(text)?;
    let mut next = config.clone();
    next.authorized_keys = keys;
    next.password_hash = None;
    save_json(&host_path(), &next)?;
    *config = next;
    Ok(())
}

pub fn add_public_key(config: &mut HostConfig, text: &str) -> Result<()> {
    let mut key = PublicKey::from_openssh(text.trim())?;
    key.set_comment("");
    let normalized = key.to_openssh()?;
    if !config.authorized_keys.contains(&normalized) {
        config.authorized_keys.push(normalized);
    }
    save_json(&host_path(), config)
}

#[cfg(test)]
mod ssh_key_tests {
    use super::*;
    #[test]
    fn import_rejects_empty_and_restricted_authorized_keys() {
        assert!(parse_authorized_keys("# comment\n  ").is_err());
        let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let public = key.public_key().to_openssh().unwrap();
        assert_eq!(
            parse_authorized_keys(&format!("{public}\n{public}\n"))
                .unwrap()
                .len(),
            1
        );
        assert!(parse_authorized_keys(&format!("from=\"192.0.2.1\" {public}")).is_err());
    }

    #[test]
    fn keys_with_comments_match_and_are_normalized() {
        let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let public = key.public_key().to_openssh().unwrap();
        let with_comment = format!("{public} user@example");
        let keys = parse_authorized_keys(&with_comment).unwrap();
        assert_eq!(keys.len(), 1);
        assert!(
            !keys[0].contains("user@example"),
            "导入时必须去掉注释：{}",
            keys[0]
        );
        let config = HostConfig {
            username: "u".into(),
            authorized_keys: keys,
            ..Default::default()
        };
        assert!(config.verify_key("u", &key.public_key()));
        // 旧配置里已存的带注释条目也必须继续匹配客户端密钥。
        let legacy = HostConfig {
            username: "u".into(),
            authorized_keys: vec![with_comment],
            ..Default::default()
        };
        assert!(legacy.verify_key("u", &key.public_key()));
    }
}

#[cfg(test)]
mod keyring_tests {
    /// keyring 3 在没有启用任何平台后端时会悄悄退回「内存模拟存储」：`set_password` 成功，
    /// 但换一个 `Entry` 就读不回来。那样“记住密码”和 GUI 里确认过的主机指纹都不会真正保存。
    /// Cargo.toml 里必须启用各平台的真实后端。
    #[test]
    fn keyring_uses_a_real_credential_store_not_the_mock() {
        let builder = keyring::default::default_credential_builder();
        assert!(
            !builder
                .as_any()
                .is::<keyring::mock::MockCredentialBuilder>(),
            "keyring is using the in-memory mock; enable the platform features in Cargo.toml"
        );
    }
}
