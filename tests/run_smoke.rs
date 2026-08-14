//! What the execution boundary must guarantee.
//!
//! Two halves: report the command's result faithfully and always terminate, and reach
//! nothing the view did not name.
//!
//! Everything runs under the sandbox, which is the backend a user gets. A missing
//! `bwrap` fails these tests, the same answer the tool gives.

use std::path::PathBuf;

use benkyou::run::{Access, Backend, Job, Limits, Want};

/// `Want::Auto` accepts either isolating backend, so the refusal names both.
fn sandbox() -> Backend {
    Backend::choose(Want::Auto, None).expect(
        "no sandbox and no container engine: install bubblewrap, or install \
         docker/podman and run `benkyou runner --pull`",
    )
}

/// A run directory with a `work/` in it, like every job has.
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

/// Grading reads 127 as a broken exercise, not a wrong answer.
#[test]
fn a_missing_command_reports_127() {
    let out = run(&scratch("missing"), "definitely-not-a-real-binary", 30);
    assert_eq!(out.exit_code, Some(127));
    assert!(!out.timed_out);
}

/// Writes land on the host, in the directory the view named. The learner's work has to
/// survive the run.
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

/// A sibling directory the view did not name is not there. This is the gate's `check/`
/// against its reference solution, in miniature.
#[test]
fn a_directory_outside_the_view_does_not_exist() {
    let dir = scratch("view");
    std::fs::create_dir_all(dir.join("secret")).unwrap();
    std::fs::write(dir.join("secret/answer"), "42").unwrap();

    let out = run(&dir, "cat ../secret/answer", 30);
    assert!(!out.succeeded(), "read a directory the view did not name: {out:?}");
    assert!(out.stdout.trim().is_empty(), "{out:?}");
}

/// A read-only view entry is read-only. The gate hands `check/` over this way.
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

/// Absolute host paths do not work either. The view is the only filesystem the job has.
#[test]
fn the_host_filesystem_is_not_reachable() {
    let dir = scratch("host");
    let witness = dir.join("witness");
    std::fs::write(&witness, "do not read me").unwrap();

    // Its own run directory, by absolute host path. The most likely accident.
    let out = run(&dir, &format!("cat {}", witness.display()), 30);
    assert!(!out.succeeded(), "{out:?}");

    // And a home directory, what a bad `rm` goes for.
    let out = run(&dir, "ls /home && ls /root", 30);
    assert!(!out.succeeded(), "enumerated real home directories: {out:?}");
}

/// No network. A grader that fetches stops working offline.
///
/// Two probes, because the first was platform-dependent. `/dev/tcp` is a bash feature,
/// so on a dash `/bin/sh` it failed on syntax and passed without opening a socket. The
/// script names the probe it ran.
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

/// `HOME` is writable, is not the workspace, and is not the user's. A grader that caches
/// into `$HOME` must not reach the real one, and `HOME=/nonexistent` breaks tooling.
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

/// The environment is an allowlist. A verdict that depended on the caller's `PYTHONPATH`
/// will not reproduce.
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
/// A script can exit while a background process holds the inherited stdout pipe open.
/// Reading that pipe to end never returns, so an unconditional join hangs with no
/// deadline left to fire.
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

/// The deadline kills the whole tree, not just the shell. A survivor keeps running
/// against the workspace after the verdict.
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

/// An output bomb is reported as truncation, not as a hang.
///
/// Stopping the read blocks the child on a full pipe until the deadline, which turns
/// "printed too much" into "timed out".
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

/// `/tmp` is a bounded tmpfs, so a runaway write fills a ceiling, not the user's disk.
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

/// The default is isolation. The host backend is a separate, explicit request.
#[test]
fn the_default_backend_is_the_sandbox() {
    assert_eq!(Backend::choose(Want::Auto, None).expect("a sandbox").name(), "sandbox");
    assert_eq!(Backend::choose(Want::UnsafeHost, None).expect("host").name(), "unsafe-host");
}

/// A view naming a directory the caller never created is refused, not left to the sandbox.
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

/// A fork bomb is contained, and the containment does not depend on the host having
/// spare process slots.
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

/// The process cap must never apply on the host, where `RLIMIT_NPROC` counts the whole
/// logged-in user.
///
/// A cap of 512 once failed a reference solution's first `fork`: a desktop session holds
/// several hundred processes. The sandbox counts from zero in its own user namespace.
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
