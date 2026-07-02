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
///
/// Streams line by line, holding at most the requested window (plus the line in
/// flight) — never the whole file. Slurping first and paginating after bounded
/// the reply but not server memory: paging 200 lines of a multi-GB log would
/// have materialized all of it and could OOM the service.
pub async fn read(path: &str, cursor: usize, limit: usize) -> Result<String, String> {
    use tokio::io::AsyncBufReadExt;
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
    let limit = limit.max(1);
    let file = fs::File::open(path).await.map_err(|e| e.to_string())?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut buf: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut window: Vec<String> = Vec::new();
    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
        }
        if total >= cursor && window.len() < limit {
            // Binary-safe: replace non-UTF-8 bytes with U+FFFD, like `lines()` did.
            window.push(String::from_utf8_lossy(&buf).into_owned());
        }
        total += 1;
    }
    // Forward pagination from the top (cursor 0 = first line): a file is read
    // start-to-end. (`job poll` instead reads newest-first — a live log's latest
    // output matters most.) Same byte/line ceilings via `paginate`, applied to
    // the pre-cut window (so the byte cap can shorten the page further).
    let refs: Vec<&str> = window.iter().map(String::as_str).collect();
    let page = crate::jobs::paginate(&refs, 0, limit);
    let body = page.lines.join("\n");
    let next = cursor.min(total) + page.lines.len();
    if next < total {
        Ok(format!(
            "{body}\n[lines {cursor}..{next} of {total}; next_cursor={next}]"
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
    // symlink_metadata, not metadata: a symlink must be unlinked, never followed.
    // Following classified a dir-symlink as a directory (remove_dir_all refuses
    // the top-level link) and made a dangling symlink undeletable (ENOENT on the
    // stat before any removal was attempted).
    let meta = fs::symlink_metadata(path)
        .await
        .map_err(|e| e.to_string())?;
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
        // `find` has no `--`; anchor a leading-dash relative path with `./` so
        // it can't be parsed as an expression.
        let path = if path.starts_with('-') {
            format!("./{path}")
        } else {
            path.to_string()
        };
        sh("find", &[&path]).await.map_err(|e| e.to_string())
    } else {
        sh("ls", &["-la", "--", path])
            .await
            .map_err(|e| e.to_string())
    }
}

pub async fn grep(pattern: &str, path: &str, recursive: bool) -> Result<String, String> {
    let flag = if recursive { "-rn" } else { "-n" };
    // `--` so a pattern like `->` or `-r` is a pattern, not an option: without
    // it, grepping Rust code for `->` errored, and `-r` silently recursed with
    // the *path* as the pattern — wrong results, not even an error.
    match sh("grep", &[flag, "--", pattern, path]).await {
        Ok(s) => Ok(s),
        // Exit 1 is grep's "no line matched" — a legitimate empty result. The
        // marker keeps it distinguishable from matching an empty line.
        Err(ShError::Status { code: Some(1), out }) if out.is_empty() => {
            Ok("[grep: no matches]".to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// A shelled-out command that didn't produce a clean result. Exit status and
/// combined output stay separate so callers can special-case a status (grep's
/// exit-1-means-no-matches) without parsing message text.
#[derive(Debug)]
enum ShError {
    Spawn(String),
    Status { code: Option<i32>, out: String },
}

impl std::fmt::Display for ShError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "{e}"),
            Self::Status { code: Some(c), out } => write!(f, "exit status {c}: {out}"),
            Self::Status { code: None, out } => write!(f, "killed by signal: {out}"),
        }
    }
}

/// Run `prog` and return its combined stdout+stderr — as `Ok` only on exit 0.
/// A failed command must surface as an error, not as a success whose body
/// happens to contain `ls: cannot access ...`.
async fn sh(prog: &str, args: &[&str]) -> Result<String, ShError> {
    let out = tokio::process::Command::new(prog)
        .args(args)
        .output()
        .await
        .map_err(|e| ShError::Spawn(e.to_string()))?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.stderr.is_empty() {
        s.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    let s = cap_bytes(s, MAX_SHELL_OUTPUT_BYTES);
    if out.status.success() {
        Ok(s)
    } else {
        Err(ShError::Status {
            code: out.status.code(),
            out: s,
        })
    }
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
    async fn grep_pattern_starting_with_dash_is_a_pattern_not_an_option() {
        // An agent grepping Rust code for `->` (or worse, `-r`) must get matches,
        // not `grep: invalid option` or a silent argument shift.
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path().join("code.rs");
        let c = c.to_str().unwrap();
        write(c, "fn f() -> i32 { 0 }").await.unwrap();
        let out = grep("->", c, false).await.unwrap();
        assert!(out.contains("-> i32"), "dash pattern must match: {out}");
        let out = grep("-r", c, false).await.unwrap();
        assert!(
            out.contains("no matches"),
            "`-r` is a pattern with no hits, not a flag: {out}"
        );
    }

    #[tokio::test]
    async fn grep_no_matches_is_ok_and_distinguishable() {
        let dir = tempfile::tempdir().unwrap();
        let c = dir.path().join("c.txt");
        let c = c.to_str().unwrap();
        write(c, "alpha").await.unwrap();
        let out = grep("zzz", c, false).await.unwrap();
        assert!(
            out.contains("no matches"),
            "zero matches is a result, not an error or a blank page: {out}"
        );
    }

    #[tokio::test]
    async fn failed_shell_command_is_an_error_not_a_success() {
        // `ls` on a missing path exits non-zero; that must surface as Err, not as
        // an Ok body containing the error text.
        assert!(list("/does/not/exist-mcp-ssh-test", false).await.is_err());
        assert!(list("/does/not/exist-mcp-ssh-test", true).await.is_err());
        assert!(
            grep("x", "/does/not/exist-mcp-ssh-test", false)
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_unlinks_symlinks_instead_of_following() {
        let dir = tempfile::tempdir().unwrap();
        // Symlink to a directory: delete must unlink the link, keep the target.
        let target = dir.path().join("target");
        tokio::fs::create_dir(&target).await.unwrap();
        write(target.join("keep.txt").to_str().unwrap(), "keep")
            .await
            .unwrap();
        let link = dir.path().join("link");
        tokio::fs::symlink(&target, &link).await.unwrap();
        delete(link.to_str().unwrap()).await.unwrap();
        assert!(!link.exists(), "the link itself must be gone");
        assert!(
            target.join("keep.txt").exists(),
            "the target must be untouched"
        );

        // Dangling symlink: previously undeletable (stat followed it to ENOENT).
        let dangling = dir.path().join("dangling");
        tokio::fs::symlink(dir.path().join("nope"), &dangling)
            .await
            .unwrap();
        delete(dangling.to_str().unwrap()).await.unwrap();
        assert!(
            tokio::fs::symlink_metadata(&dangling).await.is_err(),
            "dangling link must be removed"
        );
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
