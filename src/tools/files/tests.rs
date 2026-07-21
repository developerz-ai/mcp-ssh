use super::*;

/// The text a read/list/grep produced. Ops return a structured outcome now, so a
/// body assertion stays one line.
fn text(outcome: FileOutcome) -> String {
    match outcome {
        FileOutcome::Output(s) => s,
        other => panic!("expected output text, got {other:?}"),
    }
}

#[tokio::test]
async fn write_read_paginate_append_move_delete() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let a = a.to_str().unwrap();

    write(a, "l1\nl2\nl3").await.unwrap();
    let page = text(read(a, 0, 2).await.unwrap());
    assert!(page.contains("l1") && page.contains("l2") && !page.contains("l3"));
    assert!(page.contains("next_cursor=2"));

    append(a, "\nl4").await.unwrap();
    assert!(text(read(a, 0, 100).await.unwrap()).contains("l4"));

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
    assert_eq!(text(read(nested, 0, 10).await.unwrap()), "hi");

    // append to a fresh path under a new dir works too.
    let ap = dir.path().join("x/y/z.log");
    let ap = ap.to_str().unwrap();
    append(ap, "one\n").await.unwrap();
    assert!(text(read(ap, 0, 10).await.unwrap()).contains("one"));
}

/// A directory `read` must fail as its own variant, not as a raw "Is a directory"
/// errno — that's what lets the adapter redirect the agent to `list`.
#[tokio::test]
async fn read_on_directory_is_a_typed_error_not_an_errno() {
    let dir = tempfile::tempdir().unwrap();
    let err = read(dir.path().to_str().unwrap(), 0, 10).await.unwrap_err();
    assert!(
        matches!(err, FileError::IsDirectory { .. }),
        "dir read must be typed: {err:?}"
    );
}

/// A failed `ls`/`find`/`grep` must arrive as the typed `ShError` it was. The exit
/// code is load-bearing (grep's exit 1 = no matches); flattening to a string threw
/// it away and left the distinction to message text.
#[tokio::test]
async fn failed_shell_op_keeps_its_typed_exit_status() {
    let err = list("/does/not/exist-mcp-ssh-test", false)
        .await
        .unwrap_err();
    assert!(
        matches!(err, FileError::Shell(ShError::Status { code, .. }) if code != 0),
        "a failed listing must carry its exit status: {err:?}"
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
    let out = text(read(p.to_str().unwrap(), 0, 100).await.unwrap());
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
async fn first_page_of_large_file_reports_lower_bound_not_a_full_scan() {
    // Reading page 1 must not walk the whole file just to print an exact total: on a
    // multi-GB log that would defeat the line cursor. Proof by count — the footer
    // reports the page-sized lower bound (`≥ n`), never the file's true line count.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("big.txt");
    let mut body = String::new();
    for i in 0..5000 {
        body.push_str(&format!("line-{i:05}\n"));
    }
    tokio::fs::write(&p, &body).await.unwrap();

    let out = text(read(p.to_str().unwrap(), 0, 10).await.unwrap());
    assert!(
        out.contains("line-00000") && out.contains("line-00009"),
        "page 1 must hold the first window: {out}"
    );
    assert!(
        !out.contains("line-00010"),
        "page must stop at the window edge: {out}"
    );
    assert!(
        out.contains("of ≥10") && out.contains("next_cursor=10"),
        "early stop must report a lower-bound total: {out}"
    );
    assert!(
        !out.contains("of 5000"),
        "an exact total would mean the scan walked to EOF: {out}"
    );
}

#[tokio::test]
async fn later_pages_advance_and_the_final_page_ends_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("multi.txt");
    tokio::fs::write(&p, b"a\nb\nc\nd\ne\n").await.unwrap();
    let p = p.to_str().unwrap();

    // A middle page advances the cursor and still reports a lower bound (we stopped
    // once the two-line window filled, before learning the true total).
    let mid = text(read(p, 1, 2).await.unwrap());
    assert!(
        mid.contains('b') && mid.contains('c') && !mid.contains('d'),
        "middle page holds its window: {mid}"
    );
    assert!(
        mid.contains("of ≥3") && mid.contains("next_cursor=3"),
        "middle page advances on a lower-bound total: {mid}"
    );

    // The last lines fit in the window, so the scan reaches EOF: exact end, no footer.
    let last = text(read(p, 3, 10).await.unwrap());
    assert_eq!(last, "d\ne", "final page is exact and carries no cursor");

    // Reading past the end is an empty page, never a bogus cursor.
    let past = text(read(p, 99, 10).await.unwrap());
    assert_eq!(past, "", "past-EOF read is empty with no footer");
}

#[tokio::test]
async fn grep_finds_match() {
    let dir = tempfile::tempdir().unwrap();
    let c = dir.path().join("c.txt");
    let c = c.to_str().unwrap();
    write(c, "alpha\nbeta\ngamma").await.unwrap();
    assert!(text(grep("beta", c, false).await.unwrap()).contains("beta"));
}

#[tokio::test]
async fn grep_pattern_starting_with_dash_is_a_pattern_not_an_option() {
    // An agent grepping Rust code for `->` (or worse, `-r`) must get matches,
    // not `grep: invalid option` or a silent argument shift.
    let dir = tempfile::tempdir().unwrap();
    let c = dir.path().join("code.rs");
    let c = c.to_str().unwrap();
    write(c, "fn f() -> i32 { 0 }").await.unwrap();
    let out = text(grep("->", c, false).await.unwrap());
    assert!(out.contains("-> i32"), "dash pattern must match: {out}");
    let out = grep("-r", c, false).await.unwrap();
    assert!(
        matches!(out, FileOutcome::NoMatches),
        "`-r` is a pattern with no hits, not a flag: {out:?}"
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
        matches!(out, FileOutcome::NoMatches),
        "zero matches is its own outcome, not an error or a blank page: {out:?}"
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

    let out = text(list(dir.path().to_str().unwrap(), false).await.unwrap());
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

    let out = text(list(dir.path().to_str().unwrap(), true).await.unwrap());
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

    let out = text(list(dir.path().to_str().unwrap(), true).await.unwrap());
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

    let out = text(
        grep("beta", dir.path().to_str().unwrap(), true)
            .await
            .unwrap(),
    );
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
    let content = text(result.unwrap());
    assert!(content.contains("hello"), "ASCII prefix should survive");
    assert!(content.contains("world"), "ASCII suffix should survive");
}

#[tokio::test]
async fn move_onto_an_existing_dest_errs_and_leaves_both_files_intact() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.txt");
    let dst = dir.path().join("dst.txt");
    let (src, dst) = (src.to_str().unwrap(), dst.to_str().unwrap());
    write(src, "SRC").await.unwrap();
    write(dst, "DST").await.unwrap();

    let err = rename(src, dst).await.unwrap_err();
    assert!(
        matches!(err, FileError::DestinationExists { .. }),
        "a clobbering move must error, not overwrite: {err:?}"
    );
    // Move is all-or-nothing: neither file is touched.
    assert_eq!(
        text(read(src, 0, 10).await.unwrap()),
        "SRC",
        "source stays put"
    );
    assert_eq!(
        text(read(dst, 0, 10).await.unwrap()),
        "DST",
        "destination is untouched"
    );
}

#[tokio::test]
async fn move_onto_a_free_path_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.txt");
    let dst = dir.path().join("free.txt");
    let (src, dst) = (src.to_str().unwrap(), dst.to_str().unwrap());
    write(src, "hello").await.unwrap();

    rename(src, dst).await.unwrap();
    assert!(
        read(src, 0, 10).await.is_err(),
        "source is gone after a move"
    );
    assert_eq!(
        text(read(dst, 0, 10).await.unwrap()),
        "hello",
        "content lands at the free destination"
    );
}

