//! Config: a TOML file overlaid by env vars. Auth creds usually come from the
//! file (`mcp-ssh set-auth`); everything has a sane default except the creds.
use std::{net::SocketAddr, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub user: String,
    pub pass: String,
    /// Commands finishing within this window return inline; slower ones background.
    pub inline_timeout: Duration,
    /// Where per-job log files live.
    pub job_dir: PathBuf,
    /// Hostnames accepted in the `Host` header (rmcp DNS-rebinding guard).
    pub allowed_hosts: Vec<String>,
    /// Public base URL (e.g. https://mcp.example.com). When unset, OAuth metadata
    /// URLs are derived from the request `Host` header.
    pub public_url: Option<String>,
}

/// On-disk shape. All fields optional so a partial file still loads.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct FileConfig {
    pub bind: Option<String>,
    pub user: Option<String>,
    pub pass: Option<String>,
    pub inline_timeout_secs: Option<u64>,
    pub job_dir: Option<String>,
    pub allowed_hosts: Option<Vec<String>>,
    pub public_url: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no auth credentials: run `mcp-ssh set-auth <user>` or set MCP_SSH_USER/MCP_SSH_PASS")]
    NoCredentials,
    #[error("invalid {0}: {1}")]
    Invalid(&'static str, String),
    #[error("config io error at {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("config parse error: {0}")]
    Parse(String),
}

/// Default config path: `$MCP_SSH_CONFIG`, else `/etc/mcp-ssh/config.toml`.
pub fn config_path() -> PathBuf {
    std::env::var("MCP_SSH_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/mcp-ssh/config.toml"))
}

impl Config {
    /// Load file (if present) then overlay env vars. Env wins.
    pub fn load() -> Result<Self, ConfigError> {
        let file = load_file(&config_path())?;

        let bind = pick("MCP_SSH_BIND", file.bind, "127.0.0.1:1337")
            .parse()
            .map_err(|e| ConfigError::Invalid("bind", format!("{e}")))?;

        let user = opt("MCP_SSH_USER", file.user).ok_or(ConfigError::NoCredentials)?;
        let pass = opt("MCP_SSH_PASS", file.pass).ok_or(ConfigError::NoCredentials)?;

        let inline_timeout = match std::env::var("MCP_SSH_INLINE_TIMEOUT_SECS").ok() {
            Some(v) => v
                .parse()
                .map_err(|e| ConfigError::Invalid("inline_timeout", format!("{e}")))?,
            None => file.inline_timeout_secs.unwrap_or(2),
        };

        let job_dir = PathBuf::from(pick(
            "MCP_SSH_JOB_DIR",
            file.job_dir,
            "/var/lib/mcp-ssh/jobs",
        ));

        let allowed_hosts = match std::env::var("MCP_SSH_ALLOWED_HOSTS").ok() {
            Some(v) => split_hosts(&v),
            None => file
                .allowed_hosts
                .unwrap_or_else(|| vec!["localhost".into(), "127.0.0.1".into()]),
        };

        let public_url = opt("MCP_SSH_PUBLIC_URL", file.public_url);

        Ok(Self {
            bind,
            user,
            pass,
            inline_timeout: Duration::from_secs(inline_timeout),
            job_dir,
            allowed_hosts,
            public_url,
        })
    }
}

/// Write `user`/`pass` into the config file, preserving other fields. Chmod 600.
pub fn set_auth(user: &str, pass: &str) -> Result<PathBuf, ConfigError> {
    let path = config_path();
    let mut file = load_file(&path)?;
    file.user = Some(user.to_string());
    file.pass = Some(pass.to_string());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ConfigError::Io(parent.to_path_buf(), e))?;
    }
    let toml = toml::to_string_pretty(&file).map_err(|e| ConfigError::Parse(e.to_string()))?;
    std::fs::write(&path, toml).map_err(|e| ConfigError::Io(path.clone(), e))?;
    chmod_600(&path);
    Ok(path)
}

fn load_file(path: &std::path::Path) -> Result<FileConfig, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e| ConfigError::Parse(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(ConfigError::Io(path.to_path_buf(), e)),
    }
}

#[cfg(unix)]
fn chmod_600(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn chmod_600(_path: &std::path::Path) {}

fn split_hosts(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// env, else file value, else default.
fn pick(env: &str, file: Option<String>, default: &str) -> String {
    std::env::var(env)
        .ok()
        .or(file)
        .unwrap_or_else(|| default.to_string())
}

/// env, else file value.
fn opt(env: &str, file: Option<String>) -> Option<String> {
    std::env::var(env).ok().or(file)
}
