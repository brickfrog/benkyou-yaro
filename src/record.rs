//! The attempt log.
//!
//! An append-only line of JSON per semantic step of a sitting: the exercise opened,
//! each run and what it exited with, each submission and what it scored, the move to
//! the next entry, the end. It exists because that timing is the one thing a flashcard
//! app structurally cannot produce — a card's only measurable interval runs from reveal
//! to self-grade, which measures how fast the learner judged themselves. Here the
//! clock covers actual work, and nothing in it is self-reported.
//!
//! **This data is recorded but never scheduled on.** It is diagnostic: something to
//! read afterwards when a concept feels harder than its scores say. The scheduler's
//! state is four numbers per concept, all of them derived from graded verdicts
//! (DESIGN.md §5), and time-on-task is deliberately not one of them. Feeding
//! self-reported or wall-clock time into scheduling is precisely the mistake this
//! project avoids elsewhere — it is how an SRS ends up rewarding the learner for
//! answering quickly rather than for being right — so a reader of this log is welcome
//! to draw conclusions from it, and the code is not.
//!
//! Semantic events only. There is no keystroke capture and no draft snapshotting: the
//! learner's half-finished thinking is theirs, and a log of it would be a surveillance
//! record that buys nothing the event stream does not already give.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

/// One step of a sitting.
///
/// `ms` is a monotonic duration, never a difference of two wall clocks — see
/// [`Recorder::elapsed_ms`] for why that distinction is not pedantry.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Event {
    Open {
        exercise: String,
    },
    Run {
        ms: u64,
        /// `null` for a process that was killed rather than exiting, which is the same
        /// distinction [`crate::run::Outcome`] keeps and the reason `timed_out` is
        /// recorded beside it rather than inferred from the code.
        exit: Option<i32>,
        timed_out: bool,
    },
    Submit {
        ms: u64,
        verdict: String,
        dims: BTreeMap<String, f32>,
    },
    Next,
    Done,
}

/// A handle on one session's log file, plus the monotonic origin its durations are
/// measured from.
///
/// Constructed once per sitting: the origin is the point [`Recorder::open`] was called,
/// so a `Run` event's `ms` is time since the session began unless the caller subtracts
/// two of its own [`Recorder::elapsed_ms`] readings.
pub struct Recorder {
    file: File,
    /// Kept for error messages. A write failure that does not name the file it could
    /// not write is not reportable, and reporting is the only thing the caller can do.
    path: PathBuf,
    started: Instant,
}

impl Recorder {
    /// Opens (creating) `attempt.jsonl` as a sibling of the `work/` directory.
    ///
    /// A sibling and not a child, because `work/` is not this module's to write in:
    /// [`crate::attempt::open`] refuses a workspace that already holds files, and a log
    /// inside it would be that file. Grading also copies `work/` wholesale into the
    /// sealed run directory, which would hand the grader a copy of the log.
    ///
    /// Opened `create(true).append(true)`, so a second session over the same workspace
    /// extends the history rather than replacing it. There is no truncating mode on
    /// purpose: the point of the log is the sequence, and one lost session is a hole in
    /// it that nothing else in the tool can refill.
    pub fn open(work_dir: &Path) -> Result<Self, String> {
        let dir = work_dir.parent().ok_or_else(|| {
            format!(
                "{}: no parent directory to put attempt.jsonl beside",
                work_dir.display()
            )
        })?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join("attempt.jsonl");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self {
            file,
            path,
            started: Instant::now(),
        })
    }

    /// Append one event and flush it.
    ///
    /// The flush is per line rather than per session because the interesting sessions
    /// are the ones that end badly — a kill, a crash, a closed laptop — and a buffered
    /// log loses exactly the events that explain them.
    ///
    /// The `Result` is the caller's to discard. **A log that cannot be written must
    /// never fail an attempt**: the learner's verdict came from their code and the
    /// grader, and losing a diagnostic line is not a reason to throw that away. So this
    /// reports the failure and does nothing else with it; `let _ = rec.log(..)` is a
    /// legitimate call, and a caller that wants the failure surfaced can propagate it.
    pub fn log(&mut self, ev: Event) -> Result<(), String> {
        let line = encode(&ev, &iso_utc(epoch_secs(SystemTime::now())))?;
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.flush())
            .map_err(|e| format!("{}: {e}", self.path.display()))
    }

    /// Milliseconds since this recorder was opened, from a monotonic clock.
    ///
    /// [`Instant`] and not [`SystemTime`], because wall-clock deltas over a session are
    /// not measurements. A tab left open overnight, an NTP step mid-exercise, or a
    /// suspend-resume all move the wall clock underneath a running session, and any of
    /// them can make a subtraction negative or off by hours. `Instant` cannot go
    /// backwards, so a duration from it is either right or absent.
    pub fn elapsed_ms(&self) -> u64 {
        // u128 -> u64 cannot lose anything here: it would take ~585 million years of
        // uptime to overflow.
        self.started.elapsed().as_millis() as u64
    }
}

