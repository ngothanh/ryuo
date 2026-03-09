# Building a Disruptor in Rust: Ryuo — Part 16: Async Integration — Bridging Sync and Async Worlds

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 16 of 16
**Target Audience:** Systems engineers integrating low-latency components with async Rust
**Prerequisites:** Part 10 (EventPoller), Part 13 (builder DSL), Part 14 (production patterns)

---

## Recap

Ryuo is synchronous by design. Every component — ring buffer, sequencers, barriers, event handlers — uses lock-free atomics on dedicated threads. This is the right architecture for sub-microsecond latency.

But modern Rust is async. Tokio, async-std, and hyper power most production services. If your system accepts orders over gRPC and processes them through Ryuo, you need to cross the sync/async boundary.

This post builds three patterns for bridging sync and async, each with different trade-offs:

| Pattern | Publish Overhead | Consume Model | Best For |
|---------|-----------------|---------------|----------|
| **Dedicated Thread** | ~100-500ns (channel hop) | Callback | gRPC/HTTP → Ryuo |
| **EventPoller + Stream** | Zero (direct) | `Stream` | Ryuo → async pipeline |
| **Hybrid** | Sync: 0ns, Async: ~100ns | Both | Mixed workloads |

---

## The Problem: Why Not Just Use Async Everywhere?

```
                    ┌─────────────────────────────────────────────┐
                    │           Tokio Runtime (async)              │
                    │                                             │
                    │  HTTP ──→ gRPC ──→ ??? ──→ WebSocket out   │
                    │                    │                         │
                    │              Need Ryuo here                 │
                    │              but Ryuo is sync               │
                    │              and must not block              │
                    │              the Tokio thread pool           │
                    └─────────────────────────────────────────────┘
```

Three constraints:

1. **Ryuo must not run on Tokio threads.** `BusySpinWaitStrategy` and `YieldingWaitStrategy` will starve other tasks on the same thread. Even `BlockingWaitStrategy` holds a mutex, which blocks the executor.

2. **Async callers must not block.** Calling `sequencer.claim()` from an async context blocks the executor if the buffer is full. Tokio tasks must never block.

3. **Latency budget is different.** Ryuo's hot path is 50-100ns. An async channel hop adds 100-500ns. This is acceptable for I/O-bound producers (gRPC already costs microseconds) but not for HFT producers (use sync path directly).

---

## Pattern 1: Dedicated Thread (Async → Sync)

The simplest pattern: Ryuo runs on a dedicated OS thread. Async code publishes through a Tokio channel.

```
                    ┌──────────────┐
                    │ Tokio Task   │
                    │              │──── publish_with() ────┐
                    └──────────────┘                        │
                    ┌──────────────┐                        ▼
                    │ Tokio Task   │──── publish_with() ──→ tokio::mpsc
                    └──────────────┘                        │
                    ┌──────────────┐                        │
                    │ Tokio Task   │──── publish_with() ────┘
                    └──────────────┘                        │
                                                           ▼
                    ┌─────────────────────────────────────────┐
                    │         Dedicated OS Thread              │
                    │  blocking_recv() → ring_buffer           │
                    │    .publish_with(&sequencer, closure)    │
                    └─────────────────────────────────────────┘
                                                           │
                    ┌──────────────┐                        │
                    │  Consumer    │ ←── ring buffer ←──────┘
                    │  Thread(s)   │
                    └──────────────┘
```

### Implementation

