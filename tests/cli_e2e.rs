//! Two things the binary owes an agent that has never run it before.
//!
//! Three blind-test agents lost calls to the same two walls. `--help` on a subcommand
//! answered with a missing argument, and nothing showed the shape of a goal file.
//! Both live in argument handling and in printed output, so both run a real process.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use benkyou::graph::{EdgeType, Graph, ValidationReport, NODE_CAP, RELEVANCE_FLOOR};

fn benkyou(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_benkyou"))
        .args(args)
        .output()
        .expect("run benkyou");
    (
        out.status.success(),
        String::from_utf8(out.stdout).expect("utf8 stdout"),
        String::from_utf8(out.stderr).expect("utf8 stderr"),
    )
}

/// Asking a subcommand for help must not be read as asking it to do its job. `--help`
/// sits where a `<goal>` is expected, and every form below once answered `missing <goal>`.
#[test]
fn help_is_answered_from_any_position() {
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["help"],
        vec!["validate", "--help"],
        vec!["order", "--help"],
        vec!["seed", "--help"],
        vec!["schema", "--help"],
        // Trailing, after arguments that already parsed. The flag scan runs over the
        // whole line, not only the position a goal occupies.
        vec!["practice", "some-goal", "some-node", "--help"],
        vec!["grade", "some-dir", "--work", "/tmp/nowhere", "-h"],
    ] {
        let (ok, stdout, stderr) = benkyou(&args);
        assert!(ok, "`benkyou {}` failed: {stderr}", args.join(" "));
        // The tagline is prose and free to change. This test defends the fact that a
        // help request anywhere on the line prints usage, not an error.
        assert!(
            stdout.starts_with("benkyou — ") && stdout.contains("\nUSAGE\n"),
            "`benkyou {}` printed {stdout:?} instead of usage",
            args.join(" ")
        );
    }
}

/// A goal name that happens to be `help` is still a goal name. Only the dashed forms
/// are recognised late in the line, because a bare word there is data.
#[test]
fn a_bare_help_after_the_verb_is_not_a_help_request() {
    let (ok, stdout, stderr) = benkyou(&["validate", "help"]);
    assert!(
        !ok,
        "validate on a goal named `help` should have failed: {stdout}"
    );
    assert!(
        stderr.contains("help"),
        "expected the goal `help` to be looked up, got {stderr:?}"
    );
}

/// What the command prints must be usable as written. A graph that parses and then
/// loses a node to repair teaches the next agent a shape the tool corrects.
#[test]
fn the_printed_schema_needs_no_repair() {
    let (ok, stdout, stderr) = benkyou(&["schema"]);
    assert!(ok, "schema failed: {stderr}");

    let mut graph: Graph = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("schema did not print a Graph ({e}): {stdout}"));
    let (nodes, edges) = (graph.nodes.len(), graph.edges.len());

    let report = graph.validate(RELEVANCE_FLOOR, NODE_CAP);
    assert_eq!(
        report,
        ValidationReport::default(),
        "the example graph repaired itself on first contact"
    );
    assert_eq!(graph.nodes.len(), nodes, "validate dropped a node");
    assert_eq!(graph.edges.len(), edges, "validate dropped an edge");
}

/// The example has to show all three edge types. `requires` is the only one that blocks.
/// `encompasses` is the only one that pays practice credit. An agent that never sees
/// them cannot write them.
#[test]
fn the_printed_schema_demonstrates_every_edge_type() {
    let (_, stdout, _) = benkyou(&["schema"]);
    let graph: Graph = serde_json::from_str(&stdout).expect("schema is a Graph");

    for ty in [EdgeType::Requires, EdgeType::Helps, EdgeType::Encompasses] {
        assert!(
            graph.edges.iter().any(|e| e.ty == ty),
            "no {ty:?} edge in the example graph"
        );
    }
    // A node without goals produces a vaguer generation order, so the example must not
    // model one.
    for n in &graph.nodes {
        assert!(!n.goals.is_empty(), "node `{}` has no goals", n.id);
    }
}

