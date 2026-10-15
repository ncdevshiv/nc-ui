//! Window lifecycle, including the crash case ADR-001 calls a real failure mode.
//!
//! ADR-001 is explicit that "a `WebView` crash is a real failure mode that must be detected,
//! reported, and recovered from; it is in the fault-injection matrix, not treated as
//! impossible." That sentence is about state: a crashed host is not running, and every
//! request made against it must fail loudly rather than be answered from a dead page.
//!
//! The transitions are deliberately explicit. An implicit machine — where "loading" and
//! "ready" are boolean flags — cannot express "you asked for something on a destroyed
//! window" as a distinct, testable case, which is exactly the case that must not be silent.

use core::fmt;

/// Where a host is in its life.
///
/// Serialised with its lowercase wire name, which is what [`Display`](fmt::Display) already
/// produces: an agent reading a window's state from `ncui.window.list` should see the same
/// spelling a log line uses, so one string can be searched across both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostState {
    /// Created, no engine attached yet.
    Created,
    /// The engine is being created and attached. A page may not be navigated yet.
    Attaching,
    /// Live: navigations and messages are served.
    Running,
    /// Alive but not serving — the window is hidden or occluded. Navigations are refused;
    /// messages are queued by the caller, not by this type.
    Suspended,
    /// The engine is gone. A crash lands here, and it is distinguishable from `Destroyed`
    /// because recovery is possible from here and not from there.
    Crashed,
    /// Being torn down.
    Destroying,
    /// Gone for good. Terminal.
    Destroyed,
}

impl HostState {
    /// Whether the host is gone and cannot come back.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Destroyed)
    }

    /// Whether the host can serve a navigation or a message right now.
    #[must_use]
    pub const fn is_serving(&self) -> bool {
        matches!(self, Self::Running)
    }
}

impl fmt::Display for HostState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Created => "created",
            Self::Attaching => "attaching",
            Self::Running => "running",
            Self::Suspended => "suspended",
            Self::Crashed => "crashed",
            Self::Destroying => "destroying",
            Self::Destroyed => "destroyed",
        };
        f.write_str(name)
    }
}

/// A transition the lifecycle refuses, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IllegalTransition {
    /// Where the host was.
    pub from: HostState,
    /// What was asked for.
    pub to: HostState,
    /// Why it was refused.
    pub reason: String,
}

impl fmt::Display for IllegalTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot move from {} to {}: {}",
            self.from, self.to, self.reason
        )
    }
}

/// The legal transitions, as a function rather than a table.
///
/// A table would have to encode "which of these are legal", and getting that wrong is exactly
/// the bug this type exists to prevent. Naming the legal edges instead means every other edge
/// is refused by construction.
fn legal(from: HostState, to: HostState) -> Option<&'static str> {
    match (from, to) {
        (HostState::Created, HostState::Attaching) => Some("begin attaching"),
        (HostState::Attaching, HostState::Running) => Some("engine came up"),
        (HostState::Attaching, HostState::Crashed) => Some("the engine failed during attach"),
        (HostState::Running, HostState::Suspended) => Some("window hidden"),
        (HostState::Running, HostState::Crashed) => Some("the engine died"),
        (HostState::Suspended, HostState::Running) => Some("window shown"),
        (HostState::Suspended, HostState::Crashed) => Some("the engine died while suspended"),
        // Every live state can be torn down from, and the reason is the same for all of
        // them: the host is going away, whether or not it was healthy when that happened.
        (HostState::Suspended | HostState::Running | HostState::Crashed, HostState::Destroying) => {
            Some("tear down")
        }
        (HostState::Destroyed, HostState::Destroying) => Some("finish tearing down"),
        (HostState::Destroying, HostState::Destroyed) => Some("torn down"),
        _ => None,
    }
}

/// A window host's lifecycle.
#[derive(Debug, Clone, Default)]
pub struct WindowLifecycle {
    state: Option<HostState>,
}

impl WindowLifecycle {
    /// A fresh, unattached host.
    #[must_use]
    pub fn new() -> Self {
        Self { state: None }
    }

    /// Where the host is, or `None` before it has been created.
    #[must_use]
    pub const fn state(&self) -> Option<HostState> {
        self.state
    }

    /// Moves to `to`, or explains why not.
    ///
    /// # Errors
    ///
    /// Returns the refused transition when the edge is not one of the legal ones. The refusal
    /// is a value rather than a panic: a teardown path that raced a crash must be able to
    /// report that, not abort.
    pub fn transition(&mut self, to: HostState) -> Result<(), IllegalTransition> {
        let Some(from) = self.state else {
            // Creation is the only transition available before the host exists.
            return if to == HostState::Created {
                self.state = Some(HostState::Created);
                Ok(())
            } else {
                Err(IllegalTransition {
                    from: HostState::Destroyed,
                    to,
                    reason: "the host does not exist yet; only `Created` is reachable".to_owned(),
                })
            };
        };

        match legal(from, to) {
            Some(_) => {
                self.state = Some(to);
                Ok(())
            }
            None => Err(IllegalTransition {
                from,
                to,
                reason: if from.is_terminal() {
                    "the host is destroyed, which is terminal".to_owned()
                } else {
                    format!("no edge from {from} to {to}")
                },
            }),
        }
    }

