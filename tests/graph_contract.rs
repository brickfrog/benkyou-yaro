//! Adversarial contract tests, written independently of the implementation.
//!
//! These target the specific failure modes that showed up across competing
//! implementations: ghost node ids leaking into a plan, and a budget-skipped
//! prerequisite leaving its dependent scheduled.

use std::collections::BTreeSet;

use benkyou::graph::*;

fn node(id: &str, cost: u32) -> Node {
    Node {
        id: id.into(),
        title: id.into(),
        kind: Kind::Skill,
        probe: format!("probe for {id}"),
        goals: vec![],
        cost_minutes: cost,
        relevance: 1.0,
        provenance: Provenance::Llm,
        gradable: true,
    }
}

fn req(from: &str, to: &str, confidence: f32) -> Edge {
    Edge {
        from: from.into(),
        to: to.into(),
        ty: EdgeType::Requires,
        strength: 1.0,
        reason: "test".into(),
        needs_goals: vec![],
        provenance: Provenance::Llm,
        confidence,
    }
}

fn graph(nodes: Vec<Node>, edges: Vec<Edge>) -> Graph {
    Graph {
        goal: Goal {
            id: "g".into(),
            target: "t".into(),
            deadline: None,
            budget_hours: 100,
        },
        nodes,
        edges,
        state: State::default(),
    }
}

fn ids(v: &[NodeId]) -> Vec<&str> {
    v.iter().map(|s| s.as_str()).collect()
}

/// An edge may name a node that does not exist. Such an id must never become a
/// plan entry: a study order made of nodes you cannot study is not a study order.
#[test]
fn plan_never_contains_a_nonexistent_node() {
    let g = graph(vec![node("b", 10)], vec![req("ghost", "b", 1.0)]);
    let plan = g.plan("b", &BTreeSet::new(), 1000);
    assert!(
        !plan.iter().any(|id| !g.contains(id)),
        "plan leaked a node that is not in the graph: {plan:?}"
    );
    assert_eq!(ids(&plan), vec!["b"]);
}

/// The corrected budget rule: an unaffordable prerequisite takes its dependents
/// down with it, even when a dependent would have fit on its own.
#[test]
fn budget_skipped_prerequisite_blocks_its_dependent() {
    // expensive -> cheap_dependent, and an independent cheap node.
    let g = graph(
        vec![
            node("expensive", 500),
            node("cheap_dependent", 5),
            node("independent", 5),
            node("target", 5),
        ],
        vec![
            req("expensive", "cheap_dependent", 1.0),
            req("cheap_dependent", "target", 1.0),
            req("independent", "target", 1.0),
        ],
    );

    let plan = g.plan("target", &BTreeSet::new(), 20);

    assert!(
        !plan.iter().any(|id| id == "expensive"),
        "unaffordable node should be skipped: {plan:?}"
    );
    assert!(
        !plan.iter().any(|id| id == "cheap_dependent"),
        "dependent of a skipped prerequisite must also be dropped: {plan:?}"
    );
    assert!(
        !plan.iter().any(|id| id == "target"),
        "target transitively requires the skipped node, so it cannot be planned: {plan:?}"
    );
    assert_eq!(
        ids(&plan),
        vec!["independent"],
        "the independent cheap branch must still land"
    );
}

/// Whatever the budget does, the returned plan is internally coherent: nothing
/// appears before something it requires.
#[test]
fn plan_is_prerequisite_closed_and_ordered() {
    let g = graph(
        vec![
            node("a", 10),
            node("b", 10),
            node("c", 10),
            node("d", 10),
            node("e", 400),
            node("f", 10),
        ],
        vec![
            req("a", "b", 1.0),
            req("a", "c", 1.0),
            req("b", "d", 1.0),
            req("c", "d", 1.0),
            req("e", "f", 1.0),
            req("d", "target", 1.0),
            req("f", "target", 1.0),
        ],
    );
    let mut nodes = g.nodes.clone();
    nodes.push(node("target", 10));
    let g = graph(nodes, g.edges.clone());

    for budget in [0u32, 5, 15, 35, 60, 1000] {
        let plan = g.plan("target", &BTreeSet::new(), budget);
        let placed: BTreeSet<&str> = plan.iter().map(|s| s.as_str()).collect();

        for (i, id) in plan.iter().enumerate() {
            for prereq in g.requires_ancestors(id) {
                if !g.contains(&prereq) {
                    continue;
                }
                assert!(
                    placed.contains(prereq.as_str()),
                    "budget {budget}: {id} planned without its prerequisite {prereq}: {plan:?}"
                );
                let at = plan.iter().position(|p| *p == prereq).unwrap();
                assert!(
                    at < i,
                    "budget {budget}: prerequisite {prereq} scheduled after {id}: {plan:?}"
                );
            }
        }

        let total: u32 = plan
            .iter()
            .filter_map(|id| g.node(id))
            .map(|n| n.cost_minutes)
            .sum();
        assert!(
            total <= budget,
            "budget {budget} exceeded by plan costing {total}: {plan:?}"
        );
    }
}

