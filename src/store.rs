//! Persistence, and where it lives.
//!
//! One JSON file per goal, holding the graph and the learner state, plus a sibling
//! file of practice fluency. Plain, diffable, and regenerable — the graph half is
//! disposable and the `state` half is the only thing worth backing up. See DESIGN.md §1.
//!
//! Those files have a home, and the tool knows it. A study tool that makes the user
//! name a path on every invocation is a tool that gets a `~/benkyou` invented for it,
//! which is how this module came to own XDG resolution.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::graph::Graph;
use crate::sched::Fluencies;

/// Goals and fluency: `$XDG_DATA_HOME/benkyou`, else `~/.local/share/benkyou`.
///
/// Data, not state or cache — a goal file holds graded evidence that took the learner
/// real time to produce and cannot be regenerated from anything else.
pub fn data_dir() -> Result<PathBuf, String> {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

/// Workspaces: `$XDG_STATE_HOME/benkyou`, else `~/.local/state/benkyou`.
///
/// State, not data: a workspace is scratch for one sitting, reconstructible from the
/// exercise directory. Only the answer the learner types is theirs, and it is graded
/// out of here into fluency, which does live in the data dir.
pub fn state_dir() -> Result<PathBuf, String> {
    xdg_dir("XDG_STATE_HOME", ".local/state")
}

/// The env read. Kept to one line of impurity so [`xdg_base`] can be tested without
/// mutating process-global environment from a threaded test harness.
fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf, String> {
    xdg_base(
        std::env::var_os(var).as_deref(),
        std::env::var_os("HOME").as_deref(),
        var,
        fallback,
    )
}

/// The XDG spec is explicit that a relative value must be ignored as invalid, rather
/// than resolved against the working directory. An empty value is unset.
fn xdg_base(
    configured: Option<&OsStr>,
    home: Option<&OsStr>,
    var: &str,
    fallback: &str,
) -> Result<PathBuf, String> {
    if let Some(base) = configured.filter(|b| !b.is_empty()) {
        let base = Path::new(base);
        if base.is_absolute() {
            return Ok(base.join("benkyou"));
        }
    }
    let home = home
        .filter(|h| !h.is_empty())
        .ok_or_else(|| format!("neither ${var} nor $HOME is set — pass an explicit path"))?;
    Ok(Path::new(home).join(fallback).join("benkyou"))
}

/// Resolve a goal argument. A bare word — no `/`, no `.json` — names a stored goal in
/// the data dir; anything else is a path exactly as typed, so a goal checked into a
/// repo still works.
pub fn goal_path(arg: &str) -> Result<PathBuf, String> {
    if arg.is_empty() {
        return Err("empty goal name".to_string());
    }
    if arg.contains('/') || arg.ends_with(".json") {
        return Ok(PathBuf::from(arg));
    }
    Ok(data_dir()?.join("goals").join(format!("{arg}.json")))
}

/// Stored goal names, sorted. Without this, resolving goals by name would be a lookup
/// with no way to see what there is to look up.
pub fn list_goals() -> Result<Vec<String>, String> {
    goal_names_in(&data_dir()?.join("goals"))
}

/// The goals directory, created if absent.
///
/// The binary never writes the first graph — generation lives in the host agent — so
/// the directory has to exist before anything the agent does can land in it.
pub fn goals_dir() -> Result<PathBuf, String> {
    let dir = data_dir()?.join("goals");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

fn goal_names_in(dir: &Path) -> Result<Vec<String>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // No goals yet is an empty list, not a failure.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut out: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        // `<name>.fluency.json` is a sibling of `<name>.json`, not a goal of its own —
        // listing it would send the caller to load practice history as a graph.
        if !stem.ends_with(".fluency") {
            out.push(stem);
        }
    }
    out.sort();
    Ok(out)
}

/// The per-exercise scratch container, derived rather than asked for so that `attempt`
/// and `grade` cannot be pointed at two different directories.
///
/// This is the root the attempt module lays out under — it holds `work/` plus the
/// throwaway sealed directory grading needs — so it is keyed by concept and slug and
/// is deliberately not itself called `work`.
pub fn work_root(concept: &str, slug: &str) -> Result<PathBuf, String> {
    Ok(state_dir()?.join("exercises").join(concept).join(slug))
}

/// Where fluency lives, given the goal file. Kept separate because the graph is
/// regenerated freely and practice history must survive that.
pub fn fluency_path(goal_file: &Path) -> PathBuf {
    let stem = goal_file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "goal".into());
    goal_file.with_file_name(format!("{stem}.fluency.json"))
}

pub fn load_graph(path: &Path) -> Result<Graph, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Write atomically: a crash mid-write must not leave a goal file that no longer
/// parses, because the learner state in it is not reconstructible.
pub fn save_graph(path: &Path, graph: &Graph) -> Result<(), String> {
    let text = serde_json::to_string_pretty(graph).map_err(|e| e.to_string())?;
    write_atomic(path, &text)
}

