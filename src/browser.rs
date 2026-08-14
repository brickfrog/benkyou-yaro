//! The browser-facing exercise runner.
//!
//! [`crate::serve`] is transport and knows nothing about exercises; this is the half
//! that knows what a kata is. The split is what keeps the HTTP code testable without
//! a workspace and this code testable without a socket.
//!
//! The load-bearing property is that **code executes here, not in the tab**. Run and
//! Submit both go through [`crate::run::Runner`] — the same struct the CLI uses, in
//! the same environment, against the same grader. A second execution environment in
//! the page (Pyodide, a WASM SQLite) would mean the gate's twice-run guarantee held
//! in neither: the learner would get a green in the browser and a red from `grade`,
//! and would believe whichever came first. Monaco is a view over files on disk.
//!
//! The queue is the argument list: directories, or digests of banked exercises. What
//! is still missing is a queue built from a *goal* — the bank knows which concept each
//! bundle belongs to, but nothing yet decides which of several exercises for one
//! concept a learner should get, and picking the first match would be a policy
//! invented at the call site. Until that policy exists, the caller names what it
//! wants, exactly as it does for `attempt` and `grade`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::attempt;
use crate::exercise::{self, Reward, Task, Verdict};
use crate::record::{Event, Recorder};
use crate::gate::{safe_join, WORK};
use crate::run::{Access, Backend, Job};
use crate::serve::{Request, Response, ShutdownHandle};

/// The page, embedded rather than served from disk. Matches the `include_str!`
/// convention in `anki.rs`: the binary is the whole installation, and a runner that
/// needed an asset directory beside it would break `cargo install`.
const PAGE: &str = include_str!("../assets/serve.html");

/// One exercise in the queue.
pub struct Item {
    pub dir: PathBuf,
    pub task: Task,
    /// Parent of `work/`, as `attempt::open` and `attempt::grade` both expect.
    pub root: PathBuf,
    pub slug: String,
}

impl Item {
    /// Load an exercise and derive its workspace, refusing anything ungated.
    ///
    /// The refusal is duplicated from `attempt::open` on purpose: failing at startup
    /// names every bad exercise at once, before a browser is opened, instead of
    /// stranding the learner on item four of six.
    pub fn load(dir: &Path, backend: &Backend) -> Result<Self, String> {
        let task = exercise::load(dir)?;
        // The same predicate `attempt::open` uses, not a weaker `gate.is_some()`: a
        // `[gate]` table can exist and record a failure, and accepting it here would
        // let the session start and then strand the learner when open() refuses. The
        // digest half matters more in a queue than anywhere else - a session composed
        // from six directories is six chances for one of them to have moved.
        exercise::require_current(dir, backend)?;
        let slug = dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .ok_or_else(|| format!("{}: no directory name to work under", dir.display()))?;
        let root = crate::store::work_root(&task.task.concept_id, &slug)?;
        Ok(Item {
            dir: dir.to_path_buf(),
            task,
            root,
            slug,
        })
    }

    fn work(&self) -> PathBuf {
        self.root.join(crate::gate::WORK)
    }
}

/// Session bookkeeping. Held briefly; never across an execution.
struct State {
    index: usize,
    recorders: BTreeMap<usize, Recorder>,
}

pub struct App {
    items: Vec<Item>,
    goal: Option<PathBuf>,
    state: Mutex<State>,
    /// Exclusive use of the workspace: held across a run, a submit, or a save.
    ///
    /// The transport allows eight concurrent connections and the page's disabled
    /// buttons are only a client-side courtesy, so without this a double-click puts
    /// two processes in one workspace: two runs racing on the same files, or a
    /// `grade` copying `work/` while a save rewrites the file being copied.
    ///
    /// Executions refuse contention; saves wait for it. That asymmetry is the point.
    /// Queueing a run would execute the learner's code again after they already have
    /// their answer, so refusing is the honest report. Refusing a save would throw
    /// away something they typed, so it waits — bounded by the execution's deadline.
    ///
    /// Lock order is always `exec` then `state`; nothing takes `exec` while holding
    /// `state`, which is what keeps the pair deadlock-free.
    exec: Mutex<()>,
    shutdown: ShutdownHandle,
    /// The one way this session executes anything. Held on `App` rather than looked
    /// up per request so a session cannot change isolation halfway through, and so the
    /// backend a verdict was gated under is the backend that grades it.
    backend: Backend,
}

