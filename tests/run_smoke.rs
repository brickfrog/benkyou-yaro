//! What the execution boundary must guarantee.
//!
//! Two halves. The first is the old contract and still the one a hang would break:
//! report the command's result faithfully, and *always terminate*. The second is new
//! and is the reason this module exists — a job reaches what its view names and
//! nothing else.
//!
//! Everything here runs under the real backend a user gets, which is the sandbox. A
//! test that proved containment against a mock would prove nothing; if `bwrap` is
//! missing these fail, which is the same answer the tool gives.

use std::path::PathBuf;

use benkyou::run::{Access, Backend, Job, Limits, Want};

fn sandbox() -> Backend {
    Backend::choose(Want::Auto, None).expect("a sandbox: install bubblewrap")
}

/// A run directory with a `work/` in it, which is what every real job has.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("benkyou-run-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("work")).expect("scratch");
    dir.canonicalize().expect("canonicalize")
}

const WORK: &[(&str, Access)] = &[("work", Access::Write)];

fn run(dir: &PathBuf, script: &str, secs: u32) -> benkyou::run::Outcome {
    sandbox()
        .run(&Job::new(dir, WORK, "work", script, secs))
        .expect("ran")
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

#[test]
fn output_and_exit_code_are_reported() {
    let out = run(
        &scratch("basic"),
        "echo to-stdout; echo to-stderr >&2; exit 3",
        30,
    );

    assert_eq!(out.exit_code, Some(3));
    assert!(!out.timed_out);
    assert!(!out.succeeded());
    assert_eq!(out.stdout.trim(), "to-stdout");
    assert_eq!(out.stderr.trim(), "to-stderr");
}

#[test]
fn a_successful_command_succeeds() {
    let out = run(&scratch("ok"), "true", 30);
    assert!(out.succeeded());
    assert_eq!(out.exit_code, Some(0));
}

/// A missing command is a broken exercise, not a failing attempt, and the grading
/// layer distinguishes them by exit code.
#[test]
fn a_missing_command_reports_127() {
    let out = run(&scratch("missing"), "definitely-not-a-real-binary", 30);
    assert_eq!(out.exit_code, Some(127));
    assert!(!out.timed_out);
}

/// Writes land on the host, in the directory the view named. Without this the sandbox
/// would be airtight and useless: the learner's work has to survive the run.
#[test]
fn writes_reach_the_host_directory() {
    let dir = scratch("writes");
    let out = run(&dir, "echo hello > answer.txt", 30);
    assert!(out.succeeded(), "{out:?}");
    let body = std::fs::read_to_string(dir.join("work/answer.txt")).expect("wrote through");
    assert_eq!(body.trim(), "hello");
}

// ---------------------------------------------------------------------------
// Containment
// ---------------------------------------------------------------------------

/// The property the whole boundary exists for: a sibling directory that the view did
/// not name is not on the filesystem the job sees.
///
/// This is the gate's `check/` versus its reference solution, in miniature.
#[test]
fn a_directory_outside_the_view_does_not_exist() {
    let dir = scratch("view");
    std::fs::create_dir_all(dir.join("secret")).unwrap();
    std::fs::write(dir.join("secret/answer"), "42").unwrap();

    let out = run(&dir, "cat ../secret/answer", 30);
    assert!(!out.succeeded(), "read a directory the view did not name: {out:?}");
    assert!(out.stdout.trim().is_empty(), "{out:?}");
}

/// A read-only entry in the view is read-only. The gate hands `check/` over this way,
/// so a grader cannot rewrite the tests it is being judged by.
#[test]
fn a_read_only_view_entry_cannot_be_written() {
    let dir = scratch("ro");
    std::fs::create_dir_all(dir.join("check")).unwrap();
    std::fs::write(dir.join("check/check.sh"), "original").unwrap();

    let out = sandbox()
        .run(&Job::new(
            &dir,
            &[("work", Access::Write), ("check", Access::Read)],
            "work",
            "echo tampered > ../check/check.sh",
            30,
        ))
        .expect("ran");

    assert!(!out.succeeded(), "wrote to a read-only view entry: {out:?}");
    let body = std::fs::read_to_string(dir.join("check/check.sh")).unwrap();
    assert_eq!(body, "original");
}

/// The user's own files are not reachable by absolute path either. The view is the
/// whole filesystem, not a working-directory convention.
#[test]
fn the_host_filesystem_is_not_reachable() {
    let dir = scratch("host");
    let witness = dir.join("witness");
    std::fs::write(&witness, "do not read me").unwrap();

    // Its own run directory, by absolute host path — the most likely accident.
    let out = run(&dir, &format!("cat {}", witness.display()), 30);
    assert!(!out.succeeded(), "{out:?}");

    // And a real home directory, the thing a bad `rm` or a curious script goes for.
    let out = run(&dir, "ls /home && ls /root", 30);
    assert!(!out.succeeded(), "enumerated real home directories: {out:?}");
}

/// No network. A grader that fetches a dataset is an exercise that stops working
/// offline and a generated script that phones out is worse; both should fail loudly
/// here rather than silently succeed once.
///
/// Two probes, and the reason is that the first one was silently platform-dependent:
/// `exec 3<>/dev/tcp/...` is a bash feature, so on any machine whose `/bin/sh` is dash —
/// which is most of Debian and everything derived from it — the redirection failed on
/// syntax and this test passed without ever opening a socket. The script now says which
/// probe it managed to run, and the assertion requires one of them, so a probe that stops
/// running can no longer read as a probe that found nothing.
#[test]
fn there_is_no_network() {
    let out = run(
        &scratch("net"),
        "if command -v python3 >/dev/null 2>&1; then echo probe=python; \
           python3 -c \"import socket;socket.create_connection(('1.1.1.1',443),timeout=5)\" \
           2>/dev/null && echo CONNECTED; \
         else echo probe=devtcp; \
           (exec 3<>/dev/tcp/1.1.1.1/443) 2>/dev/null && echo CONNECTED; fi",
        30,
    );
    assert!(out.stdout.contains("probe="), "no probe ran, so nothing was proved: {out:?}");
    assert!(!out.stdout.contains("CONNECTED"), "{out:?}");
}

/// `HOME` is real and writable but is not the workspace and is not the user's. A
/// grader that writes a cache into `$HOME` is common; one that writes into the real
/// one is not acceptable, and one that gets `HOME=/nonexistent` breaks tooling that
/// had nothing to do with the exercise.
#[test]
fn home_is_writable_and_is_not_the_users() {
    let out = run(
        &scratch("home"),
        "printf '%s' \"$HOME\" && touch \"$HOME/cache\" && echo ' ok'",
        30,
    );
    assert!(out.succeeded(), "{out:?}");
    assert!(out.stdout.trim_end().ends_with(" ok"), "{out:?}");
    assert!(!out.stdout.contains("/home/"), "HOME was a real home: {out:?}");
}

/// The environment is an allowlist. A verdict that depended on the caller's
/// `PYTHONPATH` is a verdict that will not reproduce.
#[test]
fn the_environment_is_scrubbed() {
    std::env::set_var("BENKYOU_TEST_LEAK", "leaked");
    let out = run(
        &scratch("env"),
        "echo \"[${BENKYOU_TEST_LEAK-unset}] [${PATH}]\"",
        30,
    );
    std::env::remove_var("BENKYOU_TEST_LEAK");
    assert!(out.stdout.contains("[unset]"), "caller's environment leaked: {out:?}");
    assert!(out.stdout.contains("/usr/bin"), "{out:?}");
}

// ---------------------------------------------------------------------------
// Termination and limits
// ---------------------------------------------------------------------------

#[test]
fn a_hanging_command_hits_the_deadline() {
    let out = run(&scratch("hang"), "sleep 60", 1);
    assert!(out.timed_out);
    assert!(!out.succeeded());
    assert!(out.elapsed_secs < 30.0, "took {}s", out.elapsed_secs);
}

/// The regression that matters most.
///
/// A script can exit while leaving a background process holding the inherited stdout
/// pipe open. Reading that pipe to end then never returns, so draining it on a thread
/// and joining unconditionally hangs forever — a worse failure than the timeout it was
/// meant to replace, because no deadline fires and the gate never answers.
#[test]
fn a_backgrounded_grandchild_cannot_hang_the_runner() {
    let out = run(
        &scratch("orphan"),
        "(sleep 60 &) ; echo parent-done",
        30,
    );
    assert!(out.stdout.contains("parent-done"), "{out:?}");
    assert!(out.elapsed_secs < 20.0, "took {}s", out.elapsed_secs);
}

/// The deadline kills the whole tree, not just the shell. A survivor would keep
/// running against the workspace long after a verdict was reported.
#[test]
fn the_deadline_kills_the_whole_process_tree() {
    let dir = scratch("tree");
    let out = run(
        &dir,
        "(while true; do echo x >> survivor; sleep 0.05; done) & sleep 60",
        1,
    );
    assert!(out.timed_out, "{out:?}");

    let marker = dir.join("work/survivor");
    let before = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
    std::thread::sleep(std::time::Duration::from_secs(1));
    let after = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
    assert_eq!(before, after, "a process outlived the deadline and kept writing");
}

/// An output bomb is reported as an output bomb, not as a hang.
///
/// Stopping the read would block the child on a full pipe until the deadline, which
/// turns "printed too much" into "timed out" and loses the actual finding. Reading and
/// discarding keeps the exit code meaningful and bounds this process's memory.
#[test]
fn runaway_output_is_truncated_not_hung() {
    let dir = scratch("flood");
    let mut job = Job::new(&dir, WORK, "work", "yes hello | head -c 40000000; exit 5", 60);
    job.limits = Limits { output_bytes: 64 * 1024, ..Limits::default() };

    let out = sandbox().run(&job).expect("ran");
    assert!(out.truncated, "{}", out.stdout.len());
    assert!(!out.timed_out, "an output bomb was reported as a hang");
    assert_eq!(out.exit_code, Some(5), "the exit code survived the flood");
    assert!(out.stdout.len() <= 64 * 1024, "kept {} bytes", out.stdout.len());
}

/// `/tmp` is a bounded tmpfs, so a runaway write fills a ceiling rather than the
/// user's disk.
#[test]
fn the_scratch_filesystem_is_bounded() {
    let out = run(
        &scratch("disk"),
        "dd if=/dev/zero of=/tmp/fill bs=1M count=2048 2>/dev/null; \
         du -sm /tmp/fill 2>/dev/null | cut -f1",
        120,
    );
    let mb: u64 = out.stdout.trim().parse().unwrap_or(0);
    assert!(mb > 0 && mb < 512, "wrote {mb} MiB into a 256 MiB tmpfs: {out:?}");
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// The default is isolation, and asking for the other one is not a matter of degree.
#[test]
fn the_default_backend_is_the_sandbox() {
    assert_eq!(Backend::choose(Want::Auto, None).expect("a sandbox").name(), "sandbox");
    assert_eq!(Backend::choose(Want::UnsafeHost, None).expect("host").name(), "unsafe-host");
}

/// A job that names a directory the caller never created is the caller's bug, and it
/// is reported as that rather than as a mount failure from inside the sandbox.
#[test]
fn a_view_naming_a_missing_directory_is_refused() {
    let dir = scratch("absent");
    let err = sandbox()
        .run(&Job::new(
            &dir,
            &[("work", Access::Write), ("nope", Access::Read)],
            "work",
            "true",
            30,
        ))
        .expect_err("must refuse");
    assert!(err.contains("nope"), "{err}");
}

/// A fork bomb is contained and, more importantly, the containment does not depend on
/// the host having spare process slots.
#[test]
fn a_fork_bomb_is_contained() {
    let dir = scratch("bomb");
    let mut job = Job::new(&dir, WORK, "work", ":(){ :|:& };: ; sleep 5", 10);
    job.limits = Limits { processes: 64, ..Limits::default() };

    let started = std::time::Instant::now();
    let out = sandbox().run(&job).expect("ran");
    assert!(started.elapsed().as_secs() < 30, "the bomb outlived its deadline");
    assert!(!out.succeeded() || out.timed_out, "{out:?}");
}

/// The process cap must never be applied on the host, where `RLIMIT_NPROC` is counted
/// against the whole logged-in user.
///
/// This is a regression test for a bug that shipped nothing but did reject a perfectly
/// good exercise: a desktop session holds several hundred processes, so a cap of 512
/// made the reference solution's very first `fork` fail with "Resource temporarily
/// unavailable". The sandbox is unaffected because its user namespace starts the count
/// at zero — which is exactly why the cap belongs there and only there.
#[test]
fn the_host_backend_can_still_fork() {
    let dir = scratch("hostfork");
    let mut job = Job::new(&dir, WORK, "work", "for i in 1 2 3; do (true); done; echo forked", 30);
    job.limits = Limits { processes: 1, ..Limits::default() };

    let out = Backend::choose(Want::UnsafeHost, None)
        .expect("host")
        .run(&job)
        .expect("ran");
    assert!(out.succeeded(), "a per-user cap leaked onto the host backend: {out:?}");
    assert_eq!(out.stdout.trim(), "forked");
}
