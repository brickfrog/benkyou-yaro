//! Adversarial tests for the assessment loop, written independently of the
//! implementation.
//!
//! Two invariants carry the loop. After any `record`, `known` and `unknown` are disjoint,
//! and `known` is downward-closed under `requires`. If either breaks, the closure that
//! lets a 20-question interview cover a 200-node graph stops being sound. The plan then
//! contains things the learner cannot start.

use std::collections::BTreeSet;

use benkyou::assess::*;
use benkyou::graph::*;

fn node(id: &str) -> Node {
    Node {
        id: id.into(),
        title: id.into(),
        kind: Kind::Skill,
        probe: format!("probe {id}"),
        goals: vec![format!("goal of {id}")],
        cost_minutes: 10,
        relevance: 1.0,
        provenance: Provenance::Llm,
        gradable: true,
    }
}

fn req(from: &str, to: &str) -> Edge {
    Edge {
        from: from.into(),
        to: to.into(),
        ty: EdgeType::Requires,
        strength: 1.0,
        reason: "test".into(),
        needs_goals: vec![],
        provenance: Provenance::Llm,
        confidence: 1.0,
    }
}

/// a <- b <- c <- d, plus an isolated z.
fn chain() -> Graph {
    Graph {
        goal: Goal { id: "g".into(), target: "t".into(), deadline: None, budget_hours: 10 },
        nodes: ["a", "b", "c", "d", "z"].iter().map(|n| node(n)).collect(),
        edges: vec![req("a", "b"), req("b", "c"), req("c", "d")],
        state: State::default(),
    }
}

fn check_invariants(g: &Graph, s: &State, ctx: &str) {
    let overlap: Vec<_> = s.known.intersection(&s.unknown).collect();
    assert!(overlap.is_empty(), "{ctx}: known and unknown overlap on {overlap:?}");
    assert!(
        g.is_downward_closed(&s.known),
        "{ctx}: known is not downward-closed: {:?}",
        s.known
    );
}

#[test]
fn a_pass_deep_in_the_chain_collapses_the_whole_ancestor_cone() {
    let g = chain();
    let mut s = State::default();
    record(&g, &mut s, "d", Verdict::Pass, "probe d", "t0");

    assert_eq!(
        s.known.iter().map(|x| x.as_str()).collect::<Vec<_>>(),
        vec!["a", "b", "c", "d"],
        "one answer should resolve four nodes"
    );
    check_invariants(&g, &s, "after pass on d");
}

#[test]
fn a_fail_near_the_root_collapses_the_descendant_cone() {
    let g = chain();
    let mut s = State::default();
    record(&g, &mut s, "a", Verdict::Fail, "probe a", "t0");

    for id in ["a", "b", "c", "d"] {
        assert!(s.unknown.contains(id), "{id} should be unknown");
    }
    assert!(!s.unknown.contains("z"), "the isolated node is unaffected");
    check_invariants(&g, &s, "after fail on a");
}

/// `Partial` must leave the node out of `known`, including when an earlier verdict put
/// it there. Leaving it resolved contradicts the documented semantics and can break
/// downward-closure.
#[test]
fn a_partial_verdict_unresolves_a_previously_known_node() {
    let g = chain();
    let mut s = State::default();

    // First a pass on d marks a..d known.
    record(&g, &mut s, "d", Verdict::Pass, "probe d", "t0");
    assert!(s.known.contains("c"));

    // Then c turns out to be shaky.
    record(&g, &mut s, "c", Verdict::Partial, "probe c", "t1");

    assert!(
        !s.known.contains("c"),
        "Partial must leave the node out of known, so it stays in the plan as review"
    );
    assert!(!s.unknown.contains("c"), "Partial is not a failure either");
    assert!(s.known.contains("a") && s.known.contains("b"), "ancestors stay known");
    check_invariants(&g, &s, "after partial on c");
}

/// Whatever sequence of verdicts arrives, the two invariants must hold at every step.
#[test]
fn invariants_survive_an_arbitrary_verdict_sequence() {
    let g = chain();
    let verdicts = [Verdict::Pass, Verdict::Fail, Verdict::Partial];
    let ids = ["a", "b", "c", "d", "z"];

    // Every ordered pair of (node, verdict) applied twice over, deterministically.
    let mut s = State::default();
    for round in 0..3 {
        for (i, id) in ids.iter().enumerate() {
            let v = verdicts[(i + round) % verdicts.len()].clone();
            record(&g, &mut s, id, v.clone(), "p", "t");
            check_invariants(&g, &s, &format!("round {round}, {id} -> {v:?}"));
        }
    }
}

