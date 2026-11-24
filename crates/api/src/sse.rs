//! Server-Sent Events out of a Durable Object.
//!
//! Under skyzen 0.6 a Durable Object activation is constructed once and
//! serves concurrent requests through `&self`, so a stream does not poll
//! storage on a fixed sleep loop. Instead, writers commit to storage and
//! pulse an in-memory [`Signal`], and readers snapshot [`Signal::version`]
//! before reading storage and await [`Signal::changed`] when nothing new
//! is there yet. Storage remains the source of truth across eviction;
//! [`Signal`] only eliminates the poll latency and idle read load.
//!
//! The stream task writes into the bounded channel behind
//! [`Sse::channel_with_capacity`], because the response body must be
//! `Sync` and a `DurableDb` query future is not: the task needs only
//! `Send`, which the channel gives it for free on both targets — the
//! isolate's microtask queue on wasm32, the runtime's global executor
//! natively.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll as TaskPoll, Waker};
use std::time::Duration;

use skyzen::responder::Sse;
use skyzen::responder::sse::{Event, Sender};

/// How long a stream waits before retrying after a transient storage error.
pub const RETRY_WAIT: Duration = Duration::from_secs(1);

/// How long a stream may say nothing before it proves itself alive.
///
/// The interval is what keeps an idle stream's TCP flow known-good to
/// every middlebox on the path: a `ping` comment is data, and data is
/// what resets the idle timers that reclaim quiet connections.
pub const HEARTBEAT: Duration = Duration::from_secs(15);

/// How many events the channel buffers before a slow reader stalls the
/// feed. Backpressure rather than an unbounded queue: a daemon that has
/// gone away does not need its command log copied into memory.
const CAPACITY: usize = 64;

/// An in-memory wakeup broadcast shared across concurrent requests on one
/// Durable Object activation.
///
/// Writers call [`Signal::notify`] after committing a row to storage;
/// readers snapshot [`Signal::version`] *before* querying storage and wait
/// on [`Signal::changed`] when storage had nothing yet. Because the
/// version advances monotonically under the lock, a notification that
/// lands between the read and the await wakes the reader immediately with
/// no lost-wakeup window.
#[derive(Debug, Clone, Default)]
pub struct Signal(Arc<Mutex<SignalState>>);

#[derive(Debug, Default)]
struct SignalState {
    version: u64,
    next_waiter: u64,
    wakers: Vec<(u64, Waker)>,
}

impl Signal {
    /// Current generation counter, snapshotted before a storage read.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .version
    }

    /// Advances the generation and wakes every waiter currently parked on
    /// this signal.
    pub fn notify(&self) {
        let wakers = {
            let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            state.version = state.version.wrapping_add(1);
            std::mem::take(&mut state.wakers)
        };
        for (_, waker) in wakers {
            waker.wake();
        }
    }

    /// Future that resolves as soon as [`Self::version`] differs from `seen`.
    #[must_use]
    pub fn changed(&self, seen: u64) -> Changed {
        Changed {
            signal: self.clone(),
            seen,
            waiter_id: None,
        }
    }
}

/// Future returned by [`Signal::changed`].
///
/// Deregisters its waker on drop so long-lived streams whose waits are
/// cancelled by heartbeats or deadlines never leave stale entries behind.
#[derive(Debug)]
pub struct Changed {
    signal: Signal,
    seen: u64,
    waiter_id: Option<u64>,
}

impl Future for Changed {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> TaskPoll<()> {
        let mut state = self.signal.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.version != self.seen {
            return TaskPoll::Ready(());
        }
        if let Some(id) = self.waiter_id
            && let Some((_, existing)) = state.wakers.iter_mut().find(|(entry, _)| *entry == id)
        {
            existing.clone_from(cx.waker());
            return TaskPoll::Pending;
        }
        let id = state.next_waiter;
        state.next_waiter = state.next_waiter.wrapping_add(1);
        state.wakers.push((id, cx.waker().clone()));
        drop(state);
        self.waiter_id = Some(id);
        TaskPoll::Pending
    }
}

impl Drop for Changed {
    fn drop(&mut self) {
        if let Some(id) = self.waiter_id {
            let mut state = self.signal.0.lock().unwrap_or_else(PoisonError::into_inner);
            state.wakers.retain(|(entry, _)| *entry != id);
        }
    }
}

