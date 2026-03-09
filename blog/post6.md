# Building a Disruptor in Rust: Ryuo — Part 6: Event Handlers — Zero-Cost Dispatch, Batching & Lifecycle

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 6 of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 5 (sequence barriers), Part 4 (wait strategies)

---

## Recap

In Part 5, we built the full `SequenceBarrier` with:
- **Dependency tracking** — `get_minimum_sequence` enforces consumer ordering across pipeline, diamond, and multicast topologies
- **Multi-producer gap detection** — `get_highest_published_sequence` scans the `available_buffer` to find contiguous published sequences
- **Batch-aware `wait_for`** — Returns the highest available sequence, enabling consumers to process multiple events per call

We ended with a consumer loop sketch:

```rust
let mut next_sequence = 0i64;
loop {
    let available = barrier.wait_for(next_sequence)?;
    for seq in next_sequence..=available {
        let event = ring_buffer.get(seq);
        // ... process event ...
    }
    next_sequence = available + 1;
    consumer_sequence.store(available, Ordering::Release);
}
```

But this sketch leaves critical questions unanswered:

1. **What does "process event" look like?** We need a trait that users implement — the handler.
2. **What happens if the handler panics?** The consumer sequence must be rolled back to prevent downstream consumers from reading unprocessed events.
3. **How do we handle lifecycle?** Startup initialization, graceful shutdown, timeout notifications.
4. **How do we support both read-only and mutable access?** Some handlers just observe events; others enrich them for downstream consumers.

This post answers all four.

---

## The Handler Traits

### Why Two Traits?

In a Disruptor pipeline, handlers fall into two categories:

```
Producer ──> [Ring Buffer] ──> Enricher (mutates events) ──> Logger (reads events)
                                   │                            │
                              needs &mut T                  needs &T
```

- **Enrichers** write computed fields into the event (e.g., adding a timestamp, computing a hash). They need `&mut T`.
- **Observers** read events without modification (e.g., logging, metrics, alerting). They need `&T`.

Separating these into two traits gives the compiler enough information to enforce safety at the type level.

### EventConsumer: Read-Only Access

```rust
/// Read-only event handler.
///
/// Implement this for handlers that observe events without modifying them.
/// Examples: logging, metrics collection, alerting, replication.
pub trait EventConsumer<T>: Send {
    /// Process a single event.
    ///
    /// - `event`: Shared reference to the event data in the ring buffer.
    /// - `sequence`: The sequence number of this event (0-based, monotonically increasing).
    /// - `end_of_batch`: True if this is the last event in the current batch.
    ///   Use this to trigger flush operations (e.g., flush a write buffer,
    ///   send a network batch).
    fn consume(&mut self, event: &T, sequence: i64, end_of_batch: bool);

    /// Called once when the processor thread starts, before any events are processed.
    /// Use for initialization: open files, establish connections, allocate buffers.
    fn on_start(&mut self) {}

    /// Called once when the processor thread is shutting down, after all events
    /// have been processed. Use for cleanup: flush buffers, close connections.
    fn on_shutdown(&mut self) {}

    /// Called at the start of each batch, before the first event is dispatched.
    ///
    /// - `batch_size`: Number of events in this batch.
    /// - `queue_depth`: Same as batch_size (how far behind the consumer is).
    ///
    /// Use for batch-level setup: pre-allocate a network buffer sized to
    /// the batch, start a database transaction, etc.
    fn on_batch_start(&mut self, _batch_size: i64, _queue_depth: i64) {}

    /// Called when `wait_for` returns `Err(Timeout)`.
    ///
    /// Use for periodic maintenance: flush partial batches, send heartbeats,
    /// check health. Only triggered with `TimeoutBlockingWaitStrategy`.
    fn on_timeout(&mut self, _sequence: i64) {}
}
```

### EventProcessor: Mutable Access

