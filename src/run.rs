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
}

impl<'a> Job<'a> {
    pub fn new(
        root: &'a Path,
        view: &'a [(&'a str, Access)],
        cwd: &'a str,
        script: &'a str,
        timeout_secs: u32,
    ) -> Self {
        Self { root, view, cwd, script, timeout_secs, limits: Limits::default() }
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
    /// Not isolated. The job runs as the user, with the user's rights, over the
    /// user's whole filesystem. The name is the documentation.
    UnsafeHost,
}

impl Backend {
    /// Choose a backend.
    ///
    /// The default is the sandbox, and the absence of one is a refusal rather than a
    /// downgrade. Claiming isolation while providing a working directory would be
    /// worse than not having it: the caller would stop reading the warnings.
    pub fn select(unsafe_host: bool) -> Result<Self, String> {
        if unsafe_host {
            return Ok(Backend::UnsafeHost);
        }
        SANDBOX.clone()
    }

    /// Stable name for the record, and for humans reading a refusal.
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Sandbox { .. } => "sandbox",
            Backend::UnsafeHost => "unsafe-host",
        }
    }

    /// Execution profile: what a verdict earned under this backend was earned under.
    ///
    /// For the sandbox this names the isolation tool and its version, which is a real
    /// property of the run. For the host backend it is deliberately just `host`:
    /// enumerating what the host provided is exactly the thing that cannot be done
    /// (see `digest::exercise_digest`), and a longer string would imply otherwise.
    pub fn profile(&self) -> String {
        match self {
            Backend::Sandbox { version, .. } => format!("bwrap {version}"),
            Backend::UnsafeHost => "host".to_string(),
        }
    }

    pub fn run(&self, job: &Job) -> Result<Outcome, String> {
        job.check_view()?;
        let isolated = matches!(self, Backend::Sandbox { .. });
        let script = format!("{}{}", job.limits.prelude(isolated), job.script);
        let mut cmd = match self {
            Backend::Sandbox { bwrap, .. } => sandbox_command(bwrap, job, &script)?,
            Backend::UnsafeHost => host_command(job, &script)?,
        };
        cmd
            // Own process group, so the deadline can kill everything the script
            // started. Killing only the shell leaves backgrounded grandchildren
            // running - and holding the output pipes open, which is what turns a
            // missed timeout into a permanent hang. Under the sandbox this is belt to
            // the PID namespace's braces; on the host it is the only mechanism there
            // is.
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_and_wait(cmd, job.timeout_secs, job.limits.output_bytes)
    }
}

/// Detect the sandbox once per process.
///
/// Cached because every gate runs at least three jobs and the probe spawns a real
/// `bwrap`; and because the answer cannot change underneath a single command.
static SANDBOX: LazyLock<Result<Backend, String>> = LazyLock::new(detect_sandbox);

fn detect_sandbox() -> Result<Backend, String> {
    let bwrap = which("bwrap").ok_or_else(|| {
        "no sandbox available: `bwrap` (bubblewrap) is not on PATH. Install it, or pass \
         --unsafe-host to run generated scripts with your own user's rights."
            .to_string()
    })?;

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
        // A bounded tmpfs, so filling /tmp fills 256 MiB of memory and not a disk.
        "--size",
        "268435456",
        "--perms",
        "1777",
        "--tmpfs",
        "/tmp",
        "--ro-bind",
        "/usr",
        "/usr",
    ];
    a.extend(fixed.iter().map(|s| s.to_string()));
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

/// A one-line `/etc/passwd` and `/etc/group`, written once per process.
static IDENTITY: LazyLock<Result<(PathBuf, PathBuf), String>> = LazyLock::new(|| {
    use std::os::unix::fs::MetadataExt;
    let me = fs::metadata("/proc/self").map_err(|e| format!("/proc/self: {e}"))?;
    let (uid, gid) = (me.uid(), me.gid());
    let dir = std::env::temp_dir().join(format!("benkyou-box-{}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let passwd = dir.join("passwd");
    let group = dir.join("group");
    fs::write(
        &passwd,
        format!(
            "root:x:0:0:root:/:/bin/sh\n\
             box:x:{uid}:{gid}:box:{GUEST_ROOT}/{HOME_DIR}:/bin/sh\n"
        ),
    )
    .map_err(|e| format!("{}: {e}", passwd.display()))?;
    fs::write(&group, format!("root:x:0:\nbox:x:{gid}:\n"))
        .map_err(|e| format!("{}: {e}", group.display()))?;
    Ok((passwd, group))
});

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
    Ok(cmd)
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

fn spawn_and_wait(mut cmd: Command, timeout_secs: u32, cap: usize) -> Result<Outcome, String> {
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
                kill_group(pgid);
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
    let (stdout, out_cut) = collect(&out_rx, &mut || kill_group(pgid));
    let (stderr, err_cut) = collect(&err_rx, &mut || kill_group(pgid));

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
