//! File operations, executed locally as the service user. Read/write/move go
//! through `tokio::fs`; list/grep shell out to `ls`/`find`/`grep` rather than
//! reimplementing them.
use tokio::{fs, io::AsyncWriteExt};

/// Read a file, paginated by line so a huge file can't flood the agent context.
pub async fn read(path: &str, cursor: usize, limit: usize) -> Result<String, String> {
    // Binary-safe: replace non-UTF-8 bytes with U+FFFD rather than hard-erroring.
    let bytes = fs::read(path).await.map_err(|e| e.to_string())?;
    let owned = String::from_utf8_lossy(&bytes).into_owned();
    let content = owned.as_str();
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let end = (cursor + limit).min(total);
    let body = lines.get(cursor..end).unwrap_or(&[]).join("\n");
    if end < total {
        Ok(format!(
            "{body}\n[lines {cursor}..{end} of {total}; next_cursor={end}]"
        ))
    } else {
        Ok(body)
    }
}

pub async fn write(path: &str, content: &str) -> Result<String, String> {
    fs::write(path, content).await.map_err(|e| e.to_string())?;
    Ok(format!("wrote {} bytes to {path}", content.len()))
}

pub async fn append(path: &str, content: &str) -> Result<String, String> {
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
    fs::rename(src, dest).await.map_err(|e| e.to_string())?;
    Ok(format!("moved {src} -> {dest}"))
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
    Ok(s)
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
