//! Generation orders: what the binary asks the host agent to write.
//!
//! The tool holds the state and emits the work; the agent already in a conversation
//! with a model fills it in and writes the result back through `cards`, `gate`, or an
//! edit to the graph. See DESIGN.md §6.
//!
//! An order is not a template. Everything in it that a template could not supply comes
//! from the learner's state: which node is worth generating for, which prerequisites
//! may be assumed, which dependents must not be spoiled, and how much of the solution
//! to show.

use serde_json::{json, Value};

use crate::graph::{EdgeType, Graph, Kind, NodeId, Verdict};

/// What the agent is being asked to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKind {
    /// Flashcards for a concept the learner has unlocked.
    Cards,
    /// A graded exercise directory.
    Exercise,
    /// A replacement interview question for a probe that measured nothing.
    Probe,
}

impl OrderKind {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "cards" => Ok(Self::Cards),
            "exercise" => Ok(Self::Exercise),
            "probe" => Ok(Self::Probe),
            other => Err(format!("order: kind must be cards|exercise|probe, got `{other}`")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cards => "cards",
            Self::Exercise => "exercise",
            Self::Probe => "probe",
        }
    }
}

/// How much of the solution to show, decided from the assessment rather than by default.
///
/// Worked examples help novices and actively harm knowledgeable learners, so a node the
/// learner has demonstrably failed gets the full solution and a node they have passed
/// gets nothing. The middle case — a belief with no evidence behind it — is faded.
fn guidance_for(graph: &Graph, node: &str) -> &'static str {
    if graph.state.unknown.contains(node) {
        "worked"
    } else if graph.state.known.contains(node) {
        "blank"
    } else {
        "faded"
    }
}

/// Direct `requires`-dependents: what this node is a prerequisite *for*.
fn unlocks(graph: &Graph, node: &str) -> Vec<NodeId> {
    graph
        .edges
        .iter()
        .filter(|e| e.ty == EdgeType::Requires && e.from == node)
        .map(|e| e.to.clone())
        .collect()
}

/// Whether a graded exercise can even exist for a node of this kind.
///
/// An exercise is passed by *doing* something a grader can run. `Fact`, `Concept`
/// and `Context` nodes are knowledge, not performance: the honest artifact for them
/// is a card, and asking for a kata produces a fake one that grades trivia.
pub fn is_practicable_kind(kind: Kind) -> bool {
    matches!(kind, Kind::Skill | Kind::Tool)
}

fn titled(graph: &Graph, ids: impl IntoIterator<Item = NodeId>) -> Vec<Value> {
    ids.into_iter()
        .filter_map(|id| graph.node(&id))
        .map(|n| json!({ "id": n.id, "title": n.title }))
        .collect()
}

/// The most recent probe on `node` the learner rejected as unanswerable.
fn rejected_probe(graph: &Graph, node: &str) -> Option<String> {
    graph
        .state
        .evidence
        .iter()
        .rev()
        .find(|e| e.node == node && e.verdict == Verdict::Skip)
        .map(|e| e.probe.clone())
}

/// Every node whose current probe has been skipped and not since answered.
///
/// A skip after an answer is the live signal; an answer after a skip means the
/// replacement probe already worked, so the node drops off the list.
pub fn probes_needing_rewrite(graph: &Graph) -> Vec<NodeId> {
    let mut out: Vec<NodeId> = Vec::new();
    for node in graph.nodes.iter().map(|n| &n.id) {
        let last = graph
            .state
            .evidence
            .iter()
            .rev()
            .find(|e| &e.node == node)
            .map(|e| &e.verdict);
        if last == Some(&Verdict::Skip) {
            out.push(node.clone());
        }
    }
    out
}

