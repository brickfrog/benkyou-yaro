//! Cards, and getting them into Anki.
//!
//! Anki does one thing better than anything else: it is a durable, synced, mobile,
//! FSRS-tuned queue. It gets to do exactly that. Ordering lives in our graph; Anki
//! receives the result. See DESIGN.md §4.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Which card a note is for a given concept. Part of the GUID, so renaming a role
/// orphans its notes — treat these as permanent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// What is X?
    Definition,
    /// When would you reach for X?
    Application,
    /// X versus the thing it is most often confused with.
    Contrast,
    /// A cloze over a canonical snippet.
    Cloze,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Definition => "definition",
            Role::Application => "application",
            Role::Contrast => "contrast",
            Role::Cloze => "cloze",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub concept_id: String,
    pub role: Role,
    pub front: String,
    pub back: String,
    #[serde(default)]
    pub example: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A stable note identity, derived from the concept and the role — **never from the
/// card's content**.
///
/// Content-hashed GUIDs are the default in most tooling and they are wrong here. This
/// tool re-projects a persistent graph, so an edited concept regenerates its cards; if
/// the GUID moved with the text, every touched note would return as a *new* note and
/// its review history would be discarded. Keying on identity instead means a
/// regenerated card updates in place and FSRS keeps its scheduling.
pub fn note_guid(concept_id: &str, role: Role) -> String {
    format!("benkyou:{concept_id}:{}", role.as_str())
}

/// Anki tags are whitespace-delimited, so a tag containing a space silently becomes
/// two tags. Collapse anything that would split.
pub fn normalize_tag(tag: &str) -> String {
    let mut out = String::with_capacity(tag.len());
    for c in tag.chars() {
        if c.is_whitespace() || c == '"' {
            if !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
        } else {
            out.push(c);
        }
    }
    out.trim_matches('_').to_string()
}

/// Tags that are allowed to carry markup through to a card.
const ALLOWED: &[&str] = &["pre", "code", "b", "i", "em", "strong", "br", "ul", "ol", "li"];

/// Escape a field for Anki, keeping a small allowlist of markup.
///
/// Escaping every field wholesale is the obvious thing and it is wrong for this
/// project: the card styling goes to the trouble of formatting `<pre><code>` blocks,
/// and a wholesale escape renders them as literal `&lt;pre&gt;` so that styling can
/// never fire. For a tool whose content is mostly code, that is fatal rather than
/// cosmetic. Anything not on the allowlist is still escaped.
pub fn sanitize(input: &str) -> String {
    let bytes: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes[i];
        if c != '<' {
            match c {
                '&' => out.push_str("&amp;"),
                '>' => out.push_str("&gt;"),
                _ => out.push(c),
            }
            i += 1;
            continue;
        }

        // Try to read a simple tag: <name>, </name>, <name/>.
        let Some(close) = bytes[i..].iter().position(|c| *c == '>').map(|p| i + p) else {
            out.push_str("&lt;");
            i += 1;
            continue;
        };
        let inner: String = bytes[i + 1..close].iter().collect();
        let trimmed = inner.trim();
        let name = trimmed
            .trim_start_matches('/')
            .trim_end_matches('/')
            .trim()
            .to_ascii_lowercase();

        // Only bare tags are allowed through; anything with attributes is escaped,
        // which keeps event handlers and `javascript:` URLs out by construction.
        let bare = !name.contains(char::is_whitespace) && !name.is_empty();
        if bare && ALLOWED.contains(&name.as_str()) {
            out.push('<');
            out.push_str(trimmed);
            out.push('>');
            i = close + 1;
        } else {
            out.push_str("&lt;");
            i += 1;
        }
    }
    out
}

/// A note as AnkiConnect wants it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Note {
    #[serde(rename = "deckName")]
    pub deck_name: String,
    #[serde(rename = "modelName")]
    pub model_name: String,
    pub fields: BTreeMap<String, String>,
    pub tags: Vec<String>,
}

pub const MODEL_BASIC: &str = "benkyou Basic";
pub const MODEL_CLOZE: &str = "benkyou Cloze";

