//! Guard for the public tree: this repository is public, so every tracked file
//! is a public document (issue #47). None of the rules is about a credential;
//! each is about a fact of the organisation's own infrastructure that a
//! stranger has no use for and nothing here needs.
//!
//! - **No internal host on our own domain.** Only the public site may be named.
//! - **No name of the setup mode that wrote the agent files.**
//! - **No private word.** The CI provider of the organisation's private
//!   repositories, a private product. A committed list publishes what it
//!   guards, even assembled from parts, so these come from the
//!   `PUBLIC_TREE_PRIVATE_WORDS` CI secret. A run without it (a fork's pull
//!   request, a local run) says it skipped them; this repository's own CI runs
//!   set `PUBLIC_TREE_PRIVATE_WORDS_REQUIRED=1`, where an empty secret fails.
//! - **No private repository.** A reference to an organisation repository
//!   names one on the public allowlist below.
//! - **No one's machine.** A path under a home directory names a person.
//!
//! The setup mode's name is already public, so it is assembled from parts. A
//! failure names the file and the rule, never the matched text: CI logs are
//! public. Git is the oracle for "tracked". `.dz/` is excluded: the platform
//! rewrites it whole. `.mcp.json` is scanned: the platform's onboarding merges
//! its managed servers into it, our own MCP service among them, so a commit
//! that adds one back fails here on purpose. The `fires_on_*` tests prove each
//! rule can fail, and a test pins CI's exact wiring of the secret.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use regex::Regex;

/// This file, repo-relative: the sentinel that proves the scan read the tree
/// it meant to.
const SELF: &str = "tests/public_tree.rs";

/// The fewest tracked files a real checkout of this repository has. A scan
/// that reads fewer read the wrong tree or a sparse one.
const MIN_TRACKED: usize = 100;

/// Rewritten whole by the platform; see the module docs. An entry ending in
/// `/` is a directory prefix, any other entry one exact path.
const PLATFORM_OWNED: &[&str] = &[".dz/"];

/// The CI secret holding the private words, comma- or newline-separated.
const PRIVATE_WORDS_ENV: &str = "PUBLIC_TREE_PRIVATE_WORDS";

/// Set to `1` where the secret must be there (this repository's own CI runs):
/// an empty secret there is a failure, never a skip.
const PRIVATE_WORDS_REQUIRED_ENV: &str = "PUBLIC_TREE_PRIVATE_WORDS_REQUIRED";

/// The organisation's repositories a tracked file may name: public ones.
const PUBLIC_REPOS: &[&str] = &[
    "ai-army",
    "ai-designer",
    "ai-gaming-agent-mcp",
    "ai-task-master",
    "bun-on-rails",
    "cairn",
    "claude-and-conquer",
    "claude-task-master",
    "claudes",
    "db-mcp-gateway",
    "fast_rspec",
    "gold-standards-in-ai",
    "magik",
    "mcp-ssh",
    "mini",
    "multi-ai-router",
    "nexus-engine",
    "open-mmorpg",
    "open-remote-ssh-tmux",
    "research",
    "tessera",
    "ts23d",
    "ui-debugger-mcp",
    "ultimate",
    "universal-lsp",
    "wurk",
];

/// The hosts on our own domain a tracked file may name.
const PUBLIC_HOSTS: &[&str] = &["www.developerz.ai"];

static OWN_HOST: LazyLock<Regex> =
    LazyLock::new(|| compiled(r"(?i)\b(?:[a-z0-9-]+\.)+developerz\.ai\b"));

static ORG_REPO: LazyLock<Regex> =
    LazyLock::new(|| compiled(r"(?i)\bdeveloperz-ai/([a-z0-9_.-]+)"));

/// Home directories that name no person: this package's own system user and
/// CI's and the docs' generic placeholders.
const SERVICE_HOMES: &[&str] = &["mcp-ssh", "runner", "user"];

static HOME_PATH: LazyLock<Regex> =
    LazyLock::new(|| compiled(r"(?:^|[^\w.-])/(?:home|Users)/([\w.-]+)"));

/// Words that must not appear anywhere, case-insensitively, each built from
/// parts: the setup mode. Everything else private comes from the secret.
static FORBIDDEN_WORDS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [("setup-mode name", ["dz", "runner"].join("-"))]
        .into_iter()
        .map(|(rule, word)| (rule, compiled(&format!("(?i){}", regex::escape(&word)))))
        .collect()
});

/// The private words, parsed from the secret's value: comma- or
/// newline-separated, trimmed, empty entries dropped, each matched as a
/// literal, case-insensitively. Unset or blank is none.
fn private_words(raw: Option<&str>) -> Vec<Regex> {
    raw.unwrap_or_default()
        .split([',', '\n'])
        .map(str::trim)
        .filter(|word| !word.is_empty())
        .map(|word| compiled(&format!("(?i){}", regex::escape(word))))
        .collect()
}

