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

/// Filled in by the validation gate. An exercise without this is never shown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    pub solution_passes: bool,
    pub empty_fails: bool,
    pub validated_at: String,
}

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
    #[serde(default)]
    pub gate: Option<Gate>,
    /// Additive on top of schema 1: an exercise written before this existed parses
    /// and behaves exactly as it did, which is why `schema_version` does not move.
    /// Kept last because TOML tables must follow every plain value in the struct.
    #[serde(default, skip_serializing_if = "Workspace::is_empty")]
    pub workspace: Workspace,
}

impl Task {
    /// True when the gate has run and both directions held. Only these are shown.
    pub fn is_validated(&self) -> bool {
        matches!(&self.gate, Some(g) if g.solution_passes && g.empty_fails)
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
pub fn gate_outcome(solution: &Verdict, empty: &Verdict, at: &str) -> GateOutcome {
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
            gate_outcome(&Verdict::Pass, &fail, "t"),
            GateOutcome::Validated(_)
        ));

        // Unsolvable as written.
        assert!(matches!(
            gate_outcome(&fail, &fail, "t"),
            GateOutcome::Rejected(GateFailure::SolutionFailed(_))
        ));

        // Vacuous: the empty workspace already passes.
        assert!(matches!(
            gate_outcome(&Verdict::Pass, &Verdict::Pass, "t"),
            GateOutcome::Rejected(GateFailure::ChecksVacuous(_))
        ));
    }

    /// A grader that breaks on the empty run proves nothing about discrimination,
    /// so it must not be mistaken for "the empty state failed".
    #[test]
    fn a_broken_grader_on_the_empty_run_does_not_validate() {
        let broken = Verdict::CheckBroken("boom".into());
        assert!(matches!(
            gate_outcome(&Verdict::Pass, &broken, "t"),
            GateOutcome::Rejected(GateFailure::ChecksVacuous(_))
        ));
    }

    #[test]
    fn an_ungated_task_is_never_validated() {
        let task = Task {
            schema_version: "1".into(),
            task: TaskMeta {
                id: "t".into(),
                concept_id: "c".into(),
                kind: Kind::Kata,
                guidance_level: Guidance::Blank,
                generated_by: None,
                generated_at: None,
            },
            limits: Limits::default(),
            verify: verify(&["correctness"]),
            gate: None,
            workspace: Workspace::default(),
        };
        assert!(!task.is_validated());

        let half = Task {
            gate: Some(Gate {
                solution_passes: true,
                empty_fails: false,
                validated_at: "t".into(),
            }),
            ..task.clone()
        };
        assert!(!half.is_validated(), "one direction is not enough");
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
        assert!(!task.is_validated(), "a fresh task has not been gated");
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
