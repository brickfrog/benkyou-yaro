//! Which backend a test suite gets, and which suites are allowed to skip.
//!
//! Three kinds of test live in this directory. Most ask what a verdict means: whether a
//! gate rejects a wrong answer, whether a digest ungates an edited exercise. Isolation is
//! not the subject, and those tests get [`behaviour`], which needs nothing installed.
//!
//! The other two kinds ask what an isolating backend guarantees. Those need the real
//! thing, and there are machines where the real thing cannot exist: bubblewrap is Linux
//! only, and an engine plus a pulled image is not something `cargo test` installs on its
//! own. Those suites get [`sandbox`] or [`container`], which return `None` on such a
//! machine after printing one line, and panic instead when the matching `BENKYOU_REQUIRE_`
//! variable is `1`.
//!
//! A missing prerequisite is the only licence to skip. Any other failure is a failure.

// Every test binary compiles this whole file and uses part of it, so unused is normal here.
#![allow(dead_code)]

use benkyou::run::{Backend, Want};

/// Set to `1` to turn a skipped sandbox suite into a failing one.
pub const REQUIRE_SANDBOX: &str = "BENKYOU_REQUIRE_SANDBOX";

/// Set to `1` to turn a skipped container suite into a failing one.
pub const REQUIRE_CONTAINER: &str = "BENKYOU_REQUIRE_CONTAINER";

/// The backend for tests whose subject is not isolation.
///
/// The host, on purpose. These tests run fixture scripts this repository wrote, and they
/// must give the same answer on a contributor's mac as in CI. Reaching for a sandbox here
/// makes a verdict test fail for a reason that has nothing to do with verdicts.
pub fn behaviour() -> Backend {
    Backend::UnsafeHost
}

/// Bubblewrap, or `None` on a machine without it.
///
/// Bubblewrap is Linux only, and a package rather than a default even there. So this skips
/// like [`container`] does, and `BENKYOU_REQUIRE_SANDBOX=1` is what makes it mandatory. The
/// release gate sets that, and so does the one CI step that has a `bwrap` to offer.
pub fn sandbox() -> Option<Backend> {
    match Backend::choose(Want::Sandbox, None) {
        Ok(backend) => Some(backend),
        Err(why) => absent(REQUIRE_SANDBOX, why),
    }
}

/// A container engine with the runner image present, or `None` when either is missing.
///
/// Missing prerequisites are the only licence to skip: an earlier version returned `None`
/// on any failure, so a broken policy skipped everything and reported success.
pub fn container() -> Option<Backend> {
    match benkyou::run::runner_status(None, false) {
        Err(why) => absent(REQUIRE_CONTAINER, why),
        Ok(status) if status.image.is_none() => absent(
            REQUIRE_CONTAINER,
            format!(
                "runner image not pulled: {} - run `benkyou runner --pull`",
                status.reference
            ),
        ),
        Ok(_) => Some(
            Backend::choose(Want::Container, None)
                .expect("an engine and its image are present, so the backend must build"),
        ),
    }
}

/// A missing prerequisite: a skip, or a failure under the required mode.
///
/// "skipping" is printed on the skip path alone, so a `--nocapture` run can be grepped for
/// it and a release gate can demand none.
fn absent(require: &str, why: String) -> Option<Backend> {
    if matches!(std::env::var(require).as_deref(), Ok("1")) {
        panic!("{why}\n  {require}=1, so a missing prerequisite is a failure, not a skip");
    }
    eprintln!("skipping: {why}");
    None
}
