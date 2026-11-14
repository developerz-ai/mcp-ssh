//! Public-tree guard: this repository is public, so no tracked file may carry a
//! fact about our own infrastructure (issue #47).
//!
//! Rules:
//! - The scan covers `git ls-files`, minus the platform-owned `.dz/`.
//! - It proves it read itself and a plausible number of files, so an empty or
//!   truncated listing can't pass as a clean tree.
//! - Each forbidden value is assembled from parts, so this file never carries one.
//! - A failure names the file, never the value it matched: CI logs are public.
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const SELF: &str = "tests/public_tree.rs";
const MIN_FILES: usize = 50;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Lowercased forbidden values, each joined from parts.
fn forbidden() -> Vec<String> {
    [["ubi", "cloud"]]
        .iter()
        .map(|parts| parts.concat())
        .collect()
}

fn tracked_files() -> Vec<String> {
    let out = Command::new("git")
        .arg("ls-files")
        .current_dir(root())
        .output()
        .expect("run `git ls-files`");
    assert!(out.status.success(), "`git ls-files` failed");
    String::from_utf8(out.stdout)
        .expect("tracked paths are UTF-8")
        .lines()
        .filter(|path| !path.starts_with(".dz/"))
        .map(str::to_string)
        .collect()
}

/// Whether `bytes`, compared case-insensitively, contains any forbidden value.
fn carries_forbidden(bytes: &[u8], forbidden: &[String]) -> bool {
    let text = String::from_utf8_lossy(bytes).to_lowercase();
    forbidden.iter().any(|value| text.contains(value))
}

#[test]
fn no_tracked_file_carries_an_infrastructure_fact() {
    let files = tracked_files();
    assert!(
        files.iter().any(|path| path == SELF),
        "the listing does not include {SELF}: the guard did not read the real tree"
    );
    assert!(
        files.len() >= MIN_FILES,
        "only {} tracked files listed, expected at least {MIN_FILES}",
        files.len()
    );

    let forbidden = forbidden();
    let offenders: Vec<&String> = files
        .iter()
        // A tracked path can be absent from the working tree mid-edit; skip it.
        .filter_map(|path| fs::read(root().join(path)).ok().map(|bytes| (path, bytes)))
        .filter(|(_, bytes)| carries_forbidden(bytes, &forbidden))
        .map(|(path, _)| path)
        .collect();
    assert!(
        offenders.is_empty(),
        "tracked files carry an infrastructure fact (value withheld): {offenders:?}"
    );
}

#[test]
fn the_scan_matches_case_insensitively() {
    let forbidden = vec!["needle".to_string()];
    assert!(carries_forbidden(b"a NeEdLe here", &forbidden));
    assert!(!carries_forbidden(b"nothing here", &forbidden));
}