/// Every agent copies the example's shape, and a reversed `requires` inverts the
/// curriculum silently. Asserted through the graph's own traversals, not by reading
/// `from`/`to`, so this tracks what the scheduler believes.
#[test]
fn the_printed_schema_points_its_edges_the_way_the_engine_reads_them() {
    let (_, stdout, _) = benkyou(&["schema"]);
    let graph: Graph = serde_json::from_str(&stdout).expect("schema is a Graph");

    // `requires`: from is the prerequisite. Debugging a crashloop needs the lifecycle,
    // so the lifecycle comes back as an ancestor of the debugging node.
    assert!(
        graph
            .requires_ancestors("debug_crashloop")
            .contains("pod_lifecycle"),
        "requires is reversed: `pod_lifecycle` is not a prerequisite of `debug_crashloop`"
    );
    assert!(
        graph.requires_ancestors("pod_lifecycle").is_empty(),
        "requires is reversed: the prerequisite has prerequisites of its own"
    );

    // `encompasses`: practising `to` credits `from`, so the harder node is `to`.
    let bridge = graph
        .edges
        .iter()
        .find(|e| e.ty == EdgeType::Encompasses)
        .expect("an encompasses edge");
    assert_eq!(
        (bridge.from.as_str(), bridge.to.as_str()),
        ("pod_lifecycle", "debug_crashloop"),
        "encompasses is reversed: working the kata must credit the concept under it"
    );
}

/// The documented use is `benkyou schema > goal.json && benkyou validate goal.json`.
/// That path goes through goal resolution and the store's loader. The in-process check
/// above touches neither.
#[test]
fn the_printed_schema_validates_clean_through_the_store() {
    let dir = std::env::temp_dir().join("benkyou-cli-schema");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    let path: PathBuf = dir.join("example.json");

    let (_, stdout, _) = benkyou(&["schema"]);
    fs::write(&path, &stdout).expect("write goal file");

    let (ok, out, stderr) = benkyou(&["validate", path.to_str().expect("utf8 path")]);
    assert!(ok, "validate failed: {stderr}");
    let v: serde_json::Value = serde_json::from_str(&out).expect("validate prints JSON");
    assert_eq!(v["clean"], true, "validate reported repairs: {out}");
}

/// What `validate` says about a cycle has to match what it did to the file.
///
/// The first refusal printed "Nothing was cut" unconditionally. That is false when a
/// cycle shares a graph with a sub-floor node, which is dropped on the same run. A
/// blind-test agent believed re-running was free and lost a node.
#[test]
fn a_refused_cycle_still_admits_the_repairs_it_made() {
    let dir = std::env::temp_dir().join("benkyou-cycle-honesty");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch");

    let (_, schema, _) = benkyou(&["schema"]);
    let mut graph: Graph = serde_json::from_str(&schema).expect("schema is a Graph");

    // A cycle, and one entirely unrelated mechanical defect alongside it.
    let mut back = graph.edges[0].clone();
    std::mem::swap(&mut back.from, &mut back.to);
    back.reason = "deliberate cycle".to_string();
    graph.edges.push(back);
    let mut junk = graph.nodes[0].clone();
    junk.id = "junk".to_string();
    junk.relevance = 0.1;
    graph.nodes.push(junk);

    let path = dir.join("both.json");
    fs::write(&path, serde_json::to_string(&graph).unwrap()).expect("write");
    let (ok, _, stderr) = benkyou(&["validate", path.to_str().unwrap()]);

    assert!(!ok, "a cycle must fail the command");
    assert!(
        stderr.contains("No cycle edge was cut"),
        "the refusal stopped saying what it protected: {stderr}"
    );
    assert!(
        stderr.contains("repair"),
        "the repair it made went unmentioned: {stderr}"
    );

    // And the claim is true of the file: every edge survives, the sub-floor node does not.
    let after: Graph = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(after.edges.len(), graph.edges.len(), "a cycle edge was cut");
    assert!(!after.nodes.iter().any(|n| n.id == "junk"));
}

/// `needs_goals` is `Vec<usize>`, and an example that only prints `[]` cannot show that.
/// The first agent to write a graph from this schema filled it with goal text, and the
/// file did not parse.
#[test]
fn the_printed_schema_shows_what_needs_goals_holds() {
    let (_, stdout, _) = benkyou(&["schema"]);
    let graph: Graph = serde_json::from_str(&stdout).expect("schema is a Graph");

    let demo = graph
        .edges
        .iter()
        .find(|e| !e.needs_goals.is_empty())
        .expect("no edge demonstrates a non-empty needs_goals");
    let from = graph
        .nodes
        .iter()
        .find(|n| n.id == demo.from)
        .expect("the edge names a real node");
    for &i in &demo.needs_goals {
        assert!(
            i < from.goals.len(),
            "needs_goals index {i} is out of range for `{}`, which has {} goals",
            from.id,
            from.goals.len()
        );
    }
}

