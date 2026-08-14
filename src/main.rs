//! The CLI an agent drives.
//!
//! This binary holds no API key and calls no model. It emits structured *generation
//! orders*; the agent already in a conversation fills them with the model the user is
//! already paying for and writes the result back. See DESIGN.md §6.
//!
//! One command reaches a network: `warm` installs an exercise's declared packages from
//! a package index. It is separate precisely so that it is the exception - nothing on
//! the gating or grading path can reach anything, and a verdict never depends on what
//! an index served that afternoon.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use benkyou::bank;
use benkyou::assess::{self, AssessConfig, RecordOutcome, Step};
use benkyou::exercise::{self, GateOutcome, Task};
use benkyou::gate::run_gate;
use benkyou::run::{Backend, Want};
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

  benkyou cards <cards.json> [--deck NAME] [--push] [--anki-addr HOST:PORT]
      Build Anki notes from generated cards. Prints them and stops; pass --push
      to write them. Note identity comes from concept + role, never from the card
      text, so a regenerated card updates in place and keeps its review history.
      --push talks to AnkiConnect at 127.0.0.1:8765, where the add-on listens on
      the machine running Anki. --anki-addr, or $BENKYOU_ANKI_ADDR, names a
      different one: Anki on another machine reached through a forwarded port
      (`ssh -R 8765:127.0.0.1:8765 host`, then the address stays the default), or
      a tunnel landing on a port of its own.

  benkyou gate <exercise-dir> [--scratch DIR]
      Run the validation gate. The reference solution must pass, the untouched
      starting state must fail, and every [[known_bad]] answer named in task.toml
      must fail without breaking the grader. At least one is required: the first
      two runs share an author with the exercise, so only a wrong answer you
      commit to in advance can catch a grader that misread the concept.
      Records the result in `.gate.json` beside the exercise, which is what makes
      it showable; your own files are not touched. The record is bound to a hash
      of the exercise, so any later edit ungates it until you run this again.
      Also copies the exercise into the bank under that hash, so it outlives the
      directory you wrote it in. Exits non-zero if the exercise is rejected.

  benkyou items [--concept ID]
      List banked exercises: what survived a gate and can be sat down to again.
      Each entry gives a digest, the concept, and when it last passed a gate.
      Anywhere below that takes <exercise-dir> also takes a digest, or enough of
      the front of one to be unambiguous.

  benkyou warm <exercise-dir> [--force]
      Install the packages an exercise declares in [deps] so its scripts can
      import them. One of two commands here that use a network, and the only
      reason it exists: runs have none, so a `pip install` at grade time fails
      and a PEP 723 header resolves against nothing. Warming happens once, on
      purpose, before gating; every later run binds the result read-only.
      Sets are keyed by the package list and by the runtime that will import
      them - the machine's interpreter ABI under the sandbox, or the runner
      image, its architecture and its ABI under a container - so a set is never
      loaded by an interpreter it was not built for. Warming for a container
      runs `pip` inside that image, for the same reason.
      Every requirement must be a registry name pinned to one exact version
      (`pandas==3.0.5`); a bare name or a range is refused, because the cache
      key is a digest of the list and would otherwise name changing bytes.
      Only wheels are installed: a URL, a path, an editable, or a package that
      has to be built would run code on your machine out of a generated file.
      --force refetches, and repairs a set whose manifest is missing.

  benkyou runner [--pull] [--image REF]
      Report the container engine and the runner image, and fetch the image with
      --pull. The other command that uses a network, and the only one that
      fetches an image: gate, attempt, grade and serve resolve what is already
      local and refuse otherwise, so a runtime is never downloaded in the middle
      of a verdict. Exits non-zero when the image is absent, after printing what
      is missing.

  benkyou attempt <exercise-dir> [--work <dir>]
      Materialise a workspace and sit down to the exercise. Refuses anything the
      gate has not validated or that changed since, and copies only setup/ —
      never the solution.

  benkyou serve <exercise-dir>... [--goal <goal>] [--port N] [--no-open]
      Sit down to a queue of exercises in a browser instead of an editor. Serves a
      page on 127.0.0.1 and prints its URL, which carries a one-session token. Run
      and submit execute here, in this process, against the same grader `grade`
      uses; the page only edits files and shows output. The queue is the argument
      list, in order — there is no goal-driven queue, because nothing maps a concept
      to a directory. With --goal a pass records practice fluency, as `grade` does.
      Exits when the page says it is finished.

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

