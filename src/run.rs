//! The execution boundary.
//!
//! Everything this tool runs is written by a model or by a learner. `solution/solve.sh`
//! and `check/check.sh` arrive from a generator; the workspace command is whatever the
//! learner typed. None of it has earned host access.
//!
//! The threat is not mainly a hostile author. It is a plausible generated mistake: a
//! delete with the wrong root, a path that resolves out of the workspace, a loop that
//! fills a disk, a process tree that will not die. A warning does not contain any of
//! those. See DESIGN.md §3.
//!
//! So there is exactly one way to execute anything: build a [`Job`] and hand it to a
//! [`Backend`]. Two backends exist. [`Backend::Sandbox`] is the default. The other is
//! spelled `UnsafeHost`, and a caller has to ask for it by that name.
//!
//! The two differ in isolation and in nothing else. Both scrub the environment to the
//! same allowlist, both impose the same resource limits, both lay the job out at the
//! same relative paths. That is deliberate: when an exercise passes under one backend
//! and fails under the other, the difference is isolation, and the search does not also
//! have to cover a stray `PYTHONPATH`.

use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Version of the *execution semantics*, not of this binary.
///
/// Recorded in a gate verdict and checked when the verdict is read. A gate result is a
/// claim that a grader discriminates, and that claim is only as good as the conditions
/// it was earned under: change what a job can see, which limits apply, how output is
/// captured, or when a process tree is killed, and every verdict on disk is describing
/// a run that can no longer happen. Bump this on any such change and they all re-gate.
///
/// Deliberately not the crate version. A release that touches only the graph, or the
/// card exporter, must not invalidate an exercise library.
pub const RUNNER_SEMANTICS: u32 = 1;

/// Grace period for output already in flight once the command is done or killed.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Where a job is assembled inside the sandbox.
///
/// A fixed path rather than the host one, so nothing a script prints reveals where the
/// caller keeps its state, and so the two backends put a job at the same *relative*
/// paths. Scripts address `work/`, `check/`, `out/` and `../solution/solve.sh` exactly
/// as they always have.
const GUEST_ROOT: &str = "/box";

/// `HOME` for the duration of a job: real, writable, and thrown away.
///
/// Under the sandbox it is a tmpfs. On the host it is a dot-directory beside the run,
/// which the editor's file listing already hides. A grader that scribbles into `$HOME`
/// is common and harmless; one that scribbles into the *real* `$HOME` is neither.
const HOME_DIR: &str = ".home";

/// Size of the private `/tmp` every job gets.
///
/// A ceiling rather than a budget: a runaway write fills 256 MiB of memory instead of
/// the user's disk. Named once because both isolating backends have to agree on it, and
/// a `/tmp` that is bounded under one and not the other is a job that passes on one
/// machine and fills a laptop on another.
pub(crate) const TMP_BYTES: u64 = 268_435_456;

/// Size of the tmpfs carrying `/box` and `$HOME` under the container backend.
///
/// Under bubblewrap these are directories on the sandbox's own root tmpfs and need no
/// size; a container has a real read-only rootfs, so the writable surfaces have to be
/// asked for. Small on purpose: the workspace is a bind mount and `/tmp` is where a
/// script is told to put scratch, so anything large landing here is a mistake.
const BOX_BYTES: u64 = 64 * 1024 * 1024;

/// Environment for every job, under both backends.
///
/// An allowlist rather than a filter, because the interesting variables are the ones
/// nobody thought to name: `PYTHONPATH`, `VIRTUAL_ENV`, `PYTHONSTARTUP`, `LD_PRELOAD`,
/// `GIT_*`, an `AWS_PROFILE`. A gate result that depended on any of them is a result
/// that will not reproduce, and a `check.sh` has no business reading them.
///
/// `LANG`/`LC_ALL`/`TZ` are pinned rather than dropped: sort order and date formatting
/// decide grader output, and inheriting them makes a verdict a property of the shell
/// that happened to launch the tool.
const ENV: [(&str, &str); 6] = [
    ("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin"),
    ("TMPDIR", "/tmp"),
    ("LANG", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    ("TZ", "UTC"),
    ("SHELL", "/bin/sh"),
];

/// Read-only host trees the sandbox needs for anything to run at all.
///
/// `-try` on all but `/usr` because the layout differs: merged-`/usr` systems reach
/// `/bin` and `/lib` through symlinks, and `/lib64` is absent on some architectures.
/// Nothing here is writable and nothing here is the caller's data.
const HOST_RO: [&str; 5] = ["/bin", "/sbin", "/lib", "/lib64", "/etc/alternatives"];

// ---------------------------------------------------------------------------
// What a job is
// ---------------------------------------------------------------------------

/// Whether a job may write to a directory it can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// Resource ceilings applied to a job.
///
/// These are backstops against a runaway, not a performance budget - the wall-clock
/// deadline is the real limit on how long anything takes. They exist because the
/// deadline alone does not bound how much damage a script does *before* it fires.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// CPU seconds across the process tree. Generous relative to the deadline: a
    /// threaded numpy grader legitimately burns several core-seconds per wall second,
    /// and this is here to stop a spin loop that somehow outlives the killer.
    pub cpu_secs: u32,
    /// Address space, in KiB. `RLIMIT_AS` rather than a cgroup because cgroups need
    /// privileges this tool does not have and will not ask for.
    pub address_space_kb: u64,
    /// Largest single file a job may create. The shell's block size is 512 or 1024
    /// bytes depending on which `/bin/sh` this is, so the effective ceiling is this
    /// number of KiB or half of it. Either is a cap; neither needs to be exact.
    pub file_size_kb: u64,
    /// Processes, against a fork bomb.
    pub processes: u32,
    /// Open file descriptors.
    pub open_files: u32,
    /// Bytes of stdout and of stderr kept. Reading continues past this so the job is
    /// never blocked on a full pipe - the excess is counted and dropped.
    pub output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            cpu_secs: 300,
            address_space_kb: 4 * 1024 * 1024,
            file_size_kb: 512 * 1024,
            processes: 512,
            open_files: 1024,
            output_bytes: 256 * 1024,
        }
    }
}

