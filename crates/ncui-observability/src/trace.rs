//! Correlation propagation (spec 50).
//!
//! One user action must be traceable end to end. In a runtime that spans threads, IPC
//! boundaries and native workers, "traceable" means every log line, span, metric and
//! error produced while handling that action carries the *same* correlation identifier,
//! so an operator can start from a symptom and arrive at the stage that caused it.
//!
//! # How the identifier travels
//!
//! [`TraceContext`] is a thin wrapper around [`ncui_core::CorrelationId`] whose job is to make the
//! identifier *ambient*: entering a scope publishes the identifier to the current
//! [`tracing::Span`] under [`CORRELATION_FIELD`], and [`TraceContext::current`] reads it
//! back out. Code further down the call stack therefore does not need the identifier
//! threaded through its parameters — it asks for the current context.
//!
//! ```no_run
//! use ncui_observability::{trace::operation_span, trace::TraceContext};
//!
//! # fn main() {
//! let ctx = TraceContext::new();
//! let span = operation_span(&ctx, "ipc");
//! let _guard = span.enter();
//!
//! // A stage several frames down, with no access to `ctx`:
//! let inner = TraceContext::current().expect("the scope published a correlation id");
//! assert_eq!(inner.correlation(), ctx.correlation());
//! # }
//! ```
//!
//! # Why a thread-local and not only a span field
//!
//! The identifier is published twice, for two different reasons:
//!
//! * Onto the span, so that a backend which understands spans (`tracing-subscriber`'s
//!   JSON formatter, or any OpenTelemetry bridge) sees the identifier in its own metadata
//!   and can stitch the tree together without our help.
//! * Onto a thread-local stack, so [`TraceContext::current`] can answer synchronously.
//!   Reading a field back out of a span requires a `Subscriber` to intercept the value,
//!   which `tracing` does not expose for an arbitrary already-active span; a span's field
//!   values are write-only from the caller's side.
//!
//! Keeping both means the identifier survives for backends that only understand spans and
//! for code that only wants to ask "what am I part of?". The stack is unwound by
//! [`TraceScope`]'s `Drop`, so an early return or an `?` that propagates an error cannot
//! leave a stale identifier visible to unrelated work on the same thread.

use core::fmt;
use std::cell::RefCell;

use ncui_core::CorrelationId;
use tracing::Span;

use crate::CORRELATION_FIELD;

/// The ambient correlation identifier of the logical operation in progress.
///
/// `TraceContext` is `Copy`: it is a single 128-bit identifier, and copying it is both
/// cheaper and clearer than sharing a reference to a wrapper around one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceContext {
    correlation: CorrelationId,
}

impl TraceContext {
    /// Starts a new logical operation with a freshly generated correlation identifier.
    ///
    /// Call this once, at the boundary where a user action enters the runtime (an IPC
    /// request, an agent command, a UI event). Every downstream stage should derive its
    /// context from this one rather than minting a new identifier, because two
    /// identifiers mean two traces where the operator expects one.
    #[must_use]
    pub fn new() -> Self {
        Self {
            correlation: CorrelationId::new(),
        }
    }

    /// Continues a logical operation under an identifier received from elsewhere.
    ///
    /// Used at a trust boundary: when a request arrives over IPC carrying its own
    /// correlation id, the runtime adopts that id instead of generating a new one, so
    /// the caller's trace and the runtime's trace are the same trace.
    #[must_use]
    pub const fn with_correlation(correlation: CorrelationId) -> Self {
        Self { correlation }
    }

    /// The identifier carried by this context.
    #[must_use]
    pub const fn correlation(&self) -> CorrelationId {
        self.correlation
    }

    /// Publishes this context to the current thread and enters a traced scope.
    ///
    /// The returned [`TraceScope`] pops the context again when dropped, so scopes nest
    /// correctly and an early return cannot leak an identifier into unrelated work.
    /// Prefer [`crate::trace::operation`] when a `ncui.operation` span is wanted too; this
    /// entry point exists for attaching to a span that already exists.
    ///
    /// Records the identifier onto [`Span::current`] as a side effect. `tracing` drops
    /// values recorded for fields the span did not declare, which is why
    /// [`crate::trace::operation_span`] declares [`CORRELATION_FIELD`]; a scope entered outside
    /// such a span simply carries no visible field on it, but still publishes to the
    /// thread-local so [`TraceContext::current`] works.
    pub fn enter(self) -> TraceScope {
        record_on_current_span(self.correlation);
        push_context(self);
        TraceScope { entered_span: None }
    }