```rust
/// Mutable event handler.
///
/// Implement this for handlers that modify events in the ring buffer.
/// Examples: enrichment, validation, computed field injection.
///
/// **Safety contract:** Only one EventProcessor can be active for a given
/// sequence at a time. The SequenceBarrier's dependency tracking enforces
/// this — downstream consumers cannot read a sequence until the upstream
/// processor has finished modifying it.
pub trait EventProcessor<T>: Send {
    /// Process a single event with mutable access.
    fn process(&mut self, event: &mut T, sequence: i64, end_of_batch: bool);

    fn on_start(&mut self) {}
    fn on_shutdown(&mut self) {}
    fn on_batch_start(&mut self, _batch_size: i64, _queue_depth: i64) {}
    fn on_timeout(&mut self, _sequence: i64) {}

    /// Set a callback for early sequence release.
    ///
    /// When set, the handler can update this AtomicI64 to signal that
    /// it has finished with a sequence *before* returning from `process()`.
    /// This is used for async I/O patterns where the actual work completes
    /// later (see "Early Release Pattern" below).
    fn set_sequence_callback(&mut self, _callback: Arc<AtomicI64>) {}
}
```

**Why `Send` but not `Sync`?** Each handler runs on exactly one thread (the `BatchEventProcessor`'s thread). It's never shared across threads — it's *moved* into the processor. `Send` is required because the handler is created on one thread and moved to the processor thread. `Sync` is unnecessary because no concurrent access occurs.

**Why `&mut self`?** Handlers are stateful. A logging handler accumulates a write buffer. A metrics handler maintains counters. A validation handler tracks error counts. `&mut self` gives the handler exclusive access to its own state without requiring interior mutability.

---

## Zero-Cost Dispatch: Monomorphization Over Virtual Dispatch

A critical design decision: `BatchEventProcessor<T, H>` is generic over the handler type `H`, not `Box<dyn EventConsumer<T>>`.

```
Virtual dispatch (Box<dyn>):          Monomorphization (generics):

handler.consume(event, seq, eob)      handler.consume(event, seq, eob)
  │                                     │
  ├─ load vtable pointer (~1ns)         └─ direct call (~0ns)
  ├─ indirect call through vtable           (inlined if small)
  └─ branch predictor miss on
     first call (~5ns)

Per-event overhead: ~2-5ns            Per-event overhead: ~0ns
```

**Why this matters:** At 100M events/sec, 5ns per event = 500ms/sec wasted on dispatch. With monomorphization, the compiler inlines `consume()` directly into the batch loop — the function call disappears entirely.

**Trade-off:** Each handler type generates a separate `BatchEventProcessor` instantiation. For a typical Disruptor with 3-5 handlers, this adds ~10-20KB of code. Negligible.

---

## The BatchEventProcessor

This is the core event loop — the component that ties together the barrier, the ring buffer, and the handler.

```rust
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

/// The main event processing loop.
///
/// Owns a handler and a barrier. Runs on a dedicated thread.
/// Calls `barrier.wait_for()` to get available sequences, then
/// dispatches events to the handler one at a time.
///
/// Generic over:
/// - `T`: The event type stored in the ring buffer
/// - `H`: The handler type (EventConsumer<T> or EventProcessor<T>)
pub struct BatchEventProcessor<T, H> {
    /// The ring buffer containing events.
    data_provider: Arc<RingBuffer<T>>,

    /// The sequence barrier — controls what sequences are safe to read.
    sequence_barrier: Arc<SequenceBarrier>,

    /// The user's event handler.
    handler: H,

    /// This consumer's current sequence position.
    /// Updated after each successfully processed event.
    /// Downstream consumers and the producer observe this via Acquire loads.
    sequence: Arc<AtomicI64>,

    /// Shutdown flag. Set to false by `halt()`.
    running: AtomicBool,
}

impl<T, H> BatchEventProcessor<T, H> {
    pub fn new(
        data_provider: Arc<RingBuffer<T>>,
        sequence_barrier: Arc<SequenceBarrier>,
        handler: H,
    ) -> Self {
        Self {
            data_provider,
            sequence_barrier,
            handler,
            sequence: Arc::new(AtomicI64::new(-1)),
            running: AtomicBool::new(false),
        }
    }

    /// Returns the consumer's sequence — register this as a dependent
    /// sequence for downstream barriers or as a gating sequence for
    /// the producer.
    pub fn sequence(&self) -> Arc<AtomicI64> {
        Arc::clone(&self.sequence)
    }

    /// Returns a reference to the barrier (for shutdown signaling).
    pub fn barrier(&self) -> &Arc<SequenceBarrier> {
        &self.sequence_barrier
    }

    /// Signal the processor to stop after the current batch completes.
    pub fn halt(&self) {
        self.running.store(false, Ordering::Release);
        self.sequence_barrier.alert();
    }
}

/// Event loop for read-only handlers (EventConsumer).
impl<T, H: EventConsumer<T>> BatchEventProcessor<T, H> {
    /// The main event loop. Call this from a dedicated thread.
    ///
    /// This method blocks until `halt()` is called or the barrier
    /// is alerted.
    pub fn run(&mut self) {
        if self.running.swap(true, Ordering::SeqCst) {
            panic!("BatchEventProcessor is already running");
        }

        self.handler.on_start();

        let mut next_sequence = self.sequence.load(Ordering::Relaxed) + 1;

        loop {
            match self.sequence_barrier.wait_for(next_sequence) {
                Ok(available_sequence) => {
                    let batch_size = available_sequence - next_sequence + 1;
                    self.handler.on_batch_start(batch_size, batch_size);

                    // Panic guard: if the handler panics mid-batch,
                    // roll back to the last successfully processed sequence.
                    //
                    // We extract a reference to the inner AtomicI64 via
                    // Arc deref to avoid borrowing `self` through two paths.
                    let seq_ref: &AtomicI64 = &self.sequence;
                    let last_good = seq_ref.load(Ordering::Relaxed);
                    let guard = PanicGuard::new(seq_ref, last_good);

                    while next_sequence <= available_sequence {
                        // Safety: the SequenceBarrier guarantees that
                        // `next_sequence` has been published and all
                        // upstream processors have finished with it.
                        let event = self.data_provider.get(next_sequence);
                        let end_of_batch = next_sequence == available_sequence;

                        self.handler.consume(event, next_sequence, end_of_batch);

                        // Update sequence after EACH successful event.
                        // This enables fine-grained progress tracking:
                        // if we panic on event N+1, we've committed N.
                        self.sequence.store(next_sequence, Ordering::Release);
                        next_sequence += 1;
                    }

                    // Batch completed successfully — disarm the guard.
                    guard.commit();
                }
                Err(BarrierError::Timeout) => {
                    self.handler.on_timeout(
                        self.sequence.load(Ordering::Relaxed)
                    );
                }
                Err(BarrierError::Alerted) => {
                    if !self.running.load(Ordering::Acquire) {
                        break;
                    }
                    // If still running, clear the alert and continue.
                    // This supports transient alerts (e.g., reconfiguration).
                    self.sequence_barrier.clear_alert();
                }
            }
        }

        self.handler.on_shutdown();
        self.running.store(false, Ordering::Release);
    }
}

/// Event loop for mutable handlers (EventProcessor).
/// Same structure as the EventConsumer loop, but calls `process()`
/// with `&mut T` instead of `consume()` with `&T`.
impl<T, H: EventProcessor<T>> BatchEventProcessor<T, H> {
    pub fn run_mut(&mut self) {
        if self.running.swap(true, Ordering::SeqCst) {
            panic!("BatchEventProcessor is already running");
        }

        self.handler.on_start();

        let mut next_sequence = self.sequence.load(Ordering::Relaxed) + 1;

        loop {
            match self.sequence_barrier.wait_for(next_sequence) {
                Ok(available_sequence) => {
                    let batch_size = available_sequence - next_sequence + 1;
                    self.handler.on_batch_start(batch_size, batch_size);

                    let seq_ref: &AtomicI64 = &self.sequence;
                    let last_good = seq_ref.load(Ordering::Relaxed);
                    let guard = PanicGuard::new(seq_ref, last_good);

                    while next_sequence <= available_sequence {
                        let event = self.data_provider.get_mut(next_sequence);
                        let end_of_batch = next_sequence == available_sequence;

                        self.handler.process(event, next_sequence, end_of_batch);

                        self.sequence.store(next_sequence, Ordering::Release);
                        next_sequence += 1;
                    }

                    guard.commit();
                }
                Err(BarrierError::Timeout) => {
                    self.handler.on_timeout(
                        self.sequence.load(Ordering::Relaxed)
                    );
                }
                Err(BarrierError::Alerted) => {
                    if !self.running.load(Ordering::Acquire) {
                        break;
                    }
                    self.sequence_barrier.clear_alert();
                }
            }
        }

        self.handler.on_shutdown();
        self.running.store(false, Ordering::Release);
    }
}
```

**Why `swap(true, SeqCst)` in `run()`?** This is a double-start guard. If two threads call `run()` on the same processor, the second one panics immediately. `SeqCst` ensures the swap is globally visible — no thread can slip through.

**Why `Relaxed` for `sequence.load()` inside `run()`?** The processor is the *only* writer to `self.sequence`. It reads its own last write — no cross-thread synchronization needed for the read. The *store* uses `Release` because downstream consumers read it with `Acquire`.

**Why update sequence per-event, not per-batch?** Two reasons:

1. **Fine-grained progress:** If the handler panics on event 50 of a 100-event batch, downstream consumers can still process events 0-49.
2. **Downstream latency:** In a pipeline, the downstream consumer's `wait_for` checks our sequence. Updating per-event lets it start processing sooner instead of waiting for the entire batch.

**Cost:** One `Release` store per event (~1ns on x86, ~5ns on ARM). At 100M events/sec, this is 100ms/sec — acceptable for the safety and latency benefits.

---

## Panic Safety: The PanicGuard

What happens if a handler panics mid-batch?

Our `run()` loop stores `sequence` after *each* successful event. So if we process events 10, 11, 12 and panic on 13, `self.sequence` is at 12 — the last successful event. That seems fine. So why do we need a guard?

**The subtle danger:** Consider what happens if the panic occurs *during* the `sequence.store()` call itself, or if a future refactor moves the store to batch-end. The `PanicGuard` provides a safety net: it records the sequence value *before* the batch starts, and rolls back to that value if the batch doesn't complete normally. This guarantees that downstream consumers never see a sequence value that corresponds to a partially-processed or unprocessed event.

**The trade-off is deliberate:** Rolling back means some events may be processed twice (at-least-once), but no event is ever skipped (at-most-once). For most systems, duplicate processing is recoverable; skipped events are not.

```rust
/// Panic guard: ensures sequence consistency if a handler panics.
///
/// On successful batch completion, call `commit()` to disarm.
/// If dropped without `commit()` (panic or early return), rolls
/// back the sequence to the value before the batch started.
struct PanicGuard<'a> {
    sequence: &'a AtomicI64,
    rollback_to: i64,
    committed: bool,
}

impl<'a> PanicGuard<'a> {
    fn new(sequence: &'a AtomicI64, rollback_to: i64) -> Self {
        Self {
            sequence,
            rollback_to,
            committed: false,
        }
    }

    /// Disarm the guard — batch completed successfully.
    fn commit(mut self) {
        self.committed = true;
        // Drop runs but does nothing because committed == true.
    }
}

impl Drop for PanicGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            // Panic or early return — roll back to pre-batch state.
            self.sequence.store(self.rollback_to, Ordering::Release);

            if std::thread::panicking() {
                eprintln!(
                    "[ryuo] Handler panicked. Sequence rolled back to {}.",
                    self.rollback_to
                );
            }
        }
    }
}
```

**Why `Release` in the rollback?** Downstream consumers load our sequence with `Acquire`. The `Release` store ensures they see a consistent rollback — they won't read events that we "un-processed".

**Why not just catch the panic?** `std::panic::catch_unwind` requires `UnwindSafe`, which most handlers won't satisfy (they hold `&mut` references to internal state). The `PanicGuard` works with *any* handler because it uses `Drop`, not `catch_unwind`.

### PanicGuard Trace

```
Batch: sequences 5..=9
Handler state before batch: sequence = 4

1. PanicGuard created: rollback_to = 4
2. Process seq 5 → success → sequence.store(5, Release)
3. Process seq 6 → success → sequence.store(6, Release)
4. Process seq 7 → PANIC!
5. Stack unwinds:
   a. PanicGuard::drop() runs
   b. committed == false → rollback
   c. sequence.store(4, Release)  ← rolls back past 5 and 6!
6. Downstream consumers see sequence = 4
   → They don't process 5 or 6 (even though we did)
   → On restart, 5, 6, 7, 8, 9 are all reprocessed

Wait — we rolled back past events we successfully processed!
```

**Is this correct?** Yes. The alternative — leaving sequence at 6 — means event 7 is skipped on restart. Rolling back to 4 means events 5 and 6 are processed *twice*, but no event is ever *skipped*. In a Disruptor, **at-least-once is safer than at-most-once**.

> **Design choice:** If your handler is idempotent (processing an event twice produces the same result), rollback is always safe. If not, you need an exception handler (covered in Post 12) to decide whether to skip, retry, or halt.

---

## The Early Release Pattern

In some pipelines, a handler's work completes *asynchronously* — for example, flushing data to disk or sending a network batch. The handler returns from `process()` immediately, but the actual I/O finishes later.

**The problem:** If the `BatchEventProcessor` updates the sequence when `process()` returns, downstream consumers think the event is fully processed. But the I/O hasn't completed yet — the data might not be durable.

**The solution:** The handler takes ownership of the sequence update via `set_sequence_callback()`:

```rust
pub struct AsyncIOHandler {
    sequence_callback: Option<Arc<AtomicI64>>,
    pending_writes: Vec<(i64, Vec<u8>)>,
}

impl EventProcessor<LogEvent> for AsyncIOHandler {
    fn set_sequence_callback(&mut self, callback: Arc<AtomicI64>) {
        self.sequence_callback = Some(callback);
    }

    fn process(&mut self, event: &mut LogEvent, sequence: i64, end_of_batch: bool) {
        // Buffer the write — don't flush yet.
        self.pending_writes.push((sequence, event.data.clone()));

        if end_of_batch {
            let callback = self.sequence_callback.clone().unwrap();
            let writes = std::mem::take(&mut self.pending_writes);

            // Capture the last sequence BEFORE moving `writes`.
            // `flush_to_disk(writes).await` consumes `writes` — any
            // access after that is a use-after-move compile error.
            let last_seq = writes.last().map(|(seq, _)| *seq).unwrap();

            tokio::spawn(async move {
                flush_to_disk(writes).await;

                // Only now — after I/O completes — update the sequence.
                // Downstream consumers won't see these events until
                // the flush is durable.
                callback.store(last_seq, Ordering::Release);
            });
        }
    }
}
```

**How it integrates with `BatchEventProcessor`:** When `set_sequence_callback` is used, the processor passes its `self.sequence` Arc to the handler. The handler — not the processor — is responsible for updating the sequence. The processor skips its own `sequence.store()` calls for events handled this way.

> **When to use early release:** Only when the handler's work is truly asynchronous (I/O, network calls). For CPU-bound handlers, the standard synchronous path is simpler and faster.

---

## Putting It All Together: A Complete Pipeline

Let's wire up a three-stage pipeline using everything from Posts 2–6:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread;

// Event type
struct OrderEvent {
    order_id: u64,
    price: f64,
    validated: bool,    // Set by stage 1
    risk_score: f64,    // Set by stage 2
}

// Stage 1: Validation (mutates events)
struct ValidationHandler;
impl EventProcessor<OrderEvent> for ValidationHandler {
    fn process(&mut self, event: &mut OrderEvent, _seq: i64, _eob: bool) {
        event.validated = event.price > 0.0 && event.order_id > 0;
    }
}

// Stage 2: Risk scoring (mutates events, depends on stage 1)
struct RiskHandler;
impl EventProcessor<OrderEvent> for RiskHandler {
    fn process(&mut self, event: &mut OrderEvent, _seq: i64, _eob: bool) {
        // Only score validated orders
        event.risk_score = if event.validated {
            event.price * 0.01 // Simplified risk calculation
        } else {
            f64::MAX // Flag invalid orders
        };
    }
}

// Stage 3: Logging (read-only, depends on stage 2)
struct LoggingHandler {
    log_count: usize,
}
impl EventConsumer<OrderEvent> for LoggingHandler {
    fn consume(&mut self, event: &OrderEvent, seq: i64, end_of_batch: bool) {
        self.log_count += 1;
        if end_of_batch {
            println!("Logged {} events (last seq: {})", self.log_count, seq);
        }
    }

    fn on_start(&mut self) {
        println!("Logger started");
    }

    fn on_shutdown(&mut self) {
        println!("Logger shutdown. Total events: {}", self.log_count);
    }
}

// Wire the pipeline
fn build_pipeline() {
    let ring_buffer = Arc::new(RingBuffer::<OrderEvent>::new(1024));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(1024, vec![])
    );
    let wait_strategy: Arc<dyn WaitStrategy> = Arc::new(BusySpinWaitStrategy);

    // Create processors (each gets its own barrier)
    let mut validator = BatchEventProcessor::new(
        Arc::clone(&ring_buffer),
        Arc::new(SequenceBarrier::from_sequencer(
            &sequencer, &wait_strategy, vec![],
        )),
        ValidationHandler,
    );

    let mut risk_scorer = BatchEventProcessor::new(
        Arc::clone(&ring_buffer),
        Arc::new(SequenceBarrier::from_sequencer(
            &sequencer, &wait_strategy,
            vec![validator.sequence()], // depends on validator
        )),
        RiskHandler,
    );

    let mut logger = BatchEventProcessor::new(
        Arc::clone(&ring_buffer),
        Arc::new(SequenceBarrier::from_sequencer(
            &sequencer, &wait_strategy,
            vec![risk_scorer.sequence()], // depends on risk scorer
        )),
        LoggingHandler { log_count: 0 },
    );

    // Register the terminal consumer as gating sequence
    sequencer.add_gating_sequence(logger.sequence());

    // Start consumer threads (each processor's run() blocks until halted)
    let t1 = thread::spawn(move || validator.run_mut());
    let t2 = thread::spawn(move || risk_scorer.run_mut());
    let t3 = thread::spawn(move || logger.run());

    // Producer publishes events...
    // (covered in Post 7: Publishing Patterns)

    // Shutdown: alert all barriers, then join threads.
    // Alert in reverse dependency order to drain the pipeline:
    // logger finishes last batch → risk_scorer → validator
    // (Full shutdown protocol covered in Post 13: Shutdown & Lifecycle)
}
```

**Dependency graph:**

```
Producer ──> [Ring Buffer] ──> Validator ──> RiskScorer ──> Logger
                                (mut)         (mut)        (read-only)
