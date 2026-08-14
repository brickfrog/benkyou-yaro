//! Exercises: the task schema, the grading contract, and the validation gate.
//!
//! The gate is what separates a generated exercise from an LLM emitting prose with a
//! test file next to it. See DESIGN.md §3.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// What sort of thing is being practised. Determines which grader runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Hidden tests over a function under test.
    Kata,
    /// A pinned repro flips red to green while a guard set stays green.
    Debug,
    /// Learner query and reference query compared on one fixture.
    Sql,
    /// Assert observable system state.
    Terminal,
    /// Compare a produced file against a reference.
    Artifact,
}

/// How much of the solution is shown. Worked examples help novices and actively harm
/// knowledgeable learners, so this is chosen from the assessment, not by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Guidance {
    /// Full solution shown, learner follows along.
    Worked,
    /// Backward-faded: the last step or two are blank.
    Faded,
    /// No solution shown.
    Blank,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskMeta {
    pub id: String,
    /// Back-link into the prerequisite graph.
    pub concept_id: String,
    pub kind: Kind,
    pub guidance_level: Guidance,
    #[serde(default)]
    pub generated_by: Option<String>,
    #[serde(default)]
    pub generated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    pub setup_secs: u32,
    pub learner_secs: u32,
    pub check_secs: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self { setup_secs: 60, learner_secs: 900, check_secs: 120 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verify {
    /// The grader command. Its exit code reports grader health, not the grade.
    pub cmd: String,
    /// Path the grader writes, relative to the run's `out/` directory.
    #[serde(default = "default_reward_path")]
    pub reward: String,
    /// Dimensions that gate pass/fail. Every one must be 1.0.
    pub must_pass: Vec<String>,
    /// Judge dimensions. Reported, never gating.
    #[serde(default)]
    pub advisory: Vec<String>,
    /// When true the check directory holds cases the learner must not read. The gate
    /// keeps `check/` out of the learner's workspace, so this records intent.
    #[serde(default)]
    pub hidden: bool,
}

fn default_reward_path() -> String {
    "reward.json".to_string()
}

/// What the machine looked like when the gate ran.
///
/// Recorded as evidence, never as a gating condition. A gate result earned by a
/// different binary, or on a different platform, is weaker evidence than one earned
/// here - but refusing on it would ungate an entire library on a version bump, which
/// buys nothing and trains the reader to re-gate without looking.
///
/// This is a *fingerprint*, not a description of the environment. What actually
/// decides whether a grader still behaves - interpreter build, installed packages,
/// their versions - is not enumerable from the exercise directory, and pretending
/// otherwise here would be worse than the honest gap. Drift in those shows up as a
/// failing grade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Env {
    pub benkyou: String,
    pub os: String,
    pub arch: String,
}

impl Env {
    pub fn current() -> Self {
        Env {
            benkyou: env!("CARGO_PKG_VERSION").to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        }
    }

    /// One line per difference, for a caller that prints warnings.
    pub fn drift(&self, now: &Env) -> Vec<String> {
        let mut out = Vec::new();
        if self.benkyou != now.benkyou {
            out.push(format!(
                "gated by benkyou {}, running {}",
                self.benkyou, now.benkyou
            ));
        }
        if self.os != now.os || self.arch != now.arch {
            out.push(format!(
                "gated on {}/{}, running {}/{}",
                self.os, self.arch, now.os, now.arch
            ));
        }
        out
    }
}

/// How a verdict was executed.
///
/// A gate result claims a grader discriminates. That claim holds only under the
/// conditions it was earned: an exercise that passes with the host filesystem in reach
/// may fail sandboxed, and a grader that quietly read something outside its view would
/// have been proved discriminating by evidence the learner's run will not have. So
/// unlike [`Env`], a difference here is a refusal and not a warning — the verdict is
/// about a run that can no longer happen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Runner {
    /// [`crate::run::RUNNER_SEMANTICS`] at the time of gating.
    pub semantics: u32,
    /// `sandbox`, `container` or `unsafe-host`.
    pub backend: String,
    /// Execution profile: what the backend was, concretely.
    pub profile: String,
    /// The exact runtime, for a backend that has one to name.
    ///
    /// `None` under the sandbox and the host backend, where the runtime is the
    /// machine's `/usr` and is not enumerable — see [`Env`]. `Some` under a container,
    /// where it is the image id the engine resolved, and where being able to name it is
    /// the whole reason that backend can claim more than the others.
    ///
    /// Absent from an old record and from every record the other two backends write, so
    /// it defaults and is omitted rather than serialising a null into every sidecar in
    /// an existing library.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

impl Runner {
    pub fn of(backend: &crate::run::Backend) -> Self {
        Self {
            semantics: crate::run::RUNNER_SEMANTICS,
            backend: backend.name().to_string(),
            profile: backend.profile(),
            image: backend.image_id().map(str::to_string),
        }
    }

    pub fn describe(&self) -> String {
        format!("{} ({})", self.backend, self.profile)
    }

    /// Why this record no longer describes the run that is about to happen.
    ///
    /// Three fields refuse and one warns, split by whether the difference changes what a
    /// run *can do*.
    ///
    /// `semantics` refuses: this binary changed what a job may see or how it is
    /// killed, so every verdict on disk describes a run that no longer exists.
    ///
    /// `backend` refuses too, and this is the load-bearing one. A grader that reaches
    /// the network, or reads a file outside its view, discriminates on the host and
    /// fails in the sandbox — so a host-earned verdict is not evidence about a
    /// sandboxed attempt, and a sandbox-earned verdict was proved without capabilities
    /// the host run will have. Accepting either would put the learner's grade on a
    /// claim nobody tested. The refusal is also the honest direction: it pushes an
    /// author who gated with `--unsafe-host` to re-gate sandboxed rather than leaving
    /// a library half-proved.
    ///
    /// `image` refuses, and for a reason the other two backends never had: under a
    /// container the image *is* the `/usr`. A different image is a different
    /// interpreter, a different libc and a different set of installed tools, which is
    /// exactly the drift the environment fingerprint has always had to report as a
    /// warning because it could not be pinned. Here it can be, so it is not a warning.
    /// One manifest-list reference resolves to a different id on every architecture, so
    /// this also catches the arm64 verdict being read on an amd64 machine.
    ///
    /// `profile` only warns. A bubblewrap point release, or a newer engine driving the
    /// same image, is evidence about the run rather than a change in what the run could
    /// do, and invalidating a library on it would make re-gating routine — which is how
    /// a refusal stops being read.
    pub fn stale(&self, now: &Runner) -> Option<String> {
        if self.semantics != now.semantics {
            return Some(format!(
                "runner semantics {} on record, {} now",
                self.semantics, now.semantics
            ));
        }
        if self.backend != now.backend {
            return Some(format!(
                "gated under the {} backend, running the {} backend",
                self.backend, now.backend
            ));
        }
        if self.image != now.image {
            return Some(match (&self.image, &now.image) {
                (Some(was), Some(now)) => {
                    format!("gated against runner image {was}, running {now}")
                }
                (Some(was), None) => format!("gated against runner image {was}, running without one"),
                (None, Some(now)) => format!("gated without a runner image, running {now}"),
                (None, None) => unreachable!("equal images are not a difference"),
            });
        }
        None
    }

    /// Differences worth printing but not worth refusing over.
    pub fn drift(&self, now: &Runner) -> Vec<String> {
        if self.backend == now.backend && self.profile != now.profile {
            vec![format!(
                "gated with {}, running {}",
                self.profile, now.profile
            )]
        } else {
            Vec::new()
        }
    }
}

/// The gate's verdict, stored beside the exercise rather than inside it.
///
/// This lives in `.gate.json`, not in `task.toml`, and the separation is what makes
/// the digest trustworthy. `task.toml` is authored input: the tool never rewrites it,
/// so it can be hashed exactly as the author left it - comments, formatting, and
/// sections this binary does not parse included. Writing the verdict back into the
/// file it was a verdict *about* would mean hashing a canonical form instead, and
/// arguing about which differences count.
///
/// It also makes gating non-destructive. Before this, gating a shared fixture in place
/// edited it; now the authored files are untouched and the derived record can be
/// deleted or ignored without loss.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    pub solution_passes: bool,
    pub empty_fails: bool,
    pub validated_at: String,
    /// What the gate looked at. Without this the verdict outlives its subject: a
    /// hidden case edited after gating would leave an exercise showable on the
    /// strength of a run that no longer describes it.
    pub digest: String,
    /// How it was executed. Without this the verdict outlives its *conditions*.
    pub runner: Runner,
    pub env: Env,
    /// What the warmed dependency set resolved to when the gate ran, transitive
    /// dependencies included.
    ///
    /// An exact pin in `task.toml` fixes the packages the author named and nothing
    /// below them, so this is the only place the whole tree is written down. Compared
    /// like `Env`: a change warns rather than refuses, because it describes the
    /// environment a verdict was earned in and not the exercise itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<String>,
    /// Ids of the named wrong answers this grader actually rejected.
    ///
    /// A list rather than a count so a reader can see *which* traps sprang, and so
    /// removing one from `task.toml` changes the digest and forces a re-gate.
    #[serde(default)]
    pub known_bad_caught: Vec<String>,
}