impl Card {
    pub fn model_name(&self) -> &'static str {
        match self.role {
            Role::Cloze => MODEL_CLOZE,
            _ => MODEL_BASIC,
        }
    }

    /// Build the AnkiConnect note. The GUID rides in a field so it can be searched
    /// for on the next run; Anki does not let a client choose a note id.
    pub fn to_note(&self, deck: &str) -> Note {
        let mut fields = BTreeMap::new();
        fields.insert("Front".into(), sanitize(&self.front));
        fields.insert("Back".into(), sanitize(&self.back));
        fields.insert(
            "Example".into(),
            self.example.as_deref().map(sanitize).unwrap_or_default(),
        );
        fields.insert("Concept".into(), sanitize(&self.concept_id));
        fields.insert("BenkyouGuid".into(), note_guid(&self.concept_id, self.role));

        let mut tags: Vec<String> = self
            .tags
            .iter()
            .map(|t| normalize_tag(t))
            .filter(|t| !t.is_empty())
            .collect();
        tags.push(normalize_tag(&format!("benkyou::{}", self.concept_id)));
        tags.sort();
        tags.dedup();

        Note {
            deck_name: deck.to_string(),
            model_name: self.model_name().to_string(),
            fields,
            tags,
        }
    }
}

/// Where AnkiConnect listens, on the machine that is running Anki.
///
/// The add-on binds loopback and nothing here can change that, so this is the only
/// address a local collection is ever at. It is a default rather than a constant for
/// the one case that is not local: Anki on another machine, reached through a
/// forwarded port, where the destination is whichever local port carries the tunnel.
pub const DEFAULT_ADDR: &str = "127.0.0.1:8765";

/// A minimal AnkiConnect client.
///
/// AnkiConnect is one JSON POST to a loopback address, so this speaks HTTP/1.1
/// directly rather than pulling in an async runtime for one request.
#[derive(Debug)]
pub struct AnkiConnect {
    pub addr: String,
    pub timeout: Duration,
}

impl Default for AnkiConnect {
    fn default() -> Self {
        Self { addr: DEFAULT_ADDR.into(), timeout: Duration::from_secs(10) }
    }
}

impl AnkiConnect {
    /// `HOST:PORT`, as `TcpStream::connect` reads it — a name resolves, so a tunnel
    /// endpoint or a LAN hostname both work.
    ///
    /// Checked here rather than left to the connect: `push` builds every note before
    /// it opens a socket, so a malformed address would otherwise be reported after the
    /// work, as a resolver error quoting something the caller never typed. The two
    /// wrong guesses worth naming are a URL — this posts to a bare address and has no
    /// scheme or path to honour — and a bare IPv6 address, whose colons are
    /// indistinguishable from the port separator without brackets.
    pub fn new(addr: &str) -> Result<Self, String> {
        let addr = addr.trim();
        let bad = |why: &str| Err(format!("anki address `{addr}`: {why}"));
        if addr.contains("://") {
            return bad("expected HOST:PORT, not a URL");
        }
        let Some((host, port)) = addr.rsplit_once(':') else {
            return bad(&format!("expected HOST:PORT, as in {DEFAULT_ADDR}"));
        };
        if host.is_empty() {
            return bad("no host before the port");
        }
        if host.contains(':') && !host.starts_with('[') {
            return bad("an IPv6 address needs brackets, as in [::1]:8765");
        }
        match port.parse::<u16>() {
            Ok(p) if p > 0 => {}
            _ => return bad(&format!("`{port}` is not a port")),
        }
        Ok(Self { addr: addr.to_string(), ..Default::default() })
    }

