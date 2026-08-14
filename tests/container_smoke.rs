//! What the container backend must guarantee.
//!
//! The same contract `run_smoke.rs` pins for the sandbox, asked of an engine instead:
//! report the command faithfully, always terminate, and reach nothing the view did not
//! name. It is a separate file because two of the mechanisms are genuinely different —
//! the process cap is a cgroup rather than an rlimit, and the deadline kills a container
//! rather than a process group — and because a shared harness would hide which backend a
//! failure came from.
//!
//! **On skipping.** These return early when there is no engine, or when the runner image
//! has not been pulled. That is not the licence `run_smoke.rs` refuses to take:
//! bubblewrap is a documented requirement for a Linux user, so a missing `bwrap` fails
//! the suite exactly as it fails the tool. A container engine is optional on Linux and a
//! multi-hundred-megabyte pull is not something `cargo test` should perform behind
//! somebody's back. What is never skipped is the case that matters: an engine that *is*
//! present with an image that *is* pulled runs every assertion below.

use std::path::PathBuf;

use benkyou::run::{Access, Backend, Job, Limits, Outcome, Want};

/// The backend under test, or `None` on a machine that cannot run it.
///
/// The two situations wear the same error and only one of them is a skip, which is a
/// distinction this file learned the hard way: an early version returned `None` on any
/// failure, so breaking the policy on purpose — the way you check a test measures
/// anything — made every assertion below quietly skip and report success.
///
/// So the prerequisites are asked for first, and they are the only licence to skip. Once
/// an engine and a pulled image are known to be here, a backend that will not build is a
/// failure and says so.
fn container() -> Option<Backend> {
    match benkyou::run::runner_status(None, false) {
        Err(why) => {
            eprintln!("skipping: {why}");
            return None;
        }
        Ok(status) if status.image.is_none() => {
            eprintln!(
                "skipping: runner image not pulled: {} - run `benkyou runner --pull`",
                status.reference
            );
            return None;
        }
        Ok(_) => {}
    }
    Some(
        Backend::choose(Want::Container, None)
            .expect("an engine and its image are present, so the backend must build"),
    )
}

/// A run directory with a `work/` in it, which is what every real job has.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("benkyou-ctr-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("work")).expect("scratch");
    dir.canonicalize().expect("canonicalize")
}

const WORK: &[(&str, Access)] = &[("work", Access::Write)];

fn run(backend: &Backend, dir: &PathBuf, script: &str, secs: u32) -> Outcome {
    backend
        .run(&Job::new(dir, WORK, "work", script, secs))
        .expect("ran")
}