/// Build the order for `node`.
pub fn build(graph: &Graph, kind: OrderKind, node: &str, count: usize) -> Result<Value, String> {
    let n = graph
        .node(node)
        .ok_or_else(|| format!("order: no node `{node}` in this graph"))?;
    if kind == OrderKind::Exercise && !is_practicable_kind(n.kind) {
        return Err(format!(
            "order: `{node}` is a {} node — there is nothing to run, so no grader can \
             judge it. Use --kind cards.",
            format!("{:?}", n.kind).to_lowercase()
        ));
    }
    // Being a performance is not the same as being markable. This is the author's own
    // declaration that nothing can judge it, so honour it rather than handing back an
    // order whose `check.sh` cannot be written.
    if kind == OrderKind::Exercise && !n.gradable {
        return Err(format!(
            "order: `{node}` is marked not machine-gradable — no `check.sh` can judge \
             it. Use --kind cards for the knowledge under it, and record the \
             performance itself with `benkyou practice <goal> {node} <score>`."
        ));
    }

    let ancestors = graph.requires_ancestors(node);
    let (known, unproven): (Vec<NodeId>, Vec<NodeId>) = ancestors
        .into_iter()
        .partition(|a| graph.state.known.contains(a));

    let context = json!({
        "assume_known": titled(graph, known),
        "do_not_assume": titled(graph, unproven),
        "unlocks": titled(graph, unlocks(graph, node)),
        "guidance_level": guidance_for(graph, node),
        "resolved": {
            "known": graph.state.known.len(),
            "unknown": graph.state.unknown.len(),
            "of": graph.nodes.len(),
        },
    });

    let work = match kind {
        OrderKind::Cards => cards_order(n.id.as_str(), count),
        OrderKind::Exercise => exercise_order(graph, n.id.as_str()),
        OrderKind::Probe => probe_order(graph, n.id.as_str())?,
    };

    Ok(json!({
        "order": kind.as_str(),
        "goal": {
            "target": graph.goal.target,
            "deadline": graph.goal.deadline,
        },
        "node": {
            "id": n.id,
            "title": n.title,
            "kind": n.kind,
            "goals": n.goals,
            "cost_minutes": n.cost_minutes,
            "probe": n.probe,
        },
        "context": context,
        "write": work.0,
        "submit": work.1,
    }))
}

fn cards_order(id: &str, count: usize) -> (Value, Value) {
    let roles = ["definition", "application", "contrast", "cloze"];
    let write = json!({
        "format": "a JSON array of Card objects, written to a file you choose",
        "count": count.min(roles.len()),
        "schema": {
            "concept_id": id,
            "role": roles,
            "front": "the prompt",
            "back": "the answer",
            "example": "optional, a concrete snippet",
            "tags": ["optional"],
        },
        "rules": [
            "Note identity is concept_id + role, never the text. At most one card per \
             role, and rewriting a card later updates it in place instead of orphaning \
             its review history — so pick the role deliberately.",
            "definition: what it is. application: when you would reach for it. \
             contrast: it versus the thing it is most often confused with. \
             cloze: {{c1::...}} over a canonical snippet, one span per idea.",
            "The front must be answerable in a few seconds. A front that needs a \
             paragraph is an exercise, not a card.",
            "Do not put the answer in the question. A prompt the learner can answer \
             from its own phrasing measures nothing.",
            "Assume everything under context.assume_known. Do not teach it again.",
            "Do not give away anything under context.unlocks; those are still to come.",
        ],
    });
    let submit = json!(format!(
        "benkyou cards <file>            # dry run, prints the notes\n\
         benkyou cards <file> --push     # writes them to Anki"
    ));
    (write, submit)
}

/// Whether advice about SQL dialects could possibly apply to this exercise.
///
/// The dialect rule used to be unconditional, so a spoken-German conjugation drill was
/// told to prefer `CASE WHEN` over `FILTER (WHERE ...)`. The two signals actually in
/// scope are the node and the goal it sits under, and either is enough: a node named
/// for SQL is one, and every node under a SQL goal may be. Matching the bare substring
/// also catches `sqlite`, `mysql` and `postgresql`, which is wanted; it catches `nosql`
/// too, and one stray line of advice there is far cheaper than the silence that let the
/// rule leak into every other domain.
fn is_sql_ish(graph: &Graph, id: &str) -> bool {
    let mentions_sql = |s: &str| s.to_lowercase().contains("sql");
    mentions_sql(&graph.goal.target)
        || graph
            .node(id)
            .is_some_and(|n| mentions_sql(&n.id) || mentions_sql(&n.title))
}

