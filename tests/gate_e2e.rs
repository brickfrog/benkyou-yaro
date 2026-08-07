//! The gate, end to end, against a real exercise.
//!
//! This is the test that proves the thesis: a generated exercise is only real if the
//! reference solution passes it and the untouched starting state fails it.

use std::fs;
use std::path::PathBuf;

use benkyou::exercise::{GateFailure, GateOutcome, Verdict};
use benkyou::gate::run_gate;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/exercises")
        .join(name)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("benkyou-gate-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch");
    dir.canonicalize().expect("canonicalize")
}

/// A well-formed exercise passes both directions.
#[test]
fn a_sound_exercise_is_validated() {
    let report = run_gate(&fixture("dedupe"), &scratch("sound"), "2026-08-05T00:00:00Z")
        .expect("gate ran");

    assert!(
        matches!(report.outcome, GateOutcome::Validated(_)),
        "outcome: {:?}\nsolution verdict: {:?}\nempty verdict: {:?}\ncheck stderr: {}",
        report.outcome,
        report.solution.verdict,
        report.empty.verdict,
        report.solution.check_stderr
    );

    assert!(report.solution.verdict.is_pass(), "reference solution must pass");
    assert!(
        matches!(report.empty.verdict, Verdict::Fail(_)),
        "the untouched stub must fail, got {:?}",
        report.empty.verdict
    );
}

/// A check that passes on an empty workspace asserts nothing, and must be rejected
/// even though the reference solution also passes it.
#[test]
fn a_vacuous_check_is_rejected() {
    let src = fixture("dedupe");
    let dir = scratch("vacuous-src").join("dedupe");
    fs::create_dir_all(&dir).expect("dir");
    for sub in ["setup", "solution", "check"] {
        let to = dir.join(sub);
        fs::create_dir_all(&to).expect("dir");
        for e in fs::read_dir(src.join(sub)).expect("read") {
            let e = e.expect("entry");
            fs::copy(e.path(), to.join(e.file_name())).expect("copy");
        }
    }
    fs::copy(src.join("task.toml"), dir.join("task.toml")).expect("copy");

    // Replace the grader with one that always awards full marks.
    fs::write(
        dir.join("check/check.sh"),
        "#!/bin/sh\nmkdir -p out\nprintf '{\"correctness\": 1.0}' > out/reward.json\nexit 0\n",
    )
    .expect("write");

    let report =
        run_gate(&dir, &scratch("vacuous"), "2026-08-05T00:00:00Z").expect("gate ran");

    match report.outcome {
        GateOutcome::Rejected(GateFailure::ChecksVacuous(_)) => {}
        other => panic!("expected ChecksVacuous, got {other:?}"),
    }
}

/// An exercise whose reference solution does not pass its own checks is unsolvable
/// as written, and must be rejected before a learner ever sees it.
#[test]
fn an_unsolvable_exercise_is_rejected() {
    let src = fixture("dedupe");
    let dir = scratch("unsolvable-src").join("dedupe");
    fs::create_dir_all(&dir).expect("dir");
    for sub in ["setup", "solution", "check"] {
        let to = dir.join(sub);
        fs::create_dir_all(&to).expect("dir");
        for e in fs::read_dir(src.join(sub)).expect("read") {
            let e = e.expect("entry");
            fs::copy(e.path(), to.join(e.file_name())).expect("copy");
        }
    }
    fs::copy(src.join("task.toml"), dir.join("task.toml")).expect("copy");

    // A reference solution that does not satisfy the hidden cases.
    fs::write(
        dir.join("solution/solve.sh"),
        "#!/bin/sh\nset -e\ncat > solution.py <<'PY'\ndef dedupe(xs):\n    return sorted(set(xs))\nPY\n",
    )
    .expect("write");

    let report =
        run_gate(&dir, &scratch("unsolvable"), "2026-08-05T00:00:00Z").expect("gate ran");

    match report.outcome {
        GateOutcome::Rejected(GateFailure::SolutionFailed(_)) => {}
        other => panic!("expected SolutionFailed, got {other:?}"),
    }
}

/// The learner-facing detail must survive from the grader to the verdict, because
/// "wrong on these inputs" teaches and "3/7 failed" does not.
#[test]
fn the_graders_detail_reaches_the_caller() {
    let report = run_gate(&fixture("dedupe"), &scratch("detail"), "t").expect("gate ran");
    let reward = fs::read_to_string(report.empty.root.join("out/reward.json"))
        .expect("empty run wrote a reward file");
    assert!(
        reward.contains("NotImplementedError"),
        "expected the stub's failure to be reported: {reward}"
    );
}
