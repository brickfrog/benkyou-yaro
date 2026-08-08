//! Third-party packages an exercise's own scripts need.
//!
//! The sandbox has no network, which cost the tool per-exercise dependencies: a
//! `pip install` at grade time fails, and a PEP 723 header under `uv run` resolves
//! against nothing. The recovery is to move the one step that needs a network *out* of
//! the sandbox and out of the grading path entirely, into a command a person runs on
//! purpose.
//!
//! `benkyou warm` installs an exercise's declared packages into a directory under the
//! cache, on the host, with the network. Every later run binds that directory read-only
//! and points `PYTHONPATH` at it. Nothing resolves at grade time, nothing writes to the
//! set, and no generated script ever reaches a network.
//!
//! Four properties are load-bearing and none is obvious.
//!
//! **`--target`, not a venv.** A venv records absolute paths in `pyvenv.cfg` and in
//! every console script, so one built at a cache path and bound somewhere else is
//! subtly broken. An installed `--target` directory is a plain tree of packages and is
//! relocatable, which is what lets the same bytes appear at a fixed guest path.
//!
//! **The set is keyed by the interpreter, not just the packages.** `uv` will happily
//! install a `cpython-313` wheel for a sandbox that runs 3.14, and the failure is a
//! `ModuleNotFoundError` deep inside numpy rather than anything naming the real cause.
//! Warming pins `--python` to the interpreter the sandbox will use, and the ABI tag is
//! part of the set's path, so an interpreter upgrade misses the cache and says so
//! instead of loading wheels built for the wrong ABI.
//!
//! **Every spec is validated before `uv` sees it.** This is the one command with a
//! network, it runs on the host with the user's own rights, and its argument list comes
//! out of a *generated* file. `git+https://…` clones and builds. `./thing` and `-e .`
//! build from a path. A leading `-` is not a package at all - it is a flag, and
//! `--index-url` is enough to redirect the whole install. [`check_spec`] admits a
//! deliberately small subset of PEP 508 - a name, optional extras, and exactly one
//! exact `==` version - and refuses everything else.
//!
//! **Exact pins only.** A set is keyed by the requirement list, so `pandas` would name
//! one directory on Monday and different bytes on Friday - the key would be stable while
//! its content was not, which is the opposite of what the rest of this tool does with a
//! digest. Requiring `==` makes the key mean something. It does not pin *transitive*
//! dependencies, so [`warm`] also records what actually resolved and the gate keeps that
//! beside its verdict; a later change shows up as drift rather than as a mystery.
//!
//! **Wheels only, never a build.** A plain registry name can still resolve to an sdist,
//! and building one runs `setup.py` - arbitrary code, on the host, from a generated
//! file. `--only-binary :all:` applies to transitive dependencies too, so nothing in
//! the tree gets to execute during a warm. An exercise wanting a package with no wheel
//! is an exercise that does not get warmed, which is the correct outcome.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use crate::digest::{hex, Sha256};
use crate::exercise::Deps;
use crate::store::cache_dir;

/// Where a warmed set appears inside a job. A dotted name, beside `.home`, so it does
/// not collide with the exercise's own `work/`, `check/` or `out/`.
pub const GUEST_DEPS: &str = "/box/.deps";

/// The interpreter every job gets: the host's, because `/usr` is what the sandbox
/// mounts read-only. Warming against anything else builds wheels for an ABI the job
/// cannot load.
pub const INTERPRETER: &str = "/usr/bin/python3";

/// The ABI tag of [`INTERPRETER`], read once per process.
///
/// `SOABI` rather than the version string: it carries the implementation, the minor
/// version and the platform, which together are exactly what decides whether a compiled
/// wheel loads. A pure-Python set would survive a 3.14 → 3.15 move and this key will
/// invalidate it anyway - correct, and cheap next to a wrong-ABI import.
static ABI: LazyLock<Result<String, String>> = LazyLock::new(|| {
    let out = Command::new(INTERPRETER)
        .args(["-c", "import sysconfig;print(sysconfig.get_config_var('SOABI') or 'none')"])
        .output()
        .map_err(|e| format!("{INTERPRETER}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{INTERPRETER}: could not report its ABI tag"));
    }
    let tag = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if tag.is_empty() {
        return Err(format!("{INTERPRETER}: empty ABI tag"));
    }
    Ok(tag)
});