impl App {
    pub fn new(
        items: Vec<Item>,
        goal: Option<PathBuf>,
        shutdown: ShutdownHandle,
        backend: Backend,
    ) -> Self {
        App {
            items,
            goal,
            state: Mutex::new(State {
                index: 0,
                recorders: BTreeMap::new(),
            }),
            exec: Mutex::new(()),
            shutdown,
            backend,
        }
    }

    /// Route one request. Every arm returns JSON except the page itself.
    pub fn handle(&self, req: &Request) -> Response {
        let path = req.path.as_str();
        match (req.method.as_str(), path) {
            ("GET", "/") | ("GET", "/index.html") => Response::html(PAGE),
            ("GET", "/api/session") => self.ok(self.session()),
            ("GET", p) if p.starts_with("/api/exercise/") => {
                match p.trim_start_matches("/api/exercise/").parse::<usize>() {
                    Ok(i) => self.result(self.exercise(i)),
                    Err(_) => Response::error(400, "exercise index must be a number"),
                }
            }
            ("PUT", "/api/file") => self.result(self.write_file(&req.body)),
            ("POST", "/api/run") => self.result(self.run_cmd()),
            ("POST", "/api/submit") => self.result(self.submit()),
            ("POST", "/api/next") => self.result(self.next()),
            ("POST", "/api/done") => {
                let _ = self.finish();
                self.shutdown.shutdown();
                self.ok(json!({ "ok": true }))
            }
            _ => Response::error(404, "no such route"),
        }
    }

    fn ok(&self, v: Value) -> Response {
        match serde_json::to_vec(&v) {
            Ok(b) => Response::json(200, b),
            Err(e) => Response::error(500, &format!("serialising response: {e}")),
        }
    }

    fn result(&self, r: Result<Value, String>) -> Response {
        match r {
            Ok(v) => self.ok(v),
            // 422 rather than 500: these are refusals about the exercise or the
            // workspace, not the server falling over, and the page renders them as
            // an error strip the learner can act on.
            Err(e) => Response::error(422, &e),
        }
    }