/// The private words for this run, or why the run must fail: required, and
/// the secret gave none.
fn private_words_for_run(raw: Option<&str>, required: bool) -> Result<Vec<Regex>, String> {
    let words = private_words(raw);
    if words.is_empty() && required {
        return Err(format!(
            "{PRIVATE_WORDS_ENV} is empty, and {PRIVATE_WORDS_REQUIRED_ENV}=1 says this run must check the private words"
        ));
    }
    Ok(words)
}

/// Whether `content` carries any of the private words. Never which one.
fn names_private_word(content: &str, words: &[Regex]) -> bool {
    words.iter().any(|word| word.is_match(content))
}

fn compiled(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|err| panic!("guard pattern must compile: {err}"))
}

/// The names of every rule `content` breaks. Never the matched text.
fn violations(_path: &str, content: &str) -> Vec<&'static str> {
    let mut broken = Vec::new();
    let names_internal_host = OWN_HOST
        .find_iter(content)
        .any(|m| !PUBLIC_HOSTS.contains(&m.as_str().to_ascii_lowercase().as_str()));
    if names_internal_host {
        broken.push("internal host on our own domain");
    }
    broken.extend(
        FORBIDDEN_WORDS
            .iter()
            .filter(|(_, word)| word.is_match(content))
            .map(|(rule, _)| *rule),
    );
    let names_private_repo = ORG_REPO.captures_iter(content).any(|caps| {
        // GitHub owner and repository names are case-insensitive.
        let name = caps[1].to_ascii_lowercase();
        let name = name.trim_end_matches('.');
        let name = name.strip_suffix(".git").unwrap_or(name);
        !PUBLIC_REPOS.contains(&name)
    });
    if names_private_repo {
        broken.push("private repository");
    }
    if HOME_PATH
        .captures_iter(content)
        .any(|caps| !SERVICE_HOMES.contains(&&caps[1]))
    {
        broken.push("home directory path");
    }
    broken
}

fn platform_owned(path: &str) -> bool {
    PLATFORM_OWNED.iter().any(|owned| {
        if owned.ends_with('/') {
            path.starts_with(owned)
        } else {
            path == *owned
        }
    })
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|err| panic!("git {args:?} must run: {err}"));
    assert!(
        out.status.success(),
        "git {args:?} exited {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    out.stdout
}

/// The checkout root, confirmed by git itself: a checkout nested in another
/// repository would have git answer with the outer tree's index.
fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let toplevel = git(&manifest, &["rev-parse", "--show-toplevel"]);
    let toplevel = PathBuf::from(String::from_utf8_lossy(&toplevel).trim());
    let canonical = |p: &Path| {
        p.canonicalize()
            .unwrap_or_else(|err| panic!("{} must resolve: {err}", p.display()))
    };
    assert_eq!(
        canonical(&toplevel),
        canonical(&manifest),
        "git answers for another repository; the scan would read a tree that is not this one"
    );
    manifest
}

