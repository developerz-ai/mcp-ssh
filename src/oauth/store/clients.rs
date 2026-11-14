//! Registered clients: `/register` binds a `client_id` to the redirect URIs it
//! may receive auth codes at, and `/authorize` checks that binding.
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};

use super::{RegisterError, Store, StoreUnavailable, unavailable};
use crate::db::now_unix;
use crate::oauth::secret::random_token;

/// Registered clients held at once. `/register` is unauthenticated (it bootstraps
/// auth), so the table is bounded; the stale-client sweep frees room.
pub(super) const MAX_CLIENTS: i64 = 1000;

impl Store {
    /// Register a client: mint an id and bind it to the redirect URIs that may
    /// receive its auth codes. Duplicate URIs collapse to one row. The returned
    /// `client_id` is public data, but it is still never logged.
    pub async fn register_client(&self, redirect_uris: &[String]) -> Result<String, RegisterError> {
        let client_id = random_token();
        let id = client_id.clone(); // the closure must own its inputs ('static)
        let uris = redirect_uris.to_vec();
        let now = now_unix();
        let registered = self
            .db
            .call(move |conn| {
                // Immediate: the count and the insert see the same table.
                let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
                let clients: i64 =
                    tx.query_row("SELECT COUNT(DISTINCT client_id) FROM clients", [], |r| {
                        r.get(0)
                    })?;
                if clients >= MAX_CLIENTS {
                    return Ok(false);
                }
                for uri in &uris {
                    tx.execute(
                        "INSERT OR IGNORE INTO clients (client_id, redirect_uri, created_unix) \
                         VALUES (?1, ?2, ?3)",
                        (&id, uri, now),
                    )?;
                }
                tx.commit()?;
                Ok(true)
            })
            .await
            .map_err(|e| {
                unavailable("register_client", e);
                RegisterError::Unavailable
            })?;
        if registered {
            Ok(client_id)
        } else {
            tracing::warn!(event = "register_refused", "client table full");
            Err(RegisterError::Full)
        }
    }

    /// True only if `client_id` is registered and `redirect_uri` is *exactly* one
    /// of the URIs it registered.
    pub async fn client_allows_redirect(
        &self,
        client_id: &str,
        redirect_uri: &str,
    ) -> Result<bool, StoreUnavailable> {
        let (client_id, redirect_uri) = (client_id.to_owned(), redirect_uri.to_owned());
        self.db
            .call(move |conn| {
                conn.query_row(
                    "SELECT 1 FROM clients WHERE client_id = ?1 AND redirect_uri = ?2",
                    (client_id, redirect_uri),
                    |_| Ok(()),
                )
                .optional()
                .map(|row| row.is_some())
            })
            .await
            .map_err(|e| unavailable("client_allows_redirect", e))
    }
}
