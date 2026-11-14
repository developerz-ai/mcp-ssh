//! Who is using this host: the agent (OAuth client) behind a bearer, when each
//! client last used one, and which were active recently. One host, one service
//! user — this is awareness between agents, not isolation. Only ids, names and
//! timestamps are read; never a token.
use std::time::Duration;

use super::{Store, StoreUnavailable, live_token_client, secs, unavailable};
use crate::agent::Agent;
use crate::db::now_unix;

/// How recently a client must have used a bearer to count as active.
pub const PRESENCE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// A client's last-use time is rewritten at most this often (seconds), so a busy
/// agent's requests stay reads, not a write each.
const TOUCH_EVERY: i64 = 60;
/// Most agents one presence listing names. Only an operator-approved login can be
/// active, so this is far above any real count.
const MAX_LISTED: i64 = 50;

/// A client holding a live login, and when it last used a bearer (`None`: not
/// since this was tracked).
#[derive(Debug)]
pub struct AgentActivity {
    pub agent: Agent,
    pub last_used_unix: Option<i64>,
}

impl Store {
    /// The agent behind a live bearer (by hash), noting that it was just used.
    /// `None` when the token is unknown or expired.
    pub async fn authenticate(&self, token_hash: &str) -> Result<Option<Agent>, StoreUnavailable> {
        let hash = token_hash.to_owned(); // moves into the blocking DB call
        let now = now_unix();
        self.db
            .call(move |conn| {
                let Some(client_id) = live_token_client(conn, &hash, now)? else {
                    return Ok(None);
                };
                // Aggregates always yield one row: NULLs for a client with no
                // registration row (one swept after its token was issued).
                let (name, last_used): (Option<String>, Option<i64>) = conn.query_row(
                    "SELECT MAX(client_name), MAX(last_used_unix) FROM clients \
                     WHERE client_id = ?1",
                    [&client_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                if last_used.is_none_or(|at| now - at >= TOUCH_EVERY) {
                    conn.execute(
                        "UPDATE clients SET last_used_unix = ?1 WHERE client_id = ?2",
                        (now, &client_id),
                    )?;
                }
                Ok(Some(Agent::new(client_id, name.as_deref())))
            })
            .await
            .map_err(|e| unavailable("authenticate", e))
    }

    /// Agents that used a bearer within [`PRESENCE_WINDOW`], most recent first.
    pub async fn active_agents(&self) -> Result<Vec<Agent>, StoreUnavailable> {
        let since = now_unix().saturating_sub(secs(PRESENCE_WINDOW));
        self.db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT client_id, MAX(client_name) FROM clients \
                     WHERE last_used_unix >= ?1 GROUP BY client_id \
                     ORDER BY MAX(last_used_unix) DESC LIMIT ?2",
                )?;
                stmt.query_map((since, MAX_LISTED), |r| {
                    Ok(Agent::new(
                        r.get(0)?,
                        r.get::<_, Option<String>>(1)?.as_deref(),
                    ))
                })?
                .collect()
            })
            .await
            .map_err(|e| unavailable("active_agents", e))
    }

    /// Every client holding a live access or refresh token, most recently used
    /// first — the `mcp-ssh sessions` view.
    pub async fn logged_in_agents(&self) -> Result<Vec<AgentActivity>, StoreUnavailable> {
        let now = now_unix();
        self.db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT client_id, MAX(client_name), MAX(last_used_unix) FROM clients \
                     WHERE client_id IN ( \
                         SELECT client_id FROM access_tokens WHERE expires_unix > ?1 \
                         UNION SELECT client_id FROM refresh_tokens WHERE expires_unix > ?1) \
                     GROUP BY client_id ORDER BY MAX(last_used_unix) DESC NULLS LAST",
                )?;
                stmt.query_map([now], |r| {
                    Ok(AgentActivity {
                        agent: Agent::new(r.get(0)?, r.get::<_, Option<String>>(1)?.as_deref()),
                        last_used_unix: r.get(2)?,
                    })
                })?
                .collect()
            })
            .await
            .map_err(|e| unavailable("logged_in_agents", e))
    }
}
