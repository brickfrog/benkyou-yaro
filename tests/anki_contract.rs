//! Card emission contracts.
//!
//! Two things here are load-bearing and easy to get quietly wrong: note identity must
//! not depend on card text, and code content must survive into the card.

use benkyou::anki::*;

fn card(concept: &str, role: Role, front: &str, back: &str) -> Card {
    Card {
        concept_id: concept.into(),
        role,
        front: front.into(),
        back: back.into(),
        example: None,
        tags: vec![],
    }
}

/// The whole point: editing a card's text must not change its identity, or the
/// regenerated note lands as a new note and its review history is discarded.
#[test]
fn note_identity_survives_an_edit_to_the_card_text() {
    let before = card("pandas_groupby", Role::Definition, "What is groupby?", "Splits.");
    let after = card(
        "pandas_groupby",
        Role::Definition,
        "What does DataFrame.groupby do?",
        "Split-apply-combine, returning a GroupBy object.",
    );

    let g1 = note_guid(&before.concept_id, before.role);
    let g2 = note_guid(&after.concept_id, after.role);
    assert_eq!(g1, g2, "a rewritten card is the same note");

    assert_eq!(
        before.to_note("d").fields["BenkyouGuid"],
        after.to_note("d").fields["BenkyouGuid"]
    );
}

/// Different roles on one concept, and one role across concepts, are distinct notes.
#[test]
fn identity_separates_roles_and_concepts() {
    let roles = [Role::Definition, Role::Application, Role::Contrast, Role::Cloze];
    let mut seen = std::collections::BTreeSet::new();
    for concept in ["a", "b"] {
        for role in roles {
            assert!(
                seen.insert(note_guid(concept, role)),
                "collision on {concept}/{role:?}"
            );
        }
    }
    assert_eq!(seen.len(), 8);
}

/// Code must reach the card as markup, or the styling that formats it can never fire.
#[test]
fn code_markup_survives_sanitization() {
    let out = sanitize("Use <code>df.groupby()</code> then <pre>agg()</pre>");
    assert!(out.contains("<code>"), "code tag was escaped: {out}");
    assert!(out.contains("</code>"), "closing code tag was escaped: {out}");
    assert!(out.contains("<pre>"), "pre tag was escaped: {out}");
    assert!(!out.contains("&lt;code&gt;"), "{out}");
}

/// Everything off the allowlist is escaped, including anything carrying attributes,
/// which is what keeps handlers and javascript URLs out by construction.
///
/// The property asserted is that no *live* markup survives: an escaped `&lt;img ...`
/// still contains the word "onerror" as inert text, and that is fine. What must not
/// happen is a `<` that opens a tag the allowlist does not name.
#[test]
fn markup_off_the_allowlist_is_escaped() {
    const ALLOWED: &[&str] =
        &["pre", "code", "b", "i", "em", "strong", "br", "ul", "ol", "li"];

    for hostile in [
        "<script>alert(1)</script>",
        "<img src=x onerror=alert(1)>",
        "<a href=\"javascript:alert(1)\">x</a>",
        "<CODE onmouseover=evil()>x</CODE>",
        "<div>x</div>",
        "<code x=1>",
        "<<script>>",
    ] {
        let out = sanitize(hostile);
        for (i, _) in out.match_indices('<') {
            let rest = &out[i + 1..];
            let end = rest.find('>').unwrap_or(rest.len());
            let name = rest[..end]
                .trim_matches('/')
                .trim()
                .to_ascii_lowercase();
            assert!(
                ALLOWED.contains(&name.as_str()),
                "live `<{name}>` survived from {hostile:?} -> {out:?}"
            );
        }
    }
}

#[test]
fn ampersands_and_stray_brackets_are_escaped() {
    let out = sanitize("a < b && c > d");
    assert!(out.contains("&amp;&amp;"), "{out}");
    assert!(out.contains("&gt;"), "{out}");
    assert!(!out.contains("a < b"), "bare < should be escaped: {out}");
}

/// Anki tags are whitespace-delimited, so a tag with a space silently becomes two.
#[test]
fn tags_cannot_split_into_two() {
    assert_eq!(normalize_tag("bayesian hierarchical models"), "bayesian_hierarchical_models");
    assert_eq!(normalize_tag("  padded  "), "padded");
    assert_eq!(normalize_tag("already_fine"), "already_fine");
    assert!(!normalize_tag("a\tb\nc").contains(char::is_whitespace));

    let c = Card {
        tags: vec!["two words".into(), "".into()],
        ..card("some_concept", Role::Definition, "f", "b")
    };
    let note = c.to_note("deck");
    for tag in &note.tags {
        assert!(!tag.contains(' '), "tag would split: {tag:?}");
        assert!(!tag.is_empty(), "empty tag emitted");
    }
    assert!(note.tags.iter().any(|t| t.contains("some_concept")));
}

/// Cloze cards must go to the cloze notetype; everything else to basic. Sending a
/// cloze to a basic model produces a card that never reveals anything.
#[test]
fn cloze_cards_use_the_cloze_model() {
    assert_eq!(card("c", Role::Cloze, "f", "b").model_name(), MODEL_CLOZE);
    for role in [Role::Definition, Role::Application, Role::Contrast] {
        assert_eq!(card("c", role, "f", "b").model_name(), MODEL_BASIC);
    }
}

#[test]
fn a_note_carries_every_field_the_models_declare() {
    let c = Card {
        example: Some("<code>df.groupby('k').sum()</code>".into()),
        ..card("pandas_groupby", Role::Definition, "front", "back")
    };
    let note = c.to_note("Benkyou::Ramp");

    assert_eq!(note.deck_name, "Benkyou::Ramp");
    for field in ["Front", "Back", "Example", "Concept", "BenkyouGuid"] {
        assert!(note.fields.contains_key(field), "missing field {field}");
    }
    assert!(note.fields["Example"].contains("<code>"), "example lost its markup");
}