    pub fn call(&self, action: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let body = serde_json::json!({ "action": action, "version": 6, "params": params });
        let body = serde_json::to_string(&body).map_err(|e| e.to_string())?;

        let mut stream = TcpStream::connect(&self.addr)
            .map_err(|e| format!("AnkiConnect at {}: {e} (is Anki running?)", self.addr))?;
        stream.set_read_timeout(Some(self.timeout)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(self.timeout)).map_err(|e| e.to_string())?;

        let request = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.addr,
            body.len(),
            body
        );
        stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&raw);
        let payload = text
            .split("\r\n\r\n")
            .nth(1)
            .ok_or_else(|| "malformed AnkiConnect response".to_string())?;

        let parsed: serde_json::Value =
            serde_json::from_str(payload.trim()).map_err(|e| format!("{e}: {payload}"))?;
        match parsed.get("error") {
            Some(serde_json::Value::Null) | None => {
                Ok(parsed.get("result").cloned().unwrap_or(serde_json::Value::Null))
            }
            Some(e) => Err(format!("AnkiConnect: {e}")),
        }
    }

    pub fn version(&self) -> Result<u64, String> {
        self.call("version", serde_json::json!({}))?
            .as_u64()
            .ok_or_else(|| "version was not a number".into())
    }

    /// Find the note carrying this GUID, if it already exists.
    pub fn find_by_guid(&self, guid: &str) -> Result<Option<u64>, String> {
        let query = format!("BenkyouGuid:\"{guid}\"");
        let found = self.call("findNotes", serde_json::json!({ "query": query }))?;
        Ok(found
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_u64()))
    }
}

pub const CARD_CSS: &str = include_str!("../assets/card.css");
const BASIC_QFMT: &str = include_str!("../assets/basic.qfmt.html");
const BASIC_AFMT: &str = include_str!("../assets/basic.afmt.html");
const CLOZE_QFMT: &str = include_str!("../assets/cloze.qfmt.html");
const CLOZE_AFMT: &str = include_str!("../assets/cloze.afmt.html");

/// Field order is a contract: the templates and the note builder both depend on it.
pub const FIELDS: &[&str] = &["Front", "Back", "Example", "Concept", "BenkyouGuid"];

#[derive(Debug, Clone, PartialEq)]
pub struct PushReport {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl AnkiConnect {
    /// Create the two notetypes if they are absent. Existing ones are left alone:
    /// silently restyling a collection the user has customised would be rude.
    pub fn ensure_models(&self) -> Result<Vec<String>, String> {
        let existing = self.call("modelNames", serde_json::json!({}))?;
        let have: Vec<String> = existing
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();

        let mut created = Vec::new();
        for (name, is_cloze, qfmt, afmt) in [
            (MODEL_BASIC, false, BASIC_QFMT, BASIC_AFMT),
            (MODEL_CLOZE, true, CLOZE_QFMT, CLOZE_AFMT),
        ] {
            if have.iter().any(|m| m == name) {
                continue;
            }
            self.call(
                "createModel",
                serde_json::json!({
                    "modelName": name,
                    "inOrderFields": FIELDS,
                    "css": CARD_CSS,
                    "isCloze": is_cloze,
                    "cardTemplates": [{
                        "Name": if is_cloze { "Cloze" } else { "Card 1" },
                        "Front": qfmt,
                        "Back": afmt,
                    }],
                }),
            )?;
            created.push(name.to_string());
        }
        Ok(created)
    }

    /// Add cards, updating any whose GUID is already present.
    ///
    /// This is what makes re-projecting the graph safe: a regenerated card updates the
    /// existing note in place, so its review history and FSRS state survive.
    pub fn push(&self, cards: &[Card], deck: &str) -> Result<PushReport, String> {
        self.call("createDeck", serde_json::json!({ "deck": deck }))?;
        let mut report = PushReport { added: vec![], updated: vec![], failed: vec![] };

        for card in cards {
            let guid = note_guid(&card.concept_id, card.role);
            let note = card.to_note(deck);
            match self.find_by_guid(&guid) {
                Ok(Some(id)) => {
                    let r = self.call(
                        "updateNoteFields",
                        serde_json::json!({ "note": { "id": id, "fields": note.fields } }),
                    );
                    match r {
                        Ok(_) => report.updated.push(guid),
                        Err(e) => report.failed.push((guid, e)),
                    }
                }
                Ok(None) => {
                    let r = self.call("addNote", serde_json::json!({ "note": note }));
                    match r {
                        Ok(_) => report.added.push(guid),
                        Err(e) => report.failed.push((guid, e)),
                    }
                }
                Err(e) => report.failed.push((guid, e)),
            }
        }
        Ok(report)
    }
}