```rust
use tokio::sync::mpsc;
use std::sync::Arc;

/// Command sent from async code to the dedicated Ryuo thread.
enum Command<T> {
    /// Publish an event using the provided closure.
    Publish(Box<dyn FnOnce(&mut T, i64) + Send>),
    /// Shut down the Ryuo thread.
    Shutdown,
}

/// Async wrapper around a sync Ryuo disruptor.
///
/// Publishes are forwarded to a dedicated OS thread via
/// a Tokio unbounded channel. This keeps the Tokio executor
/// free while Ryuo operates on its own thread.
pub struct AsyncPublisher<T: Send + 'static> {
    command_tx: mpsc::UnboundedSender<Command<T>>,
}

impl<T: Send + 'static> AsyncPublisher<T> {
    /// Create a new async publisher backed by a dedicated Ryuo thread.
    ///
    /// # Arguments
    /// - `ring_buffer`: The shared ring buffer.
    /// - `sequencer`: The sequencer (single or multi-producer).
    ///
    /// The dedicated thread runs until `shutdown()` is called.
    pub fn new(
        ring_buffer: Arc<RingBuffer<T>>,
        sequencer: Arc<dyn Sequencer>,
    ) -> Self {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();

        // Spawn dedicated OS thread — NOT a Tokio task.
        // This thread does the actual ring buffer writes.
        std::thread::Builder::new()
            .name("ryuo-publisher".into())
            .spawn(move || {
                // blocking_recv() is safe here — we're on a real
                // OS thread, not a Tokio worker.
                while let Some(cmd) = command_rx.blocking_recv() {
                    match cmd {
                        Command::Publish(f) => {
                            ring_buffer.publish_with(&*sequencer, f);
                        }
                        Command::Shutdown => break,
                    }
                }
            })
            .expect("failed to spawn ryuo-publisher thread");

        Self { command_tx }
    }

    /// Publish an event from an async context.
    ///
    /// This sends a command to the dedicated thread — it does NOT
    /// interact with the ring buffer directly. The closure is boxed
    /// and sent through the channel.
    ///
    /// Cost: ~100-500ns (channel send + context switch to publisher thread).
    pub async fn publish_with<F>(&self, f: F) -> Result<(), PublishError>
    where
        F: FnOnce(&mut T, i64) + Send + 'static,
    {
        self.command_tx
            .send(Command::Publish(Box::new(f)))
            .map_err(|_| PublishError::ShuttingDown)
    }

    /// Publish a batch of events.
    ///
    /// Each closure in the iterator becomes a separate command.
    /// For high throughput, consider batching on the dedicated thread
    /// side instead (see Pattern 3).
    pub async fn publish_batch<I, F>(&self, closures: I) -> Result<(), PublishError>
    where
        I: IntoIterator<Item = F>,
        F: FnOnce(&mut T, i64) + Send + 'static,
    {
        for f in closures {
            self.command_tx
                .send(Command::Publish(Box::new(f)))
                .map_err(|_| PublishError::ShuttingDown)?;
        }
        Ok(())
    }

    /// Graceful shutdown: drain pending commands, then stop.
    pub async fn shutdown(self) {
        let _ = self.command_tx.send(Command::Shutdown);
        // The thread will exit when it processes the Shutdown command.
        // The channel will be dropped when self is dropped.
    }
}

impl<T: Send + 'static> Clone for AsyncPublisher<T> {
    fn clone(&self) -> Self {
        Self {
            command_tx: self.command_tx.clone(),
        }
    }
}
```

**Why `UnboundedSender`?** A bounded channel would require `.await` on send when full — adding backpressure from Tokio into the publish path. We want the async side to fire-and-forget; backpressure is handled by Ryuo's ring buffer (the dedicated thread blocks on `sequencer.claim()` if the buffer is full). If you need to limit the command queue, use a bounded channel and add `.await`:

```rust
// Bounded variant: adds async backpressure
pub struct BoundedAsyncPublisher<T: Send + 'static> {
    command_tx: mpsc::Sender<Command<T>>,  // bounded
}

impl<T: Send + 'static> BoundedAsyncPublisher<T> {
    pub async fn publish_with<F>(&self, f: F) -> Result<(), PublishError>
    where
        F: FnOnce(&mut T, i64) + Send + 'static,
    {
        // .send().await blocks the task (not the thread) if the channel is full.
        self.command_tx
            .send(Command::Publish(Box::new(f)))
            .await
            .map_err(|_| PublishError::ShuttingDown)
    }
}
```