/// Selection must prefer the node whose answer collapses the most. On a chain that is
/// never an endpoint, which is why the loop is cheap.
#[test]
fn selection_prefers_high_leverage_nodes() {
    let g = chain();
    let s = State::default();
    let cfg = AssessConfig::default();

    match next_step(&g, &s, &BTreeSet::new(), &cfg) {
        Step::Ask(q) => {
            assert_ne!(q.node, "z", "an isolated node resolves only itself");
            assert!(
                q.gain > 1.0,
                "a chain node must beat the isolated node's gain of 1.0, got {}",
                q.gain
            );
        }
        other => panic!("expected a question, got {other:?}"),
    }
}

#[test]
fn an_isolated_node_has_gain_of_exactly_one() {
    let g = chain();
    let s = State::default();
    assert_eq!(gain(&g, &s, "z"), 1.0);
}

#[test]
fn a_resolved_node_is_never_worth_asking() {
    let g = chain();
    let mut s = State::default();
    record(&g, &mut s, "d", Verdict::Pass, "p", "t");
    for id in ["a", "b", "c", "d"] {
        assert_eq!(gain(&g, &s, id), 0.0, "{id} is resolved");
    }
}

/// You cannot do the harder thing while failing the easier one. The first such
/// contradiction is re-asked and must change nothing.
#[test]
fn a_contradictory_failure_is_re_asked_and_changes_nothing() {
    let g = chain();
    let mut s = State::default();

    // Demonstrate two things that require b.
    record(&g, &mut s, "c", Verdict::Pass, "p", "t0");
    record(&g, &mut s, "d", Verdict::Pass, "p", "t1");

    let before_known = s.known.clone();
    let before_unknown = s.unknown.clone();

    let outcome = record(&g, &mut s, "b", Verdict::Fail, "p", "t2");
    assert_eq!(outcome, RecordOutcome::ReAsk, "failing b contradicts passing c and d");
    assert_eq!(s.known, before_known, "a re-ask must not change known");
    assert_eq!(s.unknown, before_unknown, "a re-ask must not change unknown");
    assert!(
        s.evidence.iter().any(|e| e.node == "b"),
        "the contradiction itself is worth recording"
    );

    // Standing by it applies normally.
    let second = record(&g, &mut s, "b", Verdict::Fail, "p", "t3");
    assert_eq!(second, RecordOutcome::Applied, "a repeated failure is accepted");
    check_invariants(&g, &s, "after the re-asked failure is applied");
}

/// A loop driven to termination never asks the same node twice and always stops.
#[test]
fn the_loop_terminates_without_repeating_a_question() {
    let g = chain();
    let mut s = State::default();
    let cfg = AssessConfig { max_questions: 30, min_gain: 0.5 };
    let mut asked = BTreeSet::new();

    let stop = loop {
        match next_step(&g, &s, &asked, &cfg) {
            Step::Ask(q) => {
                assert!(asked.insert(q.node.clone()), "asked {} twice", q.node);
                record(&g, &mut s, &q.node, Verdict::Pass, &q.probe, "t");
                check_invariants(&g, &s, "mid-loop");
                assert!(asked.len() <= g.nodes.len(), "loop is not converging");
            }
            Step::Stop(reason) => break reason,
        }
    };

    assert_eq!(stop, StopReason::Complete, "passing everything resolves the graph");
    assert!(asked.len() < g.nodes.len(), "closure should save at least one question");
}

/// Evidence beats a prior: seeding must not overwrite something already answered.
#[test]
fn seeding_a_prior_does_not_overwrite_evidence() {
    let g = chain();
    let mut s = State::default();
    record(&g, &mut s, "z", Verdict::Fail, "p", "t");

    let unknown_ids = seed_prior(
        &g,
        &mut s,
        &[
            ("z".into(), 0.99),
            ("a".into(), 2.5),
            ("b".into(), f32::NAN),
            ("nonexistent".into(), 0.5),
        ],
    );

    assert!(s.unknown.contains("z"), "z stays failed");
    assert_eq!(unknown_ids, vec!["nonexistent".to_string()]);
    let a = s.belief.get("a").copied().unwrap_or(f32::NAN);
    assert!((0.0..=1.0).contains(&a), "belief must be clamped, got {a}");
    let b = s.belief.get("b").copied().unwrap_or(f32::NAN);
    assert!(b.is_finite(), "NaN belief must be replaced, got {b}");
}

/// Malformed state loaded straight off disk must not produce a nonsense gain.
#[test]
fn out_of_range_beliefs_cannot_poison_the_gain() {
    let g = chain();
    let mut s = State::default();
    for (id, v) in [
        ("a", f32::NAN),
        ("b", f32::INFINITY),
        ("c", -5.0),
        ("d", 12.0),
        ("z", f32::NEG_INFINITY),
    ] {
        s.belief.insert(id.into(), v);
    }

    for n in &g.nodes {
        let g_val = gain(&g, &s, &n.id);
        assert!(g_val.is_finite(), "{} produced a non-finite gain: {g_val}", n.id);
        assert!(g_val >= 0.0, "{} produced a negative gain: {g_val}", n.id);
    }
}
