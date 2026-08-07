//! The assessment loop: find out what the learner already knows, cheaply.
//!
//! Replaces ALEKS's probability mass over feasible knowledge states with closure
//! leverage, which is computable in microseconds on a graph this size.
//! See DESIGN.md §2.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::graph::{Evidence, Graph, Node, NodeId, State, Verdict};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssessConfig {
    /// Hard ceiling on questions asked. ALEKS resolves an 80-item state in ~25.
    pub max_questions: usize,
    /// Stop when the best question would resolve fewer than this many nodes.
    pub min_gain: f32,
}

impl Default for AssessConfig {
    fn default() -> Self {
        // The binding limit is the question count, not the gain floor. A question
        // costs the learner half a minute; resolving even one node decides whether
        // to spend an hour studying it. So the floor only exists to stop asking
        // about nodes that are already resolved, which score 0.0 — anything
        // unresolved scores at least 1.0 because it always resolves itself.
        Self { max_questions: 30, min_gain: 1.0 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub node: NodeId,
    pub probe: String,
    pub gain: f32,
}

/// Why the loop stopped. Reported to the user, because "I ran out of questions"
/// and "I know enough" mean different things about the plan that follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// No question would resolve `min_gain` nodes.
    Converged,
    /// Hit `max_questions`.
    Exhausted,
    /// Every node is resolved.
    Complete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Step {
    Ask(Question),
    Stop(StopReason),
}

/// Prior belief for a node with no evidence and no assigned belief.
pub const DEFAULT_BELIEF: f32 = 0.5;

/// Expected number of nodes resolved by asking `node`.
///
/// ```text
/// gain(n) = p[n]       * |({n} + requires_ancestors(n))   \ known|
///         + (1 - p[n]) * |({n} + requires_descendants(n)) \ unknown|
/// ```
///
/// The first term is what collapses on PASS, the second what collapses on FAIL.
/// `n` is included in both terms because answering always resolves at least
/// itself: the gain of an isolated unresolved node is exactly 1.0, never 0.0.
///
/// `p[n]` comes from `state.belief`, defaulting to [`DEFAULT_BELIEF`]. A node
/// already in `known` or `unknown` has gain 0.0 — there is nothing left to learn
/// by asking it.
pub fn gain(graph: &Graph, state: &State, node: &str) -> f32 {
    if state.known.contains(node) || state.unknown.contains(node) {
        return 0.0;
    }
    let p = belief_of(state, node);

    let mut on_pass = graph.requires_ancestors(node);
    on_pass.insert(node.to_string());
    let resolved_by_pass = on_pass.iter().filter(|id| !state.known.contains(*id)).count();

    let mut on_fail = graph.requires_descendants(node);
    on_fail.insert(node.to_string());
    let resolved_by_fail = on_fail.iter().filter(|id| !state.unknown.contains(*id)).count();

    p * resolved_by_pass as f32 + (1.0 - p) * resolved_by_fail as f32
}

/// `state.belief` as the loop reads it: missing and `NaN` are [`DEFAULT_BELIEF`],
/// anything outside `0.0..=1.0` is clamped into it. A state can arrive holding
/// anything, and an infinite `p` would make `p * a + (1 - p) * d` a `NaN` gain.
fn belief_of(state: &State, node: &str) -> f32 {
    match state.belief.get(node) {
        Some(p) if !p.is_nan() => p.clamp(0.0, 1.0),
        _ => DEFAULT_BELIEF,
    }
}

/// The next question, or why to stop.
///
/// Picks `argmax gain` over nodes not in `asked`. Ties break toward the belief
/// closest to 0.5 — a coin-flip node is where information gain is maximal, which
/// is ALEKS's own selection rule — and then on node id ascending, so the loop is
/// reproducible.
///
/// Stop reasons are checked in this order, and the order is part of the contract:
/// 1. [`StopReason::Complete`] — every node is in `known` or `unknown`
/// 2. [`StopReason::Exhausted`] — `asked.len() >= cfg.max_questions`
/// 3. [`StopReason::Converged`] — the best available gain is `< cfg.min_gain`,
///    which also covers the case where every remaining node has been asked
///
/// Nodes in `asked` are never returned again, even if unresolved: a re-ask is
/// driven by [`record`] returning [`RecordOutcome::ReAsk`], not by selection.
pub fn next_step(
    graph: &Graph,
    state: &State,
    asked: &BTreeSet<NodeId>,
    cfg: &AssessConfig,
) -> Step {
    if graph
        .nodes
        .iter()
        .all(|n| state.known.contains(&n.id) || state.unknown.contains(&n.id))
    {
        return Step::Stop(StopReason::Complete);
    }
    if asked.len() >= cfg.max_questions {
        return Step::Stop(StopReason::Exhausted);
    }

    // argmax gain, then a worst-case tie-break, then |p - 0.5| ascending, then node
    // id ascending. The comparison is a total order over finite values, so the
    // winner does not depend on the order the nodes happen to sit in.
    //
    // The worst-case term matters more than it looks. On a uniform chain every node
    // has the *same* expected gain, because what one answer wins in one direction it
    // loses in the other. Breaking that tie on node id picks the root, whose PASS
    // resolves nothing but itself, and the interview then walks the chain one node
    // at a time — the closure leverage never materialises. Preferring the node whose
    // two cones are most balanced maximises what is resolved *whichever way the
    // answer goes*, which is the guarantee actually worth having.
    let mut best: Option<(&Node, f32, u32, f32)> = None;
    for candidate in &graph.nodes {
        if asked.contains(&candidate.id) {
            continue;
        }
        let g = gain(graph, state, &candidate.id);
        let worst_case = worst_case_resolved(graph, state, &candidate.id);
        let coin_flip_distance = (belief_of(state, &candidate.id) - DEFAULT_BELIEF).abs();
        let better = match best {
            None => true,
            Some((node, gain, worst, distance)) => match g.total_cmp(&gain) {
                Ordering::Greater => true,
                Ordering::Less => false,
                Ordering::Equal => match worst_case.cmp(&worst) {
                    Ordering::Greater => true,
                    Ordering::Less => false,
                    Ordering::Equal => match coin_flip_distance.total_cmp(&distance) {
                        Ordering::Less => true,
                        Ordering::Greater => false,
                        Ordering::Equal => candidate.id < node.id,
                    },
                },
            },
        };
        if better {
            best = Some((candidate, g, worst_case, coin_flip_distance));
        }
    }

    match best {
        Some((node, gain, _, _)) if gain >= cfg.min_gain => Step::Ask(Question {
            node: node.id.clone(),
            probe: node.probe.clone(),
            gain,
        }),
        _ => Step::Stop(StopReason::Converged),
    }
}

/// How many nodes are resolved in the *less* informative of the two outcomes.
///
/// A PASS collapses the unresolved ancestor cone; a FAIL collapses the unresolved
/// descendant cone. The smaller of the two is what this question is guaranteed to
/// buy, and maximising it is what keeps the interview from walking a chain one node
/// at a time when every expected gain is equal.
fn worst_case_resolved(graph: &Graph, state: &State, node: &str) -> u32 {
    if state.known.contains(node) || state.unknown.contains(node) {
        return 0;
    }
    let on_pass = graph
        .requires_ancestors(node)
        .iter()
        .filter(|id| !state.known.contains(*id))
        .count();
    let on_fail = graph
        .requires_descendants(node)
        .iter()
        .filter(|id| !state.unknown.contains(*id))
        .count();
    (on_pass.min(on_fail) as u32).saturating_add(1)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordOutcome {
    /// The verdict was applied.
    Applied,
    /// A FAIL that contradicts the learner's demonstrated knowledge. The caller
    /// should ask the same node once more with a different instance before
    /// accepting it. The verdict has *not* been applied.
    ReAsk,
}

/// Apply a graded verdict, propagating through the `requires` closure.
///
/// - `Pass`    → `known |= {n} + requires_ancestors(n)`; those beliefs become 1.0
/// - `Partial` → ancestors only; `n` stays out of `known` with belief 0.5, so it
///               remains in the plan as cheap review
/// - `Fail`    → `unknown |= {n} + requires_descendants(n)`; those beliefs become 0.0
/// - `Skip`    → nothing; the probe was unanswerable, so it graded the question
///
/// Always appends to `state.evidence`, including for a [`RecordOutcome::ReAsk`],
/// because the contradiction is itself worth recording.
///
/// Careless-error handling: a `Fail` on a node with two or more `Pass` verdicts
/// already recorded against its `requires`-descendants is contradictory — you
/// cannot do the harder thing while failing the easier one. The first such `Fail`
/// returns [`RecordOutcome::ReAsk`] and changes neither `known` nor `unknown`. If
/// the same node fails again it is applied normally.
///
/// A node moving into `known` is removed from `unknown` and vice versa; the two
/// sets are always disjoint afterwards, and `known` is always downward-closed.
pub fn record(
    graph: &Graph,
    state: &mut State,
    node: &str,
    verdict: Verdict,
    probe: &str,
    at: &str,
) -> RecordOutcome {
    let descendants = graph.requires_descendants(node);

    // Two or more PASSes already recorded against things that *require* this node
    // contradict a FAIL on it. The re-ask itself leaves a FAIL on this node in the
    // log, so a second FAIL is applied normally.
    let passing_descendants = state
        .evidence
        .iter()
        .filter(|e| e.verdict == Verdict::Pass && descendants.contains(&e.node))
        .count();
    let already_re_asked = state
        .evidence
        .iter()
        .any(|e| e.node == node && e.verdict == Verdict::Fail);
    let contradicted =
        verdict == Verdict::Fail && passing_descendants >= 2 && !already_re_asked;

    state.evidence.push(Evidence {
        node: node.to_string(),
        probe: probe.to_string(),
        verdict: verdict.clone(),
        at: at.to_string(),
        source: SOURCE_ASSESS.to_string(),
    });

    if contradicted {
        return RecordOutcome::ReAsk;
    }

    match verdict {
        Verdict::Pass => {
            let mut cone = graph.requires_ancestors(node);
            cone.insert(node.to_string());
            mark_known(state, cone);
        }
        Verdict::Partial => {
            // Ancestors only: the node itself stays out of `known`, at a coin flip,
            // so the plan keeps it as cheap review.
            //
            // If a previous verdict had already resolved it, that resolution is
            // withdrawn — "half of it" is not "all of it", and it is not a failure
            // either. Pulling it out of `known` would strand any known descendant
            // that requires it, so those come out too and `known` stays
            // downward-closed.
            if state.known.remove(node) {
                for id in graph.requires_descendants(node) {
                    if state.known.remove(&id) {
                        state.belief.insert(id, DEFAULT_BELIEF);
                    }
                }
            }
            state.unknown.remove(node);
            mark_known(state, graph.requires_ancestors(node));
            state.belief.insert(node.to_string(), DEFAULT_BELIEF);
        }
        Verdict::Fail => {
            let mut cone = descendants;
            cone.insert(node.to_string());
            for id in cone {
                state.known.remove(&id);
                state.unknown.insert(id.clone());
                state.belief.insert(id, 0.0);
            }
        }
        Verdict::Skip => {
            // The probe failed, not the learner. Resolve nothing and move no
            // belief — but the evidence entry above still counts the node as
            // asked, so the loop advances instead of re-offering a bad question.
        }
    }
    RecordOutcome::Applied
}

/// `source` recorded on evidence produced by answering a question.
const SOURCE_ASSESS: &str = "assess";

/// `source` recorded on evidence the learner declared rather than answered.
const SOURCE_DECLARE: &str = "declare";

/// Stands in for the question text on declared evidence, where none was asked.
const DECLARED_PROBE: &str = "self-declared";

/// Move a whole cone into `known`, evicting it from `unknown` so the two sets stay
/// disjoint. The cone is a `requires`-ancestor closure, which is transitively
/// closed, so `known` stays downward-closed.
fn mark_known(state: &mut State, cone: BTreeSet<NodeId>) {
    for id in cone {
        state.unknown.remove(&id);
        state.known.insert(id.clone());
        state.belief.insert(id, 1.0);
    }
}

/// Seed `state.belief` from a prior supplied by the agent.
///
/// Only nodes present in the graph are accepted; unknown ids are returned so the
/// caller can report a graph/prior mismatch. Values are clamped to 0.0..=1.0, and
/// `NaN` is replaced with [`DEFAULT_BELIEF`]. Nodes already resolved in `known` or
/// `unknown` are not overwritten — evidence beats a prior, always.
pub fn seed_prior(
    graph: &Graph,
    state: &mut State,
    prior: &[(NodeId, f32)],
) -> Vec<NodeId> {
    let mut missing: Vec<NodeId> = Vec::new();
    for (id, p) in prior {
        if !graph.contains(id) {
            missing.push(id.clone());
            continue;
        }
        if state.known.contains(id) || state.unknown.contains(id) {
            continue;
        }
        let p = if p.is_nan() { DEFAULT_BELIEF } else { p.clamp(0.0, 1.0) };
        state.belief.insert(id.clone(), p);
    }
    missing
}

/// Nodes the interview has actually put a question to.
///
/// Declared evidence is excluded deliberately. Declaring what you already know
/// must not consume the [`AssessConfig::max_questions`] budget, or seeding a
/// large graph would end the interview before it asked anything.
pub fn asked(state: &State) -> BTreeSet<NodeId> {
    state
        .evidence
        .iter()
        .filter(|e| e.source == SOURCE_ASSESS)
        .map(|e| e.node.clone())
        .collect()
}

/// What [`declare`] could not take at face value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Declaration {
    /// Declared ids naming no node in this graph, sorted and deduplicated.
    pub missing: Vec<NodeId>,
    /// Nodes claimed as known that a declared `unknown` then contradicted through
    /// the `requires` closure. Each is a self-contradiction worth surfacing.
    pub conflicts: Vec<NodeId>,
    /// Nodes given a strong prior but no resolution — still to be proven.
    pub primed: Vec<NodeId>,
    /// Nodes that a graded probe had put in `known`, which an admission has now
    /// pulled back out through the `requires` closure.
    ///
    /// This is the one place a declaration overrides earned evidence, and it is
    /// forced: `known` is downward-closed, so a node cannot stay there once one of
    /// its prerequisites is unknown. Surfaced rather than done quietly, because a
    /// careless `--unknown` can otherwise discard a session's worth of graded work.
    pub retracted: Vec<NodeId>,
}

/// Prior assigned to a node the learner says they know.
///
/// Deliberately short of certainty. It is high enough that closure leverage will
/// pick the node early — a `Pass` there resolves its whole ancestor cone in one
/// question — and short enough that nothing is recorded as mastered on someone's
/// word alone.
pub const DECLARED_BELIEF: f32 = 0.9;

/// Take the learner's own account of what they know, without believing the good half.
///
/// The two directions are handled differently, and the asymmetry is the point.
///
/// - **Claiming to know is a prior, never evidence.** `known` entries must trace to a
///   graded probe or to closure from one, so a declared-known node only receives
///   [`DECLARED_BELIEF`]. People are reliably wrong about this — "I know SQL" and "I
///   can write a `GROUP BY` from a blank editor" are different claims, and the second
///   is the one that matters. A strong prior does not skip the question; it makes the
///   question worth asking sooner, because a pass there discharges everything beneath it.
/// - **Admitting ignorance resolves immediately.** Nobody claims not to know a thing
///   they can do, and the costs are not symmetric: wrongly marking something unknown
///   wastes a little time out loud, while wrongly marking it known silently skips
///   material the learner needed. So `unknown` applies the `Fail` closure and is
///   recorded as evidence.
///
/// Unknowns are applied last. Claiming `b` while declaring its prerequisite `a`
/// unknown is a contradiction; the humbler claim stands and the conflict is reported
/// rather than silently resolved either way.
///
/// Evidence outranks a *claim*: a declared-known node that a graded probe has already
/// resolved keeps the graded answer. An *admission* is the exception, and the only
/// place a declaration overrides earned evidence. It cannot be otherwise — `known` is
/// downward-closed, so a node graded into it cannot stay there once one of its
/// prerequisites is admitted unknown, exactly as a graded `Fail` on that prerequisite
/// would demote it. Every node pulled back out this way is returned in
/// [`Declaration::retracted`] so the loss is visible rather than silent.
pub fn declare(
    graph: &Graph,
    state: &mut State,
    known: &[NodeId],
    unknown: &[NodeId],
    at: &str,
) -> Declaration {
    let mut missing: Vec<NodeId> = Vec::new();
    let mut claimed: Vec<NodeId> = Vec::new();
    let mut retracted: Vec<NodeId> = Vec::new();

    for id in known {
        if !graph.contains(id) {
            missing.push(id.clone());
            continue;
        }
        claimed.push(id.clone());
        if state.known.contains(id) || state.unknown.contains(id) {
            continue; // graded already; a claim does not get to move it
        }
        state.belief.insert(id.clone(), DECLARED_BELIEF);
    }

    for id in unknown {
        if !graph.contains(id) {
            missing.push(id.clone());
            continue;
        }
        state.evidence.push(Evidence {
            node: id.clone(),
            probe: DECLARED_PROBE.to_string(),
            verdict: Verdict::Fail,
            at: at.to_string(),
            source: SOURCE_DECLARE.to_string(),
        });
        let mut cone = graph.requires_descendants(id);
        cone.insert(id.clone());
        for n in cone {
            // Anything sitting in `known` got there by a graded probe or by closure
            // from one — declarations no longer write to it — so its removal is a
            // retraction of evidence and has to be reported.
            if state.known.remove(&n) {
                retracted.push(n.clone());
            }
            state.unknown.insert(n.clone());
            state.belief.insert(n, 0.0);
        }
    }

    missing.sort();
    missing.dedup();
    retracted.sort();
    retracted.dedup();
    let (conflicts, primed) = claimed
        .into_iter()
        .partition(|id| state.unknown.contains(id));
    Declaration { missing, conflicts, primed, retracted }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::graph::{Edge, EdgeType, Evidence, Goal, Kind, Node, Provenance};

    // ------------------------------------------------------------------
    // builders
    // ------------------------------------------------------------------

    fn node(id: &str) -> Node {
        Node {
            id: id.to_string(),
            title: id.to_string(),
            kind: Kind::Concept,
            probe: format!("probe for {id}"),
            goals: Vec::new(),
            cost_minutes: 10,
            relevance: 1.0,
            provenance: Provenance::Llm,
            gradable: true,
        }
    }

    fn edge(from: &str, to: &str, ty: EdgeType) -> Edge {
        Edge {
            from: from.to_string(),
            to: to.to_string(),
            ty,
            strength: 1.0,
            reason: format!("{from} before {to}"),
            needs_goals: Vec::new(),
            provenance: Provenance::Llm,
            confidence: 1.0,
        }
    }

    /// `req(a, b)` is *a is a prerequisite of b*, so `requires_ancestors("b")`
    /// contains `"a"` and `requires_descendants("a")` contains `"b"`. Every
    /// closure-direction assertion below is written against this reading.
    fn req(from: &str, to: &str) -> Edge {
        edge(from, to, EdgeType::Requires)
    }

    fn graph_with(nodes: &[&str], edges: Vec<Edge>) -> Graph {
        Graph {
            goal: Goal {
                id: "g".to_string(),
                target: "t".to_string(),
                deadline: None,
                budget_hours: 40,
            },
            nodes: nodes.iter().map(|id| node(id)).collect(),
            edges,
            state: State::default(),
        }
    }

    fn graph(nodes: &[&str], edges: &[(&str, &str)]) -> Graph {
        graph_with(nodes, edges.iter().map(|(f, t)| req(f, t)).collect())
    }

    /// `ids[0] -> ids[1] -> ... -> ids[n-1]`.
    fn chain(ids: &[&str]) -> Graph {
        let edges: Vec<(&str, &str)> = ids.windows(2).map(|w| (w[0], w[1])).collect();
        graph(ids, &edges)
    }

    fn empty() -> Graph {
        graph(&[], &[])
    }

    fn single() -> Graph {
        graph(&["a"], &[])
    }

    fn set(members: &[&str]) -> BTreeSet<NodeId> {
        members.iter().map(|s| s.to_string()).collect()
    }

    fn ids(members: &[&str]) -> Vec<NodeId> {
        members.iter().map(|s| s.to_string()).collect()
    }

    fn state_of(known: &[&str], unknown: &[&str], belief: &[(&str, f32)]) -> State {
        State {
            known: set(known),
            unknown: set(unknown),
            belief: belief
                .iter()
                .map(|(id, p)| (id.to_string(), *p))
                .collect::<BTreeMap<NodeId, f32>>(),
            evidence: Vec::new(),
        }
    }

    fn logged(node: &str, verdict: Verdict) -> Evidence {
        Evidence {
            node: node.to_string(),
            probe: format!("probe for {node}"),
            verdict,
            at: "t0".to_string(),
            source: "test".to_string(),
        }
    }

    fn prior(entries: &[(&str, f32)]) -> Vec<(NodeId, f32)> {
        entries.iter().map(|(id, p)| (id.to_string(), *p)).collect()
    }

    fn cfg(max_questions: usize, min_gain: f32) -> AssessConfig {
        AssessConfig { max_questions, min_gain }
    }

    fn ask(step: Step) -> Question {
        match step {
            Step::Ask(q) => q,
            Step::Stop(reason) => panic!("expected a question, got Stop({reason:?})"),
        }
    }

    // ------------------------------------------------------------------
    // invariants and driver
    // ------------------------------------------------------------------

    /// The two invariants `record` must re-establish on every call, on an acyclic
    /// graph. (A cycle that survived validation can make downward closure
    /// unsatisfiable — `a` requires `b` requires `a` — so cyclic graphs are only
    /// checked for disjointness and termination.)
    fn assert_invariants(g: &Graph, s: &State) {
        let overlap: Vec<&NodeId> = s.known.intersection(&s.unknown).collect();
        assert!(
            overlap.is_empty(),
            "known and unknown overlap on {overlap:?}: known={:?} unknown={:?}",
            s.known,
            s.unknown
        );
        assert!(
            g.is_downward_closed(&s.known),
            "known {:?} is not downward-closed under requires",
            s.known
        );
    }

    /// Run the loop to termination against a fixed grader. Returns the stop reason
    /// and the questions asked, in order.
    ///
    /// Asserts as it goes that a node is never asked twice, that only real nodes are
    /// asked, and that the state invariants hold after every `record`. A
    /// [`RecordOutcome::ReAsk`] is answered the way the contract says the caller
    /// should: the same node once more, whose verdict is then applied.
    fn drive(
        g: &Graph,
        cfg: &AssessConfig,
        answer: &dyn Fn(&str) -> Verdict,
    ) -> (StopReason, Vec<NodeId>, State) {
        drive_from(g, cfg, answer, State::default())
    }

    fn drive_from(
        g: &Graph,
        cfg: &AssessConfig,
        answer: &dyn Fn(&str) -> Verdict,
        mut state: State,
    ) -> (StopReason, Vec<NodeId>, State) {
        let mut asked: BTreeSet<NodeId> = BTreeSet::new();
        let mut order: Vec<NodeId> = Vec::new();
        for _ in 0..64 {
            match next_step(g, &state, &asked, cfg) {
                Step::Stop(reason) => return (reason, order, state),
                Step::Ask(q) => {
                    assert!(g.contains(&q.node), "asked for a non-node: {}", q.node);
                    assert!(
                        asked.insert(q.node.clone()),
                        "asked {} twice; order so far {order:?}",
                        q.node
                    );
                    order.push(q.node.clone());
                    let verdict = answer(&q.node);
                    let outcome =
                        record(g, &mut state, &q.node, verdict.clone(), &q.probe, "t0");
                    assert_invariants(g, &state);
                    if outcome == RecordOutcome::ReAsk {
                        let second =
                            record(g, &mut state, &q.node, verdict, &q.probe, "t1");
                        assert_eq!(
                            second,
                            RecordOutcome::Applied,
                            "a re-asked node must not re-ask again"
                        );
                        assert_invariants(g, &state);
                    }
                }
            }
        }
        panic!("the assessment loop did not terminate in 64 steps: {order:?}");
    }

    // ==================================================================
    // gain
    // ==================================================================

    #[test]
    fn gain_of_an_isolated_node_is_exactly_one() {
        let g = single();
        assert_eq!(gain(&g, &State::default(), "a"), 1.0);
        // The node is in both terms, so p cancels: the answer resolves exactly
        // itself whatever we believe about it.
        for p in [0.0, 0.25, DEFAULT_BELIEF, 0.9, 1.0] {
            let s = state_of(&[], &[], &[("a", p)]);
            assert_eq!(gain(&g, &s, "a"), 1.0, "isolated node at belief {p}");
        }
    }

    #[test]
    fn gain_at_belief_one_is_the_whole_ancestor_cone_including_the_node() {
        let g = chain(&["a", "b", "c", "d", "e"]);
        // {e} + {a, b, c, d}, and the FAIL term contributes nothing.
        assert_eq!(gain(&g, &state_of(&[], &[], &[("e", 1.0)]), "e"), 5.0);
        assert_eq!(gain(&g, &state_of(&[], &[], &[("d", 1.0)]), "d"), 4.0);
        // Already-known ancestors are not counted twice.
        let s = state_of(&["a", "b"], &[], &[("e", 1.0)]);
        assert_eq!(gain(&g, &s, "e"), 3.0);
    }

    #[test]
    fn gain_at_belief_zero_is_the_whole_descendant_cone_including_the_node() {
        let g = chain(&["a", "b", "c", "d", "e"]);
        assert_eq!(gain(&g, &state_of(&[], &[], &[("a", 0.0)]), "a"), 5.0);
        // {c} + {d, e}: the ancestor cone is worth nothing at belief 0.0.
        assert_eq!(gain(&g, &state_of(&[], &[], &[("c", 0.0)]), "c"), 3.0);
        // ...and a deep node has only itself left to resolve.
        assert_eq!(gain(&g, &state_of(&[], &[], &[("e", 0.0)]), "e"), 1.0);
        // Already-unknown descendants are not counted twice.
        let s = state_of(&[], &["e"], &[("c", 0.0)]);
        assert_eq!(gain(&g, &s, "c"), 2.0);
    }

    #[test]
    fn gain_of_a_resolved_node_is_zero() {
        let g = chain(&["a", "b", "c", "d", "e"]);
        assert_eq!(gain(&g, &state_of(&["c"], &[], &[]), "c"), 0.0);
        assert_eq!(gain(&g, &state_of(&[], &["c"], &[]), "c"), 0.0);
        // Overlapping sets on entry are adversarial input, not a reason to differ.
        assert_eq!(gain(&g, &state_of(&["c"], &["c"], &[]), "c"), 0.0);
        // A belief entry does not resurrect a resolved node.
        let s = state_of(&["c"], &[], &[("c", 1.0)]);
        assert_eq!(gain(&g, &s, "c"), 0.0);
        // Its neighbours are still worth asking: 0.5 * |{a, b, d}| + 0.5 * |{d, e}|.
        assert_eq!(gain(&g, &state_of(&["c"], &[], &[]), "d"), 2.5);
    }

    #[test]
    fn gain_counts_both_cones_at_the_default_belief() {
        // a -> b, a -> c, b -> d, c -> d.
        let g = graph(&["a", "b", "c", "d"], &[("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")]);
        let s = State::default();
        // 0.5 * |{a, b, c, d}| + 0.5 * |{d}|
        assert_eq!(gain(&g, &s, "d"), 2.5);
        // 0.5 * |{a}| + 0.5 * |{a, b, c, d}|
        assert_eq!(gain(&g, &s, "a"), 2.5);
        // 0.5 * |{a, b}| + 0.5 * |{b, d}|
        assert_eq!(gain(&g, &s, "b"), 2.0);
        assert_eq!(gain(&g, &s, "c"), 2.0);
    }

    #[test]
    fn a_missing_belief_and_a_nan_belief_both_read_as_the_default() {
        let g = chain(&["a", "b", "c", "d", "e"]);
        // 0.5 * |{a}| + 0.5 * |{a, b, c, d, e}|
        let baseline = gain(&g, &State::default(), "a");
        assert_eq!(baseline, 3.0);
        let explicit = gain(&g, &state_of(&[], &[], &[("a", DEFAULT_BELIEF)]), "a");
        assert_eq!(explicit, baseline);
        let nan = gain(&g, &state_of(&[], &[], &[("a", f32::NAN)]), "a");
        assert!(!nan.is_nan(), "a NaN belief leaked into the gain");
        assert_eq!(nan, baseline);
    }

    #[test]
    fn gain_survives_beliefs_outside_the_unit_interval() {
        // A state can arrive holding anything. A belief outside 0.0..=1.0 has to be
        // read as clamped into it, because `p * a + (1 - p) * d` with an infinite p
        // is NaN, and a NaN gain makes the selection below meaningless.
        let g = single();
        for p in [5.0, -3.0, f32::INFINITY, f32::NEG_INFINITY] {
            let s = state_of(&[], &[], &[("a", p)]);
            assert_eq!(gain(&g, &s, "a"), 1.0, "isolated node at belief {p}");
        }
        let g = chain(&["a", "b", "c", "d", "e"]);
        for p in [5.0, -3.0, f32::INFINITY, f32::NEG_INFINITY] {
            let s = state_of(&[], &[], &[("c", p)]);
            let v = gain(&g, &s, "c");
            assert!(v.is_finite(), "gain at belief {p} was {v}");
        }
    }

    #[test]
    fn gain_follows_requires_edges_only() {
        let g = graph_with(
            &["a", "h", "t"],
            vec![
                req("a", "t"),
                edge("h", "t", EdgeType::Helps),
                edge("t", "h", EdgeType::Encompasses),
            ],
        );
        let s = State::default();
        // {t} + {a} on one side, {t} alone on the other: `h` is on neither.
        assert_eq!(gain(&g, &s, "t"), 1.5);
        // `h` has no requires edges at all, so it is isolated for our purposes.
        assert_eq!(gain(&g, &s, "h"), 1.0);
    }

    #[test]
    fn gain_terminates_on_a_cycle_that_survived_validation() {
        let g = graph(&["a", "b"], &[("a", "b"), ("b", "a")]);
        // Each node is both an ancestor and a descendant of the other.
        assert_eq!(gain(&g, &State::default(), "a"), 2.0);
        assert_eq!(gain(&g, &State::default(), "b"), 2.0);
        let g = graph(&["a"], &[("a", "a")]);
        assert_eq!(gain(&g, &State::default(), "a"), 1.0);
    }

    #[test]
    fn gain_on_an_empty_graph_and_on_ids_naming_no_node() {
        let g = empty();
        // Whatever an id that names no node is worth, it is a number, and being
        // resolved still zeroes it.
        assert!(gain(&g, &State::default(), "ghost").is_finite());
        assert_eq!(gain(&g, &state_of(&["ghost"], &[], &[]), "ghost"), 0.0);
        assert_eq!(gain(&g, &state_of(&[], &["ghost"], &[]), "ghost"), 0.0);
        assert!(gain(&g, &State::default(), "").is_finite());

        // A real graph is not disturbed by being asked about a ghost.
        let g = chain(&["a", "b"]);
        assert!(gain(&g, &State::default(), "ghost").is_finite());
        assert_eq!(gain(&g, &state_of(&["ghost"], &[], &[]), "ghost"), 0.0);
        // The formula is `|({n} + requires_ancestors(n)) \ known|` as written, and
        // `requires_ancestors` reports an id named only by a dangling edge — so a
        // ghost prerequisite counts, and is then cancelled by putting it in `known`.
        let g = graph_with(&["b"], vec![req("ghost", "b")]);
        assert_eq!(g.requires_ancestors("b"), set(&["ghost"]));
        assert_eq!(gain(&g, &State::default(), "b"), 1.5);
        assert_eq!(gain(&g, &state_of(&["ghost"], &[], &[]), "b"), 1.0);
    }

    // ==================================================================
    // next_step: selection
    // ==================================================================

    #[test]
    fn selection_prefers_the_high_leverage_node_over_the_endpoints() {
        // aa1, aa2 -> mid -> zz1, zz2. `mid` collapses three nodes whichever way it
        // is answered; every endpoint collapses one on one side and four on the
        // other, which at the default belief is worth strictly less.
        let g = graph(
            &["aa1", "aa2", "mid", "zz1", "zz2"],
            &[("aa1", "mid"), ("aa2", "mid"), ("mid", "zz1"), ("mid", "zz2")],
        );
        let s = State::default();
        assert_eq!(gain(&g, &s, "mid"), 3.0);
        assert_eq!(gain(&g, &s, "aa1"), 2.5);
        assert_eq!(gain(&g, &s, "zz2"), 2.5);
        let q = ask(next_step(&g, &s, &BTreeSet::new(), &cfg(30, 2.0)));
        // Not the lowest id, not an endpoint: the leverage decides.
        assert_eq!(q.node, "mid");
        assert_eq!(q.gain, 3.0);
    }

    #[test]
    fn a_uniform_chain_ties_on_gain_and_breaks_toward_the_balanced_node() {
        // On `a -> b -> c -> d -> e` with uniform beliefs every node is worth
        // p*(depth+1) + (1-p)*(5-depth), which at p = 0.5 is 3.0 everywhere: no node
        // is better *in expectation*. The tie then breaks on worst-case resolution,
        // which picks the middle of the chain — the node that resolves three either
        // way, rather than an endpoint that resolves one if the answer goes the
        // wrong way.
        let g = chain(&["a", "b", "c", "d", "e"]);
        let s = State::default();
        for id in ["a", "b", "c", "d", "e"] {
            assert_eq!(gain(&g, &s, id), 3.0, "chain gain at {id}");
        }
        let q = ask(next_step(&g, &s, &BTreeSet::new(), &cfg(30, 2.0)));
        assert_eq!(q.node, "c");
        assert_eq!(q.gain, 3.0);
        // Uniform at a belief other than 0.5 tilts the whole chain one way, so the
        // extreme wins on gain outright and no tie-break is involved.
        let low = state_of(&[], &[], &[("a", 0.9), ("b", 0.9), ("c", 0.9), ("d", 0.9), ("e", 0.9)]);
        assert_eq!(ask(next_step(&g, &low, &BTreeSet::new(), &cfg(30, 2.0))).node, "e");
    }

    #[test]
    fn ties_break_toward_the_belief_closest_to_one_half() {
        // Two isolated nodes, so both gains are exactly 1.0 whatever the beliefs.
        // `aa` sorts first, so an id-only tie-break would pick it; the contract
        // picks the coin-flip node instead.
        let g = graph(&["aa", "bb"], &[]);
        let s = state_of(&[], &[], &[("aa", 0.9), ("bb", 0.5)]);
        assert_eq!(gain(&g, &s, "aa"), 1.0);
        assert_eq!(gain(&g, &s, "bb"), 1.0);
        let q = ask(next_step(&g, &s, &BTreeSet::new(), &cfg(30, 0.5)));
        assert_eq!(q.node, "bb");
        assert_eq!(q.gain, 1.0);

        // Symmetric on the other side of 0.5, and the tie-break is on distance from
        // 0.5, not on the belief itself: 0.4 beats 0.9.
        let s = state_of(&[], &[], &[("aa", 0.9), ("bb", 0.4)]);
        assert_eq!(ask(next_step(&g, &s, &BTreeSet::new(), &cfg(30, 0.5))).node, "bb");
        let s = state_of(&[], &[], &[("aa", 0.4), ("bb", 0.9)]);
        assert_eq!(ask(next_step(&g, &s, &BTreeSet::new(), &cfg(30, 0.5))).node, "aa");
        // Equal distance on both sides falls through to the node id.
        let s = state_of(&[], &[], &[("aa", 0.75), ("bb", 0.25)]);
        assert_eq!(ask(next_step(&g, &s, &BTreeSet::new(), &cfg(30, 0.5))).node, "aa");
    }

    #[test]
    fn a_nan_belief_counts_as_the_default_for_the_tie_break() {
        let g = graph(&["aa", "bb"], &[]);
        // `bb` reads as 0.5, which is closer to 0.5 than `aa`'s 0.9.
        let s = state_of(&[], &[], &[("aa", 0.9), ("bb", f32::NAN)]);
        let q = ask(next_step(&g, &s, &BTreeSet::new(), &cfg(30, 0.5)));
        assert_eq!(q.node, "bb");
        assert_eq!(q.gain, 1.0);
        // A node with no belief entry at all behaves the same way.
        let s = state_of(&[], &[], &[("aa", 0.9)]);
        assert_eq!(ask(next_step(&g, &s, &BTreeSet::new(), &cfg(30, 0.5))).node, "bb");
    }

    #[test]
    fn the_question_carries_the_node_probe_and_its_gain() {
        let g = single();
        let q = ask(next_step(&g, &State::default(), &BTreeSet::new(), &cfg(30, 1.0)));
        assert_eq!(
            q,
            Question { node: "a".to_string(), probe: "probe for a".to_string(), gain: 1.0 }
        );
    }

    #[test]
    fn asked_nodes_are_never_offered_again() {
        let g = chain(&["a", "b", "c", "d", "e"]);
        let s = State::default();
        // `c` is the balanced winner, but excluding it does not resurrect it.
        let q = ask(next_step(&g, &s, &set(&["c"]), &cfg(30, 2.0)));
        assert_ne!(q.node, "c");
        let q = ask(next_step(&g, &s, &set(&["a", "b", "c"]), &cfg(30, 2.0)));
        assert!(matches!(q.node.as_str(), "d" | "e"), "got {}", q.node);
        // Asking about ids that are not in the graph excludes nothing.
        let q = ask(next_step(&g, &s, &set(&["ghost"]), &cfg(30, 2.0)));
        assert_eq!(q.node, "c");
    }

    #[test]
    fn selection_is_deterministic_under_adversarial_beliefs() {
        let g = chain(&["a", "b", "c", "d", "e"]);
        let s = state_of(
            &[],
            &[],
            &[("a", f32::NAN), ("b", 5.0), ("c", -2.0), ("d", 0.5), ("e", 1.5), ("ghost", f32::NAN)],
        );
        let first = next_step(&g, &s, &BTreeSet::new(), &cfg(30, 0.5));
        for _ in 0..8 {
            assert_eq!(next_step(&g, &s, &BTreeSet::new(), &cfg(30, 0.5)), first);
        }
        let q = ask(first);
        assert!(g.contains(&q.node), "selected a non-node: {}", q.node);
        assert!(q.gain.is_finite(), "selected gain was {}", q.gain);
    }

    // ==================================================================
    // next_step: stop reasons
    // ==================================================================

    #[test]
    fn complete_when_every_node_is_resolved_and_it_outranks_exhausted() {
        let g = chain(&["a", "b"]);
        let s = state_of(&["a"], &["b"], &[]);
        assert_eq!(
            next_step(&g, &s, &BTreeSet::new(), &cfg(30, 2.0)),
            Step::Stop(StopReason::Complete)
        );
        // Over the question ceiling and below the gain floor at the same time:
        // Complete still wins, because it says something different about the plan.
        assert_eq!(
            next_step(&g, &s, &set(&["a", "b", "c"]), &cfg(1, 1000.0)),
            Step::Stop(StopReason::Complete)
        );
    }

    #[test]
    fn exhausted_when_the_ceiling_is_hit_and_it_outranks_converged() {
        let g = chain(&["a", "b", "c"]);
        let s = State::default();
        // Nothing is resolved, and the gain floor is unreachable, so Converged is
        // also true — but the ceiling is checked first.
        assert_eq!(
            next_step(&g, &s, &set(&["a"]), &cfg(1, 1000.0)),
            Step::Stop(StopReason::Exhausted)
        );
        // `>=`, not `>`.
        assert_eq!(
            next_step(&g, &s, &set(&["a", "b"]), &cfg(1, 0.5)),
            Step::Stop(StopReason::Exhausted)
        );
        // A zero ceiling stops before the first question.
        assert_eq!(
            next_step(&g, &s, &BTreeSet::new(), &cfg(0, 0.5)),
            Step::Stop(StopReason::Exhausted)
        );
        // One under the ceiling still asks.
        assert_eq!(ask(next_step(&g, &s, &set(&["a"]), &cfg(2, 0.5))).node, "b");
    }

    #[test]
    fn converged_when_the_best_gain_is_below_the_floor() {
        let g = single();
        // The one node is worth exactly 1.0, which is under the default floor.
        assert_eq!(
            next_step(&g, &State::default(), &BTreeSet::new(), &cfg(30, 2.0)),
            Step::Stop(StopReason::Converged)
        );
        // A floor exactly at the best gain does not stop: the test is `<`.
        assert_eq!(
            ask(next_step(&g, &State::default(), &BTreeSet::new(), &cfg(30, 1.0))).node,
            "a"
        );

        // Converged also covers "everything left has already been asked", even with
        // questions to spare and a floor of zero.
        let g = chain(&["a", "b"]);
        assert_eq!(
            next_step(&g, &State::default(), &set(&["a", "b"]), &cfg(30, 0.5)),
            Step::Stop(StopReason::Converged)
        );
    }

    #[test]
    fn next_step_on_an_empty_and_a_single_node_graph() {
        // An empty graph has no unresolved node, so it is vacuously Complete —
        // and Complete is checked first, before the ceiling and the floor.
        let g = empty();
        assert_eq!(
            next_step(&g, &State::default(), &BTreeSet::new(), &cfg(30, 2.0)),
            Step::Stop(StopReason::Complete)
        );
        assert_eq!(
            next_step(&g, &State::default(), &set(&["ghost"]), &cfg(0, 0.0)),
            Step::Stop(StopReason::Complete)
        );
        // A state describing nodes that do not exist does not change that.
        let s = state_of(&["ghost"], &["other"], &[("ghost", 0.5)]);
        assert_eq!(
            next_step(&g, &s, &BTreeSet::new(), &cfg(30, 2.0)),
            Step::Stop(StopReason::Complete)
        );

        let g = single();
        let q = ask(next_step(&g, &State::default(), &BTreeSet::new(), &cfg(30, 0.5)));
        assert_eq!(q.node, "a");
        assert_eq!(q.gain, 1.0);
        assert_eq!(
            next_step(&g, &state_of(&["a"], &[], &[]), &BTreeSet::new(), &cfg(30, 0.5)),
            Step::Stop(StopReason::Complete)
        );
        assert_eq!(
            next_step(&g, &state_of(&[], &["a"], &[]), &BTreeSet::new(), &cfg(30, 0.5)),
            Step::Stop(StopReason::Complete)
        );
    }

    #[test]
    fn next_step_terminates_on_a_cycle_that_survived_validation() {
        let g = graph(&["a", "b", "c"], &[("a", "b"), ("b", "c"), ("c", "a")]);
        let q = ask(next_step(&g, &State::default(), &BTreeSet::new(), &cfg(30, 2.0)));
        // Every node reaches every other in both directions: a flat 3.0 tie.
        assert_eq!(q.gain, 3.0);
        assert_eq!(q.node, "a");
    }

    // ==================================================================
    // record: closure directions
    // ==================================================================

    #[test]
    fn pass_marks_the_whole_ancestor_cone_known_in_one_call() {
        let g = chain(&["a", "b", "c", "d", "e"]);
        let mut s = State::default();
        let outcome = record(&g, &mut s, "d", Verdict::Pass, "how do you d?", "2026-08-05");
        assert_eq!(outcome, RecordOutcome::Applied);
        assert_eq!(s.known, set(&["a", "b", "c", "d"]));
        // The other direction is untouched: `e` is harder than what was proved.
        assert!(!s.known.contains("e"), "PASS walked descendants");
        assert!(s.unknown.is_empty());
        for id in ["a", "b", "c", "d"] {
            assert_eq!(s.belief.get(id), Some(&1.0), "belief of {id} after PASS");
        }
        assert_eq!(s.belief.get("e"), None);
        assert_invariants(&g, &s);

        assert_eq!(s.evidence.len(), 1);
        assert_eq!(s.evidence[0].node, "d");
        assert_eq!(s.evidence[0].verdict, Verdict::Pass);
        assert_eq!(s.evidence[0].probe, "how do you d?");
        assert_eq!(s.evidence[0].at, "2026-08-05");
    }

    #[test]
    fn fail_marks_the_whole_descendant_cone_unknown_in_one_call() {
        let g = chain(&["a", "b", "c", "d", "e"]);
        let mut s = State::default();
        let outcome = record(&g, &mut s, "b", Verdict::Fail, "how do you b?", "2026-08-05");
        assert_eq!(outcome, RecordOutcome::Applied);
        assert_eq!(s.unknown, set(&["b", "c", "d", "e"]));
        // The other direction is untouched: `a` is easier than what was failed.
        assert!(!s.unknown.contains("a"), "FAIL walked ancestors");
        assert!(s.known.is_empty());
        for id in ["b", "c", "d", "e"] {
            assert_eq!(s.belief.get(id), Some(&0.0), "belief of {id} after FAIL");
        }
        assert_eq!(s.belief.get("a"), None);
        assert_invariants(&g, &s);

        assert_eq!(s.evidence.len(), 1);
        assert_eq!(s.evidence[0].node, "b");
        assert_eq!(s.evidence[0].verdict, Verdict::Fail);
    }

    #[test]
    fn partial_marks_the_ancestors_but_leaves_the_node_out_of_known() {
        let g = chain(&["a", "b", "c", "d", "e"]);
        let mut s = State::default();
        let outcome = record(&g, &mut s, "d", Verdict::Partial, "how do you d?", "t");
        assert_eq!(outcome, RecordOutcome::Applied);
        assert_eq!(s.known, set(&["a", "b", "c"]));
        assert!(!s.known.contains("d"), "PARTIAL put the node itself in known");
        assert!(!s.unknown.contains("d"), "PARTIAL put the node in unknown");
        assert!(s.unknown.is_empty());
        for id in ["a", "b", "c"] {
            assert_eq!(s.belief.get(id), Some(&1.0), "belief of {id} after PARTIAL");
        }
        // It stays in the plan as cheap review, at a coin flip.
        assert_eq!(s.belief.get("d"), Some(&DEFAULT_BELIEF));
        assert_eq!(s.belief.get("e"), None);
        assert_invariants(&g, &s);
        assert_eq!(s.evidence.len(), 1);
        assert_eq!(s.evidence[0].verdict, Verdict::Partial);

        // A partial on a root has no ancestors to credit.
        let mut s = State::default();
        record(&g, &mut s, "a", Verdict::Partial, "p", "t");
        assert!(s.known.is_empty());
        assert_eq!(s.belief.get("a"), Some(&DEFAULT_BELIEF));
    }

    #[test]
    fn pass_and_fail_move_nodes_between_the_two_sets() {
        // Entry state deliberately violates disjointness: `a` is in both.
        let g = chain(&["a", "b", "c"]);
        let mut s = state_of(&["a", "b"], &["a", "c"], &[]);
        assert_eq!(record(&g, &mut s, "b", Verdict::Pass, "p", "t"), RecordOutcome::Applied);
        assert_eq!(s.known, set(&["a", "b"]));
        assert_eq!(s.unknown, set(&["c"]), "PASS did not evict its cone from unknown");
        assert_invariants(&g, &s);

        // ...and the other way: failing out of a fully-known chain.
        let g = chain(&["a", "b", "c", "d", "e"]);
        let mut s = state_of(&["a", "b", "c", "d", "e"], &[], &[]);
        // The evidence log is empty, so the careless-error rule does not fire here:
        // it counts recorded PASS verdicts, not membership of `known`.
        assert_eq!(record(&g, &mut s, "c", Verdict::Fail, "p", "t"), RecordOutcome::Applied);
        assert_eq!(s.unknown, set(&["c", "d", "e"]));
        assert_eq!(s.known, set(&["a", "b"]), "FAIL did not evict its cone from known");
        for id in ["c", "d", "e"] {
            assert_eq!(s.belief.get(id), Some(&0.0));
        }
        assert_invariants(&g, &s);
    }

    #[test]
    fn partial_evicts_its_ancestors_from_unknown() {
        let g = chain(&["a", "b", "c"]);
        let mut s = state_of(&[], &["a"], &[("a", 0.0)]);
        assert_eq!(record(&g, &mut s, "b", Verdict::Partial, "p", "t"), RecordOutcome::Applied);
        assert_eq!(s.known, set(&["a"]));
        assert!(s.unknown.is_empty(), "PARTIAL left {:?} in unknown", s.unknown);
        assert_eq!(s.belief.get("a"), Some(&1.0));
        assert_invariants(&g, &s);
    }

    #[test]
    fn record_keeps_the_invariants_across_a_mixed_sequence_on_a_deep_graph() {
        // aa -> bb -> dd, aa -> cc -> dd, dd -> ee -> ff.
        let g = graph(
            &["aa", "bb", "cc", "dd", "ee", "ff"],
            &[("aa", "bb"), ("aa", "cc"), ("bb", "dd"), ("cc", "dd"), ("dd", "ee"), ("ee", "ff")],
        );
        let mut s = State::default();
        for (id, verdict) in [
            ("dd", Verdict::Pass),
            ("ff", Verdict::Partial),
            ("ee", Verdict::Fail),
            ("cc", Verdict::Fail),
            ("bb", Verdict::Pass),
        ] {
            record(&g, &mut s, id, verdict, "p", "t");
            assert_invariants(&g, &s);
        }
        // dd PASS: known {aa,bb,cc,dd}. ff PARTIAL: adds ee. ee FAIL: unknown
        // {ee,ff}, known back to {aa,bb,cc,dd}. cc FAIL: unknown {cc,dd,ee,ff},
        // known {aa,bb}. bb PASS: known {aa,bb} again.
        assert_eq!(s.known, set(&["aa", "bb"]));
        assert_eq!(s.unknown, set(&["cc", "dd", "ee", "ff"]));
        assert_eq!(s.evidence.len(), 5);
    }

    #[test]
    fn record_terminates_on_a_cycle_that_survived_validation() {
        let g = graph(&["a", "b"], &[("a", "b"), ("b", "a")]);
        let mut s = State::default();
        assert_eq!(record(&g, &mut s, "a", Verdict::Pass, "p", "t"), RecordOutcome::Applied);
        assert_eq!(s.known, set(&["a", "b"]));
        assert!(s.unknown.is_empty());

        let mut s = State::default();
        assert_eq!(record(&g, &mut s, "a", Verdict::Fail, "p", "t"), RecordOutcome::Applied);
        assert_eq!(s.unknown, set(&["a", "b"]));
        assert!(s.known.is_empty());
        assert!(
            s.known.intersection(&s.unknown).next().is_none(),
            "a cycle broke disjointness"
        );

        // A self-loop is not an ancestor of itself.
        let g = graph(&["a"], &[("a", "a")]);
        let mut s = State::default();
        record(&g, &mut s, "a", Verdict::Partial, "p", "t");
        assert!(s.known.is_empty());
        assert!(s.unknown.is_empty());
    }

    #[test]
    fn record_always_logs_evidence_even_for_an_id_naming_no_node() {
        let g = empty();
        let mut s = State::default();
        let outcome = record(&g, &mut s, "ghost", Verdict::Pass, "the probe", "2026-08-05");
        assert_eq!(outcome, RecordOutcome::Applied);
        assert_eq!(s.evidence.len(), 1);
        assert_eq!(s.evidence[0].node, "ghost");
        assert_eq!(s.evidence[0].probe, "the probe");
        assert_eq!(s.evidence[0].at, "2026-08-05");
        assert_eq!(s.evidence[0].verdict, Verdict::Pass);
        assert!(
            s.known.intersection(&s.unknown).next().is_none(),
            "known and unknown overlap after recording a ghost id"
        );

        // A real graph, an id that is not in it: the log still grows, in order.
        let g = chain(&["a", "b"]);
        let mut s = State::default();
        record(&g, &mut s, "ghost", Verdict::Fail, "p1", "t1");
        record(&g, &mut s, "b", Verdict::Pass, "p2", "t2");
        record(&g, &mut s, "", Verdict::Partial, "p3", "t3");
        assert_eq!(s.evidence.len(), 3);
        assert_eq!(
            s.evidence.iter().map(|e| e.node.as_str()).collect::<Vec<_>>(),
            vec!["ghost", "b", ""]
        );
        assert_eq!(s.known, set(&["a", "b"]));
    }

    #[test]
    fn record_on_a_single_node_graph() {
        let g = single();
        let mut s = State::default();
        assert_eq!(record(&g, &mut s, "a", Verdict::Pass, "p", "t"), RecordOutcome::Applied);
        assert_eq!(s.known, set(&["a"]));
        assert_eq!(s.belief.get("a"), Some(&1.0));
        assert_invariants(&g, &s);

        assert_eq!(record(&g, &mut s, "a", Verdict::Fail, "p", "t"), RecordOutcome::Applied);
        assert_eq!(s.unknown, set(&["a"]));
        assert!(s.known.is_empty());
        assert_eq!(s.belief.get("a"), Some(&0.0));
        assert_invariants(&g, &s);
        assert_eq!(s.evidence.len(), 2);
    }

    // ==================================================================
    // record: careless errors
    // ==================================================================

    /// `base -> d1`, `base -> d2`: two things that both need `base`.
    fn careless_graph() -> Graph {
        graph(&["base", "d1", "d2"], &[("base", "d1"), ("base", "d2")])
    }

    #[test]
    fn two_passing_descendants_make_a_fail_a_re_ask_and_the_second_fail_applies() {
        let g = careless_graph();
        let mut s = State::default();
        record(&g, &mut s, "d1", Verdict::Pass, "p1", "t1");
        record(&g, &mut s, "d2", Verdict::Pass, "p2", "t2");
        assert_eq!(s.known, set(&["base", "d1", "d2"]));

        let known_before = s.known.clone();
        let unknown_before = s.unknown.clone();
        let outcome = record(&g, &mut s, "base", Verdict::Fail, "p3", "t3");
        assert_eq!(outcome, RecordOutcome::ReAsk);
        assert_eq!(s.known, known_before, "a re-ask changed known");
        assert_eq!(s.unknown, unknown_before, "a re-ask changed unknown");
        // The contradiction is itself worth recording.
        assert_eq!(s.evidence.len(), 3);
        assert_eq!(s.evidence[2].node, "base");
        assert_eq!(s.evidence[2].verdict, Verdict::Fail);
        assert_eq!(s.evidence[2].probe, "p3");
        assert_invariants(&g, &s);

        // Failed again on a different instance: accepted.
        let outcome = record(&g, &mut s, "base", Verdict::Fail, "p4", "t4");
        assert_eq!(outcome, RecordOutcome::Applied);
        assert_eq!(s.unknown, set(&["base", "d1", "d2"]));
        assert!(s.known.is_empty());
        for id in ["base", "d1", "d2"] {
            assert_eq!(s.belief.get(id), Some(&0.0));
        }
        assert_eq!(s.evidence.len(), 4);
        assert_invariants(&g, &s);

        // And a third fail is not a fresh contradiction either.
        assert_eq!(
            record(&g, &mut s, "base", Verdict::Fail, "p5", "t5"),
            RecordOutcome::Applied
        );
        assert_eq!(s.unknown, set(&["base", "d1", "d2"]));
    }

    #[test]
    fn one_passing_descendant_is_not_a_contradiction() {
        let g = careless_graph();
        let mut s = State::default();
        record(&g, &mut s, "d1", Verdict::Pass, "p1", "t1");
        let outcome = record(&g, &mut s, "base", Verdict::Fail, "p2", "t2");
        assert_eq!(outcome, RecordOutcome::Applied);
        assert_eq!(s.unknown, set(&["base", "d1", "d2"]));
        assert!(s.known.is_empty());
        assert_invariants(&g, &s);
    }

    #[test]
    fn passing_ancestors_do_not_make_a_fail_contradictory() {
        // anc1 -> target, anc2 -> target: passing two *easier* things says nothing
        // about the harder one. Only descendant passes are contradictory.
        let g = graph(&["anc1", "anc2", "target"], &[("anc1", "target"), ("anc2", "target")]);
        let mut s = State::default();
        record(&g, &mut s, "anc1", Verdict::Pass, "p1", "t1");
        record(&g, &mut s, "anc2", Verdict::Pass, "p2", "t2");
        assert_eq!(s.known, set(&["anc1", "anc2"]));

        let outcome = record(&g, &mut s, "target", Verdict::Fail, "p3", "t3");
        assert_eq!(outcome, RecordOutcome::Applied, "the closure direction is reversed");
        assert_eq!(s.unknown, set(&["target"]));
        assert_eq!(s.known, set(&["anc1", "anc2"]));
        assert_invariants(&g, &s);
    }

    #[test]
    fn only_pass_verdicts_on_descendants_count_toward_the_re_ask() {
        // Two PARTIALs on descendants are not two PASSes.
        let g = careless_graph();
        let mut s = State::default();
        record(&g, &mut s, "d1", Verdict::Partial, "p1", "t1");
        record(&g, &mut s, "d2", Verdict::Partial, "p2", "t2");
        assert_eq!(s.known, set(&["base"]));
        let outcome = record(&g, &mut s, "base", Verdict::Fail, "p3", "t3");
        assert_eq!(outcome, RecordOutcome::Applied);
        assert_eq!(s.unknown, set(&["base", "d1", "d2"]));
        assert!(s.known.is_empty());
        assert_invariants(&g, &s);

        // Neither are two FAILs.
        let mut s = State::default();
        record(&g, &mut s, "d1", Verdict::Fail, "p1", "t1");
        record(&g, &mut s, "d2", Verdict::Fail, "p2", "t2");
        assert_eq!(
            record(&g, &mut s, "base", Verdict::Fail, "p3", "t3"),
            RecordOutcome::Applied
        );
        assert_eq!(s.unknown, set(&["base", "d1", "d2"]));
    }

    #[test]
    fn a_pass_on_the_node_itself_is_not_the_re_ask_that_disarms_the_rule() {
        // The rule looks for a *previous FAIL on this node*, which is what a re-ask
        // leaves in the log. Earlier evidence of another kind does not count.
        let g = careless_graph();
        let mut s = State::default();
        record(&g, &mut s, "base", Verdict::Pass, "p0", "t0");
        record(&g, &mut s, "d1", Verdict::Pass, "p1", "t1");
        record(&g, &mut s, "d2", Verdict::Pass, "p2", "t2");
        let known_before = s.known.clone();

        let outcome = record(&g, &mut s, "base", Verdict::Fail, "p3", "t3");
        assert_eq!(outcome, RecordOutcome::ReAsk);
        assert_eq!(s.known, known_before);
        assert!(s.unknown.is_empty());
        assert_eq!(s.evidence.len(), 4);

        // A FAIL recorded against a *different* node is not this node's re-ask
        // either.
        let mut s = State::default();
        record(&g, &mut s, "d1", Verdict::Pass, "p1", "t1");
        record(&g, &mut s, "d2", Verdict::Pass, "p2", "t2");
        record(&g, &mut s, "d1", Verdict::Fail, "p3", "t3");
        let known_before = s.known.clone();
        let unknown_before = s.unknown.clone();
        assert_eq!(
            record(&g, &mut s, "base", Verdict::Fail, "p4", "t4"),
            RecordOutcome::ReAsk
        );
        assert_eq!(s.known, known_before);
        assert_eq!(s.unknown, unknown_before);
    }

    #[test]
    fn a_pass_and_a_partial_are_never_re_asked() {
        // The rule is about FAIL only, however contradictory the log looks.
        let g = careless_graph();
        let mut s = State::default();
        record(&g, &mut s, "d1", Verdict::Pass, "p1", "t1");
        record(&g, &mut s, "d2", Verdict::Pass, "p2", "t2");
        let mut s2 = s.clone();
        assert_eq!(
            record(&g, &mut s, "base", Verdict::Pass, "p3", "t3"),
            RecordOutcome::Applied
        );
        assert_eq!(
            record(&g, &mut s2, "base", Verdict::Partial, "p3", "t3"),
            RecordOutcome::Applied
        );
    }

    #[test]
    fn the_re_ask_rule_counts_only_descendants_of_the_failed_node() {
        // Two passes exist, but on nodes in a different branch entirely.
        let g = graph(
            &["base", "d1", "other", "o1", "o2"],
            &[("base", "d1"), ("other", "o1"), ("other", "o2")],
        );
        let mut s = State::default();
        record(&g, &mut s, "o1", Verdict::Pass, "p1", "t1");
        record(&g, &mut s, "o2", Verdict::Pass, "p2", "t2");
        let outcome = record(&g, &mut s, "base", Verdict::Fail, "p3", "t3");
        assert_eq!(outcome, RecordOutcome::Applied);
        assert_eq!(s.unknown, set(&["base", "d1"]));
        assert_eq!(s.known, set(&["other", "o1", "o2"]));
        assert_invariants(&g, &s);
    }

    // ==================================================================
    // seed_prior
    // ==================================================================

    #[test]
    fn seed_prior_clamps_and_replaces_nan() {
        let g = graph(&["a", "b", "c", "d", "e", "f"], &[]);
        let mut s = State::default();
        let missing = seed_prior(
            &g,
            &mut s,
            &prior(&[
                ("a", 1.5),
                ("b", -0.25),
                ("c", f32::NAN),
                ("d", 0.75),
                ("e", f32::INFINITY),
                ("f", f32::NEG_INFINITY),
            ]),
        );
        assert_eq!(missing, ids(&[]));
        assert_eq!(s.belief.get("a"), Some(&1.0));
        assert_eq!(s.belief.get("b"), Some(&0.0));
        assert_eq!(s.belief.get("c"), Some(&DEFAULT_BELIEF));
        assert_eq!(s.belief.get("d"), Some(&0.75));
        assert_eq!(s.belief.get("e"), Some(&1.0));
        assert_eq!(s.belief.get("f"), Some(&0.0));
        assert!(
            s.belief.values().all(|p| p.is_finite() && (0.0..=1.0).contains(p)),
            "seeded beliefs escaped the unit interval: {:?}",
            s.belief
        );
        // Seeding leaves the rest of the state alone.
        assert!(s.known.is_empty());
        assert!(s.unknown.is_empty());
        assert!(s.evidence.is_empty());
    }

    #[test]
    fn seed_prior_reports_ids_that_name_no_node_and_seeds_the_rest() {
        let g = graph(&["a", "b"], &[]);
        let mut s = State::default();
        let mut missing = seed_prior(&g, &mut s, &prior(&[("a", 0.1), ("zz", 0.2), ("gh", 0.3)]));
        missing.sort();
        assert_eq!(missing, ids(&["gh", "zz"]));
        assert_eq!(s.belief.get("a"), Some(&0.1));
        assert_eq!(s.belief.len(), 1, "a ghost id was seeded: {:?}", s.belief);
        assert!(!g.contains("zz"));
    }

    #[test]
    fn seed_prior_does_not_overwrite_a_node_already_resolved_by_evidence() {
        let g = chain(&["a", "b", "c"]);
        let mut s = state_of(&["a"], &["c"], &[]);
        let missing = seed_prior(&g, &mut s, &prior(&[("a", 0.2), ("b", 0.3), ("c", 0.4)]));
        assert_eq!(missing, ids(&[]));
        assert_eq!(s.belief.get("a"), None, "a known node was given a prior");
        assert_eq!(s.belief.get("c"), None, "an unknown node was given a prior");
        assert_eq!(s.belief.get("b"), Some(&0.3));

        // The same, against the beliefs that a recorded verdict wrote.
        let mut s = State::default();
        record(&g, &mut s, "b", Verdict::Pass, "p", "t");
        assert_eq!(s.belief.get("a"), Some(&1.0));
        let missing = seed_prior(&g, &mut s, &prior(&[("a", 0.0), ("b", 0.1), ("c", 0.2)]));
        assert_eq!(missing, ids(&[]));
        assert_eq!(s.belief.get("a"), Some(&1.0), "evidence lost to a prior");
        assert_eq!(s.belief.get("b"), Some(&1.0), "evidence lost to a prior");
        assert_eq!(s.belief.get("c"), Some(&0.2));
    }

    #[test]
    fn seed_prior_overwrites_an_earlier_prior_on_an_unresolved_node() {
        let g = single();
        let mut s = State::default();
        seed_prior(&g, &mut s, &prior(&[("a", 0.2)]));
        assert_eq!(s.belief.get("a"), Some(&0.2));
        seed_prior(&g, &mut s, &prior(&[("a", 0.8)]));
        assert_eq!(s.belief.get("a"), Some(&0.8));
    }

    #[test]
    fn seed_prior_on_an_empty_and_a_single_node_graph() {
        let g = empty();
        let mut s = State::default();
        assert_eq!(seed_prior(&g, &mut s, &[]), ids(&[]));
        assert!(s.belief.is_empty());
        let mut missing = seed_prior(&g, &mut s, &prior(&[("a", 0.5), ("b", 2.0)]));
        missing.sort();
        assert_eq!(missing, ids(&["a", "b"]));
        assert!(s.belief.is_empty(), "an empty graph accepted a prior");

        let g = single();
        let mut s = State::default();
        assert_eq!(seed_prior(&g, &mut s, &[]), ids(&[]));
        assert!(s.belief.is_empty());
        assert_eq!(seed_prior(&g, &mut s, &prior(&[("a", 0.9)])), ids(&[]));
        assert_eq!(s.belief.get("a"), Some(&0.9));
        // ...and the seeded belief is what the loop then reads.
        assert_eq!(gain(&g, &s, "a"), 1.0);
    }

    #[test]
    fn a_seeded_prior_changes_which_question_comes_first() {
        let g = chain(&["a", "b", "c", "d", "e"]);
        let mut s = State::default();
        // Uniform 0.5 is a flat tie on gain, broken toward the balanced node.
        assert_eq!(
            ask(next_step(&g, &s, &BTreeSet::new(), &cfg(30, 2.0))).node,
            "c"
        );
        // Confident about the easy end, unsure about the hard end: the hard end is
        // where the leverage is.
        let missing = seed_prior(
            &g,
            &mut s,
            &prior(&[("a", 1.0), ("b", 1.0), ("c", 0.9), ("d", 0.9), ("e", 0.9)]),
        );
        assert_eq!(missing, ids(&[]));
        assert_eq!(ask(next_step(&g, &s, &BTreeSet::new(), &cfg(30, 2.0))).node, "e");
    }

    // ==================================================================
    // the loop, driven to termination
    // ==================================================================

    #[test]
    fn a_loop_that_resolves_everything_stops_complete() {
        // Failing the balanced node collapses its descendant cone; the shallow
        // remainder is then worth exactly one node, so the floor has to be at 1.0
        // for the loop to finish rather than converge with `a` still open.
        let g = chain(&["a", "b", "c", "d"]);
        let (reason, order, state) = drive(&g, &cfg(30, 1.0), &|_| Verdict::Fail);
        assert_eq!(reason, StopReason::Complete);
        assert_eq!(order, ids(&["b", "a"]), "two questions resolve four nodes");
        assert_eq!(state.unknown, set(&["a", "b", "c", "d"]));
        assert!(state.known.is_empty());
        assert_invariants(&g, &state);
    }

    #[test]
    fn a_loop_that_runs_out_of_questions_stops_exhausted() {
        // Three disjoint chains, so nothing anyone answers resolves the others.
        let g = graph(
            &["a1", "a2", "a3", "b1", "b2", "b3", "c1", "c2", "c3"],
            &[
                ("a1", "a2"),
                ("a2", "a3"),
                ("b1", "b2"),
                ("b2", "b3"),
                ("c1", "c2"),
                ("c2", "c3"),
            ],
        );
        let (reason, order, state) = drive(&g, &cfg(2, 1.0), &|_| Verdict::Partial);
        assert_eq!(reason, StopReason::Exhausted);
        assert_eq!(order.len(), 2);
        assert_eq!(order, ids(&["a2", "b2"]), "the balanced node of each chain");
        // Plenty is still unresolved, which is the point of reporting Exhausted.
        assert!(state.unknown.is_empty());
        assert_eq!(state.known, set(&["a1", "b1"]));
        assert_invariants(&g, &state);
    }

    #[test]
    fn a_loop_whose_questions_stop_paying_stops_converged() {
        // Passing the balanced node resolves it and its ancestors; what is left is
        // worth 1.5 nodes, under the floor of 2.0.
        let g = chain(&["a", "b", "c", "d"]);
        let (reason, order, state) = drive(&g, &cfg(30, 2.0), &|_| Verdict::Pass);
        assert_eq!(reason, StopReason::Converged);
        assert_eq!(order, ids(&["b"]));
        assert_eq!(state.known, set(&["a", "b"]));
        assert!(state.unknown.is_empty());
        // `c` and `d` are left unresolved, both worth 1.5.
        assert_eq!(gain(&g, &state, "c"), 1.5);
        assert_eq!(gain(&g, &state, "d"), 1.5);
        assert_invariants(&g, &state);

        // A lower floor buys more questions and finishes the graph off — in three
        // rather than four, because passing the balanced node resolves its ancestor
        // as well as itself.
        let (reason, order, state) = drive(&g, &cfg(30, 1.0), &|_| Verdict::Pass);
        assert_eq!(reason, StopReason::Complete);
        assert_eq!(order, ids(&["b", "c", "d"]));
        assert_eq!(state.known, set(&["a", "b", "c", "d"]));
    }

    #[test]
    fn a_loop_that_hits_a_careless_error_re_asks_and_still_terminates() {
        // The graph is disposable and the state is not, so a durable log can hold
        // passes on nodes that only became descendants of `base` in the graph that
        // was generated this morning. `base` is therefore unresolved and asked, and
        // failing it contradicts the log.
        let g = careless_graph();
        let start = State {
            evidence: vec![logged("d1", Verdict::Pass), logged("d2", Verdict::Pass)],
            ..State::default()
        };
        let (reason, order, state) = drive_from(&g, &cfg(30, 1.0), &|_| Verdict::Fail, start);
        assert_eq!(reason, StopReason::Complete);
        // `base` is worth 2.0 against 1.5 for either descendant, so it goes first,
        // and one applied FAIL then resolves the whole graph. The re-ask is not a
        // second selection.
        assert_eq!(order, ids(&["base"]));
        assert_eq!(state.unknown, set(&["base", "d1", "d2"]));
        assert!(state.known.is_empty());
        // Both the contradictory FAIL and the re-asked one are in the log, after the
        // two passes that made it contradictory.
        assert_eq!(state.evidence.len(), 4);
        let fails = state
            .evidence
            .iter()
            .filter(|e| e.node == "base" && e.verdict == Verdict::Fail)
            .count();
        assert_eq!(fails, 2);
        assert_invariants(&g, &state);
    }

    #[test]
    fn a_loop_over_an_empty_graph_asks_nothing() {
        let g = empty();
        let (reason, order, state) = drive(&g, &cfg(30, 2.0), &|_| Verdict::Pass);
        assert_eq!(reason, StopReason::Complete);
        assert_eq!(order, ids(&[]));
        assert!(state.evidence.is_empty());

        let g = single();
        let (reason, order, state) = drive(&g, &cfg(30, 1.0), &|_| Verdict::Pass);
        assert_eq!(reason, StopReason::Complete);
        assert_eq!(order, ids(&["a"]));
        assert_eq!(state.known, set(&["a"]));
        assert_eq!(state.evidence.len(), 1);
    }

    #[test]
    fn the_loop_replays_identically_from_the_same_state() {
        let g = graph(
            &["aa1", "aa2", "mid", "zz1", "zz2"],
            &[("aa1", "mid"), ("aa2", "mid"), ("mid", "zz1"), ("mid", "zz2")],
        );
        let answer = |id: &str| match id {
            "mid" => Verdict::Partial,
            "zz1" => Verdict::Fail,
            _ => Verdict::Pass,
        };
        let (reason, order, state) = drive(&g, &cfg(30, 1.0), &answer);
        for _ in 0..4 {
            let (r, o, s) = drive(&g, &cfg(30, 1.0), &answer);
            assert_eq!(r, reason);
            assert_eq!(o, order);
            assert_eq!(s.known, state.known);
            assert_eq!(s.unknown, state.unknown);
            assert_eq!(s.belief, state.belief);
        }
        assert!(!order.is_empty());
        assert_eq!(order[0], "mid");
    }

    // ------------------------------------------------------------------
    // declaring what you already know
    // ------------------------------------------------------------------

    #[test]
    fn claiming_to_know_something_primes_it_but_resolves_nothing() {
        // The rule the whole module turns on: `known` must trace to a graded probe.
        let g = chain(&["a", "b", "c", "d"]);
        let mut s = State::default();
        let report = declare(&g, &mut s, &ids(&["c"]), &[], "t0");
        assert_eq!(report.primed, ids(&["c"]));
        assert!(report.conflicts.is_empty());
        assert!(s.known.is_empty(), "self-report was recorded as mastery");
        assert!(s.unknown.is_empty());
        assert_eq!(s.belief.get("c"), Some(&DECLARED_BELIEF));
        assert_eq!(s.belief.get("a"), None, "a prerequisite was primed by proxy");
        assert!(s.evidence.is_empty(), "a claim was logged as evidence");
    }

    #[test]
    fn a_strong_prior_makes_the_question_more_worth_asking_not_less() {
        // Priming must not skip the interview. Closure leverage should now prefer
        // the deep claimed node, because passing it discharges everything beneath.
        let g = chain(&["a", "b", "c", "d"]);
        let mut s = State::default();
        declare(&g, &mut s, &ids(&["d"]), &[], "t0");
        match next_step(&g, &s, &asked(&s), &cfg(30, 1.0)) {
            Step::Ask(q) => assert_eq!(q.node, "d", "the claimed node was not probed first"),
            other => panic!("the loop stopped instead of testing the claim: {other:?}"),
        }
        // ...and one pass there is what actually resolves the cone.
        record(&g, &mut s, "d", Verdict::Pass, "p", "t1");
        assert_eq!(s.known, set(&["a", "b", "c", "d"]));
    }

    #[test]
    fn admitting_ignorance_resolves_immediately_and_takes_its_dependents() {
        // The asymmetry: nobody claims not to know a thing they can do, and the
        // costs of being wrong in this direction are small and loud.
        let g = chain(&["a", "b", "c", "d"]);
        let mut s = State::default();
        let report = declare(&g, &mut s, &[], &ids(&["b"]), "t0");
        assert_eq!(report, Declaration::default());
        assert_eq!(s.unknown, set(&["b", "c", "d"]));
        assert!(s.known.is_empty(), "a: never claimed, so never resolved");
        assert_eq!(s.evidence.len(), 1, "the admission was not recorded");
    }

    #[test]
    fn a_claim_contradicted_by_an_admission_is_reported() {
        // Claiming `c` while admitting its prerequisite `b` is a contradiction. The
        // admission wins, and the claim comes back rather than being quietly dropped.
        let g = chain(&["a", "b", "c"]);
        let mut s = State::default();
        let report = declare(&g, &mut s, &ids(&["c"]), &ids(&["b"]), "t0");
        assert_eq!(report.conflicts, ids(&["c"]), "the contradiction went unreported");
        assert!(report.primed.is_empty());
        assert_eq!(s.unknown, set(&["b", "c"]));
        assert_eq!(s.belief.get("c"), Some(&0.0), "the claim outlived its refutation");
    }

    #[test]
    fn a_claim_the_admission_never_reaches_stands_as_a_prior() {
        let g = chain(&["a", "b", "c", "d"]);
        let mut s = State::default();
        let report = declare(&g, &mut s, &ids(&["b"]), &ids(&["d"]), "t0");
        assert!(report.conflicts.is_empty(), "{report:?}");
        assert_eq!(report.primed, ids(&["b"]));
        assert_eq!(s.belief.get("b"), Some(&DECLARED_BELIEF));
        assert_eq!(s.unknown, set(&["d"]));
    }

    #[test]
    fn a_graded_verdict_is_never_overwritten_by_a_later_claim() {
        let g = chain(&["a", "b"]);
        let mut s = State::default();
        record(&g, &mut s, "b", Verdict::Fail, "p", "t0");
        assert_eq!(s.unknown, set(&["b"]));

        let report = declare(&g, &mut s, &ids(&["b"]), &[], "t1");
        assert_eq!(s.unknown, set(&["b"]), "a claim overturned a graded failure");
        assert_eq!(s.belief.get("b"), Some(&0.0));
        assert_eq!(report.conflicts, ids(&["b"]), "the overruled claim was not reported");
    }

    #[test]
    fn an_admission_retracts_graded_knowledge_and_says_which() {
        // The one direction where a declaration beats evidence. It has to: `known`
        // is downward-closed, so `c` cannot survive its prerequisite going unknown.
        // What is not allowed is doing it quietly.
        let g = chain(&["a", "b", "c"]);
        let mut s = State::default();
        record(&g, &mut s, "c", Verdict::Pass, "graded", "t0");
        assert_eq!(s.known, set(&["a", "b", "c"]));

        let report = declare(&g, &mut s, &[], &ids(&["b"]), "t1");
        assert_eq!(
            report.retracted,
            ids(&["b", "c"]),
            "graded knowledge was discarded without a word"
        );
        assert_eq!(s.known, set(&["a"]), "the untouched prerequisite survived");
        assert_eq!(s.unknown, set(&["b", "c"]));
        assert!(
            g.is_downward_closed(&s.known),
            "known kept a node whose prerequisite is unknown"
        );
    }

    #[test]
    fn an_admission_that_costs_nothing_reports_no_retraction() {
        // Only nodes that were actually in `known` count. A cone of never-resolved
        // nodes going unknown is not a retraction of anything.
        let g = chain(&["a", "b", "c"]);
        let mut s = State::default();
        let report = declare(&g, &mut s, &[], &ids(&["b"]), "t0");
        assert!(report.retracted.is_empty(), "{report:?}");
        assert_eq!(s.unknown, set(&["b", "c"]));

        // A merely *primed* node is not evidence either, so losing it is not a loss.
        let mut s = State::default();
        declare(&g, &mut s, &ids(&["c"]), &[], "t0");
        let report = declare(&g, &mut s, &[], &ids(&["b"]), "t1");
        assert!(
            report.retracted.is_empty(),
            "a prior was reported as retracted evidence: {report:?}"
        );
    }

    #[test]
    fn declared_ids_that_name_no_node_are_reported_and_nothing_else_is_lost() {
        let g = chain(&["a", "b"]);
        let mut s = State::default();
        let report = declare(&g, &mut s, &ids(&["b", "zz", "zz"]), &ids(&["gh"]), "t0");
        assert_eq!(report.missing, ids(&["gh", "zz"]), "sorted and deduplicated");
        assert_eq!(report.primed, ids(&["b"]), "the real claim was dropped too");
        assert!(s.evidence.is_empty(), "a ghost id was recorded: {:?}", s.evidence);
    }

    #[test]
    fn declaring_does_not_spend_the_question_budget() {
        // The reason `asked` filters by source at all: a bulk declaration must not
        // exhaust an interview that has not asked anything.
        let g = chain(&["a", "b", "c", "d", "e"]);
        let mut s = State::default();
        declare(&g, &mut s, &ids(&["b"]), &ids(&["e"]), "t0");
        assert_eq!(s.evidence.len(), 1, "the admission was not logged");
        assert!(asked(&s).is_empty(), "a declaration counted as a question");

        let small = cfg(1, 1.0);
        assert!(
            matches!(next_step(&g, &s, &asked(&s), &small), Step::Ask(_)),
            "a 1-question budget was spent before the first question"
        );

        // ...and an answered question does count, against the same budget.
        record(&g, &mut s, "a", Verdict::Pass, "p", "t1");
        assert_eq!(asked(&s), set(&["a"]));
    }

    #[test]
    fn an_admission_is_evidence_and_says_where_it_came_from() {
        let g = single();
        let mut s = State::default();
        declare(&g, &mut s, &[], &ids(&["a"]), "t0");
        let e = s.evidence.last().expect("nothing logged");
        assert_eq!(e.node, "a");
        assert_eq!(e.verdict, Verdict::Fail);
        assert_eq!(e.at, "t0");
        assert_eq!(e.source, SOURCE_DECLARE);
        assert_ne!(e.source, SOURCE_ASSESS, "indistinguishable from an answer");
    }

    // ------------------------------------------------------------------
    // a probe that graded itself
    // ------------------------------------------------------------------

    #[test]
    fn a_skipped_probe_resolves_nothing_and_moves_no_belief() {
        let g = chain(&["a", "b", "c"]);
        let mut s = state_of(&[], &[], &[("b", 0.7)]);
        let outcome = record(&g, &mut s, "b", Verdict::Skip, "leading question", "t0");
        assert_eq!(outcome, RecordOutcome::Applied);
        assert!(s.known.is_empty(), "a bad question resolved something");
        assert!(s.unknown.is_empty(), "a bad question resolved something");
        assert_eq!(s.belief.get("b"), Some(&0.7), "belief moved on no evidence");
    }

    #[test]
    fn a_skipped_probe_is_still_spent_so_the_loop_advances() {
        // Without this the highest-gain node is offered forever and the interview
        // deadlocks on its worst question.
        let g = chain(&["a", "b", "c"]);
        let mut s = State::default();
        let first = match next_step(&g, &s, &asked(&s), &cfg(30, 1.0)) {
            Step::Ask(q) => q.node,
            other => panic!("expected a question, got {other:?}"),
        };
        record(&g, &mut s, &first, Verdict::Skip, "unanswerable", "t0");
        assert_eq!(asked(&s), set(&[first.as_str()]));
        match next_step(&g, &s, &asked(&s), &cfg(30, 1.0)) {
            Step::Ask(q) => assert_ne!(q.node, first, "the bad probe was offered again"),
            other => panic!("the loop stopped instead of moving on: {other:?}"),
        }
    }

    #[test]
    fn a_skip_is_logged_so_the_probe_can_be_found_and_rewritten() {
        let g = single();
        let mut s = State::default();
        record(&g, &mut s, "a", Verdict::Skip, "why is 'we are 95% sure' wrong?", "t0");
        let e = s.evidence.last().expect("the skip went unrecorded");
        assert_eq!(e.verdict, Verdict::Skip);
        assert_eq!(e.node, "a");
        assert_eq!(e.probe, "why is 'we are 95% sure' wrong?", "the bad text was lost");
    }

    #[test]
    fn a_skip_does_not_count_toward_the_careless_error_contradiction() {
        // The re-ask rule keys on passing descendants. Skips carry no verdict on
        // the learner, so they must not push a later FAIL into a re-ask.
        let g = chain(&["a", "b", "c"]);
        let mut s = State::default();
        record(&g, &mut s, "c", Verdict::Skip, "p", "t0");
        record(&g, &mut s, "b", Verdict::Skip, "p", "t1");
        assert_eq!(
            record(&g, &mut s, "a", Verdict::Fail, "p", "t2"),
            RecordOutcome::Applied,
            "two skips were treated as two passes"
        );
        assert_eq!(s.unknown, set(&["a", "b", "c"]));
    }

}

