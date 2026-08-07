//! The CLI an agent drives.
//!
//! This binary makes no network calls and holds no API key. It emits structured
//! *generation orders*; the agent already in a conversation fills them with the model
//! the user is already paying for and writes the result back. See DESIGN.md §6.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use benkyou::assess::{self, AssessConfig, RecordOutcome, Step};
use benkyou::exercise::{self, Gate, GateOutcome, Task};
use benkyou::gate::run_gate;
use benkyou::graph::{
    Edge, EdgeType, Goal, Graph, Kind, Node, Provenance, State, Verdict, NODE_CAP, RELEVANCE_FLOOR,
};
use benkyou::order::{self, OrderKind};
use benkyou::sched::{self, SchedConfig};
use benkyou::store;

const USAGE: &str = "\
benkyou — learn a new domain quickly

USAGE
  benkyou --version
      The binary's version. The skill that drives it ships separately, so this is
      how you check they are the same generation.

  benkyou goals
      List stored goals and how far into each one you are, by assessment and by
      practice. Creates the goals directory if absent, so this is also the way to
      find out where to write a new graph.

  benkyou schema
      Print a complete, valid goal file — every field a graph needs, with one edge
      of each type — so a new graph is written from a worked example rather than
      reverse-engineered from parse errors. Redirect it into the goals directory
      `goals` names and replace the content.

  benkyou validate <goal>
      Repair a generated graph in place and report every change. A graph that
      needed heavy repair is a graph to regenerate.

  benkyou seed <goal> [--known a,b,c] [--unknown x,y] [--prior a=0.8,b=0.3]
      Give the interview your own account of your background. The two directions are
      not symmetric. `--known` only primes a strong belief: claiming to know a thing
      is a prior, never evidence, so nothing enters `known` without a graded probe —
      it makes the node worth asking about *sooner*, because passing it discharges
      everything beneath it. `--unknown` resolves outright, taking everything that
      depends on it, because nobody claims not to know a thing they can do.
      `--prior id=p` sets a belief by hand. Neither spends the question budget, and
      a claim your own admissions contradict is reported rather than hidden. An
      `--unknown` that pulls a graded node back out of `known` — which it must, since
      `known` is prerequisite-closed — lists it under `retracted`: that is earned
      evidence being discarded, so check it rather than skim past it.

  benkyou ask <goal> [--node <id>]
      Emit the next assessment question as JSON, or the reason to stop. With --node,
      emit that node's question instead of the highest-gain one.

  benkyou record <goal> <node> <pass|partial|fail|skip>
      Apply a graded verdict and print what it resolved. `reask` in the output
      means the verdict contradicts earlier evidence: ask again with a different
      instance before accepting it. Use `skip` when the probe itself was the
      problem — leading, ambiguous, or answerable from its own phrasing. It
      resolves nothing, spends the question, and marks the probe for rewriting.

  benkyou fringe <goal>
      What the learner can already do, and what they are ready to start.

  benkyou plan <goal> <target> [--budget-mins N]
      Study order for everything still needed to reach <target>.

  benkyou session <goal> [--size N]
      Compose one practice session of N entries, weakest concept first and no two
      neighbours alike. With only one concept practisable there is nothing to
      interleave, so the session is that concept repeated.

  benkyou practice <goal> <node> <score 0.0-1.0>
      Record a graded attempt and propagate credit along encompasses edges, so one
      exercise can discharge the concepts it exercises.

  benkyou order <goal> --kind <cards|exercise|probe> [--node ID] [--count N]
      Emit a generation order: what to write, for which node, with which
      prerequisites assumable and which dependents left unspoiled. This is the half
      the binary cannot do itself — it holds the state and names the work; the agent
      in the conversation writes the artifact and submits it back. Without --node the
      target is chosen from the schedule: the weakest unlocked concept still below
      target, skipping kinds that cannot carry an exercise when the order is one. It
      cannot tell whether a grader can actually judge a `skill` node - that call is
      yours.

  benkyou cards <cards.json> [--deck NAME] [--push]
      Build Anki notes from generated cards. Prints them and stops; pass --push
      to write them. Note identity comes from concept + role, never from the card
      text, so a regenerated card updates in place and keeps its review history.

  benkyou gate <exercise-dir> [--scratch DIR]
      Run the validation gate: the reference solution must pass and the untouched
      starting state must fail. Records the result in task.toml, which is what
      makes the exercise showable. Exits non-zero if the exercise is rejected.

  benkyou attempt <exercise-dir> [--work <dir>]
      Materialise a workspace and sit down to the exercise. Refuses anything the
      gate has not validated, and copies only setup/ — never the solution.

  benkyou grade <exercise-dir> [--work <dir>] [--goal <goal>]
      Run the exercise's own grader against your workspace. With --goal the score
      is recorded as practice fluency for the task's concept, so doing the kata is
      what advances the schedule.

