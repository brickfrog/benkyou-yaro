//! Exercises: the task schema, the grading contract, and the validation gate.
//!
//! The gate is what separates a generated exercise from an LLM emitting prose with a
//! test file next to it. See DESIGN.md §3.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// What sort of thing is being practised. Determines which grader runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Hidden tests over a function under test.
    Kata,
    /// A pinned repro flips red to green while a guard set stays green.
    Debug,
    /// Learner query and reference query compared on one fixture.
    Sql,
    /// Assert observable system state.
    Terminal,
    /// Compare a produced file against a reference.
    Artifact,
}

/// How much of the solution is shown. Worked examples help novices and actively harm
/// knowledgeable learners, so this is chosen from the assessment, not by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Guidance {
    /// Full solution shown, learner follows along.
    Worked,
    /// Backward-faded: the last step or two are blank.
    Faded,
    /// No solution shown.
    Blank,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskMeta {
    pub id: String,
    /// Back-link into the prerequisite graph.
    pub concept_id: String,
    pub kind: Kind,
    pub guidance_level: Guidance,
    #[serde(default)]
    pub generated_by: Option<String>,
    #[serde(default)]
    pub generated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub setup_secs: u32,
    pub learner_secs: u32,
    pub check_secs: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self { setup_secs: 60, learner_secs: 900, check_secs: 120 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verify {
    /// The grader command. Its exit code reports grader health, not the grade.
    pub cmd: String,
    /// Path the grader writes, relative to the run's `out/` directory.
    #[serde(default = "default_reward_path")]
    pub reward: String,
    /// Dimensions that gate pass/fail. Every one must be 1.0.
    pub must_pass: Vec<String>,
    /// Judge dimensions. Reported, never gating.
    #[serde(default)]
    pub advisory: Vec<String>,
    /// When true the check directory holds cases the learner must not read. The gate
    /// keeps `check/` out of the learner's workspace, so this records intent.
    #[serde(default)]
    pub hidden: bool,
}

fn default_reward_path() -> String {
    "reward.json".to_string()
}

/// What the machine looked like when the gate ran.
///
/// Recorded as evidence, never as a gating condition. A gate result earned by a
/// different binary, or on a different platform, is weaker evidence than one earned
/// here - but refusing on it would ungate an entire library on a version bump, which
/// buys nothing and trains the reader to re-gate without looking.
///
/// This is a *fingerprint*, not a description of the environment. What actually
/// decides whether a grader still behaves - interpreter build, installed packages,
/// their versions - is not enumerable from the exercise directory, and pretending
/// otherwise here would be worse than the honest gap. Drift in those shows up as a
/// failing grade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Env {
    pub benkyou: String,
    pub os: String,
    pub arch: String,
}

impl Env {
    pub fn current() -> Self {
        Env {
            benkyou: env!("CARGO_PKG_VERSION").to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        }
    }

    /// One line per difference, for a caller that prints warnings.
    pub fn drift(&self, now: &Env) -> Vec<String> {
        let mut out = Vec::new();
        if self.benkyou != now.benkyou {
            out.push(format!(
                "gated by benkyou {}, running {}",
                self.benkyou, now.benkyou
            ));
        }
        if self.os != now.os || self.arch != now.arch {
            out.push(format!(
                "gated on {}/{}, running {}/{}",
                self.os, self.arch, now.os, now.arch
            ));
        }
        out
    }
}

/// The gate's verdict, stored beside the exercise rather than inside it.
///
/// This lives in `.gate.json`, not in `task.toml`, and the separation is what makes
/// the digest trustworthy. `task.toml` is authored input: the tool never rewrites it,
/// so it can be hashed exactly as the author left it - comments, formatting, and
/// sections this binary does not parse included. Writing the verdict back into the
/// file it was a verdict *about* would mean hashing a canonical form instead, and
/// arguing about which differences count.
///
/// It also makes gating non-destructive. Before this, gating a shared fixture in place
/// edited it; now the authored files are untouched and the derived record can be
/// deleted or ignored without loss.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    pub solution_passes: bool,
    pub empty_fails: bool,
    pub validated_at: String,
    /// What the gate looked at. Without this the verdict outlives its subject: a
    /// hidden case edited after gating would leave an exercise showable on the
    /// strength of a run that no longer describes it.
    pub digest: String,
    pub env: Env,
}

