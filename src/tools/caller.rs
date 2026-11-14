//! Who is calling: the agent behind a tool call, and how its jobs read to others.
use rmcp::model::Extensions;

use super::Tools;
use crate::agent::Agent;
use crate::jobs::JobId;

/// The agent this call came from: `auth::require_auth` puts it on the HTTP
/// request, and rmcp hands the request's parts to the tool. `None` only off the
/// HTTP path (in-process tests).
pub(super) fn caller(extensions: &Extensions) -> Option<Agent> {
    extensions
        .get::<axum::http::request::Parts>()?
        .extensions
        .get::<Agent>()
        .cloned()
}

/// The span's `agent` field: a label (a cleaned name + hash tag), never an id.
pub(super) fn label(agent: Option<&Agent>) -> String {
    agent.map_or_else(|| "-".to_string(), Agent::label)
}

impl Tools {
    /// The label of the agent that started job `id`, when that is someone other
    /// than `caller` — one host, one service user, so another agent's job stays
    /// pollable and killable, but the reply says whose it is. A failed lookup
    /// only loses the note.
    pub(super) async fn foreign_owner(&self, id: &JobId, caller: Option<&Agent>) -> Option<String> {
        match self.jobs.owner(id).await {
            Ok(Some(owner)) if caller != Some(&owner) => Some(owner.label()),
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(%error, "could not read the job's owner");
                None
            }
        }
    }
}