impl Gate {
    /// True when every direction held. A record that exists and says otherwise is a
    /// recorded *failure*, which is not the same as no record at all.
    pub fn holds(&self) -> bool {
        self.solution_passes && self.empty_fails && !self.known_bad_caught.is_empty()
    }

    /// True for a record written before wrong answers were required.
    ///
    /// Distinguished from a rejection because it is not one: the exercise may be
    /// perfectly good and simply has not been asked the newer question. The reader
    /// needs "gate it again", not "this is not an exercise".
    pub fn predates_known_bad(&self) -> bool {
        self.solution_passes && self.empty_fails && self.known_bad_caught.is_empty()
    }
}

/// Where the gate's verdict is kept, relative to the exercise directory.
pub const GATE_FILE: &str = ".gate.json";

/// Optional per-exercise workspace settings.
///
/// Nothing on the CLI path needs these. They exist for a front-end that offers a Run
/// button and therefore has to know how the learner's file is meant to be executed;
/// an exercise without the section simply has no such button.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    /// How to run whatever the learner currently has, with the workspace as the
    /// working directory. This is not the grader: it produces output to read, never a
    /// verdict, and the verdict still comes from `verify.cmd` alone.
    #[serde(default)]
    pub run_cmd: Option<String>,
}

impl Workspace {
    /// True when the section carries nothing. `gate` rewrites `task.toml` after
    /// validating, and without this every gated exercise would grow an empty
    /// `[workspace]` it never asked for.
    pub fn is_empty(&self) -> bool {
        self.run_cmd.is_none()
    }
}

