//! Runtime composition and the agent-facing interface.
//!
//! This crate is where the subsystems meet. It owns no algorithm of its own: the error
//! model comes from `ncui-core`, capability enforcement from `ncui-security`, metrics from
//! `ncui-observability`, scheduling from `ncui-scheduler`, state from `ncui-state`, workers
//! from `ncui-workers`, and framing from `ncui-ipc`. What this crate adds is the thing no
//! single subsystem can provide alone: **a single place where a request enters, is
//! authenticated, is authorized, is rate limited, is audited, and produces a correlated
//! result**.
//!
//! # The agent path
//!
//! ```text
//! agent request
//!    -> authenticate   (bearer token, constant-time)
//!    -> rate limit     (token bucket, per session)
//!    -> authorize      (capability set, destructive gate)
//!    -> dispatch       (method registry)
//!    -> audit          (always, success or failure)
//! ```
//!
//! Every one of those steps is enforced, not merely documented. [`agent::AgentSession`]
//! holds an authentication flag: a session that has not authenticated cannot authorize,
//! and therefore cannot dispatch at all. A request that fails authorization is still
//! audited, because "who tried what" is exactly the question an incident review asks
//! first (ADR-007, ADR-008).
//!
//! The one step a caller performs is authentication itself, because it is the step that
//! depends on a transport. The runtime owns the verification — constant-time comparison
//! against a presented credential — and owns the consequence of not having done it.
//!
//! # What this crate does not do
//!
//! [`rpc`] produces and consumes request and response values; [`transport`] binds them to a
//! loopback socket so an agent in another process can reach the same surface. The socket
//! dispatches through the pipeline above rather than beside it, so the transport adds
//! authentication and framing but no policy of its own.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )
)]

pub mod agent;
pub mod application;
pub mod commands;
pub mod components;
pub mod describe;
pub mod dispatch;
mod dispatch_cargo;
pub mod gpu;
pub mod health;
pub mod icon;
#[cfg(feature = "gpu-islands")]
pub mod islands;
pub mod mcp;
pub mod native;
pub mod page_state;
pub mod rpc;
pub mod transport;
pub mod window;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use ncui_core::{Capability, CapabilitySet, CorrelationId, ErrorKind, Limits, NcError, Result};
use ncui_observability::logring::LogRing;
use ncui_observability::metrics::MetricsRegistry;
use ncui_scheduler::Scheduler;
use ncui_security::audit::AuditLog;
use ncui_security::authz::ConsentToken;
use ncui_security::ratelimit::RateLimiter;
use ncui_security::token::SecretToken;
use ncui_workers::WorkerPool;

use commands::{CommandRegistration, CommandRegistrationError, PageCommands};