    /// The correlation identifier currently in scope on this thread.
    ///
    /// Returns `None` when no scope has been entered. Callers that only want to tag an
    /// error or a metric treat `None` as "not part of a correlated operation" rather than
    /// inventing an identifier, because a fabricated id would join unrelated records into
    /// one trace and send an operator down a false path.
    #[must_use]
    pub fn current() -> Option<Self> {
        CONTEXT_STACK.with(|stack| stack.borrow().last().copied())
    }

    /// The correlation identifier currently in scope, or a freshly generated one.
    ///
    /// For call sites that cannot proceed uncorrelated — the entry point of a subsystem
    /// that may be either the origin or a participant. Returns a [`TraceScope`] that
    /// publishes the identifier if it had to mint one, so the answer is always part of
    /// the ambient context afterwards.
    pub fn current_or_new() -> TraceScope {
        match Self::current() {
            Some(context) => context.enter(),
            None => Self::new().enter(),
        }
    }
}

impl Default for TraceContext {
    /// Equivalent to [`TraceContext::new`]: a context with no parent starts a new
    /// correlation, which is the only correct behaviour for a default.
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TraceContext {
    /// Renders the 32-character hex correlation identifier, matching
    /// [`ncui_core::CorrelationId`]'s own `Display` so the two are interchangeable in messages.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.correlation)
    }
}

/// The guard returned by [`TraceContext::enter`], [`crate::trace::operation`] and
/// [`TraceContext::current_or_new`].
///
/// Dropping it restores the enclosing context and, if it entered a span, leaves that span.
/// It is deliberately not `Clone`: a duplicated guard would pop the same context twice and
/// corrupt the stack for the rest of the thread.
#[derive(Debug)]
pub struct TraceScope {
    entered_span: Option<tracing::span::EnteredSpan>,
}

impl TraceScope {
    /// Exits the underlying span, if this scope entered one.
    ///
    /// Normally unnecessary — dropping the guard does the same thing. Provided so that a
    /// long-running loop can bound the span's duration explicitly rather than leaving the
    /// span entered across an await point or across a `join`.
    pub fn exit(mut self) {
        drop(self.entered_span.take());
    }
}

impl Drop for TraceScope {
    fn drop(&mut self) {
        // Order matters: the span is exited first so that the final event inside it is
        // still attributed to the operation, then the context is popped so that anything
        // running after this scope does not inherit the correlation identifier.
        self.entered_span.take();
        pop_context();
    }
}

// The per-thread stack of contexts in scope.
//
// A stack rather than a single slot because scopes nest: an inner operation that runs
// under an outer request must not erase the outer request's identifier when it exits.
thread_local! {
    static CONTEXT_STACK: RefCell<Vec<TraceContext>> = const { RefCell::new(Vec::new()) };
}

fn push_context(context: TraceContext) {
    CONTEXT_STACK.with(|stack| stack.borrow_mut().push(context));
}

fn pop_context() {
    CONTEXT_STACK.with(|stack| {
        // Popping a mismatched depth means a guard was dropped out of order, which cannot
        // happen through the safe API (guards are not `Clone`, and `Drop` runs once).
        // Saturating rather than panicking keeps the library panic-free while leaving the
        // stack consistent for subsequent scopes.
        stack.borrow_mut().pop();
    });
}

/// Records the correlation identifier onto the current span under
/// [`CORRELATION_FIELD`].
fn record_on_current_span(correlation: CorrelationId) {
    // `record_debug` is used rather than `record_u128` because the identifier is
    // meaningful as its 32-character hex form in a log line; recording the raw `u128`
    // would render as a decimal number that cannot be matched against an `NcError`.
    Span::current().record(CORRELATION_FIELD, tracing::field::display(correlation));
}

/// Creates the span for one traced operation, carrying the correlation identifier.
///
/// Every stage of a single user action should sit under a span created here. The span
/// name is `OPERATION_SPAN_NAME` and it declares both [`CORRELATION_FIELD`] and
/// `COMPONENT_FIELD`, which is what makes the recorded identifier visible to a span-aware
/// backend.
#[must_use]
pub fn operation_span(context: &TraceContext, component: &str) -> Span {
    // `info_span!` rather than `tracing::span!` (which has no level): the level must be a
    // literal for the `LevelFilter` call-site optimization, and `info` is the highest verb
    // needed for a span to be recorded by every mode's default filter. Individual events
    // inside choose their own level.
    tracing::info_span!(
        target: "ncui.operation",
        "ncui.operation",
        correlation = %context.correlation(),
        component = component,
    )
}

