//! `ProcessGroupId`: the only way a group id reaches a signal.

/// Process group id to signal on kill. Holds only `2..=i32::MAX`: `0` is the
/// caller's own group (the server itself), `1` would be `kill(-1)` — every
/// process the service user may signal — and anything past `i32` would wrap onto
/// a real, wrong group. Every path into a signal goes through this type, so no
/// guard can be forgotten at a call site.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProcessGroupId(libc::pid_t);

impl ProcessGroupId {
    /// The group led by a live child (it leads its own, so pgid == pid).
    pub(super) fn new(pid: u32) -> Option<Self> {
        libc::pid_t::try_from(pid).ok().and_then(Self::checked)
    }

    /// The group behind a persisted `jobs.pgid`. `None` for anything a real job
    /// could never have written, so a corrupt row can't be signalled. Private to
    /// this module tree: outside it, a kill goes through [`super::signal::kill_persisted`].
    pub(super) fn from_persisted(pgid: i64) -> Option<Self> {
        libc::pid_t::try_from(pgid).ok().and_then(Self::checked)
    }

    fn checked(pid: libc::pid_t) -> Option<Self> {
        (pid >= 2).then_some(Self(pid))
    }

    pub(crate) fn get(self) -> libc::pid_t {
        self.0
    }
}
