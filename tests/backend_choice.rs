//! Which backend a caller gets, and which one a caller can never get by accident.
//!
//! Selection is ordered rather than negotiated, so this file states the order instead of
//! naming a backend. An earlier version asserted that the automatic choice is bubblewrap,
//! which is true on Linux and false on a mac, where the same policy reaches a container.
//!
//! Nothing here runs a job, so it gives a verdict on a machine with no bubblewrap and no
//! engine as well as on one with both.

use benkyou::run::{Backend, Want};

/// What this machine can offer, asked once so the assertions below agree with each other.
fn available() -> (bool, bool) {
    (
        Backend::choose(Want::Sandbox, None).is_ok(),
        Backend::choose(Want::Container, None).is_ok(),
    )
}

/// The automatic choice is the sandbox where there is one and a container where there is
/// not. Bubblewrap needs no daemon, no image and no pull, so it stays first.
#[test]
fn auto_takes_the_sandbox_first_and_a_container_second() {
    let auto = Backend::choose(Want::Auto, None);
    match available() {
        (true, _) => assert_eq!(
            auto.expect("a sandbox is present").name(),
            "sandbox",
            "a machine with bubblewrap must not reach for an engine"
        ),
        (false, true) => assert_eq!(
            auto.expect("an engine is present").name(),
            "container",
            "with no namespaces available the engine is the isolating backend"
        ),
        (false, false) => {
            let err = auto.expect_err("neither backend exists here");
            assert!(
                err.contains("bwrap"),
                "the refusal must name bubblewrap: {err}"
            );
            assert!(
                err.contains("docker") || err.contains("container"),
                "the refusal must name the other route: {err}"
            );
        }
    }
}

/// A named want is a refusal or that backend, never the other one.
///
/// The fallback belongs to `Auto` alone. A gate asked for a container earns a container
/// verdict or none, because a verdict records which backend earned it.
#[test]
fn a_named_want_never_falls_back() {
    let (sandbox, container) = available();
    if let Ok(backend) = Backend::choose(Want::Sandbox, None) {
        assert_eq!(backend.name(), "sandbox");
    }
    if let Ok(backend) = Backend::choose(Want::Container, None) {
        assert_eq!(backend.name(), "container");
    }
    if sandbox && !container {
        assert!(
            Backend::choose(Want::Container, None).is_err(),
            "an absent engine must refuse rather than hand back the sandbox"
        );
    }
    if container && !sandbox {
        assert!(
            Backend::choose(Want::Sandbox, None).is_err(),
            "an absent sandbox must refuse rather than hand back a container"
        );
    }
}

/// The host backend is reachable only by naming it. Isolation is never dropped silently.
#[test]
fn the_host_backend_is_only_reached_by_asking_for_it() {
    assert_eq!(
        Backend::choose(Want::UnsafeHost, None)
            .expect("the host is always available")
            .name(),
        "unsafe-host"
    );
    if let Ok(backend) = Backend::choose(Want::Auto, None) {
        assert_ne!(
            backend.name(),
            "unsafe-host",
            "the automatic choice downgraded to the host"
        );
    }
}

/// A profile names the runtime a verdict was earned against, and the two isolating
/// backends never share one. A record written under either is refused under the other.
#[test]
fn each_backend_reports_a_distinct_profile() {
    let mut seen = Vec::new();
    for want in [Want::Sandbox, Want::Container, Want::UnsafeHost] {
        if let Ok(backend) = Backend::choose(want, None) {
            assert!(!backend.profile().is_empty(), "{:?} has no profile", want);
            seen.push((backend.name(), backend.profile()));
        }
    }
    for (i, (name, profile)) in seen.iter().enumerate() {
        for (other, other_profile) in seen.iter().skip(i + 1) {
            assert_ne!(name, other, "two backends share a name");
            assert_ne!(profile, other_profile, "two backends share a profile");
        }
    }
}