EXECUTION
  `gate`, `attempt`, `grade` and `serve` run generated scripts isolated: no network, no
  host filesystem beyond a read-only runtime, no access to your goals or workspaces, a
  throwaway HOME, a scrubbed environment, and resource ceilings. Two backends provide
  that, and one of them is chosen for you.

  sandbox    `bwrap` (bubblewrap), and the default wherever it works. Linux only:
             it isolates with Linux namespaces. The runtime is the machine's own
             read-only /usr, so a verdict says nothing about which interpreter
             earned it.
  container  `docker` or `podman`, used when there is no sandbox — which is what a
             mac has. The runtime is a pinned image instead of your /usr, so a
             verdict names it exactly and is refused under any other image. Needs
             the image fetched once with `benkyou runner --pull`; nothing on this
             path ever reaches a network by itself.

  --container asks for the container backend on a machine that also has a sandbox,
  which is how you gate against the runtime a mac will use. --image REF, or
  $BENKYOU_RUNNER_IMAGE, names a different image: the default carries python3 and
  the usual shell tools and nothing else, so an exercise needing `sqlite3` needs its
  own. Warm dependencies against the same backend you will gate with; the cache is
  keyed by the runtime and a set built for one is not a set for the other.

  Everything outside those four commands - the graph, the assessment, the schedule,
  the orders, the cards - is plain file work and runs anywhere, with or without
  either backend.

  --unsafe-host turns isolation off for one invocation. Generated scripts then run
  as you, over your whole filesystem. A gate verdict records which backend earned it
  and every other backend refuses it, so this is a choice you keep making rather than
  one you make once.

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

/// Print advisory notes about an exercise's gate record to stderr.
///
/// stderr, not the JSON body: every command here prints machine-readable output on
/// success, and a caller parsing that must not have to learn a new field to stay
/// working. Advisory means advisory - nothing here can stop the command, mirroring the
/// gate's own split between an outcome and its warnings.
fn warn_drift(dir: &std::path::Path, backend: &Backend) {
    for note in exercise::gate_warnings(dir, backend) {
        eprintln!("benkyou: {}: {note}", dir.display());
    }
}

/// Choose the execution backend for this invocation.
///
/// Sandboxed unless the caller says otherwise in as many words. The absence of a
/// sandbox is a container or an error, never a silent downgrade: every script this tool
/// runs was written by a model or by a learner, and the difference between running one
/// isolated and running one as the user is the difference the caller has to consent to.
/// No prompt, because `gate` runs unattended in the middle of a generation loop — a flag
/// or nothing.
fn backend(args: &[String]) -> Result<Backend, String> {
    let want = if args.iter().any(|a| a == "--unsafe-host") {
        eprintln!(
            "benkyou: --unsafe-host: running generated scripts with your own user's \
             rights, outside any sandbox"
        );
        Want::UnsafeHost
    } else if args.iter().any(|a| a == "--container") {
        Want::Container
    } else {
        Want::Auto
    };
    Backend::choose(want, runner_image(args).as_deref())
}