#[test]
fn append_capped_marks_an_over_long_line_exactly_once() {
    // Overflow the per-line cap, then feed more: the line seals with a single marker
    // and the later bytes are dropped, never appended after it.
    let mut line = Vec::new();
    append_capped(&mut line, &vec![b'a'; MAX_READ_LINE_BYTES + 5_000]);
    append_capped(&mut line, b"tail");

    let s = String::from_utf8(line).expect("ASCII head + a valid-UTF-8 marker");
    assert!(s.starts_with("aaa"), "the line's head survives");
    assert!(
        s.ends_with(LINE_TRUNCATED),
        "an over-long line carries the truncation marker: …{}",
        &s[s.len().saturating_sub(24)..]
    );
    assert_eq!(
        s.matches(LINE_TRUNCATED).count(),
        1,
        "the marker is stamped exactly once"
    );
    assert!(
        !s.contains("tail"),
        "bytes past the seal are dropped, never appended after the marker"
    );
    assert!(
        s.len() <= MAX_READ_LINE_BYTES + LINE_TRUNCATED.len(),
        "the buffer stays bounded by the cap plus the marker"
    );
}

#[test]
fn append_capped_seals_when_the_cap_lands_in_a_run_of_continuation_bytes() {
    // A binary `read` can straddle the cap with a long run of continuation bytes
    // (0x80). The boundary walk-back is floored at 3, so the kept head can't
    // collapse: the marker's bytes still push the line past the cap, the seal
    // latches, and a second marker can never be stamped.
    let mut bytes = vec![b'a'; MAX_READ_LINE_BYTES - 64];
    bytes.extend_from_slice(&[0x80u8; 128]);

    let mut line = Vec::new();
    append_capped(&mut line, &bytes);
    assert!(
        line.len() > MAX_READ_LINE_BYTES,
        "the first overflow must seal the line, got {} bytes",
        line.len()
    );

    append_capped(&mut line, b"tail");
    let s = String::from_utf8_lossy(&line);
    assert_eq!(
        s.matches(LINE_TRUNCATED).count(),
        1,
        "the marker is stamped exactly once, even on binary input"
    );
    assert!(!s.contains("tail"), "bytes past the seal are dropped");
}

#[test]
fn append_capped_trims_a_split_multibyte_char_before_the_marker() {
    // The cap falls in the middle of a 3-byte char (€ = E2 82 AC): the straddling
    // char is dropped so the marker isn't preceded by a broken code point.
    let mut bytes = vec![b'a'; MAX_READ_LINE_BYTES - 1];
    bytes.extend_from_slice("€".as_bytes());
    bytes.extend_from_slice(b"zzzz");

    let mut line = Vec::new();
    append_capped(&mut line, &bytes);

    let s = String::from_utf8(line).expect("head trimmed to a boundary → clean UTF-8");
    assert!(
        s.ends_with(LINE_TRUNCATED) && !s.contains('€'),
        "the split char is dropped, the marker stamped cleanly: …{}",
        &s[s.len().saturating_sub(24)..]
    );
}
