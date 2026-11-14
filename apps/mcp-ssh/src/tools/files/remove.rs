//! `delete`: unlink a file or symlink, remove an empty directory, or — only with
//! `recursive=true` — a whole tree. A symlink is never followed.
use tokio::fs;

use super::{FileError, FileOutcome, io_err};

pub async fn delete(path: &str, recursive: bool) -> Result<FileOutcome, FileError> {
    // `link/` resolves *through* the link (POSIX trailing-slash rule): lstat said
    // "directory" and remove_dir_all emptied the target. Strip the slashes so the
    // name we classify and remove is the link itself.
    let target = strip_trailing_slashes(path);
    // symlink_metadata, not metadata: a symlink must be unlinked, never followed —
    // and a dangling one stays deletable.
    let meta = fs::symlink_metadata(target)
        .await
        .map_err(io_err("delete", path))?;
    if !meta.is_dir() {
        fs::remove_file(target)
            .await
            .map_err(io_err("delete", path))?;
    } else if recursive {
        fs::remove_dir_all(target)
            .await
            .map_err(io_err("delete", path))?;
    } else {
        // A whole tree goes only on explicit opt-in; an empty dir needs none.
        match fs::remove_dir(target).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                return Err(FileError::DirectoryNotEmpty {
                    path: path.to_string(),
                });
            }
            Err(e) => return Err(io_err("delete", path)(e)),
        }
    }
    Ok(FileOutcome::Deleted {
        path: path.to_string(),
    })
}

/// `a/b//` → `a/b`; a path made only of slashes stays `/`.
fn strip_trailing_slashes(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() && !path.is_empty() {
        "/"
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::strip_trailing_slashes;

    #[test]
    fn trailing_slashes_are_stripped_but_root_survives() {
        assert_eq!(strip_trailing_slashes("link/"), "link");
        assert_eq!(strip_trailing_slashes("a/b//"), "a/b");
        assert_eq!(strip_trailing_slashes("plain"), "plain");
        assert_eq!(strip_trailing_slashes("//"), "/");
    }
}
