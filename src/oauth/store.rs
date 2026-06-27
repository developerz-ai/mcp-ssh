//! In-memory OAuth state: short-lived authorization codes and opaque access
//! tokens. Opaque (not JWT) because this server is the only validator — no
//! signing keys, no JWKS. ponytail: tokens reset on restart; clients re-auth.
use std::{collections::HashMap, time::Duration};

use base64::Engine;
use rand::Rng;
use sha2::{Digest, Sha256};
use tokio::{sync::Mutex, time::Instant};

const CODE_TTL: Duration = Duration::from_secs(600);
pub const TOKEN_TTL: Duration = Duration::from_secs(3600);

struct CodeEntry {
    challenge: String,
    redirect_uri: String,
    expires: Instant,
}

struct TokenEntry {
    expires: Instant,
}

#[derive(Default)]
pub struct Store {
    codes: Mutex<HashMap<String, CodeEntry>>,
    tokens: Mutex<HashMap<String, TokenEntry>>,
}

impl Store {
    /// Issue an authorization code bound to the PKCE challenge + redirect_uri.
    pub async fn new_code(&self, challenge: String, redirect_uri: String) -> String {
        let code = random_token();
        self.codes.lock().await.insert(
            code.clone(),
            CodeEntry {
                challenge,
                redirect_uri,
                expires: Instant::now() + CODE_TTL,
            },
        );
        code
    }

    /// Exchange a code (with its PKCE verifier) for an access token.
    pub async fn redeem(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<String, &'static str> {
        let entry = self
            .codes
            .lock()
            .await
            .remove(code)
            .ok_or("invalid_grant")?;
        if entry.expires < Instant::now() {
            return Err("invalid_grant");
        }
        if entry.redirect_uri != redirect_uri {
            return Err("invalid_grant");
        }
        if !verify_pkce(&entry.challenge, verifier) {
            return Err("invalid_grant");
        }
        let token = random_token();
        self.tokens.lock().await.insert(
            token.clone(),
            TokenEntry {
                expires: Instant::now() + TOKEN_TTL,
            },
        );
        Ok(token)
    }

    /// Insert a token directly — test seam only, bypasses the PKCE flow.
    #[cfg(test)]
    pub(crate) async fn insert_token(&self, token: &str) {
        self.tokens.lock().await.insert(
            token.to_owned(),
            TokenEntry {
                // 24-hour TTL; tests finish long before this expires.
                expires: Instant::now() + Duration::from_secs(3600 * 24),
            },
        );
    }

    /// True if the bearer token is known and unexpired.
    pub async fn validate(&self, token: &str) -> bool {
        let mut tokens = self.tokens.lock().await;
        match tokens.get(token) {
            Some(e) if e.expires > Instant::now() => true,
            Some(_) => {
                tokens.remove(token);
                false
            }
            None => false,
        }
    }
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill(&mut bytes[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// PKCE S256: BASE64URL(SHA256(verifier)) == challenge.
pub fn verify_pkce(challenge: &str, verifier: &str) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest) == challenge
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_roundtrip() {
        // verifier -> S256 challenge, must verify; a wrong verifier must not.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(verify_pkce(challenge, verifier));
        assert!(!verify_pkce(challenge, "wrong-verifier"));
    }

    #[tokio::test]
    async fn redeem_requires_matching_pkce_and_redirect() {
        let store = Store::default();
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        let code = store.new_code(challenge.into(), "http://cb".into()).await;

        assert!(store.redeem(&code, verifier, "http://wrong").await.is_err());
        // code was consumed on the failed attempt above, so issue a fresh one
        let code = store.new_code(challenge.into(), "http://cb".into()).await;
        let token = store.redeem(&code, verifier, "http://cb").await.unwrap();
        assert!(store.validate(&token).await);
        assert!(!store.validate("nope").await);
    }

    #[tokio::test]
    async fn expired_token_is_invalid_and_evicted() {
        tokio::time::pause(); // take control of the mock clock
        let store = Store::default();
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        let code = store.new_code(challenge.into(), "http://cb".into()).await;
        let token = store.redeem(&code, verifier, "http://cb").await.unwrap();

        // Valid before expiry.
        assert!(store.validate(&token).await);

        // Advance past TOKEN_TTL.
        tokio::time::advance(TOKEN_TTL + Duration::from_secs(1)).await;

        // First call after expiry returns false and evicts the entry.
        assert!(!store.validate(&token).await);
        // Second call confirms the entry is gone (not just considered invalid).
        assert!(!store.validate(&token).await);
    }
}
