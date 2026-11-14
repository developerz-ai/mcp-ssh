//! `mcp-ssh set-auth`: write the login into the config file, atomically and 0600.
use std::path::{Path, PathBuf};

use super::{ConfigError, EnvSource, Password, ProcessEnv, config_path, load_file};

/// Write `user`/`pass` into the config file, preserving other fields. The file is
/// replaced atomically and is 0600 from its first byte.
pub fn set_auth(user: &str, pass: &Password) -> Result<PathBuf, ConfigError> {
    set_auth_in(&ProcessEnv, user, pass)
}

pub(super) fn set_auth_in(
    env: &dyn EnvSource,
    user: &str,
    pass: &Password,
) -> Result<PathBuf, ConfigError> {
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
pub(super) fn owner_to_adopt(file_uid: u32, dir_uid: u32, dir_gid: u32) -> Option<(u32, u32)> {
    (file_uid == 0 && dir_uid != 0).then_some((dir_uid, dir_gid))
}
