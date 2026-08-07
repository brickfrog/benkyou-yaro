//! The prerequisite graph and its invariants.
//!
//! The graph is disposable: generated per goal, regenerated in seconds, thrown away.
//! [`State`] is the only durable artifact. See DESIGN.md §1.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

pub type NodeId = String;

/// What kind of thing a node is. Affects which practice track claims it,
/// never the graph algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Fact,
    Concept,
    Skill,
    Tool,
    Context,
}

/// Where an assertion came from. The weakest claim in the graph is an `llm` edge that
/// nothing else corroborates, which is what makes this worth recording per edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Llm,
    User,
    JobDesc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeType {
    /// Hard prerequisite. Defines the closure and both fringes. Must stay acyclic.
    Requires,
    /// Soft. Affects ordering and priority only, never blocks.
    Helps,
    /// Mastery of `to` grants practice credit for `from`.
    Encompasses,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub title: String,
    pub kind: Kind,
    /// The question that proves this node. A node is defined by its probe,
    /// never by a noun. See DESIGN.md §1.
    pub probe: String,
    #[serde(default)]
    pub goals: Vec<String>,
    pub cost_minutes: u32,
    /// Relevance to *this* goal, 0.0..=1.0. Low-relevance nodes are pruned
    /// rather than the graph being grown to accommodate them.
    pub relevance: f32,
    pub provenance: Provenance,
    /// Whether a script could judge an attempt at this node. `kind` says the node is a
    /// performance; this says a grader can actually see it. They come apart exactly
    /// where a study tool is most tempted to lie: "hold a ten-minute standup in German"
    /// is a `skill`, and no `check.sh` will ever mark it. Author it `false` and `order`
    /// refuses to hand out an exercise for it, leaving `practice` — a score you assign
    /// yourself — as the honest route. Defaults true, because most nodes are.
    #[serde(default = "yes")]
    pub gradable: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    #[serde(rename = "type")]
    pub ty: EdgeType,
    /// 1.0 is a hard block; lower values are advisory.
    pub strength: f32,
    /// One line, human-rejectable. Every edge carries its justification because
    /// a quarter to a third of generated edges are wrong.
    pub reason: String,
    /// Indices into the prerequisite's `goals`: an edge may require only part of
    /// a node. Empty means the whole node.
    #[serde(default)]
    pub needs_goals: Vec<usize>,
    pub provenance: Provenance,
    /// 0.0..=1.0. Author-supplied, so it ranks nothing on its own: a hallucinated edge
    /// is as free to claim 1.0 as a sound one. Reported, never acted on.
    pub confidence: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub target: String,
    #[serde(default)]
    pub deadline: Option<String>,
    pub budget_hours: u32,
}

/// The outcome of putting a node's probe to the learner.
///
/// [`Verdict::Skip`] is the odd one out: it grades the *question*, not the answer.
/// A probe that is leading, ambiguous, or answerable from its own phrasing measures
/// nothing, so it resolves nothing — but it is still spent, and the node needs a
/// replacement probe before it is worth asking again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Partial,
    Fail,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub node: NodeId,
    pub probe: String,
    pub verdict: Verdict,
    pub at: String,
    pub source: String,
}

/// The only durable artifact. `known` is maintained downward-closed under `requires`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub known: BTreeSet<NodeId>,
    #[serde(default)]
    pub unknown: BTreeSet<NodeId>,
    /// p(mastered) for nodes not yet probed.
    #[serde(default)]
    pub belief: BTreeMap<NodeId, f32>,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    pub goal: Goal,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    #[serde(default)]
    pub state: State,
}

/// What [`Graph::validate`] changed. Reported to the user: a generated graph that
/// needed heavy repair is a graph to regenerate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ValidationReport {
    /// `requires` cycles found, each as the edges that form it. Reported, never cut:
    /// the tool cannot tell which edge of a cycle is the wrong one, and guessing
    /// silently rewrote correct curricula. The author resolves it and re-runs.
    pub cycles: Vec<Vec<Edge>>,
    /// Duplicate node ids collapsed onto the first occurrence.
    pub duplicate_nodes: Vec<NodeId>,
    /// Nodes dropped for `relevance` below the threshold.
    pub dropped_irrelevant: Vec<NodeId>,
    /// Nodes dropped because the node cap was hit, least relevant first.
    pub dropped_over_cap: Vec<NodeId>,
    /// Edges dropped because an endpoint no longer exists.
    pub dangling_edges: Vec<Edge>,
}

impl ValidationReport {
    pub fn is_clean(&self) -> bool {
        self.cycles.is_empty()
            && self.duplicate_nodes.is_empty()
            && self.dropped_irrelevant.is_empty()
            && self.dropped_over_cap.is_empty()
            && self.dangling_edges.is_empty()
    }
}

/// Default relevance floor. Below this a node is not worth carrying.
pub const RELEVANCE_FLOOR: f32 = 0.3;

/// Default node cap. More than this cannot be worked through in a ramp anyway.
pub const NODE_CAP: usize = 150;

