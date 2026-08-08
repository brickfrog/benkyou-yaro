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
    /// Advisory findings. These never reach `outcome`; see `check_run_cmd`.
    pub warnings: Vec<String>,
}

impl GateReport {
    /// The report body the `gate` command prints.
    ///
    /// `warnings` is omitted entirely rather than emitted empty, so an exercise with
    /// no `[workspace]` produces exactly the bytes it did before the advisory run
    /// existed and nothing reading this output has to be taught a new field.
    pub fn json(&self) -> serde_json::Value {
        let mut body = serde_json::json!({
            "outcome": self.outcome,
            "solution_verdict": self.solution.verdict,
            "empty_verdict": self.empty.verdict,
        });
        if !self.warnings.is_empty() {
            body["warnings"] = serde_json::json!(self.warnings);
        }
        body
    }
}

/// Run the workspace `run_cmd` once against the solved workspace, advisory only.
///
/// A broken Run button is a broken button, not an unsound exercise. Nothing on the
/// CLI path ever invokes `run_cmd`, so rejecting on it would fail exercises that are
/// perfectly valid to sit down to — the two directions above are what soundness
/// means. This reports a warning and changes nothing else.
///
/// The solved workspace is the right place to run it: it holds the reference answer,
/// so a `run_cmd` that fails there fails for reasons the learner cannot fix.
fn check_run_cmd(task: &Task, solution_root: &Path) -> Option<String> {
    let cmd = task.workspace.run_cmd.as_deref()?;
    let secs = task.limits.learner_secs;
    match Runner::in_dir(solution_root.join(WORK), secs).run(cmd) {
        Ok(o) if o.succeeded() => None,
        Ok(o) if o.timed_out => Some(format!("run_cmd timed out after {secs}s")),
        Ok(o) => Some(match o.exit_code {
            Some(code) => format!("run_cmd exited {code}"),
            None => "run_cmd did not exit normally".to_string(),
        }),
        Err(e) => Some(format!("run_cmd could not start: {e}")),
    }
}

/// Run both directions of the gate.
///
/// `scratch` must be on the same filesystem as anything large you care about; the
/// two runs get their own subdirectories under it and are left in place for
/// inspection.
pub fn run_gate(exercise_dir: &Path, scratch: &Path, at: &str) -> Result<GateReport, String> {
    // Taken before anything is read or run. This is the claim the verdict is about:
    // the bytes the two runs are going to execute.
    let before = crate::digest::exercise_digest(exercise_dir)?;
    let task = exercise::load(exercise_dir)?;

    let solution_root = scratch.join("gate-solution");
    let empty_root = scratch.join("gate-empty");
    let _ = fs::remove_dir_all(&solution_root);
    let _ = fs::remove_dir_all(&empty_root);
    fs::create_dir_all(&solution_root).map_err(|e| e.to_string())?;
    fs::create_dir_all(&empty_root).map_err(|e| e.to_string())?;

    let solution = run_once(exercise_dir, &task, &solution_root, true)?;

    // Advisory only, and only once the solution run has proved the exercise solvable:
    // a `run_cmd` failure against a workspace that does not even pass says nothing.
    let mut warnings = Vec::new();
    if solution.verdict.is_pass() {
        warnings.extend(check_run_cmd(&task, &solution_root));
    }

    let empty = run_once(exercise_dir, &task, &empty_root, false)?;

    // The exercise must not have moved underneath its own gate. An editor left open
    // during a slow gate, or a check script that writes back into the directory it was
    // copied from, would otherwise be certified on bytes that were never run - and the
    // digest stamped afterwards would make that undetectable ever after. Neither
    // digest is trustworthy on its own here; only their agreement is.
    let after = crate::digest::exercise_digest(exercise_dir)?;
    let outcome = if before == after {
        exercise::gate_outcome(&solution.verdict, &empty.verdict, at, &before)
    } else {
        exercise::GateOutcome::Rejected(exercise::GateFailure::ContentChangedDuringGate {
            before: before.clone(),
            after,
        })
    };

    Ok(GateReport { outcome, solution, empty, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(path: PathBuf, body: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, body).expect("write");
    }

    /// The smallest exercise the gate accepts: the reference writes the marker the
    /// grader looks for, and the untouched workspace does not have it. Kept free of
    /// python and uv so this test measures the gate and nothing else.
    fn exercise(name: &str, run_cmd: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("benkyou-gate-unit-{name}"));
        let _ = fs::remove_dir_all(&dir);
        put(dir.join("setup/answer.txt"), "");
        put(dir.join("solution/solve.sh"), "printf 'done\\n' > answer.txt\n");
        put(
            dir.join("check/check.sh"),
            "mkdir -p out\n\
             if [ -s work/answer.txt ]; then s=1; else s=0; fi\n\
             printf '{\"correctness\": %s}' \"$s\" > out/reward.json\n",
        );
        let workspace = match run_cmd {
            Some(cmd) => format!("\n[workspace]\nrun_cmd = \"{cmd}\"\n"),
            None => String::new(),
        };
        put(
            dir.join("task.toml"),
            &format!(
                "schema_version = \"1\"\n\n\
                 [task]\n\
                 id = \"{name}\"\n\
                 concept_id = \"unit\"\n\
                 kind = \"kata\"\n\
                 guidance_level = \"blank\"\n\n\
                 [limits]\n\
                 setup_secs = 10\n\
                 learner_secs = 10\n\
                 check_secs = 10\n\n\
                 [verify]\n\
                 cmd = \"sh check/check.sh\"\n\
                 must_pass = [\"correctness\"]\n\
                 {workspace}"
            ),
        );
        dir
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("benkyou-gate-unit-scratch-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// The whole point of the third run being advisory: the exercise is sound, only
    /// its Run button is broken, and the gate must still validate it.
    #[test]
    fn a_failing_run_cmd_warns_and_does_not_reject() {
        let report = run_gate(
            &exercise("failrun", Some("exit 2")),
            &scratch("failrun"),
            "t",
        )
        .expect("gate ran");

        assert!(
            matches!(report.outcome, GateOutcome::Validated(_)),
            "advisory run changed the outcome: {:?}",
            report.outcome
        );
        assert_eq!(report.warnings, vec!["run_cmd exited 2".to_string()]);
        assert_eq!(report.json()["warnings"][0], "run_cmd exited 2");
    }

    #[test]
    fn a_working_run_cmd_warns_about_nothing() {
        let report = run_gate(
            &exercise("okrun", Some("cat answer.txt")),
            &scratch("okrun"),
            "t",
        )
        .expect("gate ran");

        assert!(matches!(report.outcome, GateOutcome::Validated(_)));
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    /// An exercise with no `[workspace]` must print what it printed before the
    /// advisory run existed, byte for byte.
    #[test]
    fn no_run_cmd_leaves_the_printed_report_byte_identical() {
        let report = run_gate(&exercise("norun", None), &scratch("norun"), "t").expect("gate ran");
        assert!(report.warnings.is_empty());

        let before = serde_json::json!({
            "outcome": report.outcome,
            "solution_verdict": report.solution.verdict,
            "empty_verdict": report.empty.verdict,
        });
        assert_eq!(
            serde_json::to_string_pretty(&report.json()).expect("serialize"),
            serde_json::to_string_pretty(&before).expect("serialize"),
        );
    }
}
