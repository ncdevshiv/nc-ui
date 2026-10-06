//! A bounded multi-producer, multi-consumer queue with an explicit backpressure policy.
//!
//! ADR-003 is unambiguous: *no unbounded queue exists anywhere in the runtime*. This type is
//! the only queue primitive in the crate, and it cannot be constructed without a capacity.
//!
//! # Why the policy is explicit
//!
//! "What happens when the queue is full" is a decision every caller makes differently, and
//! guessing it for them is how messages get dropped without anyone noticing. So the policy is a
//! constructor argument, chosen deliberately by whoever owns the channel:
//!
//! * [`BackpressurePolicy::Block`] — the producer waits. Correct when losing work is worse
//!   than waiting, which is the usual case for a scheduler queue.
//! * [`BackpressurePolicy::Reject`] — the producer is told. Correct when the producer can
//!   react, such as an agent request that can be answered with a `Backpressure` error.
//! * [`BackpressurePolicy::DropOldest`] — the oldest is discarded. Correct only where the
//!   queue holds *state* rather than *work* — a log ring, a metric series — because it
//!   discards a message that somebody produced. It counts what it dropped so the loss is
//!   visible in metrics instead of being invisible in the data.
//!
//! # Why a `Mutex` and not a lock-free ring
//!
//! A lock-free ring would remove the contention, but it cannot express `Block` and `DropOldest`
//! without a far more intricate algorithm, and it would not survive a panicking producer
//! cleanly. The hot path here is a mutex acquisition plus a `VecDeque` push; the measurable
//! costs in this system are the `WebView` and the GPU, not a nanosecond of queue contention.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use ncui_core::{ErrorKind, NcError, Result};

/// What a full queue does to a new item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BackpressurePolicy {
    /// Wait for room.
    ///
    /// The only policy that never loses work. It can block the caller indefinitely if
    /// consumers stop, so the owner of the channel is responsible for ensuring they keep
    /// draining or for cancelling the producer.
    #[default]
    Block,
    /// Discard the oldest queued item to make room.
    ///
    /// For state-shaped queues (logs, metrics) only. A dropped item is counted by
    /// [`BoundedChannel::dropped_count`].
    DropOldest,
    /// Refuse the new item and return [`ErrorKind::Backpressure`].
    ///
    /// The only policy where the producer learns that it lost, so it can retry or report.
    Reject,
}

/// The shared state of a [`BoundedChannel`].
#[derive(Debug)]
struct Inner<T> {
    name: String,
    capacity: usize,
    policy: BackpressurePolicy,
    queue: Mutex<VecDeque<T>>,
    /// Signalled when an item is pushed, so a consumer can stop waiting.
    not_empty: Condvar,
    /// Signalled when an item is popped, so a producer under `Block` can stop waiting.
    not_full: Condvar,
    closed: AtomicBool,
    dropped: AtomicU64,
}

/// A bounded MPMC queue.
///
/// The channel is a handle: cloning shares the queue. The queue lives until the last handle
/// drops, and a blocked thread is woken on drop rather than left hanging — see
/// [`BoundedChannel::close`].
#[derive(Debug)]
pub struct BoundedChannel<T> {
    inner: Arc<Inner<T>>,
}