/// The runtime version reported by `ncui.describe` and by `ncui status`.
pub const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Every subsystem the runtime composed, held in one place.
///
/// The runtime is shared by reference (`Arc<Runtime>`) rather than cloned, so a request
/// handler cannot accidentally produce a second runtime with its own, divergent state.
pub struct Runtime {
    /// The limits this runtime was started with.
    pub limits: Limits,
    /// Shared metrics.
    pub metrics: Arc<MetricsRegistry>,
    /// Bounded in-memory log ring.
    pub logs: Arc<LogRing>,
    /// Bounded audit trail.
    pub audit: AuditLog,
    /// The cooperative scheduler.
    pub scheduler: Scheduler,
    /// The native worker pool.
    pub workers: Arc<WorkerPool>,
    /// The window surface. Empty until a host is installed.
    pub windows: window::Windows,
    /// The commands a page may call.
    pub page_commands: PageCommands,
    /// The capabilities every page is granted when its window opens.
    ///
    /// Built from the built-in actions at startup. An application adds its own before
    /// registering a command that needs them, and before installing the host.
    page_grant: CapabilitySet,
    /// The GPU surface. Empty until islands are installed.
    pub gpu: gpu::Gpu,
    /// Whether verbose tracing is active.
    ///
    /// Toggled by `ncui.trace.start` / `ncui.trace.stop`; an agent reads the current
    /// value through `ncui.state.inspect`. The flag is the runtime's own record of the
    /// intent; the subscriber that actually emits spans is configured at startup and is
    /// not rebuilt on every toggle.
    pub tracing_active: bool,
    /// Whether profiling is active.
    ///
    /// Toggled by `ncui.profile.start` / `ncui.profile.stop`. While active, the dispatch
    /// pipeline records per-method timing into the metrics registry as a histogram, so an
    /// agent can read where time went. The metrics registry is always recording; this
    /// flag controls the finer-grained per-call instrumentation.
    pub profiling_active: bool,
    /// Whether this runtime may spawn a nested `cargo test` / `cargo bench`.
    ///
    /// Read once from `NCUI_RUN_BENCH` at startup. A nested build is expensive and
    /// environment-mutating, so an embedder opts in explicitly. Tests set the field
    /// directly: mutating the process environment is `unsafe` in edition 2024 and this
    /// workspace forbids `unsafe`.
    pub(crate) bench_allowed: bool,
    /// The request budget the pages spend from.
    ///
    /// Shared by every page in the runtime, for the same reason the agent transport shares
    /// one limiter across connections: a per-page bucket would hand a page a fresh full
    /// budget every time it opened a window, so the documented rate would not be the rate
    /// the pages actually got.
    page_limiter: Arc<Mutex<RateLimiter>>,
    /// When the runtime started.
    ///
    /// The elapsed time from here is the clock the rate limiters are given, so a page's
    /// budget refills on the same timeline an agent's does. It is read from a monotonic
    /// clock, so it cannot go backwards.
    started: std::time::Instant,
    /// Minted consent tokens by nonce. The ledger is the authority: a token whose
    /// nonce is absent here was not minted by this runtime and is refused, however
    /// well-formed its JSON is.
    consent_minted: HashMap<u128, ConsentTokenBinding>,
    /// Nonces already spent. A nonce here authorizes nothing further.
    consent_spent: HashSet<u128>,
}

/// The binding the runtime recorded at mint time for one nonce.
#[derive(Debug, Clone)]
struct ConsentTokenBinding {
    correlation: CorrelationId,
    method: String,
    capability: CapabilitySet,
}

impl Runtime {
    /// Starts a runtime from `limits`.
    ///
    /// Fails rather than silently degrading: incoherent limits or an unavailable worker
    /// pool is reported here, where it can be acted on, instead of surfacing later as
    /// mysterious behaviour.
    ///
    /// # Errors
    ///
    /// Returns [`ncui_core::ErrorKind::Validation`] if `limits` is incoherent, and any
    /// error the metrics registry, log ring, scheduler, audit log or worker pool reports at
    /// construction.
    pub fn start(limits: Limits) -> Result<Self> {
        let metrics = Arc::new(MetricsRegistry::new(limits)?);
        let logs = Arc::new(LogRing::new(limits)?);
        let audit = AuditLog::new(limits.log_ring_capacity)?;
        let scheduler = Scheduler::new(limits.max_worker_queue_depth)?;
        let workers = Arc::new(WorkerPool::new(limits, Arc::clone(&metrics))?);
        let page_limiter = RateLimiter::new(limits.agent_rate_per_second, limits.agent_rate_burst)?;
        let page_commands = PageCommands::new();
        for action in window::SEAM_ACTIONS {
            page_commands
                .register_runtime_method(
                    action.name,
                    action.requires,
                    action.summary,
                    action.method,
                    action.targets_caller_window,
                )
                .map_err(|error| {
                    NcError::new(ErrorKind::Validation {
                        field: "page_commands".to_owned(),
                        reason: error.to_string(),
                    })
                    .operation("runtime::start")
                })?;
        }
        let page_grant = window::default_page_grant(window::SEAM_ACTIONS).granted();

        Ok(Self {
            limits,
            metrics,
            logs,
            audit,
            scheduler,
            workers,
            windows: window::Windows::new(),
            page_commands,
            page_grant,
            gpu: gpu::Gpu::new(),
            tracing_active: false,
            profiling_active: false,
            bench_allowed: std::env::var("NCUI_RUN_BENCH").as_deref() == Ok("1"),
            page_limiter: Arc::new(Mutex::new(page_limiter)),
            started: std::time::Instant::now(),
            consent_minted: HashMap::new(),
            consent_spent: HashSet::new(),
        })
    }

