use super::*;

/// The text a read/list/grep produced (a read page's lines joined, no footer).
/// Ops return a structured outcome, so a body assertion stays one line.
fn text(outcome: FileOutcome) -> String {
    match outcome {
        FileOutcome::Read(page) => lines(&page).join("\n"),
        FileOutcome::Listing(l) => l.text,
        other => panic!("expected output text, got {other:?}"),
    }
}

fn page(outcome: FileOutcome) -> ReadPage {
    match outcome {
        FileOutcome::Read(page) => page,
        other => panic!("expected a read page, got {other:?}"),
    }
}

fn lines(page: &ReadPage) -> Vec<&str> {
    page.lines.iter().map(|l| l.text.as_str()).collect()
}

fn listing(outcome: FileOutcome) -> Listing {
    match outcome {
        FileOutcome::Listing(l) => l,
        other => panic!("expected a listing, got {other:?}"),
    }
}

#[tokio::test]
async fn write_read_paginate_append_move_delete() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let a = a.to_str().unwrap();

    write(a, "l1\nl2\nl3").await.unwrap();
    let first = page(read(a, 0, 2).await.unwrap());
    assert_eq!(lines(&first), ["l1", "l2"]);
    assert_eq!(first.next, 2);
    assert_eq!(first.end, PageEnd::More { at_least: 3 });

    append(a, "\nl4").await.unwrap();
    assert!(text(read(a, 0, 100).await.unwrap()).contains("l4"));

    let b = dir.path().join("b.txt");
    let b = b.to_str().unwrap();
    rename(a, b).await.unwrap();
    assert!(read(a, 0, 10).await.is_err());

    delete(b, false).await.unwrap();
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
        matches!(err, FileError::Shell { op: "list", source: ShError::Status { code, .. }, .. } if code != 0),
        "a failed listing must carry its exit status: {err:?}"
    );
}

#[tokio::test]
async fn oversized_no_newline_line_is_capped_not_slurped() {
    // A multi-MiB file with no newline must not be slurped into one buffer: the
    // per-line cap keeps the reply (and memory) bounded, and the dropped tail is
    // counted exactly (B9) — one marker, the true number.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("huge.txt");
    let big = vec![b'a'; 2 * 1024 * 1024];
    tokio::fs::write(&p, &big).await.unwrap();
    let pg = page(read(p.to_str().unwrap(), 0, 100).await.unwrap());
    assert_eq!(pg.lines.len(), 1);
    let line = &pg.lines[0];
    assert_eq!(
        line.text.len(),
        MAX_READ_LINE_BYTES,
        "head kept up to the cap"
    );
    assert!(line.text.starts_with("aaa"));
    assert_eq!(
        line.text.len() + line.dropped,
        big.len(),
        "dropped count is exact"
    );
    assert_eq!(pg.end, PageEnd::Eof { total: 1 });
}

#[tokio::test]
async fn first_page_of_large_file_reports_lower_bound_not_a_full_scan() {
    // Reading page 1 must not walk the whole file just to print an exact total: on a
    // multi-GB log that would defeat the line cursor.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("big.txt");
    let mut body = String::new();
    for i in 0..5000 {
        body.push_str(&format!("line-{i:05}\n"));
    }
    tokio::fs::write(&p, &body).await.unwrap();

    let pg = page(read(p.to_str().unwrap(), 0, 10).await.unwrap());
    assert_eq!(pg.lines.len(), 10);
    assert_eq!(pg.lines[0].text, "line-00000");
    assert_eq!(pg.lines[9].text, "line-00009");
    assert_eq!((pg.cursor, pg.next), (0, 10));
    assert_eq!(
        pg.end,
        PageEnd::More { at_least: 11 },
        "early stop reports a lower bound, not the true 5000"
    );
}

#[tokio::test]
async fn later_pages_advance_and_the_final_page_ends_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("multi.txt");
    tokio::fs::write(&p, b"a\nb\nc\nd\ne\n").await.unwrap();
    let p = p.to_str().unwrap();

    let mid = page(read(p, 1, 2).await.unwrap());
    assert_eq!(lines(&mid), ["b", "c"]);
    assert_eq!(mid.next, 3);
    assert_eq!(mid.end, PageEnd::More { at_least: 4 });

    // The last lines fit in the window: exact end.
    let last = page(read(p, 3, 10).await.unwrap());
    assert_eq!(lines(&last), ["d", "e"]);
    assert_eq!(last.end, PageEnd::Eof { total: 5 });

    // Reading past the end is an empty page that still knows the line count (F11).
    let past = page(read(p, 99, 10).await.unwrap());
    assert!(past.lines.is_empty());
    assert_eq!(past.end, PageEnd::Eof { total: 5 });
}

/// F11: a file of exactly `limit` lines is one complete page — no next cursor
/// that would only lead to an empty page.
#[tokio::test]
async fn exactly_limit_lines_is_a_complete_page() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("two.txt");
    tokio::fs::write(&p, b"x\ny\n").await.unwrap();
    let pg = page(read(p.to_str().unwrap(), 0, 2).await.unwrap());
    assert_eq!(lines(&pg), ["x", "y"]);
    assert_eq!(pg.end, PageEnd::Eof { total: 2 });
}