/// What one step of a feed concluded.
#[derive(Debug)]
pub enum Poll {
    /// Events to emit, in the order they arrived. Never empty.
    Emit(Vec<Event>),
    /// Nothing new yet: park until `signal` advances past its snapshotted
    /// version, or until `after` elapses (whichever comes first).
    /// Heartbeats still fire on `heartbeat` while parked without
    /// re-stepping the feed.
    Wait {
        /// In-memory wakeup signal and the version snapshotted before the
        /// feed read storage.
        signal: Option<(Signal, u64)>,
        /// Optional deadline after which the feed should run again (for
        /// presence/lease renewal, a request timeout, or error backoff).
        after: Option<Duration>,
    },
    /// The stream is over: the body ends cleanly.
    End,
}

/// A step over a feed's state.
///
/// Boxed because the concrete future of each feed's step is unnameable;
/// a plain `fn` returning this type satisfies the bound. `Send` is the
/// most the future can promise — a `DurableDb` query's boxed future is
/// never `Sync` — and all either executor needs.
pub type PollFn<'a> = Pin<Box<dyn Future<Output = Poll> + Send + 'a>>;

/// Serves `step` over `state` as a `text/event-stream` responder.
///
/// Each iteration calls `step`: [`Poll::Emit`] puts the events on the
/// channel, [`Poll::Wait`] parks on the feed's in-memory signal and/or
/// deadline while emitting `ping` comments every `heartbeat`, and
/// [`Poll::End`] ends the body. A gone reader turns the next `send` into
/// an error, which ends the task — the channel dropping is the only
/// cancellation the feed needs.
pub fn serve<S, F>(state: S, step: F, heartbeat: Duration) -> Sse
where
    S: Send + 'static,
    F: for<'a> FnMut(&'a mut S) -> PollFn<'a> + Send + 'static,
{
    let (sender, sse) = Sse::channel_with_capacity(CAPACITY);
    spawn(drive(state, step, sender, heartbeat));
    sse
}

/// What woke a parked stream.
enum Wake {
    /// The feed's signal fired or its deadline elapsed: re-step the feed.
    Step,
    /// The heartbeat interval elapsed: emit a `ping` comment and stay parked.
    Ping,
}

/// The event-driven loop behind a served stream.
async fn drive<S, F>(mut state: S, mut step: F, sender: Sender, heartbeat: Duration)
where
    F: for<'a> FnMut(&'a mut S) -> PollFn<'a>,
{
    // A `Delay` that expires when the next ping is due: kept rather than
    // a wall-clock read so the interval can be shorter than a second —
    // which `now_unix` cannot express — and so a test can ask for one.
    let mut ping_at = futures_timer::Delay::new(heartbeat);
    loop {
        match step(&mut state).await {
            Poll::Emit(events) => {
                debug_assert!(
                    !events.is_empty(),
                    "a feed answered Emit with nothing to emit"
                );
                for event in events {
                    if sender.send(event).await.is_err() {
                        return;
                    }
                }
                ping_at.reset(heartbeat);
            }
            Poll::Wait { signal, after } => {
                let mut changed = signal.map(|(sig, seen)| sig.changed(seen));
                let mut deadline = after.map(futures_timer::Delay::new);
                loop {
                    let wake = futures_util::future::poll_fn(|cx| {
                        if let Some(fut) = changed.as_mut()
                            && Pin::new(fut).poll(cx).is_ready()
                        {
                            return TaskPoll::Ready(Wake::Step);
                        }
                        if let Some(fut) = deadline.as_mut()
                            && Pin::new(fut).poll(cx).is_ready()
                        {
                            return TaskPoll::Ready(Wake::Step);
                        }
                        if Pin::new(&mut ping_at).poll(cx).is_ready() {
                            return TaskPoll::Ready(Wake::Ping);
                        }
                        TaskPoll::Pending
                    })
                    .await;
                    match wake {
                        Wake::Step => break,
                        Wake::Ping => {
                            ping_at.reset(heartbeat);
                            if sender.send(Event::comment("ping")).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            Poll::End => return,
        }
    }
}

/// Runs `task` to completion.
///
/// The Worker drives `spawn_local` futures on the isolate's microtask
/// queue — alive exactly as long as the response the task feeds. The
/// native runtime's global executor (a `SmolGlobal` the server installs
/// at start-up) takes the same task natively.
#[cfg(target_arch = "wasm32")]
fn spawn(task: impl Future<Output = ()> + 'static) {
    skyzen::wasm_bindgen_futures::spawn_local(task);
}

/// Runs `task` to completion on the runtime's global executor.
#[cfg(not(target_arch = "wasm32"))]
fn spawn(task: impl Future<Output = ()> + Send + 'static) {
    executor_core::spawn(task).detach();
}
