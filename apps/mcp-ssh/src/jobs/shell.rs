//! How a user command is launched — see `Shell` below.

/// How a user command is launched. The default is a bare `sh -c`; a `bash` call
/// that opts into `interactive` instead gets an interactive bash that sources
/// the service user's `~/.bashrc` — so aliases and version managers
/// (mise/nvm/rbenv) resolve, matching a real shell.
///
/// `program` plus `args` form the launcher prefix; `run` appends the (wrapped)
/// command string as the final argument.
#[derive(Debug, Clone)]
pub struct Shell {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
}

impl Shell {
    /// Bare `sh -c` — the default, fast path: no rc files, no per-call startup
    /// cost. Output never depends on the host's shell config.
    pub fn sh() -> Self {
        Self {
            program: "sh".into(),
            args: vec!["-c".into()],
        }
    }

    /// Interactive bash. `-i` sources `~/.bashrc`, where aliases and version
    /// managers live behind its `case $- in *i*) ;; *) return;; esac`
    /// non-interactive guard — so commands see the same environment an
    /// interactive shell does. Opt-in per call (`bash` tool's `interactive`
    /// flag) because sourcing `~/.bashrc` adds startup cost. Startup job-control
    /// warnings (no controlling TTY under systemd) are discarded by the
    /// exec-redirect in `run`.
    pub fn interactive_bash() -> Self {
        Self {
            program: "bash".into(),
            args: vec!["-ic".into()],
        }
    }
}

#[cfg(test)]
impl Shell {
    /// Interactive bash pinned to a specific rc file — lets a test prove alias
    /// resolution against a controlled rc instead of the host's `~/.bashrc`.
    pub(crate) fn bash_with_rcfile(rcfile: &str) -> Self {
        Self {
            program: "bash".into(),
            args: vec!["--rcfile".into(), rcfile.into(), "-ic".into()],
        }
    }
}
