//! Cancellation.
//!
//! A [`CancellationToken`] is a shared, one-way flag: any number of holders may observe it and
//! any one of them may trip it. It is the runtime's answer to "the user closed the window, or
//! the request was superseded, or the deadline passed" — a condition that has to reach work
//! that is already running, possibly on another thread, without anyone owning a handle to that
//! work's future.
//!
//! # Why this is not an `AtomicBool`
//!
//! An `AtomicBool` is enough for *polling*. It is not enough for waiting, and polling a flag
//! is how a task ends up burning a core while it waits for something that will never arrive. A
//! token therefore also carries a waiter list and hands out a future: a task can
//! `tok.cancelled().await` instead of spinning.
//!
//! # Why the future borrows the token
//!
//! [`CancellationToken::cancelled`] returns a future that borrows the token rather than
//! cloning it. A spawned task that outlives its token would otherwise keep the flag alive and
//! report "not cancelled" forever — a hang that is very hard to see, because everything is
//! still running. Borrowing makes the token's lifetime the task's lifetime, which the compiler
//! enforces instead of a reviewer having to notice.
//!
//! # Racing cancel and poll
//!
//! Tripping the token takes the waiter lock, sets the flag and then wakes every waiter. A
//! future registers itself under the same lock and only then re-reads the flag. That ordering
//! is what makes the pair race-free: a waiter either observes the flag before registering, or
//! is registered before the flag is set, so it is woken. A future that registered and then
//! checked without the lock could be woken before it started waiting and would sleep forever.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// The shared state behind a token.
#[derive(Debug, Default)]
struct Inner {
    cancelled: AtomicBool,
    waiters: Mutex<Vec<core::task::Waker>>,
}

/// A cloneable, thread-safe cancellation flag.
///
/// Cloning is cheap and shares state: a token and its clones all observe and trip the *same*
/// flag. That is the point — the canceller usually does not own the work it is cancelling.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    inner: Arc<Inner>,
}

impl CancellationToken {
    /// Creates a token that has not been cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::default()),
        }
    }

    /// Trips the token and wakes everything waiting on it.
    ///
    /// Idempotent: cancelling an already-cancelled token does nothing. That matters because
    /// the second caller is usually an unwinding error path, and it should not have to check
    /// whether someone got there first.
    pub fn cancel(&self) {
        let mut waiters = Self::lock(&self.inner);
        if self.inner.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        // Drain under the lock, then wake outside it. Waking runs arbitrary `Wake`
        // implementations, and holding a lock across them invites a deadlock.
        let drained: Vec<_> = waiters.drain(..).collect();
        drop(waiters);
        for waker in drained {
            waker.wake();
        }
    }

    /// Whether the token has been cancelled.
    ///
    /// Relaxed ordering would suffice for the load if it stood alone; it is `SeqCst` here so
    /// that a thread which observes `true` also observes the writes the canceller made before
    /// calling `cancel`, which is what lets a worker publish its partial result safely.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// A future that resolves once the token is cancelled.
    ///
    /// Polling it before cancellation parks the task; polling after cancellation returns
    /// immediately. Borrows the token, so the token must outlive the task.
    #[must_use]
    pub fn cancelled(&self) -> Cancelled<'_> {
        Cancelled {
            token: self,
            registered: None,
        }
    }

    /// Locks the waiter list, recovering from poisoning instead of propagating it.
    ///
    /// A poisoned mutex means another thread panicked while holding the lock. Propagating the
    /// poison would make cancellation fail exactly when the system is already in trouble, and
    /// the only thing a panic can leave inconsistent here is the waker list — never the flag,
    /// which is an atomic and is not guarded by this lock. So the list is recovered and
    /// cancellation proceeds.
    fn lock(inner: &Inner) -> MutexGuard<'_, Vec<core::task::Waker>> {
        inner
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A future that resolves when its [`CancellationToken`] is cancelled.
///
/// Produced by [`CancellationToken::cancelled`]. Not constructed directly: the `registered`
/// field is private so the only way to obtain one is from a token, which guarantees the
/// invariant that the future and its token share a lifetime.
#[derive(Debug)]
pub struct Cancelled<'a> {
    token: &'a CancellationToken,
    /// The waker currently parked in the token's list, so it can be removed on drop.
    registered: Option<core::task::Waker>,
}