impl Limits {
    /// The `ulimit` prelude. Failures are swallowed: a shell that cannot set one of
    /// these still runs the job under the deadline, and an error line here would land
    /// in the grader's stderr and be read as the exercise's own output.
    ///
    /// `isolated` gates the process cap, and only that one. `RLIMIT_NPROC` counts
    /// processes per *user*, not per process tree, so on the host it is measured
    /// against the whole logged-in session. Set it below the session's count and
    /// nothing forks at all — the first thing this did on a desktop was fail every
    /// `fork` in the reference solution. Set it above and a fork bomb still has the
    /// headroom, because the budget was never the job's to spend. There is no useful
    /// value: the cap needs a namespace to be a cap, and inside one the count starts
    /// at zero and means what it says. Another entry on the list of things the host
    /// backend does not give you.
    fn prelude(&self, isolated: bool) -> String {
        let mut s = format!(
            "ulimit -t {} 2>/dev/null\n\
             ulimit -v {} 2>/dev/null\n\
             ulimit -f {} 2>/dev/null\n\
             ulimit -n {} 2>/dev/null\n",
            self.cpu_secs, self.address_space_kb, self.file_size_kb, self.open_files
        );
        if isolated {
            s.push_str(&format!("ulimit -u {} 2>/dev/null\n", self.processes));
        }
        s
    }
}

/// One command to execute.
///
/// `view` is the whole of what the job can reach of the caller's data: children of
/// `root`, named one at a time, each read-only or writable. Anything not named is not
/// there. That is what lets the gate run a reference solution without handing it the
/// hidden tests it is supposed to be proving something about.
#[derive(Debug)]
pub struct Job<'a> {
    /// Host directory holding the run. Never itself visible to the job.
    pub root: &'a Path,
    /// Children of `root` the job may see, and how.
    pub view: &'a [(&'a str, Access)],
    /// Working directory, relative to the root of the view. `""` is the root itself.
    pub cwd: &'a str,
    /// Shell script, run by `/bin/sh -c`.
    pub script: &'a str,
    /// Wall-clock deadline. On expiry the process tree is killed and `timed_out` set.
    pub timeout_secs: u32,
    pub limits: Limits,
    /// A warmed dependency set to bind read-only, with `PYTHONPATH` pointed at it.
    ///
    /// Read-only and outside `view` because it is not the caller's data and no job has
    /// any reason to write to it: one exercise must not be able to alter what the next
    /// one imports. See [`crate::deps`].
    pub deps: Option<&'a Path>,
}

impl<'a> Job<'a> {
    pub fn new(
        root: &'a Path,
        view: &'a [(&'a str, Access)],
        cwd: &'a str,
        script: &'a str,
        timeout_secs: u32,
    ) -> Self {
        Self { root, view, cwd, script, timeout_secs, limits: Limits::default(), deps: None }
    }

    /// Bind a warmed dependency set into the job.
    pub fn with_deps(mut self, deps: Option<&'a Path>) -> Self {
        self.deps = deps;
        self
    }

    fn host_cwd(&self) -> PathBuf {
        if self.cwd.is_empty() {
            self.root.to_path_buf()
        } else {
            self.root.join(self.cwd)
        }
    }

    fn guest_cwd(&self) -> String {
        if self.cwd.is_empty() {
            GUEST_ROOT.to_string()
        } else {
            format!("{GUEST_ROOT}/{}", self.cwd)
        }
    }

    /// Every path the job needs must exist before the backend starts.
    ///
    /// Checked here rather than left to the backend because a missing bind source is
    /// reported by `bwrap` as a mount failure, which reads as a broken sandbox rather
    /// than as the caller's missing directory.
    fn check_view(&self) -> Result<(), String> {
        for (name, _) in self.view {
            let path = self.root.join(name);
            if !path.exists() {
                return Err(format!("{}: not in the run directory", path.display()));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

/// How a job is executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// Isolated: no network, no host filesystem beyond the read-only runtime, no
    /// access to the caller's state, and a process namespace that dies as one.
    Sandbox { bwrap: PathBuf, version: String },
    /// Isolated by a container engine. The same absences — no network, no host
    /// filesystem, no study state — and one deliberate presence: the runtime is the
    /// image's, pinned by digest, rather than the machine's `/usr`.
    ///
    /// This is what makes the tool work where there are no Linux namespaces to
    /// unshare, and what makes a verdict describe an interpreter somebody chose. It is
    /// not a *second* sandbox: `name()` differs, so a verdict earned under one is
    /// refused under the other, and the image is recorded because it is the `/usr`.
    Container { cli: PathBuf, engine: &'static str, version: String, image: Image },
    /// Not isolated. The job runs as the user, with the user's rights, over the
    /// user's whole filesystem. The name is the documentation.
    UnsafeHost,
}

/// The runtime a container job gets, resolved to bytes.
///
/// `reference` is what the caller pinned and is evidence; `id` is what the engine
/// resolved it to and is *identity*. They are not interchangeable: one manifest-list
/// digest names a different image on every architecture, which is exactly the
/// difference a verdict must not be allowed to straddle. Jobs are launched by `id` for
/// the same reason — a tag, or even a re-pushed index, cannot move underneath a run
/// that has already been inspected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub reference: String,
    pub id: String,
    pub arch: String,
}

/// Which backend the caller is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Want {
    /// The sandbox if this machine has one, a container engine if it does not.
    ///
    /// Ordered rather than negotiated. Bubblewrap needs no daemon, no image and no
    /// pull, so where it works it stays the default and nothing about an existing
    /// library changes. The fallback is what a mac has.
    #[default]
    Auto,
    /// A container engine, refusing rather than falling back. What a Linux user asks
    /// for to gate against the same runtime a mac will use — and what the container
    /// tests need, since this machine has bubblewrap and `Auto` would never reach the
    /// code under test.
    Container,
    UnsafeHost,
}

impl Backend {
    /// Choose a backend.
    ///
    /// An absent sandbox is a refusal or a container, never a downgrade to the host.
    /// Claiming isolation while providing only a working directory would be worse than
    /// not having it: the caller would stop reading the warnings.
    pub fn choose(want: Want, image: Option<&str>) -> Result<Self, String> {
        let image = image.unwrap_or(DEFAULT_IMAGE);
        match want {
            Want::UnsafeHost => Ok(Backend::UnsafeHost),
            Want::Container => detect_container(image),
            Want::Auto => match SANDBOX.clone() {
                Ok(backend) => Ok(backend),
                // Both refusals, not just the second: on Linux the reader wants to know
                // bubblewrap was looked for, and on a mac the first line is the one that
                // explains why a container is being discussed at all.
                Err(no_sandbox) => detect_container(image)
                    .map_err(|no_container| format!("{no_sandbox}\n  {no_container}")),
            },
        }
    }

    /// Stable name for the record, and for humans reading a refusal.
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Sandbox { .. } => "sandbox",
            Backend::Container { .. } => "container",
            Backend::UnsafeHost => "unsafe-host",
        }
    }

    /// Execution profile: what a verdict earned under this backend was earned under.
    ///
    /// For the sandbox this names the isolation tool and its version, which is a real
    /// property of the run. For a container it names the engine, its version, the
    /// reference that was pinned and the architecture that reference resolved to —
    /// evidence a reader can act on, while the identity that decides staleness is
    /// [`Backend::image_id`]. The architecture is written here as well as being implied
    /// by the id because a refusal has to be readable: "running a different id" is true
    /// but unhelpful when the actual difference is that the verdict was earned on arm64.
    /// For the host backend it is deliberately just `host`: enumerating what the host
    /// provided is exactly the thing that cannot be done (see `digest::exercise_digest`),
    /// and a longer string would imply otherwise.
    pub fn profile(&self) -> String {
        match self {
            Backend::Sandbox { version, .. } => format!("bwrap {version}"),
            Backend::Container { engine, version, image, .. } => {
                format!("{engine} {version} {} ({})", image.reference, image.arch)
            }
            Backend::UnsafeHost => "host".to_string(),
        }
    }

    /// The exact runtime a verdict was earned against, when there is one.
    ///
    /// `None` for the two backends whose runtime is the host's, because there the
    /// honest answer is that it is not enumerable. `Some` is a promise of the opposite:
    /// these bytes, this architecture, and a refusal when they move.
    pub fn image_id(&self) -> Option<&str> {
        match self {
            Backend::Container { image, .. } => Some(&image.id),
            _ => None,
        }
    }

    pub fn run(&self, job: &Job) -> Result<Outcome, String> {
        job.check_view()?;
        // Namespaced process accounting, which decides whether `ulimit -u` means
        // anything. True only under bubblewrap: a container shares the host's uid, so
        // `RLIMIT_NPROC` is measured against the whole logged-in session there exactly
        // as it is on the host — the container's cap is `--pids-limit`, a cgroup on the
        // container, which is the thing an rlimit was standing in for.
        let namespaced = matches!(self, Backend::Sandbox { .. });
        let script = format!("{}{}", job.limits.prelude(namespaced), job.script);
        let (mut cmd, kill) = match self {
            Backend::Sandbox { bwrap, .. } => (sandbox_command(bwrap, job, &script)?, Kill::Group),
            Backend::Container { cli, image, .. } => {
                let name = container_name();
                let cmd = container_command(cli, image, job, &script, &name)?;
                (cmd, Kill::Container { cli: cli.clone(), name })
            }
            Backend::UnsafeHost => (host_command(job, &script)?, Kill::Group),
        };
        cmd
            // Own process group, so the deadline can kill everything the script
            // started. Killing only the shell leaves backgrounded grandchildren
            // running - and holding the output pipes open, which is what turns a
            // missed timeout into a permanent hang. Under the sandbox this is belt to
            // the PID namespace's braces; on the host it is the only mechanism there
            // is. Under a container it kills the *client*, which is why `Kill` also
            // stops the container the client was watching.
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_and_wait(cmd, job.timeout_secs, job.limits.output_bytes, kill)
    }
}

