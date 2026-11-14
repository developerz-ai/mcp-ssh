//! Guards against drift between `src/` and CLAUDE.md's "## Module map", the
//! repo's stated navigation aid, and enforces the file-size rule.
//!
//! Rules:
//! - Every backticked `src/...` path anywhere in the module-map section exists.
//! - Every non-test `.rs` file under `src/` is named, as an exact file path, in the
//!   first cell of some row. A directory row (`src/oauth/`) does NOT cover the
//!   files under it: each file gets its own row, so the map says what each owns.
//! - A test file is `tests.rs` or `*_tests.rs` (a module's extracted tests); those
//!   need no row and have no size limit.
//! - No non-test file exceeds `MAX_LOC` lines of non-test code: lines before an
//!   inline `#[cfg(test)] mod tests { … }` block don't count it; tests belong in a
//!   sibling `tests.rs` anyway.
use std::fs;
use std::path::{Path, PathBuf};

const MAX_LOC: usize = 300;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The "## Module map" section of CLAUDE.md, up to the next `## ` heading.
fn module_map_section() -> String {
    let claude = fs::read_to_string(root().join("CLAUDE.md")).expect("read CLAUDE.md");
    let start = claude
        .find("\n## Module map")
        .expect("CLAUDE.md has a `## Module map` section");
    let body = &claude[start + 1..];
    let end = body[3..].find("\n## ").map_or(body.len(), |i| i + 3);
    body[..end].to_string()
}

/// Every backticked token in `text` that starts with `src/`.
fn src_paths(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|token| token.starts_with("src/"))
        .map(str::to_string)
        .collect()
}

/// The paths named in each table row's first cell.
fn row_paths(section: &str) -> Vec<String> {
    section
        .lines()
        .filter_map(|line| line.strip_prefix('|'))
        .filter_map(|rest| rest.split('|').next())
        .flat_map(src_paths)
        .collect()
}

fn is_test_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name == "tests.rs" || name.ends_with("_tests.rs")
}

/// Every non-test `.rs` file under `src/`, as a `src/...` path with `/`.
fn source_files() -> Vec<String> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") && !is_test_file(&path) {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&root().join("src"), &mut files);
    let mut names: Vec<String> = files
        .iter()
        .map(|p| {
            p.strip_prefix(root())
                .expect("under the crate root")
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    names.sort();
    names
}

/// Lines of non-test code: everything before an inline `mod tests { … }` block
/// gated by a `#[cfg(…test…)]` attribute.
fn non_test_loc(source: &str) -> usize {
    let lines: Vec<&str> = source.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let line = line.trim();
        if !(line.starts_with("#[cfg(") && line.contains("test")) {
            continue;
        }
        let item = lines[i + 1..]
            .iter()
            .map(|l| l.trim())
            .find(|l| !l.starts_with("#["));
        if item.is_some_and(|l| l.starts_with("mod tests {")) {
            return i;
        }
    }
    lines.len()
}

#[test]
fn every_path_in_the_module_map_exists() {
    let missing: Vec<String> = src_paths(&module_map_section())
        .into_iter()
        .filter(|p| !root().join(p).exists())
        .collect();
    assert!(
        missing.is_empty(),
        "CLAUDE.md module map names paths that don't exist: {missing:?}"
    );
}

#[test]
fn every_source_file_has_a_module_map_row() {
    let named = row_paths(&module_map_section());
    let unnamed: Vec<String> = source_files()
        .into_iter()
        .filter(|f| !named.contains(f))
        .collect();
    assert!(
        unnamed.is_empty(),
        "source files with no row in CLAUDE.md's module map: {unnamed:?}"
    );
}

#[test]
fn no_source_file_exceeds_the_loc_limit() {
    let over: Vec<String> = source_files()
        .into_iter()
        .filter_map(|f| {
            let source = fs::read_to_string(root().join(&f)).expect("read source file");
            let loc = non_test_loc(&source);
            (loc > MAX_LOC).then(|| format!("{f}: {loc}"))
        })
        .collect();
    assert!(
        over.is_empty(),
        "non-test code over {MAX_LOC} lines (move tests to a sibling tests.rs, or split by responsibility): {over:?}"
    );
}

#[test]
fn non_test_loc_stops_at_an_inline_test_module() {
    let source = "fn a() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
    assert_eq!(non_test_loc(source), 2);
    let gated = "fn a() {}\n#[cfg(all(test, unix))]\n#[allow(dead_code)]\nmod tests {\n}\n";
    assert_eq!(non_test_loc(gated), 1);
    let extracted = "fn a() {}\n#[cfg(test)]\nmod tests;\n";
    assert_eq!(non_test_loc(extracted), 3);
}
