//! The learner-facing loop.
//!
//! `gate` proves an exercise is sound before it is ever shown; this runs the learner's
//! own solution against that same grader. `open` materialises a workspace from
//! `setup/`, and `grade` scores whatever was left in it.
//!
//! They are two commands rather than one because the learner does the work in
//! between — in their own editor, on their own clock. See DESIGN.md §3.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::exercise::{self, Task, Verdict};
use crate::gate::{copy_dir, CHECK, OUT, WORK};
use crate::run::Runner;

/// The result of grading one attempt.
///
/// Carries the reward file's text because a hidden grade destroys the directory it
/// was written in: the detail the learner is owed has to survive the cleanup.
pub struct Attempt {
    pub verdict: Verdict,
    pub reward: Option<String>,
    pub check_stdout: String,
    pub check_stderr: String,
}

/// Materialise a workspace. Copies `setup/` and nothing else: `solution/` and
/// `check/` never reach the learner's directory, so the answer is not sitting next to
/// the file they are editing.
///
/// That is the whole of the guarantee, and it is worth being exact about. During
/// grading the check scripts are staged beside `work/` and the verify command runs
/// with that directory as its cwd, so code the learner wrote can read `check/` while
/// it executes — and the Python graders import the learner's module into the grading
/// process itself, which no directory layout can undo. Sealing that needs a sandbox,
/// which this tool deliberately does not have. Confidentiality here is a convenience
/// against stumbling onto the answer, never a defence against going to look for it.
///
/// Refuses an exercise the gate has not validated, and one edited since it was.
/// That rule is the whole difference between an exercise and a model's guess at
/// one, so it lives here rather than in the CLI, where only one caller would
/// honour it.
///
/// Refuses a workspace that already holds files rather than overwriting one — the
/// thing at risk here is unsaved work.
pub fn open(exercise_dir: &Path, root: &Path) -> Result<PathBuf, String> {
    exercise::require_current(exercise_dir)?;
    let work = root.join(WORK);
    let occupied = fs::read_dir(&work)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    if occupied {
        return Err(format!(
            "{}: workspace already has files in it; grade it, or delete it to start over",
            work.display()
        ));
    }
    copy_dir(&exercise_dir.join("setup"), &work)?;
    Ok(work)
}

/// Grade whatever the learner left in the workspace.
///
/// With `verify.hidden`, grading happens in a throwaway directory that is deleted
/// before this returns: hidden cases must not become readable merely because the
/// learner asked for a grade, and a checker copied next to their work would hand
/// over the reference on the first run. Without it, the run is left beside their
/// workspace to be picked over.
///
/// Either way `work/` is the one directory never written to.
///
/// Refuses an ungated or edited exercise for the same reason [`open`] does, and with
/// more at stake: a grader nobody proved discriminating would still write a score into
/// the learner's fluency, where it decides what they are shown next.
pub fn grade(exercise_dir: &Path, task: &Task, root: &Path) -> Result<Attempt, String> {
    exercise::require_current(exercise_dir)?;
    let work = root.join(WORK);
    if !work.exists() {
        return Err(format!("{}: no workspace here yet", work.display()));
    }
    if !task.verify.hidden {
        return grade_in(exercise_dir, task, root);
    }
    let sealed = throwaway_dir()?;
    copy_dir(&work, &sealed.join(WORK))?;
    let result = grade_in(exercise_dir, task, &sealed);
    let _ = fs::remove_dir_all(&sealed);
    result
}

/// Run the grader against a prepared root. `check/` is re-copied and `out/` cleared
/// every time: the grader is not the learner's to edit, and a stale reward file must
/// never be read as a fresh result.
fn grade_in(exercise_dir: &Path, task: &Task, root: &Path) -> Result<Attempt, String> {
    let check = root.join(CHECK);
    let out = root.join(OUT);
    let _ = fs::remove_dir_all(&check);
    let _ = fs::remove_dir_all(&out);
    copy_dir(&exercise_dir.join("check"), &check)?;
    fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;

    let outcome = Runner::in_dir(root, task.limits.check_secs).run(&task.verify.cmd)?;
    let reward = fs::read_to_string(out.join(&task.verify.reward)).ok();
    let verdict = exercise::grade(
        &task.verify,
        outcome.exit_code,
        outcome.timed_out.then_some(task.limits.check_secs),
        reward.as_deref(),
    );

    Ok(Attempt {
        verdict,
        reward,
        check_stdout: outcome.stdout,
        check_stderr: outcome.stderr,
    })
}

fn throwaway_dir() -> Result<PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("benkyou-grade-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// What an attempt is worth as practice, or `None` when the run says nothing about
/// the learner at all.
///
/// A broken grader is the author's fault and must not decay anyone's fluency. A
/// timeout is the learner's: an infinite loop is a failed attempt, not a failed
/// harness.
pub fn practice_score(verdict: &Verdict) -> Option<f32> {
    match verdict {
        Verdict::Pass => Some(1.0),
        // Partial credit is the *worst* gating dimension, not the average: passing
        // three checks and failing one is not three quarters of a solved kata.
        Verdict::Fail(dims) if dims.is_empty() => Some(0.0),
        Verdict::Fail(dims) => Some(
            dims.values()
                .copied()
                .fold(f32::INFINITY, f32::min)
                .clamp(0.0, 1.0),
        ),
        Verdict::Timeout(_) => Some(0.0),
        Verdict::CheckBroken(_) => None,
    }
}

/// What crediting one attempt did to the schedule.
pub struct Credited {
    pub node: String,
    pub score: f32,
    pub confidence: Option<f32>,
    /// A set, not a list: `record_attempt` credits each ancestor once, and the
    /// ordering is what makes the CLI's output diffable between runs.
    pub also_credited: std::collections::BTreeSet<String>,
}

/// Record a graded attempt against a goal's fluency, returning what moved.
///
/// This lives here rather than in the CLI because `grade` is no longer the only
/// caller: the browser runner submits through the same path, and a second copy of
/// this would be a second answer to "what does passing a kata do to the schedule".
///
/// Refuses a concept the graph does not contain. A score recorded against a node
/// nobody declared is a score nothing will ever read again, and silently accepting
/// it hides a typo in the exercise's `concept_id` until the schedule looks wrong.
pub fn credit(
    goal_path: &Path,
    concept: &str,
    score: f32,
    today: i64,
) -> Result<Credited, String> {
    let graph = crate::store::load_graph(goal_path)?;
    if !graph.contains(concept) {
        return Err(format!(
            "no node `{concept}` in {}",
            goal_path.display()
        ));
    }
    let fpath = crate::store::fluency_path(goal_path);
    let mut fluencies = crate::store::load_fluencies(&fpath)?;
    let cfg = crate::sched::SchedConfig::default();
    let also_credited = crate::sched::record_attempt(
        &graph,
        &mut fluencies,
        concept,
        score,
        today,
        &cfg,
    );
    crate::store::save_fluencies(&fpath, &fluencies)?;
    Ok(Credited {
        node: concept.to_string(),
        score,
        confidence: fluencies.get(concept).map(|f| f.confidence),
        also_credited,
    })
}
