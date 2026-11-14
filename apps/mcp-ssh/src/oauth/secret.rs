//! The secret values the OAuth flow mints and checks: random opaque tokens, the
//! hash a token is stored as, and PKCE (RFC 7636) S256 with its input rules.
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// 256 bits from the OS-seeded CSPRNG, base64url. Used for every code, token,
/// client id, consent nonce and family id.
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes[..]);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// What a token is stored (and looked up) as. A plain SHA-256 is enough: the input
/// is 256 random bits, so there is nothing to brute-force and no salt to add — the
/// point is only that a copy of the database yields no usable bearer.
pub fn hash_token(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

/// An S256 `code_challenge` is BASE64URL(SHA-256) without padding: exactly 43
/// base64url characters.
pub fn is_valid_challenge(challenge: &str) -> bool {
    challenge.len() == 43
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// RFC 7636 §4.1: a verifier is 43–128 characters of `[A-Za-z0-9-._~]`.
fn is_valid_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// PKCE S256: a well-formed verifier whose BASE64URL(SHA256(verifier)) is the
/// challenge.
pub fn verify_pkce(challenge: &str, verifier: &str) -> bool {
    is_valid_verifier(verifier)
        && URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) == challenge
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[test]
    fn pkce_roundtrip() {
        assert!(verify_pkce(CHALLENGE, VERIFIER));
        assert!(!verify_pkce(CHALLENGE, "wrong-verifier"));
    }

    #[test]
    fn a_malformed_verifier_never_verifies() {
        // Too short, too long, or outside the unreserved set — even when its hash
        // would be computed fine.
        for verifier in [
            "a".repeat(42),
            "a".repeat(129),
            format!("{}!", "a".repeat(43)),
        ] {
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            assert!(!verify_pkce(&challenge, &verifier), "{verifier}");
        }
        let ok = "a".repeat(128);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(ok.as_bytes()));
        assert!(
            verify_pkce(&challenge, &ok),
            "128 chars is the inclusive max"
        );
    }

    #[test]
    fn a_challenge_must_be_43_base64url_chars() {
        assert!(is_valid_challenge(CHALLENGE));
        assert!(!is_valid_challenge("abc"));
        assert!(!is_valid_challenge(&format!("{}=", &CHALLENGE[..42])));
        assert!(!is_valid_challenge(&format!("{CHALLENGE}A")));
    }

    #[test]
    fn a_token_hash_is_stable_and_not_the_token() {
        let token = random_token();
        assert_eq!(hash_token(&token), hash_token(&token));
        assert_ne!(hash_token(&token), token);
        assert_ne!(random_token(), token, "tokens are random");
    }
}