/// Every test below is `let Some(backend) = container() else { return };`, so this one
/// exists to make the skip itself visible: it prints what a reader would otherwise have
/// to infer from a suspiciously fast suite.
#[test]
fn the_backend_is_selectable_by_name() {
    let Some(backend) = container() else { return };
    assert_eq!(backend.name(), "container");
    assert!(
        backend.image_id().is_some_and(|id| id.starts_with("sha256:")),
        "a container verdict has to be able to name its runtime: {:?}",
        backend.image_id()
    );
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// An engine sits between this process and the script, and it must not launder the
/// result: the exit code has to be the shell's, and the two streams have to stay apart.
#[test]
fn output_and_exit_code_survive_the_engine() {
    let Some(backend) = container() else { return };
    let out = run(
        &backend,
        &scratch("basic"),
        "echo to-stdout; echo to-stderr >&2; exit 3",
        60,
    );

    assert_eq!(out.exit_code, Some(3), "{out:?}");
    assert!(!out.timed_out);
    assert_eq!(out.stdout.trim(), "to-stdout");
    assert_eq!(out.stderr.trim(), "to-stderr");
}

/// 127 is load-bearing: grading reads it as a broken exercise rather than a failing
/// attempt, and an engine that reported its own failure code instead would turn every
/// missing interpreter into a wrong answer.
#[test]
fn a_missing_command_still_reports_127() {
    let Some(backend) = container() else { return };
    let out = run(&backend, &scratch("missing"), "definitely-not-a-real-binary", 60);
    assert_eq!(out.exit_code, Some(127), "{out:?}");
    assert!(!out.timed_out);
}

/// Writes land on the host, in the directory the view named, owned by the caller.
///
/// The ownership half is why `--user` is in the policy. A rootful daemon runs a
/// container as root by default, so without it the learner's own workspace would fill
/// with root-owned files that no later run could rewrite and no `rm` could clear.
#[test]
fn writes_reach_the_host_directory_as_the_caller() {
    let Some(backend) = container() else { return };
    let dir = scratch("writes");
    let out = run(&backend, &dir, "echo hello > answer.txt", 60);
    assert!(out.succeeded(), "{out:?}");

    let path = dir.join("work/answer.txt");
    let body = std::fs::read_to_string(&path).expect("wrote through");
    assert_eq!(body.trim(), "hello");

    use std::os::unix::fs::MetadataExt;
    let mine = std::fs::metadata(&dir).expect("scratch metadata").uid();
    let theirs = std::fs::metadata(&path).expect("answer metadata").uid();
    assert_eq!(theirs, mine, "a job's writes must belong to the caller, not to root");
}

// ---------------------------------------------------------------------------
// Containment
// ---------------------------------------------------------------------------

/// The property the whole boundary exists for, and the gate's `check/` versus its
/// reference solution in miniature: a sibling the view did not name is not there.
#[test]
fn a_directory_outside_the_view_does_not_exist() {
    let Some(backend) = container() else { return };
    let dir = scratch("view");
    std::fs::create_dir_all(dir.join("secret")).unwrap();
    std::fs::write(dir.join("secret/answer"), "42").unwrap();

    let out = run(&backend, &dir, "cat ../secret/answer", 60);
    assert!(!out.succeeded(), "read a directory the view did not name: {out:?}");
    assert!(!out.stdout.contains("42"), "{out:?}");
}

/// A read-only entry is read-only. The gate hands `check/` over this way, so a grader
/// cannot rewrite the tests it is being judged by.
#[test]
fn a_read_only_view_entry_cannot_be_written() {
    let Some(backend) = container() else { return };
    let dir = scratch("ro");
    std::fs::create_dir_all(dir.join("check")).unwrap();
    std::fs::write(dir.join("check/check.sh"), "original").unwrap();

    let out = backend
        .run(&Job::new(
            &dir,
            &[("work", Access::Write), ("check", Access::Read)],
            "work",
            "echo tampered > ../check/check.sh",
            60,
        ))
        .expect("ran");

    assert!(!out.succeeded(), "wrote to a read-only view entry: {out:?}");
    assert_eq!(std::fs::read_to_string(dir.join("check/check.sh")).unwrap(), "original");
}

/// The gate's first direction, exactly as it is built: the reference solution runs with
/// `work/` and `solution/` in view and `check/` left out, so a `solve.sh` cannot pass by
/// reading the tests it is supposed to be independent of.
///
/// Spelled out rather than left to the generic view test, because this is the claim the
/// whole gate rests on and it is the one an engine could quietly break: mount lists are
/// assembled per job, so "the view is honoured" and "this particular view is honoured"
/// are not the same assertion.
#[test]
fn the_reference_run_cannot_see_the_hidden_checks() {
    let Some(backend) = container() else { return };
    let dir = scratch("hidden");
    std::fs::create_dir_all(dir.join("check")).unwrap();
    std::fs::create_dir_all(dir.join("solution")).unwrap();
    std::fs::write(dir.join("check/cases.py"), "EXPECTED = [1, 2, 3]").unwrap();
    std::fs::write(dir.join("solution/solve.sh"), "cat ../check/cases.py").unwrap();

    let out = backend
        .run(&Job::new(
            &dir,
            &[("work", Access::Write), ("solution", Access::Read)],
            "work",
            "sh ../solution/solve.sh",
            60,
        ))
        .expect("ran");

    assert!(!out.succeeded(), "the reference run read the hidden checks: {out:?}");
    assert!(!out.stdout.contains("EXPECTED"), "{out:?}");
}

/// The user's files are not reachable by absolute path either. The view is the whole
/// filesystem the job has, not a working-directory convention.
#[test]
fn the_host_filesystem_is_not_reachable() {
    let Some(backend) = container() else { return };
    let dir = scratch("host");
    let witness = dir.join("witness");
    std::fs::write(&witness, "do not read me").unwrap();

    // Its own run directory, by absolute host path — the most likely accident.
    let out = run(&backend, &dir, &format!("cat {}", witness.display()), 60);
    assert!(!out.succeeded(), "{out:?}");
    assert!(!out.stdout.contains("do not read me"), "{out:?}");

    // And a real home directory, the thing a bad `rm` or a curious script goes for.
    let out = run(&backend, &dir, "ls /home && ls /root", 60);
    assert!(!out.succeeded(), "enumerated real home directories: {out:?}");
}

/// No network. `--network none` is the whole of it, and it is the guarantee the
/// dependency mechanism is built on: a grader that could fetch would make `warm`
/// pointless and every verdict a property of the afternoon it was earned.
///
/// The probe is Python and not `/dev/tcp`, which is what this test used first and is a
/// *bash* feature: the image's `/bin/sh` is dash, so the redirection failed on syntax and
/// the test passed with `--network none` removed. It now says which mechanism it used, so
/// a probe that stops running cannot read as a probe that found nothing.
#[test]
fn there_is_no_network() {
    let Some(backend) = container() else { return };
    let out = run(
        &backend,
        &scratch("net"),
        "echo probed; python3 -c \"import socket;socket.create_connection(('1.1.1.1',443),timeout=5)\" \
         2>/dev/null && echo CONNECTED; \
         python3 -c \"import socket;socket.gethostbyname('example.com')\" 2>/dev/null && echo RESOLVED",
        60,
    );
    assert!(out.stdout.contains("probed"), "the probe never ran: {out:?}");
    assert!(!out.stdout.contains("CONNECTED"), "{out:?}");
    assert!(!out.stdout.contains("RESOLVED"), "dns resolved inside a network-less job: {out:?}");
}

/// `HOME` is real and writable and is not the user's. A `getpwuid` that fails breaks
/// Python's `expanduser` and much else, which is why the synthetic passwd is mounted:
/// this asserts the interpreter agrees with the environment variable.
#[test]
fn home_is_writable_and_the_interpreter_agrees() {
    let Some(backend) = container() else { return };
    let out = run(
        &backend,
        &scratch("home"),
        "touch \"$HOME/cache\" && python3 -c 'import os;print(os.path.expanduser(\"~\"))'",
        60,
    );
    assert!(out.succeeded(), "{out:?}");
    assert_eq!(out.stdout.trim(), "/box/.home", "{out:?}");
    assert!(!out.stdout.contains("/home/"), "HOME was a real home: {out:?}");
}

/// The caller's environment does not reach the job.
///
/// What the allowlist cannot govern here is the image's own `ENV`, and that is stated
/// rather than asserted away: `PYTHON_VERSION` arrives because the image exports it, and
/// the image is the one thing a container verdict records exactly.
#[test]
fn the_callers_environment_does_not_leak() {
    let Some(backend) = container() else { return };
    std::env::set_var("BENKYOU_CONTAINER_LEAK", "leaked");
    let out = run(
        &backend,
        &scratch("env"),
        "echo \"[${BENKYOU_CONTAINER_LEAK-unset}] [$LC_ALL] [$TZ]\"",
        60,
    );
    std::env::remove_var("BENKYOU_CONTAINER_LEAK");

    assert!(out.stdout.contains("[unset]"), "caller's environment leaked: {out:?}");
    assert!(out.stdout.contains("[C.UTF-8]"), "the allowlist did not arrive: {out:?}");
    assert!(out.stdout.contains("[UTC]"), "the allowlist did not arrive: {out:?}");
}

/// The point of the backend, and the reason a verdict records an image: the runtime is
/// the image's, not the machine's. If these two ever matched by accident the test would
/// be vacuous, so it asserts the image's own version rather than merely "different".
#[test]
fn the_interpreter_is_the_images() {
    let Some(backend) = container() else { return };
    let out = run(
        &backend,
        &scratch("interp"),
        "python3 -c 'import sys;print(\"%d.%d\" % sys.version_info[:2])'",
        60,
    );
    assert!(out.succeeded(), "{out:?}");
    assert_eq!(
        out.stdout.trim(),
        "3.13",
        "the default runner image is python 3.13; a job that reports otherwise is not \
         running in it: {out:?}"
    );
}

// ---------------------------------------------------------------------------
// Termination and limits
// ---------------------------------------------------------------------------

#[test]
fn a_hanging_command_hits_the_deadline() {
    let Some(backend) = container() else { return };
    let out = run(&backend, &scratch("hang"), "sleep 120", 2);
    assert!(out.timed_out, "{out:?}");
    assert!(!out.succeeded());
    assert!(out.elapsed_secs < 60.0, "took {}s", out.elapsed_secs);
}

/// The regression this backend could have shipped.
///
/// Killing the process group kills the engine's *client*; the daemon owns the container
/// and never notices. A survivor would keep writing into the learner's workspace long
/// after a verdict was reported — and under `serve`, keep doing it while the next
/// exercise ran. The witness file is checked for growth *after* the deadline, because
/// "the client exited" and "the job stopped" are exactly the two things this conflates.
#[test]
fn the_deadline_kills_the_container_and_not_just_the_client() {
    let Some(backend) = container() else { return };
    let dir = scratch("tree");
    let out = run(
        &backend,
        &dir,
        "(while true; do echo x >> survivor; sleep 0.05; done) & sleep 120",
        2,
    );
    assert!(out.timed_out, "{out:?}");

    let marker = dir.join("work/survivor");
    let before = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
    assert!(before > 0, "the background writer never ran, so this proves nothing");
    std::thread::sleep(std::time::Duration::from_secs(2));
    let after = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
    assert_eq!(before, after, "the container outlived the deadline and kept writing");
}

/// An output bomb is reported as an output bomb, not as a hang, with an engine in the
/// middle of the pipe.
#[test]
fn runaway_output_is_truncated_not_hung() {
    let Some(backend) = container() else { return };
    let dir = scratch("flood");
    let mut job = Job::new(&dir, WORK, "work", "yes hello | head -c 20000000; exit 5", 120);
    job.limits = Limits { output_bytes: 64 * 1024, ..Limits::default() };

    let out = backend.run(&job).expect("ran");
    assert!(out.truncated, "kept {} bytes", out.stdout.len());
    assert!(!out.timed_out, "an output bomb was reported as a hang: {out:?}");
    assert_eq!(out.exit_code, Some(5), "the exit code survived the flood");
    assert!(out.stdout.len() <= 64 * 1024, "kept {} bytes", out.stdout.len());
}

/// `/tmp` is a bounded tmpfs, so a runaway write fills a ceiling rather than the user's
/// disk. The engine gets this from `--tmpfs`, where the sandbox gets it from `--size`;
/// the number is shared so the two agree.
#[test]
fn the_scratch_filesystem_is_bounded() {
    let Some(backend) = container() else { return };
    let out = run(
        &backend,
        &scratch("disk"),
        "dd if=/dev/zero of=/tmp/fill bs=1M count=2048 2>/dev/null; \
         du -sm /tmp/fill 2>/dev/null | cut -f1",
        180,
    );
    let mb: u64 = out.stdout.trim().parse().unwrap_or(0);
    assert!(mb > 0 && mb < 512, "wrote {mb} MiB into a 256 MiB tmpfs: {out:?}");
}

/// The process cap is the container's, and it has to actually bite.
///
/// `ulimit -u` is deliberately not applied under this backend: a container shares the
/// host's uid, so `RLIMIT_NPROC` would count the whole logged-in session exactly as it
/// does on the host — set below it nothing forks at all. `--pids-limit` counts the
/// container instead.
///
/// The assertion is a count, and that is the second version of this test. The first ran
/// a fork bomb and looked for `Cannot fork` in the output, which passes with the cap
/// *removed*: a bomb hits the host's own ceiling and reports the same words, so the test
/// was measuring Linux rather than this policy. Asking for a specific number of
/// concurrent processes discriminates — 64 of them arrive when nothing caps them and
/// cannot when the cap is 16.
#[test]
fn the_process_cap_is_the_containers_own() {
    let Some(backend) = container() else { return };
    let dir = scratch("bomb");
    let mut job = Job::new(
        &dir,
        WORK,
        "work",
        "i=0; ok=0; while [ $i -lt 64 ]; do sleep 30 & \
         if [ $? -eq 0 ]; then ok=$((ok+1)); fi; i=$((i+1)); done; echo spawned=$ok",
        30,
    );
    job.limits = Limits { processes: 16, ..Limits::default() };

    let started = std::time::Instant::now();
    let out = backend.run(&job).expect("ran");
    assert!(started.elapsed().as_secs() < 60, "the run outlived its deadline");
    assert!(
        !out.stdout.contains("spawned=64"),
        "64 concurrent processes under a cap of 16: the cap did not apply: {out:?}"
    );
}

/// A container whose owner is gone is killed by the next detection.
///
/// This is the hole `--die-with-parent` fills for the sandbox and nothing fills for an
/// engine: kill this process outright and the daemon keeps running the job it was
/// watching, against the learner's workspace, forever. The label is the only thing that
/// makes the leftovers identifiable, so the test fakes one with an owner pid that cannot
/// exist and checks that building a backend collects it.
#[test]
fn a_container_whose_owner_died_is_reaped() {
    let Some(_) = container() else { return };

    // Above `pid_max` on any Linux this runs on, so it names no process and cannot be
    // reused by one between the two halves of this test.
    let orphan_owner = "4194303";
    let name = format!("benkyou-orphan-test-{}", std::process::id());
    let started = std::process::Command::new("docker")
        .args(["run", "-d", "--rm", "--network", "none"])
        .args(["--label", &format!("benkyou.owner={orphan_owner}")])
        .args(["--name", &name, "--entrypoint", "/bin/sh"])
        .arg(benkyou::run::DEFAULT_IMAGE)
        .args(["-c", "sleep 300"])
        .output()
        .expect("start a fake orphan");
    assert!(
        started.status.success(),
        "could not stage an orphan: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let running = |name: &str| {
        let out = std::process::Command::new("docker")
            .args(["ps", "--filter", &format!("name={name}"), "--format", "{{.Names}}"])
            .output()
            .expect("docker ps");
        String::from_utf8_lossy(&out.stdout).contains(name)
    };
    assert!(running(&name), "the staged orphan is not running, so this proves nothing");

    // Detection is what reaps. Any container command performs one.
    let _ = Backend::choose(Want::Container, None).expect("backend");
    std::thread::sleep(std::time::Duration::from_millis(500));

    let survived = running(&name);
    if survived {
        let _ = std::process::Command::new("docker").args(["kill", &name]).output();
    }
    assert!(!survived, "a container whose owner is gone outlived the next detection");
}

/// A job that names a directory the caller never created is the caller's bug, and it is
/// reported as that rather than as a mount failure from inside the engine.
#[test]
fn a_view_naming_a_missing_directory_is_refused() {
    let Some(backend) = container() else { return };
    let dir = scratch("absent");
    let err = backend
        .run(&Job::new(
            &dir,
            &[("work", Access::Write), ("nope", Access::Read)],
            "work",
            "true",
            60,
        ))
        .expect_err("must refuse");
    assert!(err.contains("nope"), "{err}");
}