**Trade-off:**
- Unbounded: No async backpressure, relies on ring buffer backpressure. Simpler. Risk: unbounded memory growth if async producers outpace the ring buffer.
- Bounded: Async backpressure propagates to callers. Safer. Cost: `.await` on send.

---

## Pattern 2: EventPoller + Stream (Sync → Async)

The inverse problem: consuming Ryuo events from async code. In Part 10, we built `EventPoller` for pull-based consumption. Now we wrap it in a `Stream` to plug into Tokio's ecosystem.

```
┌──────────┐     ┌──────────┐     ┌──────────────┐     ┌──────────────┐
│ Producer │ ──→ │  Ring     │ ──→ │ EventPoller  │ ──→ │ RyuoStream   │
│ (sync)   │     │  Buffer   │     │ (pull-based) │     │ (impl Stream)│
└──────────┘     └──────────┘     └──────────────┘     └──────────────┘
                                                              │
                                                              ▼
                                                       .next().await
                                                       (Tokio task)
```

### The Stream Adapter

```rust
use futures::stream::Stream;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use tokio::time::{sleep, Duration, Sleep};

/// A `Stream` that yields events from an `EventPoller`.
///
/// Each call to `poll_next` polls the underlying `EventPoller`.
/// If no events are available, the stream schedules a wake-up
/// after a configurable interval to avoid busy-waiting.
pub struct RyuoStream<T> {
    poller: EventPoller<T>,
    /// Buffered events from the last poll.
    /// EventPoller processes a batch, but Stream yields one at a time.
    buffer: Vec<(i64, T)>,
    /// Index into the buffer for the next yield.
    buffer_idx: usize,
    /// Delay before re-polling when idle.
    idle_delay: Duration,
    /// Whether we're currently sleeping.
    sleep_future: Option<Pin<Box<Sleep>>>,
}

impl<T: Clone> RyuoStream<T> {
    pub fn new(poller: EventPoller<T>, idle_delay: Duration) -> Self {
        Self {
            poller,
            buffer: Vec::with_capacity(64),
            buffer_idx: 0,
            idle_delay,
            sleep_future: None,
        }
    }

    /// Drain events from the poller into the internal buffer.
    fn fill_buffer(&mut self) -> PollState {
        self.buffer.clear();
        self.buffer_idx = 0;

        self.poller.poll(|event, seq, _eob| {
            // Clone the event out of the ring buffer.
            // This is the cost of the Stream abstraction.
            self.buffer.push((seq, event.clone()));
            true
        })
    }
}

impl<T: Clone + Unpin> Stream for RyuoStream<T> {
    type Item = (i64, T);

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // 1. If we have buffered events, yield the next one.
        if this.buffer_idx < this.buffer.len() {
            let item = this.buffer[this.buffer_idx].clone();
            this.buffer_idx += 1;
            return Poll::Ready(Some(item));
        }

        // 2. If we're sleeping (idle backoff), check the timer.
        if let Some(ref mut sleep) = this.sleep_future {
            match Pin::new(sleep).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(()) => {
                    this.sleep_future = None;
                    // Timer fired — try polling again
                }
            }
        }

        // 3. Poll the EventPoller for new events.
        match this.fill_buffer() {
            PollState::Processing => {
                // Events available — yield the first one
                let item = this.buffer[0].clone();
                this.buffer_idx = 1;
                Poll::Ready(Some(item))
            }
            PollState::Idle | PollState::Gating => {
                // No events — schedule a wake-up after idle_delay.
                // This prevents busy-waiting on the Tokio thread.
                let sleep = Box::pin(sleep(this.idle_delay));
                this.sleep_future = Some(sleep);
                // Re-poll to register the waker with the sleep future
                if let Some(ref mut sleep) = this.sleep_future {
                    let _ = Pin::new(sleep).poll(cx);
                }
                Poll::Pending
            }
        }
    }
}
```

