//! Who is calling: the OAuth client a bearer was issued to. Every `/mcp` request
//! carries one (`auth::require_auth` puts it in the request extensions), every job
//! records the one that started it, and agents see each other through it.
//!
//! The display name comes from Dynamic Client Registration (`client_name`), which
//! anyone can send — untrusted input that ends up in an agent's context and an
//! operator's terminal. [`ClientName::parse`] is the one gate it passes through.
use sha2::{Digest, Sha256};

/// Longest client name kept, in characters. Long enough for `Claude Code`,
/// `cursor-vscode`; short enough that a hostile name can't crowd a listing.
const MAX_NAME_CHARS: usize = 32;

/// A `client_name` fit to show: no control or invisible-formatting characters
/// (escape sequences, newlines that forge lines, bidi overrides), whitespace runs
/// collapsed to one space, at most [`MAX_NAME_CHARS`] characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientName(String);

impl ClientName {
    /// The cleaned name, or `None` when nothing printable is left.
    pub fn parse(raw: &str) -> Option<Self> {
        // Whitespace controls (`\n`, `\t`) separate words; the join below makes
        // each a single space.
        let visible: String = raw
            .chars()
            .filter(|&c| c.is_whitespace() || !hidden(c))
            .collect();
        let words = visible.split_whitespace().collect::<Vec<_>>().join(" ");
        let bounded: String = words.chars().take(MAX_NAME_CHARS).collect();
        let name = bounded.trim_end();
        (!name.is_empty()).then(|| Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Characters that never belong in a displayed name: controls (C0, DEL, C1) and
/// the zero-width / bidi-override formatting marks that disguise what is shown.
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}'                // soft hyphen
            | '\u{200B}'..='\u{200F}' // zero-width, LRM/RLM
            | '\u{202A}'..='\u{202E}' // bidi embeddings/overrides
            | '\u{2060}'..='\u{2064}' // word joiner, invisible operators
            | '\u{2066}'..='\u{2069}' // bidi isolates
            | '\u{FEFF}'              // BOM / zero-width no-break space
        )
}

/// An OAuth client, as agents and the operator see it. `Debug` shows only the
/// label: the `client_id` is public, but it is still never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct Agent {
    client_id: String,
    name: Option<ClientName>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Agent").field(&self.label()).finish()
    }
}

impl Agent {
    /// `name` is re-cleaned here, so a row written by an older binary (or edited by
    /// hand) renders as safely as a fresh registration.
    pub fn new(client_id: String, name: Option<&str>) -> Self {
        Self {
            client_id,
            name: name.and_then(ClientName::parse),
        }
    }

    /// An agent from a row's nullable `(client_id, client_name)` columns; `None`
    /// for a row written before jobs recorded who started them.
    pub fn from_columns(client_id: Option<String>, name: Option<String>) -> Option<Self> {
        client_id.map(|id| Self::new(id, name.as_deref()))
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_ref().map(ClientName::as_str)
    }

    /// `<name>#<tag>`, or `client#<tag>` for a client that registered no name. The
    /// tag tells two clients with the same name apart (two Claude Code sessions)
    /// and is derived from a hash, so no part of the `client_id` is shown.
    pub fn label(&self) -> String {
        format!("{}#{}", self.name().unwrap_or("client"), self.tag())
    }

    /// First 4 hex digits of SHA-256(`client_id`).
    fn tag(&self) -> String {
        let digest = Sha256::digest(self.client_id.as_bytes());
        format!("{:02x}{:02x}", digest[0], digest[1])
    }
}

#[cfg(test)]
mod tests;