impl Future for Cancelled<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if this.token.is_cancelled() {
            return Poll::Ready(());
        }
        let mut waiters = CancellationToken::lock(&this.token.inner);
        // Re-check under the lock. The flag may have been set between the check above and
        // taking the lock; without this, `cancel` could have drained the waiter list before
        // this waker was added and nobody would ever wake this task.
        if this.token.inner.cancelled.load(Ordering::SeqCst) {
            return Poll::Ready(());
        }
        let already = this
            .registered
            .as_ref()
            .is_some_and(|w| waiters.iter().any(|other| other.will_wake(w)));
        let needs_add = this
            .registered
            .as_ref()
            .is_none_or(|w| !w.will_wake(cx.waker()));
        if !already && needs_add {
            waiters.push(cx.waker().clone());
            this.registered = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl Drop for Cancelled<'_> {
    fn drop(&mut self) {
        // Deregister so a long-lived token does not accumulate a waker per cancelled future.
        // Without this, a task that is cancelled and re-spawned in a loop grows the list
        // without bound — a slow leak in exactly the situation where the runtime is already
        // under pressure.
        let Some(registered) = self.registered.as_ref() else {
            return;
        };
        if let Ok(mut waiters) = self.token.inner.waiters.lock() {
            waiters.retain(|w| !w.will_wake(registered));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "test assertions must abort loudly on failure"
    )]

    use super::*;
    use core::task::Waker;
    use std::task::Wake;

    /// A waker that counts its own wakeups, so a test can prove the token actually signalled
    /// a parked task rather than merely flipping a flag.
    struct CountingWaker(core::sync::atomic::AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Compile-time proof that a type satisfies `Send + Sync`.
    fn assert_send_sync<T: Send + Sync>() {}

    /// Compile-time proof that a type satisfies `Send`.
    fn assert_send<T: Send>() {}

    /// Polls a future once with a fresh no-op waker.
    fn poll_once<F: Future<Output = ()>>(future: Pin<&mut F>) -> Poll<()> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn a_new_token_is_not_cancelled() {
        assert!(!CancellationToken::new().is_cancelled());
    }

    #[test]
    fn cancelling_is_visible_through_every_clone() {
        let token = CancellationToken::new();
        let a = token.clone();
        let b = token.clone();
        token.cancel();
        assert!(token.is_cancelled());
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
    }

    #[test]
    fn cancelling_twice_is_idempotent() {
        let token = CancellationToken::new();
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn the_future_is_ready_before_poll_only_after_cancellation() {
        let token = CancellationToken::new();
        let mut future = std::pin::pin!(token.cancelled());
        assert!(poll_once(future.as_mut()).is_pending());
        token.cancel();
        assert!(poll_once(future.as_mut()).is_ready());
    }

    #[test]
    fn an_already_cancelled_token_produces_a_ready_future() {
        let token = CancellationToken::new();
        token.cancel();
        let mut future = std::pin::pin!(token.cancelled());
        assert!(poll_once(future.as_mut()).is_ready());
    }

    #[test]
    fn dropping_the_future_deregisters_its_waker() {
        let token = CancellationToken::new();
        {
            let counter = Arc::new(CountingWaker(core::sync::atomic::AtomicUsize::new(0)));
            let waker = Waker::from(Arc::clone(&counter));
            let mut cx = Context::from_waker(&waker);
            let mut future = std::pin::pin!(token.cancelled());
            assert!(future.as_mut().poll(&mut cx).is_pending());
            assert_eq!(token.inner.waiters.lock().unwrap().len(), 1);
        }
        // The future is gone; its waker must not still be parked in the token.
        assert_eq!(token.inner.waiters.lock().unwrap().len(), 0);

        let counter = Arc::new(CountingWaker(core::sync::atomic::AtomicUsize::new(0)));
        {
            let waker = Waker::from(Arc::clone(&counter));
            let mut cx = Context::from_waker(&waker);
            let mut future = std::pin::pin!(token.cancelled());
            assert!(future.as_mut().poll(&mut cx).is_pending());
            token.cancel();
            assert_eq!(counter.0.load(Ordering::SeqCst), 1);
            // A second poll after cancellation is Ready and must not wake again.
            assert!(future.as_mut().poll(&mut cx).is_ready());
            assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn polling_the_same_future_twice_does_not_register_two_wakers() {
        let token = CancellationToken::new();
        let mut future = std::pin::pin!(token.cancelled());
        assert!(poll_once(future.as_mut()).is_pending());
        assert!(poll_once(future.as_mut()).is_pending());
        assert!(poll_once(future.as_mut()).is_pending());
        assert_eq!(token.inner.waiters.lock().unwrap().len(), 1);
    }

    #[test]
    fn cancelling_from_another_thread_wakes_the_waiter() {
        let token = CancellationToken::new();
        let counter = Arc::new(CountingWaker(core::sync::atomic::AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut cx = Context::from_waker(&waker);
        let mut future = std::pin::pin!(token.cancelled());
        assert!(future.as_mut().poll(&mut cx).is_pending());

        let handle = std::thread::spawn({
            let token = token.clone();
            move || token.cancel()
        });
        handle.join().unwrap();
        assert!(token.is_cancelled());
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_token_is_send_and_sync() {
        // A compile-time assertion, not a runtime one: this fails to *build* if the bounds
        // regress, which is the only moment the claim matters.
        assert_send_sync::<CancellationToken>();
        assert_send::<Cancelled<'static>>();
    }

    #[test]
    fn cancelling_from_many_threads_trips_the_token_once() {
        let token = CancellationToken::new();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let token = token.clone();
                std::thread::spawn(move || token.cancel())
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert!(token.is_cancelled());
        assert_eq!(token.inner.waiters.lock().unwrap().len(), 0);
    }
}