fn exercise_order(graph: &Graph, id: &str) -> (Value, Value) {
    let guidance = guidance_for(graph, id);

    let mut rules = vec![
        format!(
            "guidance_level is `{guidance}`, decided from the assessment, not by taste. \
             worked: show the full solution and have the learner follow it. faded: blank \
             the last step or two. blank: show nothing — a worked example actively harms \
             a learner who has already demonstrated the concept."
        ),
        "kind labels what is being practised; nothing in the tool dispatches on it, \
         because the grader is always verify.cmd. kata: hidden tests over a function. \
         sql: learner query against reference query on one fixture. debug: a pinned \
         repro goes red to green while a guard set stays green. terminal: assert \
         observable system state. artifact: compare a produced file to a reference. \
         The template ships `kata`; change it to whichever of the five describes this."
            .to_string(),
        "The grading data in check/ must differ from the sample in setup/. An exercise \
         graded on the data it showed you rewards hardcoding."
            .to_string(),
        "known_bad is required: name at least one wrong answer that must fail, and the \
         mistake it embodies. Write the answer a learner who half-understands would \
         actually produce, not a syntax error — a candidate that fails because it does \
         not parse tells you nothing about whether the grader measures the concept. \
         This is the only check that can catch a grader you misread the concept into, \
         because everything else you write here agrees with your reading of it."
            .to_string(),
    ];
    if is_sql_ish(graph, id) {
        rules.push(
            "Portable SQL in the reference: CASE WHEN, not FILTER (WHERE ...), which SQL \
             Server does not have."
                .to_string(),
        );
    }
    rules.push(
        "Assume everything under context.assume_known; require nothing under \
         context.do_not_assume."
            .to_string(),
    );
    rules.push(
        "The gate only proves an empty stub fails. Before submitting, check by hand that \
         a plausible wrong answer is rejected and a differently-written correct one is \
         accepted. An exercise that only rejects a blank file is worthless."
            .to_string(),
    );

    let write = json!({
        // Relative to wherever the user keeps their exercise library. This tool ships
        // no exercises: the library is theirs, and its root is not ours to name.
        "path": format!("exercises/{id}/<slug>/"),
        "layout": {
            "task.toml": "metadata, limits, and the verification contract",
            "instruction.md": "what the learner reads: the task, the exact output wanted, \
                               and a warning that grading uses different data",
            "setup/": "copied into the learner's workspace: schema, sample data, and the \
                       stub file they edit",
            "solution/solve.sh": "the reference solution, run with the workspace as cwd",
            "check/check.sh": "the grader. Runs in the run directory with the workspace at \
                               ./work. Writes {\"correctness\": 1.0|0.0, \"detail\": \"...\"} \
                               to ./out/reward.json. Its exit code reports grader health, \
                               never the grade.",
            "check/*": "grading data and reference output. Never copied to the learner.",
        },
        // The nesting here mirrors task.toml one for one — schema_version at the root,
        // the metadata under [task] — because the agent transcribes this object straight
        // into the file and `exercise::Task` rejects anything flatter. Every value is a
        // value the parser accepts, not a menu, so the two shapes cannot drift apart
        // without the round-trip test noticing.
        "task_toml": {
            "schema_version": "1",
            "task": {
                "id": "<slug>-01",
                "concept_id": id,
                "kind": "kata",
                "guidance_level": guidance,
                "generated_by": "<who generated it>",
            },
            "limits": { "setup_secs": 30, "learner_secs": 600, "check_secs": 60 },
            "verify": {
                "cmd": "sh check/check.sh",
                "reward": "reward.json",
                "must_pass": ["correctness"],
                "hidden": true,
            },
            // At least one is required and the gate rejects the exercise without it.
            // Named here rather than left to the rules text because an agent fills in
            // the shape it is given: an absent key is a key that does not get written,
            // and the first thing the author would see is a rejection they have to go
            // and look up.
            "known_bad": [{
                "id": "<short_name_for_the_mistake>",
                "trap": "<the misconception, in one line>",
                "files": { "<file the learner edits>": "<a wrong answer, in full>" },
            }],
        },
        "rules": rules,
    });
    let submit = json!(
        "benkyou gate <exercise-dir> --scratch /tmp/<name>   # must print Validated"
    );
    (write, submit)
}