/// One event as its log line, newline included.
///
/// Key order is whatever `serde_json`'s map gives, which is sorted — so `at` leads and
/// `t` sits among the payload fields. Ugly to read, but stable, which is what makes the
/// lines diffable and the tests below exact.
fn encode(ev: &Event, at: &str) -> Result<String, String> {
    let mut obj = match serde_json::to_value(ev).map_err(|e| e.to_string())? {
        Value::Object(map) => map,
        other => return Err(format!("event serialised to {other}, not an object")),
    };
    obj.insert("at".to_string(), Value::String(at.to_string()));
    let mut line = serde_json::to_string(&Value::Object(obj)).map_err(|e| e.to_string())?;
    line.push('\n');
    Ok(line)
}

/// Seconds since the epoch. A clock set before 1970 yields 0 rather than an error:
/// a nonsense timestamp still keeps the event, and dropping the event to protest the
/// system clock would lose the only copy of it.
fn epoch_secs(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `1970-01-01T00:00:00Z` from seconds since the epoch, UTC, no fractional part.
///
/// Hand-rolled because the alternative is a date dependency, and the whole of what is
/// needed is a sortable stamp a human can read. UTC only — a log with local times in it
/// stops being orderable the moment the learner travels or the offset shifts.
///
/// The year walk is a linear scan from 1970. At one call per logged event that is
/// nothing, and it is far easier to check by eye than the closed-form conversions.
fn iso_utc(secs: u64) -> String {
    let mut year = 1970u64;
    let mut day = secs / 86_400;
    loop {
        let len = if is_leap(year) { 366 } else { 365 };
        if day < len {
            break;
        }
        day -= len;
        year += 1;
    }
    let mut month = 1u64;
    loop {
        let len = month_days(year, month);
        if day < len {
            break;
        }
        day -= len;
        month += 1;
    }
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{:02}T{:02}:{:02}:{:02}Z",
        day + 1,
        rem / 3_600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// Gregorian rule in full. Dropping the century clauses would be correct until 2100 and
/// then silently a day out, which is the kind of bug nobody finds in a log.
fn is_leap(y: u64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn month_days(y: u64, m: u64) -> u64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(y) => 29,
        _ => 28,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: &str = "1970-01-01T00:00:00Z";

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("benkyou-record-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work")).expect("scratch");
        dir
    }

    #[test]
    fn the_formatter_agrees_with_known_epochs() {
        // Boundaries, both leap rules, and a month rollover. Every one of these was a
        // wrong answer at some point in a hand-rolled date routine.
        let cases = [
            (0u64, "1970-01-01T00:00:00Z"),
            (86_399, "1970-01-01T23:59:59Z"),
            (86_400, "1970-01-02T00:00:00Z"),
            (1_234_567_890, "2009-02-13T23:31:30Z"),
            // 2000 is divisible by 100 and by 400, so it is a leap year.
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_709_164_800, "2024-02-29T00:00:00Z"),
            // Leap day into March: the month boundary the 28/29 rule decides.
            (1_709_251_199, "2024-02-29T23:59:59Z"),
            (1_709_251_200, "2024-03-01T00:00:00Z"),
            // Year boundary.
            (1_767_225_599, "2025-12-31T23:59:59Z"),
            (1_767_225_600, "2026-01-01T00:00:00Z"),
            // 2100 is divisible by 100 but not 400, so February has 28 days.
            (4_107_542_399, "2100-02-28T23:59:59Z"),
            (4_107_542_400, "2100-03-01T00:00:00Z"),
        ];
        for (secs, want) in cases {
            assert_eq!(iso_utc(secs), want, "epoch {secs}");
        }
    }

    #[test]
    fn a_clock_set_before_the_epoch_still_produces_a_stamp() {
        let ancient = UNIX_EPOCH - std::time::Duration::from_secs(60);
        assert_eq!(iso_utc(epoch_secs(ancient)), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn every_variant_has_its_tag_and_its_timestamp() {
        let open = Event::Open {
            exercise: "dedupe".to_string(),
        };
        assert_eq!(
            encode(&open, AT).unwrap(),
            "{\"at\":\"1970-01-01T00:00:00Z\",\"exercise\":\"dedupe\",\"t\":\"open\"}\n"
        );

        let run = Event::Run {
            ms: 812,
            exit: Some(1),
            timed_out: false,
        };
        assert_eq!(
            encode(&run, AT).unwrap(),
            "{\"at\":\"1970-01-01T00:00:00Z\",\"exit\":1,\"ms\":812,\"t\":\"run\",\"timed_out\":false}\n"
        );

        // A killed process has no exit code, and the log must say so rather than
        // inventing one.
        let killed = Event::Run {
            ms: 600_000,
            exit: None,
            timed_out: true,
        };
        assert_eq!(
            encode(&killed, AT).unwrap(),
            "{\"at\":\"1970-01-01T00:00:00Z\",\"exit\":null,\"ms\":600000,\"t\":\"run\",\"timed_out\":true}\n"
        );

        let mut dims = BTreeMap::new();
        dims.insert("correctness".to_string(), 1.0f32);
        let submit = Event::Submit {
            ms: 4_200,
            verdict: "Pass".to_string(),
            dims,
        };
        assert_eq!(
            encode(&submit, AT).unwrap(),
            "{\"at\":\"1970-01-01T00:00:00Z\",\"dims\":{\"correctness\":1.0},\"ms\":4200,\"t\":\"submit\",\"verdict\":\"Pass\"}\n"
        );

        assert_eq!(
            encode(&Event::Next, AT).unwrap(),
            "{\"at\":\"1970-01-01T00:00:00Z\",\"t\":\"next\"}\n"
        );
        assert_eq!(
            encode(&Event::Done, AT).unwrap(),
            "{\"at\":\"1970-01-01T00:00:00Z\",\"t\":\"done\"}\n"
        );
    }

    #[test]
    fn the_log_lands_beside_the_workspace_and_never_in_it() {
        let root = scratch("sibling");
        let mut rec = Recorder::open(&root.join("work")).unwrap();
        rec.log(Event::Next).unwrap();
        assert!(root.join("attempt.jsonl").is_file());
        assert_eq!(
            std::fs::read_dir(root.join("work")).unwrap().count(),
            0,
            "work/ must stay exactly as the learner left it"
        );
    }

    #[test]
    fn appending_preserves_what_was_already_there() {
        let root = scratch("append");
        let work = root.join("work");

        let mut first = Recorder::open(&work).unwrap();
        first
            .log(Event::Open {
                exercise: "dedupe".to_string(),
            })
            .unwrap();
        drop(first);

        // A separate recorder over the same workspace stands in for the session after a
        // crash: it must extend the log, not start it over.
        let mut second = Recorder::open(&work).unwrap();
        second.log(Event::Done).unwrap();

        let text = std::fs::read_to_string(root.join("attempt.jsonl")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "got {text:?}");
        assert!(lines[0].contains("\"t\":\"open\""));
        assert!(lines[1].contains("\"t\":\"done\""));
    }

    #[test]
    fn each_line_is_flushed_before_log_returns() {
        // The recorder is still open and still owns the handle; if the line were only
        // buffered, this read would come back empty and a crash would lose the event.
        let root = scratch("flush");
        let mut rec = Recorder::open(&root.join("work")).unwrap();
        rec.log(Event::Next).unwrap();
        let text = std::fs::read_to_string(root.join("attempt.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 1, "got {text:?}");
    }

    #[test]
    fn durations_come_from_the_monotonic_clock() {
        let root = scratch("elapsed");
        let rec = Recorder::open(&root.join("work")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        // Only a lower bound is assertable: a loaded machine can sleep for longer, and
        // an upper bound would make this test fail on someone else's CI, not on a bug.
        assert!(rec.elapsed_ms() >= 15, "got {}", rec.elapsed_ms());
    }
}
