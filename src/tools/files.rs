//! File operations, executed locally as the service user. Read/write/move go
//! through `tokio::fs`; list/grep shell out to `ls`/`find`/`grep` rather than
//! reimplementing them.
use tokio::{fs, io::AsyncWriteExt};

/// Largest shell-listing (`ls`/`find`/`grep`) output returned to the agent. Unlike
/// `read`, these aren't line-cursored, so a `find /` or `grep -r` on a huge tree
/// would otherwise dump unbounded text into context. Truncate with a marker that
/// tells the agent to narrow the path/pattern.
const MAX_SHELL_OUTPUT_BYTES: usize = 64 * 1024;

/// Read a file, paginated by line AND bounded by bytes (via the shared job-log
/// paginator) so neither a huge file nor a single pathological line can flood the
/// agent context.
pub async fn read(path: &str, cursor: usize, limit: usize) -> Result<String, String> {
    // A directory can't be read as text: surface a useful redirect to `list`
    // instead of a raw "Is a directory" errno.
    match fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => {
            return Err(format!(
                "{path} is a directory — use file(action=\"list\", path=\"{path}\") instead"
            ));
        }
        Ok(_) => {}
        Err(e) => return Err(e.to_string()),
    }
    // Binary-safe: replace non-UTF-8 bytes with U+FFFD rather than hard-erroring.
    let bytes = fs::read(path).await.map_err(|e| e.to_string())?;
    let owned = String::from_utf8_lossy(&bytes).into_owned();
    let lines: Vec<&str> = owned.lines().collect();
    // Forward pagination from the top (cursor 0 = first line): a file is read
    // start-to-end. (`job poll` instead reads newest-first — a live log's latest
    // output matters most.) Same byte/line ceilings via `paginate`.
    let page = crate::jobs::paginate(&lines, cursor, limit);
    let body = page.lines.join("\n");
    if page.has_more {
        Ok(format!(
            "{body}\n[lines {cursor}..{} of {}; next_cursor={}]",
            page.next_cursor, page.total_lines, page.next_cursor
        ))
    } else {
        Ok(body)
    }
}

pub async fn write(path: &str, content: &str) -> Result<String, String> {
    ensure_parent(path).await?;
    fs::write(path, content).await.map_err(|e| e.to_string())?;
    Ok(format!("wrote {} bytes to {path}", content.len()))
}

pub async fn append(path: &str, content: &str) -> Result<String, String> {
    ensure_parent(path).await?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|e| e.to_string())?;
    f.write_all(content.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    // Flush before returning so a follow-up read is guaranteed to see the bytes.
    f.flush().await.map_err(|e| e.to_string())?;
    Ok(format!("appended {} bytes to {path}", content.len()))
}

pub async fn delete(path: &str) -> Result<String, String> {
    let meta = fs::metadata(path).await.map_err(|e| e.to_string())?;
    let r = if meta.is_dir() {
        fs::remove_dir_all(path).await
    } else {
        fs::remove_file(path).await
    };
    r.map_err(|e| e.to_string())?;
    Ok(format!("deleted {path}"))
}

pub async fn rename(src: &str, dest: &str) -> Result<String, String> {
    ensure_parent(dest).await?;
    fs::rename(src, dest).await.map_err(|e| e.to_string())?;
    Ok(format!("moved {src} -> {dest}"))
}

/// Create the target's parent directories so writing a new file under a fresh
/// path "just works" (like `mkdir -p` before a redirect), instead of failing with
/// a bare `ENOENT` the agent then has to diagnose. A no-op when the parent already
/// exists or the path has none (a bare filename in the cwd).
async fn ensure_parent(path: &str) -> Result<(), String> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub async fn list(path: &str, recursive: bool) -> Result<String, String> {
    if recursive {
        sh("find", &[path]).await
    } else {
        sh("ls", &["-la", path]).await
    }
}

pub async fn grep(pattern: &str, path: &str, recursive: bool) -> Result<String, String> {
    let flag = if recursive { "-rn" } else { "-n" };
    sh("grep", &[flag, pattern, path]).await
}