    fn session(&self) -> Value {
        let index = self.state.lock().map(|s| s.index).unwrap_or(0);
        let items: Vec<Value> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                json!({
                    "i": i,
                    "title": it.slug,
                    "concept": it.task.task.concept_id,
                    "kind": it.task.task.kind,
                    "dir": it.dir,
                })
            })
            .collect();
        json!({ "index": index, "items": items, "goal": self.goal })
    }

    /// Claim the workspace for one execution, or refuse.
    ///
    /// `try_lock` rather than `lock`: waiting would run the learner's code again
    /// after they already have their result, and the second run would report on a
    /// workspace the first one may have changed.
    fn busy(&self) -> Result<std::sync::MutexGuard<'_, ()>, String> {
        self.exec
            .try_lock()
            .map_err(|_| "a run or submit is already in progress".to_string())
    }

    fn item(&self, i: usize) -> Result<&Item, String> {
        self.items
            .get(i)
            .ok_or_else(|| format!("no exercise {i} in this session"))
    }

    fn current(&self) -> Result<(usize, &Item), String> {
        let i = self
            .state
            .lock()
            .map_err(|_| "session state poisoned".to_string())?
            .index;
        Ok((i, self.item(i)?))
    }

    /// Materialise the workspace if it is not already there, then list it.
    ///
    /// Resumable by design: a workspace that already holds files is reopened rather
    /// than refused, because closing the tab must not cost the learner their work.
    /// `attempt::open` refuses a non-empty workspace and that rule stays right for
    /// the CLI, where a silent overwrite would be unrecoverable.
    fn exercise(&self, i: usize) -> Result<Value, String> {
        let item = self.item(i)?;
        let work = item.work();
        if !dir_has_files(&work) {
            std::fs::create_dir_all(&item.root).map_err(|e| e.to_string())?;
            attempt::open(&item.dir, &item.root, &self.backend)?;
        }

        {
            let mut st = self
                .state
                .lock()
                .map_err(|_| "session state poisoned".to_string())?;
            if let std::collections::btree_map::Entry::Vacant(slot) = st.recorders.entry(i) {
                let mut rec = Recorder::open(&work)?;
                let _ = rec.log(Event::Open {
                    exercise: item.slug.clone(),
                });
                slot.insert(rec);
            }
        }

        let instruction = std::fs::read_to_string(item.dir.join("instruction.md"))
            .unwrap_or_else(|e| format!("instruction.md unreadable: {e}"));

        let mut files = Vec::new();
        collect_files(&work, &work, &mut files)?;

        Ok(json!({
            "i": i,
            "instruction_md": instruction,
            "files": files,
            "run_cmd": item.task.workspace.run_cmd,
            "limits": {
                "learner_secs": item.task.limits.learner_secs,
                "check_secs": item.task.limits.check_secs,
            },
            "guidance": item.task.task.guidance_level,
        }))
    }

    /// Save one file into the workspace.
    ///
    /// Waits for a running execution instead of refusing one, which is the opposite
    /// of [`Self::run_cmd`] and [`Self::submit`] and deliberately so: re-running is
    /// the wrong answer to a double-click, but *losing an edit* is the wrong answer
    /// to a debounced save. Unserialised, a save landing while `grade` copies
    /// `work/` produces a graded artifact that is half of one revision and half of
    /// another — a verdict nothing can reproduce. The wait is bounded by the
    /// execution's own deadline.
    fn write_file(&self, body: &[u8]) -> Result<Value, String> {
        #[derive(serde::Deserialize)]
        struct Put {
            path: String,
            content: String,
        }
        let put: Put = serde_json::from_slice(body).map_err(|e| format!("bad request: {e}"))?;
        let _exec = self
            .exec
            .lock()
            .map_err(|_| "workspace lock poisoned".to_string())?;
        let (_, item) = self.current()?;
        let target = safe_join(&item.work(), &put.path)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&target, put.content).map_err(|e| format!("{}: {e}", target.display()))?;
        Ok(json!({ "ok": true }))
    }

    fn run_cmd(&self) -> Result<Value, String> {
        let _exec = self.busy()?;
        let (i, item) = self.current()?;
        let cmd = item
            .task
            .workspace
            .run_cmd
            .as_deref()
            .ok_or("this exercise declares no [workspace] run_cmd")?;
        let deps = crate::deps::require(&item.task.deps, crate::deps::Runtime::of(&self.backend))?;
        let outcome = self.backend.run(
            &Job::new(
                &item.root,
                &[(WORK, Access::Write)],
                WORK,
                cmd,
                item.task.limits.learner_secs,
            )
            .with_deps(deps.as_deref()),
        )?;

        let ms = (outcome.elapsed_secs * 1000.0) as u64;
        self.log(i, Event::Run {
            ms,
            exit: outcome.exit_code,
            timed_out: outcome.timed_out,
        });

        Ok(json!({
            "exit": outcome.exit_code,
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
            "secs": outcome.elapsed_secs,
            "timed_out": outcome.timed_out,
        }))
    }

    /// Grade the workspace through exactly the path `benkyou grade` takes.
    fn submit(&self) -> Result<Value, String> {
        let _exec = self.busy()?;
        let (i, item) = self.current()?;
        let att = attempt::grade(&item.dir, &item.task, &item.root, &self.backend)?;
        let score = attempt::practice_score(&att.verdict);

        let reward: Option<Reward> = att
            .reward
            .as_deref()
            .and_then(|t| serde_json::from_str(t).ok());
        let dims = reward
            .as_ref()
            .map(|r| r.dimensions.clone())
            .unwrap_or_else(|| match &att.verdict {
                Verdict::Fail(d) => d.clone(),
                _ => BTreeMap::new(),
            });

        let mut practice = Value::Null;
        if let (Some(goal), Some(score)) = (&self.goal, score) {
            let c = attempt::credit(
                goal,
                &item.task.task.concept_id,
                score,
                crate::store::today(),
            )?;
            practice = json!({
                "node": c.node,
                "score": c.score,
                "mastery": c.mastery,
                "also_credited": c.also_credited,
            });
        }

        let ms = self.elapsed(i);
        self.log(i, Event::Submit {
            ms,
            verdict: verdict_tag(&att.verdict).to_string(),
            dims: dims.clone(),
        });

        Ok(json!({
            "verdict": verdict_tag(&att.verdict),
            "dims": dims,
            "detail": reward.as_ref().and_then(|r| r.detail.clone()),
            "advisory": item.task.verify.advisory,
            "must_pass": item.task.verify.must_pass,
            "recorded": !practice.is_null(),
            "practice": practice,
            "check_stderr": (!att.check_stderr.trim().is_empty())
                .then(|| att.check_stderr.trim().to_string()),
        }))
    }

    fn next(&self) -> Result<Value, String> {
        let mut st = self
            .state
            .lock()
            .map_err(|_| "session state poisoned".to_string())?;
        let at = st.index;
        if let Some(rec) = st.recorders.get_mut(&at) {
            let _ = rec.log(Event::Next);
        }
        let last = self.items.len().saturating_sub(1);
        let done = st.index >= last;
        if !done {
            st.index += 1;
        }
        Ok(json!({ "index": st.index, "done": done }))
    }

    fn finish(&self) -> Result<(), String> {
        let mut st = self
            .state
            .lock()
            .map_err(|_| "session state poisoned".to_string())?;
        for rec in st.recorders.values_mut() {
            let _ = rec.log(Event::Done);
        }
        Ok(())
    }

    /// Log best-effort. A journal that cannot be written is worth reporting, never
    /// worth failing an attempt over — the learner's work is the artifact, not this.
    fn log(&self, i: usize, ev: Event) {
        if let Ok(mut st) = self.state.lock() {
            if let Some(rec) = st.recorders.get_mut(&i) {
                if let Err(e) = rec.log(ev) {
                    eprintln!("attempt log: {e}");
                }
            }
        }
    }

    fn elapsed(&self, i: usize) -> u64 {
        self.state
            .lock()
            .ok()
            .and_then(|st| st.recorders.get(&i).map(|r| r.elapsed_ms()))
            .unwrap_or(0)
    }
}

