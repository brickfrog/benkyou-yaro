//! The validation gate.
//!
//! A generated exercise is not shown until the gate has run it twice: once with
//! the reference solution applied, which must pass, and once with the starting state
//! untouched, which must fail. Run one alone admits a vacuous check that passes on an
//! empty workspace; run two alone admits an unsolvable exercise.
//!
//! This is the difference between an exercise and an LLM emitting prose with a test
//! file next to it. See DESIGN.md §3.

use std::fs;
use std::path::{Path, PathBuf};

use crate::exercise::{self, GateOutcome, Task, Verdict};
use crate::run::Runner;

/// Layout of a run directory. The run directory is the working directory, so
/// a check script refers to `work/`, `check/` and `out/` relatively.
pub const WORK: &str = "work";
pub const CHECK: &str = "check";
pub const OUT: &str = "out";

/// One graded run: build a workspace, optionally solve it, then check it.
pub struct Run {
    pub root: PathBuf,
    pub verdict: Verdict,
    pub check_stdout: String,
    pub check_stderr: String,
}

pub(crate) fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let entries = match fs::read_dir(from) {
        Ok(e) => e,
        // A missing source directory is not an error: an exercise may have no setup.
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", from.display()))?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let meta = entry
            .metadata()
            .map_err(|e| format!("{}: {e}", src.display()))?;
        if meta.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            fs::copy(&src, &dst).map_err(|e| format!("{}: {e}", src.display()))?;
            // Preserve the executable bit; a check script that cannot run is a
            // broken grader that looks like a failing exercise.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode();
                let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

/// Build a run directory and grade it once.
///
/// `solve` applies the reference solution before checking. Both directions run with
/// the same deadline and the same working directory layout, so the only difference
/// between them is whether the solution was applied.
pub fn run_once(
    exercise_dir: &Path,
    task: &Task,
    root: &Path,
    solve: bool,
) -> Result<Run, String> {
    let work = root.join(WORK);
    let check = root.join(CHECK);
    let out = root.join(OUT);
    copy_dir(&exercise_dir.join("setup"), &work)?;
    copy_dir(&exercise_dir.join("check"), &check)?;
    fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;

    if solve {
        let solution = exercise_dir.join("solution");
        let dst = root.join("solution");
        copy_dir(&solution, &dst)?;
        if !dst.join("solve.sh").exists() {
            return Err(format!("{}: no solution/solve.sh", exercise_dir.display()));
        }
        let applied = Runner::in_dir(root, task.limits.learner_secs)
            .run(&format!("cd {WORK} && sh ../solution/solve.sh"))?;
        if !applied.succeeded() {
            return Ok(Run {
                root: root.to_path_buf(),
                verdict: Verdict::CheckBroken(format!(
                    "reference solution did not run: exit {:?}: {}",
                    applied.exit_code,
                    applied.stderr.trim()
                )),
                check_stdout: applied.stdout,
                check_stderr: applied.stderr,
            });
        }
        // The solution directory must not be visible to the checker.
        let _ = fs::remove_dir_all(&dst);
    }

    let outcome = Runner::in_dir(root, task.limits.check_secs).run(&task.verify.cmd)?;

    let reward_path = out.join(&task.verify.reward);
    let reward_text = fs::read_to_string(&reward_path).ok();

    let verdict = exercise::grade(
        &task.verify,
        outcome.exit_code,
        outcome.timed_out.then_some(task.limits.check_secs),
        reward_text.as_deref(),
    );

    Ok(Run {
        root: root.to_path_buf(),
        verdict,
        check_stdout: outcome.stdout,
        check_stderr: outcome.stderr,
    })
}

pub struct GateReport {
    pub outcome: GateOutcome,
    pub solution: Run,
    pub empty: Run,
}

/// Run both directions of the gate.
///
/// `scratch` must be on the same filesystem as anything large you care about; the
/// two runs get their own subdirectories under it and are left in place for
/// inspection.
pub fn run_gate(exercise_dir: &Path, scratch: &Path, at: &str) -> Result<GateReport, String> {
    let task = exercise::load(exercise_dir)?;

    let solution_root = scratch.join("gate-solution");
    let empty_root = scratch.join("gate-empty");
    let _ = fs::remove_dir_all(&solution_root);
    let _ = fs::remove_dir_all(&empty_root);
    fs::create_dir_all(&solution_root).map_err(|e| e.to_string())?;
    fs::create_dir_all(&empty_root).map_err(|e| e.to_string())?;

    let solution = run_once(exercise_dir, &task, &solution_root, true)?;
    let empty = run_once(exercise_dir, &task, &empty_root, false)?;

    let outcome = exercise::gate_outcome(&solution.verdict, &empty.verdict, at);
    Ok(GateReport { outcome, solution, empty })
}