**Why `T: Clone`?** The ring buffer owns its events. The `Stream` trait requires yielding owned values. We must clone events out of the ring buffer. This is the fundamental trade-off: `EventPoller`'s closure-based `poll()` is zero-copy (it borrows `&T`), but `Stream` requires ownership.

**Alternatives to cloning:**
1. **`T: Copy`** — For small types (primitives, fixed-size structs), `Copy` is zero-cost.
2. **`Arc<T>`** — Store `Arc<T>` in the ring buffer. Cloning an `Arc` is just a reference count increment (~5ns).
3. **Swap with default** — If `T: Default`, swap the event with a default value: `std::mem::take(event)`. Zero-copy but destructive.

```rust
// Option 3: Zero-copy swap (destructive)
impl<T: Default + Unpin> Stream for RyuoStream<T> {
    type Item = (i64, T);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        // ... same as above, but in fill_buffer:
        // self.buffer.push((seq, std::mem::take(event)));
        // The ring buffer slot now holds T::default()
    }
}
```

### Usage

```rust
use futures::StreamExt;

#[tokio::main]
async fn main() {
    let ring_buffer = Arc::new(RingBuffer::<OrderEvent>::new(1024));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(1024, vec![])
    );

    // Create poller and stream
    let poller = EventPoller::new(Arc::clone(&ring_buffer), Arc::clone(&sequencer));
    let mut stream = RyuoStream::new(poller, Duration::from_micros(100));

    // Producer (sync, on its own thread)
    let rb = Arc::clone(&ring_buffer);
    let seq = Arc::clone(&sequencer);
    std::thread::spawn(move || {
        for i in 0..1000 {
            rb.publish_with(&*seq, |event, _seq| {
                event.order_id = i;
            });
        }
    });

    // Consumer (async, on Tokio)
    let mut count = 0;
    while let Some((seq, event)) = stream.next().await {
        println!("Event {}: order_id={}", seq, event.order_id);
        count += 1;
        if count >= 1000 { break; }
    }
}
```

---

## Pattern 3: Hybrid (Sync Fast Path, Async Slow Path)

The most sophisticated pattern: sync producers bypass the channel entirely, while async producers use the channel. Both share the same ring buffer and consumer pipeline.

```
Sync Producer (HFT path):
  ring_buffer.publish_with(&sequencer, closure)
  Cost: ~50ns (direct ring buffer access)

Async Producer (I/O path):
  AsyncPublisher.publish_with() → channel → dedicated thread → ring buffer
  Cost: ~300ns (channel hop + thread wake)

                    ┌──────────────────────────────┐
                    │        Ring Buffer            │
                    │                              │
  Sync Producer ──→ │  slot[0] slot[1] ... slot[N] │ ──→ Consumer(s)
                    │                              │
  Async ──→ Channel ──→ Dedicated Thread ──→ ─────┘
                    └──────────────────────────────┘
```

### Implementation