/// Creates and enters the span for one traced operation, publishing the context.
///
/// This is the entry point subsystems should use: it establishes both the ambient
/// context ([`TraceContext::current`]) and the span in a single call, so a stage cannot
/// accidentally do one and forget the other.
#[must_use]
pub fn operation(context: &TraceContext, component: &str) -> TraceScope {
    // `Span::entered` rather than `Span::enter`: it yields an `EnteredSpan` that owns the
    // span, so the guard can be stored without borrowing a local. `EnteredSpan` is
    // deliberately `!Send`, which is the correct constraint here — a trace scope belongs to
    // the thread that entered it, and moving one across a thread boundary would leave the
    // originating thread's span stack unbalanced.
    let entered = operation_span(context, component).entered();
    record_on_current_span(context.correlation());
    push_context(*context);
    TraceScope {
        entered_span: Some(entered),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::CorrelationId;
    use std::fmt;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::Registry;

    /// A layer that records the correlation value seen on every span field set, so a test
    /// can assert what actually reached the backend rather than what the helper returned.
    #[derive(Debug, Default)]
    struct CorrelationLayer {
        seen: Arc<Mutex<Vec<Option<CorrelationId>>>>,
    }

    /// A [`Visit`] that captures the first [`CORRELATION_FIELD`] value it is shown.
    ///
    /// `tracing` calls exactly one of the typed `record_*` methods per value depending on
    /// which [`tracing::field::Value`] implementation produced it. `operation_span` and
    /// `record_on_current_span` both record through [`tracing::field::display`], which
    /// arrives as `record_debug`, so that is the arm that matters; the others are implemented
    /// so that a caller who records the raw `u128` or the hex string directly is read
    /// correctly too.
    ///
    /// This lives in the test module because observing a span's recorded values requires a
    /// `Visitor`, and `tracing` offers no public way to attach one to an arbitrary
    /// already-created span: `Span::with_subscriber` hands back only the span's `Id` and its
    /// dispatch, and the field values are write-only from the caller's side. A real backend
    /// therefore observes the identifier when the span is *created* — through a `Layer`'s
    /// `on_new_span`, exactly as `CorrelationLayer` does — and this type is the companion
    /// that makes that observation possible.
    #[derive(Debug, Default)]
    struct CorrelationVisitor {
        correlation: Option<CorrelationId>,
    }

    impl CorrelationVisitor {
        fn accept(&mut self, raw: &str) {
            if self.correlation.is_none() {
                self.correlation = raw.trim().parse().ok();
            }
        }
    }

    impl Visit for CorrelationVisitor {
        fn record_u128(&mut self, field: &Field, value: u128) {
            if field.name() == CORRELATION_FIELD {
                self.correlation = CorrelationId::from_raw(value);
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            if field.name() == CORRELATION_FIELD {
                // `display(...)` wraps the value in a `DisplayValue`, whose `Debug` rendering
                // is the `Display` output — the hex string, unquoted. Quoting is tolerated so
                // that a caller who records a `&str` is read correctly too.
                self.accept(format!("{value:?}").trim_matches('"'));
            }
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == CORRELATION_FIELD {
                self.accept(value);
            }
        }
    }

    impl<S> Layer<S> for CorrelationLayer
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: Context<'_, S>,
        ) {
            let mut visitor = CorrelationVisitor::default();
            attrs.record(&mut visitor);
            self.seen
                .lock()
                .expect("test lock")
                .push(visitor.correlation);
        }
    }

    #[test]
    fn a_new_context_has_a_fresh_non_zero_identifier() {
        let first = TraceContext::new();
        let second = TraceContext::new();
        assert_ne!(first.correlation(), second.correlation());
        assert_ne!(first.correlation().as_raw(), 0);
    }

    #[test]
    fn contexts_are_equal_when_they_carry_the_same_identifier() {
        let id = CorrelationId::new();
        assert_eq!(
            TraceContext::with_correlation(id),
            TraceContext::with_correlation(id)
        );
        assert_ne!(
            TraceContext::with_correlation(id),
            TraceContext::with_correlation(CorrelationId::new())
        );
    }

    #[test]
    fn current_is_absent_before_any_scope_is_entered() {
        // Nothing has been entered on this thread yet, so there is nothing to correlate
        // with. Fabricating an identifier here would be the failure mode this API exists
        // to avoid.
        assert_eq!(TraceContext::current(), None);
    }

    #[test]
    fn current_reports_the_innermost_context_while_a_scope_is_entered() {
        let outer = TraceContext::new();
        let inner = TraceContext::new();
        {
            let _outer_scope = outer.enter();
            assert_eq!(TraceContext::current(), Some(outer));
            {
                let _inner_scope = inner.enter();
                assert_eq!(TraceContext::current(), Some(inner));
            }
            // Exiting the inner scope restores the outer one rather than clearing it:
            // scopes nest, and an inner stage must not erase its caller's context.
            assert_eq!(TraceContext::current(), Some(outer));
        }
        assert_eq!(TraceContext::current(), None);
    }

    #[test]
    fn current_or_new_mints_a_context_when_none_is_in_scope() {
        let scope = TraceContext::current_or_new();
        let minted = TraceContext::current().expect("a context is now in scope");
        assert_eq!(minted, scope_context_under_test());
        drop(scope);
        assert_eq!(TraceContext::current(), None);
    }

    #[test]
    fn current_or_new_reuses_the_in_scope_context_when_one_exists() {
        let existing = TraceContext::new();
        let outer = existing.enter();
        {
            let _inner = TraceContext::current_or_new();
            assert_eq!(TraceContext::current(), Some(existing));
        }
        drop(outer);
    }

    /// Helper that captures the identifier of the currently entered scope.
    fn scope_context_under_test() -> TraceContext {
        TraceContext::current().expect("context in scope")
    }

    #[test]
    fn the_identifier_reaches_the_backend_as_a_span_field() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber = Registry::default().with(CorrelationLayer {
            seen: Arc::clone(&seen),
        });

        let context = TraceContext::new();
        tracing::subscriber::with_default(subscriber, || {
            let span = operation_span(&context, "ipc");
            let _entered = span.enter();
            let _scope = context.enter();
        });

        let recorded = seen.lock().expect("test lock").clone();
        assert_eq!(
            recorded,
            vec![Some(context.correlation())],
            "the operation span must carry the correlation identifier to the backend"
        );
    }

    #[test]
    fn a_context_is_readable_from_inside_an_entered_operation_scope() {
        let context = TraceContext::new();
        let scope = operation(&context, "workers");
        assert_eq!(
            TraceContext::current(),
            Some(context),
            "an entered operation scope must publish the context"
        );
        drop(scope);
        assert_eq!(TraceContext::current(), None);
    }

    #[test]
    fn the_entered_operation_scope_also_reaches_the_backend_as_a_span_field() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber = Registry::default().with(CorrelationLayer {
            seen: Arc::clone(&seen),
        });

        let context = TraceContext::new();
        tracing::subscriber::with_default(subscriber, || {
            let _scope = operation(&context, "state");
        });

        let recorded = seen.lock().expect("test lock").clone();
        assert_eq!(
            recorded,
            vec![Some(context.correlation())],
            "the scope's span must carry the identifier, not only the thread-local"
        );
    }

    #[test]
    fn an_unrelated_span_carries_no_correlation_and_does_not_break_the_stack() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber = Registry::default().with(CorrelationLayer {
            seen: Arc::clone(&seen),
        });
        let context = TraceContext::new();
        tracing::subscriber::with_default(subscriber, || {
            let _unrelated = tracing::info_span!("unrelated").entered();
            // Entering an unrelated span must not fabricate a correlation.
            assert_eq!(TraceContext::current(), None);
            let _scope = operation(&context, "state");
            assert_eq!(TraceContext::current(), Some(context));
        });
    }

    #[test]
    fn a_correlation_identifier_survives_the_scope_guard_being_dropped_out_of_order() {
        let context = TraceContext::new();
        let outer = context.enter();
        {
            let _inner = TraceContext::current_or_new();
        }
        drop(outer);
        assert_eq!(TraceContext::current(), None);
    }

    #[test]
    fn display_matches_the_identifier_rendered_by_core() {
        let context = TraceContext::new();
        assert_eq!(context.to_string(), context.correlation().to_string());
        assert_eq!(context.to_string().len(), 32);
    }

    #[test]
    fn an_explicitly_exited_span_leaves_no_context_behind() {
        let context = TraceContext::new();
        let span = operation_span(&context, "state");
        let _entered = span.enter();
        push_context(context);
        {
            let scope = TraceScope { entered_span: None };
            scope.exit();
        }
        // `exit` on a scope with no entered span only pops the context once; the enclosing
        // span entered above is unaffected because this guard never owned it.
        assert_eq!(TraceContext::current(), None);
    }
}