/// Detect the sandbox once per process.
///
/// Cached because every gate runs at least three jobs and the probe spawns a real
/// `bwrap`; and because the answer cannot change underneath a single command.
static SANDBOX: LazyLock<Result<Backend, String>> = LazyLock::new(detect_sandbox);

/// The refusal when `bwrap` is not on `$PATH`.
///
/// Two different failures wear the same missing binary. On Linux it is a package away.
/// Everywhere else `bwrap` is a Linux program — it isolates with Linux namespaces, and
/// there is nothing to install — so "install it" would send the reader after a package
/// that cannot exist. What is actionable there is a container engine, and the second
/// route is a Linux host: the state travels and the exercise library is plain JSON, so
/// the executing half is the only part that has to be anywhere in particular.
///
/// `--unsafe-host` is deliberately not offered here. It is offered on Linux, where the
/// reader has a working sandbox one package away and the flag is a considered
/// alternative; naming it as the *first* thing a mac user reads would make the easiest
/// path out of a refusal the one that runs generated scripts as them.
///
/// Takes the OS rather than reading `cfg!`, so the wording a mac user gets is reachable
/// from a test on the machine that wrote it.
fn no_sandbox_message(os: &str) -> String {
    if os == "linux" {
        "no sandbox available: `bwrap` (bubblewrap) is not on PATH. Install it, or pass \
         --unsafe-host to run generated scripts with your own user's rights."
            .to_string()
    } else {
        format!(
            "no sandbox available: the sandbox is bubblewrap, which isolates with Linux \
             namespaces, and this is {os}, where there is nothing to install. Run the \
             exercise half in a container instead - install docker or podman, then \
             `benkyou runner --pull` once - or run it on a Linux host."
        )
    }
}