```rust
/// Hybrid publisher: sync path for hot producers, async path for I/O.
///
/// Both paths write to the same ring buffer, so consumers see
/// a unified event stream.
pub struct HybridPublisher<T: Send + 'static> {
    /// Direct access for sync producers.
    ring_buffer: Arc<RingBuffer<T>>,
    sequencer: Arc<dyn Sequencer>,
    /// Channel-based access for async producers.
    async_publisher: AsyncPublisher<T>,
}

impl<T: Send + 'static> HybridPublisher<T> {
    pub fn new(
        ring_buffer: Arc<RingBuffer<T>>,
        sequencer: Arc<dyn Sequencer>,
    ) -> Self {
        let async_publisher = AsyncPublisher::new(
            Arc::clone(&ring_buffer),
            Arc::clone(&sequencer),
        );

        Self {
            ring_buffer,
            sequencer,
            async_publisher,
        }
    }

    /// Sync publish — zero overhead, for latency-critical paths.
    ///
    /// # Warning
    /// Do NOT call from async code. This calls `sequencer.claim()`
    /// which may spin-wait if the buffer is full, blocking the
    /// Tokio executor.
    pub fn publish_sync<F>(&self, f: F)
    where
        F: FnOnce(&mut T, i64),
    {
        self.ring_buffer.publish_with(&*self.sequencer, f);
    }

    /// Async publish — channel hop, for I/O-bound paths.
    pub async fn publish_async<F>(&self, f: F) -> Result<(), PublishError>
    where
        F: FnOnce(&mut T, i64) + Send + 'static,
    {
        self.async_publisher.publish_with(f).await
    }

    /// Get the async publisher for cloning into Tokio tasks.
    pub fn async_handle(&self) -> AsyncPublisher<T> {
        self.async_publisher.clone()
    }
}
```


### Usage: Mixed Workload

```rust
#[tokio::main]
async fn main() {
    let ring_buffer = Arc::new(RingBuffer::<TradeEvent>::new(4096));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        MultiProducerSequencer::new(4096, vec![])
    );

    let hybrid = Arc::new(HybridPublisher::new(
        Arc::clone(&ring_buffer),
        Arc::clone(&sequencer),
    ));

    // Sync path: market data feed (latency-critical)
    let h = Arc::clone(&hybrid);
    std::thread::spawn(move || {
        loop {
            let market_tick = receive_market_data(); // from NIC
            h.publish_sync(|event, seq| {
                event.source = Source::MarketData;
                event.price = market_tick.price;
                event.timestamp_ns = rdtsc(); // TSC timestamp
            });
        }
    });

    // Async path: REST API orders (I/O-bound)
    let async_pub = hybrid.async_handle();
    let app = axum::Router::new()
        .route("/order", axum::routing::post(move |body: Json<Order>| {
            let pub_clone = async_pub.clone();
            async move {
                pub_clone.publish_with(|event, seq| {
                    event.source = Source::RestApi;
                    event.order = body.0;
                }).await.unwrap();
                StatusCode::ACCEPTED
            }
        }));

    // Consumer sees both market data and REST orders
    // in a single, ordered event stream.
}
```

---

## Performance Characteristics

```
=== Publish Path Latency ===

Path                          p50        p99        Allocations
─────────────────────────────────────────────────────────────────
Sync (direct)                 ~50ns      ~100ns     0
Async (unbounded channel)     ~150ns     ~500ns     1 (Box<dyn FnOnce>)
Async (bounded channel)       ~200ns     ~1μs       1 (Box<dyn FnOnce>)

=== Consume Path Latency ===

Path                          p50        p99        Allocations
─────────────────────────────────────────────────────────────────
BatchEventProcessor (push)    ~50ns      ~100ns     0
EventPoller (pull, closure)   ~50ns      ~100ns     0
RyuoStream (pull, Stream)     ~100ns     ~300ns     1 per event (clone)
RyuoStream (Arc<T>)           ~55ns      ~120ns     0 (Arc clone)
```

**The overhead breakdown:**
- **Box allocation:** ~20-50ns for `Box::new(closure)` on the async publish path.
- **Channel send:** ~30-100ns for Tokio `mpsc::send()`.
- **Thread wake:** ~100-300ns if the publisher thread is sleeping (OS scheduler latency).
- **Event clone:** ~10-100ns depending on `T`'s size and complexity.

---

## When to Use Each Pattern