    /// How long the runtime has been running.
    ///
    /// The clock the page pump hands to the rate limiter, so a page's budget refills on a
    /// real timeline rather than a frozen one.
    #[must_use]
    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// The budget the pages spend from.
    #[must_use]
    pub fn page_limiter(&self) -> Arc<Mutex<RateLimiter>> {
        Arc::clone(&self.page_limiter)
    }

    /// The capabilities every new page is granted.
    #[must_use]
    pub fn page_grant(&self) -> CapabilitySet {
        self.page_grant
    }

    /// Adds capabilities to the grant every new page receives.
    ///
    /// This is how an application authorises its own commands. It only adds; a capability
    /// is never removed by this call. Call it before installing the host so every window is
    /// created with the grant in force.
    pub fn grant_page_capabilities(&mut self, capabilities: &[Capability]) {
        for capability in capabilities {
            self.page_grant = self.page_grant.with(*capability);
        }
    }

    /// Registers an application command that pages may invoke.
    ///
    /// The command's required capabilities must already be in the page grant; registering a
    /// command a page could never call is refused rather than advertised and then denied.
    ///
    /// # Errors
    ///
    /// Returns [`CommandRegistrationError`] when the name is invalid or already used, or
    /// when the page grant does not cover `requires`.
    pub fn register_page_command<F>(
        &self,
        name: &str,
        requires: CapabilitySet,
        summary: impl Into<String>,
        handler: F,
    ) -> std::result::Result<CommandRegistration, CommandRegistrationError>
    where
        F: Fn(serde_json::Value) -> std::result::Result<serde_json::Value, ncui_webview::SeamError>
            + Send
            + Sync
            + 'static,
    {
        let missing = CapabilitySet::from_iter_caps(
            requires
                .iter()
                .filter(|capability| !self.page_grant.contains(*capability)),
        );
        if !missing.is_empty() {
            return Err(CommandRegistrationError::PageNotGranted {
                name: name.to_owned(),
                missing,
            });
        }
        self.page_commands
            .register(name, requires, summary, handler)
    }

    /// Installs the window surface, with its registry and its host.
    ///
    /// A runtime without a host refuses every window operation rather than inventing
    /// windows, so the two are installed together: a registry with no factory can never
    /// open a window, and a factory with no registry has nothing to open one in.
    #[must_use]
    pub fn with_windows(mut self, windows: window::Windows) -> Self {
        self.windows = windows;
        self
    }

    /// Installs a real window host, over the origins given.
    ///
    /// The origins are the pages this runtime will load. A page that navigates anywhere
    /// else is refused by the same policy the page's own messages are checked against, so
    /// the allow-list is not a suggestion the engine may ignore.
    ///
    /// Without the `wry-host` feature there is no engine to install, so this is refused
    /// rather than silently doing nothing: a runtime that reported a host it did not have
    /// would be worse than one that says it has none.
    ///
    /// # Errors
    ///
    /// Returns [`ncui_core::ErrorKind::Validation`] when the `wry-host` feature is off.
    #[cfg(feature = "wry-host")]
    pub fn with_host(mut self, allowed_origins: &[ncui_webview::Origin]) -> Result<Self> {
        let actions = self.page_commands.actions();
        self.windows = window::hosted_surface(allowed_origins, &actions, self.page_grant);
        Ok(self)
    }

    /// Installs a real window host, over the origins given.
    ///
    /// # Errors
    ///
    /// Returns [`ncui_core::ErrorKind::Validation`] when this build has no window host.
    #[cfg(not(feature = "wry-host"))]
    pub fn with_host(self, _allowed_origins: &[ncui_webview::Origin]) -> Result<Self> {
        Err(NcError::new(ErrorKind::Validation {
            field: "wry-host".to_owned(),
            reason: "this build has no window host; rebuild with the `wry-host` feature".to_owned(),
        })
        .operation("runtime::with_host"))
    }

    /// Stops the runtime, draining workers.
    ///
    /// Explicit so that a caller which knows it has work to flush can say so, rather than
    /// relying on `Drop` and having no way to express intent.
    pub fn shutdown(&self) {
        self.workers.shutdown();
    }

