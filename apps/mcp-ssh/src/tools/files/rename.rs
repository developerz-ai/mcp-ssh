//! `move`: rename `src` to `dest`, never clobbering, never leaving stray
//! directories from a move that was always going to fail, and falling back to
//! copy+remove for a regular file across filesystems.
use std::ffi::CString;
use std::fs::Metadata;
use std::io::{Error, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use tokio::fs;

use super::write::temp_sibling;
use super::{FileError, FileOutcome, ensure_parent, io_err};

pub async fn rename(src: &str, dest: &str) -> Result<FileOutcome, FileError> {
    // Every check that can fail runs before `ensure_parent`, so a doomed move
    // (missing source, dir into itself, occupied dest) creates nothing.
    let src_meta = match fs::symlink_metadata(src).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(FileError::SourceMissing {
                src: src.to_string(),
            });
        }
        Err(e) => return Err(io_err("move", src)(e)),
    };
    if src_meta.is_dir() && dest_inside(Path::new(src), Path::new(dest)).await {
        return Err(FileError::MoveIntoItself {
            src: src.to_string(),
            dest: dest.to_string(),
        });
    }
    // symlink_metadata (don't follow): a symlink at `dest` occupies the path too.
    match fs::symlink_metadata(dest).await {
        Ok(meta) if meta.is_dir() => {
            return Err(FileError::DestinationIsDirectory {
                src: src.to_string(),
                dest: dest.to_string(),
            });
        }
        Ok(_) => return Err(destination_exists(dest)),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(io_err("move to", dest)(e)),
    }
    ensure_parent(dest).await?;
    match rename_noreplace(src.into(), dest.into()).await {
        Ok(()) => {}
        // The pre-check above is advisory; this is the race-free refusal.
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => return Err(destination_exists(dest)),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            copy_across(src, dest, &src_meta).await?;
        }
        Err(e) => return Err(io_err("move", src)(e)),
    }
    Ok(FileOutcome::Moved {
        src: src.to_string(),
        dest: dest.to_string(),
    })
}

fn destination_exists(dest: &str) -> FileError {
    FileError::DestinationExists {
        dest: dest.to_string(),
    }
}

/// Cross-filesystem move of a regular file: copy into a temp sibling of `dest`
/// (same filesystem as `dest`), fsync, no-replace rename into place, then remove
/// `src`. A directory or symlink is refused typed — copying a tree (or a link's
/// target) is `mv`'s job, not this tool's.
async fn copy_across(src: &str, dest: &str, src_meta: &Metadata) -> Result<(), FileError> {
    if !src_meta.is_file() {
        return Err(FileError::CrossDeviceUnsupported {
            src: src.to_string(),
            dest: dest.to_string(),
        });
    }
    let (tmp, mut out) = temp_sibling(Path::new(dest), true)
        .await
        .map_err(io_err("move to", dest))?;
    let copied = async {
        let mut input = fs::File::open(src).await?;
        tokio::io::copy(&mut input, &mut out).await?;
        out.sync_all().await?;
        fs::set_permissions(&tmp, src_meta.permissions()).await?;
        rename_noreplace(tmp.clone(), dest.into()).await
    }
    .await;
    if let Err(e) = copied {
        // Best effort: the error being reported is the copy's, not the cleanup's.
        let _ = fs::remove_file(&tmp).await;
        return Err(match e.raw_os_error() {
            Some(libc::EEXIST) => destination_exists(dest),
            _ => io_err("move", src)(e),
        });
    }
    // `dest` now holds the full copy; a failure here leaves both — reported, so
    // the agent knows `src` still needs removing.
    fs::remove_file(src)
        .await
        .map_err(io_err("remove the moved source", src))
}

/// Whether `dest` would land at or under the directory `src` (`mv a a/b`). The
/// kernel refuses that too, but only after `ensure_parent` has created `a/b`'s
/// parents. `dest` may not exist yet: resolve its deepest existing ancestor and
/// re-append the rest.
async fn dest_inside(src: &Path, dest: &Path) -> bool {
    let Ok(src) = fs::canonicalize(src).await else {
        return false;
    };
    for ancestor in dest.ancestors() {
        let probe = if ancestor.as_os_str().is_empty() {
            Path::new(".")
        } else {
            ancestor
        };
        if let Ok(base) = fs::canonicalize(probe).await {
            let rest = dest.strip_prefix(ancestor).unwrap_or(dest);
            return base.join(rest).starts_with(&src);
        }
    }
    false
}

/// `renameat2(RENAME_NOREPLACE)`: the kernel refuses with `EEXIST` if `dest`
/// exists, atomically — no check-then-rename window. A filesystem without the
/// flag (`EINVAL`) or a kernel/seccomp without the syscall (`ENOSYS`) falls back
/// to check-then-rename: the race returns only there.
async fn rename_noreplace(src: PathBuf, dest: PathBuf) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        let s = c_path(&src)?;
        let d = c_path(&dest)?;
        // SAFETY: `s` and `d` are valid NUL-terminated strings that outlive the
        // call; AT_FDCWD makes relative paths resolve against the cwd, as rename(2).
        let rc = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                s.as_ptr(),
                libc::AT_FDCWD,
                d.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let err = Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINVAL | libc::ENOSYS) => {
                if std::fs::symlink_metadata(&dest).is_ok() {
                    return Err(Error::from_raw_os_error(libc::EEXIST));
                }
                std::fs::rename(&src, &dest)
            }
            _ => Err(err),
        }
    })
    .await
    .map_err(Error::other)?
}

fn c_path(p: &Path) -> std::io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| Error::from(ErrorKind::InvalidInput))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noreplace_refuses_an_existing_dest_with_eexist() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, "A").unwrap();
        std::fs::write(&b, "B").unwrap();
        let err = rename_noreplace(a.clone(), b.clone()).await.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EEXIST));
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "B", "dest untouched");
        assert!(a.exists(), "src untouched");
    }

    /// The EXDEV branch: a cross-device move of a regular file copies, preserves
    /// the mode, and removes the source. Driven directly — a test box rarely
    /// offers two filesystems the test user can write to.
    #[tokio::test]
    async fn copy_across_moves_a_regular_file_and_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (src, dest) = (dir.path().join("src"), dir.path().join("dest"));
        std::fs::write(&src, "payload").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640)).unwrap();
        let meta = std::fs::symlink_metadata(&src).unwrap();
        let (s, d) = (src.to_str().unwrap(), dest.to_str().unwrap());

        copy_across(s, d, &meta).await.unwrap();
        assert!(!src.exists(), "source removed after the copy");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "payload");
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "mode preserved");
        let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 1, "no temp file left behind");
    }

    #[tokio::test]
    async fn copy_across_refuses_a_directory_typed() {
        let dir = tempfile::tempdir().unwrap();
        let meta = std::fs::symlink_metadata(dir.path()).unwrap();
        let dest = dir.path().join("elsewhere");
        let err = copy_across(dir.path().to_str().unwrap(), dest.to_str().unwrap(), &meta)
            .await
            .unwrap_err();
        assert!(
            matches!(err, FileError::CrossDeviceUnsupported { .. }),
            "{err:?}"
        );
        assert!(!dest.exists());
    }
}