pub fn load_fluencies(path: &Path) -> Result<Fluencies, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        // Absent is not an error: nothing has been practised yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Fluencies::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub fn save_fluencies(path: &Path, f: &Fluencies) -> Result<(), String> {
    let text = serde_json::to_string_pretty(f).map_err(|e| e.to_string())?;
    write_atomic(path, &text)
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Whole days since the Unix epoch. The procedural scheduler works in days, because
/// session-level spacing is the granularity the evidence supports.
pub fn today() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86_400) as i64)
        .unwrap_or(0)
}

pub fn now_iso() -> String {
    // Enough for an audit trail without pulling in a date library: seconds since the
    // epoch, which sorts correctly and is unambiguous.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    #[test]
    fn an_absolute_xdg_value_wins_and_gets_its_own_subdirectory() {
        let got = xdg_base(Some(os("/srv/data")), Some(os("/home/j")), "V", ".local/share");
        assert_eq!(got.unwrap(), PathBuf::from("/srv/data/benkyou"));
    }

    #[test]
    fn a_relative_or_empty_xdg_value_is_invalid_and_falls_back_to_home() {
        // The spec says a relative path "must be considered invalid", not resolved
        // against the working directory — otherwise state lands wherever you `cd`.
        for bad in ["relative/data", ""] {
            let got = xdg_base(Some(os(bad)), Some(os("/home/j")), "V", ".local/share");
            assert_eq!(
                got.unwrap(),
                PathBuf::from("/home/j/.local/share/benkyou"),
                "`{bad}` was honoured"
            );
        }
    }

    #[test]
    fn an_unset_xdg_value_falls_back_to_the_documented_default() {
        let data = xdg_base(None, Some(os("/home/j")), "V", ".local/share").unwrap();
        let state = xdg_base(None, Some(os("/home/j")), "V", ".local/state").unwrap();
        assert_eq!(data, PathBuf::from("/home/j/.local/share/benkyou"));
        assert_eq!(state, PathBuf::from("/home/j/.local/state/benkyou"));
        assert_ne!(data, state, "data and state must not share a directory");
    }

    #[test]
    fn with_nowhere_to_write_it_says_so_instead_of_guessing() {
        // Silently picking the working directory is how a tool scatters goal files.
        let err = xdg_base(None, None, "XDG_DATA_HOME", ".local/share").unwrap_err();
        assert!(err.contains("XDG_DATA_HOME"), "{err}");
        assert!(err.contains("HOME"), "{err}");

        let err = xdg_base(Some(os("relative")), Some(os("")), "V", ".x").unwrap_err();
        assert!(err.contains("HOME"), "an empty HOME was treated as set: {err}");
    }

    #[test]
    fn a_path_like_goal_argument_is_left_exactly_as_typed() {
        // Goals checked into a repo must keep working, and must not be rewritten into
        // the data dir behind the caller's back.
        for arg in [
            "team-goals/backend-ramp.json",
            "./x.json",
            "/abs/x.json",
            "x.json",
            "../up.json",
        ] {
            assert_eq!(goal_path(arg).unwrap(), PathBuf::from(arg), "rewrote `{arg}`");
        }
    }

    #[test]
    fn a_bare_name_lands_under_the_data_directory() {
        let got = goal_path("ramp").unwrap();
        assert!(got.is_absolute() || std::env::var_os("HOME").is_none());
        assert!(
            got.ends_with("benkyou/goals/ramp.json"),
            "{}",
            got.display()
        );
    }

    #[test]
    fn an_empty_goal_argument_is_refused_rather_than_resolved() {
        assert!(goal_path("").is_err());
    }

    #[test]
    fn a_workspace_is_keyed_by_concept_and_slug_so_two_exercises_cannot_collide() {
        let a = work_root("sql_joins", "rollup").unwrap();
        let b = work_root("sql_dedup", "rollup").unwrap();
        assert_ne!(a, b, "same slug under different concepts shared a workspace");
        assert!(a.ends_with("benkyou/exercises/sql_joins/rollup"), "{}", a.display());
    }

    #[test]
    fn listing_goals_skips_the_fluency_sibling_and_anything_that_is_not_a_goal() {
        // The fluency file shares a stem prefix with its goal. Listing it would send
        // the caller off to parse practice history as a graph.
        let dir = std::env::temp_dir().join(format!("benkyou-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("notes.json")).unwrap(); // a directory, not a goal
        for f in ["ramp.json", "ramp.fluency.json", "korean.json", "README.md", ".hidden"] {
            std::fs::write(dir.join(f), "{}").unwrap();
        }

        let got = goal_names_in(&dir).unwrap();
        assert_eq!(
            got,
            vec!["korean".to_string(), "ramp".to_string()],
            "both goals listed, fluency sibling and non-goals skipped: {got:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_absent_goal_directory_lists_nothing_rather_than_failing() {
        // First run, before anything has been stored.
        let missing = std::env::temp_dir().join("benkyou-does-not-exist-9f3a");
        assert_eq!(goal_names_in(&missing).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn fluency_sits_beside_its_goal_wherever_that_is() {
        let f = fluency_path(Path::new("/data/benkyou/goals/ramp.json"));
        assert_eq!(f, PathBuf::from("/data/benkyou/goals/ramp.fluency.json"));
    }
}
