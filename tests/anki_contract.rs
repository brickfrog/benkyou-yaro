//! Card emission contracts, and where a push goes.
//!
//! Three things are load-bearing. Note identity must not depend on card text. Code
//! content must survive into the card. `--push` must write to the collection the caller
//! named, not to whatever is listening on this machine.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;

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

/// Editing a card's text must not change its identity. Otherwise the regenerated note
/// lands as a new note and its review history is discarded.
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

/// Everything off the allowlist is escaped, including anything carrying attributes. That
/// keeps handlers and javascript URLs out by construction.
///
/// The test asserts that no live markup survives. An escaped `img` tag still contains
/// the word "onerror" as inert text, which is fine.
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

/// Cloze cards go to the cloze notetype, everything else to basic. A cloze sent to a
/// basic model produces a card that never reveals anything.
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

/// A malformed address is refused before anything is built, and the refusal names the
/// form it wanted. Two guesses are worth pinning: a URL, because this posts to a bare
/// address, and an unbracketed IPv6 literal, whose colons hide the port separator.
#[test]
fn an_address_that_is_not_host_port_is_refused() {
    for bad in [
        "http://127.0.0.1:8765",
        "127.0.0.1",
        ":8765",
        "127.0.0.1:0",
        "127.0.0.1:notaport",
        "127.0.0.1:99999",
        "::1:8765",
    ] {
        let err = AnkiConnect::new(bad).expect_err(&format!("{bad} should be refused"));
        assert!(err.contains(bad), "refusal did not quote the address: {err}");
    }

    for good in [DEFAULT_ADDR, "localhost:8765", "mac.local:43117", "[::1]:8765"] {
        let client = AnkiConnect::new(good).unwrap_or_else(|e| panic!("{good}: {e}"));
        assert_eq!(client.addr, good);
    }
}

/// One AnkiConnect exchange: read a request, answer `result`, close. The client opens a
/// connection per call, so this serves a fixed number of them and then stops.
fn serve_fake_anki(listener: TcpListener, calls: usize) -> Vec<String> {
    let mut actions = Vec::new();
    for _ in 0..calls {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        // Read until the body is in hand. `read_to_end` blocks, because the client keeps
        // the socket open for the reply.
        loop {
            let n = sock.read(&mut buf).expect("read request");
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw).to_string();
            let done = match (text.find("\r\n\r\n"), body_len(&text)) {
                (Some(head), Some(len)) => raw.len() >= head + 4 + len,
                _ => n == 0,
            };
            if done || n == 0 {
                break;
            }
        }
        let text = String::from_utf8_lossy(&raw).to_string();
        let body: serde_json::Value = serde_json::from_str(
            text.split("\r\n\r\n").nth(1).expect("request had a body").trim(),
        )
        .expect("request body was json");
        let action = body["action"].as_str().expect("action").to_string();

        let result = match action.as_str() {
            "version" => serde_json::json!(6),
            // Both notetypes already exist, so nothing is created. This test is about
            // where the traffic went.
            "modelNames" => serde_json::json!([MODEL_BASIC, MODEL_CLOZE]),
            "findNotes" => serde_json::json!([]),
            "addNote" => serde_json::json!(1234),
            _ => serde_json::Value::Null,
        };
        actions.push(action);

        let payload = serde_json::json!({ "result": result, "error": null }).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        );
        sock.write_all(response.as_bytes()).expect("write response");
    }
    actions
}

fn body_len(request: &str) -> Option<usize> {
    request
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))
        .and_then(|v| v.parse().ok())
}

/// `--anki-addr` names Anki on another machine, reached through a forwarded port. A push
/// that ignored it writes to this machine's collection, or fails with nothing listening
/// while Anki is plainly running.
#[test]
fn push_goes_to_the_address_it_was_given() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake anki");
    let addr = listener.local_addr().unwrap().to_string();
    assert_ne!(addr, DEFAULT_ADDR, "the fake must not be where the default points");

    let dir = std::env::temp_dir().join(format!("benkyou-anki-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let cards = dir.join("cards.json");
    std::fs::write(
        &cards,
        serde_json::json!([{
            "concept_id": "pandas_groupby",
            "role": "definition",
            "front": "What does groupby do?",
            "back": "Splits, applies, combines.",
        }])
        .to_string(),
    )
    .expect("write cards");

    // version, modelNames, createDeck, findNotes, addNote. Five calls, five sockets.
    let server = std::thread::spawn(move || serve_fake_anki(listener, 5));

    let out = Command::new(env!("CARGO_BIN_EXE_benkyou"))
        .args([
            "cards",
            cards.to_str().unwrap(),
            "--deck",
            "benkyou::ramp",
            "--push",
            "--anki-addr",
            &addr,
        ])
        .output()
        .expect("run benkyou");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "push failed: {stderr}{stdout}");

    let actions = server.join().expect("fake anki");
    assert!(
        actions.contains(&"addNote".to_string()),
        "the note never reached the address given: {actions:?}"
    );

    let body: serde_json::Value = serde_json::from_str(&stdout).expect("json report");
    assert_eq!(body["anki_addr"], serde_json::json!(addr), "report named the wrong host");
    assert_eq!(body["added"].as_array().map(Vec::len), Some(1));

    std::fs::remove_dir_all(&dir).ok();
}

/// The dry run is the rehearsal for the push. An address the push refuses has to fail
/// here too.
#[test]
fn a_dry_run_refuses_an_address_the_push_would_refuse() {
    let dir = std::env::temp_dir().join(format!("benkyou-anki-dry-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let cards = dir.join("cards.json");
    std::fs::write(&cards, "[]").expect("write cards");

    let out = Command::new(env!("CARGO_BIN_EXE_benkyou"))
        .args(["cards", cards.to_str().unwrap(), "--anki-addr", "http://127.0.0.1:8765"])
        .output()
        .expect("run benkyou");
    assert!(!out.status.success(), "a URL was accepted as an address");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("HOST:PORT"), "refusal did not say what was wanted: {stderr}");

    std::fs::remove_dir_all(&dir).ok();
}