/// Which runtime a container job runs in: `--image`, then `$BENKYOU_RUNNER_IMAGE`, then
/// the pinned default.
///
/// An environment variable as well as a flag because it is a property of the machine
/// rather than of the command: a reader whose exercises need a `sqlite3` the default
/// image lacks sets it once, and every later `gate` and `grade` agrees with the `warm`
/// that filled the cache. An exported-but-empty value counts as unset.
fn runner_image(args: &[String]) -> Option<String> {
    flag(args, "--image")
        .or_else(|| std::env::var("BENKYOU_RUNNER_IMAGE").ok())
        .filter(|s| !s.trim().is_empty())
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Flags that take no value. Without this list `positional` eats the argument after
/// a boolean flag, so `cards --push cards.json` loses the file — the flag consumes
/// it and the command reports a missing argument for something plainly there.
const VALUELESS: &[&str] = &[
    "--help",
    "--version",
    "--push",
    "--no-open",
    "--unsafe-host",
    "--container",
    "--pull",
    "--force",
];

fn positional(args: &[String]) -> Vec<&String> {
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a.starts_with("--") {
            skip = !VALUELESS.contains(&a.as_str());
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

/// Best effort. A failed open is not a failed session: the URL is already on stdout,
/// and a headless box or an unset `$BROWSER` is a normal way to run this.
fn open_browser(url: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
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
    // An exercise argument is a path unless it is a bare digest naming a banked
    // bundle. Resolved in one place, like `need_goal`, so no command can disagree
    // with another about what an argument means.
    //
    // A path that exists wins, always. A directory genuinely called `deadbeef` has to
    // keep working, and the bank must never shadow something the caller pointed at.
    let need_exercise = |i: usize| -> Result<PathBuf, String> {
        let dir = need(i, "exercise-dir")?;
        if dir.exists() {
            return Ok(dir);
        }
        let looks_like_a_digest = dir
            .to_str()
            .is_some_and(|s| s.len() >= 8 && s.bytes().all(|b| b.is_ascii_hexdigit()));
        if !looks_like_a_digest {
            // Not hex, so it was meant as a path. Hand back the path and let the
            // command report the missing directory: telling someone who mistyped a
            // directory name that it is not valid hex helps nobody.
            return Ok(dir);
        }
        let name = dir.to_string_lossy().into_owned();
        bank::bank_dir()
            .and_then(|b| bank::resolve(&b, &name))
            .map_err(|e| format!("{cmd}: {e}"))
    };

    match cmd {
        // What the bank holds. The counterpart to `goals`: that lists what you are
        // learning, this lists the exercises that survived a gate and are still
        // available to sit down to.
        "items" => {
            let bank = bank::bank_dir()?;
            let concept = flag(args, "--concept");
            let items: Vec<_> = bank::list(&bank)?
                .into_iter()
                .filter(|(_, m)| concept.as_ref().is_none_or(|c| &m.concept_id == c))
                .map(|(digest, m)| bank::describe(&bank, &digest, &m))
                .collect();
            json(&serde_json::json!({
                "dir": bank.display().to_string(),
                "items": items,
            }))
        }

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
                                // record for the node underneath, at credited mastery
                                // and zero attempts — counting those as practised tells
                                // the learner they drilled something they have never
                                // once sat down to, which is the same overstatement as
                                // counting a claim as knowledge.
                                entry["practised"] =
                                    f.values().filter(|x| x.attempts > 0).count().into();
                                entry["credited"] =
                                    f.values().filter(|x| x.attempts == 0).count().into();
                                // Not "retired": reaching the ceiling buys the longest
                                // review interval, it does not remove a concept from the
                                // schedule. Reporting it as retirement would promise a
                                // finish line the scheduler no longer has.
                                let today = store::today();
                                entry["at_ceiling"] = f
                                    .values()
                                    .filter(|x| x.mastery >= cfg.mastery_ceiling)
                                    .count()
                                    .into();
                                entry["due"] = f
                                    .values()
                                    .filter(|x| sched::is_due(x, today, &cfg))
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
                "mastery": fluencies.get(&node).map(|f| f.mastery),
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

            // Resolved before the dry run rather than beside the push. The dry run is
            // the rehearsal, so an address `--push` would refuse has to fail here as
            // well, or the rehearsal is not one. An exported-but-empty variable counts
            // as unset, which is what every shell that exports it conditionally means.
            let addr = flag(args, "--anki-addr")
                .or_else(|| std::env::var("BENKYOU_ANKI_ADDR").ok())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| benkyou::anki::DEFAULT_ADDR.to_string());
            let anki = benkyou::anki::AnkiConnect::new(&addr)?;

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

            let version = anki.version()?;
            let created = anki.ensure_models()?;
            let report = anki.push(&cards, &deck)?;
            let failed = report.failed.len();
            let body = json(&serde_json::json!({
                // Which collection took the write. With a tunnel in play the version
                // number alone does not say which machine answered.
                "anki_addr": anki.addr,
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
            let backend = backend(args)?;
            let report = run_gate(&dir, &scratch, &at, &backend)?;
            let body = report.json();
            let text = json(&body)?;
            match report.outcome {
                GateOutcome::Validated(gate) => {
                    // Persist it. An exercise is showable *because* the gate has run,
                    // and `attempt` has no other way to know that it did.
                    //
                    // Into a sidecar, never into `task.toml`: the digest inside this
                    // record covers the authored files byte for byte, which is only
                    // possible while the tool never writes to them.
                    exercise::write_gate(&dir, &gate)?;

                    // And into the bank, because this is the moment the exercise
                    // became worth keeping: it has been run twice and against every
                    // wrong answer its author named. The directory it was written to
                    // is usually under /tmp and will not survive the week.
                    //
                    // Best-effort. A bank that cannot be written is worth reporting
                    // and never worth failing a gate over - the verdict was earned,
                    // and the sidecar beside the exercise already records it.
                    let mut body = body;
                    let task = exercise::load(&dir)?;
                    // The attestation is the gate record verbatim. Summarising it
                    // would throw away the `Runner`, which is the thing that decides
                    // whether this verdict still applies on the machine that later
                    // picks the bundle up.
                    match bank::bank_dir()
                        .and_then(|b| bank::deposit(&b, &dir, &gate.digest, &task, &gate))
                    {
                        Ok(path) => {
                            body["banked"] = serde_json::json!({
                                "digest": gate.digest,
                                "path": path.display().to_string(),
                            });
                        }
                        Err(e) => body["bank_failed"] = e.into(),
                    }
                    json(&body)
                }
                // A rejected exercise is a failure of the command, not a report:
                // the caller must not go on to show it to a learner.
                GateOutcome::Rejected(_) => {
                    println!("{text}");
                    Err("gate: exercise rejected — see outcome".into())
                }
            }
        }

        // The only command that uses a network, and the only one that is expected to
        // be slow. Split out from `gate` rather than folded into it so that the
        // networked step is something a person runs on purpose: a gate that silently
        // reached an index would make every later verdict depend on what a registry
        // served that afternoon.
        "warm" => {
            let dir = need_exercise(0)?;
            let task = exercise::load(&dir)?;
            let force = args.iter().any(|a| a == "--force");
            // Warming needs to know which runtime will import the result, so it selects
            // a backend exactly as `gate` does. A set built against the machine's Python
            // is not a set for the runner image's, and the failure if they diverge is an
            // import error inside a package that is plainly installed.
            let backend = backend(args)?;
            let runtime = benkyou::deps::Runtime::of(&backend);
            match benkyou::deps::warm(&task.deps, force, runtime)? {
                None => json(&serde_json::json!({
                    "warmed": [],
                    "note": "no [deps] declared - nothing to warm",
                })),
                Some(w) => json(&serde_json::json!({
                    "warmed": w.python,
                    "runtime": w.runtime,
                    "backend": backend.name(),
                    "path": w.path,
                    "fetched": w.fetched,
                    // The whole tree, not just what was asked for: an exact pin fixes
                    // the names above and nothing under them, and this is the list the
                    // gate will record beside its verdict.
                    "resolved": w.resolved,
                })),
            }
        }

        // Where the container backend's one network step lives. Reporting and fetching
        // are the same command because the answer to "is the runtime here?" is the thing
        // a reader needs before either gating or pulling.
        "runner" => {
            let pull = args.iter().any(|a| a == "--pull");
            let status = benkyou::run::runner_status(runner_image(args).as_deref(), pull)?;
            let body = match &status.image {
                Some(image) => serde_json::json!({
                    "engine": status.engine,
                    "engine_version": status.version,
                    "image": image.reference,
                    "id": image.id,
                    "arch": image.arch,
                    "present": true,
                    "pulled": status.pulled,
                }),
                None => serde_json::json!({
                    "engine": status.engine,
                    "engine_version": status.version,
                    "image": status.reference,
                    "present": false,
                    "pulled": status.pulled,
                    "hint": "run `benkyou runner --pull` to fetch it",
                }),
            };
            let text = json(&body)?;
            if status.image.is_none() {
                // The one command that can report an absent runtime without failing is
                // still not going to pretend it found one: print the report, then exit
                // non-zero so a script that gates next stops here.
                println!("{text}");
                return Err(format!("runner image not present: {}", status.reference));
            }
            Ok(text)
        }

        "attempt" => {
            let dir = need_exercise(0)?;
            let task = exercise::load(&dir)?;
            let root = work_root(args, &dir, &task)?;
            std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
            let backend = backend(args)?;
            warn_drift(&dir, &backend);
            let work = benkyou::attempt::open(&dir, &root, &backend)?;
            json(&serde_json::json!({
                "workspace": work,
                "concept": task.task.concept_id,
                "kind": task.task.kind,
                "instruction": dir.join("instruction.md"),
                "learner_secs": task.limits.learner_secs,
            }))
        }

        "serve" => {
            // The queue is the argument list. Each entry is a directory, or the digest
            // of a banked exercise — which is how "redo that kata" works at all, since
            // the directory an exercise was authored in is usually under /tmp and gone
            // by the time you want it again.
            let dirs: Vec<PathBuf> = (0..pos.len())
                .map(need_exercise)
                .collect::<Result<Vec<_>, _>>()?;
            if dirs.is_empty() {
                return Err("serve: name at least one exercise directory".into());
            }
            let backend = backend(args)?;
            dirs.iter().for_each(|d| warn_drift(d, &backend));
            let items = dirs
                .iter()
                .map(|d| benkyou::browser::Item::load(d, &backend))
                .collect::<Result<Vec<_>, _>>()?;

            let goal = match flag(args, "--goal") {
                Some(g) => {
                    let p = store::goal_path(&g).map_err(|e| format!("serve: {e}"))?;
                    // Fail now rather than on the first submit: a session that cannot
                    // record is one the learner would finish before finding out.
                    store::load_graph(&p)?;
                    Some(p)
                }
                None => None,
            };

            let port: u16 = match flag(args, "--port") {
                Some(p) => p.parse().map_err(|_| "serve: --port wants a number")?,
                None => 0,
            };
            let server = benkyou::serve::bind(port)?;
            let url = format!("http://127.0.0.1:{}/?t={}", server.port(), server.token());
            let app = benkyou::browser::App::new(items, goal, server.shutdown_handle(), backend);

            println!("{url}");
            if !args.iter().any(|a| a == "--no-open") {
                open_browser(&url);
            }
            server.run(move |req| app.handle(req))?;
            Ok(String::new())
        }

        "grade" => {
            let dir = need_exercise(0)?;
            let task = exercise::load(&dir)?;
            let root = work_root(args, &dir, &task)?;
            let backend = backend(args)?;
            warn_drift(&dir, &backend);
            let attempt = benkyou::attempt::grade(&dir, &task, &root, &backend)?;
            let score = benkyou::attempt::practice_score(&attempt.verdict);

            let mut practice = serde_json::Value::Null;
            if let (Some(goal), Some(score)) = (flag(args, "--goal"), score) {
                let gpath = store::goal_path(&goal).map_err(|e| format!("grade: {e}"))?;
                let c = benkyou::attempt::credit(
                    &gpath,
                    &task.task.concept_id,
                    score,
                    store::today(),
                )
                .map_err(|e| format!("grade: {e}"))?;
                practice = serde_json::json!({
                    "node": c.node,
                    "score": c.score,
                    "mastery": c.mastery,
                    "also_credited": c.also_credited,
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