impl Graph {
    /// Repair a generated graph in place, reporting every change.
    ///
    /// Order matters and is part of the contract:
    /// 1. collapse duplicate node ids onto the first occurrence
    /// 2. drop nodes with `relevance < floor`
    /// 3. if still over `cap`, drop the least relevant until at cap
    /// 4. drop edges whose endpoints no longer exist, and self-loops
    /// 5. report `requires` cycles in full, cutting nothing
    ///
    /// Deterministic: ties break on node id, then on edge `(from, to)`.
    pub fn validate(&mut self, floor: f32, cap: usize) -> ValidationReport {
        let mut report = ValidationReport::default();

        // 1. duplicate ids: the first occurrence wins, later ones are reported.
        let mut seen: BTreeSet<NodeId> = BTreeSet::new();
        let mut kept: Vec<Node> = Vec::with_capacity(self.nodes.len());
        for n in std::mem::take(&mut self.nodes) {
            if seen.insert(n.id.clone()) {
                kept.push(n);
            } else {
                report.duplicate_nodes.push(n.id);
            }
        }
        self.nodes = kept;

        // 2. relevance floor. `NaN < floor` is false, so a NaN relevance survives here
        // and is dealt with by the total order in step 3.
        let mut kept: Vec<Node> = Vec::with_capacity(self.nodes.len());
        for n in std::mem::take(&mut self.nodes) {
            if n.relevance < floor {
                report.dropped_irrelevant.push(n.id);
            } else {
                kept.push(n);
            }
        }
        self.nodes = kept;

        // 3. node cap: least relevant first, ties on id then on position.
        if self.nodes.len() > cap {
            let excess = self.nodes.len() - cap;
            let mut order: Vec<(usize, NodeId, f32)> = self
                .nodes
                .iter()
                .enumerate()
                .map(|(i, n)| (i, n.id.clone(), n.relevance))
                .collect();
            order.sort_by(|a, b| {
                a.2.total_cmp(&b.2)
                    .then_with(|| a.1.cmp(&b.1))
                    .then_with(|| a.0.cmp(&b.0))
            });
            let doomed: BTreeSet<usize> = order.iter().take(excess).map(|(i, _, _)| *i).collect();
            report.dropped_over_cap = order
                .into_iter()
                .take(excess)
                .map(|(_, id, _)| id)
                .collect();
            let mut position = 0usize;
            self.nodes.retain(|_| {
                let keep = !doomed.contains(&position);
                position += 1;
                keep
            });
        }

        // 4. edges left dangling by steps 1-3, plus self-loops of every type.
        let present: BTreeSet<NodeId> = self.nodes.iter().map(|n| n.id.clone()).collect();
        let mut kept_edges: Vec<Edge> = Vec::with_capacity(self.edges.len());
        for e in std::mem::take(&mut self.edges) {
            if e.from == e.to || !present.contains(&e.from) || !present.contains(&e.to) {
                report.dangling_edges.push(e);
            } else {
                kept_edges.push(e);
            }
        }
        self.edges = kept_edges;

        // 5. `requires` cycles: reported in full, never cut.
        //
        // This used to drop the lowest-confidence edge of each cycle. Confidence is
        // written by the same author as the edge, so a hallucinated edge asserted at
        // 1.0 always survived and the modest, correct ones were deleted instead —
        // in place, with no backup, after which re-validating reported `clean`. A
        // silently rewritten curriculum is worse than a rejected one, and nothing in
        // the file distinguishes the wrong edge from the right ones. So the tool
        // names the cycle and the author, who knows the domain, resolves it.
        //
        // Enumeration needs to make progress, which means retiring an edge per round.
        // That happens on a working copy: `self.edges` is restored intact below, so
        // every cycle is reported in one run without the graph losing anything.
        let intact = self.edges.clone();
        while let Some(cycle) = self.find_requires_cycle() {
            let members: Vec<Edge> = self
                .edges
                .iter()
                .filter(|e| {
                    e.ty == EdgeType::Requires && cycle.contains(&(e.from.clone(), e.to.clone()))
                })
                .cloned()
                .collect();
            let mut retirable: Vec<(f32, &NodeId, &NodeId, usize)> = self
                .edges
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.ty == EdgeType::Requires && cycle.contains(&(e.from.clone(), e.to.clone()))
                })
                .map(|(i, e)| (e.confidence, &e.from, &e.to, i))
                .collect();
            retirable.sort_by(|a, b| {
                a.0.total_cmp(&b.0)
                    .then_with(|| a.1.cmp(b.1))
                    .then_with(|| a.2.cmp(b.2))
                    .then_with(|| a.3.cmp(&b.3))
            });
            let Some(&(_, _, _, at)) = retirable.first() else {
                break;
            };
            if at >= self.edges.len() {
                break;
            }
            self.edges.remove(at);
            report.cycles.push(members);
        }
        self.edges = intact;

        report
    }

    /// One simple `requires` cycle among existing nodes, as its set of consecutive
    /// `(from, to)` pairs, or `None` when the `requires` subgraph is acyclic.
    ///
    /// Deterministic: the cycle is the shortest one through the lowest node id that
    /// lies on any cycle.
    fn find_requires_cycle(&self) -> Option<BTreeSet<(NodeId, NodeId)>> {
        let present: BTreeSet<&str> = self.nodes.iter().map(|n| n.id.as_str()).collect();
        let mut succ: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for e in &self.edges {
            if e.ty != EdgeType::Requires || e.from == e.to {
                continue;
            }
            if !present.contains(e.from.as_str()) || !present.contains(e.to.as_str()) {
                continue;
            }
            succ.entry(e.from.as_str())
                .or_default()
                .insert(e.to.as_str());
        }

        for start in present.iter().copied() {
            let mut parent: BTreeMap<&str, &str> = BTreeMap::new();
            let mut visited: BTreeSet<&str> = BTreeSet::new();
            visited.insert(start);
            let mut queue: VecDeque<&str> = VecDeque::new();
            queue.push_back(start);
            while let Some(cur) = queue.pop_front() {
                let Some(next) = succ.get(cur) else { continue };
                for n in next.iter().copied() {
                    if n == start {
                        // start -> ... -> cur -> start, walked back up the BFS tree.
                        let mut pairs: BTreeSet<(NodeId, NodeId)> = BTreeSet::new();
                        pairs.insert((cur.to_string(), start.to_string()));
                        let mut at = cur;
                        while let Some(p) = parent.get(at).copied() {
                            pairs.insert((p.to_string(), at.to_string()));
                            at = p;
                        }
                        return Some(pairs);
                    }
                    if visited.insert(n) {
                        parent.insert(n, cur);
                        queue.push_back(n);
                    }
                }
            }
        }
        None
    }

    /// Everything reachable from `id` along `requires` edges, backwards when
    /// `ancestors` and forwards otherwise. `id` itself is never included, and a
    /// cyclic graph terminates because nodes are only ever queued once.
    fn requires_reachable(&self, id: &str, ancestors: bool) -> BTreeSet<NodeId> {
        let mut seen: BTreeSet<NodeId> = BTreeSet::new();
        let mut stack: Vec<NodeId> = vec![id.to_string()];
        while let Some(cur) = stack.pop() {
            for e in &self.edges {
                if e.ty != EdgeType::Requires {
                    continue;
                }
                let next = if ancestors {
                    if e.to != cur {
                        continue;
                    }
                    &e.from
                } else {
                    if e.from != cur {
                        continue;
                    }
                    &e.to
                };
                if *next == cur {
                    continue;
                }
                if seen.insert(next.clone()) {
                    stack.push(next.clone());
                }
            }
        }
        seen.remove(id);
        seen
    }

    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.nodes.iter().any(|n| n.id == id)
    }

    /// Every node reachable from `id` by following `requires` edges backwards:
    /// everything that must be known before `id`. Excludes `id` itself.
    pub fn requires_ancestors(&self, id: &str) -> BTreeSet<NodeId> {
        self.requires_reachable(id, true)
    }

    /// Every node that transitively requires `id`. Excludes `id` itself.
    pub fn requires_descendants(&self, id: &str) -> BTreeSet<NodeId> {
        self.requires_reachable(id, false)
    }

    /// True when every member's `requires`-ancestors are also members.
    pub fn is_downward_closed(&self, known: &BTreeSet<NodeId>) -> bool {
        known
            .iter()
            .all(|id| self.requires_ancestors(id).iter().all(|a| known.contains(a)))
    }

    /// `known` plus the `requires`-ancestors of everything in it. This closure is
    /// where the pruning leverage comes from: one PASS collapses a whole cone.
    pub fn close_known(&self, known: &BTreeSet<NodeId>) -> BTreeSet<NodeId> {
        let mut closed = known.clone();
        for id in known {
            closed.extend(self.requires_ancestors(id));
        }
        closed
    }

    /// `{ n not in known : every requires-predecessor of n is in known }` —
    /// what the learner is ready to start. Sorted by node id.
    pub fn outer_fringe(&self, known: &BTreeSet<NodeId>) -> Vec<NodeId> {
        let mut fringe: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|n| !known.contains(&n.id))
            .filter(|n| {
                self.edges.iter().all(|e| {
                    e.ty != EdgeType::Requires
                        || e.to != n.id
                        || e.from == n.id
                        || known.contains(&e.from)
                })
            })
            .map(|n| n.id.clone())
            .collect();
        fringe.sort();
        fringe.dedup();
        fringe
    }

    /// `{ n in known : known without n is still downward-closed }` —
    /// the most advanced things the learner has. Sorted by node id.
    pub fn inner_fringe(&self, known: &BTreeSet<NodeId>) -> Vec<NodeId> {
        // `known` is a BTreeSet, so this is already ascending by node id.
        known
            .iter()
            .filter(|id| self.contains(id))
            .filter(|id| {
                let mut rest = known.clone();
                rest.remove(*id);
                self.is_downward_closed(&rest)
            })
            .cloned()
            .collect()
    }

    /// Study order for everything still needed to reach `target`.
    ///
    /// Topological over `requires`, restricted to nodes that `target` transitively
    /// requires (plus `target`), minus `close_known(known)`. Within a topological
    /// level, cheapest `cost_minutes` first, ties on node id.
    ///
    /// The budget is greedy over that order. A node whose `cost_minutes` would push
    /// the total over `budget_minutes` is skipped, and so is everything in the
    /// candidate set that transitively requires it — scheduling a dependent without
    /// its prerequisite is not a study order. Independent branches are unaffected,
    /// so later cheap nodes still land. The result is therefore always
    /// prerequisite-closed within the candidate set.
    pub fn plan(&self, target: &str, known: &BTreeSet<NodeId>, budget_minutes: u32) -> Vec<NodeId> {
        if budget_minutes == 0 || !self.contains(target) {
            return Vec::new();
        }

        // Candidates: the target's ancestor cone plus the target, minus what is
        // already known. Ids named only by a dangling edge are not schedulable.
        let mut candidates: BTreeSet<NodeId> = self
            .requires_ancestors(target)
            .into_iter()
            .filter(|id| self.contains(id))
            .collect();
        candidates.insert(target.to_string());
        for id in self.close_known(known) {
            candidates.remove(&id);
        }
        if candidates.is_empty() {
            return Vec::new();
        }

        // Prerequisites are taken transitively, so a chain that runs through an
        // already-known node still orders its endpoints correctly.
        let mut preds: BTreeMap<&str, BTreeSet<&NodeId>> = BTreeMap::new();
        let mut succs: BTreeMap<&str, BTreeSet<&NodeId>> = BTreeMap::new();
        for c in &candidates {
            let ancestors: BTreeSet<&NodeId> = self
                .requires_ancestors(c)
                .into_iter()
                .filter_map(|a| candidates.get(&a))
                .collect();
            for p in &ancestors {
                succs.entry(p.as_str()).or_default().insert(c);
            }
            preds.insert(c.as_str(), ancestors);
        }

        // Topological levels by rounds of Kahn: a node settles once every one of its
        // prerequisites has. Anything left over sits in a cycle and goes last.
        let mut waiting: BTreeMap<&str, usize> =
            preds.iter().map(|(id, p)| (*id, p.len())).collect();
        let mut level: BTreeMap<&str, usize> = BTreeMap::new();
        let mut round = 0usize;
        loop {
            let ready: Vec<&str> = waiting
                .iter()
                .filter(|(_, d)| **d == 0)
                .map(|(id, _)| *id)
                .collect();
            if ready.is_empty() {
                break;
            }
            for id in &ready {
                level.insert(id, round);
                waiting.remove(id);
            }
            for id in &ready {
                if let Some(next) = succs.get(id) {
                    for s in next {
                        if let Some(d) = waiting.get_mut(s.as_str()) {
                            *d -= 1;
                        }
                    }
                }
            }
            round += 1;
        }

        let mut ordered: Vec<&NodeId> = candidates.iter().collect();
        ordered.sort_by(|a, b| {
            let la = level.get(a.as_str()).copied().unwrap_or(round);
            let lb = level.get(b.as_str()).copied().unwrap_or(round);
            let ca = self.node(a).map_or(0, |n| n.cost_minutes);
            let cb = self.node(b).map_or(0, |n| n.cost_minutes);
            la.cmp(&lb).then_with(|| ca.cmp(&cb)).then_with(|| a.cmp(b))
        });

        // Greedy over that order. A node that does not fit takes everything that
        // transitively requires it down with it, so the plan stays a study order.
        let budget = u64::from(budget_minutes);
        let mut spent: u64 = 0;
        let mut blocked: BTreeSet<&NodeId> = BTreeSet::new();
        let mut plan: Vec<NodeId> = Vec::new();
        for id in ordered {
            if blocked.contains(id) {
                continue;
            }
            let cost = u64::from(self.node(id).map_or(0, |n| n.cost_minutes));
            if cost <= budget - spent {
                spent += cost;
                plan.push(id.clone());
            } else {
                blocked.insert(id);
                for d in self.requires_descendants(id) {
                    if let Some(c) = candidates.get(&d) {
                        blocked.insert(c);
                    }
                }
            }
        }
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // builders
    // ------------------------------------------------------------------

    fn node(id: &str, cost_minutes: u32, relevance: f32) -> Node {
        Node {
            id: id.to_string(),
            title: id.to_string(),
            kind: Kind::Concept,
            probe: format!("what is {id}?"),
            goals: Vec::new(),
            cost_minutes,
            relevance,
            provenance: Provenance::Llm,
            gradable: true,
        }
    }

    fn edge(from: &str, to: &str, ty: EdgeType, confidence: f32) -> Edge {
        Edge {
            from: from.to_string(),
            to: to.to_string(),
            ty,
            strength: 1.0,
            reason: format!("{from} before {to}"),
            needs_goals: Vec::new(),
            provenance: Provenance::Llm,
            confidence,
        }
    }

    /// `req(a, b)` is `Edge { from: a, to: b, ty: Requires }`: *a is a prerequisite of
    /// b*, so `requires_ancestors("b")` contains `"a"` and `requires_descendants("a")`
    /// contains `"b"`.
    fn req(from: &str, to: &str) -> Edge {
        edge(from, to, EdgeType::Requires, 1.0)
    }

    fn graph(nodes: Vec<Node>, edges: Vec<Edge>) -> Graph {
        Graph {
            goal: Goal {
                id: "g".to_string(),
                target: "t".to_string(),
                deadline: None,
                budget_hours: 40,
            },
            nodes,
            edges,
            state: State::default(),
        }
    }

    fn set(members: &[&str]) -> BTreeSet<NodeId> {
        members.iter().map(|s| s.to_string()).collect()
    }

    fn ids(members: &[&str]) -> Vec<NodeId> {
        members.iter().map(|s| s.to_string()).collect()
    }

    fn node_ids(g: &Graph) -> Vec<NodeId> {
        g.nodes.iter().map(|n| n.id.clone()).collect()
    }

    /// `a -> b`, `a -> c`, `b -> d`, `c -> d`.
    fn diamond() -> Graph {
        graph(
            vec![
                node("a", 10, 1.0),
                node("b", 20, 0.9),
                node("c", 30, 0.8),
                node("d", 40, 0.7),
            ],
            vec![req("a", "b"), req("a", "c"), req("b", "d"), req("c", "d")],
        )
    }

    /// `a -> b -> c -> d`.
    fn chain() -> Graph {
        graph(
            vec![
                node("a", 10, 1.0),
                node("b", 10, 1.0),
                node("c", 10, 1.0),
                node("d", 10, 1.0),
            ],
            vec![req("a", "b"), req("b", "c"), req("c", "d")],
        )
    }

    fn empty() -> Graph {
        graph(Vec::new(), Vec::new())
    }

    fn single() -> Graph {
        graph(vec![node("a", 30, 0.9)], Vec::new())
    }

    // ------------------------------------------------------------------
    // test-side oracles
    // ------------------------------------------------------------------

    /// Kahn over the `Requires` subgraph, ignoring edges with an endpoint that is not
    /// a node. Independent of the code under test.
    fn requires_is_acyclic(g: &Graph) -> bool {
        let present: BTreeSet<&str> = g.nodes.iter().map(|n| n.id.as_str()).collect();
        let mut indegree: BTreeMap<&str, usize> = present.iter().map(|id| (*id, 0)).collect();
        let mut out: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for e in &g.edges {
            if e.ty != EdgeType::Requires {
                continue;
            }
            if !present.contains(e.from.as_str()) || !present.contains(e.to.as_str()) {
                continue;
            }
            out.entry(e.from.as_str()).or_default().push(e.to.as_str());
            if let Some(d) = indegree.get_mut(e.to.as_str()) {
                *d += 1;
            }
        }
        let mut ready: Vec<&str> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(id, _)| *id)
            .collect();
        let mut settled = 0usize;
        while let Some(id) = ready.pop() {
            settled += 1;
            if let Some(next) = out.get(id) {
                for n in next.clone() {
                    if let Some(d) = indegree.get_mut(n) {
                        *d -= 1;
                        if *d == 0 {
                            ready.push(n);
                        }
                    }
                }
            }
        }
        settled == present.len()
    }

    /// The plan contract: no duplicates, nothing outside the candidate set, and for
    /// every planned node every candidate-set node it requires appears *earlier*.
    fn assert_plan_is_prerequisite_closed(
        g: &Graph,
        target: &str,
        known: &BTreeSet<NodeId>,
        plan: &[NodeId],
    ) {
        let mut candidates = g.requires_ancestors(target);
        if g.contains(target) {
            candidates.insert(target.to_string());
        }
        for known_id in g.close_known(known) {
            candidates.remove(&known_id);
        }

        let unique: BTreeSet<&NodeId> = plan.iter().collect();
        assert_eq!(unique.len(), plan.len(), "plan repeats a node: {plan:?}");

        let mut position: BTreeMap<&str, usize> = BTreeMap::new();
        for (i, id) in plan.iter().enumerate() {
            position.insert(id.as_str(), i);
        }

        for (i, id) in plan.iter().enumerate() {
            assert!(
                candidates.contains(id),
                "plan contains {id}, which is not in the candidate set {candidates:?}"
            );
            for prereq in g.requires_ancestors(id) {
                if !candidates.contains(&prereq) {
                    continue;
                }
                match position.get(prereq.as_str()) {
                    None => panic!(
                        "plan {plan:?} schedules {id} but never its candidate-set \
                         prerequisite {prereq}"
                    ),
                    Some(at) => assert!(
                        *at < i,
                        "plan {plan:?} schedules {id} at {i}, before its prerequisite \
                         {prereq} at {at}"
                    ),
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // ancestors and descendants
    // ------------------------------------------------------------------

    #[test]
    fn diamond_ancestors() {
        let g = diamond();
        assert_eq!(g.requires_ancestors("a"), set(&[]));
        assert_eq!(g.requires_ancestors("b"), set(&["a"]));
        assert_eq!(g.requires_ancestors("c"), set(&["a"]));
        assert_eq!(g.requires_ancestors("d"), set(&["a", "b", "c"]));
        // A node that does not exist has no ancestors.
        assert_eq!(g.requires_ancestors("zz"), set(&[]));
    }

    #[test]
    fn diamond_descendants() {
        let g = diamond();
        assert_eq!(g.requires_descendants("a"), set(&["b", "c", "d"]));
        assert_eq!(g.requires_descendants("b"), set(&["d"]));
        assert_eq!(g.requires_descendants("c"), set(&["d"]));
        assert_eq!(g.requires_descendants("d"), set(&[]));
        assert_eq!(g.requires_descendants("zz"), set(&[]));
    }

    #[test]
    fn traversal_follows_requires_only() {
        let g = graph(
            vec![node("a", 10, 1.0), node("h", 10, 1.0), node("t", 10, 1.0)],
            vec![
                req("a", "t"),
                edge("h", "t", EdgeType::Helps, 1.0),
                edge("t", "h", EdgeType::Encompasses, 1.0),
            ],
        );
        assert_eq!(g.requires_ancestors("t"), set(&["a"]));
        assert_eq!(g.requires_descendants("a"), set(&["t"]));
        assert_eq!(g.requires_descendants("t"), set(&[]));
        assert_eq!(g.requires_ancestors("h"), set(&[]));
    }

    #[test]
    fn two_cycle_traversal_terminates() {
        let g = graph(
            vec![node("a", 10, 1.0), node("b", 10, 1.0)],
            vec![req("a", "b"), req("b", "a")],
        );
        // `id` itself is always excluded, even when it is reachable from itself.
        assert_eq!(g.requires_ancestors("a"), set(&["b"]));
        assert_eq!(g.requires_ancestors("b"), set(&["a"]));
        assert_eq!(g.requires_descendants("a"), set(&["b"]));
        assert_eq!(g.requires_descendants("b"), set(&["a"]));
    }

    #[test]
    fn three_cycle_traversal_terminates() {
        let g = graph(
            vec![node("a", 10, 1.0), node("b", 10, 1.0), node("c", 10, 1.0)],
            vec![req("a", "b"), req("b", "c"), req("c", "a")],
        );
        assert_eq!(g.requires_ancestors("a"), set(&["b", "c"]));
        assert_eq!(g.requires_descendants("a"), set(&["b", "c"]));
        assert_eq!(g.close_known(&set(&["a"])), set(&["a", "b", "c"]));
        assert!(!g.is_downward_closed(&set(&["a"])));
        assert!(g.is_downward_closed(&set(&["a", "b", "c"])));
        // Every member is an ancestor of another member, so nothing can be removed.
        assert_eq!(g.inner_fringe(&set(&["a", "b", "c"])), ids(&[]));
        // Every node has a prerequisite outside the empty set.
        assert_eq!(g.outer_fringe(&set(&[])), ids(&[]));
    }

    #[test]
    fn self_loop_traversal_terminates() {
        let g = graph(vec![node("a", 10, 1.0)], vec![req("a", "a")]);
        assert_eq!(g.requires_ancestors("a"), set(&[]));
        assert_eq!(g.requires_descendants("a"), set(&[]));
        assert!(g.is_downward_closed(&set(&["a"])));
        assert_eq!(g.close_known(&set(&["a"])), set(&["a"]));
    }

    // ------------------------------------------------------------------
    // closure
    // ------------------------------------------------------------------

    #[test]
    fn marking_a_deep_node_known_pulls_in_its_whole_ancestor_cone() {
        let g = diamond();
        let closed = g.close_known(&set(&["d"]));
        assert_eq!(closed, set(&["a", "b", "c", "d"]));
        assert!(g.is_downward_closed(&closed));
        // ...and the un-closed input was not closed to begin with.
        assert!(!g.is_downward_closed(&set(&["d"])));
    }

    #[test]
    fn close_known_is_idempotent_and_keeps_unrelated_members() {
        let g = diamond();
        let once = g.close_known(&set(&["b", "c"]));
        assert_eq!(once, set(&["a", "b", "c"]));
        assert_eq!(g.close_known(&once), once);
        assert_eq!(g.close_known(&set(&[])), set(&[]));
    }

    #[test]
    fn close_known_carries_ids_that_name_no_node_through_unchanged() {
        let g = diamond();
        assert_eq!(g.close_known(&set(&["ghost"])), set(&["ghost"]));
        assert_eq!(
            g.close_known(&set(&["b", "ghost"])),
            set(&["a", "b", "ghost"])
        );
        // A set of pure ghosts is trivially downward-closed.
        assert!(g.is_downward_closed(&set(&["ghost"])));
        assert!(g.is_downward_closed(&set(&["a", "b", "ghost"])));
    }

    #[test]
    fn is_downward_closed_matches_close_known() {
        let g = diamond();
        assert!(g.is_downward_closed(&set(&[])));
        assert!(g.is_downward_closed(&set(&["a"])));
        assert!(g.is_downward_closed(&set(&["a", "b"])));
        assert!(g.is_downward_closed(&set(&["a", "b", "c", "d"])));
        assert!(!g.is_downward_closed(&set(&["b"])));
        assert!(!g.is_downward_closed(&set(&["a", "d"])));
        for candidate in [
            set(&[]),
            set(&["a"]),
            set(&["b"]),
            set(&["a", "d"]),
            set(&["a", "b", "c", "d"]),
        ] {
            assert_eq!(
                g.is_downward_closed(&candidate),
                g.close_known(&candidate) == candidate,
                "is_downward_closed disagrees with close_known on {candidate:?}"
            );
        }
    }

    // ------------------------------------------------------------------
    // fringes
    // ------------------------------------------------------------------

    #[test]
    fn diamond_outer_fringe() {
        let g = diamond();
        assert_eq!(g.outer_fringe(&set(&[])), ids(&["a"]));
        assert_eq!(g.outer_fringe(&set(&["a"])), ids(&["b", "c"]));
        assert_eq!(g.outer_fringe(&set(&["a", "b"])), ids(&["c"]));
        assert_eq!(g.outer_fringe(&set(&["a", "c"])), ids(&["b"]));
        assert_eq!(g.outer_fringe(&set(&["a", "b", "c"])), ids(&["d"]));
        assert_eq!(g.outer_fringe(&set(&["a", "b", "c", "d"])), ids(&[]));
    }

    #[test]
    fn diamond_inner_fringe() {
        let g = diamond();
        assert_eq!(g.inner_fringe(&set(&[])), ids(&[]));
        assert_eq!(g.inner_fringe(&set(&["a"])), ids(&["a"]));
        assert_eq!(g.inner_fringe(&set(&["a", "b"])), ids(&["b"]));
        assert_eq!(g.inner_fringe(&set(&["a", "b", "c"])), ids(&["b", "c"]));
        assert_eq!(g.inner_fringe(&set(&["a", "b", "c", "d"])), ids(&["d"]));
    }

    #[test]
    fn outer_fringe_of_the_empty_set_is_exactly_the_roots() {
        // r1, r2 have no prerequisites; h has only a `helps` predecessor, which never
        // blocks; m and n sit behind `requires` edges.
        let g = graph(
            vec![
                node("m", 10, 1.0),
                node("r2", 10, 1.0),
                node("h", 10, 1.0),
                node("r1", 10, 1.0),
                node("n", 10, 1.0),
            ],
            vec![
                req("r1", "m"),
                req("r2", "n"),
                edge("r1", "h", EdgeType::Helps, 1.0),
                edge("m", "h", EdgeType::Encompasses, 1.0),
            ],
        );
        assert_eq!(g.outer_fringe(&set(&[])), ids(&["h", "r1", "r2"]));
    }

    #[test]
    fn inner_fringe_is_the_frontier_of_known_not_all_of_it() {
        let g = chain();
        assert_eq!(
            g.inner_fringe(&set(&["a", "b", "c", "d"])),
            ids(&["d"]),
            "only the most advanced member is removable"
        );
        assert_eq!(g.inner_fringe(&set(&["a", "b"])), ids(&["b"]));
        // A member whose removal leaves a downward-closed set qualifies even when the
        // set as a whole is not closed: dropping `c` from {a, c} leaves {a}.
        assert_eq!(g.inner_fringe(&set(&["a", "c"])), ids(&["c"]));
        assert!(!g.is_downward_closed(&set(&["a", "c"])));
    }

    #[test]
    fn inner_fringe_holds_only_nodes_that_exist() {
        let g = diamond();
        assert_eq!(g.inner_fringe(&set(&["a", "b", "ghost"])), ids(&["b"]));
        assert_eq!(g.inner_fringe(&set(&["ghost"])), ids(&[]));
    }

    #[test]
    fn fringes_never_invent_nodes_from_dangling_edges() {
        // `ghost` is named by an edge but is not a node.
        let g = graph(
            vec![node("n", 10, 1.0), node("r", 10, 1.0)],
            vec![req("ghost", "n"), req("r", "n")],
        );
        let outer = g.outer_fringe(&set(&[]));
        assert_eq!(outer, ids(&["r"]));
        let outer_r = g.outer_fringe(&set(&["r"]));
        assert!(!outer_r.iter().any(|id| id == "ghost"));
        assert!(
            outer_r.iter().all(|id| g.contains(id)),
            "outer fringe leaked a non-node: {outer_r:?}"
        );
        assert_eq!(g.inner_fringe(&set(&["r", "ghost"])), ids(&["r"]));
    }

    #[test]
    fn fringes_are_sorted_and_independent_of_node_insertion_order() {
        // Twelve independent roots, inserted in descending id order. A HashMap-ordered
        // implementation will vary run to run; the contract is ascending node id.
        let mut nodes: Vec<Node> = (1..=12).rev().map(|i| node(&format!("n{i:02}"), 7, 1.0)).collect();
        nodes.push(node("t", 1, 1.0));
        let edges: Vec<Edge> = (1..=12).map(|i| req(&format!("n{i:02}"), "t")).collect();
        let g = graph(nodes, edges);

        let expected: Vec<NodeId> = (1..=12).map(|i| format!("n{i:02}")).collect();
        for _ in 0..8 {
            assert_eq!(g.outer_fringe(&set(&[])), expected);
        }

        let all_roots: BTreeSet<NodeId> = expected.iter().cloned().collect();
        for _ in 0..8 {
            assert_eq!(g.outer_fringe(&all_roots), ids(&["t"]));
            assert_eq!(g.inner_fringe(&all_roots), expected);
        }

        // Same for the plan: equal costs on one level tie-break on id, ascending.
        let mut expected_plan = expected.clone();
        expected_plan.push("t".to_string());
        for _ in 0..8 {
            assert_eq!(g.plan("t", &set(&[]), 1000), expected_plan);
        }
    }

    // ------------------------------------------------------------------
    // plan: ordering
    // ------------------------------------------------------------------

    #[test]
    fn plan_orders_by_topological_level_then_cost_then_id() {
        // Levels: {r1, r2} -> {x} -> {t}. Within level 0, r1 (5) is cheaper than
        // r2 (50), so r2 still precedes the cheaper x, which lives a level deeper.
        let g = graph(
            vec![
                node("t", 1, 1.0),
                node("x", 1, 1.0),
                node("r2", 50, 1.0),
                node("r1", 5, 1.0),
            ],
            vec![req("r1", "x"), req("r2", "t"), req("x", "t")],
        );
        let plan = g.plan("t", &set(&[]), 1000);
        assert_eq!(plan, ids(&["r1", "r2", "x", "t"]));
        assert_plan_is_prerequisite_closed(&g, "t", &set(&[]), &plan);
    }

    #[test]
    fn plan_breaks_cost_ties_on_node_id() {
        let g = graph(
            vec![
                node("b", 7, 1.0),
                node("a", 7, 1.0),
                node("c", 3, 1.0),
                node("t", 1, 1.0),
            ],
            vec![req("a", "t"), req("b", "t"), req("c", "t")],
        );
        assert_eq!(g.plan("t", &set(&[]), 1000), ids(&["c", "a", "b", "t"]));
    }

    #[test]
    fn plan_subtracts_the_closure_of_known() {
        let g = chain();
        // Knowing `b` implies knowing `a`, so the plan starts at `c`.
        let plan = g.plan("d", &set(&["b"]), 1000);
        assert_eq!(plan, ids(&["c", "d"]));
        assert_eq!(g.plan("d", &set(&["c"]), 1000), ids(&["d"]));
        assert_eq!(g.plan("d", &set(&["d"]), 1000), ids(&[]));
    }

    #[test]
    fn plan_only_follows_requires_edges() {
        let g = graph(
            vec![node("a", 2, 1.0), node("h", 1, 1.0), node("t", 3, 1.0)],
            vec![req("a", "t"), edge("h", "t", EdgeType::Helps, 1.0)],
        );
        assert_eq!(g.plan("t", &set(&[]), 1000), ids(&["a", "t"]));
    }

    #[test]
    fn plan_of_an_unknown_target_is_empty() {
        let g = diamond();
        assert_eq!(g.plan("zz", &set(&[]), 1000), ids(&[]));
        assert_eq!(g.plan("", &set(&[]), 1000), ids(&[]));
    }

    #[test]
    fn plan_with_a_zero_budget_is_empty() {
        let g = diamond();
        assert_eq!(g.plan("d", &set(&[]), 0), ids(&[]));
        assert_eq!(g.plan("a", &set(&[]), 0), ids(&[]));
    }

    // ------------------------------------------------------------------
    // plan: budget
    // ------------------------------------------------------------------

    #[test]
    fn plan_budget_admits_an_exact_fit_and_rejects_one_minute_over() {
        let g = graph(
            vec![node("a", 10, 1.0), node("t", 5, 1.0)],
            vec![req("a", "t")],
        );
        assert_eq!(g.plan("t", &set(&[]), 15), ids(&["a", "t"]));
        assert_eq!(g.plan("t", &set(&[]), 14), ids(&["a"]));
        assert_eq!(g.plan("t", &set(&[]), 9), ids(&[]));
    }

    #[test]
    fn plan_skips_an_unaffordable_node_but_a_later_cheap_branch_still_lands() {
        // Level 0: b1 (2), e (100). Level 1: b2 (3). Level 2: t (1).
        // `e` is unaffordable and blocks only `t`; the b-branch is independent and
        // b2 comes *after* e in the order, so it proves the scan does not stop.
        let g = graph(
            vec![
                node("e", 100, 1.0),
                node("b1", 2, 1.0),
                node("b2", 3, 1.0),
                node("t", 1, 1.0),
            ],
            vec![req("e", "t"), req("b1", "b2"), req("b2", "t")],
        );
        let plan = g.plan("t", &set(&[]), 20);
        assert_eq!(plan, ids(&["b1", "b2"]));
        assert_plan_is_prerequisite_closed(&g, "t", &set(&[]), &plan);
        // With room for everything the whole cone is planned.
        assert_eq!(
            g.plan("t", &set(&[]), 106),
            ids(&["b1", "e", "b2", "t"]),
            "level 0 is b1 (2) then e (100)"
        );
    }

    #[test]
    fn plan_drops_a_cheap_dependent_of_an_unaffordable_prerequisite() {
        // d costs 2 and would fit on its own, but it requires the unaffordable e.
        let g = graph(
            vec![
                node("e", 100, 1.0),
                node("d", 2, 1.0),
                node("z", 4, 1.0),
                node("t", 1, 1.0),
            ],
            vec![req("e", "d"), req("d", "t"), req("z", "t")],
        );
        let plan = g.plan("t", &set(&[]), 20);
        assert_eq!(plan, ids(&["z"]));
        assert!(!plan.iter().any(|id| id == "d"));
        assert_plan_is_prerequisite_closed(&g, "t", &set(&[]), &plan);
    }

    #[test]
    fn plan_stays_prerequisite_closed_at_every_budget() {
        // Levels: {a(3), b(4), d(6)} -> {e(1), c(2)} -> {t(5)}; order a,b,d,e,c,t.
        let g = graph(
            vec![
                node("a", 3, 1.0),
                node("b", 4, 1.0),
                node("c", 2, 1.0),
                node("d", 6, 1.0),
                node("e", 1, 1.0),
                node("t", 5, 1.0),
            ],
            vec![
                req("a", "c"),
                req("b", "c"),
                req("c", "t"),
                req("d", "e"),
                req("e", "t"),
            ],
        );
        assert_eq!(g.plan("t", &set(&[]), 21), ids(&["a", "b", "d", "e", "c", "t"]));
        assert_eq!(g.plan("t", &set(&[]), 20), ids(&["a", "b", "d", "e", "c"]));
        // 12: d (6) does not fit after a+b = 7, so e is ineligible, but c (2) does.
        assert_eq!(g.plan("t", &set(&[]), 12), ids(&["a", "b", "c"]));
        // 5: only a fits; b's loss takes c with it, d's loss takes e.
        assert_eq!(g.plan("t", &set(&[]), 5), ids(&["a"]));
        assert_eq!(g.plan("t", &set(&[]), 2), ids(&[]));

        for budget in 0..=25 {
            let plan = g.plan("t", &set(&[]), budget);
            assert_plan_is_prerequisite_closed(&g, "t", &set(&[]), &plan);
            let spent: u64 = plan
                .iter()
                .filter_map(|id| g.node(id))
                .map(|n| u64::from(n.cost_minutes))
                .sum();
            assert!(
                spent <= u64::from(budget),
                "plan {plan:?} spends {spent} of a {budget} minute budget"
            );
        }

        for budget in 0..=25 {
            let plan = g.plan("t", &set(&["a"]), budget);
            assert_plan_is_prerequisite_closed(&g, "t", &set(&["a"]), &plan);
            assert!(!plan.iter().any(|id| id == "a"));
        }
    }

    #[test]
    fn plan_does_not_overflow_on_a_huge_cost() {
        let g = graph(
            vec![
                node("big", u32::MAX, 1.0),
                node("cheap", 5, 1.0),
                node("t", 1, 1.0),
            ],
            vec![req("big", "t"), req("cheap", "t")],
        );
        // cheap (5) lands, then big would overflow the running total, so it and its
        // dependent t are dropped.
        assert_eq!(g.plan("t", &set(&[]), u32::MAX), ids(&["cheap"]));
    }

    // ------------------------------------------------------------------
    // validate
    // ------------------------------------------------------------------

    #[test]
    fn validate_leaves_a_clean_graph_alone() {
        let mut g = diamond();
        let before = g.clone();
        let report = g.validate(RELEVANCE_FLOOR, NODE_CAP);
        assert_eq!(report, ValidationReport::default());
        assert!(report.is_clean());
        assert_eq!(g, before);
    }

    #[test]
    fn validate_collapses_duplicate_ids_onto_the_first_occurrence() {
        let mut g = graph(
            vec![
                node("a", 10, 0.9),
                node("b", 20, 0.9),
                // Same id, different payload, and a relevance under the floor: it is
                // dropped as a duplicate in step 1, before the floor is applied.
                node("a", 99, 0.05),
            ],
            vec![req("a", "b")],
        );
        let report = g.validate(0.3, 150);
        assert_eq!(report.duplicate_nodes, ids(&["a"]));
        assert_eq!(report.dropped_irrelevant, ids(&[]));
        assert_eq!(report.dangling_edges, Vec::new());
        assert_eq!(node_ids(&g), ids(&["a", "b"]));
        assert_eq!(g.node("a").map(|n| n.cost_minutes), Some(10));
        assert_eq!(g.node("a").map(|n| n.relevance), Some(0.9));
        assert_eq!(g.edges, vec![req("a", "b")]);
    }

    #[test]
    fn validate_drops_nodes_below_the_floor() {
        let mut g = graph(
            vec![
                node("a", 10, 0.9),
                node("b", 10, 0.29),
                node("c", 10, 0.3),
                node("d", 10, -1.0),
            ],
            vec![req("a", "c")],
        );
        let report = g.validate(0.3, 150);
        // 0.3 is not below 0.3; a negative relevance is.
        assert_eq!(report.dropped_irrelevant, ids(&["b", "d"]));
        assert_eq!(report.dropped_over_cap, ids(&[]));
        assert_eq!(node_ids(&g), ids(&["a", "c"]));
    }

    #[test]
    fn validate_keeps_relevance_above_one() {
        let mut g = graph(vec![node("a", 10, 5.0), node("b", 10, 0.5)], Vec::new());
        let report = g.validate(0.3, 150);
        assert!(report.is_clean());
        assert_eq!(node_ids(&g), ids(&["a", "b"]));
    }

    #[test]
    fn validate_drops_the_least_relevant_over_the_cap() {
        let mut g = graph(
            vec![
                node("a", 10, 0.9),
                node("b", 10, 0.8),
                node("c", 10, 0.5),
                node("d", 10, 0.4),
            ],
            vec![req("a", "b"), req("a", "c")],
        );
        let report = g.validate(0.3, 2);
        assert_eq!(report.dropped_irrelevant, ids(&[]));
        // least relevant first
        assert_eq!(report.dropped_over_cap, ids(&["d", "c"]));
        assert_eq!(node_ids(&g), ids(&["a", "b"]));
        // dropping `c` in step 3 is what makes `a -> c` dangling in step 4
        assert_eq!(report.dangling_edges, vec![req("a", "c")]);
        assert_eq!(g.edges, vec![req("a", "b")]);
    }

    #[test]
    fn validate_drops_edges_naming_a_missing_node() {
        let mut g = graph(
            vec![node("a", 10, 0.9), node("b", 10, 0.9)],
            vec![req("a", "b"), req("ghost", "b")],
        );
        let report = g.validate(0.3, 150);
        assert_eq!(report.dangling_edges, vec![req("ghost", "b")]);
        assert!(report.cycles.is_empty());
        assert_eq!(g.edges, vec![req("a", "b")]);
        assert_eq!(node_ids(&g), ids(&["a", "b"]));
        // and the repaired graph no longer mentions the ghost anywhere
        assert_eq!(g.outer_fringe(&set(&[])), ids(&["a"]));
    }

    #[test]
    fn validate_drops_self_loops_of_every_type() {
        let mut g = graph(
            vec![node("a", 10, 0.9), node("b", 10, 0.9)],
            vec![
                req("a", "a"),
                edge("b", "b", EdgeType::Helps, 0.5),
                req("a", "b"),
            ],
        );
        let report = g.validate(0.3, 150);
        assert_eq!(
            report.dangling_edges,
            vec![req("a", "a"), edge("b", "b", EdgeType::Helps, 0.5)]
        );
        // A self-loop is removed in step 4, so step 5 has no cycle left to report.
        assert!(report.cycles.is_empty());
        assert_eq!(g.edges, vec![req("a", "b")]);
        assert_eq!(g.outer_fringe(&set(&[])), ids(&["a"]));
    }

    #[test]
    fn validate_reports_a_two_cycle_and_keeps_every_edge() {
        let mut g = graph(
            vec![node("a", 10, 0.9), node("b", 10, 0.9)],
            vec![
                edge("a", "b", EdgeType::Requires, 0.9),
                edge("b", "a", EdgeType::Requires, 0.2),
            ],
        );
        let before = g.edges.clone();
        let report = g.validate(0.3, 150);
        assert_eq!(
            report.cycles,
            vec![vec![
                edge("a", "b", EdgeType::Requires, 0.9),
                edge("b", "a", EdgeType::Requires, 0.2),
            ]]
        );
        assert_eq!(g.edges, before, "a reported cycle must cost the graph nothing");
        assert!(!report.is_clean());
    }

    #[test]
    fn validate_reports_the_whole_three_cycle_not_one_suspect_edge() {
        // The author needs every edge of the cycle to decide which is wrong. Naming
        // one of the three is the guess this reports instead of making.
        let mut g = graph(
            vec![node("a", 10, 0.9), node("b", 10, 0.9), node("c", 10, 0.9)],
            vec![
                edge("a", "b", EdgeType::Requires, 0.9),
                edge("b", "c", EdgeType::Requires, 0.1),
                edge("c", "a", EdgeType::Requires, 0.5),
            ],
        );
        let before = g.edges.clone();
        let report = g.validate(0.3, 150);
        assert_eq!(report.cycles.len(), 1);
        assert_eq!(report.cycles[0].len(), 3, "{:?}", report.cycles[0]);
        assert_eq!(g.edges, before);
    }

    /// The regression this whole change exists for. Confidence is written by whoever
    /// wrote the edge, so a hallucinated prerequisite asserted at 1.0 used to survive
    /// while the correct, modestly-scored edges were deleted around it.
    #[test]
    fn a_confident_wrong_edge_no_longer_displaces_the_correct_ones() {
        let mut g = graph(
            vec![node("a", 10, 0.9), node("b", 10, 0.9), node("c", 10, 0.9)],
            vec![
                edge("a", "b", EdgeType::Requires, 0.7),
                edge("b", "c", EdgeType::Requires, 0.7),
                edge("c", "a", EdgeType::Requires, 1.0),
            ],
        );
        let report = g.validate(0.3, 150);
        assert!(!report.is_clean());
        for e in [
            edge("a", "b", EdgeType::Requires, 0.7),
            edge("b", "c", EdgeType::Requires, 0.7),
        ] {
            assert!(g.edges.contains(&e), "a correct edge was deleted: {e:?}");
        }
        assert!(
            g.edges.contains(&edge("c", "a", EdgeType::Requires, 1.0)),
            "the suspect edge is the author's to remove, not the tool's"
        );
    }

    #[test]
    fn validate_reports_every_independent_cycle_in_one_run() {
        // One run has to show all of them, or fixing the first only reveals the next.
        let mut g = graph(
            vec![
                node("a", 10, 0.9),
                node("b", 10, 0.9),
                node("c", 10, 0.9),
                node("d", 10, 0.9),
            ],
            vec![
                edge("a", "b", EdgeType::Requires, 0.9),
                edge("b", "a", EdgeType::Requires, 0.2),
                edge("c", "d", EdgeType::Requires, 0.8),
                edge("d", "c", EdgeType::Requires, 0.1),
            ],
        );
        let before = g.edges.clone();
        let report = g.validate(0.3, 150);
        assert_eq!(report.cycles.len(), 2, "{:?}", report.cycles);
        assert_eq!(g.edges, before);

        // Determinism: the same input yields the same report, order included.
        let mut again = graph(
            vec![
                node("a", 10, 0.9),
                node("b", 10, 0.9),
                node("c", 10, 0.9),
                node("d", 10, 0.9),
            ],
            vec![
                edge("a", "b", EdgeType::Requires, 0.9),
                edge("b", "a", EdgeType::Requires, 0.2),
                edge("c", "d", EdgeType::Requires, 0.8),
                edge("d", "c", EdgeType::Requires, 0.1),
            ],
        );
        assert_eq!(again.validate(0.3, 150), report);
    }

    /// An empty `cycles` report and an acyclic `requires` subgraph are the same
    /// statement. Worth pinning in both directions now that nothing is cut: the report
    /// is the only signal, so it has to track the graph exactly.
    #[test]
    fn an_empty_cycle_report_means_the_requires_graph_is_acyclic() {
        let mut acyclic = graph(
            vec![node("a", 10, 0.9), node("b", 10, 0.9)],
            vec![edge("a", "b", EdgeType::Requires, 0.9)],
        );
        assert!(acyclic.validate(0.3, 150).cycles.is_empty());
        assert!(requires_is_acyclic(&acyclic));

        let mut cyclic = graph(
            vec![node("a", 10, 0.9), node("b", 10, 0.9)],
            vec![
                edge("a", "b", EdgeType::Requires, 0.9),
                edge("b", "a", EdgeType::Requires, 0.2),
            ],
        );
        assert!(!cyclic.validate(0.3, 150).cycles.is_empty());
        assert!(
            !requires_is_acyclic(&cyclic),
            "the cycle was reported and then quietly cut anyway"
        );
    }

    #[test]
    fn validate_never_cuts_helps_or_encompasses_cycles() {
        let mut g = graph(
            vec![node("a", 10, 0.9), node("b", 10, 0.9), node("c", 10, 0.9)],
            vec![
                edge("a", "b", EdgeType::Helps, 0.1),
                edge("b", "a", EdgeType::Helps, 0.2),
                edge("b", "c", EdgeType::Encompasses, 0.1),
                edge("c", "b", EdgeType::Encompasses, 0.2),
                req("a", "c"),
            ],
        );
        let before = g.clone();
        let report = g.validate(0.3, 150);
        assert!(report.is_clean());
        assert_eq!(g, before);
    }

    #[test]
    fn validate_breaks_cycles_last_so_the_report_covers_survivors_only() {
        // b is dropped for irrelevance, which strands both cycle edges as dangling.
        // There is no cycle left among survivors, so `cycles_broken` stays empty.
        let mut g = graph(
            vec![node("a", 10, 0.9), node("b", 10, 0.1)],
            vec![
                edge("a", "b", EdgeType::Requires, 0.5),
                edge("b", "a", EdgeType::Requires, 0.9),
            ],
        );
        let report = g.validate(0.3, 150);
        assert_eq!(report.dropped_irrelevant, ids(&["b"]));
        assert_eq!(
            report.dangling_edges,
            vec![
                edge("a", "b", EdgeType::Requires, 0.5),
                edge("b", "a", EdgeType::Requires, 0.9),
            ]
        );
        assert!(report.cycles.is_empty());
        assert_eq!(node_ids(&g), ids(&["a"]));
        assert_eq!(g.edges, Vec::new());
    }

    #[test]
    fn validate_repairs_everything_at_once_and_is_idempotent() {
        let mut g = graph(
            vec![
                node("a", 10, 0.9),
                node("b", 10, 0.8),
                node("a", 99, 0.9),  // duplicate
                node("junk", 10, 0.05), // below floor
                node("c", 10, 0.7),
            ],
            vec![
                req("a", "b"),
                req("b", "c"),
                edge("c", "a", EdgeType::Requires, 0.15), // closes a 3-cycle
                req("junk", "b"),                         // dangling once junk is gone
                req("b", "b"),                            // self-loop
            ],
        );
        let report = g.validate(0.3, 150);
        assert_eq!(report.duplicate_nodes, ids(&["a"]));
        assert_eq!(report.dropped_irrelevant, ids(&["junk"]));
        assert_eq!(report.dropped_over_cap, ids(&[]));
        assert_eq!(
            report.dangling_edges,
            vec![req("junk", "b"), req("b", "b")]
        );
        // The cycle is reported whole and left in the file; every other repair here
        // is mechanical and still applied around it.
        assert_eq!(report.cycles.len(), 1);
        assert_eq!(node_ids(&g), ids(&["a", "b", "c"]));
        assert_eq!(
            g.edges,
            vec![
                req("a", "b"),
                req("b", "c"),
                edge("c", "a", EdgeType::Requires, 0.15)
            ]
        );
        assert_eq!(g.plan("c", &set(&[]), 100), ids(&["a", "b", "c"]));

        // Idempotent where it can be: every mechanical repair is settled, so a second
        // run finds nothing left to do and touches nothing. The cycle is reported
        // again, identically, because it is still there — it is the author's to resolve.
        let settled = g.clone();
        let second = g.validate(0.3, 150);
        assert_eq!(g, settled, "a second run changed the graph");
        assert!(second.duplicate_nodes.is_empty());
        assert!(second.dropped_irrelevant.is_empty());
        assert!(second.dropped_over_cap.is_empty());
        assert!(second.dangling_edges.is_empty());
        assert_eq!(second.cycles, report.cycles, "the cycle stopped being named");
    }

    // ------------------------------------------------------------------
    // NaN
    // ------------------------------------------------------------------

    #[test]
    fn nan_relevance_is_survivable_and_deterministic() {
        let build = || {
            graph(
                vec![
                    node("a", 10, 1.0),
                    node("nan1", 10, f32::NAN),
                    node("z", 10, 0.0),
                    node("nan2", 10, f32::NAN),
                    node("b", 10, 0.6),
                ],
                vec![req("a", "b")],
            )
        };
        let mut first = build();
        let first_report = first.validate(0.3, 150);
        let mut second = build();
        let second_report = second.validate(0.3, 150);

        // z is unambiguously below the floor; a and b are unambiguously above it.
        assert!(first_report.dropped_irrelevant.contains(&"z".to_string()));
        assert!(!first_report.dropped_irrelevant.contains(&"a".to_string()));
        assert!(!first_report.dropped_irrelevant.contains(&"b".to_string()));
        assert!(first.contains("a") && first.contains("b"));
        assert_eq!(first.edges, vec![req("a", "b")]);
        assert_eq!(first.requires_ancestors("b"), set(&["a"]));

        // Whatever the NaN policy is, it is the same policy twice.
        assert_eq!(first_report, second_report);
        assert_eq!(node_ids(&first), node_ids(&second));
    }

    #[test]
    fn nan_relevance_does_not_break_the_over_cap_sort() {
        let build = || {
            graph(
                vec![
                    node("a", 10, 0.9),
                    node("b", 10, f32::NAN),
                    node("c", 10, 0.7),
                    node("d", 10, f32::NAN),
                    node("e", 10, 0.5),
                    node("f", 10, 0.4),
                ],
                Vec::new(),
            )
        };
        let mut first = build();
        let first_report = first.validate(0.3, 3);
        assert_eq!(first.nodes.len(), 3, "the cap is a hard ceiling");
        assert_eq!(first_report.dropped_over_cap.len(), 3);
        assert_eq!(first_report.dropped_irrelevant, ids(&[]));

        let mut second = build();
        let second_report = second.validate(0.3, 3);
        assert_eq!(first_report, second_report);
        assert_eq!(node_ids(&first), node_ids(&second));
    }

    #[test]
    fn nan_confidence_does_not_break_cycle_cutting() {
        let build = || {
            graph(
                vec![node("a", 10, 0.9), node("b", 10, 0.9), node("c", 10, 0.9)],
                vec![
                    edge("a", "b", EdgeType::Requires, f32::NAN),
                    edge("b", "c", EdgeType::Requires, 0.5),
                    edge("c", "a", EdgeType::Requires, 0.9),
                ],
            )
        };
        let mut first = build();
        let first_report = first.validate(0.3, 150);
        assert_eq!(first_report.cycles.len(), 1, "one cycle, reported once");
        assert_eq!(first.edges.len(), 3, "a NaN confidence still costs no edges");

        let mut second = build();
        let second_report = second.validate(0.3, 150);
        // NaN != NaN, so compare the rendered reports rather than the values.
        assert_eq!(
            format!("{first_report:?}"),
            format!("{second_report:?}"),
            "cycle cutting is not stable under a NaN confidence"
        );
        assert_eq!(format!("{:?}", first.edges), format!("{:?}", second.edges));
    }

    // ------------------------------------------------------------------
    // degenerate graphs
    // ------------------------------------------------------------------

    #[test]
    fn empty_graph_answers_every_method() {
        let mut g = empty();
        assert!(!g.contains("a"));
        assert_eq!(g.node("a"), None);
        assert_eq!(g.requires_ancestors("a"), set(&[]));
        assert_eq!(g.requires_descendants("a"), set(&[]));
        assert_eq!(g.close_known(&set(&[])), set(&[]));
        assert_eq!(g.close_known(&set(&["a"])), set(&["a"]));
        assert!(g.is_downward_closed(&set(&[])));
        assert!(g.is_downward_closed(&set(&["a"])));
        assert_eq!(g.outer_fringe(&set(&[])), ids(&[]));
        assert_eq!(g.outer_fringe(&set(&["a"])), ids(&[]));
        assert_eq!(g.inner_fringe(&set(&[])), ids(&[]));
        assert_eq!(g.inner_fringe(&set(&["a"])), ids(&[]));
        assert_eq!(g.plan("a", &set(&[]), 1000), ids(&[]));
        assert_eq!(g.plan("a", &set(&["a"]), 1000), ids(&[]));

        let report = g.validate(RELEVANCE_FLOOR, NODE_CAP);
        assert_eq!(report, ValidationReport::default());
        assert_eq!(g.nodes, Vec::new());
        assert_eq!(g.edges, Vec::new());
    }

    #[test]
    fn single_node_graph_answers_every_method() {
        let mut g = single();
        assert!(g.contains("a"));
        assert_eq!(g.node("a").map(|n| n.cost_minutes), Some(30));
        assert_eq!(g.requires_ancestors("a"), set(&[]));
        assert_eq!(g.requires_descendants("a"), set(&[]));
        assert_eq!(g.close_known(&set(&["a"])), set(&["a"]));
        assert!(g.is_downward_closed(&set(&["a"])));
        assert_eq!(g.outer_fringe(&set(&[])), ids(&["a"]));
        assert_eq!(g.outer_fringe(&set(&["a"])), ids(&[]));
        assert_eq!(g.inner_fringe(&set(&[])), ids(&[]));
        assert_eq!(g.inner_fringe(&set(&["a"])), ids(&["a"]));
        assert_eq!(g.plan("a", &set(&[]), 30), ids(&["a"]));
        assert_eq!(g.plan("a", &set(&[]), 29), ids(&[]));
        assert_eq!(g.plan("a", &set(&[]), 0), ids(&[]));
        assert_eq!(g.plan("a", &set(&["a"]), 1000), ids(&[]));
        assert_eq!(g.plan("zz", &set(&[]), 1000), ids(&[]));

        let report = g.validate(RELEVANCE_FLOOR, NODE_CAP);
        assert!(report.is_clean());
        assert_eq!(node_ids(&g), ids(&["a"]));
    }

    #[test]
    fn single_node_graph_with_a_self_loop_is_repaired() {
        let mut g = graph(vec![node("a", 30, 0.9)], vec![req("a", "a")]);
        let report = g.validate(RELEVANCE_FLOOR, NODE_CAP);
        assert_eq!(report.dangling_edges, vec![req("a", "a")]);
        assert!(report.cycles.is_empty());
        assert_eq!(g.edges, Vec::new());
        assert_eq!(g.outer_fringe(&set(&[])), ids(&["a"]));
        assert_eq!(g.inner_fringe(&set(&["a"])), ids(&["a"]));
        assert_eq!(g.plan("a", &set(&[]), 30), ids(&["a"]));
    }
}