/// A wrong answer the author names in advance, and the mistake it embodies.
///
/// The gate's two directions prove an exercise is solvable and that its checks are not
/// vacuous. Neither says the grader discriminates on the *concept*. A grader that
/// accepts anything except an empty file passes both and teaches nothing, and the
/// author cannot catch it by reading their own work: the same model that misread the
/// concept writes the reference, the checks and the prose, so they agree with each
/// other and are wrong together.
///
/// A known-bad candidate breaks that agreement by making the author commit to a
/// prediction the machine can test: *this specific answer must fail, for this reason*.
/// If it passes, the grader does not measure what the exercise claims to teach, and
/// that is an arithmetic contradiction rather than a matter of opinion.
///
/// These are mutation tests for the grader. They do **not** show the exercise teaches
/// the intended concept — a model wrong about the concept can be consistently wrong
/// across the reference, the checks *and* its own candidates. The narrower claim is
/// the one that is true.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnownBad {
    pub id: String,
    /// The misconception in one line, printed when the candidate wrongly passes. The
    /// reader needs to know which trap failed to spring, not that one did.
    pub trap: String,
    /// Files written into a fresh workspace, relative path to content.
    ///
    /// Static content and not a command, deliberately. An executable `apply` step
    /// would be one more generated script to run, which is the thing the execution
    /// boundary exists to bound; and a candidate that can compute is a candidate that
    /// can read the answer. Paths are resolved with `gate::safe_join`, so a generated
    /// `task.toml` cannot reach `../../check/check.sh` and grade itself.
    pub files: BTreeMap<String, String>,
}

/// Packages an exercise needs that the machine may not have.
///
/// Empty for most exercises: the sandbox exposes the host's `/usr`, so anything
/// installed system-wide is already importable and needs no declaration. This is for
/// the case that isolation otherwise makes impossible - a kata about pandas on a
/// machine without pandas.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deps {
    /// Requirement strings, in the small subset `crate::deps::check_spec` admits: a
    /// name, optional extras, and exactly one exact `==` version. A bare name or a
    /// range is refused, because a set is cached under a digest of this list and an
    /// unpinned entry makes that key name different bytes over time. No URLs, paths or
    /// flags either - warming runs on the host with a network, and those forms execute
    /// code there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub python: Vec<String>,
}

