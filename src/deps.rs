//! Third-party packages an exercise's own scripts need.
//!
//! The sandbox has no network, so nothing can resolve at grade time. `benkyou warm`
//! installs an exercise's declared packages into a cache directory on the host, with the
//! network. Every later run binds that directory read-only and points `PYTHONPATH` at it.
//!
//! Five properties carry the design.
//!
//! **`--target`, not a venv.** A venv records absolute paths in `pyvenv.cfg` and in every
//! console script, so it breaks when it is bound at another path. A `--target` tree
//! relocates.
//!
//! **Keyed by the interpreter, not only the packages.** A wheel built for the wrong ABI
//! imports as a `ModuleNotFoundError` deep inside a package that is installed. Warming
//! pins `--python`, and the ABI tag is part of the set's path.
//!
//! **Every spec is checked before `uv` sees it.** This is the one command with a network.
//! It runs with the user's own rights, and its arguments come from a generated file.
//! `git+https://…`, `./thing` and `-e .` build code. A leading `-` is a flag, and
//! `--index-url` redirects the install. [`check_spec`] admits a name, optional extras and
//! one exact `==` version.
//!
//! **Exact pins only.** The key is the requirement list, so `pandas` names one directory
//! whose contents change on their own. An exact pin does not reach transitive
//! dependencies, so [`warm`] records what resolved and the gate keeps that beside its
//! verdict.
//!
//! **Wheels only, never a build.** Building an sdist runs `setup.py` on the host, from a
//! generated file. `--only-binary :all:` covers transitive dependencies too.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::digest::{hex, Sha256};
use crate::exercise::Deps;
use crate::run::Backend;
use crate::store::cache_dir;

/// Where a warmed set appears inside a job. Dotted, so it cannot collide with the
/// exercise's own `work/`, `check/` or `out/`.
pub const GUEST_DEPS: &str = "/box/.deps";

/// The interpreter a host-run job gets, because the sandbox mounts `/usr` read-only.
/// Warming against another one builds wheels the job cannot load.
pub const INTERPRETER: &str = "/usr/bin/python3";

/// The runtime a warmed set belongs to. Wheels for the wrong ABI import as a
/// `ModuleNotFoundError` inside a package that is installed.
///
/// A container key includes the image itself, because two images with one Python version
/// can carry different C libraries.
#[derive(Debug, Clone, Copy)]
pub enum Runtime<'a> {
    /// The machine's `/usr/bin/python3`, warmed by the host's `uv`.
    Host,
    /// An image's `python3`, warmed by `pip` inside that image.
    Image { cli: &'a Path, engine: &'static str, image: &'a crate::run::Image },
}

impl<'a> Runtime<'a> {
    /// The runtime the given backend runs a job in.
    pub fn of(backend: &'a Backend) -> Self {
        match backend {
            Backend::Container { cli, engine, image, .. } => {
                Runtime::Image { cli, engine, image }
            }
            _ => Runtime::Host,
        }
    }

    /// The cache segment a set for this runtime lives under.
    ///
    /// The host segment is the bare ABI tag. Changing its shape orphans every set on disk.
    fn key(&self) -> Result<String, String> {
        match self {
            Runtime::Host => Ok(abi()?.to_string()),
            Runtime::Image { cli, engine, image } => {
                let abi = image_abi(cli, engine, image)?;
                // The whole digest, and no `sha256:` prefix, because a `:` in a path
                // segment is its own problem. Changing this shape orphans warmed sets.
                let id = image.id.trim_start_matches("sha256:");
                Ok(format!("{id}-{}-{abi}", image.arch))
            }
        }
    }

    /// What to tell a reader who has to fix something here.
    fn describe(&self) -> Result<String, String> {
        match self {
            Runtime::Host => Ok(format!("{INTERPRETER} ({})", abi()?)),
            Runtime::Image { image, .. } => Ok(image.reference.clone()),
        }
    }
}

/// Admit an ABI tag, or refuse a value that must not become a directory name.
///
/// The tag comes from inside the image and becomes a host path segment that [`warm`]
/// creates and recursively removes. A leading `-` or `.` is refused, so `..` cannot occur.
fn abi_tag(raw: &str) -> Result<String, String> {
    let shaped = !raw.is_empty()
        && raw.len() <= 128
        && !raw.starts_with(['-', '.'])
        && raw.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if shaped {
        return Ok(raw.to_string());
    }
    Err(format!(
        "unusable ABI tag {raw:?}: it names a directory in the cache, so it may only be \
         letters, digits, `.`, `_` and `-`, and may not begin with `.` or `-` - a real \
         one looks like `cpython-313-x86_64-linux-gnu`. Pass --image to name an image \
         whose python3 reports one."
    ))
}

