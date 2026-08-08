//! The gate, end to end, against a real exercise.
//!
//! This is the test that proves the thesis: a generated exercise is only real if the
//! reference solution passes it and the untouched starting state fails it.

use std::fs;
use std::path::PathBuf;

use benkyou::exercise::{self, GateFailure, GateOutcome, Verdict};
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

/// A working copy of the fixture that can be edited without touching the repo's.
fn copy_fixture(name: &str) -> PathBuf {
    let src = fixture("dedupe");
    let dir = scratch(name).join("dedupe");
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
    fs::copy(src.join("instruction.md"), dir.join("instruction.md")).expect("copy");
    dir
}

/// Gating records what it ran against, and the exercise is showable afterwards.
///
/// The positive case has to be asserted here or every negative case below is
/// satisfied by a `require_current` that refuses everything.
#[test]
fn gating_records_the_content_and_the_exercise_becomes_showable() {
    let dir = copy_fixture("stamp");
    let report = run_gate(&dir, &scratch("stamp-run"), "t").expect("gate ran");
    let gate = match report.outcome {
        GateOutcome::Validated(g) => g,
        other => panic!("expected Validated, got {other:?}"),
    };
    exercise::write_gate(&dir, &gate).expect("write gate");

    assert_eq!(gate.digest.len(), 64, "a sha-256 hex digest");
    assert_eq!(gate.env, exercise::Env::current());
    exercise::require_current(&dir).expect("a freshly gated exercise must be showable");

    // The authored files are untouched: the verdict went to the sidecar.
    let pristine = fs::read_to_string(fixture("dedupe").join("task.toml")).expect("read");
    let ours = fs::read_to_string(dir.join("task.toml")).expect("read");
    assert_eq!(ours, pristine, "gating rewrote task.toml");
    assert!(dir.join(".gate.json").is_file(), "no sidecar was written");
}

/// The property the digest exists for: every part of an exercise that decides what a
/// learner sees or how they are graded must ungate it when it changes.
#[test]
fn editing_any_content_after_gating_ungates_the_exercise() {
    // (name, path to touch, new content)
    let edits: [(&str, &str, &str); 5] = [
        ("a hidden case", "check/cases.py", "# rewritten\n"),
        ("the grader", "check/check.sh", "#!/bin/sh\nexit 0\n"),
        ("the starting stub", "setup/solution.py", "# rewritten\n"),
        ("the reference solution", "solution/solve.sh", "#!/bin/sh\nexit 0\n"),
        ("the prose", "instruction.md", "# a different question\n"),
    ];

    for (i, (what, rel, content)) in edits.into_iter().enumerate() {
        let dir = copy_fixture(&format!("edit-{i}"));
        let report = run_gate(&dir, &scratch(&format!("edit-run-{i}")), "t").expect("gate ran");
        let gate = match report.outcome {
            GateOutcome::Validated(g) => g,
            other => panic!("{what}: expected Validated, got {other:?}"),
        };
        exercise::write_gate(&dir, &gate).expect("write gate");
        exercise::require_current(&dir).expect("showable before the edit");

        fs::write(dir.join(rel), content).expect("edit");
        let err = exercise::require_current(&dir)
            .expect_err(&format!("editing {what} left the exercise showable"));
        assert!(err.contains("changed since it was gated"), "{what}: {err}");
    }
}

/// `task.toml` is hashed byte for byte, so a limit or a grader command that moved
/// after gating ungates too - and so does a section this binary does not parse.
#[test]
fn editing_the_task_file_ungates_the_exercise() {
    for (i, addition) in ["\nlearner_secs = 5\n", "\n[[negative]]\nid = \"future\"\n"]
        .into_iter()
        .enumerate()
    {
        let dir = copy_fixture(&format!("task-edit-{i}"));
        let report =
            run_gate(&dir, &scratch(&format!("task-edit-run-{i}")), "t").expect("gate ran");
        let gate = match report.outcome {
            GateOutcome::Validated(g) => g,
            other => panic!("expected Validated, got {other:?}"),
        };
        exercise::write_gate(&dir, &gate).expect("write gate");

        let path = dir.join("task.toml");
        let mut text = fs::read_to_string(&path).expect("read");
        text.push_str(addition);
        fs::write(&path, text).expect("write");

        let err = exercise::require_current(&dir).expect_err("task.toml edit was not noticed");
        assert!(err.contains("changed since it was gated"), "{err}");
    }
}

/// Rewriting a file with the bytes it already had is not a change. Without this the
/// digest would be a modification-time check wearing a hash for a hat.
#[test]
fn rewriting_a_file_unchanged_keeps_the_exercise_showable() {
    let dir = copy_fixture("touch");
    let report = run_gate(&dir, &scratch("touch-run"), "t").expect("gate ran");
    let gate = match report.outcome {
        GateOutcome::Validated(g) => g,
        other => panic!("expected Validated, got {other:?}"),
    };
    exercise::write_gate(&dir, &gate).expect("write gate");

    let path = dir.join("check/cases.py");
    let same = fs::read(&path).expect("read");
    fs::write(&path, same).expect("rewrite");

    exercise::require_current(&dir).expect("an identical rewrite must not ungate");
}

/// An exercise that moves while its own gate is running cannot be certified: neither
/// run describes what is now on disk. The grader edits the directory it was copied
/// from, which is the realistic version of an editor left open.
#[test]
fn an_exercise_that_changes_mid_gate_is_rejected() {
    let dir = copy_fixture("midgate");
    let marker = dir.join("check/scribble.txt");
    fs::write(
        dir.join("check/check.sh"),
        format!(
            "#!/bin/sh\nmkdir -p out\ndate >> {}\nprintf '{{\"correctness\": 1.0}}' > out/reward.json\nexit 0\n",
            marker.display()
        ),
    )
    .expect("write");

    let report = run_gate(&dir, &scratch("midgate-run"), "t").expect("gate ran");
    match report.outcome {
        GateOutcome::Rejected(GateFailure::ContentChangedDuringGate { before, after }) => {
            assert_ne!(before, after);
        }
        other => panic!("expected ContentChangedDuringGate, got {other:?}"),
    }
}
