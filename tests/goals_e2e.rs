//! The discovery verb, against a real store.
//!
//! `goals` is the first command anyone runs and the one that tells an agent where to
//! write a new graph. It has to work on a machine where nothing exists yet, and it has
//! to keep working when part of the store is corrupt — a half-written file is precisely
//! when you need to see what you still have. Both properties were broken when this was
//! written, and both are one stray `?` away from breaking again, so they are pinned
//! here rather than in a library test: the behaviour lives in the binary's argument
//! handling and only a real process exercises it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn store(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("benkyou-goals-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp store");
    dir
}

/// Run `benkyou goals` against an isolated store. Returns the exit status and parsed
/// stdout, because "did it exit zero" is half of what is under test.
fn goals(data_home: &Path) -> (bool, serde_json::Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_benkyou"))
        .arg("goals")
        .env("XDG_DATA_HOME", data_home)
        .output()
        .expect("run benkyou");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let parsed = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "goals did not print JSON ({e}): stdout={stdout:?} stderr={:?}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.success(), parsed)
}

/// A graph these tests can store and read back. Written out here rather than loaded
/// from a file in the repo: this tool ships no goal corpus, because the graph is the
/// thing the user is supposed to build for their own domain. Two nodes and one edge
/// is all any assertion below needs.
fn real_graph() -> String {
    r#"{
      "goal": { "id": "ramp", "target": "ship a service on the new stack",
                "deadline": "2026-09-01", "budget_hours": 40 },
      "nodes": [
        { "id": "first", "title": "the prerequisite", "probe": "explain it",
          "kind": "concept", "goals": ["know it"], "cost_minutes": 20,
          "relevance": 1.0, "provenance": "user", "gradable": true },
        { "id": "second", "title": "the thing that needs it", "probe": "do it",
          "kind": "skill", "goals": ["do it"], "cost_minutes": 30,
          "relevance": 1.0, "provenance": "user", "gradable": true }
      ],
      "edges": [
        { "from": "first", "to": "second", "type": "requires", "strength": 1.0,
          "confidence": 1.0, "reason": "you cannot do the second without the first",
          "needs_goals": [], "provenance": "user" }
      ]
    }"#
    .to_string()
}

/// Counted from the fixture rather than written down at each assertion, so editing
/// the graph above cannot fail a test for a reason unrelated to what it checks.
fn real_graph_nodes() -> usize {
    serde_json::from_str::<serde_json::Value>(&real_graph())
        .expect("fixture parses")["nodes"]
        .as_array()
        .expect("fixture nodes")
        .len()
}

/// On a machine where nothing has been stored, the directory it names must exist
/// afterwards. The binary never writes the first graph — the agent does — so if this
/// command does not create the directory, nothing will, and the agent's first write
/// fails with ENOENT on the documented happy path.
#[test]
fn a_fresh_machine_gets_a_directory_it_can_actually_write_into() {
    let home = store("fresh");
    let (ok, v) = goals(&home);

    assert!(ok, "goals failed on a fresh store");
    assert_eq!(v["goals"].as_array().expect("goals array").len(), 0);

    let dir = PathBuf::from(v["dir"].as_str().expect("dir"));
    assert!(dir.is_dir(), "reported {} but did not create it", dir.display());

    // The directory it named is the one a bare goal name resolves to.
    fs::write(dir.join("newjob.json"), real_graph()).expect("agent's first write");
    let (ok, v) = goals(&home);
    assert!(ok);
    assert_eq!(v["goals"][0]["name"], "newjob");
    assert_eq!(v["goals"][0]["nodes"], real_graph_nodes());
}

