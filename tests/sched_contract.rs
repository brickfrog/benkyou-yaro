//! Adversarial tests for the scheduler, written independently of the implementation.
//!
//! The load-bearing property here is the `encompasses` bridge: one exercise granting
//! credit to the concepts inside it is what stops the procedural track from being a
//! parallel chore competing with the card queue. Getting its direction backwards
//! typechecks and quietly makes the whole feature useless, so it is asserted directly.

use benkyou::graph::*;
use benkyou::sched::*;

fn node(id: &str) -> Node {
    Node {
        id: id.into(),
        title: id.into(),
        kind: Kind::Skill,
        probe: id.into(),
        goals: vec![],
        cost_minutes: 10,
        relevance: 1.0,
        provenance: Provenance::Llm,
        gradable: true,
    }
}

fn edge(from: &str, to: &str, ty: EdgeType) -> Edge {
    Edge {
        from: from.into(),
        to: to.into(),
        ty,
        strength: 1.0,
        reason: "test".into(),
        needs_goals: vec![],
        provenance: Provenance::Llm,
        confidence: 1.0,
    }
}

fn graph(nodes: &[&str], edges: Vec<Edge>) -> Graph {
    Graph {
        goal: Goal { id: "g".into(), target: "t".into(), deadline: None, budget_hours: 10 },
        nodes: nodes.iter().map(|n| node(n)).collect(),
        edges,
        state: State::default(),
    }
}

/// Mastery of the harder node credits the easier node inside it — not the reverse.
/// `Edge { from: easy, to: hard, Encompasses }`, so attempting `hard` credits `easy`.
#[test]
fn encompass_credit_flows_from_harder_to_easier() {
    let g = graph(&["easy", "hard"], vec![edge("easy", "hard", EdgeType::Encompasses)]);
    let cfg = SchedConfig { encompass_credit: 0.5, ..SchedConfig::default() };

    let mut f = Fluencies::new();
    let credited = record_attempt(&g, &mut f, "hard", 1.0, 0, &cfg);

    assert!(credited.contains("easy"), "attempting the hard node must credit the easy one");
    assert_eq!(f["hard"].confidence, 1.0);
    assert_eq!(f["easy"].confidence, 0.5, "direct encompass gets one hop of credit");

    // And not the other way round.
    let mut f2 = Fluencies::new();
    let credited2 = record_attempt(&g, &mut f2, "easy", 1.0, 0, &cfg);
    assert!(
        !credited2.contains("hard"),
        "practising the easy node must NOT credit the hard one"
    );
}

/// Credit attenuates per hop, so a two-hop concept gets a quarter, not a half.
#[test]
fn encompass_credit_attenuates_per_hop() {
    // c is inside b, b is inside a. Attempting a credits b then c.
    let g = graph(
        &["a", "b", "c"],
        vec![
            edge("b", "a", EdgeType::Encompasses),
            edge("c", "b", EdgeType::Encompasses),
        ],
    );
    let cfg = SchedConfig { encompass_credit: 0.5, ..SchedConfig::default() };

    let mut f = Fluencies::new();
    record_attempt(&g, &mut f, "a", 1.0, 0, &cfg);

    assert_eq!(f["a"].confidence, 1.0);
    assert_eq!(f["b"].confidence, 0.5, "one hop");
    assert_eq!(f["c"].confidence, 0.25, "two hops");
}

/// Encompassed concepts were not attempted, so their attempt count must not move.
/// Otherwise a single exercise inflates the practice history of everything beneath it.
#[test]
fn encompassed_nodes_are_not_credited_with_an_attempt() {
    let g = graph(&["easy", "hard"], vec![edge("easy", "hard", EdgeType::Encompasses)]);
    let cfg = SchedConfig::default();
    let mut f = Fluencies::new();

    record_attempt(&g, &mut f, "hard", 1.0, 0, &cfg);

    assert_eq!(f["hard"].attempts, 1);
    assert_eq!(f["easy"].attempts, 0, "credit is not an attempt");
}