fn verdict_tag(v: &Verdict) -> &'static str {
    match v {
        Verdict::Pass => "Pass",
        Verdict::Fail(_) => "Fail",
        Verdict::Timeout(_) => "Timeout",
        Verdict::CheckBroken(_) => "CheckBroken",
    }
}

fn dir_has_files(dir: &Path) -> bool {
    std::fs::read_dir(dir).map(|mut d| d.next().is_some()).unwrap_or(false)
}


/// Read the workspace as editable text.
///
/// Skips anything that is not valid UTF-8 and anything over 256 KiB: a sample
/// dataset copied in by `setup/` is context the learner reads elsewhere, not
/// something to load into an editor tab, and a binary would arrive as mojibake and
/// be saved back corrupted.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<Value>) -> Result<(), String> {
    const MAX: u64 = 256 * 1024;
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| !p.is_symlink())
        .collect();
    paths.sort();
    for p in paths {
        let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        if p.is_dir() {
            if !is_noise_dir(&name) {
                collect_files(root, &p, out)?;
            }
            continue;
        }
        let big = std::fs::metadata(&p).map(|m| m.len() > MAX).unwrap_or(true);
        let rel = p
            .strip_prefix(root)
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_default();
        match (big, std::fs::read_to_string(&p)) {
            (false, Ok(content)) => out.push(json!({ "path": rel, "content": content })),
            // Too large to edit, but the learner should know it is there: a sample
            // dataset copied in by `setup/` is part of the task.
            (true, _) => out.push(json!({ "path": rel, "content": Value::Null })),
            // Not text at all. Listing it would open an editor tab that saves back
            // corrupted, and the learner did not put it there anyway.
            (false, Err(_)) => {}
        }
    }
    Ok(())
}

/// Directories the learner's own tooling creates and never edits.
///
/// Not cosmetic: running the code once makes `__pycache__`, and without this the
/// editor grows a tab holding a `.pyc` between the first Run and the first Submit.
/// Dot-directories go the same way — a `.venv` is thousands of files.
fn is_noise_dir(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "__pycache__" | "node_modules" | "target" | "venv" | "env"
        )
}

#[cfg(test)]
mod listing_tests {
    use super::*;

