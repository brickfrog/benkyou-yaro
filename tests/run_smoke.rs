//! What the graded-command runner must guarantee.
//!
//! There is no sandbox here by design, so the runner's whole contract is: report the
//! command's result faithfully, and *always terminate*. The second half is the part
//! that is easy to get wrong — a script that leaves something running behind it must
//! not be able to hang the gate.

use std::path::PathBuf;
use std::time::Instant;

use benkyou::run::Runner;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("benkyou-run-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir.canonicalize().expect("canonicalize")
}

#[test]
fn output_and_exit_code_are_reported() {
    let out = Runner::in_dir(scratch("basic"), 30)
        .run("echo to-stdout; echo to-stderr >&2; exit 3")
        .expect("ran");

    assert_eq!(out.exit_code, Some(3));
    assert!(!out.timed_out);
    assert!(!out.succeeded());
    assert_eq!(out.stdout.trim(), "to-stdout");
    assert_eq!(out.stderr.trim(), "to-stderr");
}

#[test]
fn a_successful_command_succeeds() {
    let out = Runner::in_dir(scratch("ok"), 30).run("true").expect("ran");
    assert!(out.succeeded());
    assert_eq!(out.exit_code, Some(0));
}

#[test]
fn the_working_directory_is_the_run_directory() {
    let dir = scratch("cwd");
    let out = Runner::in_dir(&dir, 30).run("pwd; printf '%s' \"$HOME\" > home").expect("ran");

    assert_eq!(out.stdout.trim(), dir.to_str().expect("utf8"));
    let home = std::fs::read_to_string(dir.join("home")).expect("wrote home");
    assert_eq!(home, dir.to_str().expect("utf8"), "HOME points at the run directory");
}

/// A missing command is a broken exercise, not a failing attempt, and the grading
/// layer distinguishes them by exit code.
#[test]
fn a_missing_command_reports_127() {
    let out = Runner::in_dir(scratch("missing"), 30)
        .run("definitely-not-a-real-binary-xyz")
        .expect("ran");
    assert_eq!(out.exit_code, Some(127));
}

#[test]
fn a_hanging_command_hits_the_deadline() {
    let started = Instant::now();
    let out = Runner::in_dir(scratch("hang"), 1).run("sleep 60").expect("ran");

    assert!(out.timed_out, "deadline was not reported");
    assert!(!out.succeeded(), "a timed-out command never succeeds");
    assert!(
        started.elapsed().as_secs() < 20,
        "took {:?}, so the deadline did not fire",
        started.elapsed()
    );
}

/// The regression that matters most.
///
/// A script can exit while leaving a background process holding the inherited stdout
/// pipe open. Reading that pipe to end then never returns, so draining it on a thread
/// and joining unconditionally hangs forever — a worse failure than the timeout it
/// was meant to replace, because no deadline fires and the gate never answers.
#[test]
fn a_backgrounded_grandchild_cannot_hang_the_runner() {
    let started = Instant::now();
    let out = Runner::in_dir(scratch("orphan"), 30)
        .run("sleep 120 & echo parent-done")
        .expect("ran");

    assert!(
        started.elapsed().as_secs() < 20,
        "took {:?}: a surviving grandchild held the pipe open",
        started.elapsed()
    );
    assert_eq!(out.exit_code, Some(0), "the script itself exited cleanly");
    assert!(!out.timed_out, "the script finished; only its orphan lingered");
    assert!(
        out.stdout.contains("parent-done"),
        "output produced before the orphan was reaped must survive: {:?}",
        out.stdout
    );
}

/// The deadline kills the whole process group, not just the shell. A survivor would
/// keep running against the run directory long after the gate reported a verdict.
#[test]
fn the_deadline_kills_the_whole_process_group() {
    let dir = scratch("group");
    let out = Runner::in_dir(&dir, 1)
        .run("(sleep 4; touch survived) & wait")
        .expect("ran");
    assert!(out.timed_out, "expected the deadline to fire");

    // Outlast the grandchild's own sleep. If the group died, the marker never lands.
    std::thread::sleep(std::time::Duration::from_secs(6));
    assert!(
        !dir.join("survived").exists(),
        "a process outlived the deadline and kept writing to the run directory"
    );
}
