//! Scheduling the procedural track.
//!
//! Deliberately not an SRS. FSRS schedules an *item* for re-presentation, and an exercise
//! is consumed by being solved — the second attempt tests recall of your own solution, not
//! the skill. The unit that survives repetition is the concept, with a fresh item each
//! time. The model here is mastery gating — taken from keybr, whose entire value
//! proposition is adaptive practice and which contains no scheduler at all — plus
//! coarse session-level spacing, which is the granularity
//! the procedural-spacing literature actually supports. See DESIGN.md §5.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::graph::{Graph, Node, NodeId};

/// Everything remembered about practice on one concept. Four numbers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fluency {
    /// Best graded score ever achieved, 0.0..=1.0.
    pub best_score: f32,
    /// Whole days since the epoch, or `None` if never practised.
    pub last_practiced: Option<i64>,
    pub attempts: u32,
    /// Current confidence, 0.0..=1.0, before decay is applied.
    pub confidence: f32,
}

impl Default for Fluency {
    fn default() -> Self {
        Self {
            best_score: 0.0,
            last_practiced: None,
            attempts: 0,
            confidence: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedConfig {
    /// Confidence at or above which a concept counts as mastered, and so stops
    /// blocking its dependents.
    pub target: f32,
    /// Days for decayed confidence to fall to half. Session-level spacing, not
    /// item-level intervals.
    pub half_life_days: f32,
    /// Confidence at or above which a concept is retired and stops being scheduled.
    /// Individual learning curves are exponential with a hard asymptote, so
    /// reviewing forever buys nothing.
    pub retire_at: f32,
    /// How many items one session holds.
    pub session_size: usize,
    /// Fraction of a direct attempt's confidence gain that flows to a concept
    /// reached through an `encompasses` edge.
    pub encompass_credit: f32,
    /// A direct attempt scoring below this is a lapse: it discards the confidence
    /// accumulated so far rather than adding nothing to it. Without a lapse rule a
    /// retired concept can never be demoted, so failing its exercise would retire
    /// it forever — the opposite of what failing means.
    pub lapse_at: f32,
}

impl Default for SchedConfig {
    fn default() -> Self {
        Self {
            target: 1.0,
            half_life_days: 21.0,
            retire_at: 1.5,
            session_size: 5,
            encompass_credit: 0.5,
            lapse_at: 0.5,
        }
    }
}

pub type Fluencies = BTreeMap<NodeId, Fluency>;

/// Confidence after exponential decay for time elapsed since last practice.
///
/// `decayed = confidence * 0.5^(days_since / half_life_days)`. Never practised, or
/// `now` earlier than `last_practiced`, yields the undecayed `confidence`. A
/// `half_life_days` of zero or less, or non-finite, disables decay rather than
/// producing infinities.
pub fn decayed_confidence(f: &Fluency, now_days: i64, cfg: &SchedConfig) -> f32 {
    let confidence = finite_nonnegative(f.confidence);
    let Some(last_practiced) = f.last_practiced else {
        return confidence;
    };
    if now_days <= last_practiced || !cfg.half_life_days.is_finite() || cfg.half_life_days <= 0.0 {
        return confidence;
    }

    let days_since = now_days.saturating_sub(last_practiced) as f64;
    let factor = 0.5_f64.powf(days_since / f64::from(cfg.half_life_days));
    finite_nonnegative((f64::from(confidence) * factor) as f32)
}

fn finite_nonnegative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn fluency_for(fluencies: &Fluencies, node: &str) -> Fluency {
    fluencies.get(node).cloned().unwrap_or_default()
}

/// True when every `requires`-predecessor of `node` has decayed confidence at or
/// above `cfg.target`. A node with no prerequisites is always unlocked. A node
/// absent from the graph is never unlocked.
///
/// This is keybr's rule: a new item is admitted only once everything already
/// included is at target.
pub fn is_unlocked(
    graph: &Graph,
    fluencies: &Fluencies,
    node: &str,
    now_days: i64,
    cfg: &SchedConfig,
) -> bool {
    if !graph.nodes.iter().any(|candidate| candidate.id == node) {
        return false;
    }

    graph
        .edges
        .iter()
        .filter(|edge| edge.ty == crate::graph::EdgeType::Requires && edge.to == node)
        .all(|edge| {
            graph
                .nodes
                .iter()
                .any(|candidate| candidate.id == edge.from)
                && decayed_confidence(&fluency_for(fluencies, &edge.from), now_days, cfg)
                    >= cfg.target
        })
}

/// Every node that is unlocked, not retired, and not yet at target — the set the
/// session is drawn from. Sorted by node id.
pub fn practisable(
    graph: &Graph,
    fluencies: &Fluencies,
    now_days: i64,
    cfg: &SchedConfig,
) -> Vec<NodeId> {
    let mut result: Vec<_> = graph
        .nodes
        .iter()
        .filter(|node| is_unlocked(graph, fluencies, &node.id, now_days, cfg))
        .filter(|node| {
            let confidence = decayed_confidence(&fluency_for(fluencies, &node.id), now_days, cfg);
            confidence < cfg.retire_at && confidence < cfg.target
        })
        .map(|node| node.id.clone())
        .collect();
    result.sort();
    result.dedup();
    result
}

/// The single weakest practisable concept: what new practice material should be
/// generated for. Ties break on node id ascending. `None` when nothing is
/// practisable.
pub fn focus(
    graph: &Graph,
    fluencies: &Fluencies,
    now_days: i64,
    cfg: &SchedConfig,
) -> Option<NodeId> {
    focus_where(graph, fluencies, now_days, cfg, |_| true)
}

/// [`focus`], restricted to nodes the caller can actually use.
///
/// The weakest concept overall is not always the weakest concept a given artifact
/// can be built for — a grader cannot exercise a bare fact — so the caller supplies
/// the predicate and the ordering stays in one place.
pub fn focus_where(
    graph: &Graph,
    fluencies: &Fluencies,
    now_days: i64,
    cfg: &SchedConfig,
    keep: impl Fn(&Node) -> bool,
) -> Option<NodeId> {
    practisable(graph, fluencies, now_days, cfg)
        .into_iter()
        .filter(|id| graph.node(id).is_some_and(&keep))
        .min_by(|a, b| {
            let a_confidence = decayed_confidence(&fluency_for(fluencies, a), now_days, cfg);
            let b_confidence = decayed_confidence(&fluency_for(fluencies, b), now_days, cfg);
            a_confidence.total_cmp(&b_confidence).then_with(|| a.cmp(b))
        })
}

/// Compose one session, interleaved across concepts.
///
/// Interleaving rather than blocking is the one scheduling lever with strong
/// evidence behind it (Rohrer & Taylor). The contract is a round-robin, which is
/// the only rule that stays satisfiable when there are fewer practisable concepts
/// than session slots:
///
/// - order [`practisable`] by decayed confidence ascending, ties on node id
///   ascending — call that the rotation
/// - emit the rotation repeatedly until the session holds `cfg.session_size`
///   entries; the last pass may be partial
/// - the session therefore has exactly `cfg.session_size` entries whenever
///   anything is practisable, and concepts repeat when slots outnumber them
/// - **consequence, and the only adjacency guarantee made:** no two adjacent
///   entries share a concept whenever two or more concepts are practisable. With
///   exactly one, the session is that concept repeated, because there is nothing
///   to interleave with
/// - empty when nothing is practisable, or when `cfg.session_size` is 0
pub fn compose_session(
    graph: &Graph,
    fluencies: &Fluencies,
    now_days: i64,
    cfg: &SchedConfig,
) -> Vec<NodeId> {
    if cfg.session_size == 0 {
        return Vec::new();
    }

    let mut rotation = practisable(graph, fluencies, now_days, cfg);
    rotation.sort_by(|a, b| {
        let a_confidence = decayed_confidence(&fluency_for(fluencies, a), now_days, cfg);
        let b_confidence = decayed_confidence(&fluency_for(fluencies, b), now_days, cfg);
        a_confidence.total_cmp(&b_confidence).then_with(|| a.cmp(b))
    });
    if rotation.is_empty() {
        return Vec::new();
    }

    rotation
        .into_iter()
        .cycle()
        .take(cfg.session_size)
        .collect()
}

/// Record a graded attempt and propagate credit.
///
/// On the attempted node: `attempts` increments, `best_score` takes the max,
/// `last_practiced` becomes `now_days`, and `confidence` becomes
/// `decayed_confidence(..) + score`, so a good attempt on a decayed concept both
/// restores and advances it. Clamped to `0.0..=cfg.retire_at`.
///
/// Then the bridge. An `Edge { from, to, ty: Encompasses }` means mastery of `to`
/// grants practice credit for `from`, so `to` is the harder node and `from` the
/// easier one inside it. An attempt on `to` therefore credits `from`, and credit
/// keeps flowing because encompassing composes.
///
/// Credit **attenuates per hop**: a node at encompass-depth `d` from the attempted
/// node gains `score * cfg.encompass_credit.powi(d)`, applied on the same terms as
/// a direct attempt, and has `last_practiced` set. With the default credit of 0.5
/// that is half for a direct encompass, a quarter two hops out. `attempts` is
/// **not** incremented for encompassed nodes; they were not attempted.
///
/// This is what lets one exercise retire a pile of cards instead of competing with
/// them.
///
/// Each node is credited exactly once, at its **shortest** encompass-depth, even
/// when the edges form a cycle or a node encompasses itself. The attempted node
/// itself is never additionally credited as an encompassed node. `score` outside
/// `0.0..=1.0` is clamped; non-finite `score` is ignored and the fluencies are left
/// untouched.
///
/// Returns the ids that received encompassed credit, sorted, for reporting.
pub fn record_attempt(
    graph: &Graph,
    fluencies: &mut Fluencies,
    node: &str,
    score: f32,
    now_days: i64,
    cfg: &SchedConfig,
) -> BTreeSet<NodeId> {
    let credited = BTreeSet::new();
    if !score.is_finite() || !graph.nodes.iter().any(|candidate| candidate.id == node) {
        return credited;
    }

    let score = score.clamp(0.0, 1.0);
    apply_credit(fluencies, node, score, now_days, cfg, true);

    let node_ids: BTreeSet<_> = graph
        .nodes
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect();
    let mut credited = BTreeSet::new();
    let mut seen = BTreeSet::from([node.to_string()]);
    let mut queue = VecDeque::from([(node.to_string(), 0_i32)]);
    let credit_rate = if cfg.encompass_credit.is_finite() {
        cfg.encompass_credit
    } else {
        0.0
    };

    while let Some((encompassing, depth)) = queue.pop_front() {
        for edge in graph.edges.iter().filter(|edge| {
            edge.ty == crate::graph::EdgeType::Encompasses && edge.to == encompassing
        }) {
            if !node_ids.contains(edge.from.as_str()) || !seen.insert(edge.from.clone()) {
                continue;
            }
            let next_depth = depth.saturating_add(1);
            let gain = finite_nonnegative(score * credit_rate.powi(next_depth));
            apply_credit(fluencies, &edge.from, gain, now_days, cfg, false);
            credited.insert(edge.from.clone());
            queue.push_back((edge.from.clone(), next_depth));
        }
    }

    credited
}

fn apply_credit(
    fluencies: &mut Fluencies,
    node: &str,
    gain: f32,
    now_days: i64,
    cfg: &SchedConfig,
    attempted: bool,
) {
    let previous = fluency_for(fluencies, node);
    let ceiling = finite_nonnegative(cfg.retire_at);
    // A failed attempt is evidence *against* mastery, so it discards what was
    // accumulated rather than adding nothing to it. Only a direct attempt can
    // demote: credit arriving over an `encompasses` edge is someone else's
    // verdict, and must never cost this node the confidence it earned.
    let carried = if attempted && gain < cfg.lapse_at {
        0.0
    } else {
        decayed_confidence(&previous, now_days, cfg)
    };
    let confidence = (carried + gain).clamp(0.0, ceiling);
    let entry = fluencies.entry(node.to_string()).or_default();
    entry.confidence = finite_nonnegative(confidence);
    entry.last_practiced = Some(now_days);
    if attempted {
        entry.attempts = entry.attempts.saturating_add(1);
        entry.best_score = finite_nonnegative(previous.best_score).max(gain);
    } else if !entry.best_score.is_finite() || entry.best_score < 0.0 {
        entry.best_score = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, EdgeType, Goal, Kind, Node, Provenance, State};

    fn node(id: &str) -> Node {
        Node {
            id: id.to_string(),
            title: id.to_string(),
            kind: Kind::Skill,
            probe: format!("perform {id}"),
            goals: Vec::new(),
            cost_minutes: 5,
            relevance: 1.0,
            provenance: Provenance::User,
            gradable: true,
        }
    }

    fn edge(from: &str, to: &str, ty: EdgeType) -> Edge {
        Edge {
            from: from.to_string(),
            to: to.to_string(),
            ty,
            strength: 1.0,
            reason: format!("{from} -> {to}"),
            needs_goals: Vec::new(),
            provenance: Provenance::User,
            confidence: 1.0,
        }
    }

    fn graph(ids: &[&str], edges: Vec<Edge>) -> Graph {
        Graph {
            goal: Goal {
                id: "goal".to_string(),
                target: "target".to_string(),
                deadline: None,
                budget_hours: 1,
            },
            nodes: ids.iter().map(|id| node(id)).collect(),
            edges,
            state: State::default(),
        }
    }

    fn fluency(confidence: f32, last_practiced: Option<i64>) -> Fluency {
        Fluency {
            confidence,
            last_practiced,
            ..Fluency::default()
        }
    }

    fn config() -> SchedConfig {
        SchedConfig {
            target: 1.0,
            half_life_days: 10.0,
            retire_at: 1.5,
            session_size: 5,
            encompass_credit: 0.5,
            lapse_at: 0.5,
        }
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1.0e-6,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn decay_obeys_elapsed_time_and_never_practised_rules() {
        let cfg = config();
        assert_close(decayed_confidence(&fluency(0.8, Some(10)), 20, &cfg), 0.4);
        assert_close(decayed_confidence(&fluency(0.8, Some(10)), 10, &cfg), 0.8);
        assert_close(decayed_confidence(&fluency(0.8, None), 100, &cfg), 0.8);
        assert_close(decayed_confidence(&fluency(0.8, Some(10)), 9, &cfg), 0.8);
    }

    #[test]
    fn invalid_half_lives_disable_decay() {
        for half_life_days in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let cfg = SchedConfig {
                half_life_days,
                ..config()
            };
            assert_close(decayed_confidence(&fluency(0.8, Some(0)), 100, &cfg), 0.8);
        }
    }

    #[test]
    fn unlocking_uses_decayed_prerequisite_confidence() {
        let graph = graph(
            &["root", "dependent"],
            vec![edge("root", "dependent", EdgeType::Requires)],
        );
        let mut fluencies = Fluencies::new();

        assert!(is_unlocked(&graph, &fluencies, "root", 10, &config()));
        assert!(!is_unlocked(&graph, &fluencies, "dependent", 10, &config()));

        fluencies.insert("root".into(), fluency(1.0, Some(0)));
        assert!(is_unlocked(&graph, &fluencies, "dependent", 0, &config()));
        assert!(!is_unlocked(&graph, &fluencies, "dependent", 10, &config()));
        assert!(!is_unlocked(&graph, &fluencies, "missing", 0, &config()));
    }

    #[test]
    fn practicable_excludes_target_boundary_and_retired_concepts() {
        let graph = graph(&["learning", "target", "retired"], Vec::new());
        let mut fluencies = Fluencies::new();
        fluencies.insert("learning".into(), fluency(0.99, Some(0)));
        fluencies.insert("target".into(), fluency(1.0, Some(0)));
        fluencies.insert("retired".into(), fluency(1.5, Some(0)));
        fluencies.insert("not-in-graph".into(), fluency(0.0, None));

        assert_eq!(
            practisable(&graph, &fluencies, 0, &config()),
            vec!["learning"]
        );
    }

    #[test]
    fn focus_chooses_weakest_then_node_id_and_handles_empty_graph() {
        let g = graph(&["z", "b", "a"], Vec::new());
        let mut fluencies = Fluencies::new();
        fluencies.insert("z".into(), fluency(0.4, None));
        fluencies.insert("b".into(), fluency(0.2, None));
        fluencies.insert("a".into(), fluency(0.2, None));

        assert_eq!(focus(&g, &fluencies, 0, &config()), Some("a".into()));
        let empty = graph(&[], Vec::new());
        assert_eq!(focus(&empty, &Fluencies::new(), 0, &config()), None);
    }

    #[test]
    fn focus_where_skips_nodes_the_caller_cannot_use() {
        // The weakest node overall is not always one the caller can build for, and
        // skipping it must not change how the remaining candidates are ordered.
        let mut g = graph(&["a", "b", "c"], Vec::new());
        g.nodes[0].kind = Kind::Fact;
        let mut fluencies = Fluencies::new();
        fluencies.insert("a".into(), fluency(0.1, None));
        fluencies.insert("b".into(), fluency(0.2, None));
        fluencies.insert("c".into(), fluency(0.3, None));

        assert_eq!(focus(&g, &fluencies, 0, &config()), Some("a".into()));
        assert_eq!(
            focus_where(&g, &fluencies, 0, &config(), |n| n.kind != Kind::Fact),
            Some("b".into()),
            "the filter was ignored, or it reordered the survivors"
        );
        assert_eq!(
            focus_where(&g, &fluencies, 0, &config(), |_| false),
            None,
            "a predicate matching nothing still returned a node"
        );
    }

    #[test]
    fn session_round_robins_from_weakest_concept() {
        let graph = graph(&["middle", "strong", "weak"], Vec::new());
        let mut fluencies = Fluencies::new();
        fluencies.insert("weak".into(), fluency(0.1, None));
        fluencies.insert("middle".into(), fluency(0.2, None));
        fluencies.insert("strong".into(), fluency(0.3, None));
        let cfg = SchedConfig {
            session_size: 6,
            ..config()
        };

        let session = compose_session(&graph, &fluencies, 0, &cfg);
        assert_eq!(
            session,
            vec!["weak", "middle", "strong", "weak", "middle", "strong"]
        );
        assert!(session.windows(2).all(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn session_repeats_a_single_concept_and_honours_zero_size() {
        let graph = graph(&["only"], Vec::new());
        let cfg = SchedConfig {
            session_size: 3,
            ..config()
        };
        assert_eq!(
            compose_session(&graph, &Fluencies::new(), 0, &cfg),
            vec!["only", "only", "only"]
        );

        let zero = SchedConfig {
            session_size: 0,
            ..cfg
        };
        assert!(compose_session(&graph, &Fluencies::new(), 0, &zero).is_empty());
    }

    #[test]
    fn direct_attempt_decays_then_advances_and_updates_attempt_metadata() {
        let graph = graph(&["skill"], Vec::new());
        let mut fluencies = Fluencies::from([(
            "skill".into(),
            Fluency {
                best_score: 0.8,
                last_practiced: Some(0),
                attempts: 2,
                confidence: 1.0,
            },
        )]);

        let credited = record_attempt(&graph, &mut fluencies, "skill", 0.6, 10, &config());
        let updated = &fluencies["skill"];
        assert!(credited.is_empty());
        assert_close(updated.confidence, 1.1);
        assert_close(updated.best_score, 0.8);
        assert_eq!(updated.attempts, 3);
        assert_eq!(updated.last_practiced, Some(10));

        record_attempt(&graph, &mut fluencies, "skill", 1.0, 10, &config());
        assert_close(fluencies["skill"].confidence, config().retire_at);
        assert_close(fluencies["skill"].best_score, 1.0);
    }

    #[test]
    fn a_failed_attempt_demotes_a_retired_concept_and_reschedules_it() {
        let graph = graph(&["skill"], Vec::new());
        let mut fluencies = Fluencies::from([(
            "skill".into(),
            Fluency {
                best_score: 1.0,
                last_practiced: Some(10),
                attempts: 4,
                confidence: config().retire_at,
            },
        )]);
        assert!(!practisable(&graph, &fluencies, 10, &config()).contains(&"skill".to_string()));

        // Same day, so no decay: the demotion is the verdict talking, not time.
        record_attempt(&graph, &mut fluencies, "skill", 0.0, 10, &config());
        let updated = &fluencies["skill"];
        assert_close(updated.confidence, 0.0);
        // History survives. What they once managed remains true, and the attempt counts.
        assert_close(updated.best_score, 1.0);
        assert_eq!(updated.attempts, 5);
        // The point of all of it: a concept you just failed comes back.
        assert!(practisable(&graph, &fluencies, 10, &config()).contains(&"skill".to_string()));
    }

    #[test]
    fn a_failed_attempt_never_demotes_an_encompassed_neighbour() {
        // `easier` is credited whenever `harder` is practised. A bad attempt on
        // `harder` is a verdict on `harder` alone: it must not cost `easier` the
        // confidence it earned on its own.
        let graph = graph(
            &["easier", "harder"],
            vec![edge("easier", "harder", EdgeType::Encompasses)],
        );
        let mut fluencies = Fluencies::from([
            ("easier".into(), fluency(1.2, Some(10))),
            ("harder".into(), fluency(1.2, Some(10))),
        ]);

        record_attempt(&graph, &mut fluencies, "harder", 0.0, 10, &config());
        assert_close(fluencies["harder"].confidence, 0.0);
        assert_close(fluencies["easier"].confidence, 1.2);
    }

    #[test]
    fn encompass_credit_flows_harder_to_easier_and_attenuates_per_hop() {
        // easier -> medium -> harder: attempts on the `to` end flow back toward `from`.
        let graph = graph(
            &["easier", "medium", "harder"],
            vec![
                edge("easier", "medium", EdgeType::Encompasses),
                edge("medium", "harder", EdgeType::Encompasses),
            ],
        );
        let mut fluencies = Fluencies::new();

        let credited = record_attempt(&graph, &mut fluencies, "harder", 1.0, 7, &config());
        assert_eq!(credited, BTreeSet::from(["easier".into(), "medium".into()]));
        assert_close(fluencies["harder"].confidence, 1.0);
        assert_close(fluencies["medium"].confidence, 0.5);
        assert_close(fluencies["easier"].confidence, 0.25);
        assert_eq!(fluencies["harder"].attempts, 1);
        assert_eq!(fluencies["medium"].attempts, 0);
        assert_eq!(fluencies["easier"].attempts, 0);
        assert_eq!(fluencies["medium"].last_practiced, Some(7));
        assert_eq!(fluencies["easier"].last_practiced, Some(7));
    }

    #[test]
    fn encompass_cycle_credits_each_other_node_once_at_shortest_depth() {
        let graph = graph(
            &["a", "b", "c"],
            vec![
                edge("b", "a", EdgeType::Encompasses),
                edge("c", "b", EdgeType::Encompasses),
                edge("a", "c", EdgeType::Encompasses),
                edge("a", "a", EdgeType::Encompasses),
            ],
        );
        let mut fluencies = Fluencies::new();

        let credited = record_attempt(&graph, &mut fluencies, "a", 1.0, 0, &config());
        assert_eq!(credited, BTreeSet::from(["b".into(), "c".into()]));
        assert_close(fluencies["a"].confidence, 1.0);
        assert_close(fluencies["b"].confidence, 0.5);
        assert_close(fluencies["c"].confidence, 0.25);
        assert_eq!(fluencies["a"].attempts, 1);
    }

    #[test]
    fn unknown_attempt_and_stray_fluencies_are_untouched() {
        let graph = graph(&["real"], Vec::new());
        let original = Fluencies::from([
            ("real".into(), fluency(0.25, Some(3))),
            ("stray".into(), fluency(0.75, Some(4))),
        ]);
        let mut fluencies = original.clone();

        assert!(record_attempt(&graph, &mut fluencies, "missing", 0.8, 9, &config()).is_empty());
        assert_eq!(fluencies, original);
        assert_eq!(practisable(&graph, &fluencies, 9, &config()), vec!["real"]);
    }

    #[test]
    fn non_finite_inputs_never_create_non_finite_confidence() {
        let graph = graph(&["a", "b"], vec![edge("b", "a", EdgeType::Encompasses)]);
        let mut fluencies = Fluencies::from([
            ("a".into(), fluency(f32::NAN, Some(0))),
            ("b".into(), fluency(f32::INFINITY, Some(0))),
        ]);

        assert!(decayed_confidence(&fluencies["a"], 10, &config()).is_finite());
        assert!(record_attempt(&graph, &mut fluencies, "a", f32::NAN, 10, &config()).is_empty());
        assert!(fluencies["a"].confidence.is_nan());
        assert_eq!(fluencies["a"].attempts, 0);
        assert_eq!(fluencies["a"].last_practiced, Some(0));
        assert_eq!(fluencies["b"].confidence, f32::INFINITY);
        assert_eq!(fluencies["b"].attempts, 0);

        record_attempt(&graph, &mut fluencies, "a", 0.5, 10, &config());
        assert!(fluencies.values().all(|f| f.confidence.is_finite()));
    }
}