fn tracked_files(root: &Path) -> Vec<String> {
    String::from_utf8_lossy(&git(root, &["ls-files", "-z"]))
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn no_tracked_file_carries_internal_infrastructure() {
    let root = repo_root();
    let tracked = tracked_files(&root);
    assert!(
        tracked.iter().any(|path| path == SELF),
        "git ls-files does not list {SELF}: the scan is not reading this tree"
    );

    let required = std::env::var(PRIVATE_WORDS_REQUIRED_ENV).is_ok_and(|v| v == "1");
    let private = private_words_for_run(std::env::var(PRIVATE_WORDS_ENV).ok().as_deref(), required)
        .unwrap_or_else(|why| panic!("{why}"));
    if private.is_empty() {
        eprintln!("public-tree guard: {PRIVATE_WORDS_ENV} is unset, private words not checked");
    }

    let mut read = 0;
    let mut failures = Vec::new();
    for path in &tracked {
        if platform_owned(path) {
            continue;
        }
        let full = root.join(path);
        if full.is_dir() {
            continue; // a submodule entry, not a file
        }
        let bytes =
            std::fs::read(&full).unwrap_or_else(|err| panic!("tracked {path} must read: {err}"));
        read += 1;
        let content = String::from_utf8_lossy(&bytes);
        let mut rules = violations(path, &content);
        if names_private_word(&content, &private) {
            rules.push("private word (from the secret)");
        }
        failures.extend(rules.into_iter().map(|rule| format!("{path}: {rule}")));
    }

    eprintln!(
        "public-tree guard read {read} of {} tracked files, {} private words",
        tracked.len(),
        private.len()
    );
    assert!(
        read >= MIN_TRACKED,
        "read only {read} tracked files; expected at least {MIN_TRACKED}"
    );
    assert!(
        failures.is_empty(),
        "tracked files carry internal infrastructure (rule per file, value withheld):\n{}",
        failures.join("\n")
    );
}

#[test]
fn passes_public_references() {
    let content = "See https://www.developerz.ai, email security@developerz.ai, \
                   clone developerz-ai/mcp-ssh.git, cache under ~/.cargo/registry.";
    assert_eq!(violations("README.md", content), Vec::<&str>::new());
}

#[test]
fn skips_only_platform_owned_paths() {
    assert!(platform_owned(".dz/maintainer/maintainer.yml"));
    for path in [
        ".mcp.json",
        ".mcp.json.bak",
        "config/.mcp.json",
        ".dzz/x",
        "README.md",
    ] {
        assert!(!platform_owned(path), "{path} must be scanned");
    }
}

#[test]
fn fires_on_internal_host() {
    let host = ["svc", "developerz", "ai"].join(".");
    assert_eq!(
        violations("a.md", &format!("url: https://{host}/v1")),
        ["internal host on our own domain"]
    );
}

#[test]
fn fires_on_the_setup_mode_name() {
    let word = ["DZ", "RUNNER"].join("-");
    assert_eq!(
        violations("a.md", &format!("x {word} y")),
        ["setup-mode name"]
    );
}

#[test]
fn fires_on_a_private_word_from_the_secret() {
    let words = private_words(Some(" Quimtel ,\n\nzorblax.io\n"));
    assert_eq!(words.len(), 2);
    assert!(names_private_word("x QUIMTEL y", &words));
    assert!(names_private_word("see zorblax.io.", &words));
    assert!(!names_private_word("zorblax-io and quim", &words));
    assert!(private_words(None).is_empty());
    assert!(private_words(Some(" , \n ")).is_empty());
}

#[test]
fn a_required_run_without_the_secret_fails() {
    assert!(private_words_for_run(None, true).is_err());
    assert!(private_words_for_run(Some(" , \n"), true).is_err());
    assert!(matches!(private_words_for_run(None, false), Ok(words) if words.is_empty()));
    assert!(matches!(private_words_for_run(Some("quimtel"), true), Ok(words) if words.len() == 1));
}

#[test]
fn fires_on_our_own_mcp_service_in_the_tracked_config() {
    let host = ["mcp", "developerz", "ai"].join(".");
    let config =
        format!(r#"{{"mcpServers":{{"x":{{"type":"http","url":"https://{host}/v1/mcp"}}}}}}"#);
    assert!(!platform_owned(".mcp.json"));
    assert_eq!(
        violations(".mcp.json", &config),
        ["internal host on our own domain"]
    );
}

#[test]
fn fires_on_private_repository() {
    for repo in [["developerz-ai", "infra"], ["Developerz-AI", "Infra"]] {
        let repo = repo.join("/");
        assert_eq!(
            violations("a.md", &format!("see `{repo}`.")),
            ["private repository"]
        );
    }
    let public = ["Developerz-AI", "MCP-SSH"].join("/");
    assert!(violations("a.md", &public).is_empty());
}

#[test]
fn fires_on_home_directory() {
    let path = ["", "home", "someone", "src"].join("/");
    assert_eq!(
        violations("a.md", &format!("cd {path}")),
        ["home directory path"]
    );
    let service = ["", "home", "mcp-ssh"].join("/");
    assert!(violations("postinst", &format!("adduser --home {service}")).is_empty());
}

#[test]
fn ci_passes_the_secret_and_the_required_flag() {
    // Deleting, commenting out or weakening either line would switch the
    // private-word rule off with every test still green, so the live
    // (uncommented) lines are matched exactly.
    let ci = std::fs::read_to_string(repo_root().join(".github/workflows/ci.yml"))
        .unwrap_or_else(|err| panic!("ci.yml must read: {err}"));
    let live: Vec<&str> = ci
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .collect();
    let secret = format!("{PRIVATE_WORDS_ENV}: ${{{{ secrets.{PRIVATE_WORDS_ENV} }}}}");
    let required = format!(
        "{PRIVATE_WORDS_REQUIRED_ENV}: ${{{{ (github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name == github.repository) && github.actor != 'dependabot[bot]' && '1' || '' }}}}"
    );
    assert!(
        live.contains(&secret.as_str()),
        "ci.yml does not pass {PRIVATE_WORDS_ENV}"
    );
    assert!(
        live.contains(&required.as_str()),
        "ci.yml does not set {PRIVATE_WORDS_REQUIRED_ENV} exactly"
    );
}