/// A corrupt graph is named in place; every readable goal still lists. Failing the
/// whole command would hide the store exactly when the user needs to see it.
#[test]
fn an_unreadable_graph_does_not_hide_the_readable_ones() {
    let home = store("badgraph");
    let (_, v) = goals(&home);
    let dir = PathBuf::from(v["dir"].as_str().unwrap());
    fs::write(dir.join("good.json"), real_graph()).unwrap();
    fs::write(dir.join("truncated.json"), r#"{"goal": {"id": "tr"#).unwrap();

    let (ok, v) = goals(&home);
    assert!(ok, "one bad file failed the whole listing");

    let goals = v["goals"].as_array().unwrap();
    assert_eq!(goals.len(), 2, "{goals:#?}");
    assert_eq!(goals[0]["name"], "good");
    assert_eq!(goals[0]["nodes"], real_graph_nodes());
    assert!(goals[0]["unreadable"].is_null(), "healthy goal marked broken");

    assert_eq!(goals[1]["name"], "truncated");
    assert!(
        goals[1]["unreadable"].is_string(),
        "the broken goal was not named: {:#?}",
        goals[1]
    );
}

/// The fluency sibling rots independently of the graph. Losing practice history does
/// not make the goal unreadable, and it must not take the listing down with it — this
/// is the same bug as above, one file over, and it survived the first fix.
#[test]
fn an_unreadable_fluency_file_costs_only_the_practice_counts() {
    let home = store("badfluency");
    let (_, v) = goals(&home);
    let dir = PathBuf::from(v["dir"].as_str().unwrap());
    fs::write(dir.join("ramp.json"), real_graph()).unwrap();
    fs::write(dir.join("ramp.fluency.json"), r#"{"sql_joins": {"confid"#).unwrap();

    let (ok, v) = goals(&home);
    assert!(ok, "a corrupt fluency file failed the whole listing");

    let g = &v["goals"][0];
    assert_eq!(g["name"], "ramp");
    assert_eq!(g["nodes"], real_graph_nodes(), "the graph half was still readable");
    assert!(
        g["fluency_unreadable"].is_string(),
        "the loss was silent: {g:#?}"
    );
    // Reported in place of the counts, never a zero that reads as "nothing practised".
    assert!(g["practised"].is_null(), "a broken file was counted as 0");
}

/// The fluency file shares its goal's directory and stem prefix. Listing it as a goal
/// would send the caller off to parse practice history as a graph.
#[test]
fn a_healthy_store_reports_practice_and_never_lists_the_fluency_sibling() {
    let home = store("healthy");
    let (_, v) = goals(&home);
    let dir = PathBuf::from(v["dir"].as_str().unwrap());
    fs::write(dir.join("ramp.json"), real_graph()).unwrap();
    fs::write(
        dir.join("ramp.fluency.json"),
        r#"{"sql_joins": {"confidence": 2.0, "last_practiced": 0, "attempts": 1, "best_score": 1.0}}"#,
    )
    .unwrap();

    let (ok, v) = goals(&home);
    assert!(ok);
    let goals = v["goals"].as_array().unwrap();
    assert_eq!(goals.len(), 1, "the fluency sibling was listed: {goals:#?}");
    assert_eq!(goals[0]["practised"], 1);
    // 2.0 is past the default retire_at, so it counts as retired rather than merely seen.
    assert_eq!(goals[0]["retired"], 1);
    assert!(goals[0]["fluency_unreadable"].is_null(), "healthy run reported a loss");
}

/// `practised` has to mean "sat down to this", not "a record exists". An `encompasses`
/// edge opens a fluency entry for the node underneath at zero attempts, and counting
/// those told the learner they had drilled something they never once attempted — the
/// same overstatement as counting a claim as knowledge.
#[test]
fn credit_from_an_encompasses_edge_is_not_counted_as_practice() {
    let home = store("credit");
    let (_, v) = goals(&home);
    let dir = PathBuf::from(v["dir"].as_str().unwrap());
    fs::write(dir.join("ramp.json"), real_graph()).unwrap();

    // `drilled` was attempted. `bystander` only ever received credit.
    fs::write(
        dir.join("ramp.fluency.json"),
        r#"{
          "drilled":   {"confidence": 1.0, "last_practiced": 0, "attempts": 1, "best_score": 1.0},
          "bystander": {"confidence": 0.5, "last_practiced": 0, "attempts": 0, "best_score": 0.0}
        }"#,
    )
    .unwrap();

    let (ok, v) = goals(&home);
    assert!(ok);
    let g = &v["goals"][0];
    assert_eq!(g["practised"], 1, "credit was counted as practice: {g:#?}");
    assert_eq!(g["credited"], 1, "the credited node went unreported: {g:#?}");
}