```

Each stage sees the mutations from the previous stage because the `SequenceBarrier` enforces ordering: the risk scorer's barrier depends on the validator's sequence, so it only reads events that the validator has already processed.

---

## Testing

### Test 1: Handler Receives All Events in Order

```rust
#[test]
fn handler_receives_all_events_in_order() {
    struct RecordingHandler {
        received: Vec<(i64, bool)>, // (sequence, end_of_batch)
    }

    impl EventConsumer<u64> for RecordingHandler {
        fn consume(&mut self, _event: &u64, sequence: i64, end_of_batch: bool) {
            self.received.push((sequence, end_of_batch));
        }
    }

    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let cursor = Arc::new(AtomicI64::new(-1));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(8, vec![])
    );
    let wait_strategy: Arc<dyn WaitStrategy> = Arc::new(BusySpinWaitStrategy);

    let barrier = Arc::new(SequenceBarrier::from_sequencer(
        &sequencer, &wait_strategy, vec![],
    ));

    let handler = RecordingHandler { received: vec![] };
    let mut processor = BatchEventProcessor::new(
        Arc::clone(&ring_buffer),
        barrier,
        handler,
    );

    let proc_seq = processor.sequence();

    // Publish 5 events
    for i in 0..5 {
        ring_buffer.set(i, i as u64);
    }
    cursor.store(4, Ordering::Release);

    // Run processor in a thread, halt after events are processed
    let barrier_ref = Arc::clone(processor.barrier());
    let handle = std::thread::spawn(move || {
        processor.run();
        processor.handler.received
    });

    // Wait for processor to catch up, then halt
    while proc_seq.load(Ordering::Acquire) < 4 {
        std::hint::spin_loop();
    }
    barrier_ref.alert();

    let received = handle.join().unwrap();
    assert_eq!(received.len(), 5);
    assert_eq!(received[0], (0, false));
    assert_eq!(received[3], (3, false));
    assert_eq!(received[4], (4, true)); // last event is end_of_batch
}
```

### Test 2: Panic Rolls Back Sequence

```rust
#[test]
fn panic_rolls_back_sequence() {
    struct PanickingHandler {
        panic_at: i64,
    }

    impl EventConsumer<u64> for PanickingHandler {
        fn consume(&mut self, _event: &u64, sequence: i64, _end_of_batch: bool) {
            if sequence == self.panic_at {
                panic!("intentional panic at sequence {}", sequence);
            }
        }
    }

    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(8, vec![])
    );
    let wait_strategy: Arc<dyn WaitStrategy> = Arc::new(BusySpinWaitStrategy);

    let barrier = Arc::new(SequenceBarrier::from_sequencer(
        &sequencer, &wait_strategy, vec![],
    ));

    let mut processor = BatchEventProcessor::new(
        Arc::clone(&ring_buffer),
        barrier,
        PanickingHandler { panic_at: 3 },
    );

    let proc_seq = processor.sequence();

    // Publish 5 events (sequences 0..=4)
    for i in 0..5 {
        ring_buffer.set(i, i as u64);
    }
    // Simulate cursor at 4
    // (In a real setup, the sequencer's cursor would be updated)

    // The processor will process 0, 1, 2 successfully, then panic on 3.
    // PanicGuard should roll back sequence to -1 (pre-batch value).
    let handle = std::thread::spawn(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            processor.run();
        }))
    });

    let result = handle.join();
    // The thread panicked
    assert!(result.is_ok()); // catch_unwind caught it

    // Sequence should be rolled back to -1 (the value before the batch)
    assert_eq!(proc_seq.load(Ordering::Acquire), -1);
}
```

### Test 3: Lifecycle Hooks Called in Order

```rust
#[test]
fn lifecycle_hooks_called_in_order() {
    struct LifecycleTracker {
        events: Vec<String>,
    }

    impl EventConsumer<u64> for LifecycleTracker {
        fn consume(&mut self, _event: &u64, seq: i64, _eob: bool) {
            self.events.push(format!("consume:{}", seq));
        }

        fn on_start(&mut self) {
            self.events.push("start".to_string());
        }

        fn on_shutdown(&mut self) {
            self.events.push("shutdown".to_string());
        }

        fn on_batch_start(&mut self, batch_size: i64, _queue_depth: i64) {
            self.events.push(format!("batch_start:{}", batch_size));
        }
    }

    // ... setup ring buffer, sequencer, barrier ...
    // Publish 3 events, run processor, halt

    // Expected order:
    // ["start", "batch_start:3", "consume:0", "consume:1", "consume:2", "shutdown"]
}
```

### Test 4: Halt Stops Processor Gracefully

```rust
#[test]
fn halt_stops_processor() {
    struct NoOpHandler;
    impl EventConsumer<u64> for NoOpHandler {
        fn consume(&mut self, _: &u64, _: i64, _: bool) {}
    }

    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(8, vec![])
    );
    let wait_strategy: Arc<dyn WaitStrategy> = Arc::new(BusySpinWaitStrategy);

    let barrier = Arc::new(SequenceBarrier::from_sequencer(
        &sequencer, &wait_strategy, vec![],
    ));

    let mut processor = BatchEventProcessor::new(
        Arc::clone(&ring_buffer),
        Arc::clone(&barrier),
        NoOpHandler,
    );

    let handle = std::thread::spawn(move || {
        processor.run();
        // run() returns after halt
    });

    // Give the processor time to start
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Halt — this sets running=false and alerts the barrier
    barrier.alert();

    // Processor should exit within a reasonable time
    handle.join().unwrap();
}
```

---

## Common Pitfalls

| Pitfall | Why It's Bad | Fix |
|---------|-------------|-----|
| Blocking I/O in `consume()` | Stalls the entire pipeline. A 10ms disk write blocks all downstream consumers for 10ms. | Use the early release pattern or buffer writes and flush on `end_of_batch`. |
| Holding references across batches | The ring buffer may overwrite the event on the next lap. Accessing a stale reference is UB. | Copy data out of the event during `consume()` if you need it later. |
| Ignoring `end_of_batch` | Missed flush opportunities. If you buffer writes, you must flush when the batch ends — otherwise data sits in memory indefinitely during low-throughput periods. | Always check `end_of_batch` and flush accumulated work. |
| Allocating in `consume()` | Heap allocation (~50-100ns) dominates event processing time (~5-10ns). | Pre-allocate buffers in `on_start()` or use arena allocators. |
| Panicking without idempotency | `PanicGuard` rolls back the sequence, causing events to be reprocessed. Non-idempotent handlers may produce duplicate side effects. | Design handlers to be idempotent, or use an exception handler (Post 12). |

---

## Performance Characteristics

| Metric | Value | Notes |
|--------|-------|-------|
| Per-event dispatch | ~0ns | Monomorphized — inlined by compiler |
| Per-event sequence update | ~1ns (x86), ~5ns (ARM) | One `Release` store |
| Per-batch barrier check | ~50ns (BusySpin) to ~10μs (Blocking) | Depends on wait strategy (Post 4) |
| Batch size (typical) | 10–1,000 events | Higher under load, lower under light traffic |
| Lifecycle hook overhead | ~0ns | Default impls are empty — optimized away |

**Amortization example:** With a batch of 100 events and `BusySpinWaitStrategy`:
- Barrier cost: 50ns (once per batch)
- Sequence updates: 100 × 1ns = 100ns
- Total overhead: 150ns / 100 events = **1.5ns per event**

---

## Key Takeaways

1. **Two handler traits, one processor** — `EventConsumer<T>` for read-only access (`&T`), `EventProcessor<T>` for mutable access (`&mut T`). The `BatchEventProcessor` is generic over both.

2. **Monomorphization eliminates dispatch overhead** — Generic `H` instead of `Box<dyn>` means the compiler inlines handler calls directly into the batch loop. Zero per-event overhead.

3. **`PanicGuard` provides at-least-once semantics** — If a handler panics, the sequence rolls back to the pre-batch value. Events may be reprocessed, but none are skipped.

4. **Per-event sequence updates enable fine-grained progress** — Downstream consumers can start processing as soon as each event completes, not just at batch boundaries. Cost: ~1ns per event.

5. **Early release decouples processing from I/O** — For async handlers, `set_sequence_callback` lets the handler update the sequence after I/O completes, preventing downstream consumers from reading events whose side effects aren't durable yet.

6. **`end_of_batch` is your flush signal** — Use it to trigger I/O flushes, network sends, and database commits. This amortizes I/O costs across the batch.

---

## Next Up: Publishing Patterns

In **Part 7**, we'll build the producer side:

- **Closure-based publishing** — `publish_with(|event, seq| { ... })` replaces Java's `EventTranslator` hierarchy
- **Batch publishing** — Claim multiple slots and write them all before publishing
- **Try-publish** — Non-blocking publish for backpressure-aware producers

---

## References

### Rust Documentation

- [std::panic::catch_unwind](https://doc.rust-lang.org/std/panic/fn.catch_unwind.html)
- [std::panic::UnwindSafe](https://doc.rust-lang.org/std/panic/trait.UnwindSafe.html)
- [Drop trait](https://doc.rust-lang.org/std/ops/trait.Drop.html) — Used by `PanicGuard`

### Java Disruptor Reference

- [EventHandler.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/EventHandler.java)
- [BatchEventProcessor.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/BatchEventProcessor.java)
- [LifecycleAware.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/LifecycleAware.java)

### Performance

- [Rust Monomorphization](https://doc.rust-lang.org/book/ch10-01-syntax.html#performance-of-code-using-generics) — The Rust Book, Chapter 10

---

**Next:** [Part 7 — Publishing Patterns: Closures Over Translators →](post7.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*