```
                    Is your producer async?
                           │
                    ┌──────┴──────┐
                   Yes            No
                    │              │
            Is latency critical   Use sync path directly
            (< 100ns)?            (no wrapper needed)
                    │
             ┌──────┴──────┐
            Yes            No
             │              │
      Don't use async.     Pattern 1:
      Restructure to       AsyncPublisher
      keep producer sync.  (dedicated thread)
                           │
                    Is your consumer async?
                           │
                    ┌──────┴──────┐
                   Yes            No
                    │              │
            Pattern 2:       Use BatchEventProcessor
            RyuoStream       (push-based, Part 6)
            (EventPoller     or EventPoller
             + Stream)       (pull-based, Part 10)
                           │
                    Need both sync and async producers?
                           │
                    ┌──────┴──────┐
                   Yes            No
                    │              │
            Pattern 3:       Pick Pattern 1 or 2
            HybridPublisher  based on your needs.
```

**Decision matrix:**

| Scenario | Pattern | Why |
|----------|---------|-----|
| gRPC → Ryuo → sync consumer | Pattern 1 | Async producer, sync consumer |
| Sync producer → Ryuo → Tokio Stream | Pattern 2 | Sync producer, async consumer |
| Market data (sync) + REST (async) → Ryuo | Pattern 3 | Mixed producers |
| HFT: everything sync | No wrapper | Use Ryuo directly |

---

## Common Pitfalls

### Pitfall 1: Blocking the Tokio Runtime

```rust
// ❌ WRONG: sequencer.claim() may spin-wait, blocking the executor
async fn bad_publish(rb: &RingBuffer<Event>, sequencer: &dyn Sequencer) {
    rb.publish_with(sequencer, |event, seq| { /* ... */ }); // blocks if buffer is full!
    // ...
}

// ✅ CORRECT: use AsyncPublisher (channel hop to dedicated thread)
async fn good_publish(publisher: &AsyncPublisher<Event>) {
    publisher.publish_with(|event, seq| {
        event.value = 42;
    }).await.unwrap();
}
```

### Pitfall 2: Using `tokio::task::spawn_blocking` Instead of a Dedicated Thread

```rust
// ❌ WRONG: spawn_blocking uses Tokio's blocking thread pool
// These threads are shared and may be exhausted by other blocking tasks.
tokio::task::spawn_blocking(move || {
    loop {
        let cmd = command_rx.blocking_recv(); // uses a shared pool thread
        // ...
    }
});

// ✅ CORRECT: dedicated OS thread (not shared, not managed by Tokio)
std::thread::Builder::new()
    .name("ryuo-publisher".into())
    .spawn(move || {
        while let Some(cmd) = command_rx.blocking_recv() {
            // ...
        }
    });
```

**Why?** Tokio's blocking thread pool has a default limit (512 threads). If your application has other `spawn_blocking` calls (e.g., file I/O), Ryuo's thread could be starved. A dedicated OS thread guarantees availability.

### Pitfall 3: RyuoStream Busy-Waiting

```rust
// ❌ WRONG: idle_delay of 0 causes 100% CPU when stream is idle
let stream = RyuoStream::new(poller, Duration::ZERO);

// ✅ CORRECT: tune idle_delay based on your latency tolerance
let stream = RyuoStream::new(poller, Duration::from_micros(100));
// p50 idle→processing latency: ~100μs (acceptable for most async consumers)
```

### Pitfall 4: Forgetting Multi-Producer Sequencer for Hybrid Pattern

```rust
// ❌ WRONG: SingleProducerSequencer with multiple writers
let sequencer = Arc::new(SingleProducerSequencer::new(1024, vec![]));
let hybrid = HybridPublisher::new(rb, sequencer);
// Sync thread writes directly, dedicated thread also writes → UB!

// ✅ CORRECT: MultiProducerSequencer when >1 writer
let sequencer = Arc::new(MultiProducerSequencer::new(1024, vec![]));
let hybrid = HybridPublisher::new(rb, sequencer);
```

---

## Testing

### Test 1: AsyncPublisher Delivers Events

