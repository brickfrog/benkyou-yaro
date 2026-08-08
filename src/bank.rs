//! The content-addressed exercise bank.
//!
//! An exercise directory is caller-owned and usually lives in `/tmp`. That was fine
//! while an exercise was assumed to be consumed by being solved: the graph recorded
//! that you practised a concept, and the material could go. It is not fine once you
//! want to sit down to the *same* task again and compare, or explain a score you got
//! last month, or check whether the grader that produced it was any good. The system
//! remembered the practice and threw away the instrument.
//!
//! So: gating an exercise banks it. The digest is already the key — [`crate::digest`]
//! hashes the authored bytes and the gate verdict is bound to that hash — and the
//! bundle is written under it, immutable once there.
//!
//! **Bundle and attestation are separate on purpose.** The bundle is the exercise:
//! same bytes, same digest, forever. An attestation is one machine's report that the
//! gate passed here, on this interpreter, against this dependency set, on this date.
//! A bundle collects attestations over time and never loses the old ones, because
//! "this passed on the laptop in March" stays true after it stops passing on the
//! desktop in August. Folding them together would mean a re-gate silently rewriting
//! the record of what an earlier verdict was actually about.
//!
//! Nothing here re-runs a grader or decides whether a banked exercise is still good.
//! That is [`crate::gate`]'s job, and the bank exists to give it something to be
//! asked about.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::exercise::{Gate, Task};

/// What the bundle is, beyond its bytes. Written once, beside the copied files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// Content digest of the authored files: the directory name, repeated inside so a
    /// bundle moved by hand can be checked against where it landed.
    pub digest: String,
    pub concept_id: String,
    pub kind: String,
    /// The directory name the exercise was authored under. Not an identity — two
    /// unrelated exercises may share a slug — but the only human-readable handle
    /// there is.
    pub slug: String,
    pub banked_at: String,
}

/// One machine's report that the gate passed here.
///
/// This *is* the [`Gate`] record, not a summary of one. The temptation was a tidy
/// `{ at, env, backend, deps }` triple, and it cannot do the job: deciding whether a
/// banked exercise is showable on this machine runs through `Runner::stale`, which
/// compares the exact runner the gate used, and a string like `"sandbox"` throws away
/// what that comparison needs. Storing anything less than the whole record means
/// reconstructing a verdict from a lossy copy of it.
///
/// Never replaced. A bundle that passes today and fails after a Python upgrade has two
/// true attestations, and the pair is the useful record.
pub type Attestation = Gate;

const META: &str = "meta.json";
const ATTESTATIONS: &str = "attestations.jsonl";

/// `$XDG_DATA_HOME/benkyou/items`.
///
/// Data, not cache: a bundle is the evidence behind a score, and re-generating one
/// produces a *different* exercise rather than the same bytes back.
/// The env read. Kept to one line of impurity, like [`crate::store::data_dir`], so
/// every function below is testable without mutating process-global environment from
/// a threaded harness.
pub fn bank_dir() -> Result<PathBuf, String> {
    Ok(crate::store::data_dir()?.join("items"))
}

/// Where one digest lives under `bank`.
pub fn item_dir(bank: &Path, digest: &str) -> Result<PathBuf, String> {
    Ok(bank.join(check_digest(digest)?))
}

/// A digest is a directory name, so it has to be one before it is joined to a path.
///
/// Lowercase hex only. This is not defence against a hostile caller — the digests
/// come from our own hasher — it is what stops a mistyped argument from resolving to
/// `..` and reading somewhere else entirely.
fn check_digest(digest: &str) -> Result<&str, String> {
    if digest.len() < 8 || digest.len() > 64 {
        return Err(format!("`{digest}`: not a digest"));
    }
    if !digest.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return Err(format!("`{digest}`: not lowercase hex"));
    }
    Ok(digest)
}