/// The ABI tag of an image's `python3`, memoised per image id.
///
/// Read from inside the image, because `python:3.13-slim` says nothing about the platform
/// triple in `SOABI`, and that triple decides whether a wheel loads.
fn image_abi(cli: &Path, engine: &str, image: &crate::run::Image) -> Result<String, String> {
    static SEEN: LazyLock<Mutex<BTreeMap<String, String>>> =
        LazyLock::new(|| Mutex::new(BTreeMap::new()));
    if let Some(tag) = SEEN.lock().unwrap_or_else(|e| e.into_inner()).get(&image.id) {
        return Ok(tag.clone());
    }

    // Bounded, named, and removed afterwards. On a timeout the client dies and the daemon
    // keeps the container, and the reaper cannot help while this process is still alive.
    let name = crate::run::container_name();
    let label = format!("{}={}", crate::run::OWNER_LABEL, std::process::id());
    let out = engine_run(
        cli,
        &[
            "run",
            "--rm",
            "--network",
            "none",
            "--name",
            &name,
            "--label",
            &label,
            "--entrypoint",
            "/bin/sh",
            &image.id,
            "-c",
            "python3 -c \"import sysconfig;print(sysconfig.get_config_var('SOABI') or 'none')\"",
        ],
        &name,
        crate::run::CONTROL_SECS,
    )?;
    let raw = out.stdout.trim().to_string();
    if !out.ok || raw.is_empty() {
        return Err(format!(
            "{engine}: {} has no usable python3 ({}). A runner image needs one for \
             [deps] to mean anything; pass --image to name another.",
            image.reference,
            out.stderr.trim()
        ));
    }
    // This value becomes a host path segment, so it goes through [`abi_tag`] first.
    let tag = abi_tag(&raw).map_err(|e| format!("{engine} image {}: {e}", image.reference))?;
    SEEN.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(image.id.clone(), tag.clone());
    Ok(tag)
}

/// The ABI tag of [`INTERPRETER`], read once per process.
///
/// `SOABI` carries the implementation, the minor version and the platform, which together
/// decide whether a compiled wheel loads.
static ABI: LazyLock<Result<String, String>> = LazyLock::new(|| {
    let out = Command::new(INTERPRETER)
        .args(["-c", "import sysconfig;print(sysconfig.get_config_var('SOABI') or 'none')"])
        .output()
        .map_err(|e| format!("{INTERPRETER}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{INTERPRETER}: could not report its ABI tag"));
    }
    // Through the gate an image's tag goes through. One rule for cache directory names is
    // easier to hold than two, and this check also refuses an empty tag.
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    abi_tag(&raw).map_err(|e| format!("{INTERPRETER}: {e}"))
});

fn abi() -> Result<&'static str, String> {
    ABI.as_ref().map(String::as_str).map_err(Clone::clone)
}

// ---------------------------------------------------------------------------
// What can be asked for
// ---------------------------------------------------------------------------

const COMPARATORS: [&str; 8] = ["===", "==", "!=", "~=", "<=", ">=", "<", ">"];

/// Admit a requirement, or say why not.
///
/// An allowlist, not a blocklist, because the dangerous forms are the ones nobody thought
/// of. Allowed: a PEP 503 name, optional `[extras]`, and version specifiers.
pub fn check_spec(spec: &str) -> Result<(), String> {
    let s = spec.trim();
    if s != spec {
        return Err(format!("`{spec}`: surrounding whitespace"));
    }
    if s.is_empty() {
        return Err("an empty requirement".to_string());
    }
    if s.starts_with('-') {
        return Err(format!("`{s}`: starts with `-`, which is a flag and not a package"));
    }
    for (bad, why) in [
        ('/', "a path"),
        ('\\', "a path"),
        ('@', "a URL or direct reference"),
        (':', "a URL scheme"),
        (';', "an environment marker"),
        ('%', "an escape"),
        ('&', "a shell character"),
        ('|', "a shell character"),
        ('$', "a shell character"),
        ('\'', "a quote"),
        ('"', "a quote"),
    ] {
        if s.contains(bad) {
            return Err(format!("`{s}`: contains `{bad}` - {why}. Registry names only."));
        }
    }
    if s.chars().any(char::is_whitespace) {
        return Err(format!("`{s}`: contains whitespace"));
    }
    if !s.is_ascii() {
        return Err(format!("`{s}`: not ASCII"));
    }

    // name [ extras ] [ specifiers ]
    let (name_extras, specifiers) = split_specifiers(s);
    let (name, extras) = match name_extras.split_once('[') {
        Some((n, rest)) => {
            let inner = rest
                .strip_suffix(']')
                .ok_or_else(|| format!("`{s}`: unclosed `[` in extras"))?;
            (n, Some(inner))
        }
        None => {
            if name_extras.contains(']') {
                return Err(format!("`{s}`: `]` without `[`"));
            }
            (name_extras, None)
        }
    };

    check_name(name).map_err(|e| format!("`{s}`: {e}"))?;
    if let Some(extras) = extras {
        if extras.is_empty() {
            return Err(format!("`{s}`: empty extras"));
        }
        for extra in extras.split(',') {
            check_name(extra).map_err(|e| format!("`{s}`: extra {e}"))?;
        }
    }
    if specifiers.is_empty() {
        return Err(format!(
            "`{s}`: no version. Pin it exactly - `{s}==<version>` - so the warmed set \
             keeps naming the same bytes."
        ));
    }
    for part in &specifiers {
        check_specifier(part).map_err(|e| format!("`{s}`: {e}"))?;
    }
    // One exact comparator among several is not a pin. `pkg==1.0,!=1.0.1` still admits
    // whatever the index decides `1.0` means today, and `==1.0.*` is a range.
    if specifiers.len() > 1 {
        return Err(format!(
            "`{s}`: {} version specifiers. An exact pin is one `==<version>` and nothing \
             else.",
            specifiers.len()
        ));
    }
    let only = specifiers[0];
    let exact = only.starts_with("===") || (only.starts_with("==") && !only.contains('*'));
    if !exact {
        return Err(format!(
            "`{s}`: not an exact pin. A range resolves to different bytes on different \
             days under one cache key; use `==<version>`."
        ));
    }
    Ok(())
}