fn detect_sandbox() -> Result<Backend, String> {
    let bwrap =
        which("bwrap").ok_or_else(|| no_sandbox_message(std::env::consts::OS))?;

    let version = Command::new(&bwrap)
        .arg("--version")
        .output()
        .map_err(|e| format!("no sandbox available: {} --version: {e}", bwrap.display()))
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;
    let version = version.strip_prefix("bubblewrap ").unwrap_or(&version).to_string();

    // Presence is not capability. Unprivileged user namespaces are disabled outright on
    // some kernels and restricted by LSM policy on others, and the failure surfaces
    // only on the first real mount - which would otherwise be the first exercise
    // somebody tried to gate.
    let probe = Command::new(&bwrap)
        .args(base_args()?)
        .args(["--chdir", "/", "/bin/sh", "-c", "exit 7"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("no sandbox available: {e}"))?;
    if probe.status.code() != Some(7) {
        return Err(format!(
            "no sandbox available: {} could not start a container ({}). Unprivileged \
             user namespaces may be disabled on this kernel. Pass --unsafe-host to run \
             generated scripts with your own user's rights.",
            bwrap.display(),
            String::from_utf8_lossy(&probe.stderr).trim()
        ));
    }

    Ok(Backend::Sandbox { bwrap, version })
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

/// Container engines this can drive, in the order they are tried.
///
/// Both take the same arguments for everything used here. Docker first only because a
/// machine with both is usually a machine where docker is the one that is running.
const ENGINES: [&str; 2] = ["docker", "podman"];

/// The runtime a container job gets unless the caller names another one.
///
/// Pinned to a manifest-list digest, which is one identity across every architecture in
/// it: the same reference resolves to an arm64 image on a mac and an amd64 image on a
/// desktop, and the per-platform id that lands in a verdict distinguishes the two. A
/// tag alone would make "the runner image" mean whatever was pushed last, which is the
/// one thing a recorded runtime must not do.
///
/// `python:3.13-slim` because the graders this tool grades with are shell and Python,
/// and slim is Debian rather than Alpine: musl has no manylinux wheels, so an Alpine
/// runner would turn every warmed dependency into a source build that cannot happen
/// offline. What the image does *not* have is as load-bearing as what it does — no
/// `sqlite3`, for one — and `--image` exists for exactly that.
pub const DEFAULT_IMAGE: &str =
    "python:3.13-slim@sha256:ffb752e139c0a19692a43af8d8523b274222dd68eebad5d583b45c2201c6e30a";

/// Label carried by every container this process starts, valued with its pid.
///
/// The container backend has no `--die-with-parent`. If this process is killed outright
/// the engine keeps running the job it was watching, and nothing else would ever stop
/// it. Each run is labelled with the pid that owns it, and the next detection kills the
/// ones whose owner is gone.
const OWNER_LABEL: &str = "benkyou.owner";

/// Find an engine, or say what to install.
fn find_engine() -> Result<(&'static str, PathBuf), String> {
    ENGINES
        .iter()
        .find_map(|name| which(name).map(|path| (*name, path)))
        .ok_or_else(|| {
            "no container engine: neither `docker` nor `podman` is on PATH. Install one, \
             or pass --unsafe-host to run generated scripts with your own user's rights."
                .to_string()
        })
}

/// What `benkyou runner` reports: the engine, and whether the runtime is here yet.
///
/// `id` absent is the normal state on a fresh machine and is not an error here - this is
/// the command that tells a reader what to do about it, so it has to be able to describe
/// the situation it exists to fix.
#[derive(Debug)]
pub struct RunnerStatus {
    pub engine: &'static str,
    pub version: String,
    pub reference: String,
    pub image: Option<Image>,
    pub pulled: bool,
}

/// Report the engine and the runner image, optionally fetching the image first.
///
/// The fetch lives here and nowhere else. `warm` is the only other command that touches
/// a network, and both are commands a person runs on purpose: everything on the gating
/// path resolves what is already local or refuses.
pub fn runner_status(reference: Option<&str>, pull: bool) -> Result<RunnerStatus, String> {
    let reference = reference.unwrap_or(DEFAULT_IMAGE).to_string();
    let (engine, cli) = find_engine()?;
    let version = Command::new(&cli)
        .arg("--version")
        .output()
        .map_err(|e| format!("{}: {e}", cli.display()))
        .map(|o| engine_version(&String::from_utf8_lossy(&o.stdout)))?;

    if pull {
        let out = Command::new(&cli)
            .args(["pull", &reference])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("{}: {e}", cli.display()))?;
        if !out.status.success() {
            return Err(format!(
                "{engine} pull {reference}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }

    Ok(RunnerStatus {
        engine,
        version,
        image: inspect_image(&cli, engine, &reference).ok(),
        reference,
        pulled: pull,
    })
}

fn detect_container(reference: &str) -> Result<Backend, String> {
    let (engine, cli) = find_engine()?;

    let version = Command::new(&cli)
        .arg("--version")
        .output()
        .map_err(|e| format!("{}: {e}", cli.display()))
        .map(|o| engine_version(&String::from_utf8_lossy(&o.stdout)))?;

    let image = inspect_image(&cli, engine, reference)?;

    // Presence is not capability, and here that covers more than a kernel feature: a
    // daemon that is installed but not running, an engine that rejects one of the
    // policy flags, an SELinux host that denies the bind mounts, a cgroup without
    // memory accounting. All of them fail on the first real job otherwise, which is
    // the middle of somebody's gate.
    let backend = Backend::Container { cli, engine, version, image };
    probe_container(&backend)?;

    if let Backend::Container { cli, .. } = &backend {
        reap_orphans(cli);
    }
    Ok(backend)
}

/// `docker version 29.7.2, build …` → `29.7.2`. Cosmetic, and cosmetic in a field that
/// gets compared: a build hash that moves with every point release would make the
/// drift warning fire on upgrades nobody needs to hear about.
fn engine_version(line: &str) -> String {
    line.split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or("unknown")
        .trim_end_matches(',')
        .to_string()
}

/// Resolve a reference to the bytes behind it, locally and without a network.
///
/// A pull here would put a network on the gating path, which is the one thing the whole
/// dependency mechanism exists to avoid: an image fetched mid-gate is a runtime nobody
/// chose, arriving at the least reviewable moment. So an absent image is a refusal
/// naming the command that fetches it on purpose.
fn inspect_image(cli: &Path, engine: &str, reference: &str) -> Result<Image, String> {
    let out = Command::new(cli)
        .args(["image", "inspect", "--format", "{{.Id}} {{.Architecture}}", reference])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", cli.display()))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!(
            "runner image not present: {reference}\n  {why}\n  run `benkyou runner --pull` \
             once, on purpose: gate, attempt, grade and serve never reach a network."
        ));
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (id, arch) = line
        .split_once(' ')
        .ok_or_else(|| format!("{engine}: could not read the id of {reference}: {line:?}"))?;
    if !id.starts_with("sha256:") {
        return Err(format!("{engine}: {reference} reported no digest id ({id:?})"));
    }
    Ok(Image {
        reference: reference.to_string(),
        id: id.to_string(),
        arch: arch.trim().to_string(),
    })
}

/// Run one job under the real policy, through the real path, with a real bind mount.
///
/// Not a cheaper imitation: it goes through [`Backend::run`], so the prelude, the
/// deadline and the mounts are the ones a gate will get. A probe that tested something
/// easier is a probe that passes on a machine where the first exercise fails.
fn probe_container(backend: &Backend) -> Result<(), String> {
    // Named like a job rather than after the process: every thread of one process shares
    // a pid, so a pid-keyed directory let two concurrent detections delete each other's
    // witness file mid-probe. Found by the container tests, which detect once per test.
    let dir = std::env::temp_dir().join(format!("benkyou-probe-{}", container_name()));
    let _ = fs::remove_dir_all(&dir);
    let work = dir.join("work");
    fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    fs::write(work.join("witness"), "witness").map_err(|e| format!("{}: {e}", work.display()))?;

    let job = Job::new(
        &dir,
        &[("work", Access::Read)],
        "",
        "cat work/witness > /dev/null && : > /tmp/probe && exit 7",
        60,
    );
    let outcome = backend.run(&job);
    let _ = fs::remove_dir_all(&dir);

    match outcome {
        Ok(out) if out.exit_code == Some(7) => Ok(()),
        Ok(out) => Err(format!(
            "{} could not run a job ({}). Is the engine running?",
            backend.profile(),
            if out.stderr.trim().is_empty() { out.stdout.trim() } else { out.stderr.trim() }
        )),
        Err(e) => Err(format!("{}: {e}", backend.profile())),
    }
}

/// Kill containers left behind by a benkyou that is no longer running.
///
/// Best effort throughout: this is tidying, and a failure to tidy must never stop a
/// gate. Liveness is asked of the shell rather than of libc, for the same reason
/// `kill_group` does — `kill -0` is a shell builtin everywhere `/bin/sh` is.
fn reap_orphans(cli: &Path) {
    let Ok(out) = Command::new(cli)
        .args([
            "ps",
            "--filter",
            &format!("label={OWNER_LABEL}"),
            "--format",
            &format!("{{{{.ID}}}} {{{{.Label \"{OWNER_LABEL}\"}}}}"),
        ])
        .stdin(Stdio::null())
        .output()
    else {
        return;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((id, owner)) = line.split_whitespace().next().zip(line.split_whitespace().nth(1))
        else {
            continue;
        };
        if owner.parse::<u32>().is_err() {
            continue;
        }
        let alive = Command::new("/bin/sh")
            .args(["-c", &format!("kill -0 {owner} 2>/dev/null")])
            .status()
            .map(|s| s.success())
            .unwrap_or(true);
        if !alive {
            let _ = Command::new(cli)
                .args(["kill", id])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// A name per job, so the deadline has something to kill.
///
/// The pid alone is not enough: a gate runs at least three jobs, and `serve` runs one
/// per press of a button, so a shared name would make the second run collide with the
/// first and a kill hit whichever container happened to hold it.
fn container_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "benkyou-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// The isolation policy, with no job-specific mounts.
///
/// Split out so the capability probe runs under exactly the policy a real job gets.
/// A probe that tested something easier would pass on a kernel where the real thing
/// fails.
fn base_args() -> Result<Vec<String>, String> {
    let (passwd, group) = identity_files()?;
    let home = format!("{GUEST_ROOT}/{HOME_DIR}");
    let mut a: Vec<String> = Vec::new();

    let fixed: &[&str] = &[
        // Namespaces named one at a time rather than --unshare-all, which is `-try`
        // for the user namespace: on a kernel without one that would silently continue
        // in the host's, and the caller would be told it was sandboxed.
        "--unshare-user",
        "--unshare-ipc",
        "--unshare-pid",
        "--unshare-net",
        "--unshare-uts",
        "--unshare-cgroup-try",
        // Kills the container if this process dies, however it dies.
        "--die-with-parent",
        // No controlling terminal: a job must not be able to push characters back
        // into the terminal the user is sitting at.
        "--new-session",
        "--clearenv",
        // Mount parents made explicitly, before anything lands under them. bwrap will
        // create a missing parent on demand, but then its mode is whatever the default
        // is; naming them fixes the permissions and fixes the order, so a later
        // `--bind` cannot be the thing that decides what `/box` looks like.
        "--perms",
        "0755",
        "--dir",
        "/etc",
        "--perms",
        "0755",
        "--dir",
        GUEST_ROOT,
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--ro-bind",
        "/usr",
        "/usr",
    ];
    a.extend(fixed.iter().map(|s| s.to_string()));
    // Pushed rather than sitting in the array above, so the ceiling is written down in
    // exactly one place: `--size` applies to the mount that follows it.
    a.extend([
        "--size".to_string(),
        TMP_BYTES.to_string(),
        "--perms".to_string(),
        "1777".to_string(),
        "--tmpfs".to_string(),
        "/tmp".to_string(),
    ]);
    for path in HOST_RO {
        a.extend(["--ro-bind-try".to_string(), path.to_string(), path.to_string()]);
    }
    // A synthetic passwd rather than the host's: a failing `getpwuid` breaks Python's
    // `expanduser` and a lot of tooling besides, and the host's file lists every
    // account on the machine to a script that has no business enumerating them.
    for (src, dst) in [(passwd, "/etc/passwd"), (group, "/etc/group")] {
        a.extend(["--ro-bind".to_string(), src.display().to_string(), dst.to_string()]);
    }
    for (k, v) in ENV {
        a.extend(["--setenv".to_string(), k.to_string(), v.to_string()]);
    }
    a.extend(["--setenv".to_string(), "HOME".to_string(), home.clone()]);
    a.extend(["--tmpfs".to_string(), home]);
    Ok(a)
}

/// A one-line `/etc/passwd` and `/etc/group`, bind-mounted over the host's.
///
/// One directory per user, reused by every process and every run. The obvious shape
/// is a per-pid directory, and it leaks: a `LazyLock` static never drops, and a kata
/// killed by its deadline never reaches an exit hook either, so `/tmp` grows by one
/// directory per invocation forever. The contents are a pure function of uid and gid,
/// so sharing is safe and a directory left by an earlier run is already correct.
static IDENTITY: LazyLock<Result<(PathBuf, PathBuf), String>> = LazyLock::new(|| {
    let (uid, gid) = uid_gid()?;
    let dir = identity_dir(&std::env::temp_dir(), uid)?;
    write_identity(&dir, uid, gid)
});

/// This process's effective uid and gid, read once.
///
/// From a file this process just created rather than from `/proc/self`, which does not
/// exist on macOS - and the container backend has to work there, since it is the whole
/// reason it exists. A freshly created file is owned by the effective uid and gid by
/// definition, so the answer is the same one `/proc` gave on Linux without asking a
/// second operating system for a filesystem it does not have. No subprocess either: an
/// `id -u` would be one more thing to find on `PATH` before anything can run.
static UID_GID: LazyLock<Result<(u32, u32), String>> = LazyLock::new(|| {
    use std::os::unix::fs::MetadataExt;
    let probe = std::env::temp_dir().join(format!("benkyou-whoami-{}", std::process::id()));
    let _ = fs::remove_file(&probe);
    write_new(&probe, "")?;
    let md = fs::metadata(&probe).map_err(|e| format!("{}: {e}", probe.display()));
    let _ = fs::remove_file(&probe);
    let md = md?;
    Ok((md.uid(), md.gid()))
});

pub(crate) fn uid_gid() -> Result<(u32, u32), String> {
    UID_GID.as_ref().copied().map_err(Clone::clone)
}

/// Write the two files, returning their paths.
///
/// Split from the directory check above because it is the half with the interesting
/// failure: the directory being ours does not make every name inside it ours, and
/// these two files decide what the sandbox resolves for every uid and gid.
fn write_identity(dir: &Path, uid: u32, gid: u32) -> Result<(PathBuf, PathBuf), String> {
    let passwd = dir.join("passwd");
    let group = dir.join("group");
    let body = [
        (
            &passwd,
            format!(
                "root:x:0:0:root:/:/bin/sh\n\
                 box:x:{uid}:{gid}:box:{GUEST_ROOT}/{HOME_DIR}:/bin/sh\n"
            ),
        ),
        (&group, format!("root:x:0:\nbox:x:{gid}:\n")),
    ];
    // Staged under a per-pid name and renamed, so two runs starting at once cannot
    // have one read a file the other is halfway through writing.
    for (path, text) in body {
        let staging = dir.join(format!(
            "{}.{}",
            path.file_name().and_then(|s| s.to_str()).unwrap_or("f"),
            std::process::id()
        ));
        write_new(&staging, &text)?;
        fs::rename(&staging, path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok((passwd, group))
}

/// Create `path` and write `text`, refusing to write through anything already there.
///
/// `fs::write` opens with `O_TRUNC` and follows symlinks, so a link sitting at this
/// name would send the write to its target. `create_new` is `O_EXCL`: it fails on any
/// existing name, link or not. A staging file that already exists is the debris of a
/// crashed run whose pid has been reused, which is expected rather than hostile, so
/// it is unlinked once and retried - unlinking removes the link itself, never what it
/// points at, which is exactly the property `fs::write` lacks.
fn write_new(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let attempt = || -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).open(path)?;
        f.write_all(text.as_bytes())
    };
    match attempt() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
            attempt().map_err(|e| format!("{}: {e}", path.display()))
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Create or adopt this user's identity directory under `base`.
///
/// A shared path in a world-writable directory has to be checked before it is
/// trusted: the two files written here become `/etc/passwd` and `/etc/group` inside
/// the sandbox, so whoever owns the directory chooses what a run sees for every name
/// lookup. A symlink is refused rather than followed, for the same reason
/// `safe_join` refuses one.
fn identity_dir(base: &Path, uid: u32) -> Result<PathBuf, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let dir = base.join(format!("benkyou-box-{uid}"));
    // `create_dir_all` is content with an existing symlink to a directory, so the
    // check below is what decides, not this call.
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let md = fs::symlink_metadata(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if md.file_type().is_symlink() || !md.is_dir() {
        return Err(format!("{}: not a directory", dir.display()));
    }
    if md.uid() != uid {
        return Err(format!("{}: owned by uid {}, not {uid}", dir.display(), md.uid()));
    }
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

fn identity_files() -> Result<&'static (PathBuf, PathBuf), String> {
    IDENTITY.as_ref().map_err(|e| e.clone())
}

fn sandbox_command(bwrap: &Path, job: &Job, script: &str) -> Result<Command, String> {
    let mut cmd = Command::new(bwrap);
    cmd.args(base_args()?);
    for (name, access) in job.view {
        let flag = match access {
            Access::Write => "--bind",
            Access::Read => "--ro-bind",
        };
        cmd.args([
            flag,
            &job.root.join(name).display().to_string(),
            &format!("{GUEST_ROOT}/{name}"),
        ]);
    }
    // Read-only, and named outside `view` so no caller can hand it out writable.
    // `PYTHONPATH` is set here rather than inherited: the ENV allowlist exists because
    // an inherited `PYTHONPATH` makes a verdict depend on the shell that launched the
    // tool, and this one is a property of the exercise instead - declared in
    // `task.toml`, warmed on purpose, identical on every run.
    if let Some(set) = job.deps {
        cmd.args(["--ro-bind", &set.display().to_string(), crate::deps::GUEST_DEPS]);
        cmd.args(["--setenv", "PYTHONPATH", crate::deps::GUEST_DEPS]);
    }
    cmd.args(["--chdir", &job.guest_cwd(), "/bin/sh", "-c", script]);
    // bwrap itself inherits nothing; --clearenv governs the child.
    cmd.env_clear();
    Ok(cmd)
}

fn host_command(job: &Job, script: &str) -> Result<Command, String> {
    // The one thing this backend can still honour: keep a grader that writes to $HOME
    // out of the real one. It is not containment and is not offered as any.
    let home = job.root.join(HOME_DIR);
    fs::create_dir_all(&home).map_err(|e| format!("{}: {e}", home.display()))?;

    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(script).current_dir(job.host_cwd());
    cmd.env_clear();
    for (k, v) in ENV {
        cmd.env(k, v);
    }
    cmd.env("HOME", &home);
    // The host path, since there is no mount namespace to put it anywhere else. Not
    // read-only: this backend cannot make it so, which is one more entry on the list of
    // things it does not give you.
    if let Some(set) = job.deps {
        cmd.env("PYTHONPATH", set);
    }
    Ok(cmd)
}

/// The container policy, with no job-specific mounts.
///
/// Every flag here is one of the guarantees the sandbox gets from a namespace, asked of
/// the engine instead:
///
/// - `--network none`: no route out, the same absence `--unshare-net` gives.
/// - `--read-only`: the image's filesystem cannot be written, so nothing a job does
///   survives it and no two jobs can see each other's leftovers.
/// - `--cap-drop ALL` and `no-new-privileges`: a job starts with no capabilities and
///   cannot acquire any, which is the part `--unshare-user` gets for free by having
///   none to begin with.
/// - `--pids-limit`: the real process cap, and the reason `ulimit -u` is not applied
///   here. A container shares the host's uid, so `RLIMIT_NPROC` would be measured
///   against the whole session exactly as it is on the host; a cgroup counts the
///   container.
/// - `--memory`/`--memory-swap` equal: a ceiling on resident memory that cannot be
///   dodged by swapping. `ulimit -v` still applies from the prelude; one bounds the
///   address space a process may ask for, the other what the container may hold.
/// - `--user`: the caller's own uid, so files a job writes into its workspace belong to
///   the caller and not to root. This is what makes the writable view usable at all.
/// - The three tmpfs mounts are the writable surfaces: `/box` for the job's root,
///   `$HOME` under it, and a bounded `/tmp`. Each is owned by the caller's uid, because
///   a root-owned tmpfs under `--user` is a directory the job cannot write - which
///   would diverge from the sandbox in the one place a grader always touches.
/// - The synthetic `/etc/passwd` and `/etc/group` are the same two files the sandbox
///   binds, and for the same reason: `--user 1000` names a uid the image has never
///   heard of, and a failing `getpwuid` breaks Python's `expanduser` and much else.
fn container_policy(limits: &Limits) -> Result<Vec<String>, String> {
    let (uid, gid) = uid_gid()?;
    let (passwd, group) = identity_files()?;
    let home = format!("{GUEST_ROOT}/{HOME_DIR}");
    let own = format!("uid={uid},gid={gid}");

    let mut a: Vec<String> = ["run", "--rm", "--network", "none", "--read-only"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    a.extend(
        [
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--pids-limit",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    a.push(limits.processes.to_string());
    a.extend(["--memory".to_string(), format!("{}k", limits.address_space_kb)]);
    a.extend(["--memory-swap".to_string(), format!("{}k", limits.address_space_kb)]);
    a.extend(["--user".to_string(), format!("{uid}:{gid}")]);
    a.extend([
        "--tmpfs".to_string(),
        format!("{GUEST_ROOT}:rw,mode=0755,{own},size={BOX_BYTES}"),
    ]);
    a.extend([
        "--tmpfs".to_string(),
        format!("{home}:rw,mode=0700,{own},size={BOX_BYTES}"),
    ]);
    a.extend([
        "--tmpfs".to_string(),
        format!("/tmp:rw,mode=1777,size={TMP_BYTES}"),
    ]);
    for (src, dst) in [(passwd, "/etc/passwd"), (group, "/etc/group")] {
        a.extend([
            "--mount".to_string(),
            format!("type=bind,source={},target={dst},readonly", src.display()),
        ]);
    }
    for (k, v) in ENV {
        a.extend(["--env".to_string(), format!("{k}={v}")]);
    }
    a.extend(["--env".to_string(), format!("HOME={home}")]);
    Ok(a)
}

fn container_command(
    cli: &Path,
    image: &Image,
    job: &Job,
    script: &str,
    name: &str,
) -> Result<Command, String> {
    let mut cmd = Command::new(cli);
    cmd.args(container_policy(&job.limits)?);
    cmd.args(["--name", name]);
    cmd.args([
        "--label".to_string(),
        format!("{OWNER_LABEL}={}", std::process::id()),
    ]);

    for (entry, access) in job.view {
        let mut mount = format!(
            "type=bind,source={},target={GUEST_ROOT}/{entry}",
            job.root.join(entry).display()
        );
        if *access == Access::Read {
            mount.push_str(",readonly");
        }
        cmd.args(["--mount".to_string(), mount]);
    }
    // Read-only, and named outside `view` so no caller can hand it out writable.
    // `PYTHONPATH` is set here rather than inherited, for the reason the whole
    // environment is an allowlist: a verdict must not depend on the shell that launched
    // the tool. This one is a property of the exercise - declared in `task.toml`,
    // warmed on purpose, identical on every run.
    if let Some(set) = job.deps {
        cmd.args([
            "--mount".to_string(),
            format!(
                "type=bind,source={},target={},readonly",
                set.display(),
                crate::deps::GUEST_DEPS
            ),
        ]);
        cmd.args([
            "--env".to_string(),
            format!("PYTHONPATH={}", crate::deps::GUEST_DEPS),
        ]);
    }
    cmd.args(["--workdir", &job.guest_cwd()]);
    // `/bin/sh` rather than the image's entrypoint: a runner image is not required to
    // have one, and an image whose entrypoint wrapped the script would be executing
    // something this tool never wrote.
    cmd.args(["--entrypoint", "/bin/sh"]);
    // By id, not by reference. The reference was resolved once, at detection; a tag or
    // an index that moved in between would otherwise put a different runtime under a
    // verdict that names the old one.
    cmd.args([&image.id, "-c", script]);
    // The client keeps the caller's environment, deliberately, and it does not reach
    // the job: an engine forwards only what `--env` names, so `DOCKER_HOST`,
    // `CONTAINER_HOST` and a rootless `XDG_RUNTIME_DIR` are how the client finds its
    // daemon rather than something a script can read. Clearing them would break
    // rootless podman and every non-default context for no gain.
    //
    // What the allowlist cannot govern here is the image's *own* `ENV`. A python image
    // exports `PYTHON_VERSION` and a `GPG_KEY`, and there is no engine flag that
    // unsets them. That is a property of the image rather than of the caller's shell,
    // and the image is the one thing a container verdict records exactly - so it is a
    // difference from the sandbox, and a bounded one.
    Ok(cmd)
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// How a run is stopped when the deadline fires, or when output is still in flight
/// after it should not be.
///
/// The process group is enough for the two backends whose job *is* the child process
/// tree. It is not enough for a container: killing the engine's client leaves the
/// container running - the daemon owns it, and it never noticed the client leave. So a
/// container is stopped by name first, and the group is killed afterwards to collect
/// the client. The order matters in that direction only: kill the client first and the
/// name may still be running a fork bomb nobody is watching.
enum Kill {
    Group,
    Container { cli: PathBuf, name: String },
}

impl Kill {
    fn fire(&self, pgid: u32) {
        if let Kill::Container { cli, name } = self {
            let _ = Command::new(cli)
                .args(["kill", name])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        kill_group(pgid);
    }
}

fn spawn_and_wait(
    mut cmd: Command,
    timeout_secs: u32,
    cap: usize,
    kill: Kill,
) -> Result<Outcome, String> {
    let started = Instant::now();
    let mut child = cmd.spawn().map_err(|e| format!("failed to start: {e}"))?;
    let pgid = child.id();

    // Drain both pipes on their own threads. Polling for the deadline while the child
    // fills a pipe buffer would deadlock: it blocks on write, we never reap it, and the
    // deadline fires on a process that was making progress.
    let out_rx = drain(child.stdout.take().expect("piped"), cap);
    let err_rx = drain(child.stderr.take().expect("piped"), cap);

    let deadline = Duration::from_secs(timeout_secs as u64);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if started.elapsed() >= deadline => {
                kill.fire(pgid);
                timed_out = true;
                break child.wait().map_err(|e| e.to_string())?;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };

    // A well-behaved command that has exited may still have left a daemonised
    // grandchild holding the write end. Never block on it indefinitely: collect what
    // arrived, kill the group to release the rest, and move on. Output is diagnostic;
    // the verdict comes from the exit code and the reward file.
    let (stdout, out_cut) = collect(&out_rx, &mut || kill.fire(pgid));
    let (stderr, err_cut) = collect(&err_rx, &mut || kill.fire(pgid));

    Ok(Outcome {
        // A killed child reports no code. Distinguishing that from a real exit is what
        // `timed_out` is for; grading treats the two differently.
        exit_code: status.code(),
        timed_out,
        truncated: out_cut || err_cut,
        elapsed_secs: started.elapsed().as_secs_f32(),
        stdout,
        stderr,
    })
}

/// Read a pipe to the end, keeping at most `cap` bytes.
///
/// Reading continues past the cap rather than stopping: a job that stops being read
/// blocks on a full pipe and dies to the deadline, which reports an output bomb as a
/// hang. Counting and discarding reports it as what it is, and bounds this process's
/// memory either way.
fn drain<R: Read + Send + 'static>(mut pipe: R, cap: usize) -> mpsc::Receiver<(Vec<u8>, bool)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut kept: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut truncated = false;
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(kept.len());
                    if room == 0 {
                        truncated = true;
                    } else if n > room {
                        kept.extend_from_slice(&chunk[..room]);
                        truncated = true;
                    } else {
                        kept.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        }
        let _ = tx.send((kept, truncated));
    });
    rx
}

/// Wait for a drained pipe, but only for a bounded grace period. On expiry run
/// `release` — which kills whatever still holds the pipe — and try once more.
fn collect(
    rx: &mpsc::Receiver<(Vec<u8>, bool)>,
    release: &mut dyn FnMut(),
) -> (String, bool) {
    let got = match rx.recv_timeout(DRAIN_GRACE) {
        Ok(v) => Some(v),
        Err(_) => {
            release();
            rx.recv_timeout(DRAIN_GRACE).ok()
        }
    };
    match got {
        Some((bytes, cut)) => (String::from_utf8_lossy(&bytes).into_owned(), cut),
        None => (String::new(), false),
    }
}

/// Kill a whole process group. Routed through `sh` so it uses the shell's built-in
/// `kill`, which is available anywhere `/bin/sh` is — no libc dependency, and no
/// assumption that a standalone `kill` binary is on PATH.
fn kill_group(pgid: u32) {
    let _ = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -KILL -{pgid} 2>/dev/null"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[derive(Debug, Clone)]
pub struct Outcome {
    /// The command's own exit code, or `None` when it was killed by a signal.
    pub exit_code: Option<i32>,
    /// True when the wall-clock deadline fired, including our own kill.
    pub timed_out: bool,
    /// True when output exceeded `Limits::output_bytes` and was cut.
    pub truncated: bool,
    pub elapsed_secs: f32,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// The wording of a refusal is the whole of what a caller on the wrong platform gets,
/// and the case that matters is the one this machine cannot reach.
#[cfg(test)]
mod refusal_tests {
    use super::no_sandbox_message;

    #[test]
    fn linux_is_told_to_install_it() {
        let msg = no_sandbox_message("linux");
        assert!(msg.contains("not on PATH"), "{msg}");
        assert!(msg.contains("Install it"), "{msg}");
        assert!(msg.contains("--unsafe-host"), "{msg}");
    }

    /// On a mac there is nothing to install, so advice to install is worse than none:
    /// it sends the reader to a Homebrew formula for a Linux namespace API. What is
    /// actionable there is a container engine, and the refusal has to say so — a reader
    /// who reaches for `--unsafe-host` because the message named nothing else is the
    /// failure this wording exists to prevent, which is why the flag is absent from it.
    #[test]
    fn a_mac_is_pointed_at_a_container() {
        let msg = no_sandbox_message("macos");
        assert!(msg.contains("macos"), "the refusal must name the platform: {msg}");
        assert!(!msg.contains("Install it"), "nothing to install on a mac: {msg}");
        assert!(msg.contains("Linux namespaces"), "{msg}");
        assert!(msg.contains("Linux host"), "the other route must stay named: {msg}");
        assert!(!msg.contains("--unsafe-host"), "must not offer the host backend: {msg}");
        for wanted in ["docker", "podman", "benkyou runner --pull"] {
            assert!(msg.contains(wanted), "refusal did not name {wanted}: {msg}");
        }
    }
}

#[cfg(test)]
mod identity_tests {
    use super::{identity_dir, write_identity};
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bk-ident-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// The property the per-pid version failed: repeated calls converge on one
    /// directory instead of leaving one behind per invocation.
    #[test]
    fn repeated_calls_reuse_one_directory() {
        let base = scratch("reuse");
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let first = identity_dir(&base, uid).expect("first call");
        fs::write(first.join("passwd"), "x").unwrap();
        for _ in 0..5 {
            assert_eq!(identity_dir(&base, uid).expect("later call"), first);
        }
        let dirs: Vec<_> = fs::read_dir(&base).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(dirs.len(), 1, "left behind: {dirs:?}");
        // Adopting is not erasing: a run must not lose the files a sibling wrote.
        assert_eq!(fs::read_to_string(first.join("passwd")).unwrap(), "x");
        let _ = fs::remove_dir_all(&base);
    }

    /// These files become `/etc/passwd` and `/etc/group` in the sandbox, so a symlink
    /// planted at the path must be refused, never followed.
    #[test]
    fn a_symlink_is_refused() {
        let base = scratch("symlink");
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let target = base.join("elsewhere");
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, base.join(format!("benkyou-box-{uid}"))).unwrap();
        let err = identity_dir(&base, uid).expect_err("a symlink must not be adopted");
        assert!(err.contains("not a directory"), "{err}");
        let _ = fs::remove_dir_all(&base);
    }

    /// `fs::write` follows a symlink and truncates its target, so a link planted at
    /// the staging name would redirect the write out of the directory entirely. The
    /// link must be replaced, and whatever it pointed at must be untouched.
    #[test]
    fn a_symlink_at_the_staging_name_is_not_written_through() {
        let base = scratch("staging");
        let dir = base.join("box");
        fs::create_dir_all(&dir).unwrap();
        let canary = base.join("canary");
        fs::write(&canary, "untouched").unwrap();
        for name in ["passwd", "group"] {
            let staging = dir.join(format!("{name}.{}", std::process::id()));
            std::os::unix::fs::symlink(&canary, &staging).unwrap();
        }

        let (passwd, group) = write_identity(&dir, 1000, 1000).expect("write");

        assert_eq!(
            fs::read_to_string(&canary).unwrap(),
            "untouched",
            "the write followed the symlink out of the directory"
        );
        assert!(fs::read_to_string(&passwd).unwrap().contains("box:x:1000:1000"));
        assert!(fs::read_to_string(&group).unwrap().contains("box:x:1000:"));
        assert!(!fs::symlink_metadata(&passwd).unwrap().file_type().is_symlink());
        let _ = fs::remove_dir_all(&base);
    }

    /// Debris from a crashed run whose pid has been reused is expected, not hostile:
    /// the next run must clear it rather than refuse to start.
    #[test]
    fn a_stale_staging_file_does_not_wedge_the_next_run() {
        let base = scratch("stale");
        let dir = base.join("box");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("passwd.{}", std::process::id())), "junk").unwrap();

        let (passwd, _) = write_identity(&dir, 1000, 1000).expect("stale debris must clear");

        assert!(fs::read_to_string(&passwd).unwrap().contains("box:x:1000:1000"));
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains('.'))
            .collect();
        assert!(left.is_empty(), "staging files left behind: {left:?}");
        let _ = fs::remove_dir_all(&base);
    }

    /// Readable by anyone would mean any other account on the machine can see, and
    /// race, the identity a run is about to mount.
    #[test]
    fn the_directory_is_private() {
        let base = scratch("mode");
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let dir = base.join(format!("benkyou-box-{uid}"));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        let got = identity_dir(&base, uid).expect("an owned directory is adopted");
        let mode = fs::metadata(&got).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "left world-readable: {mode:o}");
        let _ = fs::remove_dir_all(&base);
    }
}
