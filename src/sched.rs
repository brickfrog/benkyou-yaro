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

/// Everything remembered about practice on one concept.
///
/// `mastery` is *evidence*, and evidence does not expire. What elapsed time changes
/// is whether a fresh check is owed, which is [`due_in`] — a separate question with a
/// separate answer. Collapsing the two was the original design error here: one number
/// stood for historical mastery, current retention, prerequisite readiness, ordering
/// priority and retirement at once, and decaying it to serve the second meaning
/// silently destroyed the first. A learner who proved something last month has still
/// proved it; they may simply owe a re-check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fluency {
    /// Best graded score ever achieved, 0.0..=1.0.
    pub best_score: f32,
    /// Whole days since the epoch, or `None` if never practised.
    pub last_practiced: Option<i64>,
    /// Direct attempts only. Credit arriving over an `encompasses` edge does not
    /// increment this, which is what makes it usable as "has this ever been proven".
    pub attempts: u32,
    /// Accumulated evidence of mastery, `0.0..=cfg.mastery_ceiling`. Never reduced by
    /// elapsed time; reduced only by a direct attempt that goes badly.
    #[serde(alias = "confidence")]
    pub mastery: f32,
}

impl Default for Fluency {
    fn default() -> Self {
        Self {
            best_score: 0.0,
            last_practiced: None,
            attempts: 0,
            mastery: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedConfig {
    /// Mastery at or above which a concept counts as proven, and so stops blocking
    /// its dependents.
    pub target: f32,
    /// Days a concept at exactly `target` stays settled before a re-check is owed.
    /// Scaled by evidence in [`due_in`], so more mastery buys a longer interval.
    /// Session-level spacing, not item-level intervals.
    #[serde(alias = "half_life_days")]
    pub review_after_days: f32,
    /// Ceiling on accumulated mastery. Reaching it does not retire a concept - it
    /// buys the longest review interval, which is what an asymptote on the
    /// acquisition curve actually licenses. Heathcote et al. found individual
    /// learning curves fit exponentials with a hard asymptote; that is a claim about
    /// how fast performance stops improving, not a promise that it never decays, so
    /// it cannot justify never checking again.
    #[serde(alias = "retire_at")]
    pub mastery_ceiling: f32,
    /// How many items one session holds.
    pub session_size: usize,
    /// Fraction of a direct attempt's gain that flows to a concept reached through
    /// an `encompasses` edge.
    pub encompass_credit: f32,
    /// Score below which a direct attempt costs previously accumulated mastery.
    /// The penalty is proportional rather than total - see [`apply_credit`].
    pub lapse_at: f32,
}

impl Default for SchedConfig {
    fn default() -> Self {
        Self {
            target: 1.0,
            review_after_days: 21.0,
            mastery_ceiling: 1.5,
            session_size: 5,
            encompass_credit: 0.5,
            lapse_at: 0.5,
        }
    }
}

pub type Fluencies = BTreeMap<NodeId, Fluency>;

/// Days until this concept owes a fresh check. Negative means overdue by that many.
///
/// The interval is `review_after_days * (mastery / target)`, so evidence buys time:
/// a concept at the ceiling waits half again as long as one that just reached target.
/// A concept never practised is due now, and so is one whose mastery is below target -
/// there is nothing to wait for when the work is not done.
///
/// A non-positive or non-finite `review_after_days` means no concept is ever due on
/// time alone, which is the honest reading of "spacing turned off".
pub fn due_in(f: &Fluency, now_days: i64, cfg: &SchedConfig) -> i64 {
    let Some(last_practiced) = f.last_practiced else {
        return 0;
    };
    if !cfg.review_after_days.is_finite() || cfg.review_after_days <= 0.0 {
        return i64::MAX;
    }
    let target = if cfg.target.is_finite() && cfg.target > 0.0 {
        f64::from(cfg.target)
    } else {
        return 0;
    };
    let evidence = f64::from(finite_nonnegative(f.mastery)) / target;
    let interval = (f64::from(cfg.review_after_days) * evidence).round();
    // Saturating: an absurd config must not wrap the day counter into the past.
    let interval = interval.clamp(0.0, i64::MAX as f64) as i64;
    last_practiced
        .saturating_add(interval)
        .saturating_sub(now_days)
}

/// Whether a fresh check is owed. Evidence being stale is not evidence being absent:
/// this changes what to schedule, never what has been proven.
pub fn is_due(f: &Fluency, now_days: i64, cfg: &SchedConfig) -> bool {
    due_in(f, now_days, cfg) <= 0
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

/// True when every `requires`-predecessor of `node` is *proven*: mastery at or above
/// `cfg.target`, reached by at least one direct attempt. A node with no prerequisites
/// is always unlocked. A node absent from the graph is never unlocked.
///
/// Two properties this has to hold, both learned the hard way:
///
/// **Admission is monotonic in time.** It reads undecayed mastery, so a prerequisite
/// proven today is still proven tomorrow. Gating on a decaying value meant a single
/// perfect pass landed at exactly `target` and fell under it within one day, closing
/// every dependent overnight. A one-day gate flicker is not a spacing policy.
///
/// **Credit is not proof.** An `encompasses` edge is an assertion about the graph,
/// not a check that ran, and it can carry a node to `target` with zero attempts. Such
/// a node stays practisable (see [`practisable`]) rather than unlocking others on
/// evidence nobody produced.
pub fn is_unlocked(graph: &Graph, fluencies: &Fluencies, node: &str, cfg: &SchedConfig) -> bool {
    if !graph.nodes.iter().any(|candidate| candidate.id == node) {
        return false;
    }

    graph
        .edges
        .iter()
        .filter(|edge| edge.ty == crate::graph::EdgeType::Requires && edge.to == node)
        .all(|edge| {
            graph.nodes.iter().any(|candidate| candidate.id == edge.from) && {
                let f = fluency_for(fluencies, &edge.from);
                f.attempts > 0 && finite_nonnegative(f.mastery) >= cfg.target
            }
        })
}

/// Why a concept is in the session. Ordered: the reasons above come first, and the
/// ordering is a stated policy rather than a number that happens to sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// Not yet at target. Weakest first, mastery in thousandths so the tuple is `Ord`
    /// without imposing an order on floats.
    BelowTarget(i32),
    /// At target on encompass credit alone, never directly attempted. One attempt
    /// settles whether the graph was telling the truth.
    Unproven(i32),
    /// Proven, and the review interval has elapsed. `due_in` is negative once
    /// overdue, so the raw value already sorts the most overdue first — wrapping it
    /// in `Reverse` would put the concept you neglected longest at the back.
    DueForCheck(i64),
}

/// Why this concept is worth practising now, or `None` if it is settled.
pub fn reason(f: &Fluency, now_days: i64, cfg: &SchedConfig) -> Option<Reason> {
    let mastery = finite_nonnegative(f.mastery);
    let thousandths = (f64::from(mastery) * 1000.0).clamp(0.0, f64::from(i32::MAX)) as i32;
    if mastery < cfg.target {
        return Some(Reason::BelowTarget(thousandths));
    }
    if f.attempts == 0 {
        return Some(Reason::Unproven(thousandths));
    }
    if is_due(f, now_days, cfg) {
        return Some(Reason::DueForCheck(due_in(f, now_days, cfg)));
    }
    None
}

/// Every unlocked node with a live [`Reason`] — the set a session is drawn from.
/// Sorted by node id.
///
/// Reaching the mastery ceiling no longer removes a concept permanently. The ceiling
/// caps how much evidence one concept can bank, and evidence buys interval, so a
/// finished concept simply comes back rarely.
pub fn practisable(
    graph: &Graph,
    fluencies: &Fluencies,
    now_days: i64,
    cfg: &SchedConfig,
) -> Vec<NodeId> {
    let mut result: Vec<_> = graph
        .nodes
        .iter()
        .filter(|node| is_unlocked(graph, fluencies, &node.id, cfg))
        .filter(|node| reason(&fluency_for(fluencies, &node.id), now_days, cfg).is_some())
        .map(|node| node.id.clone())
        .collect();
    result.sort();
    result.dedup();
    result
}

/// The order a session works through concepts: by [`Reason`], then node id.
fn urgency(fluencies: &Fluencies, node: &str, now_days: i64, cfg: &SchedConfig) -> Option<Reason> {
    reason(&fluency_for(fluencies, node), now_days, cfg)
}

/// The most urgent practisable concept: what new practice material should be
/// generated for. Ordered by [`Reason`], ties on node id ascending. `None` when
/// nothing is practisable.
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
            urgency(fluencies, a, now_days, cfg)
                .cmp(&urgency(fluencies, b, now_days, cfg))
                .then_with(|| a.cmp(b))
        })
}