/// Copy a validated exercise into the bank and record one attestation.
///
/// Idempotent on the bundle: a digest already banked keeps the bytes it has, because
/// they are by definition the same bytes. The attestation is appended regardless —
/// re-gating the same exercise on a second machine is exactly the event worth keeping.
pub fn deposit(
    bank: &Path,
    exercise_dir: &Path,
    digest: &str,
    task: &Task,
    attestation: &Attestation,
) -> Result<PathBuf, String> {
    let dir = item_dir(bank, digest)?;
    let already = dir.join(META).exists();
    if !already {
        // Staged under a sibling and renamed, so a bundle is either wholly present or
        // wholly absent. A half-copied directory would carry a digest naming content
        // it does not hold.
        // Unique per *call*, not per process. A pid-keyed name collides between two
        // threads banking the same digest, and the first thing this block does is
        // `remove_dir_all`, so one caller deletes the tree another is copying into -
        // which surfaces as a spurious "changed while being banked" or a vanished
        // source file. The counter costs nothing and removes the whole class.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let ticket = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let staging = bank.join(format!(
            ".staging-{}-{}-{ticket}",
            digest,
            std::process::id()
        ));
        if let Some(parent) = staging.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;

        // Exactly the files the digest covers, and nothing else. Copying the whole
        // directory would sweep in the `.gate.json` written moments earlier, so every
        // learner pulling this bundle would inherit one machine's verdict as though it
        // were a property of the exercise - and `attempt`, which trusts that sidecar,
        // would skip re-gating on a box the exercise had never run on. What a gate
        // proved is per-environment and belongs in `attestations.jsonl`, which is why
        // that file exists. Keeping the bundle equal to the digest's input is also
        // what makes the directory name checkable against its contents.
        for name in crate::digest::FILES {
            let src = exercise_dir.join(name);
            if src.is_file() {
                fs::copy(&src, staging.join(name))
                    .map_err(|e| format!("{}: {e}", src.display()))?;
            }
        }
        for name in crate::digest::DIRS {
            let src = exercise_dir.join(name);
            if src.is_dir() {
                crate::gate::copy_dir(&src, &staging.join(name))?;
            }
        }

        // The key has to describe the contents, or the store is not content-addressed
        // and every later reuse is trusting a name. Re-hash what was actually written,
        // which catches two different things: an author editing the directory between
        // the gate's hash and this copy, and any future change to what gets copied
        // drifting away from what the digest covers. Refusing here costs a re-gate;
        // accepting would put wrong bytes under a trusted key, and nothing downstream
        // ever looks again.
        let copied = crate::digest::exercise_digest(&staging)?;
        if copied != digest {
            let _ = fs::remove_dir_all(&staging);
            return Err(format!(
                "{}: changed while being banked - gated {digest}, copy hashes {copied}",
                exercise_dir.display()
            ));
        }
        let meta = Meta {
            digest: digest.to_string(),
            concept_id: task.task.concept_id.clone(),
            kind: format!("{:?}", task.task.kind).to_lowercase(),
            slug: exercise_dir
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| digest.to_string()),
            banked_at: crate::store::now_iso(),
        };
        fs::write(
            staging.join(META),
            serde_json::to_string_pretty(&meta).map_err(|e| e.to_string())? + "\n",
        )
        .map_err(|e| format!("{}: {e}", staging.join(META).display()))?;
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        match fs::rename(&staging, &dir) {
            Ok(()) => {}
            // Another process banked the same digest between the check and the rename.
            // Same bytes either way, so the loser cleans up and carries on.
            Err(_) if dir.join(META).exists() => {
                let _ = fs::remove_dir_all(&staging);
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(format!("{}: {e}", dir.display()));
            }
        }
    }

    // Opened in append mode and written as one line, rather than read-modify-written.
    // Two processes gating the same exercise at once is ordinary — a re-gate on a
    // second terminal, a batch over a directory — and read-then-write loses whichever
    // report lands second. `O_APPEND` puts the seek and the write under one atomic
    // step per file, so concurrent writers interleave lines instead of overwriting
    // each other's. It also means the file is only ever extended, which is what makes
    // "never lose an old attestation" a property of the storage rather than a promise
    // in a comment.
    use std::io::Write;
    let mut line = serde_json::to_string(attestation).map_err(|e| e.to_string())?;
    line.push('\n');
    let path = dir.join(ATTESTATIONS);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(dir)
}