/// Split at the first comparator outside the extras bracket. The comparator characters are
/// legal nowhere else in a name.
fn split_specifiers(s: &str) -> (&str, Vec<&str>) {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            '=' | '!' | '~' | '<' | '>' if depth == 0 => {
                return (&s[..i], s[i..].split(',').collect());
            }
            _ => {}
        }
    }
    (s, Vec::new())
}

/// PEP 503: letters, digits, and single `-`, `_`, `.` separators between them.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty package name".to_string());
    }
    let bytes = name.as_bytes();
    let ends_ok = |b: u8| b.is_ascii_alphanumeric();
    if !ends_ok(bytes[0]) || !ends_ok(bytes[bytes.len() - 1]) {
        return Err(format!("name `{name}` must start and end alphanumeric"));
    }
    if let Some(bad) = name.chars().find(|c| !c.is_ascii_alphanumeric() && !"-_.".contains(*c)) {
        return Err(format!("name `{name}` contains `{bad}`"));
    }
    Ok(())
}

fn check_specifier(part: &str) -> Result<(), String> {
    let op = COMPARATORS
        .iter()
        .find(|op| part.starts_with(**op))
        .ok_or_else(|| format!("`{part}` does not begin with a version comparator"))?;
    let version = &part[op.len()..];
    if version.is_empty() {
        return Err(format!("`{part}` has no version"));
    }
    if let Some(bad) =
        version.chars().find(|c| !c.is_ascii_alphanumeric() && !".*+!-_".contains(*c))
    {
        return Err(format!("version `{version}` contains `{bad}`"));
    }
    Ok(())
}

/// Check a whole declaration. Name every problem, not only the first.
pub fn check(deps: &Deps) -> Result<(), String> {
    let bad: Vec<String> =
        deps.python.iter().filter_map(|s| check_spec(s).err()).collect();
    if bad.is_empty() {
        return Ok(());
    }
    Err(format!("[deps] python: {}", bad.join("; ")))
}

// ---------------------------------------------------------------------------
// Where a set lives
// ---------------------------------------------------------------------------

/// A content address for a dependency list.
///
/// Sorted, so the digest names the set and not the typing order. Each entry is
/// length-prefixed, or `["ab", "c"]` and `["a", "bc"]` hash alike.
pub fn digest(python: &[String]) -> String {
    let mut sorted: Vec<&str> = python.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut h = Sha256::new();
    for spec in sorted {
        h.update(&(spec.len() as u64).to_le_bytes());
        h.update(spec.as_bytes());
    }
    hex(&h.finish())
}

/// Host directory holding a warmed set, whether or not it exists yet.
pub fn set_path(python: &[String], runtime: Runtime) -> Result<PathBuf, String> {
    Ok(cache_dir()?.join("sets").join(runtime.key()?).join(digest(python)))
}

/// The warmed set a declaration needs, or `None` when it declares no packages.
///
/// A missing set is refused here, because the grader instead fails as `CheckBroken` and
/// reads as broken. The refusal names the runtime: a host set is not an image set.
pub fn require(deps: &Deps, runtime: Runtime) -> Result<Option<PathBuf>, String> {
    if deps.python.is_empty() {
        return Ok(None);
    }
    check(deps)?;
    let path = set_path(&deps.python, runtime)?;
    if !path.is_dir() {
        return Err(format!(
            "dependencies are declared but not warmed for {} - run `benkyou warm <exercise-dir>`\n  \
             wanted: {}\n  expected at: {}",
            runtime.describe()?,
            deps.python.join(", "),
            path.display()
        ));
    }
    // A set that exists must have known contents, or the gate records a tree nobody can
    // identify.
    resolved(&path)?;
    Ok(Some(path))
}

// ---------------------------------------------------------------------------
// Filling it
// ---------------------------------------------------------------------------

/// File inside a warmed set listing what actually landed in it.
const RESOLVED: &str = ".resolved.json";

