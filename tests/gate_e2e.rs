//! The gate, end to end, against a real exercise.
//!
//! A generated exercise is sound only if the reference solution passes it and the
//! untouched starting state fails it. Almost every test here runs on the host backend,
//! because isolation is not the subject. The one test that asks what isolation prevents
//! takes the sandbox.

use std::fs;
use std::path::PathBuf;

use benkyou::exercise::{self, GateFailure, GateOutcome, Verdict};
use benkyou::gate::run_gate;

mod support;

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
    let report = run_gate(
        &fixture("dedupe"),
        &scratch("sound"),
        "2026-08-05T00:00:00Z",
        &support::behaviour(),
    )
    .expect("gate ran");

    assert!(
        matches!(report.outcome, GateOutcome::Validated(_)),
        "outcome: {:?}\nsolution verdict: {:?}\nempty verdict: {:?}\ncheck stderr: {}",
        report.outcome,
        report.solution.verdict,
        report.empty.verdict,
        report.solution.check_stderr
    );

    assert!(
        report.solution.verdict.is_pass(),
        "reference solution must pass"
    );
    assert!(
        matches!(report.empty.verdict, Verdict::Fail(_)),
        "the untouched stub must fail, got {:?}",
        report.empty.verdict
    );
}

/// A check that passes on an empty workspace asserts nothing. It is rejected even though
/// the reference solution also passes it.
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

    let report = run_gate(
        &dir,
        &scratch("vacuous"),
        "2026-08-05T00:00:00Z",
        &support::behaviour(),
    )
    .expect("gate ran");

    match report.outcome {
        GateOutcome::Rejected(GateFailure::ChecksVacuous(_)) => {}
        other => panic!("expected ChecksVacuous, got {other:?}"),
    }
}

/// An exercise whose reference solution fails its own checks is unsolvable as written.
/// It is rejected before a learner sees it.
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

    let report = run_gate(
        &dir,
        &scratch("unsolvable"),
        "2026-08-05T00:00:00Z",
        &support::behaviour(),
    )
    .expect("gate ran");

    match report.outcome {
        GateOutcome::Rejected(GateFailure::SolutionFailed(_)) => {}
        other => panic!("expected SolutionFailed, got {other:?}"),
    }
}