#[tokio::test]
async fn empty_file_is_an_empty_page_with_zero_total() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("empty.txt");
    tokio::fs::write(&p, b"").await.unwrap();
    let pg = page(read(p.to_str().unwrap(), 0, 10).await.unwrap());
    assert!(pg.lines.is_empty());
    assert_eq!(pg.end, PageEnd::Eof { total: 0 });
}

/// B4: `limit` is clamped and the page is byte-budgeted — `usize::MAX` can't
/// make one reply (or the server's memory) proportional to the file.
#[tokio::test]
async fn huge_limit_is_clamped_and_byte_budgeted() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("wide.txt");
    let line = "w".repeat(1000);
    let body = format!("{line}\n").repeat(5000);
    tokio::fs::write(&p, &body).await.unwrap();
    let pg = page(read(p.to_str().unwrap(), 0, usize::MAX).await.unwrap());
    let bytes: usize = pg.lines.iter().map(|l| l.text.len() + 1).sum();
    assert!(
        bytes <= 64 * 1024,
        "page stays within the byte budget: {bytes}"
    );
    assert!(
        matches!(pg.end, PageEnd::More { .. }),
        "more remains: {:?}",
        pg.end
    );
    assert_eq!(pg.next, pg.lines.len());

    let short = dir.path().join("short.txt");
    tokio::fs::write(&short, "s\n".repeat(5000)).await.unwrap();
    let pg = page(read(short.to_str().unwrap(), 0, usize::MAX).await.unwrap());
    assert_eq!(pg.lines.len(), read::MAX_READ_LINES, "line count clamped");
}

/// B4/B5 on a sparse 50 MiB file (one newline-free line of zeros): returns
/// promptly with one capped line and the true dropped count.
#[tokio::test]
async fn sparse_huge_file_reads_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("sparse.bin");
    let f = std::fs::File::create(&p).unwrap();
    f.set_len(50 * 1024 * 1024).unwrap();
    let pg = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        read(p.to_str().unwrap(), 0, usize::MAX),
    )
    .await
    .expect("bounded scan")
    .unwrap();
    let pg = page(pg);
    assert_eq!(pg.lines.len(), 1);
    assert_eq!(
        pg.lines[0].text.len() + pg.lines[0].dropped,
        50 * 1024 * 1024
    );
}

/// B5: a device never ends and a FIFO never starts — both are refused typed,
/// promptly, instead of pinning a core forever.
#[tokio::test]
async fn non_regular_files_are_refused_promptly() {
    let zero = tokio::time::timeout(std::time::Duration::from_secs(5), read("/dev/zero", 0, 10))
        .await
        .expect("must not spin on /dev/zero")
        .unwrap_err();
    assert!(matches!(zero, FileError::NotRegularFile { .. }), "{zero:?}");

    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("pipe");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read(fifo.to_str().unwrap(), 0, 10),
    )
    .await
    .expect("opening a writer-less FIFO must not block")
    .unwrap_err();
    assert!(matches!(err, FileError::NotRegularFile { .. }), "{err:?}");
}

/// F7: an I/O failure names the op and the path, not just an errno.
#[tokio::test]
async fn io_errors_carry_op_and_path() {
    let err = read("/does/not/exist-mcp-ssh-test", 0, 10)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, FileError::Io { op: "read", path, .. } if path == "/does/not/exist-mcp-ssh-test"),
        "{err:?}"
    );
    assert!(
        err.to_string()
            .starts_with("read /does/not/exist-mcp-ssh-test: ")
    );
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
    delete(link.to_str().unwrap(), false).await.unwrap();
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
    delete(dangling.to_str().unwrap(), false).await.unwrap();
    assert!(
        tokio::fs::symlink_metadata(&dangling).await.is_err(),
        "dangling link must be removed"
    );
}

/// F1: `link/` (trailing slash) resolves *through* the link under POSIX rules, so
/// classifying it with lstat-on-the-raw-path recursed into the target and emptied
/// it. The link must be unlinked and the target left whole.
#[cfg(unix)]
#[tokio::test]
async fn delete_symlink_with_trailing_slash_keeps_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    tokio::fs::create_dir(&target).await.unwrap();
    tokio::fs::write(target.join("keep.txt"), "keep")
        .await
        .unwrap();
    let link = dir.path().join("link");
    tokio::fs::symlink(&target, &link).await.unwrap();

    let with_slash = format!("{}/", link.to_str().unwrap());
    let _ = delete(&with_slash, true).await;
    assert!(
        target.join("keep.txt").exists(),
        "the symlink target's contents must survive"
    );
    assert!(
        tokio::fs::symlink_metadata(&link).await.is_err(),
        "the link itself is what gets removed"
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

    let l = listing(list(dir.path().to_str().unwrap(), true).await.unwrap());
    assert!(
        !l.truncated,
        "exclusion must be the depth bound, not the byte cap"
    );
    let out = l.text;
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

    // F2: without the opt-in, a populated tree is refused and left whole.
    let err = delete(sub.to_str().unwrap(), false).await.unwrap_err();
    assert!(
        matches!(err, FileError::DirectoryNotEmpty { .. }),
        "{err:?}"
    );
    assert!(f.exists(), "a refused delete removes nothing");

    delete(sub.to_str().unwrap(), true).await.unwrap();
    assert!(
        !sub.exists(),
        "directory and its contents should be removed"
    );
}

