use super::*;

fn page(text: &str, end: Option<u64>, limit: usize) -> TailPage {
    tail_of_text(text, end, TailBudget::page(limit))
}

#[test]
fn newest_window_first_then_older_by_absolute_offset() {
    let text: String = (1..=10).map(|i| format!("{i}\n")).collect();
    let p0 = page(&text, None, 3);
    assert_eq!(p0.lines, ["8", "9", "10"], "newest window, in order");
    let older = p0.older.expect("older lines remain");
    assert_eq!(&text[older as usize..], "8\n9\n10\n", "cursor = page start");

    let p1 = page(&text, Some(older), 3);
    assert_eq!(p1.lines, ["5", "6", "7"]);

    let p3 = page(&text, Some(2), 3);
    assert_eq!(p3.lines, ["1"], "only the oldest line is left");
    assert_eq!(p3.older, None, "nothing older remains");
}

/// The cursor is anchored to bytes already written, so output appended after
/// the first page never shifts the pages behind it (B8).
#[test]
fn appending_output_never_shifts_an_older_page() {
    let mut text: String = (1..=10).map(|i| format!("{i}\n")).collect();
    let older = page(&text, None, 3).older.unwrap();
    text.push_str("11\n12\n13\n");
    assert_eq!(page(&text, Some(older), 3).lines, ["5", "6", "7"]);
}

#[test]
fn edge_shapes() {
    assert_eq!(
        page("", None, 5),
        TailPage {
            lines: vec![],
            older: None
        }
    );
    assert_eq!(page("abc", None, 5).lines, ["abc"], "no trailing newline");
    assert_eq!(page("\n\n", None, 5).lines, ["", ""], "two empty lines");
    assert_eq!(page("a\r\nb\r\n", None, 5).lines, ["a", "b"], "CRLF");
    assert_eq!(
        page("a\nb\n", Some(999), 5).lines,
        ["a", "b"],
        "cursor past end"
    );
    assert_eq!(
        page("a\nb\nc\n", None, 0).lines,
        ["c"],
        "limit 0 is one line"
    );
}

/// A progress bar redraws one line with `\r`: the page shows what the
/// terminal would — the last frame, not the first (B7).
#[test]
fn carriage_return_keeps_the_last_frame() {
    assert_eq!(
        page("10%\r50%\r100%\ndone\n", None, 5).lines,
        ["100%", "done"]
    );
}

#[test]
fn an_over_long_line_keeps_its_newest_bytes() {
    let line = format!("{}END", "x".repeat(100 * 1024));
    let p = page(&format!("{line}\n"), None, 5);
    assert_eq!(p.lines.len(), 1);
    let shown = &p.lines[0];
    assert!(shown.ends_with("END"), "the tail of the line survives");
    assert!(
        shown.len() <= MAX_LINE_BYTES + 32,
        "clamped: {}",
        shown.len()
    );
    let want = format!("[+{} bytes]…", line.len() - MAX_LINE_BYTES);
    assert!(
        shown.starts_with(&want),
        "exact dropped count: {}",
        &shown[..30]
    );
    assert_eq!(p.older, None, "the whole line was scanned");
}

/// A 5 MiB line exceeds the scan ceiling: the page shows its newest bytes,
/// marked cut, and the next page resumes inside it.
#[test]
fn a_line_longer_than_the_scan_ceiling_is_cut_and_continued() {
    let text = format!("{}END\n", "x".repeat(5 * 1024 * 1024));
    let p = page(&text, None, 5);
    assert_eq!(p.lines.len(), 1);
    assert!(p.lines[0].starts_with(CUT_MARKER) && p.lines[0].ends_with("END"));
    assert!(p.lines[0].len() <= MAX_LINE_BYTES + 64);
    let next = page(&text, p.older, 5);
    assert!(next.lines[0].starts_with(CUT_MARKER) && next.older < p.older);
}

#[test]
fn non_utf8_is_rendered_lossily_not_dropped() {
    let p = tail_of_bytes(b"line1\nline2\xff\xfeline3\n", 5);
    assert_eq!(p.lines.len(), 2);
    assert!(p.lines[1].contains("line3"));
}

fn tail_of_bytes(bytes: &[u8], limit: usize) -> TailPage {
    tail_of(
        bytes.len() as u64,
        None,
        TailBudget::page(limit),
        |buf, off| {
            let off = off as usize;
            buf.copy_from_slice(&bytes[off..off + buf.len()]);
            Ok(())
        },
    )
    .unwrap()
}

#[test]
fn a_page_stays_under_the_byte_budget_keeping_the_newest() {
    let text: String = (0..1000).map(|i| format!("{i:01000}\n")).collect();
    let p = page(&text, None, DEFAULT_PAGE);
    let returned: usize = p.lines.iter().map(|l| l.len() + 1).sum();
    assert!(
        returned <= MAX_PAGE_BYTES,
        "byte budget honored: {returned}"
    );
    assert!(p.older.is_some());
    assert_eq!(p.lines.last().unwrap(), &format!("{:01000}", 999));
}

/// The server-OOM regression (B1): a multi-GB log with a short real tail must
/// come back promptly having read only a bounded number of bytes.
#[tokio::test]
async fn a_huge_sparse_log_is_read_in_bounded_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.log");
    let file = std::fs::File::create(&path).unwrap();
    let hole = 2u64 * 1024 * 1024 * 1024;
    file.set_len(hole).unwrap();
    drop(file);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"\nlast-1\nlast-2\n").unwrap();
    }

    let started = std::time::Instant::now();
    let p = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_tail(&path, None, TailBudget::page(2)),
    )
    .await
    .expect("bounded read returns promptly")
    .unwrap();
    assert_eq!(p.lines, ["last-1", "last-2"]);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));

    // Count the bytes a full-budget read touches: the scan ceiling, not 2 GiB.
    let mut read = 0u64;
    let file = std::fs::File::open(&path).unwrap();
    let len = file.metadata().unwrap().len();
    let p = tail_of(len, None, TailBudget::page(DEFAULT_PAGE), |buf, off| {
        use std::os::unix::fs::FileExt;
        read += buf.len() as u64;
        file.read_exact_at(buf, off)
    })
    .unwrap();
    assert!(
        read <= MAX_PAGE_BYTES as u64 + SCAN_SLACK + CHUNK as u64,
        "read {read} bytes"
    );
    assert!(
        p.lines[0].starts_with(CUT_MARKER),
        "the endless line is cut"
    );
    assert!(p.older.is_some(), "and the next page resumes inside it");
}

#[tokio::test]
async fn a_missing_log_is_an_error_not_an_empty_page() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.log");
    assert!(
        read_tail(&missing, None, TailBudget::page(5))
            .await
            .is_err()
    );
    assert!(tail(&missing, TailBudget::page(5)).await.is_err());
}
