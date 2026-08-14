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
use crate::run::{Access, Backend, Job};

/// Layout of a run directory. The run directory is the root of the job's view, so a
/// check script refers to `work/`, `check/` and `out/` relatively — under either
/// backend, because the sandbox reproduces these names.
pub const WORK: &str = "work";
pub const CHECK: &str = "check";
pub const OUT: &str = "out";
pub const SOLUTION: &str = "solution";

/// One graded run: build a workspace, optionally solve it, then check it.
pub struct Run {
    pub root: PathBuf,
    pub verdict: Verdict,
    pub check_stdout: String,
    pub check_stderr: String,
}

/// Copy a directory tree, refusing anything that is not a plain file or a directory.
///
/// One rule for every copy this tool makes, in both directions: an exercise and a
/// workspace are plain files and directories. Nothing else has a defined meaning here
/// and two of the alternatives are holes.
///
/// **Symlinks.** Following one reads a file the tree does not contain. On the exercise
/// side that breaks the digest, which hashes a link by its target *string* while the
/// copy reads the target's *content* — so `setup/data.csv` pointing at `/etc/shadow`
/// has a digest that never changes while the bytes reaching the runner do. On the
/// learner's side it is worse, because the learner writes that side: hidden grading
/// copies `work/` into a sealed directory, and this copy runs on the host with this
/// process's rights, before any sandbox exists. `ln -s ~/.ssh/id_rsa key` would be
/// answered by copying the key somewhere a check script reads. Copying the link
/// unfollowed would be safe under the sandbox and unsafe under `--unsafe-host`, so it
/// is refused in both.
///
/// **Device nodes, sockets, fifos.** Nothing well-defined to hash or to copy, and a
/// fifo would hang the copy.
///
/// A missing source directory is not an error: an exercise may have no `setup/`.
pub(crate) fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let entries = match fs::read_dir(from) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", from.display()))?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        // Not `entry.metadata()`: that follows links, and a link has to be seen to be
        // refused.
        let meta = fs::symlink_metadata(&src).map_err(|e| format!("{}: {e}", src.display()))?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            return Err(format!(
                "{}: symbolic link - not supported here, replace it with the file it \
                 points at",
                src.display()
            ));
        } else if kind.is_dir() {
            copy_dir(&src, &dst)?;
        } else if kind.is_file() {
            fs::copy(&src, &dst).map_err(|e| format!("{}: {e}", src.display()))?;
            // Preserve the executable bit; a check script that cannot run is a
            // broken grader that looks like a failing exercise.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode();
                let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(mode));
            }
        } else {
            return Err(format!("{}: not a regular file or directory", src.display()));
        }
    }
    Ok(())
}

/// Resolve a caller-supplied relative path inside a directory, or refuse.
///
/// Checked lexically on the *cleaned* path rather than by canonicalising, because the
/// target may not exist yet — canonicalize fails on a new file, and doing it on the
/// parent instead silently permits a symlinked parent. Rejecting `..` and absolute
/// roots outright is the rule that holds for files that do not exist.
///
/// Two callers with the same need: the browser resolving a path the page sent, and the
/// gate laying a known-bad candidate's files into a workspace. The first is a guard
/// against a wrong path; the second is a guard against a generated `task.toml` writing
/// `../../check/check.sh` and grading itself.
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return Err("empty path".into());
    }
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(format!("{rel}: absolute paths are not writable here"));
    }
    let mut out = root.to_path_buf();
    for part in p.components() {
        match part {
            std::path::Component::Normal(s) => out.push(s),
            std::path::Component::CurDir => {}
            _ => return Err(format!("{rel}: path must stay inside the workspace")),
        }
    }
    if out.is_symlink() {
        return Err(format!("{rel}: refusing to write through a symlink"));
    }
    Ok(out)
}

/// What is put in the workspace before the grader runs.
///
/// The three ways an exercise gets graded during a gate. Everything downstream is
/// identical — same deadline, same layout, same grader — so a difference in verdict is
/// a difference in the workspace and nothing else.
#[derive(Debug, Clone, Copy)]
pub enum Apply<'a> {
    /// The reference answer. Must pass.
    Solution,
    /// Nothing: `setup/` as the learner first sees it. Must fail.
    Nothing,
    /// A named wrong answer. Must fail, and must not break the grader.
    KnownBad(&'a exercise::KnownBad),
}