impl Gate {
    /// True when both directions held. A record that exists and says otherwise is a
    /// recorded *failure*, which is not the same as no record at all.
    pub fn holds(&self) -> bool {
        self.solution_passes && self.empty_fails
    }
}

/// Where the gate's verdict is kept, relative to the exercise directory.
pub const GATE_FILE: &str = ".gate.json";

/// Optional per-exercise workspace settings.
///
/// Nothing on the CLI path needs these. They exist for a front-end that offers a Run
/// button and therefore has to know how the learner's file is meant to be executed;
/// an exercise without the section simply has no such button.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    /// How to run whatever the learner currently has, with the workspace as the
    /// working directory. This is not the grader: it produces output to read, never a
    /// verdict, and the verdict still comes from `verify.cmd` alone.
    #[serde(default)]
    pub run_cmd: Option<String>,
}

impl Workspace {
    /// True when the section carries nothing. `gate` rewrites `task.toml` after
    /// validating, and without this every gated exercise would grow an empty
    /// `[workspace]` it never asked for.
    pub fn is_empty(&self) -> bool {
        self.run_cmd.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub schema_version: String,
    pub task: TaskMeta,
    #[serde(default)]
    pub limits: Limits,
    pub verify: Verify,
    /// Additive on top of schema 1: an exercise written before this existed parses
    /// and behaves exactly as it did, which is why `schema_version` does not move.
    /// Kept last because TOML tables must follow every plain value in the struct.
    #[serde(default, skip_serializing_if = "Workspace::is_empty")]
    pub workspace: Workspace,
}

/// Read the gate's verdict for an exercise, if one has been recorded.
///
/// A missing file is `None`, not an error: not having been gated is an ordinary state.
/// A file that will not parse *is* an error - something wrote it, and silently
/// treating corruption as "ungated" would hide the corruption.
pub fn read_gate(dir: &Path) -> Result<Option<Gate>, String> {
    let path = dir.join(GATE_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Record the gate's verdict beside the exercise, leaving authored files untouched.
pub fn write_gate(dir: &Path, gate: &Gate) -> Result<(), String> {
    let path = dir.join(GATE_FILE);
    let text = serde_json::to_string_pretty(gate).map_err(|e| e.to_string())?;
    std::fs::write(&path, text + "\n").map_err(|e| format!("{}: {e}", path.display()))
}

/// Refuse an exercise whose gate verdict is missing, negative, or no longer describes
/// the files on disk.
///
/// Three ways to fail, kept distinct because they need different things from the
/// reader. No record: run the gate. A record that says the exercise was *rejected*:
/// this is not an exercise, and re-running the gate will say so again. A record earned
/// by different bytes: run it again, because the stamp is about a file that no longer
/// exists.
pub fn require_current(dir: &Path) -> Result<(), String> {
    let gate = read_gate(dir)?.ok_or_else(|| {
        format!(
            "{}: not validated - run `benkyou gate` on it first",
            dir.display()
        )
    })?;
    if !gate.holds() {
        return Err(format!(
            "{}: the gate rejected this exercise (solution_passes={}, empty_fails={})",
            dir.display(),
            gate.solution_passes,
            gate.empty_fails
        ));
    }
    let actual = crate::digest::exercise_digest(dir)?;
    if actual != gate.digest {
        return Err(format!(
            "{}: changed since it was gated ({} on record, {} now) - run `benkyou gate` again",
            dir.display(),
            &gate.digest[..gate.digest.len().min(12)],
            &actual[..actual.len().min(12)],
        ));
    }
    Ok(())
}

/// Advisory notes about a validated exercise. Never a reason to refuse it.
///
/// Separate from [`require_current`] on purpose, mirroring the gate's own split
/// between an outcome and its warnings: the caller decides whether it has anywhere to
/// print these, and nothing changes if it does not.
pub fn gate_warnings(dir: &Path) -> Vec<String> {
    match read_gate(dir) {
        Ok(Some(gate)) => gate.env.drift(&Env::current()),
        _ => Vec::new(),
    }
}

/// What the grader wrote. Dimension name to score in `0.0..=1.0`, plus learner-facing
/// detail.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reward {
    #[serde(flatten)]
    pub dimensions: BTreeMap<String, f32>,
    #[serde(default)]
    pub detail: Option<String>,
}

impl Reward {
    /// Parse a grader's reward file. `detail` is lifted out of the dimension map so a
    /// grader can write both in one flat object.
    pub fn parse(s: &str) -> Result<Reward, String> {
        let raw: serde_json::Value =
            serde_json::from_str(s).map_err(|e| format!("reward is not valid JSON: {e}"))?;
        let obj = raw
            .as_object()
            .ok_or_else(|| "reward must be a JSON object".to_string())?;

        let mut dimensions = BTreeMap::new();
        let mut detail = None;
        for (k, v) in obj {
            if k == "detail" {
                detail = v.as_str().map(str::to_string);
                continue;
            }
            match v.as_f64() {
                Some(n) if n.is_finite() => {
                    dimensions.insert(k.clone(), n as f32);
                }
                Some(_) => return Err(format!("dimension `{k}` is not finite")),
                None => return Err(format!("dimension `{k}` is not a number")),
            }
        }
        Ok(Reward { dimensions, detail })
    }

    pub fn score(&self, dimension: &str) -> Option<f32> {
        self.dimensions.get(dimension).copied()
    }
}

/// The outcome of grading one attempt.
///
/// `CheckBroken` is deliberately distinct from `Fail`: conflating grader health with
/// the grade is the classic source of unreadable grading failures, and a learner who
/// cannot tell "you got it wrong" from "the grader crashed" stops trusting the tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Verdict {
    Pass,
    /// Gating dimensions that were not 1.0, with their scores.
    Fail(BTreeMap<String, f32>),
    /// The grader itself failed: non-zero exit, missing or unparseable reward file,
    /// or a gating dimension it never reported.
    CheckBroken(String),
    Timeout(u32),
}

impl Verdict {
    pub fn is_pass(&self) -> bool {
        matches!(self, Verdict::Pass)
    }
}

/// Decide a verdict from what the grader produced.
///
/// `exit_code` is the grader's own exit status, which reports whether it *ran*, not
/// whether the learner passed. Every dimension in `must_pass` has to be present and
/// exactly 1.0; a gating dimension the grader never wrote is a broken grader, not a
/// failure, because silently failing an attempt the grader never judged is worse than
/// admitting the grader is wrong.
pub fn grade(
    verify: &Verify,
    exit_code: Option<i32>,
    timed_out_after: Option<u32>,
    reward_text: Option<&str>,
) -> Verdict {
    if let Some(secs) = timed_out_after {
        return Verdict::Timeout(secs);
    }
    match exit_code {
        Some(0) => {}
        Some(code) => return Verdict::CheckBroken(format!("grader exited {code}")),
        None => return Verdict::CheckBroken("grader did not exit normally".into()),
    }

    let Some(text) = reward_text else {
        return Verdict::CheckBroken(format!("grader wrote no {}", verify.reward));
    };
    let reward = match Reward::parse(text) {
        Ok(r) => r,
        Err(e) => return Verdict::CheckBroken(e),
    };

    let mut failed = BTreeMap::new();
    for dim in &verify.must_pass {
        match reward.score(dim) {
            Some(score) if score >= 1.0 => {}
            Some(score) => {
                failed.insert(dim.clone(), score);
            }
            None => {
                return Verdict::CheckBroken(format!("grader never reported gating `{dim}`"))
            }
        }
    }

    if failed.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fail(failed)
    }
}

