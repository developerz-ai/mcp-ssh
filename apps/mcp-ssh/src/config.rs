//! Config: a TOML file overlaid by env vars. Auth creds usually come from the
//! file (`mcp-ssh set-auth`); everything has a sane default except the creds.
use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};

mod set_auth;

pub use set_auth::set_auth;

/// The login password. Its `Debug` is redacted, so a config dumped to a log, a
/// panic message or an error report never carries it.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Password(String);

impl Password {
    pub fn new(pass: impl Into<String>) -> Self {
        Self(pass.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

/// Longest `inline_timeout` honoured: a request held open longer than this before
/// backgrounding just ties up the client.
const MAX_INLINE_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub user: String,
    pub pass: Password,
    /// Commands finishing within this window return inline; slower ones background.
    pub inline_timeout: Duration,
    /// Where per-job log files live.
    pub job_dir: PathBuf,
    /// SQLite database file (OAuth tokens + job metadata + output tail).
    pub db_path: PathBuf,
    /// Hostnames accepted in the `Host` header (rmcp DNS-rebinding guard).
    pub allowed_hosts: Vec<String>,
    /// Public base URL, normalised to `scheme://host[:port]` (e.g.
    /// https://mcp.example.com). When unset, OAuth metadata URLs are derived from
    /// the (allow-listed) request `Host` header.
    pub public_url: Option<String>,
}

/// On-disk shape. All fields optional so a partial file still loads.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct FileConfig {
    pub bind: Option<String>,
    pub user: Option<String>,
    pub pass: Option<Password>,
    pub inline_timeout_secs: Option<u64>,
    pub job_dir: Option<String>,
    pub db_path: Option<String>,
    pub allowed_hosts: Option<Vec<String>>,
    pub public_url: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no auth credentials: run `mcp-ssh set-auth <user>` or set MCP_SSH_USER/MCP_SSH_PASS")]
    NoCredentials,
    #[error(
        "bind {0} is not loopback but MCP_SSH_ALLOWED_HOSTS is unset or empty: set it to the \
         public hostname(s), or bind to loopback behind the TLS reverse proxy"
    )]
    AllowedHostsRequired(IpAddr),
    #[error("invalid {0}: {1}")]
    Invalid(&'static str, String),
    #[error("config io error at {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("config parse error: {0}")]
    Parse(String),
}

/// Source of environment variables. Production reads the real process env; tests
/// supply an in-memory map, so config loading needs no process-env mutation
/// (`unsafe` under Rust 2024) and stays race-free without a global lock.
trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

/// Reads the real process environment.
#[derive(Debug)]
struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Default config path: `$MCP_SSH_CONFIG`, else `/etc/mcp-ssh/config.toml`.
fn config_path(env: &dyn EnvSource) -> PathBuf {
    env.get("MCP_SSH_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/mcp-ssh/config.toml"))
}

impl Config {
    /// Load file (if present) then overlay env vars. Env wins.
    pub fn load() -> Result<Self, ConfigError> {
        Self::from_env(&ProcessEnv)
    }

    /// Load file then overlay `env`. Real env in production; an in-memory map in tests.
    fn from_env(env: &dyn EnvSource) -> Result<Self, ConfigError> {
        let file = load_file(&config_path(env))?;

        let bind: SocketAddr = pick(env, "MCP_SSH_BIND", file.bind, "127.0.0.1:1337")
            .parse()
            .map_err(|e| ConfigError::Invalid("bind", format!("{e}")))?;

        // A blank value is no credential: it must neither start the server with an
        // empty password nor (as an empty env var) shadow the file's real one.
        let user = opt(env, "MCP_SSH_USER", file.user).ok_or(ConfigError::NoCredentials)?;
        let pass = opt(env, "MCP_SSH_PASS", file.pass.map(|p| p.0))
            .map(Password)
            .ok_or(ConfigError::NoCredentials)?;

        let inline_timeout = match env.get("MCP_SSH_INLINE_TIMEOUT_SECS") {
            Some(v) => v
                .parse()
                .map_err(|e| ConfigError::Invalid("inline_timeout", format!("{e}")))?,
            None => file.inline_timeout_secs.unwrap_or(2),
        };
        if inline_timeout > MAX_INLINE_TIMEOUT_SECS {
            tracing::warn!(
                requested = inline_timeout,
                max = MAX_INLINE_TIMEOUT_SECS,
                "inline_timeout clamped"
            );
        }
        let inline_timeout = inline_timeout.min(MAX_INLINE_TIMEOUT_SECS);

        let job_dir = PathBuf::from(pick(
            env,
            "MCP_SSH_JOB_DIR",
            file.job_dir,
            "/var/lib/mcp-ssh/logs/jobs",
        ));

        // SQLite DB at the StateDirectory root, independent of the (deeper) job-log
        // dir so moving logs around never drags the database with them.
        let db_path = PathBuf::from(pick(
            env,
            "MCP_SSH_DB",
            file.db_path,
            "/var/lib/mcp-ssh/mcp-ssh.db",
        ));

        let explicit_hosts = match env.get("MCP_SSH_ALLOWED_HOSTS") {
            Some(v) => Some(split_hosts(&v)),
            None => file
                .allowed_hosts
                .map(|hosts| normalize_hosts(hosts.into_iter())),
        };
        let allowed_hosts = match explicit_hosts {
            Some(hosts) if !hosts.is_empty() => hosts,
            // Unset, OR set but empty after trimming (`MCP_SSH_ALLOWED_HOSTS=`,
            // `allowed_hosts = []`). Empty is treated exactly like unset because
            // rmcp reads an empty allowlist as allow-ALL hosts — the guard would
            // be silently OFF, worse than the default.
            _ => {
                // Off-loopback the default allowlist is no guard at all: a remote
                // attacker satisfies `["localhost","127.0.0.1"]` just by sending
                // `Host: 127.0.0.1`. Warning and serving anyway left the DNS-rebinding
                // hole open, so refuse to start — explicit hosts are mandatory for a
                // bind reachable from off the box. Loopback keeps the safe default.
                if !bind.ip().is_loopback() {
                    return Err(ConfigError::AllowedHostsRequired(bind.ip()));
                }
                vec!["localhost".into(), "127.0.0.1".into()]
            }
        };

        let public_url = opt(env, "MCP_SSH_PUBLIC_URL", file.public_url)
            .map(|url| normalize_public_url(&url))
            .transpose()?;

        Ok(Self {
            bind,
            user,
            pass,
            inline_timeout: Duration::from_secs(inline_timeout),
            job_dir,
            db_path,
            allowed_hosts,
            public_url,
        })
    }
}