/// Compose one session, interleaved across concepts.
///
/// Interleaving rather than blocking is the one scheduling lever with strong
/// evidence behind it (Rohrer & Taylor). The contract is a round-robin, which is
/// the only rule that stays satisfiable when there are fewer practisable concepts
/// than session slots:
///
/// - order [`practisable`] by [`Reason`], ties on node id ascending — call that the
///   rotation
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
        urgency(fluencies, a, now_days, cfg)
            .cmp(&urgency(fluencies, b, now_days, cfg))
            .then_with(|| a.cmp(b))
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
/// `last_practiced` becomes `now_days`, and `mastery` gains `score`. Clamped to
/// `0.0..=cfg.mastery_ceiling`. Elapsed time is not subtracted first — evidence is
/// not spent by waiting, it is only made stale, which [`due_in`] answers instead.
///
/// Then the bridge. An `Edge { from, to, ty: Encompasses }` means mastery of `to`
/// grants practice credit for `from`, so `to` is the harder node and `from` the
/// easier one inside it. An attempt on `to` therefore credits `from`, and credit
/// keeps flowing because encompassing composes.
///
/// Credit **attenuates per hop**: a node at encompass-depth `d` from the attempted
/// node gains `score * cfg.encompass_credit.powi(d)`, and has `last_practiced` set.
/// With the default credit of 0.5 that is half for a direct encompass, a quarter two
/// hops out. `attempts` is **not** incremented for encompassed nodes; they were not
/// attempted, and until one of them is, that node cannot unlock anything.
///
/// This is what lets one exercise settle a pile of cards instead of competing with
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