fn abi() -> Result<&'static str, String> {
    ABI.as_ref().map(String::as_str).map_err(Clone::clone)
}

// ---------------------------------------------------------------------------
// What may be asked for
// ---------------------------------------------------------------------------

const COMPARATORS: [&str; 8] = ["===", "==", "!=", "~=", "<=", ">=", "<", ">"];

/// Admit a requirement, or say exactly why not.
///
/// A small allowlist rather than a blocklist of the dangerous forms, because the
/// dangerous forms are the ones nobody thought of. What is allowed is a PEP 503 name,
/// optional `[extras]`, and optional comma-separated version specifiers. No URL, no
/// path, no VCS, no editable, no environment marker, no flag.
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
    // Exactly one, and it must be `==` or `===`. One exact comparator among several is
    // not a pin: `pkg==1.0,!=1.0.1` still admits whatever the index decides `1.0` means
    // today, and a wildcard `==1.0.*` is a range wearing an equals sign.
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

/// Split at the first comparator outside the extras bracket. Done by hand because the
/// comparator characters are also legal nowhere else in a name, so finding the first of
/// them is the whole parse.
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

/// Validate a whole declaration, naming every problem rather than only the first.
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
/// Sorted first, so the digest names the *set* and not the order somebody typed it in;
/// two exercises asking for the same packages share one warmed directory. Each entry is
/// length-prefixed for the same reason the exercise digest does it: without that,
/// `["ab", "c"]` and `["a", "bc"]` hash alike.
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
pub fn set_path(python: &[String]) -> Result<PathBuf, String> {
    Ok(cache_dir()?.join("sets").join(abi()?).join(digest(python)))
}

/// The warmed set a declaration needs, or `None` when it declares no packages.
///
/// A refusal rather than a silent miss: an exercise whose grader imports a package that
/// is not there fails as `CheckBroken`, which reads as a broken grader and sends the
/// author to debug a script that is fine. Naming the missing set and the command that
/// fills it is the whole difference between a five-second fix and an afternoon.
pub fn require(deps: &Deps) -> Result<Option<PathBuf>, String> {
    if deps.python.is_empty() {
        return Ok(None);
    }
    check(deps)?;
    let path = set_path(&deps.python)?;
    if !path.is_dir() {
        return Err(format!(
            "dependencies are declared but not warmed for {} - run `benkyou warm <exercise-dir>`\n  \
             wanted: {}\n  expected at: {}",
            abi()?,
            deps.python.join(", "),
            path.display()
        ));
    }
    // A set that exists must be a set whose contents are known, or the record the gate
    // writes would name a tree nobody can identify.
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
/// Read from the `.dist-info` directories rather than from `uv`'s output, because this
/// has to answer for what is *on disk* - including transitive dependencies the
/// declaration never named, which is the half an exact pin cannot cover.
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
/// Fallible on purpose. The manifest is written before the staging directory is renamed
/// into place, so any set *this* code created has one - but a directory left by an
/// older version, or by a half-finished copy, does not. Returning an empty list there
/// would be the worst outcome available: the gate would record `deps: []`, the drift
/// check would find nothing to compare, and a set of unknown contents would carry a
/// verdict that claimed to name what it ran against.
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
    pub abi: String,
    /// Everything on disk in the set, transitive dependencies included.
    pub resolved: Vec<String>,
    /// False when the set was already present and nothing was fetched.
    pub fetched: bool,
}

