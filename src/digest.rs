//! Content digest for an exercise directory.
//!
//! The gate's verdict is about a *specific* set of files. Without binding the two
//! together, `validated_at` outlives the thing it validated: edit a hidden case after
//! gating and the exercise stays showable, with a grader nobody proved discriminating
//! deciding what the learner sees next. So the gate records what it looked at, and
//! anything that shows an exercise re-derives that digest and refuses a mismatch.
//!
//! SHA-256 is implemented here rather than taken as a dependency, for the same reason
//! the HTTP server is: it is a fixed, fully specified algorithm with published test
//! vectors, and it costs less to write than the five crates behind `sha2` cost to
//! carry. The vectors are in the tests below.
//!
//! This is a staleness check, not an authenticity one. It answers "are these the bytes
//! the gate ran?", which is a question about accidents - an edit, a half-finished
//! rewrite, a `git checkout` between gating and sitting down. There is no signature and
//! no secret: someone who can rewrite `check/` can equally rewrite the digest beside
//! it, and on a single-user machine that is not a threat, it is just the user.

use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// SHA-256 (FIPS 180-4)
// ---------------------------------------------------------------------------

#[rustfmt::skip]
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    buffered: usize,
    /// Message length in bits, which is what the padding encodes.
    bits: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 { h: H0, buf: [0; 64], buffered: 0, bits: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.bits = self.bits.wrapping_add((data.len() as u64) * 8);
        // Top up a partial block first; only then take whole blocks straight from
        // the input without copying them.
        if self.buffered > 0 {
            let want = 64 - self.buffered;
            let take = want.min(data.len());
            self.buf[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buffered = 0;
            }
        }
        while data.len() >= 64 {
            let (block, rest) = data.split_at(64);
            self.compress(block.try_into().expect("64 bytes"));
            data = rest;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buffered = data.len();
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.bits;
        self.update_unlen(&[0x80]);
        while self.buffered != 56 {
            self.update_unlen(&[0x00]);
        }
        self.update_unlen(&bits.to_be_bytes());
        let mut out = [0u8; 32];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.h.iter()) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// `update` without advancing the length counter, for padding bytes.
    fn update_unlen(&mut self, data: &[u8]) {
        let bits = self.bits;
        self.update(data);
        self.bits = bits;
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(chunk.try_into().expect("4 bytes"));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in self.h.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(v);
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).expect("nibble"));
        s.push(char::from_digit((b & 0xf) as u32, 16).expect("nibble"));
    }
    s
}

// ---------------------------------------------------------------------------
// The exercise digest
// ---------------------------------------------------------------------------

/// Top-level files whose bytes decide whether a gate verdict still applies.
///
/// `instruction.md` is here because an exercise whose prose changed is a different
/// exercise even when every byte of code is the same: the grader still passes, and the
/// learner is now answering a different question.
const FILES: [&str; 2] = ["task.toml", "instruction.md"];

/// Directories hashed in full.
///
/// `solution/` is included even though the learner never sees it: the gate's first
/// direction is a claim about that script, and a rewritten reference is a different
/// claim.
const DIRS: [&str; 3] = ["setup", "check", "solution"];

/// Hash the content of an exercise directory, byte for byte.
///
/// Everything an exercise is made of is hashed exactly as written - comments,
/// formatting, key order and any section this binary does not yet parse. That is
/// possible only because the gate's verdict lives in a sidecar rather than inside
/// `task.toml`: nothing here is ever rewritten by the tool, so there is no
/// chicken-and-egg between hashing a file and stamping it, and no canonical form to
/// argue about.
///
/// What is deliberately *not* here is the execution environment. That exclusion is a
/// real limitation and not a claim of completeness: an exercise whose interpreter or
/// installed packages moved underneath it has changed in a way this digest cannot see,
/// and it shows up as a failing grade rather than a stale gate. Hashing it was
/// considered and rejected - the closure is not enumerable from the exercise
/// directory, and any approximation would ungate every exercise on the machine the
/// next time anything unrelated moved.
pub fn exercise_digest(dir: &Path) -> Result<String, String> {
    let mut h = Sha256::new();

    for name in FILES {
        let path = dir.join(name);
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        feed(&mut h, name, 0, &bytes);
    }

    for name in DIRS {
        let root = dir.join(name);
        if !root.exists() {
            continue;
        }
        let mut entries = Vec::new();
        walk(&root, name, &mut entries)?;
        // Sorted so the digest does not depend on directory iteration order, which is
        // filesystem-dependent and not stable across machines.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (rel, mode, bytes) in entries {
            feed(&mut h, &rel, mode, &bytes);
        }
    }

    Ok(hex(&h.finish()))
}

/// One record in the hashed stream.
///
/// The path and the length are hashed alongside the content so that moving a file, or
/// splitting one file into two whose bytes concatenate to the original, is a different
/// digest rather than the same one.
fn feed(h: &mut Sha256, rel: &str, mode: u8, bytes: &[u8]) {
    h.update(&[mode]);
    h.update(&(rel.len() as u64).to_le_bytes());
    h.update(rel.as_bytes());
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

/// Mode byte: 0 plain file, 1 executable, 2 symlink.
///
/// The executable bit is content here, not metadata - `copy_dir` preserves it, and a
/// `check.sh` that lost it is an exercise that no longer runs the same way.
fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, u8, Vec<u8>)>) -> Result<(), String> {
    let read = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in read {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = format!("{prefix}/{name}");
        // Not `metadata`: that follows links, and a link to a file outside the
        // exercise would be hashed as if its content lived here.
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            out.push((rel, 2, target.to_string_lossy().into_owned().into_bytes()));
        } else if meta.is_dir() {
            walk(&path, &rel, out)?;
        } else {
            let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            out.push((rel, executable(&meta), bytes));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn executable(meta: &fs::Metadata) -> u8 {
    use std::os::unix::fs::PermissionsExt;
    u8::from(meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(_meta: &fs::Metadata) -> u8 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(s: &str) -> String {
        let mut h = Sha256::new();
        h.update(s.as_bytes());
        hex(&h.finish())
    }

    #[test]
    fn matches_the_published_vectors() {
        // FIPS 180-4 / NIST CAVP.
        assert_eq!(
            digest_of(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest_of("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            digest_of("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_million_a_s() {
        // The long vector: exercises the multi-block path and the length counter.
        let mut h = Sha256::new();
        for _ in 0..1000 {
            h.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(&h.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn update_is_chunking_independent() {
        // The same bytes fed in awkward splits must give the same digest, or the
        // buffering is wrong in a way no single-call test would show.
        let data: Vec<u8> = (0..500u32).map(|i| (i % 251) as u8).collect();
        let mut whole = Sha256::new();
        whole.update(&data);
        let expected = hex(&whole.finish());
        for chunk in [1usize, 7, 63, 64, 65, 127] {
            let mut h = Sha256::new();
            for part in data.chunks(chunk) {
                h.update(part);
            }
            assert_eq!(hex(&h.finish()), expected, "chunk size {chunk}");
        }
    }

    #[test]
    fn the_stream_separates_fields() {
        // "ab" + "" must not collide with "a" + "b": the reason lengths are hashed.
        let mut a = Sha256::new();
        feed(&mut a, "x", 0, b"ab");
        feed(&mut a, "y", 0, b"");
        let mut b = Sha256::new();
        feed(&mut b, "x", 0, b"a");
        feed(&mut b, "y", 0, b"b");
        assert_ne!(hex(&a.finish()), hex(&b.finish()));
    }
}
