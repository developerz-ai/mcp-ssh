//! File operations, executed locally as the service user. Read/write/move go
//! through `tokio::fs`; list/grep shell out to `ls`/`find`/`grep` rather than
//! reimplementing them.
use tokio::{fs, io::AsyncWriteExt};

/// Read a file, paginated by line so a huge file can't flood the agent context.
pub async fn read(path: &str, cursor: usize, limit: usize) -> Result<String, String> {
    let content = fs::read_to_string(path).await.map_err(|e| e.to_string())?;
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
}
