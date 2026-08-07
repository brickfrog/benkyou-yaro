//! Running a graded command.
//!
//! This is deliberately not a sandbox. The code being run is the learner's own
//! solution to their own kata, graded on their own machine, so there is no adversary
//! to isolate from — and buying isolation would cost a hard dependency on Linux
//! namespace tooling for no threat it defends against.
//!
//! What is kept is a wall-clock deadline, because an infinite loop in your own draft
//! solution is an ordinary mistake and the gate has to report it as one rather than
//! hanging. See DESIGN.md §3.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Grace period for output already in flight once the command is done or killed.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Runner {
    /// Working directory for the command.
    pub dir: PathBuf,
    /// Wall-clock deadline. On expiry the process group is killed and `timed_out` set.
    pub timeout_secs: u32,
}

impl Runner {
    pub fn in_dir(dir: impl Into<PathBuf>, timeout_secs: u32) -> Self {
        Self { dir: dir.into(), timeout_secs }
    }

    /// Run `script` under `/bin/sh` in the run directory.
    ///
    /// `HOME` is pointed at the run directory. That is not containment — the script
    /// can still write anywhere the user can — it just keeps a grader that scribbles
    /// into `$HOME` from scribbling into the real one.
    pub fn run(&self, script: &str) -> Result<Outcome, String> {
        let started = Instant::now();
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            // Own process group, so the deadline can kill everything the script
            // started. Killing only the shell leaves backgrounded grandchildren
            // running — and holding the output pipes open, which is what turns a
            // missed timeout into a permanent hang.
            .process_group(0)
            .current_dir(&self.dir)
            .env("HOME", &self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start `/bin/sh -c`: {e}"))?;

        let pgid = child.id();

        // Drain both pipes on their own threads. Polling for the deadline while the
        // child fills a pipe buffer would deadlock: it blocks on write, we never
        // reap it, and the deadline fires on a process that was making progress.
        let out_rx = drain(child.stdout.take().expect("piped"));
        let err_rx = drain(child.stderr.take().expect("piped"));

        let deadline = Duration::from_secs(self.timeout_secs as u64);
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
        // grandchild holding the write end. Never block on it indefinitely: collect
        // what arrived, kill the group to release the rest, and move on. Output is
        // diagnostic; the verdict comes from the exit code and the reward file.
        let stdout = collect(&out_rx, &mut || kill_group(pgid));
        let stderr = collect(&err_rx, &mut || kill_group(pgid));

        Ok(Outcome {
            // A killed child reports no code. Distinguishing that from a real exit is
            // what `timed_out` is for; grading treats the two differently.
            exit_code: status.code(),
            timed_out,
            elapsed_secs: started.elapsed().as_secs_f32(),
            stdout,
            stderr,
        })
    }
}

fn drain<R: Read + Send + 'static>(mut pipe: R) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

/// Wait for a drained pipe, but only for a bounded grace period. On expiry run
/// `release` — which kills whatever still holds the pipe — and try once more.
fn collect(rx: &mpsc::Receiver<Vec<u8>>, release: &mut dyn FnMut()) -> String {
    let bytes = match rx.recv_timeout(DRAIN_GRACE) {
        Ok(b) => b,
        Err(_) => {
            release();
            rx.recv_timeout(DRAIN_GRACE).unwrap_or_default()
        }
    };
    String::from_utf8_lossy(&bytes).into_owned()
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
    /// The command's own exit code, or `None` when it was killed by a signal —
    /// including our own deadline kill. `127` is a command not found, which is a
    /// broken exercise rather than a failing one.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub elapsed_secs: f32,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}
