//! The execution boundary.
//!
//! A model or a learner writes everything this tool runs, and none of it gets host access.
//! The threat is a plausible generated mistake: a wrong delete root, a path out of the
//! workspace, a disk-filling loop, a process tree that will not die. A warning contains none
//! of those. See DESIGN.md §3.
//!
//! So there is one way to execute anything: build a [`Job`] and hand it to a [`Backend`].
//! [`Backend::Sandbox`] is the default. A caller has to ask for `UnsafeHost` by that name.
//!
//! The two differ in isolation and in nothing else. Same environment allowlist, same limits,
//! same relative paths. A job that passes under one and fails under the other differs by
//! isolation alone, so the search never has to cover a stray `PYTHONPATH`.

use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Version of the execution semantics, not of this binary.
///
/// A verdict records it, and the reader of a verdict checks it. Bump it when what a job
/// sees, which limits apply, how output is captured, or when a process tree is killed,
/// changes. Every verdict on disk then describes a run that cannot happen. Not the crate
/// version, so a release that touches only the graph keeps a library valid.
pub const RUNNER_SEMANTICS: u32 = 1;

/// Grace period for output already in flight once the command is done or killed.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Where a job is assembled inside the sandbox.
///
/// Fixed rather than the host path, so no script output reveals where the caller keeps its
/// state. Both backends then put a job at the same relative paths.
const GUEST_ROOT: &str = "/box";

/// `HOME` for the duration of a job: real, writable, and thrown away.
///
/// A tmpfs under the sandbox, a hidden dot-directory beside the run on the host. A grader
/// that writes into `$HOME` is common, and it must not reach the user's own.
const HOME_DIR: &str = ".home";

/// Size of the private `/tmp` every job gets.
///
/// A ceiling, not a budget: a runaway write fills 256 MiB of memory and not the user's disk.
/// Both isolating backends use this one number, so a job behaves the same under each.
pub(crate) const TMP_BYTES: u64 = 268_435_456;

/// Size of the tmpfs carrying `/box` and `$HOME` under the container backend.
///
/// A container has a real read-only rootfs, so writable surfaces must be asked for and
/// sized. Small on purpose: the workspace is a bind mount and scratch belongs in `/tmp`.
const BOX_BYTES: u64 = 64 * 1024 * 1024;

/// Environment for every job, under both backends.
///
/// An allowlist, because the dangerous variables are the ones nobody names: `PYTHONPATH`,
/// `VIRTUAL_ENV`, `PYTHONSTARTUP`, `LD_PRELOAD`, `GIT_*`, `AWS_PROFILE`. `LANG`, `LC_ALL`
/// and `TZ` are pinned rather than dropped, because sort order and date format decide
/// grader output.
const ENV: [(&str, &str); 6] = [
    (
        "PATH",
        "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin",
    ),
    ("TMPDIR", "/tmp"),
    ("LANG", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    ("TZ", "UTC"),
    ("SHELL", "/bin/sh"),
];

/// Read-only host trees the sandbox needs for anything to run at all.
///
/// `-try` on all but `/usr`: merged-`/usr` systems reach `/bin` and `/lib` by symlink, and
/// `/lib64` is absent on some architectures.
const HOST_RO: [&str; 5] = ["/bin", "/sbin", "/lib", "/lib64", "/etc/alternatives"];

// ---------------------------------------------------------------------------
// What a job is
// ---------------------------------------------------------------------------

/// Whether a job can write to a directory in its view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// Resource ceilings applied to a job.
///
/// Backstops against a runaway, not a performance budget. The wall-clock deadline limits how
/// long a job takes, and it does not bound the damage done before it fires.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// CPU seconds across the process tree. Generous, because a threaded numpy grader burns
    /// several core-seconds per wall second.
    pub cpu_secs: u32,
    /// Address space, in KiB. `RLIMIT_AS` rather than a cgroup, which needs privileges this
    /// tool will not ask for.
    pub address_space_kb: u64,
    /// Largest single file a job can create. The shell's block size is 512 or 1024 bytes, so
    /// the effective ceiling is this many KiB or half of it.
    pub file_size_kb: u64,
    /// Processes, against a fork bomb.
    pub processes: u32,
    /// Open file descriptors.
    pub open_files: u32,
    /// Bytes of stdout and of stderr kept. Reading continues past this, so the job never
    /// blocks on a full pipe.
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
    /// The `ulimit` prelude. Failures are swallowed, because an error line here lands in the
    /// grader's stderr and reads as the exercise's own output.
    ///
    /// `isolated` gates the process cap alone. `RLIMIT_NPROC` counts per user, so on the host
    /// it measures the whole session: set it low and nothing forks, set it high and a fork
    /// bomb has headroom. The cap needs a namespace to mean anything.
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
/// `view` is all the job reaches of the caller's data: children of `root`, named one at a
/// time, each read-only or writable. Anything not named is not there, so the gate can run a
/// reference solution without handing it the hidden tests.
#[derive(Debug)]
pub struct Job<'a> {
    /// Host directory holding the run. Never itself visible to the job.
    pub root: &'a Path,
    /// Children of `root` the job can see, and how.
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
    /// Read-only and outside `view`, so one exercise cannot alter what the next one imports.
    /// See [`crate::deps`].
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
        Self {
            root,
            view,
            cwd,
            script,
            timeout_secs,
            limits: Limits::default(),
            deps: None,
        }
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

    /// Check that every path the job needs exists and is what the caller said it was.
    ///
    /// Existence is checked here because `bwrap` reports a missing bind source as a mount
    /// failure, which reads as a broken sandbox.
    ///
    /// Containment takes three checks: the name must be plain, the entry must not be a
    /// symlink, and the resolved path must sit under the resolved root. An absolute name
    /// replaces the root, and a `..` climbs out of it. A symlink can stay inside the root and
    /// still alias another entry, which widens the access the caller asked for. Every caller
    /// passes a literal like `"work"`, so nothing legitimate is refused.
    fn check_view(&self) -> Result<(), String> {
        let root = self
            .root
            .canonicalize()
            .map_err(|e| format!("{}: not a usable run directory: {e}", self.root.display()))?;

        for (name, _) in self.view {
            let plain = Path::new(name)
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)));
            if !plain || name.is_empty() {
                return Err(format!(
                    "{name:?}: not a view entry name. An entry is one or more plain path \
                     components inside the run directory - not absolute, and never `..`."
                ));
            }

            let path = root.join(name);
            if !path.exists() {
                return Err(format!("{}: not in the run directory", path.display()));
            }

            // `symlink_metadata`, so the link itself is judged. A link inside the root passes
            // the containment check below and still makes the stricter access mode decorative.
            let meta = fs::symlink_metadata(&path)
                .map_err(|e| format!("{}: cannot be read: {e}", path.display()))?;
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "{}: is a symlink. A view entry is mounted, and a mounted link is \
                     followed - which would let one entry alias another and quietly widen \
                     the access the caller asked for. Pass the directory itself.",
                    path.display()
                ));
            }

            // A parent component can be a link even when the entry is not, and the resolved
            // path is the one the mount uses.
            let real = path
                .canonicalize()
                .map_err(|e| format!("{}: cannot be resolved: {e}", path.display()))?;
            if !real.starts_with(&root) {
                return Err(format!(
                    "{name}: resolves to {}, outside the run directory {}.",
                    real.display(),
                    root.display()
                ));
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
    /// Isolated: no network, no host filesystem beyond the read-only runtime, no access to
    /// the caller's state, and a process namespace that dies as one.
    Sandbox { bwrap: PathBuf, version: String },
    /// Isolated by a container engine, with the same absences and one presence: the runtime
    /// is the image's, pinned by digest, rather than the machine's `/usr`.
    ///
    /// This is what makes the tool work where there are no Linux namespaces to unshare.
    /// `name()` differs from the sandbox, so a verdict earned under one backend is refused
    /// under the other.
    Container {
        cli: PathBuf,
        engine: &'static str,
        version: String,
        image: Image,
    },
    /// Not isolated. The job runs as the user, with the user's rights, over the user's whole
    /// filesystem.
    UnsafeHost,
}