/// Why an exercise was rejected by the gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GateFailure {
    /// The reference solution did not pass. The exercise is unsolvable as written.
    SolutionFailed(Verdict),
    /// The untouched starting state passed. The checks assert nothing.
    ChecksVacuous(Verdict),
    /// The exercise changed while the gate was running, so neither run describes
    /// what is now on disk and there is nothing to certify.
    ContentChangedDuringGate { before: String, after: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GateOutcome {
    Validated(Gate),
    Rejected(GateFailure),
}

/// Decide the gate from the two runs.
///
/// Run 1 applies `solution/solve.sh` and must pass. Run 2 leaves `setup/` untouched
/// and must fail. Both are required: run 1 alone admits a vacuous check that passes on
/// an empty workspace, and run 2 alone admits an unsolvable exercise.
pub fn gate_outcome(solution: &Verdict, empty: &Verdict, at: &str, digest: &str) -> GateOutcome {
    if !solution.is_pass() {
        return GateOutcome::Rejected(GateFailure::SolutionFailed(solution.clone()));
    }
    // A broken grader on the empty run is not evidence the checks discriminate.
    if empty.is_pass() || matches!(empty, Verdict::CheckBroken(_)) {
        return GateOutcome::Rejected(GateFailure::ChecksVacuous(empty.clone()));
    }
    GateOutcome::Validated(Gate {
        solution_passes: true,
        empty_fails: true,
        validated_at: at.to_string(),
        digest: digest.to_string(),
        env: Env::current(),
    })
}

/// Load a `task.toml` from an exercise directory.
pub fn load(dir: &Path) -> Result<Task, String> {
    let path = dir.join("task.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verify(must_pass: &[&str]) -> Verify {
        Verify {
            cmd: "bash check.sh".into(),
            reward: "reward.json".into(),
            must_pass: must_pass.iter().map(|s| s.to_string()).collect(),
            advisory: vec!["approach".into()],
            hidden: true,
        }
    }

    #[test]
    fn reward_parses_dimensions_and_detail() {
        let r = Reward::parse(r#"{"correctness": 1.0, "approach": 0.5, "detail": "ok"}"#)
            .expect("parse");
        assert_eq!(r.score("correctness"), Some(1.0));
        assert_eq!(r.score("approach"), Some(0.5));
        assert_eq!(r.detail.as_deref(), Some("ok"));
        assert!(!r.dimensions.contains_key("detail"), "detail is not a dimension");
    }

    #[test]
    fn reward_rejects_non_numeric_and_non_finite() {
        assert!(Reward::parse(r#"{"correctness": "yes"}"#).is_err());
        assert!(Reward::parse("[1,2]").is_err());
        assert!(Reward::parse("not json").is_err());
    }

    #[test]
    fn passing_requires_every_gating_dimension() {
        let v = verify(&["correctness", "safety"]);
        let ok = r#"{"correctness": 1.0, "safety": 1.0, "approach": 0.2}"#;
        assert_eq!(grade(&v, Some(0), None, Some(ok)), Verdict::Pass);
    }

    #[test]
    fn advisory_dimensions_never_gate() {
        let v = verify(&["correctness"]);
        // approach is 0.0 and must not matter.
        let text = r#"{"correctness": 1.0, "approach": 0.0}"#;
        assert_eq!(grade(&v, Some(0), None, Some(text)), Verdict::Pass);
    }

    #[test]
    fn a_low_gating_dimension_fails_with_its_score() {
        let v = verify(&["correctness"]);
        let text = r#"{"correctness": 0.75}"#;
        match grade(&v, Some(0), None, Some(text)) {
            Verdict::Fail(dims) => assert_eq!(dims.get("correctness"), Some(&0.75)),
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    /// Grader health and the grade are different channels. A crashed grader must never
    /// be reported as a failed attempt.
    #[test]
    fn grader_health_is_not_the_grade() {
        let v = verify(&["correctness"]);
        assert!(matches!(
            grade(&v, Some(1), None, Some(r#"{"correctness": 1.0}"#)),
            Verdict::CheckBroken(_)
        ));
        assert!(matches!(grade(&v, Some(0), None, None), Verdict::CheckBroken(_)));
        assert!(matches!(
            grade(&v, Some(0), None, Some("garbage")),
            Verdict::CheckBroken(_)
        ));
        assert!(matches!(grade(&v, None, Some(120), None), Verdict::Timeout(120)));
    }

    /// A gating dimension the grader never wrote is a broken grader, not a failure.
    #[test]
    fn a_missing_gating_dimension_is_broken_not_failed() {
        let v = verify(&["correctness"]);
        let text = r#"{"approach": 1.0}"#;
        match grade(&v, Some(0), None, Some(text)) {
            Verdict::CheckBroken(msg) => assert!(msg.contains("correctness"), "{msg}"),
            other => panic!("expected CheckBroken, got {other:?}"),
        }
    }

    #[test]
    fn gate_requires_both_directions() {
        let fail = Verdict::Fail(BTreeMap::from([("correctness".into(), 0.0)]));

        // Both hold.
        assert!(matches!(
            gate_outcome(&Verdict::Pass, &fail, "t", "d"),
            GateOutcome::Validated(_)
        ));

        // Unsolvable as written.
        assert!(matches!(
            gate_outcome(&fail, &fail, "t", "d"),
            GateOutcome::Rejected(GateFailure::SolutionFailed(_))
        ));

        // Vacuous: the empty workspace already passes.
        assert!(matches!(
            gate_outcome(&Verdict::Pass, &Verdict::Pass, "t", "d"),
            GateOutcome::Rejected(GateFailure::ChecksVacuous(_))
        ));
    }

    /// A grader that breaks on the empty run proves nothing about discrimination,
    /// so it must not be mistaken for "the empty state failed".
    #[test]
    fn a_broken_grader_on_the_empty_run_does_not_validate() {
        let broken = Verdict::CheckBroken("boom".into());
        assert!(matches!(
            gate_outcome(&Verdict::Pass, &broken, "t", "d"),
            GateOutcome::Rejected(GateFailure::ChecksVacuous(_))
        ));
    }

    fn gate(solution_passes: bool, empty_fails: bool) -> Gate {
        Gate {
            solution_passes,
            empty_fails,
            validated_at: "t".into(),
            digest: "d".into(),
            env: Env::current(),
        }
    }

    #[test]
    fn one_direction_is_not_enough() {
        assert!(gate(true, true).holds());
        assert!(!gate(true, false).holds(), "the empty run never failed");
        assert!(!gate(false, true).holds(), "the solution never passed");
    }

    /// A recorded rejection and no record at all are different states, and the
    /// difference is what the reader needs: one says re-gate, the other says this is
    /// not an exercise.
    #[test]
    fn a_recorded_rejection_is_refused_distinctly() {
        let dir = std::env::temp_dir().join(format!("benkyou-gaterec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("task.toml"), "schema_version = \"1\"\n").expect("write");

        let missing = require_current(&dir).expect_err("no record must refuse");
        assert!(missing.contains("not validated"), "{missing}");

        write_gate(&dir, &gate(true, false)).expect("write gate");
        let rejected = require_current(&dir).expect_err("a recorded failure must refuse");
        assert!(rejected.contains("rejected this exercise"), "{rejected}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn task_toml_round_trips() {
        let text = r#"
schema_version = "1"

[task]
id = "pandas-groupby-01"
concept_id = "pandas_groupby"
kind = "kata"
guidance_level = "blank"

[verify]
cmd = "bash check/check.sh"
must_pass = ["correctness"]
advisory = ["approach"]
hidden = true
"#;
        let task: Task = toml::from_str(text).expect("parse task.toml");
        assert_eq!(task.task.kind, Kind::Kata);
        assert_eq!(task.task.guidance_level, Guidance::Blank);
        assert_eq!(task.verify.reward, "reward.json", "default applies");
        assert_eq!(task.limits.learner_secs, 900, "default applies");
        assert_eq!(task.workspace.run_cmd, None, "no [workspace] means no run command");
    }

    /// The `[workspace]` section is additive. Every task.toml written before it
    /// existed must still load, and must not grow an empty section when the gate
    /// writes the file back.
    #[test]
    fn a_task_toml_without_a_workspace_section_is_unchanged() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/exercises/dedupe");
        let task = load(&dir).expect("the existing fixture still parses");

        assert_eq!(task.schema_version, "1", "the schema version did not move");
        assert_eq!(task.task.concept_id, "python_sets_and_order");
        assert!(task.workspace.is_empty());

        let back = toml::to_string_pretty(&task).expect("serialize");
        assert!(!back.contains("[workspace]"), "an absent section came back:\n{back}");
    }

    #[test]
    fn a_workspace_run_cmd_round_trips() {
        let text = r#"
schema_version = "1"

[task]
id = "dedupe-01"
concept_id = "python_sets_and_order"
kind = "kata"
guidance_level = "blank"

[verify]
cmd = "sh check/check.sh"
must_pass = ["correctness"]

[workspace]
run_cmd = "uv run --no-project solution.py"
"#;
        let task: Task = toml::from_str(text).expect("parse task.toml");
        assert_eq!(
            task.workspace.run_cmd.as_deref(),
            Some("uv run --no-project solution.py")
        );

        let back = toml::to_string_pretty(&task).expect("serialize");
        let again: Task = toml::from_str(&back).expect("reparse");
        assert_eq!(again, task, "a workspace did not survive the trip through TOML");
    }
}