/// `kind` says a node is a performance. `gradable` says a script can mark one. They come
/// apart on the nodes a study tool is most tempted to fake. So the example shows a
/// `skill` on the critical path that no `check.sh` can judge.
#[test]
fn the_printed_schema_shows_a_skill_no_grader_can_judge() {
    let (_, stdout, _) = benkyou(&["schema"]);
    let graph: Graph = serde_json::from_str(&stdout).expect("schema is a Graph");

    let ungradable: Vec<_> = graph.nodes.iter().filter(|n| !n.gradable).collect();
    assert_eq!(
        ungradable.len(),
        1,
        "the example must demonstrate `gradable: false` exactly once"
    );
    let n = ungradable[0];
    assert!(
        benkyou::order::is_practicable_kind(n.kind),
        "a kind that already refuses exercises demonstrates nothing about `gradable`"
    );
    // Wired into the graph, not parked beside it.
    assert!(
        graph.edges.iter().any(|e| e.from == n.id || e.to == n.id),
        "`{}` is disconnected, so refusing it costs the example nothing",
        n.id
    );
    // And every other node stays gradable, so the flag reads as the exception it is.
    assert_eq!(
        graph.nodes.len() - 1,
        graph.nodes.iter().filter(|x| x.gradable).count()
    );
}

/// The refusal has to name the way forward. A dead end here sent one blind-test agent
/// off to write a `check.sh` for a spoken monologue.
#[test]
fn an_exercise_order_for_an_ungradable_node_points_at_practice() {
    let dir = std::env::temp_dir().join("benkyou-ungradable");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("benkyou/goals")).expect("scratch");

    let (_, schema, _) = benkyou(&["schema"]);
    fs::write(dir.join("benkyou/goals/k.json"), &schema).expect("write goal");

    let out = Command::new(env!("CARGO_BIN_EXE_benkyou"))
        .args([
            "order",
            "k",
            "--kind",
            "exercise",
            "--node",
            "incident_writeup",
        ])
        .env("XDG_DATA_HOME", &dir)
        .output()
        .expect("run benkyou");
    let stderr = String::from_utf8(out.stderr).expect("utf8");

    assert!(
        !out.status.success(),
        "an ungradable node was handed an exercise order"
    );
    assert!(
        stderr.contains("practice"),
        "the refusal named no way forward: {stderr}"
    );
    assert!(
        stderr.contains("incident_writeup"),
        "the refusal did not say which node: {stderr}"
    );
}

/// With no `--node` the scheduler picks the target. Ignoring `gradable` hands back a node
/// the next step refuses.
///
/// `focus` breaks ties on node id ascending, so `aaa_ungradable` outranks `zzz_gradable`
/// and an unfiltered scheduler stops on it with a gradable node available.
#[test]
fn auto_selection_passes_over_an_ungradable_node_for_one_it_can_use() {
    let dir = std::env::temp_dir().join("benkyou-autoskip");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("benkyou/goals")).expect("scratch");

    let node = |id: &str, gradable: bool| {
        serde_json::json!({
            "id": id, "title": id, "kind": "skill",
            "probe": "Do the thing.", "goals": ["Do it"],
            "cost_minutes": 20, "relevance": 1.0, "provenance": "user",
            "gradable": gradable
        })
    };
    let graph = serde_json::json!({
        "goal": { "id": "k", "target": "ship it", "budget_hours": 4 },
        "nodes": [node("aaa_ungradable", false), node("zzz_gradable", true)],
        "edges": []
    });
    fs::write(
        dir.join("benkyou/goals/k.json"),
        serde_json::to_string(&graph).unwrap(),
    )
    .expect("write goal");

    let out = Command::new(env!("CARGO_BIN_EXE_benkyou"))
        .args(["order", "k", "--kind", "exercise"])
        .env("XDG_DATA_HOME", &dir)
        .output()
        .expect("run benkyou");

    assert!(
        out.status.success(),
        "the scheduler stopped on the ungradable node instead of passing over it: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("order printed JSON");
    assert_eq!(
        body["node"]["id"], "zzz_gradable",
        "wrong target chosen: {body}"
    );
}