FILES
  A <goal> is a bare name — `ramp` — resolving to a stored goal under
  $XDG_DATA_HOME/benkyou/goals (default ~/.local/share/benkyou/goals), beside the
  fluency file it must not be separated from. An argument holding a `/` or ending in
  `.json` is used as a path exactly as typed, so a goal checked into a repo works.

  Workspaces go under $XDG_STATE_HOME/benkyou/exercises/<concept>/<exercise>/work
  (default ~/.local/state/benkyou) — state, not data: scratch for one sitting, rebuildable
  from the exercise, and graded out into fluency. `attempt` and `grade` derive the same
  path from the task, so they cannot be pointed at different directories by accident.
  Pass --work to override.

Every command prints JSON on success unless stated otherwise.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("benkyou: {e}");
            ExitCode::FAILURE
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn positional(args: &[String]) -> Vec<&String> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a.starts_with("--") {
            skip = true;
            continue;
        }
        out.push(a);
    }
    out
}

/// The workspace for an exercise. Derived from the task rather than asked for, so
/// `attempt` and `grade` always agree; `--work` overrides for anyone who wants their
/// own layout.
fn work_root(args: &[String], dir: &Path, task: &Task) -> Result<PathBuf, String> {
    if let Some(explicit) = flag(args, "--work") {
        return Ok(PathBuf::from(explicit));
    }
    let slug = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{}: no directory name to work under", dir.display()))?;
    store::work_root(&task.task.concept_id, &slug)
}