#[tokio::test]
async fn delete_empty_directory_needs_no_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("empty");
    tokio::fs::create_dir(&sub).await.unwrap();
    delete(sub.to_str().unwrap(), false).await.unwrap();
    assert!(!sub.exists());
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

/// F3: moving onto an existing directory is refused typed (the adapter then
/// suggests the full path), and both sides are untouched.
#[tokio::test]
async fn move_onto_a_directory_is_refused_typed() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("app.tar");
    let releases = dir.path().join("releases");
    tokio::fs::create_dir(&releases).await.unwrap();
    tokio::fs::write(releases.join("v1"), "v1").await.unwrap();
    write(src.to_str().unwrap(), "new").await.unwrap();

    let err = rename(src.to_str().unwrap(), releases.to_str().unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(err, FileError::DestinationIsDirectory { .. }),
        "{err:?}"
    );
    assert!(src.exists() && releases.join("v1").exists());
}

/// F4: a move that can't succeed creates nothing — the missing source is named
/// and the destination's parents are not left behind.
#[tokio::test]
async fn failed_move_leaves_no_stray_directories() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.txt");
    let dest = dir.path().join("q/r/out.txt");
    let err = rename(missing.to_str().unwrap(), dest.to_str().unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, FileError::SourceMissing { src } if src == missing.to_str().unwrap()),
        "{err:?}"
    );
    assert!(
        !dir.path().join("q").exists(),
        "no parent created for a doomed move"
    );

    // A directory into itself: refused before anything is created.
    let a = dir.path().join("a");
    tokio::fs::create_dir(&a).await.unwrap();
    let inner = a.join("x/y");
    let err = rename(a.to_str().unwrap(), inner.to_str().unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, FileError::MoveIntoItself { .. }), "{err:?}");
    assert!(!a.join("x").exists(), "no stray dir inside the source");
}

/// F6: a recursive listing with one unreadable subdir is a partial result, not
/// an error that throws the readable entries away.
#[cfg(unix)]
#[tokio::test]
async fn recursive_list_with_unreadable_subdir_is_partial() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    tokio::fs::write(dir.path().join("visible.txt"), "v")
        .await
        .unwrap();
    let locked = dir.path().join("locked");
    tokio::fs::create_dir(&locked).await.unwrap();
    tokio::fs::write(locked.join("hidden.txt"), "h")
        .await
        .unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // root reads through 000: the premise doesn't hold there, so the probe decides.
    let blocked = std::fs::read_dir(&locked).is_err();

    let result = list(dir.path().to_str().unwrap(), true).await;
    // Restore before asserting so tempdir cleanup always works.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if !blocked {
        return;
    }
    let l = listing(result.expect("partial results are not an error"));
    assert!(l.partial, "the non-zero exit is noted");
    assert!(
        l.text.contains("visible.txt"),
        "readable entries survive: {}",
        l.text
    );
}

/// F16: grep on a directory without `recursive` is typed, so the adapter can
/// point at `recursive=true`.
#[tokio::test]
async fn grep_on_directory_without_recursive_is_typed() {
    let dir = tempfile::tempdir().unwrap();
    let err = grep("x", dir.path().to_str().unwrap(), false)
        .await
        .unwrap_err();
    assert!(matches!(err, FileError::GrepDirectory { .. }), "{err:?}");
}

/// F15: write replaces atomically — an existing file keeps its mode, and no
/// temp file is left beside it.
#[cfg(unix)]
#[tokio::test]
async fn write_is_atomic_and_preserves_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let env = dir.path().join(".env");
    std::fs::write(&env, "OLD=1").unwrap();
    std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o600)).unwrap();

    write(env.to_str().unwrap(), "NEW=2").await.unwrap();
    assert_eq!(std::fs::read_to_string(&env).unwrap(), "NEW=2");
    let mode = std::fs::metadata(&env).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "mode preserved");
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "no temp left"
    );
}

/// F15: writing through a symlink updates the target; the link stays a link.
#[cfg(unix)]
#[tokio::test]
async fn write_follows_an_existing_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("real.conf");
    std::fs::write(&target, "old").unwrap();
    let link = dir.path().join("link.conf");
    std::os::unix::fs::symlink("real.conf", &link).unwrap();

    write(link.to_str().unwrap(), "new").await.unwrap();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
}

/// F7/F15: writing onto a directory fails naming the op and path, and the temp
/// file is cleaned up.
#[tokio::test]
async fn write_onto_a_directory_errs_with_context_and_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    tokio::fs::create_dir(&sub).await.unwrap();
    let err = write(sub.to_str().unwrap(), "x").await.unwrap_err();
    assert!(
        matches!(&err, FileError::Io { op: "write", path, .. } if path == sub.to_str().unwrap()),
        "{err:?}"
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "no temp left"
    );
}