/// Build a run directory and grade it once.
///
/// Each variant of [`Apply`] is a separate job with its own view, and that is the
/// point of splitting them: the reference solution never sees `check/`. A `solve.sh`
/// that reads the hidden tests would pass the gate's first direction while proving
/// nothing about whether the exercise is solvable from what the learner is given.
pub fn run_once(
    exercise_dir: &Path,
    task: &Task,
    root: &Path,
    apply: Apply,
    backend: &Backend,
) -> Result<Run, String> {
    // Resolved once, before anything is copied: a missing set is the author's problem
    // to fix and there is no point building a workspace to discover it.
    let deps = crate::deps::require(&task.deps, crate::deps::Runtime::of(backend))?;
    let deps = deps.as_deref();
    let work = root.join(WORK);
    let check = root.join(CHECK);
    let out = root.join(OUT);
    copy_dir(&exercise_dir.join("setup"), &work)?;
    copy_dir(&exercise_dir.join("check"), &check)?;
    fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;

    match apply {
        Apply::Nothing => {}
        Apply::KnownBad(candidate) => {
            // Written by this process, not by a script the candidate supplies. A
            // candidate that can execute is one more generated script inside the
            // boundary, and one that could read `check/` on its way past.
            for (rel, body) in &candidate.files {
                let dst = safe_join(&work, rel)
                    .map_err(|e| format!("known_bad `{}`: {e}", candidate.id))?;
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
                }
                fs::write(&dst, body).map_err(|e| format!("{}: {e}", dst.display()))?;
            }
        }
        Apply::Solution => {
            let solution = exercise_dir.join(SOLUTION);
            let dst = root.join(SOLUTION);
            copy_dir(&solution, &dst)?;
            if !dst.join("solve.sh").exists() {
                return Err(format!("{}: no solution/solve.sh", exercise_dir.display()));
            }
            let applied = backend.run(&Job::new(
                root,
                &[(WORK, Access::Write), (SOLUTION, Access::Read)],
                WORK,
                "sh ../solution/solve.sh",
                task.limits.learner_secs,
            ).with_deps(deps))?;
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
            // Gone before the checker runs, so a check script cannot read the answer
            // it is supposed to be judging independently. The view would deny it
            // anyway; this keeps the left-behind run directory honest for inspection.
            let _ = fs::remove_dir_all(&dst);
        }
    }

    let outcome = backend.run(&Job::new(
        root,
        &[(WORK, Access::Write), (CHECK, Access::Read), (OUT, Access::Write)],
        "",
        &task.verify.cmd,
        task.limits.check_secs,
    ).with_deps(deps))?;

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
    /// One entry per named wrong answer, in the order `task.toml` lists them.
    pub known_bad: Vec<(exercise::KnownBad, Run)>,
    /// Advisory findings. These never reach `outcome`; see `check_run_cmd`.
    pub warnings: Vec<String>,
}