/// Encompassing may be circular in generated data. Credit must terminate and land once.
#[test]
fn an_encompass_cycle_terminates_and_credits_once() {
    let g = graph(
        &["a", "b"],
        vec![
            edge("a", "b", EdgeType::Encompasses),
            edge("b", "a", EdgeType::Encompasses),
        ],
    );
    let cfg = SchedConfig { encompass_credit: 0.5, ..SchedConfig::default() };
    let mut f = Fluencies::new();

    record_attempt(&g, &mut f, "a", 1.0, 0, &cfg);

    assert_eq!(f["a"].confidence, 1.0, "the attempted node is not re-credited by the cycle");
    assert_eq!(f["b"].confidence, 0.5);
}

/// A prerequisite you have let rot must re-lock its dependents: gating reads the
/// decayed value, not the stored one.
#[test]
fn a_decayed_prerequisite_relocks_its_dependent() {
    let g = graph(&["base", "dep"], vec![edge("base", "dep", EdgeType::Requires)]);
    let cfg = SchedConfig {
        target: 1.0,
        half_life_days: 10.0,
        ..SchedConfig::default()
    };

    let mut f = Fluencies::new();
    record_attempt(&g, &mut f, "base", 1.0, 0, &cfg);

    assert!(is_unlocked(&g, &f, "dep", 0, &cfg), "at target on the day of practice");
    assert!(
        !is_unlocked(&g, &f, "dep", 40, &cfg),
        "four half-lives later the prerequisite has rotted and must re-lock"
    );
}

/// Interleaving is the one scheduling lever with strong evidence behind it.
#[test]
fn a_session_interleaves_across_concepts() {
    let g = graph(&["a", "b", "c"], vec![]);
    let cfg = SchedConfig { session_size: 6, ..SchedConfig::default() };
    let f = Fluencies::new();

    let session = compose_session(&g, &f, 0, &cfg);
    assert_eq!(session.len(), 6);
    for w in session.windows(2) {
        assert_ne!(w[0], w[1], "blocked practice: {session:?}");
    }
    for id in ["a", "b", "c"] {
        assert_eq!(session.iter().filter(|s| *s == id).count(), 2, "{session:?}");
    }
}

/// With a single practisable concept there is nothing to interleave with, so the
/// round-robin degenerates rather than returning a short session.
#[test]
fn a_single_concept_session_repeats_it() {
    let g = graph(&["only"], vec![]);
    let cfg = SchedConfig { session_size: 3, ..SchedConfig::default() };
    let session = compose_session(&g, &Fluencies::new(), 0, &cfg);
    assert_eq!(session, vec!["only".to_string(); 3]);
}

/// Nothing the scheduler does may produce a non-finite confidence, whatever the
/// generated data looks like.
#[test]
fn confidence_never_becomes_non_finite() {
    let g = graph(
        &["a", "b"],
        vec![
            edge("a", "b", EdgeType::Encompasses),
            edge("ghost", "a", EdgeType::Requires),
        ],
    );

    for half_life in [0.0, -5.0, f32::NAN, f32::INFINITY] {
        for credit in [0.5, f32::NAN, f32::INFINITY, -1.0] {
            for score in [1.0, 0.0, -3.0, 7.0] {
                let cfg = SchedConfig {
                    half_life_days: half_life,
                    encompass_credit: credit,
                    ..SchedConfig::default()
                };
                let mut f = Fluencies::new();
                record_attempt(&g, &mut f, "b", score, 0, &cfg);
                record_attempt(&g, &mut f, "a", score, 99, &cfg);
                for (id, fl) in &f {
                    assert!(
                        fl.confidence.is_finite(),
                        "{id} went non-finite: {fl:?} (half_life={half_life}, credit={credit}, score={score})"
                    );
                    assert!(fl.confidence >= 0.0, "{id} went negative: {fl:?}");
                    assert!(
                        decayed_confidence(fl, 10_000, &cfg).is_finite(),
                        "{id} decayed to non-finite"
                    );
                }
            }
        }
    }
}

/// An id that names no node is not schedulable, and must not become a phantom entry.
#[test]
fn attempting_a_node_outside_the_graph_creates_no_phantom_state() {
    let g = graph(&["real"], vec![]);
    let cfg = SchedConfig::default();
    let mut f = Fluencies::new();

    record_attempt(&g, &mut f, "ghost", 1.0, 0, &cfg);

    assert!(!f.contains_key("ghost"), "phantom fluency for a nonexistent node: {f:?}");
    assert!(!practisable(&g, &f, 0, &cfg).iter().any(|id| id == "ghost"));
}