/// The grader's detail must survive to the verdict. "Wrong on these inputs" teaches and
/// "3/7 failed" does not.
#[test]
fn the_graders_detail_reaches_the_caller() {
    let report = run_gate(
        &fixture("dedupe"),
        &scratch("detail"),
        "t",
        &support::behaviour(),
    )
    .expect("gate ran");
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
/// Without this positive case, every negative case below is satisfied by a
/// `require_current` that refuses everything.
#[test]
fn gating_records_the_content_and_the_exercise_becomes_showable() {
    let dir = copy_fixture("stamp");
    let report =
        run_gate(&dir, &scratch("stamp-run"), "t", &support::behaviour()).expect("gate ran");
    let gate = match report.outcome {
        GateOutcome::Validated(g) => g,
        other => panic!("expected Validated, got {other:?}"),
    };
    exercise::write_gate(&dir, &gate).expect("write gate");

    assert_eq!(gate.digest.len(), 64, "a sha-256 hex digest");
    assert_eq!(gate.env, exercise::Env::current());
    exercise::require_current(&dir, &support::behaviour())
        .expect("a freshly gated exercise must be showable");

    // The authored files are untouched: the verdict went to the sidecar.
    let pristine = fs::read_to_string(fixture("dedupe").join("task.toml")).expect("read");
    let ours = fs::read_to_string(dir.join("task.toml")).expect("read");
    assert_eq!(ours, pristine, "gating rewrote task.toml");
    assert!(dir.join(".gate.json").is_file(), "no sidecar was written");
}

/// Every part of an exercise that decides what a learner sees, or how they are graded,
/// must ungate it when it changes.
#[test]
fn editing_any_content_after_gating_ungates_the_exercise() {
    // (name, path to touch, new content)
    let edits: [(&str, &str, &str); 5] = [
        ("a hidden case", "check/cases.py", "# rewritten\n"),
        ("the grader", "check/check.sh", "#!/bin/sh\nexit 0\n"),
        ("the starting stub", "setup/solution.py", "# rewritten\n"),
        (
            "the reference solution",
            "solution/solve.sh",
            "#!/bin/sh\nexit 0\n",
        ),
        ("the prose", "instruction.md", "# a different question\n"),
    ];

    for (i, (what, rel, content)) in edits.into_iter().enumerate() {
        let dir = copy_fixture(&format!("edit-{i}"));
        let report = run_gate(
            &dir,
            &scratch(&format!("edit-run-{i}")),
            "t",
            &support::behaviour(),
        )
        .expect("gate ran");
        let gate = match report.outcome {
            GateOutcome::Validated(g) => g,
            other => panic!("{what}: expected Validated, got {other:?}"),
        };
        exercise::write_gate(&dir, &gate).expect("write gate");
        exercise::require_current(&dir, &support::behaviour()).expect("showable before the edit");

        fs::write(dir.join(rel), content).expect("edit");
        let err = exercise::require_current(&dir, &support::behaviour())
            .expect_err(&format!("editing {what} left the exercise showable"));
        assert!(err.contains("changed since it was gated"), "{what}: {err}");
    }
}

/// `task.toml` is hashed byte for byte, so a limit or a grader command that moved after
/// gating ungates too. So does a section this binary does not parse.
#[test]
fn editing_the_task_file_ungates_the_exercise() {
    for (i, addition) in ["\nlearner_secs = 5\n", "\n[[negative]]\nid = \"future\"\n"]
        .into_iter()
        .enumerate()
    {
        let dir = copy_fixture(&format!("task-edit-{i}"));
        let report = run_gate(
            &dir,
            &scratch(&format!("task-edit-run-{i}")),
            "t",
            &support::behaviour(),
        )
        .expect("gate ran");
        let gate = match report.outcome {
            GateOutcome::Validated(g) => g,
            other => panic!("expected Validated, got {other:?}"),
        };
        exercise::write_gate(&dir, &gate).expect("write gate");

        let path = dir.join("task.toml");
        let mut text = fs::read_to_string(&path).expect("read");
        text.push_str(addition);
        fs::write(&path, text).expect("write");

        let err = exercise::require_current(&dir, &support::behaviour())
            .expect_err("task.toml edit was not noticed");
        assert!(err.contains("changed since it was gated"), "{err}");
    }
}

/// Rewriting a file with the bytes it already had is not a change. Otherwise the digest
/// is a modification-time check in disguise.
#[test]
fn rewriting_a_file_unchanged_keeps_the_exercise_showable() {
    let dir = copy_fixture("touch");
    let report =
        run_gate(&dir, &scratch("touch-run"), "t", &support::behaviour()).expect("gate ran");
    let gate = match report.outcome {
        GateOutcome::Validated(g) => g,
        other => panic!("expected Validated, got {other:?}"),
    };
    exercise::write_gate(&dir, &gate).expect("write gate");

    let path = dir.join("check/cases.py");
    let same = fs::read(&path).expect("read");
    fs::write(&path, same).expect("rewrite");

    exercise::require_current(&dir, &support::behaviour())
        .expect("an identical rewrite must not ungate");
}

/// An exercise that moves while its own gate runs cannot be certified. The runs describe
/// a snapshot, and the verdict lands next to a directory that is no longer that snapshot.
///
/// The case here is an editor saving mid-gate. The earlier case, a check script writing
/// back into its source directory, is now impossible under the sandbox.
#[test]
fn an_exercise_that_changes_mid_gate_is_rejected() {
    let dir = copy_fixture("midgate");
    let check = dir.join("check/check.sh");
    let body = fs::read_to_string(&check).expect("read");
    fs::write(&check, body.replace("#!/bin/sh", "#!/bin/sh\nsleep 2")).expect("write");

    let editing = dir.clone();
    let editor = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        let path = editing.join("instruction.md");
        let mut text = fs::read_to_string(&path).unwrap_or_default();
        text.push_str("\n\nan edit the author made while the gate was running\n");
        fs::write(&path, text).expect("edit mid-gate");
    });

    let report =
        run_gate(&dir, &scratch("midgate-run"), "t", &support::behaviour()).expect("gate ran");
    editor.join().expect("editor");

    match report.outcome {
        GateOutcome::Rejected(GateFailure::ContentChangedDuringGate { before, after }) => {
            assert_ne!(before, after);
        }
        other => panic!("expected ContentChangedDuringGate, got {other:?}"),
    }
}

/// A grader cannot reach the exercise directory it was copied from. Before the sandbox
/// the digest reported this only after the fact.
#[test]
fn a_grader_cannot_reach_the_exercise_directory() {
    let Some(backend) = support::sandbox() else {
        return;
    };
    let dir = copy_fixture("noreach");
    let marker = dir.join("check/scribble.txt");
    fs::write(
        dir.join("check/check.sh"),
        format!(
            "#!/bin/sh\nmkdir -p out\ndate >> {}\n\
             printf '{{\"correctness\": 1.0}}' > out/reward.json\nexit 0\n",
            marker.display()
        ),
    )
    .expect("write");

    run_gate(&dir, &scratch("noreach-run"), "t", &backend).expect("gate ran");
    assert!(
        !marker.exists(),
        "a grader wrote into its own source directory"
    );
}

// ---------------------------------------------------------------------------
// Named wrong answers
// ---------------------------------------------------------------------------

