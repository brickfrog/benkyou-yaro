//! What the container backend must guarantee.
//!
//! The contract `run_smoke.rs` pins for the sandbox, asked of an engine. Separate file
//! because the process cap is a cgroup, not an rlimit, and the deadline kills a
//! container, not a process group.
//!
//! On skipping. These tests return early when there is no engine, or when the runner
//! image is not pulled. A multi-hundred-megabyte pull is not work `cargo test` performs
//! on its own. `BENKYOU_REQUIRE_CONTAINER=1` turns every skip here into a failure, which
//! is what CI sets. Without it this file can print `19 passed` having skipped nineteen.

use std::path::PathBuf;

use benkyou::deps::{self, Runtime};
use benkyou::exercise::Deps;
use benkyou::run::{Access, Backend, Job, Limits, Outcome, Want};

/// Set to `1` to turn every skip below into a failure.
///
/// Matched against `1` alone: on any other value the variable reads as unset.
const REQUIRE: &str = "BENKYOU_REQUIRE_CONTAINER";

/// The backend under test, or `None` on a machine that cannot run it.
///
/// Missing prerequisites are the only licence to skip: an earlier version returned `None`
/// on any failure, so a broken policy skipped everything and passed.
fn container() -> Option<Backend> {
    match benkyou::run::runner_status(None, false) {
        Err(why) => return absent(why),
        Ok(status) if status.image.is_none() => {
            return absent(format!(
                "runner image not pulled: {} - run `benkyou runner --pull`",
                status.reference
            ));
        }
        Ok(_) => {}
    }
    Some(
        Backend::choose(Want::Container, None)
            .expect("an engine and its image are present, so the backend must build"),
    )
}

/// A missing prerequisite: a skip, or a failure under the required mode.
///
/// "skipping" is printed on the skip path alone, so a `--nocapture` run can be grepped.
fn absent(why: String) -> Option<Backend> {
    if matches!(std::env::var(REQUIRE).as_deref(), Ok("1")) {
        panic!("{why}\n  {REQUIRE}=1, so a missing prerequisite is a failure, not a skip");
    }
    eprintln!("skipping: {why}");
    None
}

/// A run directory with a `work/` in it, like every job has.
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

/// Makes the skip visible. Every other test here returns early in silence.
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

/// The engine must not launder the result: the shell's exit code, two separate streams.
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

/// Grading reads 127 as a broken exercise, not a wrong answer. An engine that substitutes
/// its own failure code breaks that.
#[test]
fn a_missing_command_still_reports_127() {
    let Some(backend) = container() else { return };
    let out = run(&backend, &scratch("missing"), "definitely-not-a-real-binary", 60);
    assert_eq!(out.exit_code, Some(127), "{out:?}");
    assert!(!out.timed_out);
}

/// Writes land on the host, in the directory the view named, owned by the caller.
///
/// `--user` is why. A rootful daemon writes as root, leaving files no later run can clear.
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

/// A sibling directory the view did not name is not there.
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

/// A read-only entry is read-only. The gate hands `check/` over this way.
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

/// The gate's first direction: `solution/` in view, `check/` left out.
///
/// Mount lists are assembled per job, so the generic view test does not cover this one.
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

/// Absolute host paths do not work either. The view is the only filesystem the job has.
#[test]
fn the_host_filesystem_is_not_reachable() {
    let Some(backend) = container() else { return };
    let dir = scratch("host");
    let witness = dir.join("witness");
    std::fs::write(&witness, "do not read me").unwrap();

    // Its own run directory, by absolute host path. The most likely accident.
    let out = run(&backend, &dir, &format!("cat {}", witness.display()), 60);
    assert!(!out.succeeded(), "{out:?}");
    assert!(!out.stdout.contains("do not read me"), "{out:?}");

    // And a home directory, what a bad `rm` goes for.
    let out = run(&backend, &dir, "ls /home && ls /root", 60);
    assert!(!out.succeeded(), "enumerated real home directories: {out:?}");
}

/// No network, from `--network none`. A grader that can fetch makes `warm` pointless.
///
/// The probe is Python. `/dev/tcp` is bash-only and the image's `/bin/sh` is dash, so the
/// first version failed on syntax and passed with `--network none` removed.
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

/// `HOME` is writable and is not the user's. The synthetic passwd is mounted so
/// `getpwuid` works, which `expanduser` needs.
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

/// The caller's environment does not reach the job. The image's own `ENV` still does.
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

/// The runtime is the image's, not the machine's. Asserts the image's version, because
/// "different" passes when the two match by accident.
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

/// The deadline must kill the container, not the engine's client.
///
/// A survivor keeps writing into the learner's workspace after the verdict, so the
/// witness file is checked for growth after the deadline.
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

/// An output bomb is reported as truncation, not as a hang.
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

/// `/tmp` is a bounded tmpfs, so a runaway write fills a ceiling, not the user's disk.
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