```rust
#[tokio::test]
async fn async_publisher_delivers_events() {
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(8, vec![])
    );
    let poller = EventPoller::new(Arc::clone(&ring_buffer), Arc::clone(&sequencer));

    let publisher = AsyncPublisher::new(
        Arc::clone(&ring_buffer),
        Arc::clone(&sequencer),
    );

    // Publish from async context
    for i in 0..5u64 {
        publisher.publish_with(move |event, _seq| {
            *event = i * 10;
        }).await.unwrap();
    }

    // Give the dedicated thread time to process
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Verify events arrived
    let mut values = vec![];
    poller.poll(|event, _, _| {
        values.push(*event);
        true
    });

    assert_eq!(values, vec![0, 10, 20, 30, 40]);
}
```

### Test 2: RyuoStream Yields Events

```rust
#[tokio::test]
async fn ryuo_stream_yields_events() {
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(8, vec![])
    );
    let poller = EventPoller::new(Arc::clone(&ring_buffer), Arc::clone(&sequencer));
    let mut stream = RyuoStream::new(poller, Duration::from_millis(1));

    // Publish 3 events from sync context
    let rb = Arc::clone(&ring_buffer);
    let seq = Arc::clone(&sequencer);
    for i in 0..3u64 {
        rb.publish_with(&*seq, |event, _| {
            *event = i * 100;
        });
    }

    // Consume via Stream
    let mut results = vec![];
    for _ in 0..3 {
        if let Some((seq, value)) = stream.next().await {
            results.push((seq, value));
        }
    }

    assert_eq!(results, vec![(0, 0), (1, 100), (2, 200)]);
}
```

### Test 3: Shutdown Drains Pending Commands

```rust
#[tokio::test]
async fn shutdown_drains_pending() {
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(8, vec![])
    );
    let poller = EventPoller::new(Arc::clone(&ring_buffer), Arc::clone(&sequencer));

    let publisher = AsyncPublisher::new(
        Arc::clone(&ring_buffer),
        Arc::clone(&sequencer),
    );

    // Publish then immediately shutdown
    publisher.publish_with(|event, _| { *event = 42; }).await.unwrap();
    publisher.shutdown().await;

    // Give thread time to drain
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify event was delivered before shutdown
    let mut value = 0u64;
    poller.poll(|event, _, _| { value = *event; true });
    assert_eq!(value, 42);
}
```

### Test 4: AsyncPublisher Returns Error After Shutdown

```rust
#[tokio::test]
async fn publish_after_shutdown_returns_error() {
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(8, vec![])
    );

    let publisher = AsyncPublisher::new(
        Arc::clone(&ring_buffer),
        Arc::clone(&sequencer),
    );

    publisher.shutdown().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Publish after shutdown should fail
    let result = publisher.publish_with(|event, _| { *event = 99; }).await;
    assert!(matches!(result, Err(PublishError::ShuttingDown)));
}
```

---

## Key Takeaways

1. **Never run Ryuo on Tokio threads.** Spin-wait strategies starve other tasks. Mutex-based strategies block the executor. Always use dedicated OS threads for Ryuo's hot path.

2. **The dedicated thread pattern is the simplest bridge.** Async code publishes through a Tokio channel to a dedicated thread that writes to the ring buffer. ~100-500ns overhead per publish.

3. **`RyuoStream` bridges consumption to async, but requires cloning.** The `Stream` trait needs owned values. Use `Arc<T>`, `T: Copy`, or `std::mem::take()` to minimize clone cost.

4. **Hybrid publishers serve mixed workloads.** Sync producers (market data, NIC) bypass the channel. Async producers (REST, gRPC) use the channel. Both write to the same ring buffer.

5. **Use `MultiProducerSequencer` when multiple paths write.** The hybrid pattern has multiple writers (sync thread + dedicated async thread). `SingleProducerSequencer` is unsafe with multiple writers.

6. **Tune `idle_delay` for your latency tolerance.** `RyuoStream`'s `idle_delay` controls the sleep duration between polls when idle. Lower = faster reaction, higher CPU. Higher = less CPU, slower reaction.

---

## Series Conclusion