/// The failure the feature exists for, end to end.
///
/// A model that misreads the task writes a reference solution and hidden cases that
/// agree with each other, so both gate directions hold. The author's named wrong answer
/// is the one artifact that disagrees.
#[test]
fn a_grader_that_misread_the_concept_is_rejected_by_its_own_trap() {
    let dir = copy_fixture("drift");

    // The drifted grader: expects sorted output, and no longer minds mutation.
    let cases = dir.join("check/cases.py");
    let mut text = fs::read_to_string(&cases).expect("read");
    text = text
        .replace(
            "([3, 1, 3, 2, 1], [3, 1, 2]),",
            "([3, 1, 3, 2, 1], [1, 2, 3]),",
        )
        .replace(
            r#"(["b", "a", "b"], ["b", "a"]),"#,
            r#"(["b", "a", "b"], ["a", "b"]),"#,
        );
    let mutation =
        "    if xs != original:\n        failures.append(f\"dedupe mutated its input: {xs!r}\")\n";
    text = text.replace(mutation, "");
    fs::write(&cases, text).expect("write");

    // ...and a reference solution that agrees with it, so direction one still passes.
    fs::write(
        dir.join("solution/solve.sh"),
        "cat > solution.py <<'PY'\ndef dedupe(xs):\n    return sorted(set(xs))\nPY\n",
    )
    .expect("write");

    let report =
        run_gate(&dir, &scratch("drift-run"), "t", &support::behaviour()).expect("gate ran");

    assert!(
        report.solution.verdict.is_pass(),
        "direction one still holds"
    );
    assert!(
        matches!(report.empty.verdict, Verdict::Fail(_)),
        "direction two still holds: {:?}",
        report.empty.verdict
    );
    match report.outcome {
        GateOutcome::Rejected(GateFailure::KnownBadPassed { id, trap }) => {
            assert_eq!(id, "sorted_not_first_seen");
            assert!(trap.contains("first appearance"), "{trap}");
        }
        other => panic!("both directions passed and nothing caught the drift: {other:?}"),
    }
}

/// An exercise that names no wrong answer cannot be shown, however sound its two
/// directions look.
#[test]
fn an_exercise_with_no_named_wrong_answer_is_rejected() {
    let dir = copy_fixture("notrap");
    let path = dir.join("task.toml");
    let text = fs::read_to_string(&path).expect("read");
    let stripped = text
        .split("[[known_bad]]")
        .next()
        .expect("head")
        .to_string();
    fs::write(&path, stripped).expect("write");

    let report =
        run_gate(&dir, &scratch("notrap-run"), "t", &support::behaviour()).expect("gate ran");
    assert!(matches!(
        report.outcome,
        GateOutcome::Rejected(GateFailure::NoKnownBad)
    ));
    exercise::require_current(&dir, &support::behaviour())
        .expect_err("and it must not be showable");
}

/// Each candidate gets a fresh workspace. A shared one lets the first candidate's files
/// survive into the second, so a trap can spring on residue instead of on the answer.
#[test]
fn each_candidate_runs_in_its_own_workspace() {
    let dir = copy_fixture("fresh");
    let path = dir.join("task.toml");
    let mut text = fs::read_to_string(&path).expect("read");
    // A candidate that writes a correct solution plus a second file. A leaked workspace
    // hands that file to the next candidate.
    text.push_str(
        "\n[[known_bad]]\nid = \"leaves_litter\"\ntrap = \"writes an unrelated file\"\n\
         files.\"solution.py\" = \"\"\"\ndef dedupe(xs):\n    return list(xs)\n\"\"\"\n\
         files.\"litter.txt\" = \"residue\"\n",
    );
    fs::write(&path, text).expect("write");

    let report =
        run_gate(&dir, &scratch("fresh-run"), "t", &support::behaviour()).expect("gate ran");
    assert!(
        matches!(report.outcome, GateOutcome::Validated(_)),
        "{:?}",
        report.outcome
    );
    assert_eq!(report.known_bad.len(), 3);
    for (candidate, run) in &report.known_bad {
        assert!(
            matches!(run.verdict, Verdict::Fail(_)),
            "{} should have failed, got {:?}",
            candidate.id,
            run.verdict
        );
        // Only the candidate that writes it has it.
        let has_litter = run.root.join("work/litter.txt").exists();
        assert_eq!(
            has_litter,
            candidate.id == "leaves_litter",
            "{}: workspace leaked between candidates",
            candidate.id
        );
    }
}

/// A candidate cannot write outside the workspace. A generated `task.toml` naming
/// `../check/cases.py` otherwise rewrites the tests it is judged by.
#[test]
fn a_candidate_cannot_write_outside_the_workspace() {
    let dir = copy_fixture("escape");
    let path = dir.join("task.toml");
    let mut text = fs::read_to_string(&path).expect("read");
    text.push_str(
        "\n[[known_bad]]\nid = \"escapes\"\ntrap = \"rewrites the grader\"\n\
         files.\"../check/cases.py\" = \"import sys; sys.exit(0)\"\n",
    );
    fs::write(&path, text).expect("write");

    let err = match run_gate(&dir, &scratch("escape-run"), "t", &support::behaviour()) {
        Err(e) => e,
        Ok(_) => panic!("a candidate escaping the workspace must be an error"),
    };
    assert!(err.contains("escapes"), "{err}");
    assert!(err.contains("inside the workspace"), "{err}");
}
