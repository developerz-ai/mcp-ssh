//! File operations, executed locally as the service user. Read/write/move go
//! through `tokio::fs`; list/grep shell out to `ls`/`find`/`grep` rather than
//! reimplementing them — bounded by the runner in [`shell`].
//!
//! Ops return *facts* — a [`FileOutcome`] or a typed [`FileError`] — never a
//! finished sentence. The agent-facing wording, and every hint that names the
//! `file` tool's own actions, is rendered by the adapter in [`super`].
use tokio::fs;

mod read;
mod remove;
mod rename;
mod shell;
mod write;

pub use read::read;
pub use remove::delete;
pub use rename::rename;
pub use write::{append, write};

#[cfg(test)]
use read::MAX_READ_LINE_BYTES;
pub(in crate::tools) use shell::{MAX_SHELL_OUTPUT_BYTES, ShError};
use shell::{ShOutput, sh};

/// What a file op produced. The text-producing ops carry their already-bounded
/// output as structure (lines, flags); the mutating ops carry only *what
/// happened*. Every sentence describing either stays in the tool adapter.
#[derive(Debug)]
pub enum FileOutcome {
    Read(ReadPage),
    /// `ls`/`find`/`grep` output.
    Listing(Listing),
    /// `grep` matched nothing — a result, not an error, and its own variant so it
    /// can't be confused with matching an empty line.
    NoMatches,
    Wrote {
        path: String,
        bytes: usize,
    },
    Appended {
        path: String,
        bytes: usize,
    },
    Deleted {
        path: String,
    },
    Moved {
        src: String,
        dest: String,
    },
}

/// One `read` page: lines `[cursor, next)` and what lies past them.
#[derive(Debug)]
pub struct ReadPage {
    pub lines: Vec<ReadLine>,
    pub cursor: usize,
    pub next: usize,
    pub end: PageEnd,
}

/// One line of a `read` page. `dropped` counts the bytes cut from an over-long
/// line's tail (0 = whole line), so the marker can state the true loss.
#[derive(Debug, PartialEq, Eq)]
pub struct ReadLine {
    pub text: String,
    pub dropped: usize,
}

/// Where a `read` page stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum PageEnd {
    /// The scan reached end of file: `total` is the file's exact line count.
    Eof { total: usize },
    /// The page filled first; at least `at_least` lines exist. Counting the rest
    /// would walk a multi-GB file to answer page 1 — what the cursor avoids.
    More { at_least: usize },
}

/// A shelled-out listing. `truncated`: the byte cap cut it. `partial`: the tool
/// exited non-zero (an unreadable subdir) but still produced results — success
/// with a caveat, not a failure.
#[derive(Debug)]
pub struct Listing {
    pub text: String,
    pub truncated: bool,
    pub partial: bool,
}

/// Why a file op couldn't produce a result. Variants stay structured so the
/// adapter can phrase a next step, and every one names the path it concerns.
/// A failed `ls`/`find`/`grep` keeps its typed [`ShError`] — grep's
/// exit-1-means-no-matches lives on the code, not on message text.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("{op} {path}: {source}")]
    Io {
        op: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is a directory")]
    IsDirectory { path: String },
    /// A device, FIFO or socket: reading one can block forever or never end.
    #[error("{path} is not a regular file")]
    NotRegularFile { path: String },
    #[error("read {path}: timed out after {secs}s")]
    ReadTimedOut { path: String, secs: u64 },
    #[error("{path} is a non-empty directory")]
    DirectoryNotEmpty { path: String },
    #[error("grep {path}: is a directory")]
    GrepDirectory { path: String },
    #[error("move {src}: no such file or directory")]
    SourceMissing { src: String },
    #[error("move {src} -> {dest}: destination is inside the source")]
    MoveIntoItself { src: String, dest: String },
    #[error("destination exists: {dest}")]
    DestinationExists { dest: String },
    #[error("move {src} -> {dest}: destination is a directory")]
    DestinationIsDirectory { src: String, dest: String },
    #[error("move {src} -> {dest}: crosses filesystems and is not a regular file")]
    CrossDeviceUnsupported { src: String, dest: String },
    #[error("{op} {path}: {source}")]
    Shell {
        op: &'static str,
        path: String,
        #[source]
        source: ShError,
    },
}