impl GateReport {
    /// The report body the `gate` command prints.
    ///
    /// `warnings` is omitted entirely rather than emitted empty, so an exercise with
    /// no `[workspace]` produces exactly the bytes it did before the advisory run
    /// existed and nothing reading this output has to be taught a new field.
    ///
    /// `known_bad_verdicts` is always present, because it is never empty in a
    /// validated exercise and its absence in a rejected one is itself the finding.
    pub fn json(&self) -> serde_json::Value {
        let mut body = serde_json::json!({
            "outcome": self.outcome,
            "solution_verdict": self.solution.verdict,
            "empty_verdict": self.empty.verdict,
            "known_bad_verdicts": self
                .known_bad
                .iter()
                .map(|(c, r)| serde_json::json!({ "id": c.id, "verdict": r.verdict }))
                .collect::<Vec<_>>(),
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
fn check_run_cmd(task: &Task, solution_root: &Path, backend: &Backend) -> Option<String> {
    let cmd = task.workspace.run_cmd.as_deref()?;
    let secs = task.limits.learner_secs;
    // An unwarmed set makes this advisory unrunnable, not the exercise unsound: the
    // caller already refused before getting here, so this only ever sees `Ok`.
    let deps = crate::deps::require(&task.deps, crate::deps::Runtime::of(backend)).ok().flatten();
    let job = Job::new(solution_root, &[(WORK, Access::Write)], WORK, cmd, secs)
        .with_deps(deps.as_deref());
    match backend.run(&job) {
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
///
/// Both runs read a frozen copy, never the exercise directory. Digesting a tree and
/// then executing from it are two reads, and anything that changes in between is
/// executed but not described - a file that moves to a second version and back is the
/// clean case, but a script that rewrites its own directory is the likely one. Copying
/// first collapses the two reads into one: the digest is of the snapshot, the runs are
/// of the snapshot, and no window exists between them.
pub fn run_gate(
    exercise_dir: &Path,
    scratch: &Path,
    at: &str,
    backend: &Backend,
) -> Result<GateReport, String> {
    let frozen = scratch.join("gate-frozen");
    let _ = fs::remove_dir_all(&frozen);
    copy_dir(exercise_dir, &frozen)?;

    // The claim the verdict is about: the bytes the two runs are going to execute,
    // read from the copy that will execute them.
    let before = crate::digest::exercise_digest(&frozen)?;
    let task = exercise::load(&frozen)?;

    // What the declared packages actually resolved to, recorded beside the verdict.
    // An exact pin fixes what the author named; this is the only record of the tree
    // underneath it, and `run_once` has already refused an unwarmed set by here.
    let resolved_deps = match crate::deps::require(&task.deps, crate::deps::Runtime::of(backend))? {
        Some(set) => crate::deps::resolved(&set)?,
        None => Vec::new(),
    };

    let solution_root = scratch.join("gate-solution");
    let empty_root = scratch.join("gate-empty");
    let _ = fs::remove_dir_all(&solution_root);
    let _ = fs::remove_dir_all(&empty_root);
    fs::create_dir_all(&solution_root).map_err(|e| e.to_string())?;
    fs::create_dir_all(&empty_root).map_err(|e| e.to_string())?;

    let solution = run_once(&frozen, &task, &solution_root, Apply::Solution, backend)?;

    // Advisory only, and only once the solution run has proved the exercise solvable:
    // a `run_cmd` failure against a workspace that does not even pass says nothing.
    let mut warnings = Vec::new();
    if solution.verdict.is_pass() {
        warnings.extend(check_run_cmd(&task, &solution_root, backend));
    }

    let empty = run_once(&frozen, &task, &empty_root, Apply::Nothing, backend)?;

    // One fresh workspace per candidate. Sharing one would let the first candidate's
    // files survive into the second, so a trap could spring on residue rather than on
    // the answer it was written to describe.
    let mut known_bad = Vec::new();
    for candidate in &task.known_bad {
        let root = scratch.join(format!("gate-bad-{}", candidate.id));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let run = run_once(&frozen, &task, &root, Apply::KnownBad(candidate), backend)?;
        known_bad.push((candidate.clone(), run));
    }

    // The snapshot is what ran, but the verdict is written next to the original, and
    // it is the original a learner will sit down to. If the author edited it while the
    // gate was working, this verdict describes a tree that is no longer there. Neither
    // digest is trustworthy alone here; only their agreement is.
    let after = crate::digest::exercise_digest(exercise_dir)?;
    let outcome = if before == after {
        let verdicts: Vec<(&exercise::KnownBad, Verdict)> = known_bad
            .iter()
            .map(|(c, r)| (c, r.verdict.clone()))
            .collect();
        exercise::gate_outcome(
            &solution.verdict,
            &empty.verdict,
            &verdicts,
            at,
            &before,
            backend,
            &resolved_deps,
        )
    } else {
        exercise::GateOutcome::Rejected(exercise::GateFailure::ContentChangedDuringGate {
            before: before.clone(),
            after,
        })
    };

    Ok(GateReport { outcome, solution, empty, known_bad, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::Want;

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
                 {workspace}\n\
                 [[known_bad]]\n\
                 id = \"always_empty\"\n\
                 trap = \"leaves the answer file empty\"\n\
                 files.\"answer.txt\" = \"\"\n"
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
            &Backend::choose(Want::Auto, None).expect("a sandbox"),
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
            &Backend::choose(Want::Auto, None).expect("a sandbox"),
        )
        .expect("gate ran");

        assert!(matches!(report.outcome, GateOutcome::Validated(_)));
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    /// An exercise with no `[workspace]` must print what it printed before the advisory
    /// run existed, apart from the fields added since — checked by naming them, so a
    /// future field cannot slip in unnoticed.
    #[test]
    fn the_printed_report_carries_exactly_the_expected_fields() {
        let report = run_gate(
            &exercise("norun", None),
            &scratch("norun"),
            "t",
            &Backend::choose(Want::Auto, None).expect("a sandbox"),
        )
        .expect("gate ran");
        assert!(report.warnings.is_empty());

        let body = report.json();
        let mut keys: Vec<&str> = body.as_object().expect("object").keys().map(|s| s.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["empty_verdict", "known_bad_verdicts", "outcome", "solution_verdict"],
            "the report grew or lost a field"
        );
        assert_eq!(body["known_bad_verdicts"][0]["id"], "always_empty");
    }
}