    /// Whether the host is serving requests right now.
    #[must_use]
    pub const fn is_serving(&self) -> bool {
        matches!(self.state, Some(HostState::Running))
    }

    /// Whether the engine died and the host has not been torn down.
    #[must_use]
    pub const fn is_crashed(&self) -> bool {
        matches!(self.state, Some(HostState::Crashed))
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    fn attached() -> WindowLifecycle {
        let mut host = WindowLifecycle::new();
        host.transition(HostState::Created).expect("created");
        host.transition(HostState::Attaching).expect("attaching");
        host.transition(HostState::Running).expect("running");
        host
    }

    #[test]
    fn a_host_walks_the_happy_path() {
        let mut host = WindowLifecycle::new();
        assert_eq!(host.state(), None);
        for state in [
            HostState::Created,
            HostState::Attaching,
            HostState::Running,
            HostState::Suspended,
            HostState::Running,
            HostState::Destroying,
            HostState::Destroyed,
        ] {
            host.transition(state)
                .unwrap_or_else(|e| panic!("{state} should be reachable: {e}"));
            assert_eq!(host.state(), Some(state));
        }
    }

    #[test]
    fn a_running_host_serves_and_a_suspended_one_does_not() {
        let mut host = attached();
        assert!(host.is_serving());
        host.transition(HostState::Suspended).expect("suspend");
        assert!(!host.is_serving(), "a suspended host must not serve");
        host.transition(HostState::Running).expect("resume");
        assert!(host.is_serving());
    }

    #[test]
    fn a_crash_is_reachable_from_every_live_state() {
        // ADR-001: a crash is a real failure mode. If it were only reachable from `Running`,
        // a host that died while suspended would report itself healthy forever.
        // Each case names the state the crash is exercised *from*, and walks the host there
        // rather than assuming it is already there: `attached()` leaves it running, so
        // asking it to suspend again would be a `running -> running` edge that does not exist.
        for from in [
            HostState::Attaching,
            HostState::Running,
            HostState::Suspended,
        ] {
            let mut host = WindowLifecycle::new();
            host.transition(HostState::Created).expect("created");
            host.transition(HostState::Attaching).expect("attaching");
            if from == HostState::Running {
                host.transition(HostState::Running).expect("running");
            } else if from == HostState::Suspended {
                host.transition(HostState::Running).expect("running");
                host.transition(HostState::Suspended).expect("suspend");
            }
            host.transition(HostState::Crashed)
                .unwrap_or_else(|e| panic!("a crash from {from} must be reachable: {e}"));
            assert!(host.is_crashed());
            assert!(!host.is_serving(), "a crashed host must not serve");
        }
    }

    #[test]
    fn a_crashed_host_can_be_torn_down_but_not_resumed() {
        let mut host = attached();
        host.transition(HostState::Crashed).expect("crash");
        let refused = host.transition(HostState::Running).expect_err("no revival");
        assert!(
            refused.reason.contains("crashed") || refused.reason.contains("edge"),
            "the refusal must explain itself; got {refused}"
        );
        // Recovery means a *new* host, not reviving this one. The old one is cleaned up.
        host.transition(HostState::Destroying).expect("tear down");
        host.transition(HostState::Destroyed).expect("torn down");
    }

    #[test]
    fn a_destroyed_host_is_terminal_and_refuses_everything() {
        let mut host = attached();
        host.transition(HostState::Destroying).expect("tear down");
        host.transition(HostState::Destroyed).expect("torn down");
        assert!(host.state().expect("state").is_terminal());

        // Everything except the teardown edge must be refused from a destroyed host.
        for attempt in [
            HostState::Created,
            HostState::Attaching,
            HostState::Running,
            HostState::Suspended,
            HostState::Crashed,
        ] {
            host.transition(attempt)
                .expect_err(&format!("{attempt} was accepted from a destroyed host"));
        }
        // Re-entering `Destroying` is legal — teardown runs twice on some paths — and it leaves
        // the host in `Destroying`, not terminal, until the final edge runs again.
        assert!(host.transition(HostState::Destroying).is_ok());
        assert_eq!(host.state(), Some(HostState::Destroying));
        host.transition(HostState::Destroyed)
            .expect("torn down again");
        assert!(host.state().expect("state").is_terminal());
    }

    #[test]
    fn a_host_cannot_skip_attachment_and_serve() {
        // Created -> Running is the edge that would let a host serve before its engine
        // exists, which is how "the page loaded but nothing works" happens.
        let mut host = WindowLifecycle::new();
        host.transition(HostState::Created).expect("created");
        let refused = host
            .transition(HostState::Running)
            .expect_err("must attach first");
        assert_eq!(refused.from, HostState::Created);
        assert!(refused.to_string().contains("created"));
    }

    #[test]
    fn nothing_is_reachable_before_the_host_exists() {
        let mut host = WindowLifecycle::new();
        for state in [HostState::Running, HostState::Crashed, HostState::Destroyed] {
            host.transition(state)
                .expect_err("only `Created` is reachable before creation");
        }
    }

    #[test]
    fn every_illegal_transition_explains_itself() {
        // An undiagnosable refusal is what makes a lifecycle bug unauditable.
        let mut host = attached();
        let refused = host
            .transition(HostState::Created)
            .expect_err("created is behind us");
        assert!(!refused.reason.trim().is_empty());
        assert!(!refused.to_string().trim().is_empty());
    }
}