impl Deps {
    pub fn is_empty(&self) -> bool {
        self.python.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub schema_version: String,
    pub task: TaskMeta,
    #[serde(default)]
    pub limits: Limits,
    pub verify: Verify,
    /// Additive on top of schema 1: an exercise written before this existed parses
    /// and behaves exactly as it did, which is why `schema_version` does not move.
    /// Kept last because TOML tables must follow every plain value in the struct.
    #[serde(default, skip_serializing_if = "Workspace::is_empty")]
    pub workspace: Workspace,
    /// Third-party packages the exercise's own scripts need.
    ///
    /// Declared rather than discovered from a PEP 723 header, because `benkyou warm`
    /// has to learn the list without executing anything: importing a generated script
    /// to find its imports would put that script on a network. See [`crate::deps`].
    #[serde(default, skip_serializing_if = "Deps::is_empty")]
    pub deps: Deps,
    /// Wrong answers that must fail. At least one is required; see [`KnownBad`].
    ///
    /// After `workspace` because an array of tables has to follow every other table
    /// in TOML, not only every plain value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_bad: Vec<KnownBad>,
}

/// Read the gate's verdict for an exercise, if one has been recorded.
///
/// A missing file is `None`, not an error: not having been gated is an ordinary state.
/// A file that will not parse *is* an error - something wrote it, and silently
/// treating corruption as "ungated" would hide the corruption.
///
/// A banked bundle has no sidecar, deliberately: the bundle is the authored bytes and
/// nothing else, so a verdict earned on one machine cannot travel inside it as though
/// it were a property of the exercise. Its verdicts live in `attestations.jsonl`
/// instead, one line per gate run, and the newest is returned here. Every check in
/// [`require_current`] then applies unchanged - including `Runner::stale`, which is
/// the one that notices the bundle was validated somewhere this machine is not.
pub fn read_gate(dir: &Path) -> Result<Option<Gate>, String> {
    let path = dir.join(GATE_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(crate::bank::newest_attestation(dir))
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Record the gate's verdict beside the exercise, leaving authored files untouched.
///
/// Written to a temporary name and renamed into place. A verdict is read by every
/// door and by the next gate run; a half-written one is a parse error that reads as
/// corruption, and losing power between `open` and `write` would turn a validated
/// exercise into a broken one. `rename` within a directory is atomic, so a reader sees
/// the old record or the new one.
pub fn write_gate(dir: &Path, gate: &Gate) -> Result<(), String> {
    let path = dir.join(GATE_FILE);
    let tmp = dir.join(format!("{GATE_FILE}.{}.tmp", std::process::id()));
    let text = serde_json::to_string_pretty(gate).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text + "\n").map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// Refuse an exercise whose gate verdict is missing, negative, earned under different
/// execution conditions, or no longer describing the files on disk.
///
/// Four ways to fail, kept distinct because they need different things from the
/// reader. No record: run the gate. A record that says the exercise was *rejected*:
/// this is not an exercise, and re-running the gate will say so again. A record earned
/// under a runner that no longer exists: run it again, under this one. A record earned
/// by different bytes: run it again, because the stamp is about a file that no longer
/// exists.
pub fn require_current(dir: &Path, backend: &crate::run::Backend) -> Result<(), String> {
    let gate = read_gate(dir)?.ok_or_else(|| {
        format!(
            "{}: not validated - run `benkyou gate` on it first",
            dir.display()
        )
    })?;
    if gate.predates_known_bad() {
        return Err(format!(
            "{}: gated before wrong answers were required - run `benkyou gate` again",
            dir.display()
        ));
    }
    if !gate.holds() {
        return Err(format!(
            "{}: the gate rejected this exercise (solution_passes={}, empty_fails={})",
            dir.display(),
            gate.solution_passes,
            gate.empty_fails
        ));
    }
    if let Some(why) = gate.runner.stale(&Runner::of(backend)) {
        return Err(format!(
            "{}: {why} - run `benkyou gate` again",
            dir.display()
        ));
    }
    let actual = crate::digest::exercise_digest(dir)?;
    if actual != gate.digest {
        return Err(format!(
            "{}: changed since it was gated ({} on record, {} now) - run `benkyou gate` again",
            dir.display(),
            &gate.digest[..gate.digest.len().min(12)],
            &actual[..actual.len().min(12)],
        ));
    }
    Ok(())
}

/// Advisory notes about a validated exercise. Never a reason to refuse it.
///
/// Separate from [`require_current`] on purpose, mirroring the gate's own split
/// between an outcome and its warnings: the caller decides whether it has anywhere to
/// print these, and nothing changes if it does not.
pub fn gate_warnings(dir: &Path, backend: &crate::run::Backend) -> Vec<String> {
    match read_gate(dir) {
        Ok(Some(gate)) => {
            let mut out = gate.env.drift(&Env::current());
            out.extend(gate.runner.drift(&Runner::of(backend)));
            out.extend(deps_drift(dir, &gate, backend));
            out
        }
        _ => Vec::new(),
    }
}

/// Whether the dependency tree under the exercise's pins has moved since it was gated.
///
/// A warning and not a refusal, for the same reason `Env` warns: it describes the
/// environment a verdict was earned in, not the exercise. Every authored byte is still
/// covered by the digest and unchanged - what moved is a wheel below the version the
/// author pinned. Silence would be wrong though: a grader that passed against one numpy
/// and fails against the next looks like a broken exercise, and this is the one line
/// that says otherwise.
fn deps_drift(dir: &Path, gate: &Gate, backend: &crate::run::Backend) -> Vec<String> {
    let Ok(task) = load(dir) else { return Vec::new() };
    // A set that cannot be identified is reported by `require` on the run path, where it
    // is an error. Here it would be noise on top of that.
    let Ok(Some(set)) = crate::deps::require(&task.deps, crate::deps::Runtime::of(backend))
    else {
        return Vec::new();
    };
    let Ok(now) = crate::deps::resolved(&set) else { return Vec::new() };
    if gate.deps == now {
        return Vec::new();
    }
    let gone: Vec<&str> =
        gate.deps.iter().filter(|d| !now.contains(d)).map(String::as_str).collect();
    let added: Vec<&str> =
        now.iter().filter(|d| !gate.deps.contains(d)).map(String::as_str).collect();
    vec![format!(
        "dependency set changed since gating: was [{}], now [{}]",
        gone.join(", "),
        added.join(", ")
    )]
}

/// What the grader wrote. Dimension name to score in `0.0..=1.0`, plus learner-facing
/// detail.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reward {
    #[serde(flatten)]
    pub dimensions: BTreeMap<String, f32>,
    #[serde(default)]
    pub detail: Option<String>,
}

impl Reward {
    /// Parse a grader's reward file. `detail` is lifted out of the dimension map so a
    /// grader can write both in one flat object.
    pub fn parse(s: &str) -> Result<Reward, String> {
        let raw: serde_json::Value =
            serde_json::from_str(s).map_err(|e| format!("reward is not valid JSON: {e}"))?;
        let obj = raw
            .as_object()
            .ok_or_else(|| "reward must be a JSON object".to_string())?;

        let mut dimensions = BTreeMap::new();
        let mut detail = None;
        for (k, v) in obj {
            if k == "detail" {
                detail = v.as_str().map(str::to_string);
                continue;
            }
            match v.as_f64() {
                Some(n) if n.is_finite() => {
                    dimensions.insert(k.clone(), n as f32);
                }
                Some(_) => return Err(format!("dimension `{k}` is not finite")),
                None => return Err(format!("dimension `{k}` is not a number")),
            }
        }
        Ok(Reward { dimensions, detail })
    }

    pub fn score(&self, dimension: &str) -> Option<f32> {
        self.dimensions.get(dimension).copied()
    }
}

/// The outcome of grading one attempt.
///
/// `CheckBroken` is deliberately distinct from `Fail`: conflating grader health with
/// the grade is the classic source of unreadable grading failures, and a learner who
/// cannot tell "you got it wrong" from "the grader crashed" stops trusting the tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Verdict {
    Pass,
    /// Gating dimensions that were not 1.0, with their scores.
    Fail(BTreeMap<String, f32>),
    /// The grader itself failed: non-zero exit, missing or unparseable reward file,
    /// or a gating dimension it never reported.
    CheckBroken(String),
    Timeout(u32),
}

impl Verdict {
    pub fn is_pass(&self) -> bool {
        matches!(self, Verdict::Pass)
    }
}

/// Decide a verdict from what the grader produced.
///
/// `exit_code` is the grader's own exit status, which reports whether it *ran*, not
/// whether the learner passed. Every dimension in `must_pass` has to be present and
/// exactly 1.0; a gating dimension the grader never wrote is a broken grader, not a
/// failure, because silently failing an attempt the grader never judged is worse than
/// admitting the grader is wrong.
pub fn grade(
    verify: &Verify,
    exit_code: Option<i32>,
    timed_out_after: Option<u32>,
    reward_text: Option<&str>,
) -> Verdict {
    if let Some(secs) = timed_out_after {
        return Verdict::Timeout(secs);
    }
    match exit_code {
        Some(0) => {}
        Some(code) => return Verdict::CheckBroken(format!("grader exited {code}")),
        None => return Verdict::CheckBroken("grader did not exit normally".into()),
    }

    let Some(text) = reward_text else {
        return Verdict::CheckBroken(format!("grader wrote no {}", verify.reward));
    };
    let reward = match Reward::parse(text) {
        Ok(r) => r,
        Err(e) => return Verdict::CheckBroken(e),
    };

    let mut failed = BTreeMap::new();
    for dim in &verify.must_pass {
        match reward.score(dim) {
            Some(score) if score >= 1.0 => {}
            Some(score) => {
                failed.insert(dim.clone(), score);
            }
            None => {
                return Verdict::CheckBroken(format!("grader never reported gating `{dim}`"))
            }
        }
    }

    if failed.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fail(failed)
    }
}