/// When the only work left is a performance nobody can grade, the refusal has to say so.
///
/// It used to print "nothing is practisable - every unlocked concept is at target". That
/// reads as "you are finished" when a ramp ends on the call you still owe. It is also
/// false, because `session` still returns that node.
#[test]
fn the_bare_order_dead_end_names_the_node_and_the_way_out() {
    let dir = std::env::temp_dir().join("benkyou-deadend");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("benkyou/goals")).expect("scratch");

    let graph = serde_json::json!({
        "goal": { "id": "d", "target": "run the call", "budget_hours": 4 },
        "nodes": [{
            "id": "incident_command", "title": "run the call", "kind": "skill",
            "probe": "Run the bridge for ten minutes.", "goals": ["Keep the cadence"],
            "cost_minutes": 30, "relevance": 1.0, "provenance": "user",
            "gradable": false
        }],
        "edges": []
    });
    fs::write(
        dir.join("benkyou/goals/d.json"),
        serde_json::to_string(&graph).unwrap(),
    )
    .expect("write goal");

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_benkyou"))
            .args(args)
            .env("XDG_DATA_HOME", &dir)
            .output()
            .expect("run benkyou")
    };

    let out = run(&["order", "d", "--kind", "exercise"]);
    let stderr = String::from_utf8(out.stderr).expect("utf8");
    assert!(!out.status.success());
    assert!(
        stderr.contains("incident_command"),
        "no node named: {stderr}"
    );
    assert!(
        stderr.contains("practice"),
        "no way forward offered: {stderr}"
    );
    assert!(
        !stderr.contains("every unlocked concept is at target"),
        "still claims the curriculum is finished: {stderr}"
    );

    // The claim it must not make: `session` disagrees, and that is the bug.
    let session = run(&["session", "d", "--size", "1"]);
    let body: serde_json::Value =
        serde_json::from_slice(&session.stdout).expect("session printed JSON");
    assert_eq!(
        body["session"][0], "incident_command",
        "fixture no longer reproduces the case: {body}"
    );
}

/// A score you assigned yourself is the weakest evidence the tool takes. On a node a
/// grader can judge, that is the self-preference problem the design avoids. A kata done
/// on paper is a real attempt, so it is allowed, but never silent.
#[test]
fn hand_scoring_something_a_grader_could_judge_is_flagged() {
    let dir = std::env::temp_dir().join("benkyou-selfscore");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("benkyou/goals")).expect("scratch");

    let node = |id: &str, gradable: bool| {
        serde_json::json!({
            "id": id, "title": id, "kind": "skill", "probe": "Do it.", "goals": ["g"],
            "cost_minutes": 10, "relevance": 1.0, "provenance": "user", "gradable": gradable
        })
    };
    fs::write(
        dir.join("benkyou/goals/s.json"),
        serde_json::to_string(&serde_json::json!({
            "goal": { "id": "s", "target": "t", "budget_hours": 4 },
            "nodes": [node("markable", true), node("performance", false)],
            "edges": []
        }))
        .unwrap(),
    )
    .expect("write goal");

    let practice = |id: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_benkyou"))
            .args(["practice", "s", id, "0.7"])
            .env("XDG_DATA_HOME", &dir)
            .output()
            .expect("run benkyou");
        assert!(out.status.success(), "practice refused `{id}`");
        serde_json::from_slice::<serde_json::Value>(&out.stdout).expect("JSON")
    };

    let flagged = practice("markable");
    assert!(
        flagged["warning"]
            .as_str()
            .unwrap_or("")
            .contains("markable"),
        "hand-scoring a gradable node went unremarked: {flagged}"
    );
    // Still recorded. The point is to mark it, not to block it. f32 through JSON, so
    // compare with a tolerance.
    let scored = flagged["score"].as_f64().expect("a score");
    assert!((scored - 0.7).abs() < 1e-6, "score not recorded: {scored}");

    assert!(
        practice("performance")["warning"].is_null(),
        "the node the flag exists for was flagged instead"
    );
}

/// The skill and the binary install from different URLs, so an agent has to ask what it
/// holds. Pinned to the crate version, so a release bump cannot leave this printing the
/// last number. Answered from any position, for the same reason `--help` is.
#[test]
fn the_version_is_the_crate_version_from_any_position() {
    let expected = format!("benkyou {}", env!("CARGO_PKG_VERSION"));

    for args in [vec!["--version"], vec!["-V"], vec!["goals", "--version"]] {
        let (ok, stdout, stderr) = benkyou(&args);
        assert!(ok, "{args:?} must succeed, got: {stderr}");
        assert_eq!(
            stdout.trim(),
            expected,
            "{args:?} must report the crate version"
        );
    }
}