async fn sh(prog: &str, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new(prog)
        .args(args)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.stderr.is_empty() {
        s.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    Ok(cap_bytes(s, MAX_SHELL_OUTPUT_BYTES))
}

/// Bound a non-paginated listing to `max` bytes, cut on a UTF-8 boundary, with a
/// marker telling the agent to narrow the path/pattern. A no-op under the cap.
fn cap_bytes(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = s.len() - end;
    s.truncate(end);
    s.push_str(&format!(
        "\n[output truncated: +{dropped} bytes — narrow the path or pattern]"
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_read_paginate_append_move_delete() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let a = a.to_str().unwrap();

        write(a, "l1\nl2\nl3").await.unwrap();
        let page = read(a, 0, 2).await.unwrap();
        assert!(page.contains("l1") && page.contains("l2") && !page.contains("l3"));
        assert!(page.contains("next_cursor=2"));

        append(a, "\nl4").await.unwrap();
        assert!(read(a, 0, 100).await.unwrap().contains("l4"));

        let b = dir.path().join("b.txt");
        let b = b.to_str().unwrap();
        rename(a, b).await.unwrap();
        assert!(read(a, 0, 10).await.is_err());

        delete(b).await.unwrap();
        assert!(read(b, 0, 10).await.is_err());
    }

    #[tokio::test]
    async fn write_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        // Two levels that don't exist yet — write must `mkdir -p` them.
        let nested = dir.path().join("a/b/c.txt");
        let nested = nested.to_str().unwrap();
        write(nested, "hi").await.unwrap();
        assert_eq!(read(nested, 0, 10).await.unwrap(), "hi");

        // append to a fresh path under a new dir works too.
        let ap = dir.path().join("x/y/z.log");
        let ap = ap.to_str().unwrap();
        append(ap, "one\n").await.unwrap();
        assert!(read(ap, 0, 10).await.unwrap().contains("one"));
    }

    #[tokio::test]
    async fn read_on_directory_redirects_to_list() {
        let dir = tempfile::tempdir().unwrap();
        let err = read(dir.path().to_str().unwrap(), 0, 10).await.unwrap_err();
        assert!(
            err.contains("is a directory") && err.contains("list"),
            "dir read should redirect to list: {err}"
        );
    }

    #[tokio::test]
    async fn grep_finds_match() {
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path().join("c.txt");
        let c = c.to_str().unwrap();
        write(c, "alpha\nbeta\ngamma").await.unwrap();
        assert!(grep("beta", c, false).await.unwrap().contains("beta"));
    }

    #[tokio::test]
    async fn list_non_recursive_shows_top_level_only() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        tokio::fs::create_dir(&sub).await.unwrap();
        let f_top = dir.path().join("top.txt");
        let f_nested = sub.join("nested.txt");
        write(f_top.to_str().unwrap(), "top").await.unwrap();
        write(f_nested.to_str().unwrap(), "nested").await.unwrap();

        let out = list(dir.path().to_str().unwrap(), false).await.unwrap();
        assert!(out.contains("top.txt"), "should list top-level file: {out}");
        assert!(out.contains("sub"), "should list sub dir: {out}");
        assert!(!out.contains("nested.txt"), "should not recurse: {out}");
    }

    #[tokio::test]
    async fn list_recursive_finds_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        tokio::fs::create_dir(&sub).await.unwrap();
        let f_nested = sub.join("deep.txt");
        write(f_nested.to_str().unwrap(), "deep").await.unwrap();

        let out = list(dir.path().to_str().unwrap(), true).await.unwrap();
        assert!(
            out.contains("deep.txt"),
            "recursive find should reach nested file: {out}"
        );
    }

    #[tokio::test]
    async fn delete_directory_removes_entire_tree() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("to_delete");
        tokio::fs::create_dir(&sub).await.unwrap();
        let f = sub.join("file.txt");
        write(f.to_str().unwrap(), "content").await.unwrap();

        delete(sub.to_str().unwrap()).await.unwrap();
        assert!(
            !sub.exists(),
            "directory and its contents should be removed"
        );
    }

    #[tokio::test]
    async fn grep_recursive_finds_match_in_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        tokio::fs::create_dir(&sub).await.unwrap();
        let f = sub.join("d.txt");
        write(f.to_str().unwrap(), "alpha\nbeta\ngamma")
            .await
            .unwrap();

        let out = grep("beta", dir.path().to_str().unwrap(), true)
            .await
            .unwrap();
        assert!(
            out.contains("beta"),
            "recursive grep should find pattern in subdir: {out}"
        );
    }

    #[tokio::test]
    async fn binary_file_reads_as_lossy_utf8_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bin.dat");
        // Write bytes that are not valid UTF-8.
        tokio::fs::write(&p, b"hello\xff\xfeworld\n").await.unwrap();
        let result = read(p.to_str().unwrap(), 0, 100).await;
        assert!(result.is_ok(), "binary read should not hard-error");
        let content = result.unwrap();
        assert!(content.contains("hello"), "ASCII prefix should survive");
        assert!(content.contains("world"), "ASCII suffix should survive");
    }
}