/// The process cap is `--pids-limit`, not `ulimit -u`, because a container shares the
/// host's uid and `RLIMIT_NPROC` counts the whole login session.
///
/// Counts processes instead of grepping for `Cannot fork`. That grep passed with the cap
/// removed, because a fork bomb hits the host ceiling and prints the same words.
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
/// `--die-with-parent` covers this for the sandbox. Nothing covers it for an engine, so
/// the owner label plus a reap on detection is the only mechanism.
#[test]
fn a_container_whose_owner_died_is_reaped() {
    let Some(backend) = container() else { return };
    // The engine detection chose, at the path it chose. Hard-coded `docker` staged the
    // orphan under an engine that was not the one asked to reap it.
    let Backend::Container { cli, image, .. } = &backend else {
        panic!("container() hands back a container backend, not {}", backend.name())
    };

    // Above `pid_max`, so it names no process and cannot be reused mid-test.
    let orphan_owner = "4194303";
    let name = format!("benkyou-orphan-test-{}", std::process::id());
    let started = std::process::Command::new(cli)
        .args(["run", "-d", "--rm", "--network", "none"])
        .args(["--label", &format!("benkyou.owner={orphan_owner}")])
        .args(["--name", &name, "--entrypoint", "/bin/sh"])
        // By id, as a real job is launched: no engine has to re-resolve the reference.
        .arg(&image.id)
        .args(["-c", "sleep 300"])
        .output()
        .expect("start a fake orphan");
    assert!(
        started.status.success(),
        "could not stage an orphan: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let running = |name: &str| {
        let out = std::process::Command::new(cli)
            .args(["ps", "--filter", &format!("name={name}"), "--format", "{{.Names}}"])
            .output()
            .expect("ask the engine what is running");
        String::from_utf8_lossy(&out.stdout).contains(name)
    };
    assert!(running(&name), "the staged orphan is not running, so this proves nothing");

    // Detection is what reaps. Any container command performs one.
    let _ = Backend::choose(Want::Container, None).expect("backend");
    std::thread::sleep(std::time::Duration::from_millis(500));

    let survived = running(&name);
    if survived {
        let _ = std::process::Command::new(cli).args(["kill", &name]).output();
    }
    assert!(!survived, "a container whose owner is gone outlived the next detection");
}

/// A view naming a directory the caller never created is refused, not left to the engine.
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

// ---------------------------------------------------------------------------
// Dependencies
// ---------------------------------------------------------------------------

/// The `warm --container` path, from the image's own ABI to an import with no network.
///
/// The only test in this file that uses a network: it needs a package index the first
/// time it runs. Under `BENKYOU_REQUIRE_CONTAINER=1` it fails instead of skipping.
#[test]
fn a_warmed_set_is_the_images_own_and_mounts_read_only() {
    let Some(backend) = container() else { return };
    let Backend::Container { image, .. } = &backend else {
        panic!("container() hands back a container backend, not {}", backend.name())
    };
    // Small, pure Python, no dependencies of its own.
    let declared = Deps { python: vec!["idna==3.10".to_string()] };

    let warmed = deps::warm(&declared, false, Runtime::of(&backend))
        .expect("warm into the image's runtime")
        .expect("a declaration naming a package warms a set");
    // Keyed by the image, not by the host's ABI tag. Containment rather than equality:
    // the key's shape is the cache's business.
    let hex = image.id.trim_start_matches("sha256:");
    assert!(
        warmed.runtime.contains(&hex[..12]) && warmed.runtime.contains(&image.arch),
        "a set for an image must be keyed by that image: {} does not name {} {}",
        warmed.runtime,
        image.id,
        image.arch
    );
    assert!(warmed.path.is_dir(), "the host owns the set: {}", warmed.path.display());
    assert!(
        warmed.resolved.iter().any(|d| d == "idna==3.10"),
        "the manifest must name what landed on disk: {:?}",
        warmed.resolved
    );

    // Under the real policy, `--network none` included: a set that only worked while an
    // index was reachable passes here and fails every gate.
    let dir = scratch("deps");
    let out = backend
        .run(
            &Job::new(
                &dir,
                WORK,
                "work",
                "python3 -c 'import idna; print(idna.encode(\"benkyou.example\"))'",
                120,
            )
            .with_deps(Some(&warmed.path)),
        )
        .expect("ran");
    assert!(out.succeeded(), "the warmed set is not importable: {out:?}");
    // `encode` returns bytes, so the `b''` is what proves the package's own function ran.
    assert_eq!(out.stdout.trim(), "b'benkyou.example'", "{out:?}");

    // `mounted` is asserted first: a write that fails on an absent mount is the same red.
    let probe = format!(
        "test -d {d} && echo mounted; echo tampered > {d}/tampered",
        d = deps::GUEST_DEPS
    );
    let out = backend
        .run(&Job::new(&dir, WORK, "work", &probe, 60).with_deps(Some(&warmed.path)))
        .expect("ran");
    assert!(out.stdout.contains("mounted"), "the set was not mounted at all: {out:?}");
    assert!(!out.succeeded(), "a job wrote into the shared dependency set: {out:?}");
    assert!(
        !warmed.path.join("tampered").exists(),
        "a write reached the host's cache: {}",
        warmed.path.display()
    );

    // Already present, so nothing is fetched and nothing reaches a network.
    let again = deps::warm(&declared, false, Runtime::of(&backend))
        .expect("warm a set that is already present")
        .expect("the same declaration warms the same set");
    assert!(!again.fetched, "the second warm refetched a set already on disk: {again:?}");
    assert_eq!(again.path, warmed.path, "and it is the same set");
}
