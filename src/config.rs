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

        let bind = pick(env, "MCP_SSH_BIND", file.bind, "127.0.0.1:1337")
            .parse()
            .map_err(|e| ConfigError::Invalid("bind", format!("{e}")))?;

        let user = opt(env, "MCP_SSH_USER", file.user).ok_or(ConfigError::NoCredentials)?;
        let pass = opt(env, "MCP_SSH_PASS", file.pass).ok_or(ConfigError::NoCredentials)?;

        let inline_timeout = match env.get("MCP_SSH_INLINE_TIMEOUT_SECS") {
            Some(v) => v
                .parse()
                .map_err(|e| ConfigError::Invalid("inline_timeout", format!("{e}")))?,
            None => file.inline_timeout_secs.unwrap_or(2),
        };

        let job_dir = PathBuf::from(pick(
            env,
            "MCP_SSH_JOB_DIR",
            file.job_dir,
            "/var/lib/mcp-ssh/jobs",
        ));

        let allowed_hosts = match env.get("MCP_SSH_ALLOWED_HOSTS") {
            Some(v) => split_hosts(&v),
            None => file
                .allowed_hosts
                .unwrap_or_else(|| vec!["localhost".into(), "127.0.0.1".into()]),
        };

        let public_url = opt(env, "MCP_SSH_PUBLIC_URL", file.public_url);

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
    set_auth_in(&ProcessEnv, user, pass)
}

fn set_auth_in(env: &dyn EnvSource, user: &str, pass: &str) -> Result<PathBuf, ConfigError> {
    let path = config_path(env);
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
fn pick(env: &dyn EnvSource, key: &str, file: Option<String>, default: &str) -> String {
    env.get(key).or(file).unwrap_or_else(|| default.to_string())
}

/// env, else file value.
fn opt(env: &dyn EnvSource, key: &str, file: Option<String>) -> Option<String> {
    env.get(key).or(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::tempdir;

    /// In-memory `EnvSource`: tests inject vars without touching the process env,
    /// so they need no `unsafe`, no global lock, and run in parallel safely.
    struct MapEnv(HashMap<String, String>);

    impl MapEnv {
        fn new(vars: &[(&str, &str)]) -> Self {
            Self(
                vars.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        }
    }

    impl EnvSource for MapEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    #[test]
    fn env_overrides_file() {
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");

        let file_cfg = FileConfig {
            user: Some("file_user".into()),
            pass: Some("file_pass".into()),
            ..Default::default()
        };
        std::fs::write(&cfg_path, toml::to_string_pretty(&file_cfg).unwrap()).unwrap();

        let env = MapEnv::new(&[
            ("MCP_SSH_CONFIG", cfg_path.to_str().unwrap()),
            ("MCP_SSH_USER", "env_user"),
            ("MCP_SSH_PASS", "env_pass"),
        ]);
        let cfg = Config::from_env(&env).expect("load should succeed");
        assert_eq!(cfg.user, "env_user");
        assert_eq!(cfg.pass, "env_pass");
    }

    #[test]
    fn missing_creds_returns_no_credentials() {
        let dir = tempdir().unwrap();
        // Point at a non-existent file so load_file returns FileConfig::default().
        let cfg_path = dir.path().join("absent.toml");

        // No USER/PASS keys present → treated as unset.
        let env = MapEnv::new(&[("MCP_SSH_CONFIG", cfg_path.to_str().unwrap())]);
        let err = Config::from_env(&env).expect_err("should fail without credentials");
        assert!(
            matches!(err, ConfigError::NoCredentials),
            "expected NoCredentials, got {err}"
        );
    }

    #[test]
    fn set_auth_writes_toml_and_chmod_600() {
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");

        let env = MapEnv::new(&[("MCP_SSH_CONFIG", cfg_path.to_str().unwrap())]);
        let written = set_auth_in(&env, "bob", "s3cr3t").expect("set_auth should succeed");
        assert_eq!(written, cfg_path);

        let contents = std::fs::read_to_string(&cfg_path).unwrap();
        let parsed: FileConfig = toml::from_str(&contents).unwrap();
        assert_eq!(parsed.user.as_deref(), Some("bob"));
        assert_eq!(parsed.pass.as_deref(), Some("s3cr3t"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = std::fs::metadata(&cfg_path).unwrap().mode();
            assert_eq!(mode & 0o777, 0o600, "expected mode 0o600, got {mode:o}");
        }
    }
}
