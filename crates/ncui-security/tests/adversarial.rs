//! Adversarial tests across the security crate's public surface.
//!
//! Each test attacks a guard from the direction a hostile caller would use. These are the
//! tests a security review actually rests on: not "does the happy path work" but "does the
//! guard hold when someone pushes".

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use std::path::PathBuf;
use std::time::Duration;

use ncui_core::{Capability, CapabilitySet};
use ncui_security::audit::{AuditEvent, AuditLog, AuditOutcome};
use ncui_security::authz::{AgentRole, Authorizer};
use ncui_security::path::SafePath;
use ncui_security::ratelimit::RateLimiter;
use ncui_security::token::{SecretToken, constant_time_eq};

/// A deterministic xorshift PRNG, so a failure is reproducible rather than a one-off.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn the_role_ladder_is_monotonic_and_tops_out_at_everything() {
    // The ladder is the whole agent-facing authority model. A role granting more than the
    // administrator would make every role check meaningless.
    let mut previous = CapabilitySet::empty();
    for role in AgentRole::ALL {
        let grants = role.capabilities();
        assert!(
            previous.is_subset_of(grants),
            "{role:?} grants less than the role below it, so the ladder is not monotonic"
        );
        previous = grants;
    }
    assert_eq!(
        AgentRole::Administrator.capabilities(),
        CapabilitySet::all(),
        "the administrator must hold everything, or the top of the ladder is a fiction"
    );
}

#[test]
fn a_destructive_capability_never_acts_without_consent() {
    // Each destructive capability is checked against the roles that actually hold it: a
    // role holding `filesystem.write` does not hold `delete.data`, so testing them apart
    // would assert against a grant nobody has.
    let destructive = [
        Capability::FilesystemWrite,
        Capability::ProcessSpawn,
        Capability::DeleteData,
        Capability::Deploy,
    ];

    for capability in destructive {
        let required = CapabilitySet::empty().with(capability);
        let holders: Vec<AgentRole> = AgentRole::ALL
            .iter()
            .copied()
            .filter(|role| role.capabilities().contains(capability))
            .collect();
        assert!(
            !holders.is_empty(),
            "no role holds {capability}, so this capability is unreachable"
        );

        for role in holders {
            let grants = role.capabilities();
            // Holding it is necessary...
            assert!(Authorizer.authorize("agent", required, grants).is_ok());
            // ...but never sufficient on its own.
            assert!(
                Authorizer
                    .require_destructive("agent", required, grants, false)
                    .is_err(),
                "{role:?} acted on {capability} without consent"
            );
            assert!(
                Authorizer
                    .require_destructive("agent", required, grants, true)
                    .is_ok()
            );
        }
    }
}

#[test]
fn no_generated_traversal_payload_escapes_its_root() {
    // The single most important property of `SafePath`: whatever the input, the answer is
    // either a path inside the root or an error — never an escaping path.
    let root = if cfg!(windows) {
        PathBuf::from("C:\\root")
    } else {
        PathBuf::from("/root")
    };
    // An alphabet chosen from the pieces that make traversal work: separators on both
    // styles, `..`, drive letters, a NUL byte, a bidi override and a non-ASCII name.
    let alphabet = [
        "..",
        "/",
        "\\",
        ".",
        "a",
        "C:",
        "\\0",
        "\\u{202e}",
        "\\u{00e9}",
        "//",
        ":",
        "~",
    ];
    let mut rng = Rng(0x5EED_1234);

    for _ in 0..20_000 {
        let length = (rng.next() % 6) as usize;
        let mut payload = String::new();
        for _ in 0..length {
            payload.push_str(alphabet[(rng.next() as usize) % alphabet.len()]);
        }
        if let Ok(resolved) = SafePath::resolve(&root, &payload)
            && !SafePath::is_contained(&root, &resolved)
        {
            panic!("resolve escaped the root for payload {payload:?}: {resolved:?}");
        }
    }
}

#[test]
fn a_sibling_whose_name_extends_the_root_is_not_inside_it() {
    // The classic mistake: `C:\workevil`.starts_with(`C:\work`) is true, so a
    // string-prefix check would accept a path that is outside the root.
    let (root, sibling) = if cfg!(windows) {
        (PathBuf::from("C:\\work"), PathBuf::from("C:\\workevil"))
    } else {
        (PathBuf::from("/work"), PathBuf::from("/workevil"))
    };
    assert!(
        sibling
            .to_string_lossy()
            .starts_with(root.to_string_lossy().as_ref()),
        "the test premise requires the string-prefix check to be fooled here"
    );
    assert!(
        !SafePath::is_contained(&root, &sibling),
        "a string-prefix check would wrongly accept this"
    );
}

#[test]
fn secrets_never_render_their_contents() {
    let token = SecretToken::new(vec![0xAB; 32]).expect("token");
    for rendered in [format!("{token:?}"), format!("{token}")] {
        assert!(!rendered.contains("ab"), "a secret leaked into: {rendered}");
        assert!(
            rendered.contains("redacted") || rendered.contains("SecretToken"),
            "the rendering must still say what it is: {rendered}"
        );
    }
}

#[test]
fn comparison_detects_every_single_bit_difference() {
    let base = vec![0u8; 32];
    for bit in 0..32 * 8 {
        let mut other = base.clone();
        other[bit / 8] ^= 1 << (bit % 8);
        assert!(
            !constant_time_eq(&base, &other),
            "a difference in bit {bit} went undetected"
        );
    }
    assert!(constant_time_eq(&base, &base));
    assert!(!constant_time_eq(&base, &[0u8; 16]), "a length mismatch");
}

#[test]
fn the_rate_limiter_cannot_be_starved_by_frequent_polling() {
    // A limiter that banks its balance only on success starves every client that polls at
    // the limit's own frequency, which is the most common way to implement one by mistake.
    let mut limiter = RateLimiter::new(10, 10).expect("limiter");
    let mut granted = 0u32;
    for tick in 0..20u64 {
        let now = Duration::from_millis(tick * 100);
        if limiter.try_acquire(now, 1).is_ok() {
            granted += 1;
        }
    }
    assert!(granted > 1, "the limiter starved a polling client entirely");
}

#[test]
fn an_audit_trail_that_dropped_records_says_so() {
    // A truncated audit log presented as complete is the dangerous case: it answers "who
    // did this" with a confident "nobody".
    let mut log = AuditLog::new(4).expect("log");
    let correlation = ncui_core::CorrelationId::new();
    for index in 0..10 {
        log.record(
            AuditEvent::new(
                std::time::SystemTime::now(),
                "agent-1",
                correlation,
                format!("action-{index}"),
                "target",
                AuditOutcome::Allowed,
                CapabilitySet::empty(),
            )
            .expect("event"),
        );
    }
    assert_eq!(log.len(), 4, "the ring must stay bounded");
    assert!(
        log.evicted() > 0,
        "eviction must be visible, or a truncated log reads as a complete one"
    );
}

#[test]
fn an_anonymous_denial_is_refused_because_it_cannot_be_attributed() {
    let required = CapabilitySet::empty();
    let err = Authorizer
        .authorize(
            "   ",
            required,
            CapabilitySet::empty().with(Capability::Gpu),
        )
        .expect_err("an anonymous subject must be refused");
    assert!(
        err.to_string().to_lowercase().contains("cannot be audited"),
        "the reason must say why attribution is required: {err}"
    );
    assert!(
        Authorizer
            .authorize(
                "agent",
                required,
                CapabilitySet::empty().with(Capability::Gpu)
            )
            .is_ok(),
        "a named subject with no requirements is allowed"
    );
}
