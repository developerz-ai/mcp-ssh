//! Agent-facing wording for the `file` tool. `files` returns facts (a
//! [`FileOutcome`] or a typed [`FileError`]); every sentence, marker and
//! next-step hint — the ones that name `file`'s own actions and params — is
//! phrased here, so the domain never writes prose.
use std::path::Path;

use super::files::{
    FileError, FileOutcome, Listing, MAX_SHELL_OUTPUT_BYTES, PageEnd, ReadPage, ShError,
};

/// The text a finished file op reads as.
pub(super) fn render_file(outcome: FileOutcome) -> String {
    match outcome {
        FileOutcome::Read(page) => render_read(&page),
        FileOutcome::Listing(listing) => render_listing(listing),
        // A marker, not a blank page: zero matches must stay distinguishable from
        // grep matching an empty line.
        FileOutcome::NoMatches => "[grep: no matches]".to_string(),
        FileOutcome::Wrote { path, bytes } => format!("wrote {bytes} bytes to {path}"),
        FileOutcome::Appended { path, bytes } => format!("appended {bytes} bytes to {path}"),
        FileOutcome::Deleted { path } => format!("deleted {path}"),
        FileOutcome::Moved { src, dest } => format!("moved {src} -> {dest}"),
    }
}

fn render_read(page: &ReadPage) -> String {
    if page.lines.is_empty() {
        // Both an empty file and a cursor past the end used to read as "" —
        // indistinguishable from a file holding one empty line.
        return match page.end {
            PageEnd::Eof { total: 0 } => "[empty file]".to_string(),
            PageEnd::Eof { total } | PageEnd::More { at_least: total } => format!(
                "[cursor {} is past the end — the file has {total} lines]",
                page.cursor
            ),
        };
    }
    let body = page
        .lines
        .iter()
        .map(|line| match line.dropped {
            0 => line.text.clone(),
            n => format!("{}…[+{n} bytes]", line.text),
        })
        .collect::<Vec<_>>()
        .join("\n");
    match page.end {
        PageEnd::Eof { .. } => body,
        PageEnd::More { at_least } => format!(
            "{body}\n[lines {}..{} of ≥{at_least}; next_cursor={}]",
            page.cursor, page.next, page.next
        ),
    }
}

fn render_listing(listing: Listing) -> String {
    let mut out = listing.text;
    if listing.partial {
        out.push_str(
            "\n[exited with errors — some entries were unreadable; the results above are partial]",
        );
    }
    if listing.truncated {
        out.push_str(&format!(
            "\n[output truncated at {MAX_SHELL_OUTPUT_BYTES} bytes — narrow the path or pattern]"
        ));
    }
    out
}

/// The message a failed file op reads as: what failed, on which path, and the
/// next step where there is one.
pub(super) fn render_file_error(e: &FileError) -> String {
    match e {
        FileError::IsDirectory { path } => {
            format!("{path} is a directory — use file(action=\"list\", path=\"{path}\") instead")
        }
        FileError::NotRegularFile { path } => format!(
            "{path} is not a regular file (a device, FIFO or socket) — read only pages regular files"
        ),
        FileError::ReadTimedOut { .. } => {
            format!("{e} — read from a smaller cursor, or grep for what you need")
        }
        FileError::DirectoryNotEmpty { path } => format!(
            "{path} is a non-empty directory — nothing was deleted; pass recursive=true to delete it and everything under it"
        ),
        FileError::GrepDirectory { path } => {
            format!("{path} is a directory — pass recursive=true to search under it")
        }
        FileError::SourceMissing { .. } | FileError::MoveIntoItself { .. } => {
            format!("{e} — nothing was moved")
        }
        FileError::DestinationExists { dest } => {
            format!("destination exists: {dest} — move won't overwrite; choose another path")
        }
        // Never suggest deleting the directory: following that advice removed
        // whole trees. Moving *into* it is what the agent almost always meant.
        FileError::DestinationIsDirectory { src, dest } => {
            let name = Path::new(src)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<name>".to_string());
            let into = Path::new(dest).join(name);
            format!(
                "destination is a directory: {dest} — move won't overwrite it; to move into it, pass the full path, e.g. dest=\"{}\"",
                into.display()
            )
        }
        FileError::CrossDeviceUnsupported { .. } => {
            format!("{e} — use `mv` through the bash tool for directories and links")
        }
        FileError::Shell {
            op,
            path,
            source: ShError::Timeout { secs, out },
        } => format!("{out}\n[{op} {path}: timed out after {secs}s — narrow the path or pattern]"),
        // The op, path and errno / exit status already say it all.
        FileError::Io { .. } | FileError::Shell { .. } => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::files::ReadLine;

    fn page(lines: &[(&str, usize)], cursor: usize, end: PageEnd) -> ReadPage {
        let lines: Vec<ReadLine> = lines
            .iter()
            .map(|&(text, dropped)| ReadLine {
                text: text.to_string(),
                dropped,
            })
            .collect();
        let next = cursor + lines.len();
        ReadPage {
            lines,
            cursor,
            next,
            end,
        }
    }

    #[test]
    fn read_pages_render_footer_only_when_more_remains() {
        let last = page(&[("a", 0), ("b", 0)], 0, PageEnd::Eof { total: 2 });
        assert_eq!(render_read(&last), "a\nb");
        let more = page(&[("a", 0)], 3, PageEnd::More { at_least: 5 });
        assert_eq!(render_read(&more), "a\n[lines 3..4 of ≥5; next_cursor=4]");
    }

    #[test]
    fn empty_file_and_past_end_are_distinct_markers() {
        assert_eq!(
            render_read(&page(&[], 0, PageEnd::Eof { total: 0 })),
            "[empty file]"
        );
        let past = render_read(&page(&[], 9, PageEnd::Eof { total: 3 }));
        assert!(
            past.contains("past the end") && past.contains("3 lines"),
            "{past}"
        );
    }

    #[test]
    fn long_line_carries_one_marker_with_the_true_count() {
        let out = render_read(&page(&[("head", 70_000)], 0, PageEnd::Eof { total: 1 }));
        assert_eq!(out, "head…[+70000 bytes]");
    }

    #[test]
    fn listing_notes_partial_and_truncated() {
        let out = render_listing(Listing {
            text: "a".into(),
            truncated: true,
            partial: true,
        });
        assert!(out.starts_with("a\n"));
        assert!(
            out.contains("partial") && out.contains("truncated at"),
            "{out}"
        );
    }

    #[test]
    fn errors_name_op_and_path_and_never_advise_deleting_a_dir() {
        let out = render_file_error(&FileError::DestinationIsDirectory {
            src: "/tmp/app.tar".into(),
            dest: "/srv/releases".into(),
        });
        assert!(!out.contains("delete"), "{out}");
        assert!(out.contains("dest=\"/srv/releases/app.tar\""), "{out}");

        let out = render_file_error(&FileError::Io {
            op: "read",
            path: "/nope".into(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        });
        assert!(out.starts_with("read /nope: "), "{out}");

        let out = render_file_error(&FileError::Shell {
            op: "list",
            path: "/big".into(),
            source: ShError::Timeout {
                secs: 30,
                out: "partial".into(),
            },
        });
        assert!(
            out.starts_with("partial\n") && out.contains("narrow the path"),
            "{out}"
        );
    }
}