/// Every distribution installed in a set, as `name==version`, sorted.
///
/// Read from the `.dist-info` directories, not from `uv`'s output, so it covers transitive
/// dependencies the declaration never named.
fn installed(dir: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(stem) = name.strip_suffix(".dist-info") {
            if let Some((dist, version)) = stem.rsplit_once('-') {
                out.push(format!("{dist}=={version}"));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// What a warmed set resolved to.
///
/// Fallible on purpose. An empty list here makes the gate record `deps: []` for a set of
/// unknown contents. Sets this code wrote have a manifest, older ones do not.
pub fn resolved(set: &Path) -> Result<Vec<String>, String> {
    let path = set.join(RESOLVED);
    let text = fs::read_to_string(&path).map_err(|e| {
        format!(
            "{}: {e}\n  this warmed set predates its manifest or is incomplete - \
             run `benkyou warm <exercise-dir> --force`",
            path.display()
        )
    })?;
    serde_json::from_str(&text).map_err(|e| {
        format!(
            "{}: {e}\n  run `benkyou warm <exercise-dir> --force` to rebuild it",
            path.display()
        )
    })
}

/// What a warm did, for the caller to print.
#[derive(Debug)]
pub struct Warmed {
    pub python: Vec<String>,
    pub path: PathBuf,
    /// The runtime the set was built for: an ABI tag for the host, or the image key.
    pub runtime: String,
    /// Everything on disk in the set, transitive dependencies included.
    pub resolved: Vec<String>,
    /// False when the set was already present and nothing was fetched.
    pub fetched: bool,
}

/// Install a declaration's packages into the cache.
///
/// The only command in this tool that uses a network. It runs where the packages will be
/// imported, because wheels are built for one ABI, and it never runs exercise code.
pub fn warm(deps: &Deps, force: bool, runtime: Runtime) -> Result<Option<Warmed>, String> {
    if deps.python.is_empty() {
        return Ok(None);
    }
    check(deps)?;
    let key = runtime.key()?;
    let path = set_path(&deps.python, runtime)?;
    // A present set with a readable manifest is done. Without one it counts as absent,
    // because `warm` is the command that repairs that.
    if path.is_dir() && !force {
        if let Ok(resolved) = resolved(&path) {
            return Ok(Some(Warmed {
                python: deps.python.clone(),
                path,
                runtime: key,
                resolved,
                fetched: false,
            }));
        }
    }

    // From here on the key is held. The lock protects which resolved tree this key names,
    // not the directory.
    let parent = path.parent().ok_or_else(|| format!("{}: no parent", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let _lock = lock_key(&path)?;

    // Asked twice. The unlocked check above answers the common case, and this one turns a
    // wait for another warm into a cache hit.
    if !force {
        if let Ok(resolved) = resolved(&path) {
            return Ok(Some(Warmed {
                python: deps.python.clone(),
                path,
                runtime: key,
                resolved,
                fetched: false,
            }));
        }
    }

    // Renamed into place, so an interrupted warm leaves no half-installed set. Uniquely
    // named, because a stale-lock takeover lets two installs overlap.
    let staging = parent.join(scratch_name("tmp"));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;

    let out = match runtime {
        Runtime::Host => install_on_host(&deps.python, &staging),
        Runtime::Image { cli, engine, image } => {
            install_in_image(cli, engine, image, &deps.python, &staging)
        }
    };
    if let Err(e) = out {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }

    // Written before the rename, so a set that exists is a set whose manifest exists.
    let resolved = installed(&staging)?;
    let manifest = serde_json::to_string_pretty(&resolved).map_err(|e| e.to_string())?;
    fs::write(staging.join(RESOLVED), manifest)
        .map_err(|e| format!("{}: {e}", staging.display()))?;

    // A set that appeared under the lock came from a stale-lock takeover or from outside
    // this tool. A readable set still wins.
    if !force {
        if let Some(resolved) = published_meanwhile(&staging, &path) {
            return Ok(Some(Warmed {
                python: deps.python.clone(),
                path,
                runtime: key,
                resolved,
                fetched: false,
            }));
        }
    }

    if let Err(e) = publish(&staging, &path) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    Ok(Some(Warmed {
        python: deps.python.clone(),
        path,
        runtime: key,
        resolved,
        fetched: true,
    }))
}

/// A scratch directory name beside a set that no concurrent attempt can also choose.
///
/// A pid alone is not enough. Threads of one process share it, and the tests warm
/// concurrently.
fn scratch_name(what: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(".{what}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

/// How long to wait for the warm that holds this key.
const LOCK_WAIT: Duration = Duration::from_secs(20 * 60);

/// When a lock is a leftover rather than a holder. Longer than any real warm.
const LOCK_STALE: Duration = Duration::from_secs(30 * 60);

/// The right to write one cache key. Released on drop.
///
/// Exact pins do not pin what those packages require, so two warms of one key can install
/// different trees. Without this, the winner of a race decides the contents.
#[derive(Debug)]
struct KeyLock {
    dir: PathBuf,
}

impl Drop for KeyLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Take the lock for one set directory. Wait for the holder, then refuse.
///
/// A leftover lock is named and never stolen. Two waiters can both judge one lock dead,
/// and the slow holder then removes the new owner's lock.
fn lock_key(path: &Path) -> Result<KeyLock, String> {
    lock_key_for(path, LOCK_WAIT)
}

/// The same, with the wait budget named. Tests use a short one.
fn lock_key_for(path: &Path, wait: Duration) -> Result<KeyLock, String> {
    let parent = path.parent().ok_or_else(|| format!("{}: no parent", path.display()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{}: unusable set name", path.display()))?;
    let dir = parent.join(format!(".lock-{name}"));

    let waited = Instant::now();
    loop {
        match fs::create_dir(&dir) {
            Ok(()) => {
                // Diagnostic only. A pid can be reused.
                let _ = fs::write(dir.join("holder"), format!("pid {}\n", std::process::id()));
                return Ok(KeyLock { dir });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if held_too_long(&dir) || waited.elapsed() >= wait {
                    return Err(format!(
                        "{}: this set is locked by another warm.\n  If nothing is warming, \
                         remove that directory and run this again.",
                        dir.display()
                    ));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(format!("{}: {e}", dir.display())),
        }
    }
}

/// Whether a lock is older than any real warm. An unreadable timestamp counts as old.
fn held_too_long(dir: &Path) -> bool {
    fs::metadata(dir)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|age| age > LOCK_STALE).unwrap_or(false))
        .unwrap_or(true)
}

/// The set another warm published while this one was installing, if there is one.
///
/// A directory with no readable manifest is not a set and does not win, because that is
/// the case `warm` repairs.
fn published_meanwhile(staging: &Path, path: &Path) -> Option<Vec<String>> {
    if !path.is_dir() {
        return None;
    }
    let existing = resolved(path).ok()?;
    let _ = fs::remove_dir_all(staging);
    Some(existing)
}

/// Put a finished staging directory where the set belongs.
///
/// No path removes a valid set before its replacement is in place, and no failure leaves
/// the key with nothing. `remove_dir_all` then `rename` loses the set on an interruption,
/// so the replacement is renamed in first.
fn publish(staging: &Path, path: &Path) -> Result<(), String> {
    // `rename` succeeds while the destination is absent or empty, which is the common case.
    let first = match fs::rename(staging, path) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    // Any other failure is this one: a permission error, or a staging directory on another
    // filesystem. Reporting it beats reporting the move that follows.
    if !path.exists() {
        return Err(format!("{}: {first}", path.display()));
    }
    let parent = path.parent().ok_or_else(|| format!("{}: no parent", path.display()))?;
    let aside = parent.join(scratch_name("old"));
    fs::rename(path, &aside).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Err(e) = fs::rename(staging, path) {
        // The replacement did not land, so what was here goes back. A rollback fails only
        // when something else filled the destination, and then the aside copy is garbage.
        if fs::rename(&aside, path).is_err() && path.is_dir() {
            let _ = fs::remove_dir_all(&aside);
        }
        return Err(format!("{}: {e}", path.display()));
    }
    let _ = fs::remove_dir_all(&aside);
    Ok(())
}

/// The host path: `uv`, pinned to the interpreter the job will run.
fn install_on_host(specs: &[String], staging: &Path) -> Result<(), String> {
    let uv = which_uv()?;
    let mut cmd = Command::new(&uv);
    cmd.args(["pip", "install", "--only-binary", ":all:", "--python", INTERPRETER, "--target"])
        .arg(staging)
        .args(specs);
    let out = cmd.output().map_err(|e| format!("{}: {e}", uv.display()))?;
    if out.status.success() {
        return Ok(());
    }
    Err(warming_failed(specs, &String::from_utf8_lossy(&out.stderr)))
}

/// Run a named container, then remove it whatever happened.
///
/// `--rm` covers a client that exits. It does not cover a deadline: the client dies, the
/// daemon keeps the container, and the orphan reaper skips it because this process is
/// still alive.
fn engine_run(
    cli: &Path,
    args: &[&str],
    name: &str,
    secs: u64,
) -> Result<crate::run::Control, String> {
    let out = crate::run::engine_output(cli, args, secs);
    let _ = crate::run::engine_output(cli, &["rm", "--force", name], crate::run::CONTROL_SECS);
    out
}

/// The container path: `pip` inside the image, writing into a bound staging directory.
///
/// `pip` because a runner image is not required to carry `uv`. The install runs as the
/// caller's uid, because no later run can read a root-owned cache.
fn install_in_image(
    cli: &Path,
    engine: &str,
    image: &crate::run::Image,
    specs: &[String],
    staging: &Path,
) -> Result<(), String> {
    let (uid, gid) = crate::run::uid_gid()?;
    let script = format!(
        "python3 -m pip install --only-binary=:all: --no-input --no-cache-dir \
         --disable-pip-version-check --target /out {}",
        specs.join(" ")
    );

    // A pull can precede the install, so this gets the pull budget. Named and removed
    // afterwards for the same reason the ABI probe is.
    let name = crate::run::container_name();
    let tmpfs = format!("/tmp:rw,mode=1777,size={}", crate::run::TMP_BYTES);
    let user = format!("{uid}:{gid}");
    let label = format!("{}={}", crate::run::OWNER_LABEL, std::process::id());
    let mount = format!("type=bind,source={},target=/out", crate::run::mount_source(staging)?);
    let out = engine_run(
        cli,
        &[
            "run",
            "--rm",
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--user",
            &user,
            "--tmpfs",
            &tmpfs,
            "--env",
            "HOME=/tmp",
            "--env",
            "TMPDIR=/tmp",
            "--name",
            &name,
            "--label",
            &label,
            "--mount",
            &mount,
            "--entrypoint",
            "/bin/sh",
            &image.id,
            "-c",
            &script,
        ],
        &name,
        crate::run::PULL_SECS,
    )?;
    if out.ok {
        return Ok(());
    }
    // Both streams: pip reports "no matching distribution" on stdout, not stderr.
    let both = format!("{}\n{}", out.stdout, out.stderr);
    Err(format!(
        "{}\n  in {engine} image {}",
        warming_failed(specs, &both),
        image.reference
    ))
}

/// The last few lines of an installer's error output, where the reason is.
fn warming_failed(specs: &[String], err: &str) -> String {
    let tail: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).rev().take(6).collect();
    format!(
        "warming failed: {}\n{}",
        specs.join(", "),
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    )
}

fn which_uv() -> Result<PathBuf, String> {
    which("uv").ok_or_else(|| {
        "no `uv` on PATH: warming needs it to install packages. Install uv, or drop \
         [deps] from task.toml and use packages the machine already has."
            .to_string()
    })
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|d| d.join(bin)).find(|p| p.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps(specs: &[&str]) -> Deps {
        Deps { python: specs.iter().map(|s| s.to_string()).collect() }
    }

    // -- what is admitted ---------------------------------------------------

    #[test]
    fn exactly_pinned_requirements_are_admitted() {
        for spec in [
            "numpy==2.5.1",
            "zope.interface==7.0",
            "ruamel-yaml==0.18.6",
            "a==1",
            "requests[socks]==2.32.3",
            "black[d,jupyter]==24.1.0",
            "pkg===1.0",
            "pkg==1.0rc1",
            "pkg==1!2.0",
            "pkg==1.0+local",
        ] {
            assert!(check_spec(spec).is_ok(), "should admit {spec}: {:?}", check_spec(spec));
        }
    }

    /// An unpinned requirement makes one cache key name different bytes on different days.
    #[test]
    fn unpinned_and_ranged_requirements_are_refused() {
        for spec in [
            "pandas",
            "idna",
            "pandas>=2.2,<3",
            "pkg~=1.4",
            "pkg!=1.0",
            "pkg>1",
            "pkg==1.0.*",
            "pkg==1.0,!=1.0.1",
            "pkg>=1.0,==1.2",
        ] {
            let err = check_spec(spec).expect_err("must refuse {spec}");
            assert!(
                err.contains("pin") || err.contains("exact") || err.contains("specifiers"),
                "refusal should name the rule for {spec}: {err}"
            );
        }
    }

    // -- what is refused, and why it matters --------------------------------

    /// Each of these runs code on the host during a warm, or redirects where packages come
    /// from.
    #[test]
    fn code_execution_and_redirection_are_refused() {
        for spec in [
            "git+https://example.invalid/x",
            "git+ssh://example.invalid/x",
            "./local",
            "../escape",
            "/abs/path",
            "pkg @ https://example.invalid/x.whl",
            "https://example.invalid/x.tar.gz",
            "file:///tmp/x",
            "-e .",
            "-e/tmp/x",
            "--index-url=https://example.invalid",
            "--find-links=/tmp",
            "-r requirements.txt",
        ] {
            assert!(check_spec(spec).is_err(), "must refuse {spec}");
        }
    }

    #[test]
    fn shell_metacharacters_are_refused() {
        for spec in ["pkg;rm -rf /", "pkg&&curl x", "pkg|sh", "pkg$(id)", "pkg'x", "pkg\"x"] {
            assert!(check_spec(spec).is_err(), "must refuse {spec}");
        }
    }

    /// Admitting a marker means admitting the characters that carry the dangerous forms.
    #[test]
    fn markers_and_whitespace_are_refused() {
        for spec in ["pkg; python_version<'3.9'", "pkg ", " pkg", "two pkgs", ""] {
            assert!(check_spec(spec).is_err(), "must refuse {spec:?}");
        }
    }

    #[test]
    fn malformed_names_and_versions_are_refused() {
        for spec in ["-pkg", "pkg-", ".pkg", "pkg==", "pkg=1.0", "pkg<>1", "pkg[", "pkg]", "pkg[]", "påckage"] {
            assert!(check_spec(spec).is_err(), "must refuse {spec}");
        }
    }

    #[test]
    fn a_whole_declaration_names_every_problem() {
        let err = check(&deps(&["pandas==3.0.5", "./bad", "git+https://x.invalid/y"])).unwrap_err();
        assert!(err.contains("./bad"), "{err}");
        assert!(err.contains("git+"), "{err}");
    }

    #[test]
    fn a_valid_declaration_passes() {
        check(&deps(&["pandas==3.0.5", "numpy==2.5.1"])).expect("both are exact pins");
    }

    /// The check guards the network command, so it runs before it.
    #[test]
    fn warming_refuses_a_bad_spec_without_touching_the_network() {
        let err = warm(&deps(&["git+https://example.invalid/x"]), false, Runtime::Host).unwrap_err();
        assert!(err.contains("git+https://example.invalid/x"), "must name the spec: {err}");
        assert!(err.contains("Registry names only"), "must say what is allowed: {err}");
    }

    #[test]
    fn requiring_refuses_a_bad_spec_too() {
        let err = require(&deps(&["-e ."]), Runtime::Host).unwrap_err();
        assert!(err.contains("flag"), "{err}");
    }

    // -- set identity -------------------------------------------------------

    #[test]
    fn a_digest_names_the_set_and_not_the_typing_order() {
        assert_eq!(
            digest(&deps(&["pandas==3.0.5", "idna==3.18"]).python),
            digest(&deps(&["idna==3.18", "pandas==3.0.5"]).python)
        );
    }

    #[test]
    fn a_repeated_package_is_the_same_set() {
        assert_eq!(
            digest(&deps(&["pandas==3.0.5"]).python),
            digest(&deps(&["pandas==3.0.5", "pandas==3.0.5"]).python)
        );
    }

    /// Without length prefixes these two collide, and two exercises share one directory.
    #[test]
    fn concatenation_is_not_ambiguous() {
        assert_ne!(digest(&deps(&["ab", "c"]).python), digest(&deps(&["a", "bc"]).python));
    }

    #[test]
    fn a_version_pin_is_a_different_set() {
        assert_ne!(
            digest(&deps(&["pandas==3.0.5"]).python),
            digest(&deps(&["pandas==2.2.3"]).python)
        );
    }

    #[test]
    fn an_empty_declaration_needs_nothing() {
        let none = Deps::default();
        assert!(require(&none, Runtime::Host).expect("no deps is not an error").is_none());
        assert!(warm(&none, false, Runtime::Host).expect("nothing to warm").is_none());
    }

    // -- the manifest is the set's identity --------------------------------

    /// A directory with no manifest has unknown contents. The gate must not record it as
    /// `deps: []`.
    #[test]
    fn a_set_without_a_manifest_cannot_be_read() {
        let dir = std::env::temp_dir().join(format!("benkyou-set-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        let err = resolved(&dir).expect_err("an unidentifiable set must not read as empty");
        assert!(err.contains("--force"), "must name the repair: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_manifest_cannot_be_read() {
        let dir = std::env::temp_dir().join(format!("benkyou-set-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join(RESOLVED), "{not json").expect("write");
        let err = resolved(&dir).expect_err("corruption is not an empty set");
        assert!(err.contains("--force"), "must name the repair: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manifest_round_trips() {
        let dir = std::env::temp_dir().join(format!("benkyou-set-ok-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join(RESOLVED), r#"["numpy==2.5.1","pandas==3.0.5"]"#).expect("write");
        assert_eq!(resolved(&dir).expect("reads"), vec!["numpy==2.5.1", "pandas==3.0.5"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A set answers for what is on disk, transitive packages included.
    #[test]
    fn installed_reads_every_dist_info_including_transitives() {
        let dir = std::env::temp_dir().join(format!("benkyou-inst-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for d in ["pandas-3.0.5.dist-info", "numpy-2.5.1.dist-info", "pandas"] {
            fs::create_dir_all(dir.join(d)).expect("dir");
        }
        assert_eq!(installed(&dir).expect("reads"), vec!["numpy==2.5.1", "pandas==3.0.5"]);
        let _ = fs::remove_dir_all(&dir);
    }

    // -- what can name a cache directory ------------------------------------

    /// The tag comes out of the image and becomes a host path segment that `warm` creates
    /// and recursively removes.
    #[test]
    fn a_hostile_abi_tag_is_refused() {
        let long = "a".repeat(129);
        for raw in [
            "../escape",
            "..",
            ".",
            "a/b",
            "/abs",
            "",
            "cpython-313\nx",
            "cpython-313\n",
            "-flag",
            ".hidden",
            "cpython 313",
            "cpython-313;rm -rf /",
            "$(id)",
            long.as_str(),
        ] {
            let err = abi_tag(raw).expect_err("must refuse {raw:?}");
            assert!(err.contains("--image"), "must name what to change for {raw:?}: {err}");
            assert!(err.contains(&format!("{raw:?}")), "must show the value: {err}");
        }
    }

    #[test]
    fn a_real_abi_tag_passes_through_unchanged() {
        for raw in ["cpython-313-x86_64-linux-gnu", "cpython-314t-aarch64-linux-gnu", "none"] {
            assert_eq!(abi_tag(raw).expect("a real tag"), raw);
        }
    }

    // -- publication --------------------------------------------------------

    /// The replacement lands before the old set goes, and no staging or aside copy is left.
    #[test]
    fn publishing_over_a_set_replaces_it_and_leaves_nothing_behind() {
        let root = std::env::temp_dir().join(format!("benkyou-pub-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("0123456789abcdef");
        fs::create_dir_all(&path).expect("dir");
        fs::write(path.join(RESOLVED), r#"["old==1.0"]"#).expect("write");

        let staging = root.join(scratch_name("tmp"));
        fs::create_dir_all(&staging).expect("dir");
        fs::write(staging.join(RESOLVED), r#"["new==2.0"]"#).expect("write");

        publish(&staging, &path).expect("publishes over an existing set");

        assert_eq!(resolved(&path).expect("the new manifest is readable"), vec!["new==2.0"]);
        let left: Vec<String> = fs::read_dir(&root)
            .expect("read")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["0123456789abcdef".to_string()], "leftovers: {left:?}");
        let _ = fs::remove_dir_all(&root);
    }

    /// The first warm of a set has nothing to move aside, and must still be one rename.
    #[test]
    fn publishing_into_an_absent_destination_works() {
        let root = std::env::temp_dir().join(format!("benkyou-pub-new-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("fedcba9876543210");
        let staging = root.join(scratch_name("tmp"));
        fs::create_dir_all(&staging).expect("dir");
        fs::write(staging.join(RESOLVED), r#"["new==2.0"]"#).expect("write");

        publish(&staging, &path).expect("publishes into an empty parent");

        assert_eq!(resolved(&path).expect("manifest"), vec!["new==2.0"]);
        assert!(!staging.exists(), "the staging directory was renamed, not copied");
        let _ = fs::remove_dir_all(&root);
    }

    /// If the replacement cannot land, the set that was already there survives. An older
    /// publication removed the destination first and left nothing.
    #[test]
    fn a_failed_publication_leaves_the_existing_set_alone() {
        let root = std::env::temp_dir().join(format!("benkyou-pub-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("0123456789abcdef");
        fs::create_dir_all(&path).expect("dir");
        fs::write(path.join(RESOLVED), r#"["old==1.0"]"#).expect("write");

        // Nothing to publish. Whatever else this does, it must not delete the set.
        publish(&root.join(scratch_name("tmp")), &path).expect_err("there is no staging dir");

        assert_eq!(resolved(&path).expect("the old manifest still reads"), vec!["old==1.0"]);
        let left: Vec<String> = fs::read_dir(&root)
            .expect("read")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["0123456789abcdef".to_string()], "leftovers: {left:?}");
        let _ = fs::remove_dir_all(&root);
    }

    /// A warm that finishes second reports the published set and removes its own copy.
    #[test]
    fn a_set_published_while_warming_wins_and_the_loser_cleans_up() {
        let root = std::env::temp_dir().join(format!("benkyou-race-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("0123456789abcdef");
        fs::create_dir_all(&path).expect("dir");
        fs::write(path.join(RESOLVED), r#"["first==1.0"]"#).expect("write");

        let staging = root.join(scratch_name("tmp"));
        fs::create_dir_all(&staging).expect("dir");
        fs::write(staging.join(RESOLVED), r#"["second==1.0"]"#).expect("write");

        let existing = published_meanwhile(&staging, &path).expect("the published set wins");
        assert_eq!(existing, vec!["first==1.0"], "the reported set is the one on disk");
        assert_eq!(resolved(&path).expect("untouched"), vec!["first==1.0"]);
        let left: Vec<String> = fs::read_dir(&root)
            .expect("read")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["0123456789abcdef".to_string()], "leftovers: {left:?}");
        let _ = fs::remove_dir_all(&root);
    }

    /// An unreadable directory is not a set, so the staging copy survives to replace it.
    #[test]
    fn an_unreadable_directory_does_not_win_the_race() {
        let root = std::env::temp_dir().join(format!("benkyou-race-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let path = root.join("0123456789abcdef");
        fs::create_dir_all(&path).expect("dir");

        let staging = root.join(scratch_name("tmp"));
        fs::create_dir_all(&staging).expect("dir");
        fs::write(staging.join(RESOLVED), r#"["new==2.0"]"#).expect("write");

        assert!(published_meanwhile(&staging, &path).is_none(), "an unidentifiable set");
        assert!(staging.exists(), "the staging directory is still needed");
        publish(&staging, &path).expect("replaces the unreadable directory");
        assert_eq!(resolved(&path).expect("manifest"), vec!["new==2.0"]);
        let _ = fs::remove_dir_all(&root);
    }

    // -- one writer per key -------------------------------------------------

    fn key_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bk-lock-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("root");
        d.join("set")
    }

    /// Two warms of one key cannot both install. The second names the lock and refuses.
    #[test]
    fn a_second_writer_is_refused_and_told_where_the_lock_is() {
        let path = key_dir("busy");
        let held = lock_key_for(&path, Duration::from_millis(10)).expect("first writer");
        let err = lock_key_for(&path, Duration::from_millis(10)).expect_err("second writer");
        assert!(err.contains(".lock-set"), "{err}");
        assert!(err.contains("remove that directory"), "{err}");
        drop(held);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// The lock is released on drop, so the next warm proceeds.
    #[test]
    fn dropping_the_lock_frees_the_key() {
        let path = key_dir("freed");
        let lock = lock_key_for(&path, Duration::from_millis(10)).expect("first writer");
        let dir = lock.dir.clone();
        assert!(dir.is_dir(), "the lock directory was not created");
        drop(lock);
        assert!(!dir.exists(), "the lock outlived its holder");
        let again = lock_key_for(&path, Duration::from_millis(10));
        assert!(again.is_ok(), "a freed key stayed locked: {again:?}");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A fresh lock is a holder, not a leftover. Judging it stale is how takeover started.
    #[test]
    fn a_fresh_lock_is_not_stale() {
        let path = key_dir("fresh");
        let lock = lock_key_for(&path, Duration::from_millis(10)).expect("first writer");
        assert!(!held_too_long(&lock.dir), "a lock made now was called a leftover");
        assert!(held_too_long(&path.with_extension("missing")), "an absent lock reads as old");
        drop(lock);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