/// The runtime a container job gets, resolved to bytes.
///
/// `reference` is what the caller pinned and is evidence. `id` is what the engine resolved it
/// to and is identity, because one manifest-list digest names a different image per
/// architecture. Jobs launch by `id`, so a moved tag or index cannot change the runtime under
/// a run this code already inspected.
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
    /// Ordered rather than negotiated. Bubblewrap needs no daemon, no image and no pull, so
    /// it stays the default where it works.
    #[default]
    Auto,
    /// A container engine, refusing rather than falling back. A Linux user asks for it to
    /// gate against the runtime a mac uses, and the container tests need it because `Auto`
    /// picks bubblewrap here.
    Container,
    UnsafeHost,
}

impl Backend {
    /// Choose a backend.
    ///
    /// An absent sandbox is a refusal or a container, never a downgrade to the host. Claiming
    /// isolation while providing only a working directory stops the caller reading warnings.
    pub fn choose(want: Want, image: Option<&str>) -> Result<Self, String> {
        let image = image.unwrap_or(DEFAULT_IMAGE);
        match want {
            Want::UnsafeHost => Ok(Backend::UnsafeHost),
            Want::Container => detect_container(image),
            Want::Auto => match SANDBOX.clone() {
                Ok(backend) => Ok(backend),
                // Both refusals: on Linux the reader wants to know bubblewrap was looked
                // for, and on a mac the first line explains why a container is discussed.
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
    /// The container form names the engine, its version, the pinned reference and the
    /// resolved architecture, because a refusal has to be readable. The identity that decides
    /// staleness is [`Backend::image_id`]. The host form is `host`, because what the host
    /// provided is not enumerable, see `digest::exercise_digest`.
    pub fn profile(&self) -> String {
        match self {
            Backend::Sandbox { version, .. } => format!("bwrap {version}"),
            Backend::Container {
                engine,
                version,
                image,
                ..
            } => {
                format!("{engine} {version} {} ({})", image.reference, image.arch)
            }
            Backend::UnsafeHost => "host".to_string(),
        }
    }

    /// The exact runtime a verdict was earned against, when there is one.
    ///
    /// `None` where the runtime is the host's, because it is not enumerable. `Some` promises
    /// these bytes and this architecture, and a refusal when they move.
    pub fn image_id(&self) -> Option<&str> {
        match self {
            Backend::Container { image, .. } => Some(&image.id),
            _ => None,
        }
    }

    pub fn run(&self, job: &Job) -> Result<Outcome, String> {
        job.check_view()?;
        // `ulimit -u` means something only in a PID namespace. A container shares the host's
        // uid, so `RLIMIT_NPROC` counts the whole session and `--pids-limit` is the cap there.
        let namespaced = matches!(self, Backend::Sandbox { .. });
        let script = format!("{}{}", job.limits.prelude(namespaced), job.script);
        let (mut cmd, kill) = match self {
            Backend::Sandbox { bwrap, .. } => (sandbox_command(bwrap, job, &script)?, Kill::Group),
            Backend::Container { cli, image, .. } => {
                let name = container_name();
                let cmd = container_command(cli, image, job, &script, &name)?;
                (
                    cmd,
                    Kill::Container {
                        cli: cli.clone(),
                        name,
                    },
                )
            }
            Backend::UnsafeHost => (host_command(job, &script)?, Kill::Group),
        };
        cmd
            // Own process group, so the deadline kills everything the script started.
            // Backgrounded grandchildren otherwise hold the output pipes open, which turns a
            // missed timeout into a permanent hang. Under a container this kills the client,
            // so `Kill` also stops the container.
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_and_wait(cmd, job.timeout_secs, job.limits.output_bytes, kill)
    }
}

/// Detect the sandbox once per process.
///
/// The probe spawns a real `bwrap`, and every gate runs at least three jobs. The answer
/// cannot change during one command.
static SANDBOX: LazyLock<Result<Backend, String>> = LazyLock::new(detect_sandbox);

/// The refusal when `bwrap` is not on `$PATH`.
///
/// On Linux the binary is one package away. Elsewhere `bwrap` isolates with Linux namespaces
/// and there is nothing to install, so the routes are a container engine or a Linux host.
/// That message omits `--unsafe-host`, so the easiest path out of a refusal is not the one
/// that runs generated scripts as the user. The OS is a parameter so a test on any machine
/// can read the wording a mac user gets.
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
    let bwrap = which("bwrap").ok_or_else(|| no_sandbox_message(std::env::consts::OS))?;

    let version = Command::new(&bwrap)
        .arg("--version")
        .output()
        .map_err(|e| format!("no sandbox available: {} --version: {e}", bwrap.display()))
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;
    let version = version
        .strip_prefix("bubblewrap ")
        .unwrap_or(&version)
        .to_string();

    // Presence is not capability. Unprivileged user namespaces are disabled on some kernels
    // and restricted by LSM policy on others, and the failure surfaces on the first mount.
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

/// How much of a control command's output to keep.
///
/// These commands print an id, a version, or one line of refusal. The cap stops a `pull`
/// progress stream from being held in full, and `drain` discards past it rather than
/// blocking.
const CONTROL_CAP: usize = 64 * 1024;

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

/// Container engines this can drive, in the order they are tried.
///
/// Both take the same arguments for everything used here. Docker first, because a machine
/// with both is usually one where docker runs.
const ENGINES: [&str; 2] = ["docker", "podman"];

/// The runtime a container job gets unless the caller names another one.
///
/// Pinned to a manifest-list digest, which is one identity across every architecture in it.
/// A tag alone makes the runner image mean whatever was pushed last.
///
/// Python, because the graders are shell and Python. Slim is Debian, not Alpine: musl has no
/// manylinux wheels, so an Alpine runner turns every warmed dependency into a source build
/// that cannot happen offline. The image has no `sqlite3`, and `--image` exists for that.
pub const DEFAULT_IMAGE: &str =
    "python:3.13-slim@sha256:ffb752e139c0a19692a43af8d8523b274222dd68eebad5d583b45c2201c6e30a";

/// Label carried by every container this process starts, valued with its pid.
///
/// The container backend has no `--die-with-parent`. If this process is killed outright, the
/// next detection kills the containers whose owner pid is gone.
pub(crate) const OWNER_LABEL: &str = "benkyou.owner";

/// A control command's ceiling, in seconds.
///
/// This bounds the engine client, not the job. A dead context does not fail, it waits: a
/// socket that never answers, a starting VM, a context pointing at a host that is gone.
/// `benkyou runner --pull` hung with no bound on a reviewer's machine.
pub(crate) const CONTROL_SECS: u64 = 20;

/// A pull's ceiling. Longer by two orders of magnitude, because a first pull is hundreds of
/// megabytes over a home connection.
pub(crate) const PULL_SECS: u64 = 900;

/// Run an engine control command with a deadline, and never inherit a terminal.
///
/// `Command::output` waits forever. A bounded wait without draining is worse: `pull` prints
/// a progress line per layer per second. Once a pipe fills, the client blocks on write and
/// the deadline reports a timeout on a command that was making progress. So both pipes drain
/// on their own threads and the cap discards.
pub(crate) fn engine_output(cli: &Path, args: &[&str], secs: u64) -> Result<Control, String> {
    let mut child = Command::new(cli)
        .args(args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", cli.display()))?;

    let pgid = child.id();
    let out_rx = drain(child.stdout.take().expect("piped"), CONTROL_CAP);
    let err_rx = drain(child.stderr.take().expect("piped"), CONTROL_CAP);

    let started = Instant::now();
    let mut timed_out = false;
    loop {
        match child
            .try_wait()
            .map_err(|e| format!("{}: {e}", cli.display()))?
        {
            Some(_) => break,
            None if started.elapsed() >= Duration::from_secs(secs) => {
                kill_group(pgid);
                let _ = child.wait();
                timed_out = true;
                break;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }

    let mut release = || kill_group(pgid);
    let (stdout, _) = collect(&out_rx, &mut release);
    let (stderr, _) = collect(&err_rx, &mut release);

    if timed_out {
        return Err(format!(
            "{} {}: no answer in {secs}s. The engine is installed but not answering - \
             check that its daemon or machine is running, and that the context this shell \
             points at exists.",
            cli.display(),
            args.first().copied().unwrap_or_default()
        ));
    }
    Ok(Control {
        ok: child.wait().map(|s| s.success()).unwrap_or(false),
        stdout,
        stderr,
    })
}

/// The result of a control command: whether it worked, and what it said.
pub(crate) struct Control {
    pub(crate) ok: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Control {
    /// The first non-empty line of `stderr`, where every engine puts the sentence a human
    /// needs.
    fn why(&self) -> String {
        self.stderr
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output")
            .trim()
            .to_string()
    }
}

/// Find an engine that works, or say what is wrong with each one that does not.
///
/// Presence on `$PATH` is not the question. A mac with a Docker CLI and no Docker Desktop
/// has the binary and no daemon. Stopping there never tries a working podman beside it.
/// Every failure is reported, because either half of "docker is down, podman is absent"
/// misleads alone.
fn find_engine() -> Result<(&'static str, PathBuf), String> {
    let mut why: Vec<String> = Vec::new();
    for name in ENGINES {
        let Some(path) = which(name) else {
            why.push(format!("{name}: not on PATH"));
            continue;
        };
        match engine_usable(&path) {
            Ok(()) => return Ok((name, path)),
            Err(e) => why.push(format!("{name}: {e}")),
        }
    }
    Err(format!(
        "no usable container engine:\n  {}\n  Install docker or podman and start it, or \
         pass --unsafe-host to run generated scripts with your own user's rights.",
        why.join("\n  ")
    ))
}

/// Whether an engine can do anything, asked with `info`.
///
/// Not `--version`, which the client answers alone and which passes with no daemon at all.
/// Not `version --format {{.Server.Version}}` either: podman is daemonless, its `.Server`
/// field describes a remote podman, and a working local podman reports nothing there. `info`
/// fails when the engine cannot act.
fn engine_usable(cli: &Path) -> Result<(), String> {
    let out = engine_output(cli, &["info"], CONTROL_SECS)?;
    if !out.ok {
        return Err(out.why());
    }
    Ok(())
}

/// The version string a verdict records: the client's, from `--version`.
///
/// The client is the half always there to ask. This string is evidence in a profile, not the
/// identity of anything. The identity is the image digest.
fn client_version(cli: &Path) -> Result<String, String> {
    let out = engine_output(cli, &["--version"], CONTROL_SECS)?;
    if !out.ok {
        return Err(out.why());
    }
    Ok(engine_version(&out.stdout))
}

/// What `benkyou runner` reports: the engine, and whether the runtime is here yet.
///
/// An absent `id` is the normal state on a fresh machine, and not an error here. This
/// command exists to tell a reader what to do about it.
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
/// The fetch lives here and in `warm` only. Everything on the gating path resolves what is
/// local, or refuses.
///
/// `image: None` means the engine works and answered that this reference is not here. Every
/// other failure is an `Err`. An earlier `inspect_image(..).ok()` let a test harness read a
/// dead daemon as "not pulled yet" and skip nineteen assertions.
pub fn runner_status(reference: Option<&str>, pull: bool) -> Result<RunnerStatus, String> {
    let reference = reference.unwrap_or(DEFAULT_IMAGE).to_string();
    let (engine, cli) = find_engine()?;
    let version = client_version(&cli)?;

    if pull {
        let out = engine_output(&cli, &["pull", &reference], PULL_SECS)?;
        if !out.ok {
            return Err(format!("{engine} pull {reference}: {}", out.why()));
        }
    }

    Ok(RunnerStatus {
        engine,
        version,
        image: inspect_image(&cli, engine, &reference)?,
        reference,
        pulled: pull,
    })
}

fn detect_container(reference: &str) -> Result<Backend, String> {
    let (engine, cli) = find_engine()?;
    let version = client_version(&cli)?;
    let image = inspect_image(&cli, engine, reference)?.ok_or_else(|| absent(reference))?;

    // Presence is not capability. A daemon can be installed and not running. An engine can
    // reject a policy flag, an SELinux host can deny the bind mounts, a cgroup can lack
    // memory accounting. All of them otherwise fail in the middle of somebody's gate.
    let backend = Backend::Container {
        cli,
        engine,
        version,
        image,
    };
    probe_container(&backend)?;

    if let Backend::Container { cli, .. } = &backend {
        reap_orphans(cli);
    }
    Ok(backend)
}

/// What to say when the runtime is not on the machine yet.
///
/// Its own function because two callers need the identical sentence, and `benkyou runner`
/// reports the fact without refusing.
fn absent(reference: &str) -> String {
    format!(
        "runner image not present: {reference}\n  run `benkyou runner --pull` once, on \
         purpose: gate, attempt, grade and serve never reach a network."
    )
}

/// `docker version 29.7.2, build …` becomes `29.7.2`. This field gets compared, and a build
/// hash moves with every point release, so keeping it fires the drift warning on any upgrade.
fn engine_version(line: &str) -> String {
    line.split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or("unknown")
        .trim_end_matches(',')
        .to_string()
}

/// Resolve a reference to the bytes behind it, locally and without a network.
///
/// A pull here puts a network on the gating path and makes the runtime one nobody chose.
///
/// Three outcomes. `Ok(Some)` is the image. `Ok(None)` means the engine answered that this
/// reference is not here, so a pull fixes it. `Err` is every other failure: a hung socket, a
/// vanished context, a permission change, output this code cannot parse. Absence must be
/// recognized, never inferred from a failure, or a dead daemon reads as an unpulled image.
fn inspect_image(cli: &Path, engine: &str, reference: &str) -> Result<Option<Image>, String> {
    let out = engine_output(
        cli,
        &[
            "image",
            "inspect",
            "--format",
            "{{.Id}} {{.Architecture}}",
            reference,
        ],
        CONTROL_SECS,
    )?;
    if !out.ok {
        let why = out.why();
        if absent_image(&why) {
            return Ok(None);
        }
        return Err(format!(
            "{engine}: could not resolve {reference}: {why}\n  This is not a missing \
             image - the engine failed to answer for one. Check that it is running and \
             that this shell's context points at it."
        ));
    }
    let line = out.stdout.trim().to_string();
    let (id, arch) = line
        .split_once(' ')
        .ok_or_else(|| format!("{engine}: could not read the id of {reference}: {line:?}"))?;
    if !id.starts_with("sha256:") {
        return Err(format!(
            "{engine}: {reference} reported no digest id ({id:?})"
        ));
    }
    Ok(Some(Image {
        reference: reference.to_string(),
        id: id.to_string(),
        arch: arch.trim().to_string(),
    }))
}

/// Whether an engine's failure means the image is not here, and nothing worse.
///
/// A closed set of sentences, matched case-insensitively, because the two mistakes cost
/// differently. An unknown failure read as absence sends a reader to `--pull` for a dead
/// daemon. Absence read as a failure costs one confusing sentence.
///
/// Docker says `No such image: <ref>`, and older API versions say `No such object`. podman
/// says `no such image` from libpod and `<ref>: image not known` from its storage layer.
fn absent_image(why: &str) -> bool {
    const ABSENT: [&str; 5] = [
        "no such image",
        "no such object",
        "image not known",
        "image not found",
        "unable to find image",
    ];
    let lower = why.to_ascii_lowercase();
    ABSENT.iter().any(|phrase| lower.contains(phrase))
}

/// Run one job under the real policy, through the real path, with a real bind mount.
///
/// It goes through [`Backend::run`], so the prelude, deadline and mounts are the ones a gate
/// gets. A cheaper probe passes on a machine where the first exercise fails.
fn probe_container(backend: &Backend) -> Result<(), String> {
    // Named per job, not per process: threads share a pid, so a pid-keyed directory let two
    // concurrent detections delete each other's witness file.
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
            if out.stderr.trim().is_empty() {
                out.stdout.trim()
            } else {
                out.stderr.trim()
            }
        )),
        Err(e) => Err(format!("{}: {e}", backend.profile())),
    }
}

/// Kill containers left behind by a benkyou that is no longer running.
///
/// Best effort: a failure to tidy must never stop a gate. Liveness is asked of the shell,
/// because `kill -0` is a builtin everywhere `/bin/sh` is.
fn reap_orphans(cli: &Path) {
    // Bounded like every other engine call. This runs on the way into somebody's gate, and
    // an unbounded `ps` against a half-answering daemon hangs there.
    let Ok(out) = engine_output(
        cli,
        &[
            "ps",
            "--filter",
            &format!("label={OWNER_LABEL}"),
            "--format",
            &format!("{{{{.ID}}}} {{{{.Label \"{OWNER_LABEL}\"}}}}"),
        ],
        CONTROL_SECS,
    ) else {
        return;
    };
    for line in out.stdout.lines() {
        let Some((id, owner)) = line
            .split_whitespace()
            .next()
            .zip(line.split_whitespace().nth(1))
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
            let _ = engine_output(cli, &["rm", "--force", id], CONTROL_SECS);
        }
    }
}

/// A name per job, so the deadline has something to kill.
///
/// The pid alone is not enough: a gate runs at least three jobs, so a shared name makes a
/// kill hit whichever container holds it.
pub(crate) fn container_name() -> String {
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
/// Split out so the capability probe runs under the policy a real job gets.
fn base_args() -> Result<Vec<String>, String> {
    let (passwd, group) = identity_files()?;
    let home = format!("{GUEST_ROOT}/{HOME_DIR}");
    let mut a: Vec<String> = Vec::new();

    let fixed: &[&str] = &[
        // Named one at a time rather than --unshare-all, which is `-try` for the user
        // namespace: on a kernel without one it continues in the host's and reports success.
        "--unshare-user",
        "--unshare-ipc",
        "--unshare-pid",
        "--unshare-net",
        "--unshare-uts",
        "--unshare-cgroup-try",
        // Kills the container if this process dies, however it dies.
        "--die-with-parent",
        // No controlling terminal: a job must not push characters into the user's terminal.
        "--new-session",
        "--clearenv",
        // Parents made explicitly, so their modes and the mount order are fixed. bwrap
        // creates a missing parent with whatever the default mode is.
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
    // Pushed rather than listed above, so the ceiling is written once. `--size` applies to
    // the mount that follows it.
    a.extend([
        "--size".to_string(),
        TMP_BYTES.to_string(),
        "--perms".to_string(),
        "1777".to_string(),
        "--tmpfs".to_string(),
        "/tmp".to_string(),
    ]);
    for path in HOST_RO {
        a.extend([
            "--ro-bind-try".to_string(),
            path.to_string(),
            path.to_string(),
        ]);
    }
    // A synthetic passwd: a failing `getpwuid` breaks Python's `expanduser`, and the host's
    // file lists every account on the machine to the job.
    for (src, dst) in [(passwd, "/etc/passwd"), (group, "/etc/group")] {
        a.extend([
            "--ro-bind".to_string(),
            src.display().to_string(),
            dst.to_string(),
        ]);
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
/// One directory per user, shared by every run. A per-pid directory leaks, because nothing
/// drops a `LazyLock` and a kata killed by its deadline runs no exit hook. Sharing is safe,
/// because the contents depend only on uid and gid.
static IDENTITY: LazyLock<Result<(PathBuf, PathBuf), String>> = LazyLock::new(|| {
    let (uid, gid) = uid_gid()?;
    let dir = identity_dir(&std::env::temp_dir(), uid)?;
    write_identity(&dir, uid, gid)
});

/// This process's effective uid and gid, read once.
///
/// Read from a file this process created, because macOS has no `/proc/self` and the
/// container backend must work there. A new file is owned by the effective uid and gid. An
/// `id -u` is one more binary to find on `PATH` before anything runs.
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
/// The directory being ours does not make every name inside it ours. These two files decide
/// what the sandbox resolves for every uid and gid.
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
    // Staged under a per-pid name and renamed, so no run reads a half-written file.
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
/// `fs::write` follows symlinks, so a link at this name sends the write to its target.
/// `create_new` is `O_EXCL`, and unlinking removes the link and never its target. An
/// existing file is debris from a crashed run whose pid was reused, so it is unlinked once
/// and the write retried.
fn write_new(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let attempt = || -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
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
/// These files become `/etc/passwd` and `/etc/group`, so whoever owns the directory decides
/// what a run resolves for every name lookup. A symlink is refused, never followed.
fn identity_dir(base: &Path, uid: u32) -> Result<PathBuf, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let dir = base.join(format!("benkyou-box-{uid}"));
    // `create_dir_all` accepts an existing symlink to a directory, so the check below is
    // what decides.
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let md = fs::symlink_metadata(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if md.file_type().is_symlink() || !md.is_dir() {
        return Err(format!("{}: not a directory", dir.display()));
    }
    if md.uid() != uid {
        return Err(format!(
            "{}: owned by uid {}, not {uid}",
            dir.display(),
            md.uid()
        ));
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
    // Read-only, and outside `view` so no caller can hand it out writable. `PYTHONPATH` is
    // set here, not inherited: it is a property of the exercise, not of the caller's shell.
    if let Some(set) = job.deps {
        cmd.args([
            "--ro-bind",
            &set.display().to_string(),
            crate::deps::GUEST_DEPS,
        ]);
        cmd.args(["--setenv", "PYTHONPATH", crate::deps::GUEST_DEPS]);
    }
    cmd.args(["--chdir", &job.guest_cwd(), "/bin/sh", "-c", script]);
    // bwrap itself inherits nothing. `--clearenv` governs the child.
    cmd.env_clear();
    Ok(cmd)
}

fn host_command(job: &Job, script: &str) -> Result<Command, String> {
    // The one thing this backend still honours: keep a grader that writes to $HOME out of
    // the real one. It is not containment.
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
    // read-only: this backend cannot make it so.
    if let Some(set) = job.deps {
        cmd.env("PYTHONPATH", set);
    }
    Ok(cmd)
}

/// The container policy, with no job-specific mounts.
///
/// Each flag asks the engine for one guarantee the sandbox gets from a namespace:
///
/// - `--network none`: no route out, as `--unshare-net` gives.
/// - `--read-only`: nothing a job writes to the image survives the job.
/// - `--cap-drop ALL` and `no-new-privileges`: no capabilities, and none to acquire.
/// - `--pids-limit`: the process cap. `ulimit -u` is useless here, because a container
///   shares the host's uid and `RLIMIT_NPROC` counts the whole session.
/// - `--memory` and `--memory-swap` equal: a memory ceiling swapping cannot dodge. The
///   prelude's `ulimit -v` still bounds the address space a process asks for.
/// - `--user`: the caller's uid, so files a job writes into its workspace belong to the
///   caller. Without it the writable view is unusable.
/// - The three tmpfs mounts are the writable surfaces: `/box`, `$HOME` under it, and a
///   bounded `/tmp`. The caller's uid owns each, because a root-owned tmpfs under `--user`
///   is a directory the job cannot write.
/// - The synthetic `/etc/passwd` and `/etc/group`: `--user 1000` names a uid the image does
///   not know, and a failing `getpwuid` breaks Python's `expanduser`.
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
    a.extend([
        "--memory".to_string(),
        format!("{}k", limits.address_space_kb),
    ]);
    a.extend([
        "--memory-swap".to_string(),
        format!("{}k", limits.address_space_kb),
    ]);
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

/// A host path as a `--mount` field value, or a refusal saying why it cannot be one.
///
/// `--mount` takes comma-separated `key=value` pairs and has no escape, so a path with a
/// comma is split mid-path. Quoting does not help, because docker refuses a quote that opens
/// mid-field. `-v` is no fix, because its separator is a colon. Control characters have no
/// escape on either side of this interface.
pub(crate) fn mount_source(path: &Path) -> Result<String, String> {
    let text = path.display().to_string();
    if let Some(bad) = text.chars().find(|c| *c == ',' || c.is_control()) {
        return Err(format!(
            "{text}: a container mount cannot express this path. {bad:?} has no escape in \
             `--mount` syntax.\n  Move the run directory with `--scratch`, or the \
             dependency cache with `XDG_CACHE_HOME`, or run this under the sandbox."
        ));
    }
    Ok(text)
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
            mount_source(&job.root.join(entry))?
        );
        if *access == Access::Read {
            mount.push_str(",readonly");
        }
        cmd.args(["--mount".to_string(), mount]);
    }
    // Read-only, and outside `view` so no caller can hand it out writable. `PYTHONPATH` is
    // set here, not inherited: it is a property of the exercise, not of the caller's shell.
    if let Some(set) = job.deps {
        cmd.args([
            "--mount".to_string(),
            format!(
                "type=bind,source={},target={},readonly",
                mount_source(set)?,
                crate::deps::GUEST_DEPS
            ),
        ]);
        cmd.args([
            "--env".to_string(),
            format!("PYTHONPATH={}", crate::deps::GUEST_DEPS),
        ]);
    }
    cmd.args(["--workdir", &job.guest_cwd()]);
    // `/bin/sh` rather than the image's entrypoint. An image need not have one, and an
    // entrypoint that wraps the script runs something this tool never wrote.
    cmd.args(["--entrypoint", "/bin/sh"]);
    // By id, not reference: a tag or index that moved since detection puts a different
    // runtime under a verdict that names the old one.
    cmd.args([&image.id, "-c", script]);
    // The client keeps the caller's environment, and none of it reaches the job, because an
    // engine forwards only what `--env` names. `DOCKER_HOST`, `CONTAINER_HOST` and a
    // rootless `XDG_RUNTIME_DIR` let the client find its daemon, so clearing them breaks
    // rootless podman.
    //
    // The image's own `ENV` escapes the allowlist, and no engine flag unsets it. The
    // difference from the sandbox is bounded, because a verdict records the image.
    Ok(cmd)
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// How a run is stopped when the deadline fires, or when output is still in flight.
///
/// A process group covers a child process tree. A container's daemon keeps running it when
/// the client dies, so the name is killed first and the group second.
enum Kill {
    Group,
    Container { cli: PathBuf, name: String },
}

impl Kill {
    fn fire(&self, pgid: u32) {
        if let Kill::Container { cli, name } = self {
            // Bounded: an unbounded `docker kill` against a wedged daemon hangs the deadline
            // path itself. A failure to kill reaches the caller through `timed_out`.
            let _ = engine_output(cli, &["kill", name], CONTROL_SECS);
        }
        kill_group(pgid);
    }

    /// Remove the container name after every run, deadline or no deadline.
    ///
    /// A client that dies of its own accord leaves the daemon running the container against
    /// the workspace. The orphan sweep cannot help, because this owner pid is still alive.
    fn sweep(&self) {
        if let Kill::Container { cli, name } = self {
            let _ = engine_output(cli, &["rm", "--force", name], CONTROL_SECS);
        }
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

    // Drain both pipes on their own threads. A child blocked on a full pipe is never reaped,
    // and the deadline then fires on a process that was making progress.
    let out_rx = drain(child.stdout.take().expect("piped"), cap);
    let err_rx = drain(child.stderr.take().expect("piped"), cap);

    let deadline = Duration::from_secs(timeout_secs as u64);
    let mut timed_out = false;
    // The wait result is held, not propagated. An early `?` skips the sweep below and leaves
    // the daemon running a container nobody watches.
    let waited = loop {
        match child.try_wait() {
            Err(e) => break Err(e.to_string()),
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() >= deadline => {
                kill.fire(pgid);
                timed_out = true;
                break child.wait().map_err(|e| e.to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    };

    // An exited command can still leave a daemonised grandchild holding the write end.
    // Collect what arrived, kill the group to release the rest, and move on.
    let (stdout, out_cut) = collect(&out_rx, &mut || kill.fire(pgid));
    let (stderr, err_cut) = collect(&err_rx, &mut || kill.fire(pgid));

    // After the output, so a container is not removed from under its own diagnostics. Before
    // the verdict, so it runs on success, timeout and wait failure alike.
    kill.sweep();

    let status = waited?;
    Ok(Outcome {
        // A killed child reports no code. `timed_out` tells the two apart.
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
/// Reading continues past the cap. A job that stops being read blocks on a full pipe and
/// dies to the deadline, which reports an output bomb as a hang.
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

/// Wait for a drained pipe for a bounded grace period. On expiry run `release`, which kills
/// whatever still holds the pipe, then try once more.
fn collect(rx: &mpsc::Receiver<(Vec<u8>, bool)>, release: &mut dyn FnMut()) -> (String, bool) {
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

/// Kill a whole process group. Routed through `sh` because its built-in `kill` exists
/// anywhere `/bin/sh` is, with no libc dependency and no `kill` binary on PATH.
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

/// The wording of a refusal is all a caller on the wrong platform gets. The case that
/// matters is the one this machine cannot reach.
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

    /// On a mac there is nothing to install, so advice to install sends the reader to a
    /// Homebrew formula for a Linux namespace API. The refusal names a container engine and
    /// never `--unsafe-host`.
    #[test]
    fn a_mac_is_pointed_at_a_container() {
        let msg = no_sandbox_message("macos");
        assert!(
            msg.contains("macos"),
            "the refusal must name the platform: {msg}"
        );
        assert!(
            !msg.contains("Install it"),
            "nothing to install on a mac: {msg}"
        );
        assert!(msg.contains("Linux namespaces"), "{msg}");
        assert!(
            msg.contains("Linux host"),
            "the other route must stay named: {msg}"
        );
        assert!(
            !msg.contains("--unsafe-host"),
            "must not offer the host backend: {msg}"
        );
        for wanted in ["docker", "podman", "benkyou runner --pull"] {
            assert!(msg.contains(wanted), "refusal did not name {wanted}: {msg}");
        }
    }

    /// Only a recognized not-found means the image is absent. Every other failure is an
    /// error. Reading a dead daemon as absence made the container tests report 19 passes.
    #[test]
    fn only_a_named_not_found_counts_as_an_absent_image() {
        for absent in [
            "Error response from daemon: No such image: python:3.13-slim",
            "Error: No such object: alpine:latest",
            "Error: no such image alpine:latest",
            "Error: alpine:latest: image not known",
            "Unable to find image 'alpine:latest' locally",
        ] {
            assert!(super::absent_image(absent), "not read as absent: {absent}");
        }
        for broken in [
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock.",
            "permission denied while trying to connect to the Docker daemon socket",
            "Error: unable to connect to Podman socket",
            "context deadline exceeded",
            "invalid reference format",
        ] {
            assert!(
                !super::absent_image(broken),
                "a broken engine read as absent: {broken}"
            );
        }
    }

    /// A container mount cannot carry a comma. The refusal names both movable paths.
    #[test]
    fn a_comma_in_a_path_is_refused_with_a_way_out() {
        let err = super::mount_source(std::path::Path::new("/tmp/a,b/work")).expect_err("refused");
        assert!(err.contains("/tmp/a,b/work"), "{err}");
        assert!(err.contains("--scratch"), "{err}");
        assert!(err.contains("XDG_CACHE_HOME"), "{err}");
        let ok = super::mount_source(std::path::Path::new("/tmp/plain/work")).expect("accepted");
        assert_eq!(ok, "/tmp/plain/work");
    }
}

#[cfg(test)]
mod view_tests {
    use super::{Access, Job};
    use std::fs;

    fn root(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bk-view-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("work")).expect("work");
        d.canonicalize().expect("canonicalize")
    }

    #[test]
    fn a_plain_entry_that_exists_is_accepted() {
        let dir = root("plain");
        let job = Job::new(&dir, &[("work", Access::Write)], "work", ":", 10);
        assert!(job.check_view().is_ok(), "a normal view was refused");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absolute_entry_is_refused() {
        let dir = root("abs");
        let job = Job::new(&dir, &[("/etc", Access::Read)], "", ":", 10);
        let err = job
            .check_view()
            .expect_err("an absolute entry must be refused");
        assert!(err.contains("plain path"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_climbing_entry_is_refused() {
        let dir = root("climb");
        let job = Job::new(&dir, &[("../..", Access::Read)], "", ":", 10);
        assert!(job.check_view().is_err(), "`..` must be refused");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A link is refused rather than followed, even when it lands inside the run directory.
    /// Two entries resolving to one directory widen the access the caller asked for.
    #[test]
    fn a_symlinked_entry_is_refused_even_when_it_lands_inside() {
        let dir = root("link");
        fs::create_dir_all(dir.join("check")).expect("check");
        std::os::unix::fs::symlink(dir.join("check"), dir.join("alias")).expect("symlink");
        let job = Job::new(&dir, &[("alias", Access::Write)], "", ":", 10);
        let err = job
            .check_view()
            .expect_err("a symlinked entry must be refused");
        assert!(err.contains("symlink"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_out_of_the_run_directory_is_refused() {
        let dir = root("escape");
        std::os::unix::fs::symlink("/etc", dir.join("out")).expect("symlink");
        let job = Job::new(&dir, &[("out", Access::Read)], "", ":", 10);
        assert!(job.check_view().is_err(), "a link to /etc must be refused");
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod identity_tests {
    use super::{identity_dir, uid_gid, write_identity};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bk-ident-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Repeated calls must converge on one directory. The per-pid version left one behind
    /// per invocation.
    #[test]
    fn repeated_calls_reuse_one_directory() {
        let base = scratch("reuse");
        let uid = uid_gid().expect("this process has a uid").0;
        let first = identity_dir(&base, uid).expect("first call");
        fs::write(first.join("passwd"), "x").unwrap();
        for _ in 0..5 {
            assert_eq!(identity_dir(&base, uid).expect("later call"), first);
        }
        let dirs: Vec<_> = fs::read_dir(&base)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(dirs.len(), 1, "left behind: {dirs:?}");
        // Adopting is not erasing: a run must not lose the files a sibling wrote.
        assert_eq!(fs::read_to_string(first.join("passwd")).unwrap(), "x");
        let _ = fs::remove_dir_all(&base);
    }

    /// These files become `/etc/passwd` and `/etc/group`, so a symlink at the path must be
    /// refused, never followed.
    #[test]
    fn a_symlink_is_refused() {
        let base = scratch("symlink");
        let uid = uid_gid().expect("this process has a uid").0;
        let target = base.join("elsewhere");
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, base.join(format!("benkyou-box-{uid}"))).unwrap();
        let err = identity_dir(&base, uid).expect_err("a symlink must not be adopted");
        assert!(err.contains("not a directory"), "{err}");
        let _ = fs::remove_dir_all(&base);
    }

    /// `fs::write` follows a symlink and truncates its target, so a link at the staging name
    /// redirects the write out of the directory. The link must go and its target must stay.
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
        assert!(fs::read_to_string(&passwd)
            .unwrap()
            .contains("box:x:1000:1000"));
        assert!(fs::read_to_string(&group).unwrap().contains("box:x:1000:"));
        assert!(!fs::symlink_metadata(&passwd)
            .unwrap()
            .file_type()
            .is_symlink());
        let _ = fs::remove_dir_all(&base);
    }

    /// Debris from a crashed run whose pid was reused is expected. The next run must clear
    /// it rather than refuse to start.
    #[test]
    fn a_stale_staging_file_does_not_wedge_the_next_run() {
        let base = scratch("stale");
        let dir = base.join("box");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("passwd.{}", std::process::id())), "junk").unwrap();

        let (passwd, _) = write_identity(&dir, 1000, 1000).expect("stale debris must clear");

        assert!(fs::read_to_string(&passwd)
            .unwrap()
            .contains("box:x:1000:1000"));
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains('.'))
            .collect();
        assert!(left.is_empty(), "staging files left behind: {left:?}");
        let _ = fs::remove_dir_all(&base);
    }

    /// A world-readable directory lets another account see and race the identity a run is
    /// about to mount.
    #[test]
    fn the_directory_is_private() {
        let base = scratch("mode");
        let uid = uid_gid().expect("this process has a uid").0;
        let dir = base.join(format!("benkyou-box-{uid}"));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        let got = identity_dir(&base, uid).expect("an owned directory is adopted");
        let mode = fs::metadata(&got).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "left world-readable: {mode:o}");
        let _ = fs::remove_dir_all(&base);
    }
}
