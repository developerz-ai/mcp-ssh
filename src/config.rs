//! Config: a TOML file overlaid by env vars. Auth creds usually come from the
//! file (`mcp-ssh set-auth`); everything has a sane default except the creds.
use std::{net::SocketAddr, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub user: String,
    pub pass: String,
    /// Commands finishing within this window return inline; slower ones background.
    pub inline_timeout: Duration,
    /// Where per-job log files live.
    pub job_dir: PathBuf,
    /// SQLite database file (OAuth tokens + job metadata + output tail).
    pub db_path: PathBuf,
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
    pub db_path: Option<String>,
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

        let bind: SocketAddr = pick(env, "MCP_SSH_BIND", file.bind, "127.0.0.1:1337")
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
                if !bind.ip().is_loopback() {
                    warn!(
                        "MCP_SSH_ALLOWED_HOSTS unset or empty and bind is non-loopback ({}) — \
                         DNS-rebinding attacks possible. Set MCP_SSH_ALLOWED_HOSTS explicitly.",
                        bind.ip()
                    );
                }
                vec!["localhost".into(), "127.0.0.1".into()]
            }
        };

        let public_url = opt(env, "MCP_SSH_PUBLIC_URL", file.public_url);

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
    write_secret_file(&path, &toml).map_err(|e| ConfigError::Io(path.clone(), e))?;
    // The 0600 in `write_secret_file` applies only when the file is created; a
    // pre-existing file keeps its old mode, so tighten it — and a failure here
    // must be loud, not a plaintext password left world-readable forever.
    chmod_600(&path).map_err(|e| ConfigError::Io(path.clone(), e))?;
    Ok(path)
}

/// Write `contents` with the file created 0600 from the first byte. Plain
/// `fs::write` created it umask-default (typically 0644) and only chmodded
/// afterwards — a window where any local user could read the plaintext password.
fn write_secret_file(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(contents.as_bytes())
}

fn load_file(path: &std::path::Path) -> Result<FileConfig, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e| ConfigError::Parse(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(ConfigError::Io(path.to_path_buf(), e)),
    }
}

