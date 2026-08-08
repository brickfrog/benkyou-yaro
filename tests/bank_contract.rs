//! The bank is content-addressed, so the load-bearing property is that the key
//! describes the contents. Everything else here follows from that.

use std::fs;
use std::path::{Path, PathBuf};

use benkyou::bank::{self, Attestation};
use benkyou::exercise::{Env, Runner};

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bk-bank-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// Point the bank at a private directory. Passed explicitly rather than through
/// `XDG_DATA_HOME`, so these run in parallel with each other and with everything else.
fn with_bank<T>(name: &str, body: impl FnOnce(&Path) -> T) -> T {
    let bank = scratch(name);
    let out = body(&bank);
    let _ = fs::remove_dir_all(&bank);
    out
}

fn exercise(dir: &Path, answer: &str) {
    for sub in ["setup", "check", "solution"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    fs::write(
        dir.join("task.toml"),
        r#"schema_version = "1"
[task]
id = "dedupe-01"
concept_id = "sets"
kind = "kata"
guidance_level = "blank"
generated_by = "test"
[limits]
setup_secs = 30
learner_secs = 60
check_secs = 60
[verify]
cmd = "sh check/check.sh"
reward = "reward.json"
must_pass = ["correctness"]
hidden = true
[[known_bad]]
id = "wrong"
trap = "wrong"
files = { "solution.py" = "x = 1\n" }
"#,
    )
    .unwrap();
    fs::write(dir.join("instruction.md"), "# dedupe\n").unwrap();
    fs::write(dir.join("solution/solve.sh"), answer).unwrap();
    fs::write(dir.join("check/check.sh"), "#!/bin/sh\nexit 0\n").unwrap();
}

/// A passing gate record, which is what an attestation is.
fn attestation() -> Attestation {
    Attestation {
        solution_passes: true,
        empty_fails: true,
        known_bad_caught: vec!["wrong".into()],
        validated_at: "2026-08-08T00:00:00Z".into(),
        digest: String::new(),
        runner: Runner {
            semantics: benkyou::run::RUNNER_SEMANTICS,
            backend: "sandbox".into(),
            profile: "bwrap 0.11.2".into(),
        },
        env: Env::current(),
        deps: vec!["pandas==3.0.5".into()],
    }
}

/// The whole promise: what comes out of the bank hashes to the name it was filed
/// under. Anything else and a "reuse this exact exercise" feature is reusing whatever
/// happens to be sitting at that path.
#[test]
fn a_banked_bundle_hashes_to_its_own_key() {
    with_bank("roundtrip", |bank| {
        let src = scratch("roundtrip-src");
        exercise(&src, "echo one\n");
        let digest = benkyou::digest::exercise_digest(&src).unwrap();
        let task = benkyou::exercise::load(&src).unwrap();

        let banked = bank::deposit(bank, &src, &digest, &task, &attestation()).unwrap();
        assert_eq!(
            benkyou::digest::exercise_digest(&banked).unwrap(),
            digest,
            "the bundle does not hash to the directory it lives in"
        );

        // The authored files, and only those. A `.gate.json` swept in here would be
        // one machine's verdict travelling as though it were part of the exercise,
        // and `attempt` trusts that file.
        assert!(banked.join("task.toml").is_file());
        assert!(banked.join("solution/solve.sh").is_file());
        assert!(!banked.join(".gate.json").exists());
        let _ = fs::remove_dir_all(&src);
    });
}

/// A gate hashes the directory, then the bank copies it. An edit in between must not
/// be filed under the digest of what was gated.
#[test]
fn an_exercise_edited_after_gating_is_refused() {
    with_bank("drift", |bank| {
        let src = scratch("drift-src");
        exercise(&src, "echo one\n");
        let gated = benkyou::digest::exercise_digest(&src).unwrap();
        let task = benkyou::exercise::load(&src).unwrap();

        fs::write(src.join("solution/solve.sh"), "echo two\n").unwrap();

        let err = bank::deposit(bank, &src, &gated, &task, &attestation())
            .expect_err("bytes that do not match the key must be refused");
        assert!(err.contains("changed while being banked"), "{err}");
        assert!(
            bank::resolve(bank, &gated).is_err(),
            "a refused deposit must leave nothing behind"
        );
        let _ = fs::remove_dir_all(&src);
    });
}

/// Re-gating the same exercise is the event worth recording, and the old report is
/// the thing that makes it worth recording. Attestations accumulate.
#[test]
fn re_gating_appends_an_attestation_and_keeps_the_bundle() {
    with_bank("attest", |bank| {
        let src = scratch("attest-src");
        exercise(&src, "echo one\n");
        let digest = benkyou::digest::exercise_digest(&src).unwrap();
        let task = benkyou::exercise::load(&src).unwrap();

        bank::deposit(bank, &src, &digest, &task, &attestation()).unwrap();
        let mut later = attestation();
        later.validated_at = "2026-12-01T00:00:00Z".into();
        later.runner.backend = "unsafehost".into();
        bank::deposit(bank, &src, &digest, &task, &later).unwrap();

        let seen = bank::attestations(bank, &digest);
        assert_eq!(seen.len(), 2, "a re-gate replaced the earlier report");
        assert_eq!(seen[0].runner.backend, "sandbox", "history is oldest first");
        assert_eq!(seen[1].runner.backend, "unsafehost");
        assert_eq!(
            benkyou::digest::exercise_digest(&bank::resolve(bank, &digest).unwrap()).unwrap(),
            digest,
            "the bundle changed under a second deposit"
        );
        let _ = fs::remove_dir_all(&src);
    });
}

/// Nobody retypes 64 hex characters, and silently picking one of several matches
/// would hand back a different exercise than the one asked for.
#[test]
fn a_prefix_resolves_only_when_it_is_unambiguous() {
    with_bank("prefix", |bank| {
        let src = scratch("prefix-src");
        exercise(&src, "echo one\n");
        let digest = benkyou::digest::exercise_digest(&src).unwrap();
        let task = benkyou::exercise::load(&src).unwrap();
        bank::deposit(bank, &src, &digest, &task, &attestation()).unwrap();

        assert!(bank::resolve(bank, &digest[..12]).is_ok(), "a long prefix must resolve");
        assert!(bank::resolve(bank, &digest).is_ok(), "so must the whole digest");
        let err = bank::resolve(bank, "ffffffffff").expect_err("unknown digest");
        assert!(err.contains("no banked exercise"), "{err}");
        assert!(bank::resolve(bank, "../../etc").is_err(), "path traversal");
        let _ = fs::remove_dir_all(&src);
    });
}

/// An empty bank is the normal state before the first gate, not a failure.
#[test]
fn an_empty_bank_lists_nothing_without_erroring() {
    with_bank("empty", |bank| {
        assert!(bank::list(bank).unwrap().is_empty());
    });
}

/// Two gates of the same exercise at once is ordinary: a re-gate in another terminal,
/// a batch over a directory. Read-modify-write silently dropped one of them.
#[test]
fn concurrent_deposits_lose_no_attestations() {
    with_bank("concurrent", |bank| {
        let src = scratch("concurrent-src");
        exercise(&src, "echo one\n");
        let digest = benkyou::digest::exercise_digest(&src).unwrap();
        let task = benkyou::exercise::load(&src).unwrap();

        const WRITERS: usize = 16;
        std::thread::scope(|s| {
            for i in 0..WRITERS {
                let (src, digest, task) = (&src, &digest, &task);
                s.spawn(move || {
                    let mut a = attestation();
                    a.validated_at = format!("2026-08-08T00:00:{i:02}Z");
                    bank::deposit(bank, src, digest, task, &a).expect("deposit");
                });
            }
        });

        let seen = bank::attestations(bank, &digest);
        assert_eq!(seen.len(), WRITERS, "attestations were lost: {seen:#?}");
        let mut stamps: Vec<_> = seen.iter().map(|a| a.validated_at.clone()).collect();
        stamps.sort();
        stamps.dedup();
        assert_eq!(stamps.len(), WRITERS, "a line was interleaved into garbage");
        assert_eq!(
            benkyou::digest::exercise_digest(&bank::resolve(bank, &digest).unwrap()).unwrap(),
            digest,
            "concurrent staging corrupted the bundle"
        );
        let _ = fs::remove_dir_all(&src);
    });
}