/// A cycle is a modelling error the author has to resolve. The tool reports it whole
/// and changes nothing: it cannot tell which of the three edges is the wrong one, and
/// the confidence it used to sort by is written by whoever wrote the edge, so the
/// boldest mistake always won.
#[test]
fn validate_reports_a_cycle_without_cutting_any_edge() {
    let mut g = graph(
        vec![node("a", 10), node("b", 10), node("c", 10)],
        vec![
            req("a", "b", 0.9),
            req("b", "c", 0.1),
            req("c", "a", 0.8),
        ],
    );
    let before = g.edges.clone();
    let report = g.validate(RELEVANCE_FLOOR, NODE_CAP);

    assert_eq!(report.cycles.len(), 1, "{report:?}");
    assert_eq!(report.cycles[0].len(), 3, "the whole cycle, not one suspect");
    assert_eq!(g.edges, before, "the graph paid for the report");
    assert!(!report.is_clean());

    // And it keeps saying so, rather than going quiet on the second look.
    assert_eq!(g.validate(RELEVANCE_FLOOR, NODE_CAP).cycles.len(), 1);
}

/// Validation must be idempotent: a repaired graph repairs to itself.
#[test]
fn validate_is_idempotent_and_deterministic() {
    let build = || {
        graph(
            vec![
                node("a", 10),
                node("b", 10),
                node("dup", 10),
                node("dup", 20),
                Node { relevance: 0.01, ..node("weak", 10) },
                Node { relevance: f32::NAN, ..node("nan", 10) },
            ],
            vec![
                req("a", "b", 0.5),
                req("b", "a", 0.4),
                req("a", "a", 1.0),
                req("missing", "b", 0.7),
            ],
        )
    };

    // NaN != NaN, so whole-struct equality is useless here. Compare the shape:
    // which nodes survived, and which edges, in order.
    let shape = |g: &Graph| {
        (
            g.nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>(),
            g.edges
                .iter()
                .map(|e| (e.from.clone(), e.to.clone(), e.ty))
                .collect::<Vec<_>>(),
        )
    };

    let mut first = build();
    let r1 = first.validate(RELEVANCE_FLOOR, NODE_CAP);

    let mut again = first.clone();
    let r2 = again.validate(RELEVANCE_FLOOR, NODE_CAP);
    // Every mechanical repair is settled after one run. The cycle is not a repair, so
    // it is still reported — identically — until the author removes an edge.
    assert!(r2.duplicate_nodes.is_empty());
    assert!(r2.dropped_irrelevant.is_empty());
    assert!(r2.dropped_over_cap.is_empty());
    assert!(r2.dangling_edges.is_empty());
    assert_eq!(r2.cycles, r1.cycles, "the cycle stopped being named: {r2:?}");
    assert_eq!(shape(&first), shape(&again), "validate is not idempotent");

    // Same input, same output, every time.
    let mut other = build();
    let r3 = other.validate(RELEVANCE_FLOOR, NODE_CAP);
    assert_eq!(r1, r3, "validate is not deterministic");
    assert_eq!(shape(&first), shape(&other));

    assert!(r1.duplicate_nodes.iter().any(|id| id == "dup"));
    assert!(r1.dropped_irrelevant.iter().any(|id| id == "weak"));
    // The self-loop and the edge naming a missing node are both dangling material.
    assert!(r1.dangling_edges.iter().any(|e| e.from == "a" && e.to == "a"));
    assert!(r1.dangling_edges.iter().any(|e| e.from == "missing"));
    // A NaN relevance is not below the floor, so the node survives rather than
    // being silently dropped.
    assert!(first.contains("nan"));
}

/// The closure is the whole pruning mechanism: one PASS deep in the graph must
/// collapse the entire ancestor cone.
#[test]
fn closure_collapses_the_ancestor_cone() {
    let g = graph(
        vec![node("a", 1), node("b", 1), node("c", 1), node("d", 1), node("z", 1)],
        vec![req("a", "b", 1.0), req("a", "c", 1.0), req("b", "d", 1.0), req("c", "d", 1.0)],
    );

    let known: BTreeSet<NodeId> = ["d".to_string()].into_iter().collect();
    let closed = g.close_known(&known);

    assert_eq!(
        closed.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        vec!["a", "b", "c", "d"]
    );
    assert!(g.is_downward_closed(&closed));
    assert!(!g.is_downward_closed(&known), "{{d}} alone is not downward-closed");

    // z is isolated, so it is the only thing left to start on besides nothing.
    assert_eq!(ids(&g.outer_fringe(&closed)), vec!["z"]);
    // Only d is on the frontier of what is known; a, b, c are prerequisites of it.
    assert_eq!(ids(&g.inner_fringe(&closed)), vec!["d"]);
}