/// Why an exercise was rejected by the gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GateFailure {
    /// The reference solution did not pass. The exercise is unsolvable as written.
    SolutionFailed(Verdict),
    /// The untouched starting state passed. The checks assert nothing.
    ChecksVacuous(Verdict),
    /// No wrong answer was named, so nothing tested whether the grader discriminates.
    NoKnownBad,
    /// A named wrong answer passed. The grader does not catch the mistake the author
    /// said it would, which makes the exercise decoration.
    KnownBadPassed { id: String, trap: String },
    /// The grader broke on a named wrong answer rather than failing it. It cannot
    /// judge that input at all, so its verdict on any other input is not evidence.
    KnownBadBrokeTheGrader { id: String, verdict: Verdict },
    /// The exercise changed while the gate was running, so no run describes what is
    /// now on disk and there is nothing to certify.
    ContentChangedDuringGate { before: String, after: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GateOutcome {
    Validated(Gate),
    Rejected(GateFailure),
}

/// Decide the gate from its runs.
///
/// Run 1 applies `solution/solve.sh` and must pass. Run 2 leaves `setup/` untouched
/// and must fail. Both are required: run 1 alone admits a vacuous check that passes on
/// an empty workspace, and run 2 alone admits an unsolvable exercise.
///
/// Runs 3..n apply one named wrong answer each and must fail *without breaking the
/// grader*. The distinction matters: a candidate that makes `check.sh` crash has not
/// been judged, and counting a crash as a catch is how a grader that cannot parse
/// anything scores full marks on the whole suite.
pub fn gate_outcome(
    solution: &Verdict,
    empty: &Verdict,
    known_bad: &[(&KnownBad, Verdict)],
    at: &str,
    digest: &str,
    backend: &crate::run::Backend,
    deps: &[String],
) -> GateOutcome {
    if !solution.is_pass() {
        return GateOutcome::Rejected(GateFailure::SolutionFailed(solution.clone()));
    }
    // A broken grader on the empty run is not evidence the checks discriminate.
    if empty.is_pass() || matches!(empty, Verdict::CheckBroken(_)) {
        return GateOutcome::Rejected(GateFailure::ChecksVacuous(empty.clone()));
    }
    if known_bad.is_empty() {
        return GateOutcome::Rejected(GateFailure::NoKnownBad);
    }
    for (candidate, verdict) in known_bad {
        if let Verdict::CheckBroken(_) = verdict {
            return GateOutcome::Rejected(GateFailure::KnownBadBrokeTheGrader {
                id: candidate.id.clone(),
                verdict: verdict.clone(),
            });
        }
        if verdict.is_pass() {
            return GateOutcome::Rejected(GateFailure::KnownBadPassed {
                id: candidate.id.clone(),
                trap: candidate.trap.clone(),
            });
        }
    }
    GateOutcome::Validated(Gate {
        solution_passes: true,
        empty_fails: true,
        known_bad_caught: known_bad.iter().map(|(c, _)| c.id.clone()).collect(),
        validated_at: at.to_string(),
        digest: digest.to_string(),
        runner: Runner::of(backend),
        env: Env::current(),
        deps: deps.to_vec(),
    })
}

/// Load a `task.toml` from an exercise directory.
pub fn load(dir: &Path) -> Result<Task, String> {
    let path = dir.join("task.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;

    fn verify(must_pass: &[&str]) -> Verify {
        Verify {
            cmd: "bash check.sh".into(),
            reward: "reward.json".into(),
            must_pass: must_pass.iter().map(|s| s.to_string()).collect(),
            advisory: vec!["approach".into()],
            hidden: true,
        }
    }

    fn trap(id: &str) -> KnownBad {
        KnownBad {
            id: id.into(),
            trap: "a named misconception".into(),
            files: BTreeMap::from([("solution.py".to_string(), "wrong".to_string())]),
        }
    }

    /// One wrong answer, correctly rejected — the shape every passing gate has.
    fn caught() -> Vec<(&'static KnownBad, Verdict)> {
        static ONE: LazyLock<KnownBad> = LazyLock::new(|| KnownBad {
            id: "trap".into(),
            trap: "a named misconception".into(),
            files: BTreeMap::new(),
        });
        vec![(&*ONE, Verdict::Fail(BTreeMap::from([("correctness".into(), 0.0)])))]
    }

    /// The failure the whole feature exists for.
    ///
    /// Both original directions hold: the reference passes, the empty stub fails. A
    /// grader that misread the concept looks exactly like this, because the same model
    /// wrote the reference and the checks and they agree with each other. The named
    /// wrong answer is the only thing that disagrees.
    #[test]
    fn a_wrong_answer_that_passes_rejects_the_exercise() {
        let t = trap("sorted_not_first_seen");
        let outcome = gate_outcome(
            &Verdict::Pass,
            &Verdict::Fail(BTreeMap::from([("correctness".into(), 0.0)])),
            &[(&t, Verdict::Pass)],
            "t",
            "d",
            &crate::run::Backend::UnsafeHost,
            &[],
        );
        match outcome {
            GateOutcome::Rejected(GateFailure::KnownBadPassed { id, .. }) => {
                assert_eq!(id, "sorted_not_first_seen");
            }
            other => panic!("expected KnownBadPassed, got {other:?}"),
        }
    }

    /// A candidate that breaks the grader has not been judged by it.
    ///
    /// Counting a crash as a catch is how a grader that cannot parse anything scores
    /// full marks on a whole suite of traps.
    #[test]
    fn a_wrong_answer_that_breaks_the_grader_is_not_a_catch() {
        let t = trap("syntax_error");
        let outcome = gate_outcome(
            &Verdict::Pass,
            &Verdict::Fail(BTreeMap::from([("correctness".into(), 0.0)])),
            &[(&t, Verdict::CheckBroken("boom".into()))],
            "t",
            "d",
            &crate::run::Backend::UnsafeHost,
            &[],
        );
        assert!(matches!(
            outcome,
            GateOutcome::Rejected(GateFailure::KnownBadBrokeTheGrader { .. })
        ));
    }

    /// Naming none is not the same as naming one that passes, and the reader needs the
    /// difference: one says write a trap, the other says the grader is broken.
    #[test]
    fn naming_no_wrong_answer_rejects_the_exercise() {
        let outcome = gate_outcome(
            &Verdict::Pass,
            &Verdict::Fail(BTreeMap::from([("correctness".into(), 0.0)])),
            &[],
            "t",
            "d",
            &crate::run::Backend::UnsafeHost,
            &[],
        );
        assert!(matches!(
            outcome,
            GateOutcome::Rejected(GateFailure::NoKnownBad)
        ));
    }

    /// A record from before wrong answers were required is not a rejection, and saying
    /// "this is not an exercise" about a perfectly good one would send the reader to
    /// rewrite it instead of re-gating it.
    #[test]
    fn a_record_predating_known_bad_is_its_own_refusal() {
        let old = Gate {
            solution_passes: true,
            empty_fails: true,
            validated_at: "t".into(),
            digest: "d".into(),
            known_bad_caught: Vec::new(),
            runner: Runner::of(&crate::run::Backend::UnsafeHost),
            env: Env::current(),
            deps: vec![],
        };
        assert!(old.predates_known_bad());
        assert!(!old.holds());

        let rejected = Gate { solution_passes: false, ..old.clone() };
        assert!(!rejected.predates_known_bad(), "a rejection is not merely old");
    }

    #[test]
    fn reward_parses_dimensions_and_detail() {
        let r = Reward::parse(r#"{"correctness": 1.0, "approach": 0.5, "detail": "ok"}"#)
            .expect("parse");
        assert_eq!(r.score("correctness"), Some(1.0));
        assert_eq!(r.score("approach"), Some(0.5));
        assert_eq!(r.detail.as_deref(), Some("ok"));
        assert!(!r.dimensions.contains_key("detail"), "detail is not a dimension");
    }

    #[test]
    fn reward_rejects_non_numeric_and_non_finite() {
        assert!(Reward::parse(r#"{"correctness": "yes"}"#).is_err());
        assert!(Reward::parse("[1,2]").is_err());
        assert!(Reward::parse("not json").is_err());
    }

    #[test]
    fn passing_requires_every_gating_dimension() {
        let v = verify(&["correctness", "safety"]);
        let ok = r#"{"correctness": 1.0, "safety": 1.0, "approach": 0.2}"#;
        assert_eq!(grade(&v, Some(0), None, Some(ok)), Verdict::Pass);
    }

    #[test]
    fn advisory_dimensions_never_gate() {
        let v = verify(&["correctness"]);
        // approach is 0.0 and must not matter.
        let text = r#"{"correctness": 1.0, "approach": 0.0}"#;
        assert_eq!(grade(&v, Some(0), None, Some(text)), Verdict::Pass);
    }

    #[test]
    fn a_low_gating_dimension_fails_with_its_score() {
        let v = verify(&["correctness"]);
        let text = r#"{"correctness": 0.75}"#;
        match grade(&v, Some(0), None, Some(text)) {
            Verdict::Fail(dims) => assert_eq!(dims.get("correctness"), Some(&0.75)),
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    /// Grader health and the grade are different channels. A crashed grader must never
    /// be reported as a failed attempt.
    #[test]
    fn grader_health_is_not_the_grade() {
        let v = verify(&["correctness"]);
        assert!(matches!(
            grade(&v, Some(1), None, Some(r#"{"correctness": 1.0}"#)),
            Verdict::CheckBroken(_)
        ));
        assert!(matches!(grade(&v, Some(0), None, None), Verdict::CheckBroken(_)));
        assert!(matches!(
            grade(&v, Some(0), None, Some("garbage")),
            Verdict::CheckBroken(_)
        ));
        assert!(matches!(grade(&v, None, Some(120), None), Verdict::Timeout(120)));
    }

    /// A gating dimension the grader never wrote is a broken grader, not a failure.
    #[test]
    fn a_missing_gating_dimension_is_broken_not_failed() {
        let v = verify(&["correctness"]);
        let text = r#"{"approach": 1.0}"#;
        match grade(&v, Some(0), None, Some(text)) {
            Verdict::CheckBroken(msg) => assert!(msg.contains("correctness"), "{msg}"),
            other => panic!("expected CheckBroken, got {other:?}"),
        }
    }

    #[test]
    fn gate_requires_both_directions() {
        let fail = Verdict::Fail(BTreeMap::from([("correctness".into(), 0.0)]));

        // Both hold.
        assert!(matches!(
            gate_outcome(&Verdict::Pass, &fail, &caught(), "t", "d", &crate::run::Backend::UnsafeHost, &[]),
            GateOutcome::Validated(_)
        ));

        // Unsolvable as written.
        assert!(matches!(
            gate_outcome(&fail, &fail, &caught(), "t", "d", &crate::run::Backend::UnsafeHost, &[]),
            GateOutcome::Rejected(GateFailure::SolutionFailed(_))
        ));

        // Vacuous: the empty workspace already passes.
        assert!(matches!(
            gate_outcome(&Verdict::Pass, &Verdict::Pass, &caught(), "t", "d", &crate::run::Backend::UnsafeHost, &[]),
            GateOutcome::Rejected(GateFailure::ChecksVacuous(_))
        ));
    }

    /// A grader that breaks on the empty run proves nothing about discrimination,
    /// so it must not be mistaken for "the empty state failed".
    #[test]
    fn a_broken_grader_on_the_empty_run_does_not_validate() {
        let broken = Verdict::CheckBroken("boom".into());
        assert!(matches!(
            gate_outcome(&Verdict::Pass, &broken, &caught(), "t", "d", &crate::run::Backend::UnsafeHost, &[]),
            GateOutcome::Rejected(GateFailure::ChecksVacuous(_))
        ));
    }

    fn gate(solution_passes: bool, empty_fails: bool) -> Gate {
        Gate {
            solution_passes,
            empty_fails,
            validated_at: "t".into(),
            digest: "d".into(),
            known_bad_caught: vec!["trap".into()],
            runner: Runner::of(&crate::run::Backend::UnsafeHost),
            env: Env::current(),
            deps: vec![],
        }
    }

    #[test]
    fn one_direction_is_not_enough() {
        assert!(gate(true, true).holds());
        assert!(!gate(true, false).holds(), "the empty run never failed");
        assert!(!gate(false, true).holds(), "the solution never passed");
    }

    /// Four ways to be unshowable, each needing a different thing from the reader.
    /// One assertion per state, because a single "it refused" check passes for the
    /// wrong reason — everything refuses when nothing is gated.
    #[test]
    fn each_way_to_be_unshowable_is_reported_distinctly() {
        let host = crate::run::Backend::UnsafeHost;
        let dir = std::env::temp_dir().join(format!("benkyou-gaterec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("task.toml"), "schema_version = \"1\"\n").expect("write");

        let missing = require_current(&dir, &host).expect_err("no record must refuse");
        assert!(missing.contains("not validated"), "{missing}");

        write_gate(&dir, &gate(true, false)).expect("write gate");
        let rejected = require_current(&dir, &host).expect_err("a recorded failure must refuse");
        assert!(rejected.contains("rejected this exercise"), "{rejected}");

        // Gated under one runner, read under another. The verdict was earned with
        // capabilities this run will not have, or without ones it will.
        write_gate(&dir, &gate(true, true)).expect("write gate");
        let sandboxed = crate::run::Backend::Sandbox {
            bwrap: "/usr/bin/bwrap".into(),
            version: "0.0.0".into(),
        };
        let swapped = require_current(&dir, &sandboxed).expect_err("backend swap must refuse");
        assert!(swapped.contains("backend"), "{swapped}");

        // Same runner, wrong bytes: the digest is "d" and the directory hashes to
        // something else.
        let edited = require_current(&dir, &host).expect_err("a stale digest must refuse");
        assert!(edited.contains("changed since it was gated"), "{edited}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bubblewrap point release is evidence about the run, not a change in what the
    /// run could do. Refusing on it would make re-gating routine, which is how a
    /// refusal stops being read.
    #[test]
    fn a_profile_change_warns_and_a_backend_change_refuses() {
        let a = Runner::of(&crate::run::Backend::Sandbox {
            bwrap: "/usr/bin/bwrap".into(),
            version: "0.11.2".into(),
        });
        let b = Runner::of(&crate::run::Backend::Sandbox {
            bwrap: "/usr/bin/bwrap".into(),
            version: "0.11.3".into(),
        });
        assert!(a.stale(&b).is_none(), "a point release must not ungate a library");
        assert_eq!(a.drift(&b).len(), 1, "but it must still be reported");

        let host = Runner::of(&crate::run::Backend::UnsafeHost);
        assert!(a.stale(&host).is_some(), "isolation changed and the verdict did not");
        assert!(a.drift(&host).is_empty(), "a refusal is not also a warning");
    }

    /// The container backend's own refusal, and the one that has no analogue under
    /// bubblewrap: the image is the `/usr`, so a different image is a different
    /// interpreter, a different libc and a different set of tools. An engine upgrade
    /// under the *same* image is the other half — evidence, not a change in what a run
    /// could do — and it has to stay a warning or re-gating becomes routine.
    #[test]
    fn a_different_runner_image_refuses_and_a_newer_engine_warns() {
        let image = |id: &str| crate::run::Image {
            reference: "python:3.13-slim@sha256:aaaa".into(),
            id: id.into(),
            arch: "arm64".into(),
        };
        let container = |engine_version: &str, id: &str| {
            Runner::of(&crate::run::Backend::Container {
                cli: "/usr/bin/docker".into(),
                engine: "docker",
                version: engine_version.into(),
                image: image(id),
            })
        };

        let gated = container("29.7.2", "sha256:1111");
        let same = container("29.7.2", "sha256:1111");
        assert!(gated.stale(&same).is_none(), "the same runtime is not staleness");
        assert!(gated.drift(&same).is_empty());

        let newer_engine = container("30.0.0", "sha256:1111");
        assert!(
            gated.stale(&newer_engine).is_none(),
            "an engine upgrade must not ungate a library"
        );
        assert_eq!(gated.drift(&newer_engine).len(), 1, "but it must still be reported");

        let other_image = container("29.7.2", "sha256:2222");
        let why = gated
            .stale(&other_image)
            .expect("a different image is a different runtime");
        assert!(why.contains("sha256:1111") && why.contains("sha256:2222"), "{why}");

        // And across backends in both directions: a container verdict is not evidence
        // about a sandboxed run, and the sandbox has no image to compare at all.
        let sandbox = Runner::of(&crate::run::Backend::Sandbox {
            bwrap: "/usr/bin/bwrap".into(),
            version: "0.11.2".into(),
        });
        assert!(gated.stale(&sandbox).is_some());
        assert!(sandbox.stale(&gated).is_some());
    }

    /// A record written before the field existed must keep working. Every `.gate.json` in
    /// an existing library is one of these, and ungating a whole library on a schema
    /// addition is exactly what `Env` exists to avoid doing.
    #[test]
    fn an_old_record_without_an_image_still_reads() {
        let text = r#"{
            "solution_passes": true,
            "empty_fails": true,
            "validated_at": "2026-08-08T00:00:00Z",
            "digest": "abc",
            "runner": {"semantics": 1, "backend": "sandbox", "profile": "bwrap 0.11.2"},
            "env": {"benkyou": "0.3.0", "os": "linux", "arch": "x86_64"},
            "known_bad_caught": ["wrong"]
        }"#;
        let gate: Gate = serde_json::from_str(text).expect("an old record must parse");
        assert_eq!(gate.runner.image, None);
        let again = serde_json::to_string(&gate.runner).expect("serialise");
        assert!(!again.contains("image"), "an absent image must not be written back: {again}");
    }

    #[test]
    fn task_toml_round_trips() {
        let text = r#"
schema_version = "1"

[task]
id = "pandas-groupby-01"
concept_id = "pandas_groupby"
kind = "kata"
guidance_level = "blank"

[verify]
cmd = "bash check/check.sh"
must_pass = ["correctness"]
advisory = ["approach"]
hidden = true
"#;
        let task: Task = toml::from_str(text).expect("parse task.toml");
        assert_eq!(task.task.kind, Kind::Kata);
        assert_eq!(task.task.guidance_level, Guidance::Blank);
        assert_eq!(task.verify.reward, "reward.json", "default applies");
        assert_eq!(task.limits.learner_secs, 900, "default applies");
        assert_eq!(task.workspace.run_cmd, None, "no [workspace] means no run command");
    }

    /// The `[workspace]` section is additive. Every task.toml written before it
    /// existed must still load, and must not grow an empty section when the gate
    /// writes the file back.
    #[test]
    fn a_task_toml_without_a_workspace_section_is_unchanged() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/exercises/dedupe");
        let task = load(&dir).expect("the existing fixture still parses");

        assert_eq!(task.schema_version, "1", "the schema version did not move");
        assert_eq!(task.task.concept_id, "python_sets_and_order");
        assert!(task.workspace.is_empty());

        let back = toml::to_string_pretty(&task).expect("serialize");
        assert!(!back.contains("[workspace]"), "an absent section came back:\n{back}");
    }

    #[test]
    fn a_workspace_run_cmd_round_trips() {
        let text = r#"
schema_version = "1"

[task]
id = "dedupe-01"
concept_id = "python_sets_and_order"
kind = "kata"
guidance_level = "blank"

[verify]
cmd = "sh check/check.sh"
must_pass = ["correctness"]

[workspace]
run_cmd = "uv run --no-project solution.py"
"#;
        let task: Task = toml::from_str(text).expect("parse task.toml");
        assert_eq!(
            task.workspace.run_cmd.as_deref(),
            Some("uv run --no-project solution.py")
        );

        let back = toml::to_string_pretty(&task).expect("serialize");
        let again: Task = toml::from_str(&back).expect("reparse");
        assert_eq!(again, task, "a workspace did not survive the trip through TOML");
    }
}