Over 16 posts, we've built Ryuo — a complete Disruptor implementation in Rust:

| Post | Component | Key Insight |
|------|-----------|-------------|
| 1 | Motivation | Queues are the bottleneck; pre-allocation + mechanical sympathy fix it |
| 2A-B | Ring Buffer | Power-of-2 masking, cache-line padding with `#[repr(align)]` |
| 3A-C | Sequencers | Atomic memory ordering, RAII sequence claims, multi-producer CAS |
| 4 | Wait Strategies | BusySpin → Yielding → Sleeping → Blocking (latency vs. CPU) |
| 5 | Sequence Barriers | Coordinating multi-consumer topologies (pipeline, diamond, multicast) |
| 6 | Event Handlers | Zero-cost dispatch via monomorphization, panic guards |
| 7 | Publishing | Closure-based publishing, batch publish with `ExactSizeIterator` |
| 8 | Batch Rewind | Retry failed batches without data loss, idempotency requirements |
| 9 | Multi-Producer | CAS contention, availability bitmap, gap detection |
| 10 | EventPoller | Pull-based consumption for integration with external event loops |
| 11 | Dynamic Topologies | Runtime reconfiguration, hot-swap handlers |
| 12 | Panic Handling | `PanicGuard` for RAII rollback, panic strategies (Halt, Log, Retry) |
| 13 | Builder DSL | Type-state pattern for compile-time validation, `disruptor!` macro |
| 14 | Production Patterns | Metrics, backpressure, circuit breaker, graceful shutdown |
| 15 | Benchmarking | RDTSC, HdrHistogram, coordinated omission correction, Welch's t-test |
| 16 | Async Integration | Dedicated thread, `Stream` adapter, hybrid sync/async |

**What makes Ryuo production-grade:**
- ✅ Zero-allocation hot path (pre-allocated ring buffer, no `Box<dyn FnOnce>` on sync path)
- ✅ Compile-time safety (type-state builder, `#[must_use]` on claims, `Send + Sync` bounds)
- ✅ Panic safety (RAII rollback, `catch_unwind` isolation)
- ✅ Observable (metrics for queue depth, latency, backpressure events)
- ✅ Tested (unit tests, Loom model checking, benchmark suite)
- ✅ Documented (`// SAFETY:` comments, design rationale in every post)

**The Disruptor pattern is not just a faster queue.** It's a fundamentally different approach to inter-thread communication: eliminate contention by design, use mechanical sympathy to exploit hardware, and make the compiler enforce correctness at compile time. Rust is uniquely suited to this — its ownership model, type system, and zero-cost abstractions let us build a Disruptor that is both safer and faster than the original Java implementation.

---

## References

### Async Rust

- [Tokio — Working with Blocking Code](https://tokio.rs/tokio/topics/bridging) — Official guide for sync/async bridging
- [tokio::sync::mpsc](https://docs.rs/tokio/latest/tokio/sync/mpsc/index.html) — The channel used in Pattern 1
- [futures::Stream](https://docs.rs/futures/latest/futures/stream/trait.Stream.html) — The trait implemented in Pattern 2

### Disruptor References

- [LMAX Disruptor](https://lmax-exchange.github.io/disruptor/) — The original Java implementation
- [Disruptor Technical Paper](https://lmax-exchange.github.io/disruptor/disruptor.html) — Martin Thompson et al.
- [Mechanical Sympathy](https://mechanical-sympathy.blogspot.com/) — Martin Thompson's blog

### Related Rust Crates

- [disruptor-rs](https://crates.io/crates/disruptor) — Another Rust Disruptor implementation
- [ringbuf](https://crates.io/crates/ringbuf) — Lock-free ring buffer (simpler, single-producer single-consumer)
- [crossbeam-channel](https://crates.io/crates/crossbeam-channel) — High-performance bounded/unbounded channels

---

*This concludes the Ryuo series. Thank you for reading — and for demanding 10/10.*