#[cfg(unix)]
fn chmod_600(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}
#[cfg(not(unix))]
fn chmod_600(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
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

    /// A `MakeWriter` collecting subscriber output into a shared buffer so a test
    /// can assert whether a given log line was (or was not) emitted.
    #[derive(Clone, Default)]
    struct BufWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut guard) = self.0.lock() {
                guard.extend_from_slice(buf);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
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

    #[test]
    fn empty_allowed_hosts_falls_back_to_loopback_default() {
        // rmcp treats an empty allowlist as allow-ALL hosts, so set-but-empty
        // (`MCP_SSH_ALLOWED_HOSTS=`, whitespace/commas, or `allowed_hosts = []`
        // in the file) must behave exactly like unset — loopback default, never
        // an empty vec.
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        for empty in ["", " , "] {
            let env = MapEnv::new(&[
                ("MCP_SSH_CONFIG", cfg_path.to_str().unwrap()),
                ("MCP_SSH_ALLOWED_HOSTS", empty),
                ("MCP_SSH_USER", "test"),
                ("MCP_SSH_PASS", "test"),
            ]);
            let cfg = Config::from_env(&env).expect("load");
            assert_eq!(
                cfg.allowed_hosts,
                vec!["localhost", "127.0.0.1"],
                "env {empty:?} must not disable the host guard"
            );
        }

        // Same for an explicit empty list in the file.
        std::fs::write(&cfg_path, "allowed_hosts = []\n").unwrap();
        let env = MapEnv::new(&[
            ("MCP_SSH_CONFIG", cfg_path.to_str().unwrap()),
            ("MCP_SSH_USER", "test"),
            ("MCP_SSH_PASS", "test"),
        ]);
        let cfg = Config::from_env(&env).expect("load");
        assert_eq!(cfg.allowed_hosts, vec!["localhost", "127.0.0.1"]);
    }

    #[test]
    fn db_path_propagates_a_broken_config_file() {
        // A config file that exists but doesn't parse must be an error — the old
        // unwrap_or_default() sent admin commands to the DEFAULT db path while
        // the server (whose Config::load errors loudly) used the configured one.
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, "this is { not toml").unwrap();
        let env = MapEnv::new(&[("MCP_SSH_CONFIG", cfg_path.to_str().unwrap())]);
        assert!(
            matches!(db_path_in(&env), Err(ConfigError::Parse(_))),
            "broken config must not silently resolve to the default db path"
        );

        // A missing file is still fine (defaults apply).
        let env = MapEnv::new(&[(
            "MCP_SSH_CONFIG",
            dir.path().join("absent.toml").to_str().unwrap(),
        )]);
        assert_eq!(
            db_path_in(&env).unwrap(),
            PathBuf::from("/var/lib/mcp-ssh/mcp-ssh.db")
        );
    }

    #[cfg(unix)]
    #[test]
    fn set_auth_tightens_a_preexisting_loose_config_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, "").unwrap();
        std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let env = MapEnv::new(&[("MCP_SSH_CONFIG", cfg_path.to_str().unwrap())]);
        set_auth_in(&env, "bob", "s3cr3t").expect("set_auth");

        use std::os::unix::fs::MetadataExt;
        let mode = std::fs::metadata(&cfg_path).unwrap().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "a pre-existing world-readable file must be tightened"
        );
    }

    #[test]
    fn non_loopback_bind_without_allowed_hosts_env_loads() {
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");

        // Bind to non-loopback (0.0.0.0) without MCP_SSH_ALLOWED_HOSTS set.
        // Should load successfully; warning issued at runtime.
        let env = MapEnv::new(&[
            ("MCP_SSH_CONFIG", cfg_path.to_str().unwrap()),
            ("MCP_SSH_BIND", "0.0.0.0:8080"),
            ("MCP_SSH_USER", "test"),
            ("MCP_SSH_PASS", "test"),
        ]);
        let cfg = Config::from_env(&env).expect("should load with warning");
        assert_eq!(cfg.bind.ip().to_string(), "0.0.0.0");
        // Default allowed_hosts used (localhost/127.0.0.1).
        assert_eq!(cfg.allowed_hosts, vec!["localhost", "127.0.0.1"]);
    }

    #[test]
    fn loopback_bind_without_allowed_hosts_env_loads_silently() {
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");

        // Bind to loopback (127.0.0.1) without MCP_SSH_ALLOWED_HOSTS set.
        // Should load without warning.
        let env = MapEnv::new(&[
            ("MCP_SSH_CONFIG", cfg_path.to_str().unwrap()),
            ("MCP_SSH_BIND", "127.0.0.1:1337"),
            ("MCP_SSH_USER", "test"),
            ("MCP_SSH_PASS", "test"),
        ]);
        let cfg = Config::from_env(&env).expect("should load");
        assert_eq!(cfg.bind.ip().to_string(), "127.0.0.1");
        assert_eq!(cfg.allowed_hosts, vec!["localhost", "127.0.0.1"]);
    }

    #[test]
    fn non_loopback_bind_with_explicit_file_hosts_loads_without_warning() {
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");

        // Explicit allowed_hosts in config.toml; non-loopback bind; env unset.
        let file_cfg = FileConfig {
            allowed_hosts: Some(vec!["mcp.example.com".into()]),
            ..Default::default()
        };
        std::fs::write(&cfg_path, toml::to_string_pretty(&file_cfg).unwrap()).unwrap();

        let env = MapEnv::new(&[
            ("MCP_SSH_CONFIG", cfg_path.to_str().unwrap()),
            ("MCP_SSH_BIND", "0.0.0.0:8080"),
            ("MCP_SSH_USER", "test"),
            ("MCP_SSH_PASS", "test"),
        ]);

        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let cfg =
            tracing::subscriber::with_default(subscriber, || Config::from_env(&env).expect("load"));

        // Explicit file hosts win; the fallback (and its warning) is never reached.
        assert_eq!(cfg.allowed_hosts, vec!["mcp.example.com"]);
        let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            !logs.contains("DNS-rebinding"),
            "no DNS-rebinding warning expected with explicit file hosts: {logs}"
        );
    }
}
