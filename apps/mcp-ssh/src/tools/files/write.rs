//! `write` (atomic replace) and `append`.
use std::path::{Path, PathBuf};

use tokio::{fs, io::AsyncWriteExt};

use super::{FileError, FileOutcome, ensure_parent, io_err};

/// Symlink hops `write` follows before giving up — the kernel's own ELOOP limit.
const MAX_SYMLINK_HOPS: usize = 40;

/// Replace `path`'s content atomically: write a temp sibling, fsync, rename over.
/// Truncate-then-write left an empty `.env` behind on ENOSPC or a crash; now the
/// old content stays until the new is fully on disk. An existing file keeps its
/// mode, and an existing symlink keeps pointing at its target, whose content is
/// what changes. (A rename gives the path a new inode: hard links to the old one
/// keep the old content — the accepted cost of atomicity.)
pub async fn write(path: &str, content: &str) -> Result<FileOutcome, FileError> {
    ensure_parent(path).await?;
    let target = follow_symlinks(Path::new(path))
        .await
        .map_err(io_err("write", path))?;
    let mode = match fs::metadata(&target).await {
        Ok(meta) => Some(meta.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io_err("write", path)(e)),
    };
    let (tmp, mut file) = temp_sibling(&target, mode.is_some())
        .await
        .map_err(io_err("write", path))?;
    let written = async {
        file.write_all(content.as_bytes()).await?;
        file.sync_all().await?;
        if let Some(mode) = mode {
            fs::set_permissions(&tmp, mode).await?;
        }
        fs::rename(&tmp, &target).await
    }
    .await;
    if let Err(e) = written {
        // Best effort: the error being reported is the write's, not the cleanup's.
        let _ = fs::remove_file(&tmp).await;
        return Err(io_err("write", path)(e));
    }
    Ok(FileOutcome::Wrote {
        path: path.to_string(),
        bytes: content.len(),
    })
}

pub async fn append(path: &str, content: &str) -> Result<FileOutcome, FileError> {
    ensure_parent(path).await?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(io_err("append to", path))?;
    f.write_all(content.as_bytes())
        .await
        .map_err(io_err("append to", path))?;
    // Flush before returning so a follow-up read is guaranteed to see the bytes.
    f.flush().await.map_err(io_err("append to", path))?;
    Ok(FileOutcome::Appended {
        path: path.to_string(),
        bytes: content.len(),
    })
}

/// Resolve a chain of symlinks at `path` to the path they finally name — which
/// may not exist yet (a dangling link is written *through*, as `fs::write` did).
/// A non-link or missing path resolves to itself.
async fn follow_symlinks(path: &Path) -> std::io::Result<PathBuf> {
    let mut p = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        match fs::symlink_metadata(&p).await {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = fs::read_link(&p).await?;
                // `join` keeps an absolute link as-is and anchors a relative one
                // at the link's own directory, as the kernel does.
                p = match p.parent() {
                    Some(dir) => dir.join(link),
                    None => link,
                };
            }
            _ => return Ok(p),
        }
    }
    Err(std::io::Error::from_raw_os_error(libc::ELOOP))
}

/// Create a fresh, exclusively-owned temp file next to `target` (same directory,
/// so the final rename stays on one filesystem and is atomic). `private`: start
/// it `0600` because the caller will chmod it to a preserved mode; otherwise it
/// gets the default mode (umask applies), as a newly created file would.
pub(super) async fn temp_sibling(
    target: &Path,
    private: bool,
) -> std::io::Result<(PathBuf, fs::File)> {
    let dir = target.parent().unwrap_or(Path::new(""));
    let name = target
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?
        .to_string_lossy();
    loop {
        let tmp = dir.join(format!(
            ".{name}.mcp-ssh-{:016x}.tmp",
            rand::random::<u64>()
        ));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        if private {
            opts.mode(0o600);
        }
        match opts.open(&tmp).await {
            Ok(file) => return Ok((tmp, file)),
            // A 64-bit random collision: just draw again.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}