fn probe_order(graph: &Graph, id: &str) -> Result<(Value, Value), String> {
    let rejected = rejected_probe(graph, id).ok_or_else(|| {
        format!("order: `{id}` has no skipped probe — nothing to rewrite")
    })?;
    let write = json!({
        "field": format!("the `probe` string on node `{id}` in the goal file"),
        "rejected": rejected,
        "rules": [
            "The rejected probe measured nothing. Diagnose which failure it was before \
             writing the replacement: it contained its own answer, it was ambiguous, it \
             tested recall of a phrase rather than use of the idea, or it asked about the \
             topic instead of asking the learner to do something with it.",
            "A good probe can be answered wrongly by someone who half-knows the concept. \
             If every plausible answer is right, it discriminates nothing.",
            "Ask for a judgement or a construction, not a definition to recite.",
            "One question. A probe with three parts grades three things and resolves none.",
        ],
    });
    let submit = json!(
        "edit the probe in the goal file, then: benkyou ask <goal> --node <id>"
    );
    Ok((write, submit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assess;
    use crate::graph::{Edge, Goal, Kind, Node, Provenance, State};

    fn node(id: &str) -> Node {
        Node {
            id: id.to_string(),
            title: format!("the {id} concept"),
            kind: Kind::Concept,
            probe: format!("probe for {id}"),
            goals: vec![format!("goal for {id}")],
            cost_minutes: 20,
            relevance: 1.0,
            provenance: Provenance::Llm,
            gradable: true,
        }
    }

    fn req(from: &str, to: &str) -> Edge {
        Edge {
            from: from.to_string(),
            to: to.to_string(),
            ty: EdgeType::Requires,
            strength: 1.0,
            reason: "r".to_string(),
            needs_goals: Vec::new(),
            provenance: Provenance::Llm,
            confidence: 1.0,
        }
    }

    /// `a -> b -> c`, so `b` has one prerequisite and unlocks one dependent.
    fn chain() -> Graph {
        Graph {
            goal: Goal {
                id: "g".to_string(),
                target: "the target".to_string(),
                deadline: Some("2026-08-19".to_string()),
                budget_hours: 40,
            },
            nodes: vec![node("a"), node("b"), node("c")],
            edges: vec![req("a", "b"), req("b", "c")],
            state: State::default(),
        }
    }

    fn strings(v: &Value) -> Vec<String> {
        v.as_array()
            .expect("not an array")
            .iter()
            .map(|e| e["id"].as_str().expect("no id").to_string())
            .collect()
    }

    /// `chain()` with `b` made runnable, which is the only kind an exercise is legal for.
    fn exercisable_chain() -> Graph {
        let mut g = chain();
        g.nodes[1].kind = Kind::Skill;
        g
    }

    fn exercise_write(g: &Graph) -> Value {
        build(g, OrderKind::Exercise, "b", 1).expect("no order")["write"].clone()
    }

    fn rules_of(g: &Graph) -> Vec<String> {
        exercise_write(g)["rules"]
            .as_array()
            .expect("no rules")
            .iter()
            .map(|r| r.as_str().expect("a rule is not a string").to_string())
            .collect()
    }

    #[test]
    fn an_order_separates_proven_prerequisites_from_merely_assumed_ones() {
        let mut g = chain();
        g.state.known.insert("a".to_string());
        let o = build(&g, OrderKind::Cards, "b", 4).expect("no order");
        assert_eq!(strings(&o["context"]["assume_known"]), vec!["a"]);
        assert!(strings(&o["context"]["do_not_assume"]).is_empty());

        // ...and with nothing proven, the same prerequisite moves sides.
        let g = chain();
        let o = build(&g, OrderKind::Cards, "b", 4).expect("no order");
        assert!(strings(&o["context"]["assume_known"]).is_empty());
        assert_eq!(strings(&o["context"]["do_not_assume"]), vec!["a"]);
    }

    #[test]
    fn an_order_names_the_dependents_it_must_not_spoil() {
        let g = chain();
        let o = build(&g, OrderKind::Cards, "b", 4).expect("no order");
        assert_eq!(strings(&o["context"]["unlocks"]), vec!["c"]);
        assert!(
            strings(&o["context"]["unlocks"]).iter().all(|id| id != "a"),
            "a prerequisite was reported as a dependent"
        );
    }

    #[test]
    fn guidance_comes_from_the_assessment_not_from_a_default() {
        let mut g = chain();
        g.nodes[1].kind = Kind::Skill; // only a runnable kind can carry an exercise
        let faded = build(&g, OrderKind::Exercise, "b", 1).expect("no order");
        assert_eq!(faded["context"]["guidance_level"], "faded");

        g.state.unknown.insert("b".to_string());
        let worked = build(&g, OrderKind::Exercise, "b", 1).expect("no order");
        assert_eq!(
            worked["context"]["guidance_level"], "worked",
            "a failed node was not shown the solution"
        );
        assert_eq!(worked["write"]["task_toml"]["task"]["guidance_level"], "worked");

        g.state.unknown.remove("b");
        g.state.known.insert("b".to_string());
        let blank = build(&g, OrderKind::Exercise, "b", 1).expect("no order");
        assert_eq!(
            blank["context"]["guidance_level"], "blank",
            "a passed node was handed a worked example"
        );
    }

    #[test]
    fn a_cards_order_is_capped_by_the_number_of_roles() {
        let g = chain();
        let o = build(&g, OrderKind::Cards, "b", 99).expect("no order");
        assert_eq!(
            o["write"]["count"], 4,
            "asked for more cards than there are distinct note identities"
        );
    }

    #[test]
    fn a_probe_order_carries_the_text_that_failed() {
        let mut g = chain();
        assert!(
            build(&g, OrderKind::Probe, "b", 1).is_err(),
            "offered to rewrite a probe nobody rejected"
        );

        let mut state = g.state.clone();
        assess::record(&g, &mut state, "b", Verdict::Skip, "why is this wrong?", "t0");
        g.state = state;
        let o = build(&g, OrderKind::Probe, "b", 1).expect("no order");
        assert_eq!(o["write"]["rejected"], "why is this wrong?");
    }

    #[test]
    fn only_a_probe_whose_latest_verdict_is_a_skip_needs_rewriting() {
        let mut g = chain();
        let mut state = g.state.clone();
        assess::record(&g, &mut state, "b", Verdict::Skip, "bad", "t0");
        assess::record(&g, &mut state, "c", Verdict::Skip, "also bad", "t1");
        g.state = state;
        assert_eq!(probes_needing_rewrite(&g), vec!["b", "c"]);

        // The replacement worked, so `b` drops off the list.
        let mut state = g.state.clone();
        assess::record(&g, &mut state, "b", Verdict::Pass, "the rewrite", "t2");
        g.state = state;
        assert_eq!(probes_needing_rewrite(&g), vec!["c"]);
    }

    #[test]
    fn an_order_for_a_node_outside_the_graph_is_refused() {
        let g = chain();
        let err = build(&g, OrderKind::Cards, "zz", 1).expect_err("accepted a ghost node");
        assert!(err.contains("zz"), "{err}");
    }

    #[test]
    fn a_node_with_nothing_to_run_is_refused_an_exercise_but_not_cards() {
        let mut g = chain();
        for (kind, exercisable) in [
            (Kind::Fact, false),
            (Kind::Concept, false),
            (Kind::Context, false),
            (Kind::Skill, true),
            (Kind::Tool, true),
        ] {
            g.nodes[1].kind = kind;
            assert_eq!(
                build(&g, OrderKind::Exercise, "b", 1).is_ok(),
                exercisable,
                "{kind:?} was judged wrongly for an exercise"
            );
            assert!(
                build(&g, OrderKind::Cards, "b", 1).is_ok(),
                "{kind:?} was refused cards, which every kind can carry"
            );
        }

        g.nodes[1].kind = Kind::Fact;
        let err = build(&g, OrderKind::Exercise, "b", 1).expect_err("graded a bare fact");
        assert!(err.contains("fact"), "the refusal does not say what b is: {err}");
        assert!(err.contains("cards"), "the refusal offers no way forward: {err}");
    }

    #[test]
    fn every_kind_emits_the_node_the_agent_needs_and_a_way_to_submit_it() {
        let mut g = chain();
        g.nodes[1].kind = Kind::Skill; // so all three kinds are legal for it
        let mut state = g.state.clone();
        assess::record(&g, &mut state, "b", Verdict::Skip, "bad", "t0");
        g.state = state;
        for kind in [OrderKind::Cards, OrderKind::Exercise, OrderKind::Probe] {
            let o = build(&g, kind, "b", 2).expect("no order");
            assert_eq!(o["order"], kind.as_str());
            assert_eq!(o["node"]["id"], "b");
            assert_eq!(o["node"]["goals"][0], "goal for b");
            assert_eq!(o["goal"]["target"], "the target");
            assert!(
                o["submit"].as_str().is_some_and(|s| s.contains("benkyou")),
                "{kind:?} order has no way to hand the work back: {:?}",
                o["submit"]
            );
            assert!(o["write"]["rules"].as_array().is_some_and(|r| !r.is_empty()));
        }
    }

    #[test]
    fn the_task_template_nests_metadata_the_way_task_toml_does() {
        let t = exercise_write(&exercisable_chain())["task_toml"].clone();
        assert_eq!(t["schema_version"], "1", "schema_version belongs at the root");
        assert_eq!(t["task"]["concept_id"], "b");
        assert_eq!(t["task"]["guidance_level"], "faded");
        assert!(t["task"]["id"].is_string(), "the task needs its own id");
        assert!(
            t["id"].is_null() && t["concept_id"].is_null() && t["guidance_level"].is_null(),
            "metadata is flat again, which is exactly the shape the parser rejects: {t}"
        );
    }

    /// The template is transcribed verbatim, so anything it emits that the loader will
    /// not read is a defect the agent pays for. Parse it with the real loader rather
    /// than restating the schema here, which is how the two drifted apart before.
    #[test]
    fn the_task_template_parses_through_the_real_exercise_loader() {
        let t = exercise_write(&exercisable_chain())["task_toml"].clone();
        let dir = std::env::temp_dir()
            .join(format!("benkyou-order-template-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let text = toml::to_string(&t).expect("the template is not serialisable as TOML");
        std::fs::write(dir.join("task.toml"), &text).expect("write task.toml");

        let task = crate::exercise::load(&dir)
            .unwrap_or_else(|e| panic!("the template the agent transcribes does not parse: {e}\n{text}"));
        assert_eq!(task.schema_version, "1");
        assert_eq!(task.task.concept_id, "b");
        assert_eq!(task.task.guidance_level, crate::exercise::Guidance::Faded);
        assert_eq!(task.task.kind, crate::exercise::Kind::Kata);
        assert_eq!(task.verify.must_pass, ["correctness"]);
        assert_eq!(task.limits.learner_secs, 600, "the template's limits, not the defaults");
        assert!(
            crate::exercise::read_gate(&dir).expect("read gate").is_none(),
            "a template arrives ungated"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sql_dialect_rule_is_withheld_from_exercises_that_are_not_sql() {
        let dialect = |g: &Graph| rules_of(g).iter().any(|r| r.contains("FILTER (WHERE"));

        let g = exercisable_chain();
        assert!(
            !dialect(&g),
            "a node with nothing to do with SQL was told which dialect to write"
        );

        let mut by_title = exercisable_chain();
        by_title.nodes[1].title = "Window functions in SQL".to_string();
        assert!(dialect(&by_title), "the node says SQL and the rule was withheld");

        let mut by_goal = exercisable_chain();
        by_goal.goal.target = "analytics with PostgreSQL".to_string();
        assert!(dialect(&by_goal), "every node under a SQL goal may be a SQL exercise");
    }
}