/// `map_err` adapter that attaches the op and path an `io::Error` happened on —
/// a bare errno ("No such file or directory") tells the agent nothing about which
/// of its paths failed.
fn io_err(op: &'static str, path: &str) -> impl FnOnce(std::io::Error) -> FileError {
    let path = path.to_string();
    move |source| FileError::Io { op, path, source }
}

/// Deepest a recursive `list` (`find`) descends below its starting point. The
/// streamed byte cap already bounds the *output*, but a pathologically deep tree
/// (or a bind-mount / hardlink cycle) could still drive `find` far down before it
/// emits cap-worth of text; `-maxdepth` bounds the walk itself. 20 clears any real
/// source tree while keeping a runaway descent finite.
const MAX_FIND_DEPTH: u32 = 20;

/// Create the target's parent directories so writing a new file under a fresh
/// path "just works" (like `mkdir -p` before a redirect), instead of failing with
/// a bare `ENOENT` the agent then has to diagnose. A no-op when the parent already
/// exists or the path has none (a bare filename in the cwd).
async fn ensure_parent(path: &str) -> Result<(), FileError> {
    if let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .await
            .map_err(io_err("create parent directory of", path))?;
    }
    Ok(())
}

pub async fn list(path: &str, recursive: bool) -> Result<FileOutcome, FileError> {
    let run = if recursive {
        // `find` has no `--`: an argument starting with `-`, `(`, `)`, `,` or `!`
        // parses as an expression, not a starting point. Anchor such a relative
        // path with `./`.
        let path = if path.starts_with(['-', '(', ')', ',', '!']) {
            format!("./{path}")
        } else {
            path.to_string()
        };
        // `-maxdepth` (after the starting point, before any test → no find warning)
        // caps how far the walk descends, so a deep tree can't recurse without bound.
        let depth = MAX_FIND_DEPTH.to_string();
        sh("find", &[&path, "-maxdepth", &depth]).await
    } else {
        sh("ls", &["-la", "--", path]).await
    };
    listing(run, "list", path)
}

pub async fn grep(pattern: &str, path: &str, recursive: bool) -> Result<FileOutcome, FileError> {
    // Without -r, grep on a directory prints "Is a directory" and exits 2; say
    // so typed, so the adapter can point at `recursive=true`.
    if !recursive
        && let Ok(meta) = fs::metadata(path).await
        && meta.is_dir()
    {
        return Err(FileError::GrepDirectory {
            path: path.to_string(),
        });
    }
    let flag = if recursive { "-rn" } else { "-n" };
    // `--` so a pattern like `->` or `-r` is a pattern, not an option: without
    // it, grepping Rust code for `->` errored, and `-r` silently recursed with
    // the *path* as the pattern — wrong results, not even an error.
    match sh("grep", &[flag, "--", pattern, path]).await {
        // Exit 1 is grep's "no line matched" — a legitimate empty result, so it
        // leaves as an outcome rather than an error.
        Err(ShError::Status { code: 1, out, .. }) if out.is_empty() => Ok(FileOutcome::NoMatches),
        run => listing(run, "grep", path),
    }
}

/// Classify a shelled-out run: a clean exit is a listing; a non-zero exit that
/// still wrote results to stdout (one unreadable subdir in a big tree) is a
/// *partial* listing — failing the whole call would throw the results away; any
/// other failure is an error naming the op and path.
fn listing(
    run: Result<ShOutput, ShError>,
    op: &'static str,
    path: &str,
) -> Result<FileOutcome, FileError> {
    match run {
        Ok(ShOutput { text, truncated }) => Ok(FileOutcome::Listing(Listing {
            text,
            truncated,
            partial: false,
        })),
        Err(ShError::Status {
            out,
            wrote_stdout: true,
            ..
        }) => Ok(FileOutcome::Listing(Listing {
            text: out,
            truncated: false,
            partial: true,
        })),
        Err(source) => Err(FileError::Shell {
            op,
            path: path.to_string(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests;