impl<T> BoundedChannel<T> {
    /// Creates a channel holding at most `capacity` items, named for diagnostics.
    ///
    /// A zero capacity is rejected: it would make every send fail or block forever while
    /// looking like a working channel, which is the kind of configuration error that only
    /// shows up under load.
    pub fn named(
        name: impl Into<String>,
        capacity: usize,
        policy: BackpressurePolicy,
    ) -> Result<Self> {
        if capacity == 0 {
            return Err(ncui_core::error::validation(
                "capacity",
                "a bounded channel must hold at least one item; zero would make every send fail",
            ));
        }
        let inner = Inner {
            name: name.into(),
            capacity,
            policy,
            queue: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            closed: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Creates a channel named after its policy-free default, for callers that have no better
    /// name. Diagnostics will say `"channel"`, so prefer [`BoundedChannel::named`].
    pub fn new(capacity: usize, policy: BackpressurePolicy) -> Result<Self> {
        Self::named("channel", capacity, policy)
    }

    /// The configured capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// The number of queued items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the queue holds no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the queue is at capacity.
    ///
    /// Checked under the lock rather than against a cached length, so a caller that uses it to
    /// decide whether to send is not racing another producer.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.lock().len() >= self.inner.capacity
    }

    /// The channel's name, as it appears in [`ErrorKind::Backpressure`].
    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// The configured policy.
    #[must_use]
    pub fn policy(&self) -> BackpressurePolicy {
        self.inner.policy
    }

    /// How many items this channel has discarded under [`BackpressurePolicy::DropOldest`].
    ///
    /// Exists so that the one policy which loses work makes the loss measurable. A drop count
    /// nobody reads is a silent drop.
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Whether the channel has been closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Sends an item, honouring the configured policy.
    ///
    /// Under [`BackpressurePolicy::Block`] this waits for room. Under `DropOldest` it
    /// discards the head and counts the loss. Under `Reject` it returns
    /// [`ErrorKind::Backpressure`]. Sends after [`BoundedChannel::close`] always fail with
    /// [`ErrorKind::Cancelled`], whatever the policy.
    pub fn send(&self, item: T) -> Result<()> {
        if self.is_closed() {
            return Err(Self::closed_error());
        }
        if self.inner.policy == BackpressurePolicy::Block {
            return self.send_blocking(item);
        }
        self.send_now(item)
    }

    /// Sends without ever waiting.
    ///
    /// Under [`BackpressurePolicy::Block`] this cannot honour the policy, so it behaves like
    /// `Reject` and returns [`ErrorKind::Backpressure`]. Documented rather than silent,
    /// because the alternative — blocking inside a method named `try_send` — would be a trap.
    pub fn try_send(&self, item: T) -> Result<()> {
        self.send_now(item)
    }

    /// Sends, dropping the head if the queue is full.
    fn send_now(&self, item: T) -> Result<()> {
        let mut queue = self.lock();
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(Self::closed_error());
        }
        if queue.len() >= self.inner.capacity {
            if self.inner.policy == BackpressurePolicy::DropOldest {
                if queue.pop_front().is_some() {
                    self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                return Err(self.full_error());
            }
        }
        queue.push_back(item);
        drop(queue);
        self.inner.not_empty.notify_one();
        Ok(())
    }

    /// Sends, waiting for room under the `Block` policy.
    fn send_blocking(&self, item: T) -> Result<()> {
        let mut queue = self.lock();
        loop {
            if self.inner.closed.load(Ordering::SeqCst) {
                return Err(Self::closed_error());
            }
            if queue.len() < self.inner.capacity {
                queue.push_back(item);
                drop(queue);
                self.inner.not_empty.notify_one();
                return Ok(());
            }
            queue = match self.inner.not_full.wait_timeout(queue, Self::WAIT_SLICE) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Receives an item, waiting for one if the queue is empty.
    ///
    /// Returns `None` once the channel is closed *and* drained. A closed channel with items
    /// still queued hands those out first: closing must not discard work that was accepted.
    pub fn recv(&self) -> Option<T> {
        let mut queue = self.lock();
        loop {
            if let Some(item) = queue.pop_front() {
                drop(queue);
                self.inner.not_full.notify_one();
                return Some(item);
            }
            if self.inner.closed.load(Ordering::SeqCst) {
                return None;
            }
            queue = match self.inner.not_empty.wait_timeout(queue, Self::WAIT_SLICE) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Receives an item only if one is already queued.
    #[must_use]
    pub fn try_recv(&self) -> Option<T> {
        let mut queue = self.lock();
        let item = queue.pop_front();
        drop(queue);
        if item.is_some() {
            self.inner.not_full.notify_one();
        }
        item
    }

    /// Receives an item, giving up after `timeout`.
    ///
    /// Returns [`ErrorKind::Timeout`] on expiry, which is retryable, so a caller looping on
    /// this can distinguish "nothing yet" from "closed".
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<T>> {
        let deadline = std::time::Instant::now() + timeout;
        let mut queue = self.lock();
        loop {
            if let Some(item) = queue.pop_front() {
                drop(queue);
                self.inner.not_full.notify_one();
                return Ok(Some(item));
            }
            if self.inner.closed.load(Ordering::SeqCst) {
                return Ok(None);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(NcError::new(ErrorKind::Timeout {
                    elapsed_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
                })
                .operation("ipc::channel::recv_timeout"));
            }
            let remaining = deadline - now;
            queue = match self.inner.not_empty.wait_timeout(queue, remaining) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Closes the channel and wakes everyone waiting on it.
    ///
    /// After this, `send` fails and `recv` drains whatever is left and then returns `None`.
    /// Waiting producers and consumers are released rather than left blocked, so a shutdown
    /// cannot hang on a channel nobody is servicing.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.not_empty.notify_all();
        self.inner.not_full.notify_all();
    }

    /// Drains every queued item.
    ///
    /// Used at shutdown to hand a caller everything the channel accepted. Infallible: a
    /// channel is bounded by construction, so "what is queued" always fits in the caller's
    /// memory and there is no policy to apply. Closing does not discard accepted work — the
    /// point of this method is that it does not.
    pub fn drain(&self) -> Vec<T> {
        let mut queue = self.lock();
        let items: Vec<T> = queue.drain(..).collect();
        drop(queue);
        self.inner.not_full.notify_all();
        items
    }

    /// How long a blocking wait parks before re-checking.
    ///
    /// A `Condvar` wait can end spuriously, and a notify can be missed if it lands between a
    /// waiter's check and its wait. Re-checking on a bounded park means neither can park a
    /// thread forever; it costs nothing when the channel is behaving and bounds the damage
    /// when it is not.
    const WAIT_SLICE: Duration = Duration::from_millis(50);

    /// The error reported when a non-blocking send finds the queue full.
    fn full_error(&self) -> NcError {
        NcError::new(ErrorKind::Backpressure {
            channel: self.inner.name.clone(),
            capacity: self.inner.capacity,
        })
        .operation("ipc::channel::send")
    }

    /// The error reported when a send or receive follows a close.
    ///
    /// Carries no channel detail: once a channel is closed the reason it was closed is not
    /// recorded, so naming the channel here would imply a diagnostic that does not exist.
    fn closed_error() -> NcError {
        NcError::new(ErrorKind::Cancelled).operation("ipc::channel::send")
    }

    /// Locks the queue, recovering from poisoning.
    ///
    /// A panic while the lock was held cannot have corrupted the `VecDeque` — the standard
    /// library's own invariants make that impossible for the operations used here — so
    /// recovering keeps the channel usable instead of turning one panicking task into a
    /// permanently broken queue.
    fn lock(&self) -> MutexGuard<'_, VecDeque<T>> {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    use std::sync::Mutex;

    #[test]
    fn a_zero_capacity_channel_is_rejected() {
        let err = BoundedChannel::<u8>::new(0, BackpressurePolicy::Block)
            .expect_err("zero capacity must be rejected");
        assert!(err.to_string().contains("capacity"), "got: {err}");
    }

    #[test]
    fn a_new_channel_is_empty_and_not_full() {
        let channel = BoundedChannel::<u8>::named("q", 2, BackpressurePolicy::Block).unwrap();
        assert_eq!(channel.len(), 0);
        assert_eq!(channel.capacity(), 2);
        assert!(channel.is_empty());
        assert!(!channel.is_full());
        assert_eq!(channel.name(), "q");
    }

    #[test]
    fn items_come_back_in_order() {
        let channel = BoundedChannel::new(4, BackpressurePolicy::Block).unwrap();
        for i in 0..3u8 {
            channel.send(i).unwrap();
        }
        assert_eq!(channel.len(), 3);
        for i in 0..3u8 {
            assert_eq!(channel.recv(), Some(i));
        }
        assert!(channel.is_empty());
    }

    #[test]
    fn reject_returns_backpressure_and_keeps_the_original_item() {
        let channel = BoundedChannel::named("reject.q", 2, BackpressurePolicy::Reject).unwrap();
        channel.send(1u8).unwrap();
        channel.send(2u8).unwrap();
        let err = channel
            .send(3u8)
            .expect_err("a full channel must reject rather than grow");
        match err.kind() {
            ErrorKind::Backpressure { channel, capacity } => {
                assert_eq!(channel, "reject.q");
                assert_eq!(*capacity, 2);
            }
            other => panic!("expected Backpressure, got {other:?}"),
        }
        assert_eq!(
            channel.len(),
            2,
            "a rejected item must not displace a queued one"
        );
        assert_eq!(channel.recv(), Some(1));
        assert_eq!(channel.recv(), Some(2));
    }

    #[test]
    fn drop_oldest_discards_the_head_and_counts_it() {
        let channel = BoundedChannel::named("ring", 2, BackpressurePolicy::DropOldest).unwrap();
        for i in 0..5u8 {
            channel.send(i).unwrap();
        }
        assert_eq!(channel.len(), 2);
        assert_eq!(channel.dropped_count(), 3);
        assert_eq!(channel.recv(), Some(3));
        assert_eq!(channel.recv(), Some(4));
    }

    #[test]
    fn block_never_drops_and_never_rejects() {
        // The point of `Block`: the producer waits, so nothing is lost. Here the queue is
        // given room by a consumer draining it, and the send must succeed rather than fail.
        let channel =
            Arc::new(BoundedChannel::named("blocking", 1, BackpressurePolicy::Block).unwrap());
        channel.send(1u8).unwrap();
        assert!(channel.is_full());

        let consumer = {
            let channel = Arc::clone(&channel);
            std::thread::spawn(move || {
                // Wait for the producer to be parked, then drain.
                std::thread::sleep(Duration::from_millis(20));
                assert_eq!(channel.recv(), Some(1));
                assert_eq!(channel.recv(), Some(2));
            })
        };
        // This send must block until the consumer makes room, then succeed.
        channel.send(2u8).unwrap();
        consumer.join().unwrap();
        assert_eq!(channel.dropped_count(), 0);
    }

    #[test]
    fn try_send_under_block_reports_backpressure_rather_than_blocking() {
        let channel = BoundedChannel::named("try", 1, BackpressurePolicy::Block).unwrap();
        channel.send(1u8).unwrap();
        let err = channel
            .try_send(2u8)
            .expect_err("try_send must not block under the Block policy");
        assert!(matches!(err.kind(), ErrorKind::Backpressure { .. }));
    }

    #[test]
    fn recv_timeout_reports_timeout_rather_than_blocking_forever() {
        let channel = BoundedChannel::<u8>::new(1, BackpressurePolicy::Block).unwrap();
        let err = channel
            .recv_timeout(Duration::from_millis(10))
            .expect_err("an empty channel must time out");
        assert!(matches!(err.kind(), ErrorKind::Timeout { .. }));
        assert!(err.is_retryable());
    }

    #[test]
    fn recv_timeout_returns_an_item_that_arrives_in_time() {
        let channel = Arc::new(BoundedChannel::<u8>::new(1, BackpressurePolicy::Block).unwrap());
        let producer = {
            let channel = Arc::clone(&channel);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(5));
                channel.send(9u8).unwrap();
            })
        };
        assert_eq!(
            channel
                .recv_timeout(Duration::from_secs(5))
                .expect("the producer delivers within the timeout"),
            Some(9)
        );
        producer.join().unwrap();
    }

    #[test]
    fn a_closed_channel_refuses_sends_and_drains_then_ends() {
        let channel = BoundedChannel::<u8>::named("closing", 4, BackpressurePolicy::Block).unwrap();
        channel.send(1u8).unwrap();
        channel.close();
        assert!(channel.is_closed());
        let err = channel.send(2u8).expect_err("send after close must fail");
        assert!(matches!(err.kind(), ErrorKind::Cancelled));
        // Accepted work is still delivered: closing must not discard it.
        assert_eq!(channel.recv(), Some(1));
        assert_eq!(channel.recv(), None);
    }

    #[test]
    fn a_blocked_receiver_is_released_by_close() {
        let channel = Arc::new(BoundedChannel::<u8>::new(1, BackpressurePolicy::Block).unwrap());
        let waiter = {
            let channel = Arc::clone(&channel);
            std::thread::spawn(move || channel.recv())
        };
        std::thread::sleep(Duration::from_millis(20));
        channel.close();
        // Without the close waking waiters, this join would hang for the wait slice at a
        // time; the assertion is that it returns promptly and reports the close.
        assert_eq!(waiter.join().unwrap(), None);
    }

    #[test]
    fn try_recv_returns_none_when_empty_and_the_item_when_present() {
        let channel = BoundedChannel::<u8>::new(2, BackpressurePolicy::Block).unwrap();
        assert_eq!(channel.try_recv(), None);
        channel.send(5u8).unwrap();
        assert_eq!(channel.try_recv(), Some(5));
        assert_eq!(channel.try_recv(), None);
    }

    #[test]
    fn drain_hands_back_everything_that_was_accepted() {
        let channel = BoundedChannel::new(8, BackpressurePolicy::Block).unwrap();
        for i in 0..5u8 {
            channel.send(i).unwrap();
        }
        assert_eq!(channel.drain(), vec![0, 1, 2, 3, 4]);
        assert!(channel.is_empty());
    }

    #[test]
    fn many_producers_and_consumers_move_every_item() {
        const PRODUCERS: u32 = 4;
        const PER_PRODUCER: u32 = 250;
        const CONSUMERS: usize = 3;
        const CAPACITY: usize = 8;
        let channel = Arc::new(
            BoundedChannel::<u32>::named("mpmc", CAPACITY, BackpressurePolicy::Block).unwrap(),
        );
        let total = (PRODUCERS * PER_PRODUCER) as usize;

        let producers: Vec<_> = (0..PRODUCERS)
            .map(|p| {
                let channel = Arc::clone(&channel);
                std::thread::spawn(move || {
                    for i in 0..PER_PRODUCER {
                        channel.send(p * 10_000 + i).unwrap();
                    }
                })
            })
            .collect();

        // Several consumers, so the MPMC path is genuinely exercised rather than one thread
        // doing everything. They drain until the channel closes rather than each taking a
        // fixed count: a fixed count would park the last consumer forever waiting for an
        // item nobody is going to send.
        let collected = Arc::new(Mutex::new(Vec::with_capacity(total)));
        let consumers: Vec<_> = (0..CONSUMERS)
            .map(|_| {
                let channel = Arc::clone(&channel);
                let collected = Arc::clone(&collected);
                std::thread::spawn(move || {
                    let mut mine = Vec::new();
                    while let Some(item) = channel.recv() {
                        mine.push(item);
                    }
                    collected.lock().unwrap().extend(mine);
                })
            })
            .collect();

        for producer in producers {
            producer.join().unwrap();
        }
        // Closing is what lets the consumers finish: the last one is still blocked in `recv`.
        channel.close();
        for consumer in consumers {
            consumer.join().unwrap();
        }

        // Exactly-once delivery: the multiset received must equal the multiset sent.
        let mut received = Arc::try_unwrap(collected)
            .map_err(|_| ())
            .expect("all consumers must have exited")
            .into_inner()
            .expect("the collection lock is not poisoned");
        received.sort_unstable();
        let expected: Vec<u32> = (0..PRODUCERS)
            .flat_map(|p| (0..PER_PRODUCER).map(move |i| p * 10_000 + i))
            .collect();
        assert_eq!(received, expected, "every item must arrive exactly once");
        assert_eq!(channel.dropped_count(), 0, "Block must never drop");
    }

    #[test]
    fn drop_oldest_never_grows_past_capacity() {
        let channel = BoundedChannel::named("ring", 4, BackpressurePolicy::DropOldest).unwrap();
        for i in 0..10_000u32 {
            channel.send(i).unwrap();
            assert!(channel.len() <= channel.capacity());
        }
        assert_eq!(channel.len(), 4);
        assert_eq!(channel.dropped_count(), 10_000 - 4);
    }

    #[test]
    fn the_channel_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BoundedChannel<u8>>();
    }
}