    #[test]
    fn the_editor_never_sees_caches_or_binaries() {
        let root = std::env::temp_dir().join(format!("bk-listing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("__pycache__")).unwrap();
        std::fs::create_dir_all(root.join(".venv/lib")).unwrap();
        std::fs::write(root.join("solution.py"), "x = 1\n").unwrap();
        // Invalid UTF-8, as a .pyc is.
        std::fs::write(root.join("__pycache__/s.pyc"), [0xffu8, 0xfe, 0x00]).unwrap();
        std::fs::write(root.join(".venv/lib/junk.py"), "y = 2\n").unwrap();
        std::fs::write(root.join("blob.bin"), [0xffu8, 0xfe, 0x00]).unwrap();

        let mut out = Vec::new();
        collect_files(&root, &root, &mut out).unwrap();
        let paths: Vec<&str> = out
            .iter()
            .map(|v| v["path"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(paths, vec!["solution.py"], "listed: {paths:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::Want;

    fn work() -> PathBuf {
        PathBuf::from("/tmp/w")
    }

    #[test]
    fn a_plain_relative_path_lands_in_the_workspace() {
        assert_eq!(safe_join(&work(), "solution.py").unwrap(), work().join("solution.py"));
        assert_eq!(safe_join(&work(), "a/b.py").unwrap(), work().join("a/b.py"));
        assert_eq!(safe_join(&work(), "./s.py").unwrap(), work().join("s.py"));
    }

    #[test]
    fn a_path_that_climbs_out_is_refused() {
        // The interesting case is the one that *lands* back inside after
        // normalisation: rejecting the component outright means we never have to
        // reason about whether `a/../b` resolved somewhere acceptable.
        for bad in ["../x", "a/../../x", "/etc/passwd", ""] {
            assert!(safe_join(&work(), bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn every_verdict_has_a_tag_the_page_can_switch_on() {
        assert_eq!(verdict_tag(&Verdict::Pass), "Pass");
        assert_eq!(verdict_tag(&Verdict::Fail(BTreeMap::new())), "Fail");
        assert_eq!(verdict_tag(&Verdict::Timeout(60)), "Timeout");
        assert_eq!(
            verdict_tag(&Verdict::CheckBroken("boom".into())),
            "CheckBroken"
        );
    }

    /// A session must refuse everything unshowable at startup. Refusing late strands
    /// the learner mid-queue, which is the whole reason this check is duplicated
    /// from `attempt::open`.
    ///
    /// The three cases are the three distinct ways an exercise can fail to be
    /// showable, and they are kept apart because a single "refused" assertion passes
    /// for the wrong reason: everything is refused when nothing is gated.
    #[test]
    fn a_session_refuses_anything_the_gate_did_not_validate() {
        const BASE: &str = "schema_version = \"1\"\n\
             [task]\nid = \"t\"\nconcept_id = \"c\"\nkind = \"kata\"\nguidance_level = \"blank\"\n\
             [verify]\ncmd = \"sh check/check.sh\"\nmust_pass = [\"correctness\"]\n";

        let backend = Backend::choose(Want::Auto, None).expect("a sandbox");
        let gate = |solution_passes, empty_fails, digest: &str| exercise::Gate {
            solution_passes,
            empty_fails,
            validated_at: "x".into(),
            digest: digest.into(),
            known_bad_caught: vec!["trap".into()],
            runner: exercise::Runner::of(&Backend::choose(Want::Auto, None).expect("a sandbox")),
            env: exercise::Env::current(),
            deps: vec![],
        };

        // (name, record to write, the phrase the refusal must carry)
        let cases: [(&str, Option<Box<dyn Fn(&str) -> exercise::Gate>>, &str); 4] = [
            ("no gate record at all", None, "not validated"),
            (
                "the gate recorded a rejection",
                Some(Box::new(move |_| gate(false, false, "x"))),
                "rejected this exercise",
            ),
            (
                "solution passed but the empty stub also passed",
                Some(Box::new(move |_| gate(true, false, "x"))),
                "rejected this exercise",
            ),
            (
                "validated, then the exercise was edited",
                Some(Box::new(move |_| gate(true, true, "deadbeefdeadbeef"))),
                "changed since it was gated",
            ),
        ];

        for (i, (name, record, expected)) in cases.into_iter().enumerate() {
            let dir =
                std::env::temp_dir().join(format!("bk-browser-{}-{i}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("task.toml"), BASE).unwrap();
            if let Some(build) = record {
                exercise::write_gate(&dir, &build("")).unwrap();
            }
            match Item::load(&dir, &backend) {
                Ok(_) => panic!("accepted an exercise where {name}"),
                Err(e) => assert!(e.contains(expected), "{name}: wanted {expected:?}, got {e}"),
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The positive half of the check above: a record whose digest matches is
    /// accepted. Without this, every assertion in that test is satisfied by a
    /// `load` that refuses unconditionally.
    #[test]
    fn a_matching_digest_is_accepted() {
        let dir = std::env::temp_dir().join(format!("bk-browser-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("task.toml"),
            "schema_version = \"1\"\n\
             [task]\nid = \"t\"\nconcept_id = \"c\"\nkind = \"kata\"\nguidance_level = \"blank\"\n\
             [verify]\ncmd = \"sh check/check.sh\"\nmust_pass = [\"correctness\"]\n",
        )
        .unwrap();
        let backend = Backend::choose(Want::Auto, None).expect("a sandbox");
        let digest = crate::digest::exercise_digest(&dir).unwrap();
        exercise::write_gate(
            &dir,
            &exercise::Gate {
                solution_passes: true,
                empty_fails: true,
                validated_at: "x".into(),
                digest,
                known_bad_caught: vec!["trap".into()],
                runner: exercise::Runner::of(&backend),
                env: exercise::Env::current(),
                deps: vec![],
            },
        )
        .unwrap();
        Item::load(&dir, &backend).expect("a current gate record must be accepted");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
