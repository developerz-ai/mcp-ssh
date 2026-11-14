//! Config: a TOML file overlaid by env vars. Auth creds usually come from the
//! file (`mcp-ssh set-auth`); everything has a sane default except the creds.
use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};

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

/// Write `user`/`pass` into the config file, preserving other fields. The file is
/// replaced atomically and is 0600 from its first byte.
pub fn set_auth(user: &str, pass: &Password) -> Result<PathBuf, ConfigError> {
    set_auth_in(&ProcessEnv, user, pass)
}

fn set_auth_in(env: &dyn EnvSource, user: &str, pass: &Password) -> Result<PathBuf, ConfigError> {
    if user.trim().is_empty() || pass.expose().trim().is_empty() {
        return Err(ConfigError::NoCredentials);
    }
    let path = config_path(env);
    let mut file = load_file(&path)?;
    file.user = Some(user.to_string());
    file.pass = Some(pass.clone());

    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&dir).map_err(|e| ConfigError::Io(dir.clone(), e))?;
    let toml = toml::to_string_pretty(&file).map_err(|e| ConfigError::Parse(e.to_string()))?;
    replace_secret_file(&path, &dir, &toml).map_err(|e| ConfigError::Io(path.clone(), e))?;
    Ok(path)
}

/// Write `contents` to a fresh 0600 temp file beside `path`, then rename it over
/// `path`: readers see the old file or the new one, never a truncated mix, and a
/// failed write leaves the old file untouched. Run as root into a directory owned
/// by the service user (`sudo mcp-ssh set-auth`), the file is handed to that
/// owner — otherwise it would be root-owned 0600 and unreadable by the service.
fn replace_secret_file(path: &Path, dir: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let written = opts.open(&tmp).and_then(|mut f| {
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        adopt_dir_owner(&tmp, dir)?;
        std::fs::rename(&tmp, path)
    });
    if written.is_err() {
        // Best effort: the temp holds the new password; don't leave it lying around.
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(unix)]
fn adopt_dir_owner(file: &Path, dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let (file_meta, dir_meta) = (std::fs::metadata(file)?, std::fs::metadata(dir)?);
    match owner_to_adopt(file_meta.uid(), dir_meta.uid(), dir_meta.gid()) {
        Some((uid, gid)) => std::os::unix::fs::chown(file, Some(uid), Some(gid)),
        None => Ok(()),
    }
}
#[cfg(not(unix))]
fn adopt_dir_owner(_file: &Path, _dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The owner a freshly written secret should get. The file was just created, so
/// its uid is ours: only root (uid 0) writing into another user's directory hands
/// it over; anyone else keeps the file they wrote.
fn owner_to_adopt(file_uid: u32, dir_uid: u32, dir_gid: u32) -> Option<(u32, u32)> {
    (file_uid == 0 && dir_uid != 0).then_some((dir_uid, dir_gid))
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
            pass: Some(Password::new("file_pass")),
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
        assert_eq!(cfg.pass.expose(), "env_pass");
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
        let written =
            set_auth_in(&env, "bob", &Password::new("s3cr3t")).expect("set_auth should succeed");
        assert_eq!(written, cfg_path);

        let contents = std::fs::read_to_string(&cfg_path).unwrap();
        let parsed: FileConfig = toml::from_str(&contents).unwrap();
        assert_eq!(parsed.user.as_deref(), Some("bob"));
        assert_eq!(parsed.pass.as_ref().map(Password::expose), Some("s3cr3t"));

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

    #[test]
    fn db_path_matches_server_config_across_sources() {
        // The admin CLI (`mcp-ssh jobs`/`job kill`/`sessions`) resolves the DB path via
        // `db_path_in`, independently of `Config::from_env` (used by `serve`). If the two
        // ever diverge, admin inspects (or kills jobs in!) a different database than the
        // running server — silently, with no error. Pin them to agree across every source:
        // env override, config-file value, and the bare default.
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        // Seed a file-set db_path up front, so case 1 proves env *overrides* a
        // configured value rather than merely filling a gap.
        let file_cfg = FileConfig {
            user: Some("test".into()),
            pass: Some(Password::new("test")),
            db_path: Some("/tmp/file-set.db".into()),
            ..Default::default()
        };
        std::fs::write(&cfg_path, toml::to_string_pretty(&file_cfg).unwrap()).unwrap();

        // 1. Env-set: MCP_SSH_DB wins over everything, the file's db_path included.
        let env = MapEnv::new(&[
            ("MCP_SSH_CONFIG", cfg_path.to_str().unwrap()),
            ("MCP_SSH_DB", "/tmp/env-set.db"),
            ("MCP_SSH_USER", "test"),
            ("MCP_SSH_PASS", "test"),
        ]);
        let admin_path = db_path_in(&env).expect("admin resolve");
        let server_path = Config::from_env(&env).expect("server load").db_path;
        assert_eq!(admin_path, PathBuf::from("/tmp/env-set.db"));
        assert_eq!(
            admin_path, server_path,
            "env-set path must match server config"
        );

        // 2. Config-file value: no env override, db_path comes from the TOML file.
        let env = MapEnv::new(&[("MCP_SSH_CONFIG", cfg_path.to_str().unwrap())]);
        let admin_path = db_path_in(&env).expect("admin resolve");
        let server_path = Config::from_env(&env).expect("server load").db_path;
        assert_eq!(admin_path, PathBuf::from("/tmp/file-set.db"));
        assert_eq!(
            admin_path, server_path,
            "file-set path must match server config"
        );

        // 3. Default: neither env nor file sets a db_path.
        let absent = dir.path().join("absent.toml");
        let env = MapEnv::new(&[
            ("MCP_SSH_CONFIG", absent.to_str().unwrap()),
            ("MCP_SSH_USER", "test"),
            ("MCP_SSH_PASS", "test"),
        ]);
        let admin_path = db_path_in(&env).expect("admin resolve");
        let server_path = Config::from_env(&env).expect("server load").db_path;
        assert_eq!(admin_path, PathBuf::from("/var/lib/mcp-ssh/mcp-ssh.db"));
        assert_eq!(
            admin_path, server_path,
            "default path must match server config"
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
        set_auth_in(&env, "bob", &Password::new("s3cr3t")).expect("set_auth");

        use std::os::unix::fs::MetadataExt;
        let mode = std::fs::metadata(&cfg_path).unwrap().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "a pre-existing world-readable file must be tightened"
        );
    }

    #[test]
    fn non_loopback_bind_without_allowed_hosts_is_rejected() {
        // The default allowlist is worthless off-loopback — a remote attacker just
        // sends `Host: 127.0.0.1`. Refuse to serve rather than warn. Set-but-empty
        // counts as unset (see `empty_allowed_hosts_falls_back_to_loopback_default`).
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("absent.toml");
        for (bind, hosts) in [
            ("0.0.0.0:8080", None),
            ("[::]:8080", None),
            ("192.0.2.10:8080", None),
            ("0.0.0.0:8080", Some("")),
            ("0.0.0.0:8080", Some(" , ")),
        ] {
            let mut vars = vec![
                ("MCP_SSH_CONFIG", cfg_path.to_str().unwrap()),
                ("MCP_SSH_BIND", bind),
                ("MCP_SSH_USER", "test"),
                ("MCP_SSH_PASS", "test"),
            ];
            if let Some(hosts) = hosts {
                vars.push(("MCP_SSH_ALLOWED_HOSTS", hosts));
            }
            let err = Config::from_env(&MapEnv::new(&vars))
                .expect_err("non-loopback bind without explicit hosts must not serve");
            assert!(
                matches!(err, ConfigError::AllowedHostsRequired(_)),
                "bind {bind}, hosts {hosts:?}: expected AllowedHostsRequired, got {err}"
            );
        }

        // Same for an explicit empty list in the config file.
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, "allowed_hosts = []\n").unwrap();
        let env = MapEnv::new(&[
            ("MCP_SSH_CONFIG", cfg_path.to_str().unwrap()),
            ("MCP_SSH_BIND", "0.0.0.0:8080"),
            ("MCP_SSH_USER", "test"),
            ("MCP_SSH_PASS", "test"),
        ]);
        assert!(
            matches!(
                Config::from_env(&env),
                Err(ConfigError::AllowedHostsRequired(_))
            ),
            "an empty file allowlist must not serve off-loopback either"
        );
    }

    #[test]
    fn loopback_bind_without_allowed_hosts_env_loads_silently() {
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");

        // Bind to loopback (v4 and v6) without MCP_SSH_ALLOWED_HOSTS set: the safe
        // default still applies, no error — only off-loopback binds must be explicit.
        for bind in ["127.0.0.1:1337", "[::1]:1337"] {
            let env = MapEnv::new(&[
                ("MCP_SSH_CONFIG", cfg_path.to_str().unwrap()),
                ("MCP_SSH_BIND", bind),
                ("MCP_SSH_USER", "test"),
                ("MCP_SSH_PASS", "test"),
            ]);
            let cfg = Config::from_env(&env).expect("loopback should load");
            assert!(cfg.bind.ip().is_loopback());
            assert_eq!(cfg.allowed_hosts, vec!["localhost", "127.0.0.1"]);
        }
    }

    #[test]
    fn non_loopback_bind_with_explicit_hosts_loads() {
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
        let cfg = Config::from_env(&env).expect("explicit file hosts should load");
        assert_eq!(cfg.allowed_hosts, vec!["mcp.example.com"]);

        // Same via the env var, on a config file that has no hosts at all.
        let env = MapEnv::new(&[
            (
                "MCP_SSH_CONFIG",
                dir.path().join("absent.toml").to_str().unwrap(),
            ),
            ("MCP_SSH_BIND", "0.0.0.0:8080"),
            ("MCP_SSH_ALLOWED_HOSTS", "mcp.example.com"),
            ("MCP_SSH_USER", "test"),
            ("MCP_SSH_PASS", "test"),
        ]);
        let cfg = Config::from_env(&env).expect("explicit env hosts should load");
        assert_eq!(cfg.allowed_hosts, vec!["mcp.example.com"]);
    }

    fn creds_env(cfg_path: &Path, extra: &[(&'static str, &'static str)]) -> MapEnv {
        let mut vars = vec![("MCP_SSH_CONFIG", cfg_path.to_str().unwrap())];
        vars.extend_from_slice(extra);
        MapEnv::new(&vars)
    }

    #[test]
    fn a_blank_credential_never_loads() {
        let dir = tempdir().unwrap();
        let absent = dir.path().join("absent.toml");
        for (user, pass) in [("", "p"), ("u", ""), ("  ", "p"), ("u", " \t")] {
            let env = creds_env(&absent, &[("MCP_SSH_USER", user), ("MCP_SSH_PASS", pass)]);
            assert!(
                matches!(Config::from_env(&env), Err(ConfigError::NoCredentials)),
                "user {user:?} pass {pass:?}"
            );
        }
    }

    #[test]
    fn an_empty_env_password_does_not_shadow_the_file() {
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, "user = \"bob\"\npass = \"strong\"\n").unwrap();
        let env = creds_env(&cfg_path, &[("MCP_SSH_USER", ""), ("MCP_SSH_PASS", "")]);
        let cfg = Config::from_env(&env).expect("file creds apply");
        assert_eq!(cfg.user, "bob");
        assert_eq!(cfg.pass.expose(), "strong");
    }

    #[test]
    fn debug_output_never_shows_the_password() {
        let dir = tempdir().unwrap();
        let env = creds_env(
            &dir.path().join("absent.toml"),
            &[("MCP_SSH_USER", "u"), ("MCP_SSH_PASS", "hunter2-secret")],
        );
        let cfg = Config::from_env(&env).unwrap();
        assert!(!format!("{cfg:?}").contains("hunter2-secret"));
        let file = FileConfig {
            pass: Some(Password::new("hunter2-secret")),
            ..Default::default()
        };
        assert!(!format!("{file:?}").contains("hunter2-secret"));
    }

    #[test]
    fn a_broken_pass_line_is_reported_without_its_value() {
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        for line in [
            "pass = hunter2-secret",
            "pass = \"hunter2-secret",
            "pass = 'hunter2-secret",
        ] {
            std::fs::write(&cfg_path, format!("user = \"u\"\n{line}\n")).unwrap();
            let err = Config::from_env(&creds_env(&cfg_path, &[])).unwrap_err();
            let shown = err.to_string();
            assert!(matches!(err, ConfigError::Parse(_)), "{shown}");
            assert!(!shown.contains("hunter2"), "{shown}");
            assert!(shown.contains("line 2"), "{shown}");
        }
    }

    #[test]
    fn public_url_is_validated_and_normalised() {
        for (raw, want) in [
            ("https://mcp.example.com", "https://mcp.example.com"),
            ("https://mcp.example.com/", "https://mcp.example.com"),
            ("http://127.0.0.1:1337", "http://127.0.0.1:1337"),
        ] {
            assert_eq!(normalize_public_url(raw).unwrap(), want);
        }
        for bad in [
            "mcp.example.com",
            "javascript://x",
            "ftp://mcp.example.com",
            "https://mcp.example.com/mcp",
            "https://mcp.example.com?x=1",
            "https://mcp.example.com#f",
            "https://user:pw@mcp.example.com",
            "https://",
        ] {
            assert!(
                matches!(
                    normalize_public_url(bad),
                    Err(ConfigError::Invalid("public_url", _))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn inline_timeout_is_clamped_to_a_minute() {
        let dir = tempdir().unwrap();
        let env = creds_env(
            &dir.path().join("absent.toml"),
            &[
                ("MCP_SSH_USER", "u"),
                ("MCP_SSH_PASS", "p"),
                ("MCP_SSH_INLINE_TIMEOUT_SECS", "3600"),
            ],
        );
        let cfg = Config::from_env(&env).unwrap();
        assert_eq!(
            cfg.inline_timeout,
            Duration::from_secs(MAX_INLINE_TIMEOUT_SECS)
        );
    }

    #[test]
    fn set_auth_refuses_blank_credentials() {
        let dir = tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let env = creds_env(&cfg_path, &[]);
        assert!(set_auth_in(&env, " ", &Password::new("p")).is_err());
        assert!(set_auth_in(&env, "u", &Password::new("")).is_err());
        assert!(!cfg_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_set_auth_leaves_the_old_file_intact_and_no_temp_behind() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let conf_dir = dir.path().join("etc");
        std::fs::create_dir(&conf_dir).unwrap();
        let cfg_path = conf_dir.join("config.toml");
        let env = creds_env(&cfg_path, &[]);
        set_auth_in(&env, "old", &Password::new("old-pass")).unwrap();
        let before = std::fs::read_to_string(&cfg_path).unwrap();

        // A read-only directory: the temp file can't be created, so nothing changes.
        std::fs::set_permissions(&conf_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let probe = conf_dir.join("probe");
        if std::fs::write(&probe, b"").is_ok() {
            // Running as root: permissions don't bind, the scenario can't be staged.
            std::fs::set_permissions(&conf_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let result = set_auth_in(&env, "new", &Password::new("new-pass"));
        std::fs::set_permissions(&conf_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), before);
        let leftovers: Vec<_> = std::fs::read_dir(&conf_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, [std::ffi::OsString::from("config.toml")]);
    }

    #[test]
    fn only_root_hands_the_secret_to_the_directory_owner() {
        // root writing into the service user's dir → chown to that user.
        assert_eq!(owner_to_adopt(0, 999, 998), Some((999, 998)));
        // root writing into root's dir → nothing to do.
        assert_eq!(owner_to_adopt(0, 0, 0), None);
        // a regular user keeps the file they wrote, whoever owns the dir.
        assert_eq!(owner_to_adopt(1000, 999, 998), None);
        assert_eq!(owner_to_adopt(1000, 1000, 1000), None);
    }
}