/// Read one bundle's metadata.
pub fn meta(bank: &Path, digest: &str) -> Result<Meta, String> {
    let path = item_dir(bank, digest)?.join(META);
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Every attestation for one bundle, oldest first.
///
/// A line that will not parse is skipped rather than failing the read: the file is
/// append-only history, and one bad line must not hide the rest of it.
pub fn attestations(bank: &Path, digest: &str) -> Vec<Attestation> {
    match item_dir(bank, digest) {
        Ok(dir) => read_attestations(&dir),
        Err(_) => Vec::new(),
    }
}

/// The newest attestation for a bundle *directory*, or `None` if it is not one.
///
/// Takes a path rather than a digest because the caller is `exercise::read_gate`,
/// which has been handed a directory and does not know whether it came out of the
/// bank. Presence of `meta.json` is what makes it a bundle; anything else is somebody
/// else's directory and gets `None`.
///
/// Newest wins on the reasonable assumption that the last gate ran under the most
/// current conditions. If it is stale for this machine, `require_current` says so and
/// names the fix, which is a better outcome than searching the history for whichever
/// old verdict happens to match and showing an exercise on the strength of it.
pub fn newest_attestation(dir: &Path) -> Option<Gate> {
    if !dir.join(META).is_file() {
        return None;
    }
    read_attestations(dir).pop()
}

/// Parse an `attestations.jsonl` out of a bundle directory, oldest first.
///
/// A line that will not parse is skipped rather than failing the read: the file is
/// append-only history, and one bad line must not hide the rest of it.
fn read_attestations(dir: &Path) -> Vec<Attestation> {
    let Ok(text) = fs::read_to_string(dir.join(ATTESTATIONS)) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Every banked bundle, by digest.
///
/// Skips anything without readable metadata, including the staging directories of a
/// run that died mid-copy. Sorted by digest so the listing is diffable.
pub fn list(bank: &Path) -> Result<BTreeMap<String, Meta>, String> {
    let dir = bank;
    let mut out = BTreeMap::new();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        // An empty bank is not an error: nothing has been gated yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if check_digest(&name).is_err() {
            continue;
        }
        if let Ok(m) = meta(bank, &name) {
            out.insert(name, m);
        }
    }
    Ok(out)
}

/// Turn a digest, or an unambiguous prefix of one, into a bundle directory.
///
/// A prefix is accepted because a 64-character hex string is not something anyone
/// retypes, and refused when it matches more than one bundle — silently picking the
/// first would hand back a different exercise than the one asked for.
pub fn resolve(bank: &Path, prefix: &str) -> Result<PathBuf, String> {
    let prefix = prefix.trim();
    check_digest(prefix)?;
    let exact = item_dir(bank, prefix)?;
    if exact.join(META).exists() {
        return Ok(exact);
    }
    let hits: Vec<String> = list(bank)?
        .into_keys()
        .filter(|d| d.starts_with(prefix))
        .collect();
    match hits.len() {
        0 => Err(format!("`{prefix}`: no banked exercise with that digest")),
        1 => item_dir(bank, &hits[0]),
        _ => Err(format!(
            "`{prefix}` matches {} banked exercises - use more characters",
            hits.len()
        )),
    }
}

/// One bundle rendered for the CLI: what it is, and what is known about running it.
pub fn describe(bank: &Path, digest: &str, m: &Meta) -> Value {
    let seen = attestations(bank, digest);
    serde_json::json!({
        "digest": digest,
        "concept": m.concept_id,
        "kind": m.kind,
        "slug": m.slug,
        "banked_at": m.banked_at,
        "gated": seen.len(),
        // The most recent report only. The whole history is on disk for anyone who
        // needs it; a listing that grew with every re-gate would stop being a listing.
        "last_gate": seen.last().map(|a| serde_json::json!({
            "at": a.validated_at,
            "benkyou": a.env.benkyou,
            "backend": a.runner.backend,
            "runner": a.runner.profile,
            "deps": a.deps,
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_must_be_hex_of_a_plausible_length() {
        assert!(check_digest("abc123def").is_ok());
        assert!(check_digest("../../etc").is_err(), "path traversal");
        assert!(check_digest("ABC123DEF").is_err(), "uppercase");
        assert!(check_digest("abc").is_err(), "too short to be unambiguous");
        assert!(check_digest(&"a".repeat(65)).is_err(), "longer than sha256");
        assert!(check_digest("abc/123/x").is_err(), "separator");
    }
}
