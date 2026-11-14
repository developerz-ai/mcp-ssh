use super::*;

#[test]
fn a_plain_name_is_kept() {
    assert_eq!(
        ClientName::parse("Claude Code").unwrap().as_str(),
        "Claude Code"
    );
}

#[test]
fn control_and_invisible_characters_are_stripped() {
    let name = ClientName::parse("evil\u{1b}[31m\nrow\u{202E}x\u{200B}y\u{7f}\u{9b}").unwrap();
    assert_eq!(name.as_str(), "evil[31m rowxy");
    assert!(!name.as_str().chars().any(char::is_control));
}

#[test]
fn whitespace_collapses_and_length_is_bounded() {
    let name = ClientName::parse(&format!("  a \t\t b {}", "z".repeat(500))).unwrap();
    assert!(name.as_str().starts_with("a b zz"), "{name:?}");
    assert_eq!(name.as_str().chars().count(), MAX_NAME_CHARS);
    // Bounded in characters, never cut inside a multi-byte one.
    let wide = ClientName::parse(&"é".repeat(100)).unwrap();
    assert_eq!(wide.as_str().chars().count(), MAX_NAME_CHARS);
}

#[test]
fn nothing_printable_is_no_name() {
    for raw in ["", "   ", "\u{1b}\u{7}\n", "\u{200B}\u{FEFF}"] {
        assert_eq!(ClientName::parse(raw), None, "{raw:?}");
    }
}

#[test]
fn label_is_name_plus_a_hash_tag_never_the_client_id() {
    let named = Agent::new("client-id-123".into(), Some("cursor"));
    let label = named.label();
    let (name, tag) = label.split_once('#').unwrap();
    assert_eq!(name, "cursor");
    assert_eq!(tag.len(), 4);
    assert!(tag.chars().all(|c| c.is_ascii_hexdigit()), "{tag}");
    assert!(!label.contains("client-id"), "{label}");

    let anonymous = Agent::new("client-id-123".into(), None);
    assert_eq!(anonymous.label(), format!("client#{tag}"));
    // Same name, different client: told apart by the tag.
    assert_ne!(Agent::new("other".into(), Some("cursor")).label(), label);
}

#[test]
fn debug_shows_the_label_not_the_client_id() {
    let agent = Agent::new("secret-looking-id".into(), Some("cursor"));
    let shown = format!("{agent:?}");
    assert!(
        shown.contains("cursor#") && !shown.contains("secret-looking-id"),
        "{shown}"
    );
}

#[test]
fn from_columns_needs_a_client_id_and_recleans_the_name() {
    assert_eq!(Agent::from_columns(None, Some("x".into())), None);
    let agent = Agent::from_columns(Some("id".into()), Some("a\nb".into())).unwrap();
    assert_eq!(agent.name(), Some("a b"));
}
