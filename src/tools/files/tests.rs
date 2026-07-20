use super::*;
use std::time::Duration;

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
async fn oversized_no_newline_line_is_capped_not_slurped() {
    // A multi-MiB file with no newline must not be slurped into one buffer: the
    // per-line cap plus paginate's clamp keep the reply (and memory) bounded.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("huge.txt");
    let big = vec![b'a'; 2 * 1024 * 1024];
    tokio::fs::write(&p, &big).await.unwrap();
    let out = read(p.to_str().unwrap(), 0, 100).await.unwrap();
    assert!(
        out.len() <= 16 * 1024,
        "oversized no-newline line must be bounded, got {} bytes",
        out.len()
    );
    assert!(
        out.starts_with("aaa"),
        "the head of the line should survive"
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
async fn list_recursive_is_depth_bounded() {
    // A tree deeper than `MAX_FIND_DEPTH`: entries past the bound must not appear,
    // so a pathologically deep tree can't drive `find` into an unbounded descent.
    let dir = tempfile::tempdir().unwrap();

    // Nest `path/d/d/.../d` several levels beyond the depth bound.
    let mut deep = dir.path().to_path_buf();
    for _ in 0..(MAX_FIND_DEPTH as usize + 5) {
        deep = deep.join("d");
    }
    tokio::fs::create_dir_all(&deep).await.unwrap();
    let buried = deep.join("buried.txt");
    write(buried.to_str().unwrap(), "x").await.unwrap();

    // A shallow file within the bound must still be listed.
    let shallow = dir.path().join("shallow.txt");
    write(shallow.to_str().unwrap(), "y").await.unwrap();

    let out = list(dir.path().to_str().unwrap(), true).await.unwrap();
    assert!(
        !out.contains("truncated"),
        "exclusion must be the depth bound, not the byte cap: {out}"
    );
    assert!(
        out.contains("shallow.txt"),
        "within-bound file must be listed: {out}"
    );
    assert!(
        !out.contains("buried.txt"),
        "file past maxdepth must not be listed: {out}"
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

#[cfg(unix)]
#[tokio::test]
async fn shell_output_is_capped_and_infinite_producer_killed() {
    // `yes` streams "y\n" forever. A correct runner reads to the cap, kills it,
    // and returns in milliseconds; a runner that buffered the whole stream would
    // spin on it — so the outer timeout is the proof of streaming, and the bounded
    // length proves the peak buffer tracks the cap, not the (unbounded) stream.
    let result = tokio::time::timeout(Duration::from_secs(10), sh("yes", &[]))
        .await
        .expect("runner must terminate an infinite producer (streamed cap)")
        .expect("a capped listing is still Ok");
    assert!(
        result.contains("truncated"),
        "capped output must carry a truncation marker: {:?}",
        result.get(result.len().saturating_sub(120)..)
    );
    assert!(
        result.len() <= MAX_SHELL_OUTPUT_BYTES + 200,
        "peak buffer must track the cap, got {} bytes",
        result.len()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn shell_run_times_out_and_reaps_child() {
    // A command that outlives the deadline must error promptly, not wait out the
    // full sleep. `run_bounded` SIGKILLs and awaits the child, so it's reaped — no
    // orphan outlives this call.
    let start = std::time::Instant::now();
    let err = run_bounded(
        "sleep",
        &["30"],
        Duration::from_millis(200),
        MAX_SHELL_OUTPUT_BYTES,
    )
    .await
    .expect_err("a command exceeding the deadline must error");
    let elapsed = start.elapsed();
    assert!(
        matches!(err, ShError::Timeout { .. }),
        "must surface as a timeout: {err}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the deadline must fire promptly, not wait out the child: {elapsed:?}"
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
