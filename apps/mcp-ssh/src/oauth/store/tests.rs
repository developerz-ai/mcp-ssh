use super::*;
use clients::MAX_CLIENTS;

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const CB: &str = "http://localhost/cb";

fn store() -> Store {
    Store::new(Db::memory())
}

fn request(client_id: &str) -> ConsentRequest {
    ConsentRequest {
        client_id: client_id.into(),
        redirect_uri: CB.into(),
        challenge: CHALLENGE.into(),
        state: None,
    }
}

async fn mint(store: &Store) -> Tokens {
    let code = store.new_code(request("c1")).await.unwrap();
    store.redeem(&code, VERIFIER, CB, Some("c1")).await.unwrap()
}

async fn valid(store: &Store, token: &str) -> bool {
    store.validate(token).await.unwrap()
}

/// Rows in `table` — counted, never read.
async fn row_count(db: &Db, table: &'static str) -> i64 {
    db.call(move |conn| conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)))
        .await
        .unwrap()
}

async fn drop_table(db: &Db, table: &'static str) {
    db.call(move |conn| conn.execute_batch(&format!("DROP TABLE {table}")))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_client_is_bound_to_exactly_the_uris_it_registered() {
    let store = store();
    let uris = ["http://localhost/cb".to_string(), "https://app/cb".into()];
    let client_id = store.register_client(&uris, None).await.unwrap();
    let other = store
        .register_client(&["https://other/cb".into()], None)
        .await
        .unwrap();
    let unknown = "never-registered".to_string();

    for (id, uri, allowed) in [
        (&client_id, "http://localhost/cb", true),
        (&client_id, "https://app/cb", true),
        (&client_id, "https://app/cb/x", false),
        (&client_id, "https://app", false),
        (&client_id, "https://other/cb", false),
        (&other, "https://app/cb", false),
        (&unknown, "https://app/cb", false),
    ] {
        assert_eq!(
            store.client_allows_redirect(id, uri).await.unwrap(),
            allowed,
            "redirect {uri} should be allowed={allowed}"
        );
    }
}

#[tokio::test]
async fn registering_the_same_uri_twice_is_one_binding() {
    let store = store();
    let uri = "https://app/cb".to_string();
    let client_id = store
        .register_client(&[uri.clone(), uri.clone()], None)
        .await
        .unwrap();
    assert!(
        store
            .client_allows_redirect(&client_id, &uri)
            .await
            .unwrap()
    );
    assert_eq!(row_count(&store.db, "clients").await, 1);
}

#[tokio::test]
async fn registration_is_refused_once_the_client_table_is_full() {
    let store = store();
    store
        .db
        .call(|conn| {
            for i in 0..MAX_CLIENTS {
                conn.execute(
                    "INSERT INTO clients (client_id, redirect_uri, created_unix) VALUES (?1, 'https://a/cb', 1)",
                    [format!("c{i}")],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        store.register_client(&["https://b/cb".into()], None).await,
        Err(RegisterError::Full)
    );
}

#[tokio::test]
async fn no_stored_row_holds_a_token_value() {
    let store = store();
    let tokens = mint(&store).await;
    let stored: Vec<String> = store
        .db
        .call(|c| {
            let mut stmt = c.prepare(
                "SELECT token_hash FROM access_tokens UNION ALL SELECT token_hash FROM refresh_tokens",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect()
        })
        .await
        .unwrap();
    assert_eq!(stored.len(), 2);
    for value in &stored {
        assert_ne!(value, &tokens.access);
        assert_ne!(value, &tokens.refresh);
    }
    assert!(valid(&store, &tokens.access).await, "looked up by hash");
}

#[tokio::test]
async fn redeem_requires_matching_pkce_redirect_and_client() {
    let store = store();
    for (verifier, uri, client) in [
        (VERIFIER, "http://wrong", Some("c1")),
        (
            "wrong-verifier-wrong-verifier-wrong-verifier",
            CB,
            Some("c1"),
        ),
        (VERIFIER, CB, Some("someone-else")),
    ] {
        let code = store.new_code(request("c1")).await.unwrap();
        assert_eq!(
            store
                .redeem(&code, verifier, uri, client)
                .await
                .unwrap_err(),
            GrantError::InvalidGrant
        );
        // A failed attempt still consumed the code.
        assert!(store.redeem(&code, VERIFIER, CB, Some("c1")).await.is_err());
    }
    // No client_id in the token request is accepted (public client, bound by PKCE).
    let code = store.new_code(request("c1")).await.unwrap();
    let tokens = store.redeem(&code, VERIFIER, CB, None).await.unwrap();
    assert!(valid(&store, &tokens.access).await);
    assert!(!valid(&store, "nope").await);
}

#[tokio::test]
async fn a_failed_refresh_insert_rolls_the_access_row_back() {
    let store = store();
    drop_table(&store.db, "refresh_tokens").await;
    let code = store.new_code(request("c1")).await.unwrap();
    assert_eq!(
        store.redeem(&code, VERIFIER, CB, None).await.unwrap_err(),
        GrantError::ServerError,
        "a database failure is a server error, not an invalid grant"
    );
    assert_eq!(row_count(&store.db, "access_tokens").await, 0);
}

#[tokio::test]
async fn validate_reports_an_unavailable_store_as_an_error() {
    let store = store();
    drop_table(&store.db, "access_tokens").await;
    assert_eq!(store.validate("whatever").await, Err(StoreUnavailable));
}

#[tokio::test]
async fn expired_access_token_is_rejected_and_evicted() {
    let store = store();
    let token = random_token();
    let (hash, exp) = (hash_token(&token), now_unix() - 1);
    store
        .db
        .call(move |c| {
            c.execute(
                "INSERT INTO access_tokens VALUES (?1, 'c', 'f', ?2)",
                (hash, exp),
            )
        })
        .await
        .unwrap();
    assert!(!valid(&store, &token).await);
    assert_eq!(row_count(&store.db, "access_tokens").await, 0, "evicted");
}

#[tokio::test]
async fn refresh_rotates_and_issues_a_working_access_token() {
    let store = store();
    let first = mint(&store).await;
    let second = store.refresh(&first.refresh, Some("c1")).await.unwrap();
    assert!(valid(&store, &second.access).await);
    assert_ne!(first.access, second.access);
    assert_ne!(first.refresh, second.refresh);
}

#[tokio::test]
async fn replaying_a_rotated_refresh_token_revokes_the_whole_login() {
    let store = store();
    let first = mint(&store).await;
    let other = mint(&store).await;
    let second = store.refresh(&first.refresh, None).await.unwrap();

    assert_eq!(
        store.refresh(&first.refresh, None).await.unwrap_err(),
        GrantError::InvalidGrant
    );
    assert!(
        !valid(&store, &second.access).await,
        "successor access revoked"
    );
    assert!(
        !valid(&store, &first.access).await,
        "original access revoked"
    );
    assert!(
        store.refresh(&second.refresh, None).await.is_err(),
        "successor refresh revoked"
    );
    // A different login is untouched.
    assert!(valid(&store, &other.access).await);
    assert!(store.refresh(&other.refresh, None).await.is_ok());
}

#[tokio::test]
async fn refresh_by_another_client_is_refused_without_burning_the_token() {
    let store = store();
    let tokens = mint(&store).await;
    assert!(
        store
            .refresh(&tokens.refresh, Some("intruder"))
            .await
            .is_err()
    );
    assert!(store.refresh(&tokens.refresh, Some("c1")).await.is_ok());
}

#[tokio::test]
async fn a_failed_refresh_leaves_the_refresh_token_usable() {
    let store = store();
    let tokens = mint(&store).await;
    // The insert of the new pair fails after the presented row was deleted: the
    // whole rotation rolls back, so the client's only refresh token survives.
    drop_table(&store.db, "access_tokens").await;
    assert_eq!(
        store.refresh(&tokens.refresh, None).await.unwrap_err(),
        GrantError::ServerError
    );
    assert_eq!(row_count(&store.db, "refresh_tokens").await, 1);
}

#[tokio::test]
async fn refresh_and_access_tokens_are_not_interchangeable() {
    let store = store();
    let tokens = mint(&store).await;
    assert!(
        !valid(&store, &tokens.refresh).await,
        "refresh used as access"
    );
    assert!(
        store.refresh(&tokens.access, None).await.is_err(),
        "access used as refresh"
    );
}

#[tokio::test]
async fn a_login_ends_at_its_family_cap_however_often_refreshed() {
    let store = store();
    let tokens = mint(&store).await;
    // Age the family past its absolute lifetime.
    store
        .db
        .call(|c| c.execute("UPDATE refresh_tokens SET family_expires_unix = 1", []))
        .await
        .unwrap();
    assert_eq!(
        store.refresh(&tokens.refresh, None).await.unwrap_err(),
        GrantError::InvalidGrant
    );
}

#[tokio::test]
async fn revoke_all_ends_every_login() {
    let store = store();
    let a = mint(&store).await;
    let b = mint(&store).await;
    assert_eq!(revoke_all(&store.db).await.unwrap(), 4);
    for t in [&a, &b] {
        assert!(!valid(&store, &t.access).await);
        assert!(store.refresh(&t.refresh, None).await.is_err());
    }
}

#[tokio::test]
async fn sweep_deletes_expired_rows_and_keeps_live_ones() {
    let store = store();
    const NOW: i64 = 1_000_000;
    store
        .db
        .call(|c| {
            for (hash, exp) in [
                ("a-past", NOW - 1),
                ("a-boundary", NOW),
                ("a-live", NOW + 1),
            ] {
                c.execute(
                    "INSERT INTO access_tokens VALUES (?1, 'c', 'f', ?2)",
                    (hash, exp),
                )?;
            }
            for (hash, exp) in [("r-past", NOW - 1), ("r-live", NOW + 1)] {
                c.execute(
                    "INSERT INTO refresh_tokens VALUES (?1, 'c', 'f', ?2, ?2)",
                    (hash, exp),
                )?;
            }
            c.execute(
                "INSERT INTO refresh_tombstones VALUES ('t-past', 'f', ?1)",
                [NOW - 1],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(sweep_expired_access(&store.db, NOW).await, 2);
    assert_eq!(
        sweep_expired_refresh(&store.db, NOW).await,
        2,
        "refresh + tombstone"
    );
    assert_eq!(row_count(&store.db, "access_tokens").await, 1);
    assert_eq!(row_count(&store.db, "refresh_tokens").await, 1);
    assert_eq!(row_count(&store.db, "refresh_tombstones").await, 0);
    assert_eq!(sweep_expired_access(&store.db, NOW).await, 0);
    assert_eq!(sweep_expired_refresh(&store.db, NOW).await, 0);
}

#[tokio::test]
async fn stale_clients_without_a_live_token_are_swept() {
    let store = store();
    let now = now_unix();
    let old = now - secs(CLIENT_GRACE) - 1;
    let tokens = mint(&store).await; // live tokens for client `c1`
    store
        .db
        .call(move |c| {
            for (id, created) in [("c1", old), ("abandoned", old), ("fresh", now)] {
                c.execute(
                    "INSERT INTO clients (client_id, redirect_uri, created_unix) VALUES (?1, 'https://a/cb', ?2)",
                    (id, created),
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(sweep_stale_clients(&store.db, now).await, 1);
    for (id, kept) in [("c1", true), ("abandoned", false), ("fresh", true)] {
        assert_eq!(
            store
                .client_allows_redirect(id, "https://a/cb")
                .await
                .unwrap(),
            kept,
            "{id}"
        );
    }
    assert!(valid(&store, &tokens.access).await);
}

#[tokio::test]
async fn sweep_leaves_a_freshly_issued_pair_usable() {
    let store = store();
    let tokens = mint(&store).await;
    let now = now_unix();
    assert_eq!(sweep_expired_access(&store.db, now).await, 0);
    assert_eq!(sweep_expired_refresh(&store.db, now).await, 0);
    assert!(valid(&store, &tokens.access).await);
    assert!(store.refresh(&tokens.refresh, None).await.is_ok());
}

#[tokio::test]
async fn multiple_clients_hold_independent_tokens() {
    let store = store();
    let a = mint(&store).await;
    let b = mint(&store).await;
    let c = mint(&store).await;
    assert!(store.refresh(&b.refresh, None).await.is_ok());
    assert!(valid(&store, &a.access).await);
    assert!(valid(&store, &c.access).await);
}

#[test]
fn tokens_debug_is_redacted() {
    let tokens = new_tokens();
    let shown = format!("{tokens:?}");
    assert!(!shown.contains(&tokens.access) && !shown.contains(&tokens.refresh));
}

// ---- presence ----

/// A live token for a freshly registered client named `name`; returns the bearer.
async fn login_as(store: &Store, name: Option<&str>) -> String {
    let client = store
        .register_client(&[CB.into()], name.and_then(crate::agent::ClientName::parse))
        .await
        .unwrap();
    let code = store
        .new_code(ConsentRequest {
            client_id: client.clone(),
            ..request(&client)
        })
        .await
        .unwrap();
    store
        .redeem(&code, VERIFIER, CB, Some(&client))
        .await
        .unwrap()
        .access
}

async fn last_used(db: &Db) -> Vec<Option<i64>> {
    db.call(|conn| {
        let mut stmt = conn.prepare("SELECT last_used_unix FROM clients")?;
        stmt.query_map([], |r| r.get(0))?.collect()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn authenticate_names_the_agent_and_marks_it_active() {
    let db = Db::memory();
    let store = Store::new(db.clone());
    let bearer = login_as(&store, Some("cursor")).await;
    assert_eq!(last_used(&db).await, [None], "not active before any use");

    let agent = store
        .authenticate(&hash_token(&bearer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(agent.name(), Some("cursor"));
    let first = last_used(&db).await;
    assert!(first[0].is_some(), "the use is recorded");

    // A second use within a minute is a read: the time doesn't move.
    store
        .authenticate(&hash_token(&bearer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last_used(&db).await, first);

    assert_eq!(
        store.authenticate(&hash_token("unknown")).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn an_expired_bearer_authenticates_no_one_and_is_evicted() {
    let db = Db::memory();
    let store = Store::new(db.clone());
    let bearer = login_as(&store, None).await;
    db.call(|c| c.execute("UPDATE access_tokens SET expires_unix = 1", []))
        .await
        .unwrap();
    assert_eq!(
        store.authenticate(&hash_token(&bearer)).await.unwrap(),
        None
    );
    assert_eq!(row_count(&db, "access_tokens").await, 0);
}

#[tokio::test]
async fn active_agents_are_those_seen_in_the_window_newest_first() {
    let db = Db::memory();
    let store = Store::new(db.clone());
    let (a, b) = (
        login_as(&store, Some("alpha")).await,
        login_as(&store, Some("beta")).await,
    );
    let _idle = login_as(&store, Some("idle")).await;
    store.authenticate(&hash_token(&a)).await.unwrap();
    store.authenticate(&hash_token(&b)).await.unwrap();
    // `alpha` was last seen long ago: outside the window.
    let stale = now_unix() - secs(PRESENCE_WINDOW) - 1;
    let alpha = store.authenticate(&hash_token(&a)).await.unwrap().unwrap();
    let id = alpha.client_id().to_string();
    db.call(move |c| {
        c.execute(
            "UPDATE clients SET last_used_unix = ?1 WHERE client_id = ?2",
            (stale, id),
        )
    })
    .await
    .unwrap();

    let active = store.active_agents().await.unwrap();
    let names: Vec<_> = active.iter().map(|a| a.name()).collect();
    assert_eq!(names, [Some("beta")]);

    let logged_in = store.logged_in_agents().await.unwrap();
    let names: Vec<_> = logged_in.iter().map(|a| a.agent.name()).collect();
    assert_eq!(
        names,
        [Some("beta"), Some("alpha"), Some("idle")],
        "most recent first, never-used last"
    );
    assert_eq!(logged_in[2].last_used_unix, None);
}

#[tokio::test]
async fn a_client_name_is_stored_cleaned() {
    let db = Db::memory();
    let store = Store::new(db.clone());
    let bearer = login_as(&store, Some("bad\u{1b}[2J\nname")).await;
    let agent = store
        .authenticate(&hash_token(&bearer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(agent.name(), Some("bad[2J name"));
    let stored: String = db
        .call(|c| c.query_row("SELECT client_name FROM clients", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(stored, "bad[2J name");
}
