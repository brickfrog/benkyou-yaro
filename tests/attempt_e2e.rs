//! The learner loop, end to end, against a real exercise.
//!
//! Two properties carry this module. An ungated exercise must not reach a learner
//! through *either* entry point, and grading a hidden exercise must not leave the
//! hidden cases sitting next to the learner's work — a checker copied beside their
//! answer hands over the reference on the first grade.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use benkyou::attempt::{self, practice_score};
use benkyou::exercise::{self, Verdict};
use benkyou::run::{Backend, Want};

/// Copy the authored files of an exercise, and only those.
///
/// `.gate.json` is skipped, and that is not tidiness: it is derived, it is gitignored,
/// and anyone who runs `benkyou gate` on the fixture in place leaves one behind — which
/// the tool documents as safe. Copying it made the ungated case arrive pre-gated, so a
/// developer who had ever gated the fixture saw this suite fail for a reason that had
/// nothing to do with their change.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("mkdir");
    for entry in fs::read_dir(from).expect("readdir") {
        let entry = entry.expect("entry");
        if entry.file_name() == ".gate.json" {
            continue;
        }
        let dst = to.join(entry.file_name());
        if entry.metadata().expect("meta").is_dir() {
            copy_tree(&entry.path(), &dst);
        } else {
            fs::copy(entry.path(), &dst).expect("copy");
        }
    }
}

/// A copy of the `dedupe` fixture with the gate's verdict recorded, as `benkyou gate`
/// would leave it. `hidden` controls which grading path is taken.
///
/// The digest is computed rather than invented, because a hand-written one is exactly
/// what `require_current` exists to reject. The verdict lands in the sidecar, so
/// `task.toml` here is only ever written by the edit above it.
/// Every test runs under the backend a user gets by default. Proving containment
/// against anything else would prove nothing.
fn sandbox() -> Backend {
    Backend::choose(Want::Auto, None).expect("a sandbox: install bubblewrap")
}

fn exercise_dir(name: &str, gated: bool, hidden: bool) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/exercises/dedupe");
    let dir = std::env::temp_dir().join(format!("benkyou-attempt-{name}"));
    let _ = fs::remove_dir_all(&dir);
    copy_tree(&src, &dir);

    let path = dir.join("task.toml");
    let mut text = fs::read_to_string(&path).expect("task.toml");
    if !hidden {
        text = text.replace("hidden = true", "hidden = false");
    }
    fs::write(&path, &text).expect("write task.toml");

    if gated {
        let digest = benkyou::digest::exercise_digest(&dir).expect("digest");
        exercise::write_gate(
            &dir,
            &exercise::Gate {
                solution_passes: true,
                empty_fails: true,
                validated_at: "test".into(),
                digest,
                known_bad_caught: vec!["trap".into()],
                runner: exercise::Runner::of(&sandbox()),
                env: exercise::Env::current(),
                deps: vec![],
            },
        )
        .expect("write gate");
    }
    dir
}

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("benkyou-work-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// The rule the whole exercise half rests on, at both doors. `grade` matters most:
/// a grader nobody proved discriminating would still write a score into fluency.
#[test]
fn an_ungated_exercise_is_refused_by_both_entry_points() {
    let dir = exercise_dir("ungated", false, true);
    let root = workspace("ungated");
    assert!(exercise::read_gate(&dir).expect("read gate").is_none());

    let opened = attempt::open(&dir, &root, &sandbox());
    assert!(opened.is_err(), "ungated exercise was opened");

    // Reaching `grade` without `open` is not a way around it: hand-make the
    // workspace and it must still refuse.
    fs::create_dir_all(root.join("work")).expect("mkdir");
    let task = exercise::load(&dir).expect("load");
    let graded = attempt::grade(&dir, &task, &root, &sandbox());
    assert!(graded.is_err(), "ungated exercise was graded");
}

/// Grading must not turn the hidden cases into files the learner can read.
#[test]
fn hidden_grading_leaves_no_checker_beside_the_learners_work() {
    let dir = exercise_dir("hidden", true, true);
    let root = workspace("hidden");
    let task = exercise::load(&dir).expect("load");

    attempt::open(&dir, &root, &sandbox()).expect("open");
    let report = attempt::grade(&dir, &task, &root, &sandbox()).expect("grade");

    // The grade itself still happened and still says something.
    assert!(report.reward.is_some(), "no reward file was read back");

    assert!(!root.join("check").exists(), "the checker was left in the learner's directory");
    assert!(!root.join("out").exists(), "the run output was left behind");
    let left: Vec<_> = fs::read_dir(&root)
        .expect("readdir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(left, vec!["work"], "grading left more than the workspace behind");
}

/// The other half of the same flag: an exercise that is not hiding anything leaves
/// its run in place, because inspecting it is the point.
#[test]
fn a_visible_exercise_leaves_its_run_for_inspection() {
    let dir = exercise_dir("visible", true, false);
    let root = workspace("visible");
    let task = exercise::load(&dir).expect("load");

    attempt::open(&dir, &root, &sandbox()).expect("open");
    attempt::grade(&dir, &task, &root, &sandbox()).expect("grade");

    assert!(root.join("check").exists(), "a visible exercise should leave its checker");
    assert!(root.join("out").exists(), "a visible exercise should leave its output");
}

/// The answer must not be readable out of the directory the learner works in.
#[test]
fn the_reference_solution_never_reaches_the_workspace() {
    let dir = exercise_dir("nosol", true, true);
    let root = workspace("nosol");

    let work = attempt::open(&dir, &root, &sandbox()).expect("open");
    assert!(dir.join("solution/solve.sh").exists(), "the fixture has a solution to leak");
    assert!(!work.join("solve.sh").exists());
    assert!(!work.join("solution").exists());
    assert!(!work.join("check").exists());
}

/// What is at risk here is unsaved work, so a second `open` must not overwrite it.
#[test]
fn opening_refuses_to_clobber_existing_work() {
    let dir = exercise_dir("clobber", true, true);
    let root = workspace("clobber");

    let work = attempt::open(&dir, &root, &sandbox()).expect("open");
    fs::write(work.join("solution.py"), "# an hour of my life\n").expect("write");

    assert!(attempt::open(&dir, &root, &sandbox()).is_err(), "a second open overwrote the workspace");
    assert_eq!(
        fs::read_to_string(work.join("solution.py")).expect("read"),
        "# an hour of my life\n"
    );
}

/// A broken grader is the author's fault and must not touch anyone's fluency; a
/// timeout is the learner's, and partial credit is the *worst* gating dimension
/// rather than the average of them.
#[test]
fn a_verdict_is_worth_practice_only_when_it_judges_the_learner() {
    assert_eq!(practice_score(&Verdict::CheckBroken("no reward file".into())), None);
    assert_eq!(practice_score(&Verdict::Pass), Some(1.0));
    assert_eq!(practice_score(&Verdict::Timeout(60)), Some(0.0));
    assert_eq!(practice_score(&Verdict::Fail(BTreeMap::new())), Some(0.0));

    let mixed = BTreeMap::from([("correctness".into(), 0.9), ("style".into(), 0.2)]);
    assert_eq!(practice_score(&Verdict::Fail(mixed)), Some(0.2));
}