fn run(args: &[String]) -> Result<String, String> {
    // `--help` is answered from anywhere in the argument list, ahead of every other
    // reading of it. Asking a subcommand for help is the first thing anyone does, and
    // `positional` would otherwise hand `--help` to the parser as the <goal> — or eat
    // the argument after it — and answer with a missing-argument error.
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(USAGE.to_string());
    }
    // Version travels the same way, and for the same reason it is worth having at all:
    // the skill that drives this binary installs from a different URL than the binary
    // does, so the two can drift. `--version` is how an agent checks the tool in front
    // of it is the one its instructions describe.
    if args.iter().any(|a| a == "-V" || a == "--version") {
        return Ok(format!("benkyou {}", env!("CARGO_PKG_VERSION")));
    }
    let Some(cmd) = args.first().map(String::as_str) else {
        return Ok(USAGE.to_string());
    };
    // Bare `help` stays a first-position verb: unlike the dashed forms it is a
    // plausible goal name, node id, or grader argument further along the line.
    if cmd == "help" {
        return Ok(USAGE.to_string());
    }

    let pos = positional(&args[1..]);
    let need = |i: usize, what: &str| -> Result<PathBuf, String> {
        pos.get(i)
            .map(PathBuf::from)
            .ok_or_else(|| format!("{cmd}: missing <{what}>"))
    };
    // A goal argument is a stored name unless it looks like a path. Resolved in one
    // place so no command can disagree with another about where goals live.
    let need_goal = |i: usize| -> Result<PathBuf, String> {
        let arg = pos
            .get(i)
            .ok_or_else(|| format!("{cmd}: missing <goal>"))?
            .as_str();
        store::goal_path(arg).map_err(|e| format!("{cmd}: {e}"))
    };

    match cmd {
        "goals" => {
            // Create it. This is the verb that tells an agent where to write the first
            // graph, and reporting a directory that does not exist hands it an ENOENT
            // on the documented happy path.
            let dir = store::goals_dir()?;
            let mut goals = Vec::new();
            let cfg = SchedConfig::default();
            for name in store::list_goals()? {
                let path = store::goal_path(&name)?;
                // No unreadable file may hide every other goal — a half-written store is
                // exactly when you need the listing to work. Both files can rot
                // independently, so each is reported in place and neither aborts.
                let entry = match store::load_graph(&path) {
                    Err(e) => serde_json::json!({ "name": name, "unreadable": e }),
                    Ok(graph) => {
                        let mut entry = serde_json::json!({
                            "name": name,
                            "target": graph.goal.target,
                            "nodes": graph.nodes.len(),
                            "known": graph.state.known.len(),
                            "unknown": graph.state.unknown.len(),
                            "unresolved": graph.nodes.len()
                                - graph.state.known.len()
                                - graph.state.unknown.len(),
                        });
                        // Practice is a second, independently corruptible file. Losing it
                        // does not make the goal unreadable, so report the loss in place
                        // of the counts rather than in addition to them.
                        match store::load_fluencies(&store::fluency_path(&path)) {
                            Ok(f) => {
                                // Only a direct `grade` or `practice` increments
                                // `attempts`. An `encompasses` edge also opens a fluency
                                // record for the node underneath, at a credited
                                // confidence and zero attempts — counting those as
                                // practised tells the learner they drilled something they
                                // have never once sat down to, which is the same
                                // overstatement as counting a claim as knowledge.
                                entry["practised"] =
                                    f.values().filter(|x| x.attempts > 0).count().into();
                                entry["credited"] =
                                    f.values().filter(|x| x.attempts == 0).count().into();
                                entry["retired"] = f
                                    .values()
                                    .filter(|x| x.confidence >= cfg.retire_at)
                                    .count()
                                    .into();
                            }
                            Err(e) => entry["fluency_unreadable"] = e.into(),
                        }
                        entry
                    }
                };
                goals.push(entry);
            }
            json(&serde_json::json!({ "dir": dir, "goals": goals }))
        }

        "schema" => json(&example_graph()),

        "validate" => {
            let path = need_goal(0)?;
            let mut graph = store::load_graph(&path)?;
            let report = graph.validate(RELEVANCE_FLOOR, NODE_CAP);
            store::save_graph(&path, &graph)?;
            let cycles = report.cycles.len();
            // A repair the author might not have noticed is worth naming in the same
            // breath, because the file has already been rewritten by the time they read
            // this. "Nothing was cut" used to be printed unconditionally and was false
            // whenever a cycle sat alongside a sub-floor node or a dangling edge.
            let repaired = report.duplicate_nodes.len()
                + report.dropped_irrelevant.len()
                + report.dropped_over_cap.len()
                + report.dangling_edges.len();
            let body = json(&serde_json::json!({
                "clean": report.is_clean(),
                "nodes": graph.nodes.len(),
                "edges": graph.edges.len(),
                "report": report,
            }))?;
            // A cycle is not repairable here — the graph is unusable until the author
            // removes an edge, and succeeding quietly is how a wrong curriculum gets
            // studied. Same shape as `gate` on a rejected exercise: the full finding on
            // stdout, non-zero exit.
            if cycles > 0 {
                println!("{body}");
                let also = match repaired {
                    0 => String::new(),
                    n => format!(
                        " {n} unrelated repair(s) were applied and written; see the rest \
                         of the report."
                    ),
                };
                return Err(format!(
                    "{cycles} `requires` cycle(s): see report.cycles and remove one edge \
                     from each. No cycle edge was cut — which one is wrong is yours to \
                     say.{also}"
                ));
            }
            Ok(body)
        }

        "seed" => {
            let list = |name: &str| -> Vec<String> {
                flag(args, name)
                    .map(|v| {
                        v.split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let path = need_goal(0)?;
            let known = list("--known");
            let unknown = list("--unknown");

            let mut prior: Vec<(String, f32)> = Vec::new();
            if let Some(raw) = flag(args, "--prior") {
                for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let (id, p) = part.split_once('=').ok_or_else(|| {
                        format!("seed: --prior wants id=probability, got `{part}`")
                    })?;
                    let p: f32 = p
                        .trim()
                        .parse()
                        .map_err(|_| format!("seed: `{p}` is not a probability"))?;
                    prior.push((id.trim().to_string(), p));
                }
            }
            if known.is_empty() && unknown.is_empty() && prior.is_empty() {
                return Err(
                    "seed: nothing declared — pass --known, --unknown or --prior".to_string()
                );
            }

            let mut graph = store::load_graph(&path)?;
            let mut state = graph.state.clone();
            let report = assess::declare(&graph, &mut state, &known, &unknown, &store::now_iso());
            let mut missing = report.missing;
            missing.extend(assess::seed_prior(&graph, &mut state, &prior));
            missing.sort();
            missing.dedup();
            graph.state = state;
            store::save_graph(&path, &graph)?;

            json(&serde_json::json!({
                "known": graph.state.known.len(),
                "unknown": graph.state.unknown.len(),
                "remaining": graph.nodes.len()
                    - graph.state.known.len()
                    - graph.state.unknown.len(),
                "primed": report.primed,
                "retracted": report.retracted,
                "conflicts": report.conflicts,
                "missing": missing,
            }))
        }

        "ask" => {
            let path = need_goal(0)?;
            let graph = store::load_graph(&path)?;
            let asked = assess::asked(&graph.state);
            if let Some(node) = flag(args, "--node") {
                let n = graph
                    .node(&node)
                    .ok_or_else(|| format!("ask: no node `{node}` in this graph"))?;
                return json(&serde_json::json!({
                    "ask": n.id,
                    "probe": n.probe,
                    "gain": assess::gain(&graph, &graph.state, &node),
                    "goals": n.goals,
                    "asked_so_far": asked.len(),
                }));
            }
            match assess::next_step(&graph, &graph.state, &asked, &AssessConfig::default()) {
                Step::Ask(q) => json(&serde_json::json!({
                    "ask": q.node,
                    "probe": q.probe,
                    "gain": q.gain,
                    "goals": graph.node(&q.node).map(|n| n.goals.clone()).unwrap_or_default(),
                    "asked_so_far": asked.len(),
                })),
                Step::Stop(reason) => json(&serde_json::json!({
                    "stop": reason,
                    "asked": asked.len(),
                    "known": graph.state.known.len(),
                    "unknown": graph.state.unknown.len(),
                })),
            }
        }

        "record" => {
            let path = need_goal(0)?;
            let node = pos.get(1).ok_or("record: missing <node>")?.to_string();
            let verdict = match pos.get(2).map(|s| s.as_str()) {
                Some("pass") => Verdict::Pass,
                Some("partial") => Verdict::Partial,
                Some("fail") => Verdict::Fail,
                Some("skip") => Verdict::Skip,
                other => {
                    return Err(format!(
                        "record: verdict must be pass|partial|fail|skip, got {other:?}"
                    ))
                }
            };
            let mut graph = store::load_graph(&path)?;
            if !graph.contains(&node) {
                return Err(format!("record: no node `{node}` in this graph"));
            }
            let probe = graph.node(&node).map(|n| n.probe.clone()).unwrap_or_default();
            let before_known = graph.state.known.len();
            let before_unknown = graph.state.unknown.len();

            let mut state = graph.state.clone();
            let outcome = assess::record(&graph, &mut state, &node, verdict, &probe, &store::now_iso());
            graph.state = state;
            store::save_graph(&path, &graph)?;

            json(&serde_json::json!({
                "outcome": match outcome {
                    RecordOutcome::Applied => "applied",
                    RecordOutcome::ReAsk => "reask",
                },
                "resolved_known": graph.state.known.len() as i64 - before_known as i64,
                "resolved_unknown": graph.state.unknown.len() as i64 - before_unknown as i64,
                "known": graph.state.known.len(),
                "unknown": graph.state.unknown.len(),
                "remaining": graph.nodes.len() - graph.state.known.len() - graph.state.unknown.len(),
            }))
        }

        "fringe" => {
            let path = need_goal(0)?;
            let graph = store::load_graph(&path)?;
            json(&serde_json::json!({
                "can_do": graph.inner_fringe(&graph.state.known),
                "ready_for": graph.outer_fringe(&graph.state.known),
                "known": graph.state.known.len(),
                "total": graph.nodes.len(),
            }))
        }

        "plan" => {
            let path = need_goal(0)?;
            let target = pos.get(1).ok_or("plan: missing <target>")?.to_string();
            let graph = store::load_graph(&path)?;
            let budget = flag(args, "--budget-mins")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(graph.goal.budget_hours.saturating_mul(60));
            let plan = graph.plan(&target, &graph.state.known, budget);
            let minutes: u32 = plan
                .iter()
                .filter_map(|id| graph.node(id))
                .map(|n| n.cost_minutes)
                .sum();
            json(&serde_json::json!({
                "target": target,
                "budget_mins": budget,
                "minutes": minutes,
                "plan": plan,
            }))
        }

        "session" => {
            let path = need_goal(0)?;
            let graph = store::load_graph(&path)?;
            let fpath = store::fluency_path(&path);
            let fluencies = store::load_fluencies(&fpath)?;
            let mut cfg = SchedConfig::default();
            if let Some(n) = flag(args, "--size").and_then(|v| v.parse::<usize>().ok()) {
                cfg.session_size = n;
            }
            let today = store::today();
            json(&serde_json::json!({
                "session": sched::compose_session(&graph, &fluencies, today, &cfg),
                "focus": sched::focus(&graph, &fluencies, today, &cfg),
                "practisable": sched::practisable(&graph, &fluencies, today, &cfg),
            }))
        }

        "practice" => {
            let path = need_goal(0)?;
            let node = pos.get(1).ok_or("practice: missing <node>")?.to_string();
            let score: f32 = pos
                .get(2)
                .ok_or("practice: missing <score>")?
                .parse()
                .map_err(|e| format!("practice: score must be a number: {e}"))?;
            let graph = store::load_graph(&path)?;
            if !graph.contains(&node) {
                return Err(format!("practice: no node `{node}` in this graph"));
            }
            let fpath = store::fluency_path(&path);
            let mut fluencies = store::load_fluencies(&fpath)?;
            let cfg = SchedConfig::default();
            let credited =
                sched::record_attempt(&graph, &mut fluencies, &node, score, store::today(), &cfg);
            store::save_fluencies(&fpath, &fluencies)?;
            // A score you assigned yourself is the weakest evidence the tool accepts, and
            // for a node a grader could have judged it is the self-preference problem
            // this project exists to avoid. Not refused — a kata done on paper is a real
            // attempt — but never silent, because the guardrail otherwise lives only on
            // `order` and nothing would mark the difference afterwards.
            let self_scored = graph
                .node(&node)
                .filter(|n| n.gradable && order::is_practicable_kind(n.kind))
                .map(|_| {
                    format!(
                        "`{node}` is markable: a grader could have judged this. Prefer \
                         `order --kind exercise --node {node}` and let the kata score it."
                    )
                });
            json(&serde_json::json!({
                "node": node,
                "score": score,
                "confidence": fluencies.get(&node).map(|f| f.confidence),
                "also_credited": credited,
                "warning": self_scored,
            }))
        }

        "order" => {
            let path = need_goal(0)?;
            let kind = flag(args, "--kind")
                .ok_or_else(|| "order: missing --kind cards|exercise|probe".to_string())?;
            let kind = OrderKind::parse(&kind)?;
            let count: usize = match flag(args, "--count") {
                Some(c) => c
                    .parse()
                    .map_err(|_| format!("order: --count wants a number, got `{c}`"))?,
                None => 4,
            };
            let graph = store::load_graph(&path)?;

            // The node the tool picks is the whole point: the agent cannot see the
            // schedule, and the design's generation focus is the weakest unlocked
            // concept still below target.
            let node = match flag(args, "--node") {
                Some(n) => n,
                None => match kind {
                    OrderKind::Probe => order::probes_needing_rewrite(&graph)
                        .into_iter()
                        .next()
                        .ok_or_else(|| {
                            "order: no probe has been skipped, so none needs rewriting"
                                .to_string()
                        })?,
                    _ => {
                        let fluencies = store::load_fluencies(&store::fluency_path(&path))?;
                        let cfg = SchedConfig::default();
                        let want_runnable = kind == OrderKind::Exercise;
                        let today = store::today();
                        let chosen = sched::focus_where(&graph, &fluencies, today, &cfg, |n| {
                            // Both halves, or the scheduler hands back a node the very
                            // next step refuses: `kind` must be able to carry an
                            // exercise, and the author must not have declared that
                            // nothing can grade it.
                            !want_runnable || (order::is_practicable_kind(n.kind) && n.gradable)
                        });
                        match chosen {
                            Some(n) => n,
                            None => {
                                // Distinguish "nothing left to work on" from "the only
                                // thing left is hand-scored". Both used to print the
                                // former, which reads as "you are finished" at exactly
                                // the moment a ramp ends on the performance you still
                                // owe — and is simply false, since `session` still has a
                                // focus. Re-run the search without the gradability half
                                // and name what it finds.
                                let ungradable = want_runnable
                                    .then(|| {
                                        sched::focus_where(&graph, &fluencies, today, &cfg, |n| {
                                            order::is_practicable_kind(n.kind) && !n.gradable
                                        })
                                    })
                                    .flatten();
                                return Err(match ungradable {
                                    Some(n) => format!(
                                        "order: `{n}` is the weakest thing left and it is \
                                         marked not machine-gradable, so there is no \
                                         exercise to order. Score the performance with \
                                         `benkyou practice <goal> {n} <score>`, or write \
                                         cards for what sits under it with `--kind cards`."
                                    ),
                                    None => "order: nothing is practisable — every unlocked \
                                             concept is at target. Pass --node to override."
                                        .to_string(),
                                });
                            }
                        }
                    }
                },
            };
            json(&order::build(&graph, kind, &node, count)?)
        }

        "cards" => {
            let path = need(0, "cards.json")?;
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let cards: Vec<benkyou::anki::Card> = serde_json::from_str(&text)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let deck = flag(args, "--deck").unwrap_or_else(|| "benkyou".to_string());

            // Dry run builds every note and prints it, touching nothing. Default,
            // because the alternative writes into a collection the user has spent
            // years curating.
            if !args.iter().any(|a| a == "--push") {
                let notes: Vec<_> = cards.iter().map(|c| c.to_note(&deck)).collect();
                return json(&serde_json::json!({
                    "dry_run": true,
                    "deck": deck,
                    "notes": notes,
                    "hint": "re-run with --push to write these to Anki",
                }));
            }

            let anki = benkyou::anki::AnkiConnect::default();
            let version = anki.version()?;
            let created = anki.ensure_models()?;
            let report = anki.push(&cards, &deck)?;
            let failed = report.failed.len();
            let body = json(&serde_json::json!({
                "ankiconnect_version": version,
                "models_created": created,
                "deck": deck,
                "added": report.added,
                "updated": report.updated,
                "failed": report.failed,
            }))?;
            if failed > 0 {
                println!("{body}");
                return Err(format!("{failed} card(s) failed to push"));
            }
            Ok(body)
        }

        "gate" => {
            let dir = need(0, "exercise-dir")?;
            let scratch = flag(args, "--scratch")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
            let at = store::now_iso();
            let report = run_gate(&dir, &scratch, &at)?;
            let body = serde_json::json!({
                "outcome": report.outcome,
                "solution_verdict": report.solution.verdict,
                "empty_verdict": report.empty.verdict,
            });
            let text = json(&body)?;
            match report.outcome {
                GateOutcome::Validated(_) => {
                    // Persist it. An exercise is showable *because* the gate has run,
                    // and `attempt` has no other way to know that it did.
                    let mut task = exercise::load(&dir)?;
                    task.gate = Some(Gate {
                        solution_passes: true,
                        empty_fails: true,
                        validated_at: at,
                    });
                    let path = dir.join("task.toml");
                    let rewritten = toml::to_string_pretty(&task).map_err(|e| e.to_string())?;
                    std::fs::write(&path, rewritten)
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    Ok(text)
                }
                // A rejected exercise is a failure of the command, not a report:
                // the caller must not go on to show it to a learner.
                GateOutcome::Rejected(_) => {
                    println!("{text}");
                    Err("gate: exercise rejected — see outcome".into())
                }
            }
        }

        "attempt" => {
            let dir = need(0, "exercise-dir")?;
            let task = exercise::load(&dir)?;
            let root = work_root(args, &dir, &task)?;
            std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
            let work = benkyou::attempt::open(&dir, &task, &root)?;
            json(&serde_json::json!({
                "workspace": work,
                "concept": task.task.concept_id,
                "kind": task.task.kind,
                "instruction": dir.join("instruction.md"),
                "learner_secs": task.limits.learner_secs,
            }))
        }

        "grade" => {
            let dir = need(0, "exercise-dir")?;
            let task = exercise::load(&dir)?;
            let root = work_root(args, &dir, &task)?;
            let attempt = benkyou::attempt::grade(&dir, &task, &root)?;
            let score = benkyou::attempt::practice_score(&attempt.verdict);

            let mut practice = serde_json::Value::Null;
            if let (Some(goal), Some(score)) = (flag(args, "--goal"), score) {
                let gpath = store::goal_path(&goal).map_err(|e| format!("grade: {e}"))?;
                let graph = store::load_graph(&gpath)?;
                let node = task.task.concept_id.clone();
                if !graph.contains(&node) {
                    return Err(format!("grade: no node `{node}` in {}", gpath.display()));
                }
                let fpath = store::fluency_path(&gpath);
                let mut fluencies = store::load_fluencies(&fpath)?;
                let cfg = SchedConfig::default();
                let credited = sched::record_attempt(
                    &graph,
                    &mut fluencies,
                    &node,
                    score,
                    store::today(),
                    &cfg,
                );
                store::save_fluencies(&fpath, &fluencies)?;
                practice = serde_json::json!({
                    "node": node,
                    "score": score,
                    "confidence": fluencies.get(&node).map(|f| f.confidence),
                    "also_credited": credited,
                });
            }

            let passed = attempt.verdict.is_pass();
            let text = json(&serde_json::json!({
                "verdict": attempt.verdict,
                // The grader's own words. This is the whole feedback channel.
                "reward": attempt.reward.as_deref().and_then(|t| {
                    serde_json::from_str::<serde_json::Value>(t).ok()
                }),
                "practice": practice,
                "check_stderr": (!attempt.check_stderr.trim().is_empty())
                    .then(|| attempt.check_stderr.trim()),
            }))?;
            if passed {
                Ok(text)
            } else {
                // Exit non-zero so a failed kata is visible without parsing JSON.
                println!("{text}");
                Err("grade: attempt did not pass — see verdict".into())
            }
        }

        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}

/// The worked example `benkyou schema` prints.
///
/// Built out of the real types rather than held as a string of JSON, so it cannot
/// drift: a new field on [`Node`] or [`Edge`] fails to compile here, and what the
/// command prints is by construction what the parser accepts. Three nodes and one
/// edge of each type is the smallest graph that still shows every field in use —
/// including `goals`, which `ask` and `order` hand back to whoever writes the
/// artifact, so a node without it produces a vaguer exercise.
fn example_graph() -> Graph {
    Graph {
        goal: Goal {
            id: "example".to_string(),
            target: "on call for a small Kubernetes cluster: read what the scheduler \
                     actually did, and get a crashlooping service back up without guessing"
                .to_string(),
            deadline: Some("2026-09-30".to_string()),
            budget_hours: 20,
        },
        nodes: vec![
            Node {
                id: "pod_lifecycle".to_string(),
                title: "pod phases and restart backoff".to_string(),
                probe: "A pod reads Running, 0/1 ready, 7 restarts. Which container \
                        states did it pass through, and what is the kubelet waiting on \
                        before it tries again?"
                    .to_string(),
                kind: Kind::Concept,
                goals: vec![
                    "Name the phases a pod passes through".to_string(),
                    "Predict when the kubelet backs off".to_string(),
                ],
                cost_minutes: 30,
                relevance: 1.0,
                provenance: Provenance::JobDesc,
                gradable: true,
            },
            Node {
                id: "kubectl_inspect".to_string(),
                title: "inspecting a workload with kubectl".to_string(),
                probe: "Of `describe pod`, `logs --previous` and `get events`, only one \
                        shows why the last container exited. Which, and what do the \
                        other two show instead?"
                    .to_string(),
                kind: Kind::Tool,
                goals: vec!["Reach for the command that answers the question".to_string()],
                cost_minutes: 20,
                relevance: 0.8,
                provenance: Provenance::Llm,
                gradable: true,
            },
            Node {
                id: "debug_crashloop".to_string(),
                title: "diagnose a crashlooping deployment".to_string(),
                probe: "A deployment enters CrashLoopBackOff after a config change. Get \
                        from the symptom to the failing container's own error message \
                        without redeploying it."
                    .to_string(),
                kind: Kind::Skill,
                goals: vec![
                    "Separate a crash from a failing readiness probe".to_string(),
                    "Recover the dead container's output".to_string(),
                ],
                cost_minutes: 60,
                relevance: 1.0,
                provenance: Provenance::User,
                gradable: true,
            },
            // The point of the flag: a real skill on the critical path that no script
            // will ever mark. `order --kind exercise` refuses it and says so.
            Node {
                id: "incident_writeup".to_string(),
                title: "write the incident review".to_string(),
                probe: "The crashloop is fixed. Write the review: what broke, what the \
                        signal was, and what would have caught it an hour earlier."
                    .to_string(),
                kind: Kind::Skill,
                goals: vec![
                    "Separate the trigger from the cause".to_string(),
                    "Name a detection gap, not a person".to_string(),
                ],
                cost_minutes: 45,
                relevance: 0.8,
                provenance: Provenance::JobDesc,
                gradable: false,
            },
        ],
        edges: vec![
            // `requires` runs prerequisite -> dependent: you need the lifecycle before
            // you can debug a crashloop, so the lifecycle is `from`.
            Edge {
                from: "pod_lifecycle".to_string(),
                to: "debug_crashloop".to_string(),
                ty: EdgeType::Requires,
                strength: 1.0,
                reason: "backoff timing is the symptom itself; you cannot diagnose what \
                         you cannot name"
                    .to_string(),
                // Indices into the prerequisite's own `goals`, so an edge can depend on
                // part of a node. Non-empty on exactly one example edge on purpose: the
                // element type is `usize`, and an all-empty list cannot show that.
                needs_goals: vec![1],
                provenance: Provenance::Llm,
                confidence: 0.9,
            },
            // `helps` runs the same way round, and is soft ordering only.
            Edge {
                from: "kubectl_inspect".to_string(),
                to: "debug_crashloop".to_string(),
                ty: EdgeType::Helps,
                strength: 0.6,
                reason: "the right command shortens the loop, but the diagnosis is \
                         reachable without it"
                    .to_string(),
                needs_goals: Vec::new(),
                provenance: Provenance::Llm,
                confidence: 0.8,
            },
            // `encompasses` runs the OTHER way: practising `to` credits `from`, so the
            // harder node that contains the easier one is `to`.
            Edge {
                from: "pod_lifecycle".to_string(),
                to: "debug_crashloop".to_string(),
                ty: EdgeType::Encompasses,
                strength: 1.0,
                reason: "a crashloop worked end to end exercises the lifecycle it \
                         reasons over"
                    .to_string(),
                needs_goals: Vec::new(),
                provenance: Provenance::Llm,
                confidence: 0.9,
            },
            // The ungradable node is not off to one side: it sits on the path, which is
            // why refusing to invent a grader for it matters.
            Edge {
                from: "debug_crashloop".to_string(),
                to: "incident_writeup".to_string(),
                ty: EdgeType::Requires,
                strength: 0.9,
                reason: "you cannot write up a cause you never found".to_string(),
                needs_goals: Vec::new(),
                provenance: Provenance::JobDesc,
                confidence: 0.8,
            },
        ],
        state: State::default(),
    }
}

fn json<T: serde::Serialize>(v: &T) -> Result<String, String> {
    serde_json::to_string_pretty(v).map_err(|e| e.to_string())
}