/// Install a declaration's packages into the cache.
///
/// The only command in this tool that uses a network, and it runs on the host without a
/// sandbox, deliberately: `uv` needs DNS and an index, and wrapping that in isolation
/// would be theatre. What it must never do is execute anything from the exercise. The
/// package list comes from `task.toml`, which is parsed and then validated by
/// [`check`] - discovering dependencies by importing a generated script would put that
/// script on the network, which is the one thing all of this exists to prevent, and
/// building an sdist would run its `setup.py` here.
pub fn warm(deps: &Deps, force: bool) -> Result<Option<Warmed>, String> {
    if deps.python.is_empty() {
        return Ok(None);
    }
    check(deps)?;
    let abi = abi()?.to_string();
    let path = set_path(&deps.python)?;
    // A present set with a readable manifest is done. One without is treated as absent:
    // `warm` is the command that repairs this, so refusing here would leave no way out.
    if path.is_dir() && !force {
        if let Ok(resolved) = resolved(&path) {
            return Ok(Some(Warmed {
                python: deps.python.clone(),
                path,
                abi,
                resolved,
                fetched: false,
            }));
        }
    }

    let uv = which_uv()?;
    // Built beside the destination and renamed, so an interrupted warm cannot leave a
    // half-installed set that later runs would bind and trust. Same reason the gate
    // writes `.gate.json` through a temp name.
    let parent = path.parent().ok_or_else(|| format!("{}: no parent", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let staging = parent.join(format!(".tmp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);

    let mut cmd = Command::new(&uv);
    cmd.args(["pip", "install", "--only-binary", ":all:", "--python", INTERPRETER, "--target"])
        .arg(&staging)
        .args(&deps.python);
    let out = cmd.output().map_err(|e| format!("{}: {e}", uv.display()))?;
    if !out.status.success() {
        let _ = fs::remove_dir_all(&staging);
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(6).collect();
        return Err(format!(
            "warming failed: {}\n{}",
            deps.python.join(", "),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        ));
    }

    // Written before the rename, so a set that exists is a set whose manifest exists.
    let resolved = installed(&staging)?;
    let manifest = serde_json::to_string_pretty(&resolved).map_err(|e| e.to_string())?;
    fs::write(staging.join(RESOLVED), manifest)
        .map_err(|e| format!("{}: {e}", staging.display()))?;

    // Unconditionally, not only under `--force`: this path is also reached when a set
    // exists but its manifest does not, and `rename` will not replace a directory that
    // has anything in it. Removing first is what makes `warm` the repair command.
    let _ = fs::remove_dir_all(&path);
    fs::rename(&staging, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(Warmed { python: deps.python.clone(), path, abi, resolved, fetched: true }))
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

    /// A cache key is the requirement list. An unpinned requirement makes that key name
    /// different bytes on different days, which is the one thing a digest must not do.
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

    /// Each of these executes code on the host during a warm, or redirects where the
    /// packages come from. They are the reason this function exists.
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

    /// A marker is not dangerous by itself, but admitting it means admitting the
    /// characters that carry the dangerous forms, so it is out of the subset.
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

    /// The validator guards the network command, so it has to run before it, not after.
    #[test]
    fn warming_refuses_a_bad_spec_without_touching_the_network() {
        let err = warm(&deps(&["git+https://example.invalid/x"]), false).unwrap_err();
        assert!(err.contains("git+https://example.invalid/x"), "must name the spec: {err}");
        assert!(err.contains("Registry names only"), "must say what is allowed: {err}");
    }

    #[test]
    fn requiring_refuses_a_bad_spec_too() {
        let err = require(&deps(&["-e ."])).unwrap_err();
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

    /// Without length prefixes these two collide, and two different exercises would
    /// share one warmed directory.
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
        assert!(require(&none).expect("no deps is not an error").is_none());
        assert!(warm(&none, false).expect("nothing to warm").is_none());
    }

    // -- the manifest is the set's identity --------------------------------

    /// A directory with no manifest is a set of unknown contents. Trusting it would let
    /// the gate record `deps: []` and claim to name a tree nobody can identify.
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

    /// `installed` answers for what is on disk, transitive packages included, because an
    /// exact pin fixes only the names the author wrote down.
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
}