/// Apply one gain to one node.
///
/// The lapse rule is **proportional, not a cliff**. A direct attempt scoring below
/// `cfg.lapse_at` keeps `mastery * (score / lapse_at)` of what was banked, so a
/// total failure erases everything, a near miss costs almost nothing, and the two
/// meet continuously at the threshold. The previous rule discarded the whole balance
/// the moment the score dipped under `lapse_at`: from a mastery of 0.99, scoring
/// 0.50 landed on 1.49 and scoring 0.49 landed on 0.49. A hundredth of a point
/// deciding a whole point of evidence is not a judgement anyone can defend,
/// especially when the exercises being scored are generated and of uneven difficulty.
///
/// Only a direct attempt can demote. Credit arriving over an `encompasses` edge is a
/// verdict on the node that was attempted, and must never cost this one what it
/// earned.
fn apply_credit(
    fluencies: &mut Fluencies,
    node: &str,
    gain: f32,
    now_days: i64,
    cfg: &SchedConfig,
    attempted: bool,
) {
    let previous = fluency_for(fluencies, node);
    let ceiling = finite_nonnegative(cfg.mastery_ceiling);
    let banked = finite_nonnegative(previous.mastery);
    let lapse_at = finite_nonnegative(cfg.lapse_at);
    let carried = if attempted && gain < lapse_at {
        // `lapse_at` of zero disables the rule rather than dividing by zero.
        if lapse_at > 0.0 {
            banked * (gain / lapse_at)
        } else {
            banked
        }
    } else {
        banked
    };
    let mastery = (carried + gain).clamp(0.0, ceiling);
    let entry = fluencies.entry(node.to_string()).or_default();
    entry.mastery = finite_nonnegative(mastery);
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

    fn fluency(mastery: f32, last_practiced: Option<i64>) -> Fluency {
        Fluency {
            mastery,
            last_practiced,
            ..Fluency::default()
        }
    }

    fn config() -> SchedConfig {
        SchedConfig {
            target: 1.0,
            review_after_days: 10.0,
            mastery_ceiling: 1.5,
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

    /// A fluency with an attempt behind it, so `Unproven` does not mask what is
    /// being asserted.
    fn proven(mastery: f32, last_practiced: Option<i64>) -> Fluency {
        Fluency {
            mastery,
            last_practiced,
            attempts: 1,
            ..Fluency::default()
        }
    }

    #[test]
    fn the_review_interval_scales_with_evidence() {
        let cfg = config();
        // review_after_days 10, target 1.0: interval is 10 * mastery/target.
        assert_eq!(due_in(&proven(1.0, Some(0)), 0, &cfg), 10);
        assert_eq!(due_in(&proven(1.0, Some(0)), 10, &cfg), 0, "due exactly on time");
        assert_eq!(due_in(&proven(1.0, Some(0)), 13, &cfg), -3, "overdue by three");
        assert_eq!(
            due_in(&proven(1.5, Some(0)), 0, &cfg),
            15,
            "the ceiling buys half again as long, not permanent retirement"
        );
        assert_eq!(
            due_in(&proven(0.8, None), 100, &cfg),
            0,
            "never practised is due now"
        );
    }

    #[test]
    fn invalid_intervals_mean_nothing_is_due_on_time_alone() {
        for review_after_days in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let cfg = SchedConfig {
                review_after_days,
                ..config()
            };
            assert!(
                !is_due(&proven(0.8, Some(0)), 100_000, &cfg),
                "spacing off must mean never due, not always due"
            );
        }
    }

    #[test]
    fn unlocking_reads_undecayed_mastery_and_requires_an_attempt() {
        let graph = graph(
            &["root", "dependent"],
            vec![edge("root", "dependent", EdgeType::Requires)],
        );
        let mut fluencies = Fluencies::new();

        assert!(is_unlocked(&graph, &fluencies, "root", &config()));
        assert!(!is_unlocked(&graph, &fluencies, "dependent", &config()));

        // At target but never attempted: credited, not proven.
        fluencies.insert("root".into(), fluency(1.0, Some(0)));
        assert!(!is_unlocked(&graph, &fluencies, "dependent", &config()));

        fluencies.insert("root".into(), proven(1.0, Some(0)));
        assert!(is_unlocked(&graph, &fluencies, "dependent", &config()));
        assert!(
            is_unlocked(&graph, &fluencies, "dependent", &config()),
            "and it stays open however long it has been"
        );
        assert!(!is_unlocked(&graph, &fluencies, "missing", &config()));
    }

    #[test]
    fn practisable_covers_below_target_unproven_and_due() {
        let graph = graph(
            &["learning", "settled", "credited", "stale"],
            Vec::new(),
        );
        let mut fluencies = Fluencies::new();
        fluencies.insert("learning".into(), proven(0.99, Some(0)));
        fluencies.insert("settled".into(), proven(1.0, Some(0)));
        fluencies.insert("credited".into(), fluency(1.2, Some(0)));
        fluencies.insert("stale".into(), proven(1.0, Some(-40)));
        fluencies.insert("not-in-graph".into(), fluency(0.0, None));

        assert_eq!(
            practisable(&graph, &fluencies, 0, &config()),
            vec!["credited", "learning", "stale"],
            "settled is at target, proven, and not yet due"
        );
    }

    #[test]
    fn reason_orders_deficit_before_unproven_before_due() {
        let cfg = config();
        let below = reason(&proven(0.5, Some(0)), 0, &cfg).unwrap();
        let unproven = reason(&fluency(1.2, Some(0)), 0, &cfg).unwrap();
        let due = reason(&proven(1.0, Some(-40)), 0, &cfg).unwrap();
        assert!(below < unproven, "unfinished work outranks an unproven claim");
        assert!(unproven < due, "an unproven claim outranks a routine re-check");
        assert!(
            reason(&proven(1.0, Some(0)), 0, &cfg).is_none(),
            "settled concepts are not scheduled"
        );
    }

    #[test]
    fn the_longest_neglected_concept_is_scheduled_first() {
        let cfg = config();
        let mildly = reason(&proven(1.0, Some(-11)), 0, &cfg).unwrap();
        let badly = reason(&proven(1.0, Some(-40)), 0, &cfg).unwrap();
        assert!(
            badly < mildly,
            "30 days overdue must outrank 1 day overdue, got {badly:?} vs {mildly:?}"
        );

        let graph = graph(&["mild", "bad"], Vec::new());
        let fluencies = Fluencies::from([
            ("mild".into(), proven(1.0, Some(-11))),
            ("bad".into(), proven(1.0, Some(-40))),
        ]);
        assert_eq!(
            compose_session(&graph, &fluencies, 0, &SchedConfig { session_size: 2, ..cfg })[0],
            "bad"
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
    fn a_direct_attempt_advances_without_charging_for_elapsed_time() {
        let graph = graph(&["skill"], Vec::new());
        let mut fluencies = Fluencies::from([(
            "skill".into(),
            Fluency {
                best_score: 0.8,
                last_practiced: Some(0),
                attempts: 2,
                mastery: 0.6,
            },
        )]);

        // Ten days on: under the old rule the banked 0.6 was halved before the gain
        // was added, so waiting cost evidence. It no longer does.
        let credited = record_attempt(&graph, &mut fluencies, "skill", 0.6, 10, &config());
        let updated = &fluencies["skill"];
        assert!(credited.is_empty());
        assert_close(updated.mastery, 1.2);
        assert_close(updated.best_score, 0.8);
        assert_eq!(updated.attempts, 3);
        assert_eq!(updated.last_practiced, Some(10));

        record_attempt(&graph, &mut fluencies, "skill", 1.0, 10, &config());
        assert_close(fluencies["skill"].mastery, config().mastery_ceiling);
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
                mastery: config().mastery_ceiling,
            },
        )]);
        assert!(!practisable(&graph, &fluencies, 10, &config()).contains(&"skill".to_string()));

        // Same day, so no decay: the demotion is the verdict talking, not time.
        record_attempt(&graph, &mut fluencies, "skill", 0.0, 10, &config());
        let updated = &fluencies["skill"];
        assert_close(updated.mastery, 0.0);
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
        // mastery it earned on its own.
        let graph = graph(
            &["easier", "harder"],
            vec![edge("easier", "harder", EdgeType::Encompasses)],
        );
        let mut fluencies = Fluencies::from([
            ("easier".into(), fluency(1.2, Some(10))),
            ("harder".into(), fluency(1.2, Some(10))),
        ]);

        record_attempt(&graph, &mut fluencies, "harder", 0.0, 10, &config());
        assert_close(fluencies["harder"].mastery, 0.0);
        assert_close(fluencies["easier"].mastery, 1.2);
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
        assert_close(fluencies["harder"].mastery, 1.0);
        assert_close(fluencies["medium"].mastery, 0.5);
        assert_close(fluencies["easier"].mastery, 0.25);
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
        assert_close(fluencies["a"].mastery, 1.0);
        assert_close(fluencies["b"].mastery, 0.5);
        assert_close(fluencies["c"].mastery, 0.25);
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
    fn non_finite_inputs_never_create_non_finite_output() {
        let graph = graph(&["a", "b"], vec![edge("b", "a", EdgeType::Encompasses)]);
        let mut fluencies = Fluencies::from([
            ("a".into(), fluency(f32::NAN, Some(0))),
            ("b".into(), fluency(f32::INFINITY, Some(0))),
        ]);

        // The stored value is deliberately garbage; every reader must still be sane.
        assert!(due_in(&fluencies["a"], 10, &config()) > i64::MIN);
        assert!(reason(&fluencies["a"], 10, &config()).is_some());
        assert!(record_attempt(&graph, &mut fluencies, "a", f32::NAN, 10, &config()).is_empty());
        assert!(fluencies["a"].mastery.is_nan());
        assert_eq!(fluencies["a"].attempts, 0);
        assert_eq!(fluencies["a"].last_practiced, Some(0));
        assert_eq!(fluencies["b"].mastery, f32::INFINITY);
        assert_eq!(fluencies["b"].attempts, 0);

        record_attempt(&graph, &mut fluencies, "a", 0.5, 10, &config());
        assert!(fluencies.values().all(|f| f.mastery.is_finite()));
    }
}