    /// Mints a [`ConsentToken`] for `method`, bound to `correlation` and `capability`.
    ///
    /// Called **only by the runtime** at user-confirmation time. The nonce is 128
    /// bits of OS entropy. The binding is recorded in the ledger before the token
    /// is returned, so a token that was never minted here is refused even if its
    /// JSON parses.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::Io`] when the OS entropy source is unavailable.
    pub fn mint_consent(
        &mut self,
        correlation: CorrelationId,
        method: &str,
        capability: CapabilitySet,
    ) -> Result<ConsentToken> {
        let secret = SecretToken::from_os_entropy(16)?;
        let bytes = secret.expose_secret();
        let mut nonce: u128 = 0;
        for byte in bytes {
            nonce = (nonce << 8) | u128::from(*byte);
        }
        let token = ConsentToken::mint(correlation, method, capability, nonce);
        self.consent_minted.insert(
            nonce,
            ConsentTokenBinding {
                correlation,
                method: method.to_owned(),
                capability,
            },
        );
        Ok(token)
    }

    /// Validates `token` against `method`/`correlation`/`required` and spends it.
    ///
    /// The nonce must be in the minted ledger, unspent, and bound to the same
    /// correlation, method, and capability the call asserts. The nonce is marked
    /// spent on success, so a second use is refused. The ledger entry is retained
    /// (rather than removed) so replays are distinguishable from forgeries in
    /// diagnostics.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::CapabilityDenied`] when the token was not minted here,
    /// was already spent, or is bound to a different correlation/method/capability.
    pub fn consume_consent(
        &mut self,
        token: &ConsentToken,
        correlation: CorrelationId,
        method: &str,
        required: CapabilitySet,
    ) -> Result<()> {
        let nonce = token.nonce();
        if self.consent_spent.contains(&nonce) {
            return Err(NcError::new(ErrorKind::CapabilityDenied {
                capability: "consent_token".to_owned(),
                subject: "agent".to_owned(),
            })
            .operation("runtime::consume_consent"));
        }
        let Some(binding) = self.consent_minted.get(&nonce) else {
            return Err(NcError::new(ErrorKind::CapabilityDenied {
                capability: "consent_token".to_owned(),
                subject: "agent".to_owned(),
            })
            .operation("runtime::consume_consent"));
        };
        let bound = binding.correlation == correlation
            && binding.method == method
            && required.is_subset_of(binding.capability)
            && token.matches(correlation, method, required);
        if !bound {
            return Err(NcError::new(ErrorKind::CapabilityDenied {
                capability: "consent_token".to_owned(),
                subject: "agent".to_owned(),
            })
            .operation("runtime::consume_consent"));
        }
        self.consent_spent.insert(nonce);
        Ok(())
    }
}

impl std::fmt::Debug for Runtime {
    /// Reports the runtime's observable shape rather than dumping every subsystem, which
    /// would itself stall a pool being inspected.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("version", &RUNTIME_VERSION)
            .field("workers", &self.workers.worker_count())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::LimitsBuilder;

    #[test]
    fn a_runtime_starts_with_the_limits_it_was_given() {
        let limits = LimitsBuilder::new()
            .max_workers(2)
            .max_worker_queue_depth(4)
            .build()
            .expect("limits");
        let runtime = Runtime::start(limits).expect("runtime should start");
        assert_eq!(runtime.limits.max_workers, 2);
        assert_eq!(runtime.workers.worker_count(), 2);
        runtime.shutdown();
    }

    #[test]
    fn incoherent_limits_are_refused_rather_than_clamped() {
        let broken = Limits {
            request_timeout_ms: 0,
            ..Limits::default()
        };
        let err = Runtime::start(broken).expect_err("incoherent limits must be refused");
        assert!(err.to_string().contains("request_timeout_ms"), "got: {err}");
    }

    #[test]
    fn runtime_debug_reports_shape_without_dumping_subsystems() {
        let runtime = Runtime::start(Limits::default()).expect("runtime");
        let text = format!("{runtime:?}");
        assert!(text.contains("Runtime"), "got: {text}");
        assert!(text.contains("workers"), "got: {text}");
    }
}