/// Resolve just the SQLite path: env `MCP_SSH_DB`, else the file's `db_path`, else
/// the default. The admin subcommands (`jobs`/`job kill`/`sessions`) touch only the
/// database, so they resolve it this way instead of `Config::load`, which requires
/// auth credentials they don't need. A config file that exists but can't be read
/// or parsed is an error, not a silent fall-through to the default path — that
/// made the admin CLI inspect (or kill in!) a different database than the server.
pub fn db_path() -> Result<PathBuf, ConfigError> {
    db_path_in(&ProcessEnv)
}

fn db_path_in(env: &dyn EnvSource) -> Result<PathBuf, ConfigError> {
    let file = load_file(&config_path(env))?;
    Ok(PathBuf::from(pick(
        env,
        "MCP_SSH_DB",
        file.db_path,
        "/var/lib/mcp-ssh/mcp-ssh.db",
    )))
}

fn load_file(path: &Path) -> Result<FileConfig, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e| parse_error(&s, &e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(ConfigError::Io(path.to_path_buf(), e)),
    }
}

/// The parser's message and line, without the source excerpt `toml`'s `Display`
/// prints: that excerpt is the offending line verbatim, which for a broken
/// `pass = …` line is the password — headed for journald and error tracking.
fn parse_error(source: &str, error: &toml::de::Error) -> ConfigError {
    let line = error.span().map(|span| {
        source[..span.start.min(source.len())]
            .lines()
            .count()
            .max(1)
    });
    let message = error.message().lines().next().unwrap_or("invalid TOML");
    ConfigError::Parse(match line {
        Some(line) => format!("line {line}: {message}"),
        None => message.to_string(),
    })
}

/// `public_url` must be `http(s)://host[:port]` — it becomes the OAuth issuer and
/// every advertised endpoint. A trailing `/` is dropped; a path, query, fragment or
/// userinfo is refused rather than silently producing broken endpoint URLs.
fn normalize_public_url(raw: &str) -> Result<String, ConfigError> {
    let invalid = |why: &str| ConfigError::Invalid("public_url", format!("{raw:?}: {why}"));
    let uri: axum::http::Uri = raw.trim().parse().map_err(|_| invalid("not a URL"))?;
    let scheme = uri.scheme_str().ok_or_else(|| invalid("missing scheme"))?;
    if scheme != "http" && scheme != "https" {
        return Err(invalid("scheme must be http or https"));
    }
    let authority = uri.authority().ok_or_else(|| invalid("missing host"))?;
    if authority.as_str().contains('@') || authority.host().is_empty() {
        return Err(invalid("host must be a bare host[:port]"));
    }
    if !matches!(uri.path(), "" | "/") || uri.query().is_some() || raw.contains('#') {
        return Err(invalid("must not have a path, query or fragment"));
    }
    Ok(format!("{scheme}://{authority}"))
}

fn split_hosts(v: &str) -> Vec<String> {
    normalize_hosts(v.split(',').map(str::to_string))
}

/// Trim and drop empty entries so `""` or `" , "` can't smuggle an empty list
/// (or empty strings) into the allowlist.
fn normalize_hosts(hosts: impl Iterator<Item = String>) -> Vec<String> {
    hosts
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// env, else file value, else default.
fn pick(env: &dyn EnvSource, key: &str, file: Option<String>, default: &str) -> String {
    env.get(key).or(file).unwrap_or_else(|| default.to_string())
}

/// env, else file value; a blank value counts as unset at either level.
fn opt(env: &dyn EnvSource, key: &str, file: Option<String>) -> Option<String> {
    let present = |v: &String| !v.trim().is_empty();
    env.get(key).filter(present).or(file.filter(present))
}

#[cfg(test)]
mod tests;
