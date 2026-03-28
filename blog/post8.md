# Building a Disruptor in Rust: Ryuo — Part 8: Batch Rewind — Retry Without Data Loss

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 8 of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 6 (event handlers, `BatchEventProcessor`), Part 7 (publishing patterns)

---

## Recap

In Part 7, we built the producer side:
- **`publish_with` / `try_publish_with`** — Closure-based single-event publishing (blocking and non-blocking)
- **`publish_batch` / `try_publish_batch`** — Zero-allocation batch publishing with `ExactSizeIterator`
- **`LogEventPublisher`** — Reusable publisher pattern for domain-specific event types

But we left a gap on the consumer side. In Part 6, our `BatchEventProcessor` processes events with `PanicGuard` — if a handler panics, the sequence rolls back and the batch is retried. That handles *crashes*.

What about *transient failures*?

```
Producer → [Order 1] [Order 2] [Order 3] → Consumer
                                              ↓
                                         DB write fails
                                         (connection timeout)
                                              ↓
                                         What now?
```

Options:
1. **Skip the event** — Data loss. Unacceptable for financial systems.
2. **Panic** — Triggers `PanicGuard`, rewinds the batch, but panics are for bugs, not expected failures.
3. **Block forever** — Retry in a tight loop. No backoff, no escape hatch, no metrics.
4. **Signal a rewindable error** — The handler returns `Err(RewindableError)`, the processor consults a strategy, and either retries or gives up. ✅

This post implements option 4.

---

## The Problem Space

### Why Not Just Retry Inside the Handler?

```rust
// ❌ Naive approach: retry inside the handler
impl EventProcessor<OrderEvent> for DbWriter {
    fn on_event(&mut self, event: &mut OrderEvent, _seq: i64, _eob: bool) {
        loop {
            match self.db.write(event) {
                Ok(()) => return,
                Err(e) => {
                    eprintln!("DB write failed: {}, retrying...", e);
                    std::thread::sleep(Duration::from_millis(100));
                    // Problems:
                    // 1. No max retries — infinite loop on permanent failure
                    // 2. No metrics — invisible retry storms
                    // 3. No backoff — hammers the DB
                    // 4. Retry logic mixed with business logic
                    // 5. Sequence not advanced — blocks ALL downstream consumers
                }
            }
        }
    }
}
```

This violates separation of concerns. The handler should process events; the *processor* should handle retry policy. The Disruptor pattern separates these:

| Responsibility | Component |
|---------------|-----------|
| Event processing | `RewindableEventHandler` |
| Retry policy | `BatchRewindStrategy` |
| Event loop + rewind mechanics | `RewindableBatchProcessor` |

---

## Implementation

### Step 1: RewindableError

```rust
use std::error::Error;
use std::fmt;

/// Error that signals the batch should be rewound and retried.
///
/// Distinguished from panics (which indicate bugs) and fatal errors
/// (which should halt the processor). A `RewindableError` means:
/// "This failure is transient — try again and it might succeed."
///
/// # Examples
/// - Database connection timeout
/// - Network socket temporarily unavailable
/// - Lock contention (try_lock failed)
/// - Rate limit exceeded
#[derive(Debug)]
pub struct RewindableError {
    /// Human-readable description of what went wrong.
    pub message: String,
    /// Optional underlying error for error chain traversal.
    pub source: Option<Box<dyn Error + Send + Sync>>,
}

impl RewindableError {
    /// Create a rewindable error from a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    /// Create a rewindable error wrapping an underlying error.
    pub fn with_source(message: impl Into<String>, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

### Step 2: RewindAction and BatchRewindStrategy

```rust
/// Decision returned by a `BatchRewindStrategy` after a rewindable error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindAction {
    /// Rewind the batch and retry from the start.
    /// The processor resets `next_sequence` to `start_of_batch`.
    Rewind,

    /// Give up on this batch. The processor advances the sequence
    /// past the failed batch, effectively skipping those events.
    /// Use this as a last resort — it means data loss.
    Throw,
}

/// Strategy for deciding how to handle rewindable errors.
///
/// Implement this trait to control retry behavior:
/// - How many times to retry
/// - Whether to add delays between retries
/// - Whether to log, emit metrics, or alert
///
/// The strategy is stateful (`&mut self`) so it can track
/// per-sequence retry counts.
pub trait BatchRewindStrategy: Send + Sync {
    /// Called when a `RewindableError` occurs during batch processing.
    ///
    /// # Parameters
    /// - `error`: The rewindable error from the handler
    /// - `sequence`: The sequence number at the start of the failed batch
    ///
    /// # Returns
    /// `RewindAction::Rewind` to retry, `RewindAction::Throw` to skip.
    fn handle_rewind(&mut self, error: &RewindableError, sequence: i64) -> RewindAction;
}
```

**Why `&mut self`?** The strategy needs mutable state to track retry counts per sequence. An immutable strategy couldn't implement `EventuallyGiveUpStrategy`.

**Why `Send + Sync`?** The strategy lives inside the `RewindableBatchProcessor`, which runs on a dedicated thread. `Send` is required for the move; `Sync` ensures the strategy is safe if the processor is shared (though in practice it's owned by one thread).

### Step 3: Built-in Strategies

Three strategies cover the common cases. Each has a distinct failure mode:

#### AlwaysRewindStrategy

```rust
/// Always retry. Never give up.
///
/// ⚠️ Use only when you're certain the error will eventually resolve.
/// If the error is permanent, this creates an infinite retry loop.
pub struct AlwaysRewindStrategy;

impl BatchRewindStrategy for AlwaysRewindStrategy {
    fn handle_rewind(&mut self, _error: &RewindableError, _sequence: i64) -> RewindAction {
        RewindAction::Rewind
    }
}
```

**When to use:** Exactly one case — when the handler connects to a system with a strong availability guarantee and you'd rather block than lose data. Even then, pair it with an alert.

#### EventuallyGiveUpStrategy

```rust
use std::collections::HashMap;

/// Retry up to N times per batch, then skip.
///
/// Tracks retry counts per sequence number. When the batch succeeds
/// or is skipped, the count is automatically cleaned up.
pub struct EventuallyGiveUpStrategy {
    max_attempts: u32,
    attempts: HashMap<i64, u32>,
}

impl EventuallyGiveUpStrategy {
    pub fn new(max_attempts: u32) -> Self {
        assert!(max_attempts > 0, "max_attempts must be > 0");
        Self {
            max_attempts,
            attempts: HashMap::new(),
        }
    }
}

impl BatchRewindStrategy for EventuallyGiveUpStrategy {
    fn handle_rewind(&mut self, error: &RewindableError, sequence: i64) -> RewindAction {
        let attempts = self.attempts.entry(sequence).or_insert(0);
        *attempts += 1;

        if *attempts < self.max_attempts {
            eprintln!(
                "Rewind attempt {}/{} for sequence {}: {}",
                attempts, self.max_attempts, sequence, error
            );
            RewindAction::Rewind
        } else {
            eprintln!(
                "Giving up after {} attempts for sequence {}: {}",
                self.max_attempts, sequence, error
            );
            self.attempts.remove(&sequence);
            RewindAction::Throw
        }
    }
}
```

**Why `HashMap<i64, u32>`?** We need per-sequence retry counts because different batches can fail independently. A global counter would conflate retries across unrelated failures.

**Memory concern:** The map only holds entries for *currently-failing* sequences. On success, the entry is never created. On `Throw`, it's removed. In steady state (no failures), the map is empty.

#### BackoffRewindStrategy

```rust
use std::time::Duration;

/// Retry with exponential backoff, then give up.
///
/// Delay pattern: base_delay_ms × 2^(attempt-1)
/// Example with base_delay_ms=10, max_attempts=5:
///   Attempt 1: 10ms
///   Attempt 2: 20ms
///   Attempt 3: 40ms
///   Attempt 4: 80ms
///   Attempt 5: give up
pub struct BackoffRewindStrategy {
    max_attempts: u32,
    base_delay_ms: u64,
    attempts: HashMap<i64, u32>,
}

impl BackoffRewindStrategy {
    pub fn new(max_attempts: u32, base_delay_ms: u64) -> Self {
        assert!(max_attempts > 0, "max_attempts must be > 0");
        assert!(base_delay_ms > 0, "base_delay_ms must be > 0");
        Self {
            max_attempts,
            base_delay_ms,
            attempts: HashMap::new(),
        }
    }
}

impl BatchRewindStrategy for BackoffRewindStrategy {
    fn handle_rewind(&mut self, error: &RewindableError, sequence: i64) -> RewindAction {
        let attempts = self.attempts.entry(sequence).or_insert(0);
        *attempts += 1;

        if *attempts < self.max_attempts {
            let delay_ms = self.base_delay_ms.saturating_mul(2u64.saturating_pow(*attempts - 1));
            eprintln!(
                "Rewind attempt {}/{}, sleeping {}ms for sequence {}: {}",
                attempts, self.max_attempts, delay_ms, sequence, error
            );
            std::thread::sleep(Duration::from_millis(delay_ms));
            RewindAction::Rewind
        } else {
            eprintln!(
                "Giving up after {} attempts for sequence {}: {}",
                self.max_attempts, sequence, error
            );
            self.attempts.remove(&sequence);
            RewindAction::Throw
        }
    }
}
```

**Why `saturating_mul` and `saturating_pow`?** Prevents overflow on large attempt counts. Without saturation, `2u64.pow(63)` would overflow and wrap to 0, causing no delay. Saturation caps at `u64::MAX` — which is ~584 years in milliseconds. Safe enough.

**Strategy comparison:**

| Strategy | Retries | Delay | Risk |
|----------|---------|-------|------|
| `AlwaysRewind` | ∞ | None | Infinite loop on permanent failure |
| `EventuallyGiveUp` | N | None | Hammers failing system N times |
| `BackoffRewind` | N | Exponential | Adds latency (intentional) |

**Which to choose:**

```
Is the failure always transient?
├── Yes → AlwaysRewindStrategy (with alerts!)
└── No/Maybe
    ├── Is latency tolerance low? → EventuallyGiveUpStrategy
    └── Is the downstream system load-sensitive? → BackoffRewindStrategy
```


### Step 4: RewindableEventHandler

```rust
/// Event handler that can signal transient failures via `RewindableError`.
///
/// Unlike `EventProcessor<T>` (Part 6), which either succeeds or panics,
/// this trait returns `Result`. The processor uses the result to decide
/// whether to advance the sequence or rewind.
pub trait RewindableEventHandler<T>: Send {
    /// Process a single event. Return `Ok(())` to advance, or
    /// `Err(RewindableError)` to trigger batch rewind.
    ///
    /// # Parameters
    /// - `event`: Mutable reference to the event in the ring buffer
    /// - `sequence`: The event's sequence number
    /// - `end_of_batch`: True if this is the last event in the current batch
    ///
    /// # Contract
    /// This method **must be idempotent**. If a batch is rewound,
    /// all events in the batch — including already-processed ones —
    /// will be reprocessed.
    fn process_rewindable(&mut self, event: &mut T, sequence: i64, end_of_batch: bool)
        -> Result<(), RewindableError>;

    /// Called once when the processor thread starts.
    fn on_start(&mut self) {}

    /// Called once when the processor thread shuts down.
    fn on_shutdown(&mut self) {}
}
```

**The idempotency contract is the most important design decision in this post.** When a batch rewinds, *all* events from `start_of_batch` to the failure point are reprocessed. Events that already succeeded will be called again. If your handler is not idempotent, you get:

| Non-Idempotent Operation | Failure Mode |
|--------------------------|-------------|
| `counter += 1` | Double-counting |
| `db.insert(event)` | Duplicate rows |
| `send_email(event)` | Duplicate emails |
| `account.debit(amount)` | Double-debit |

### Step 5: RewindableBatchProcessor

The processor is structurally similar to `BatchEventProcessor` from Part 6, but adds rewind mechanics:

```rust
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// Batch processor with rewind support for transient failures.
///
/// Architecture:
/// ```text
/// ┌─────────────────────────────────────────────────────┐
/// │              RewindableBatchProcessor                │
/// │                                                     │
/// │  ┌─────────┐  ┌────────────┐  ┌──────────────────┐  │
/// │  │ Barrier  │→│  Handler   │→│ RewindStrategy   │  │
/// │  │wait_for()│  │process()   │  │handle_rewind()   │  │
/// │  └─────────┘  └────────────┘  └──────────────────┘  │
/// │       ↓             ↓                ↓              │
/// │   available    Ok(()) → advance  Rewind → retry     │
/// │   sequence     Err() → consult   Throw → skip       │
/// └─────────────────────────────────────────────────────┘
/// ```
pub struct RewindableBatchProcessor<T, H> {
    data_provider: Arc<RingBuffer<T>>,
    sequence_barrier: Arc<SequenceBarrier>,
    handler: H,
    sequence: Arc<AtomicI64>,
    rewind_strategy: Box<dyn BatchRewindStrategy>,
}

impl<T, H: RewindableEventHandler<T>> RewindableBatchProcessor<T, H> {
    pub fn new(
        data_provider: Arc<RingBuffer<T>>,
        sequence_barrier: Arc<SequenceBarrier>,
        handler: H,
        rewind_strategy: Box<dyn BatchRewindStrategy>,
    ) -> Self {
        Self {
            data_provider,
            sequence_barrier,
            handler,
            sequence: Arc::new(AtomicI64::new(-1)),
            rewind_strategy,
        }
    }

    /// Returns a reference to this processor's sequence.
    /// Used to register as a gating sequence on the sequencer.
    pub fn sequence(&self) -> Arc<AtomicI64> {
        Arc::clone(&self.sequence)
    }

    /// Main event loop. Runs until the barrier is alerted (shutdown).
    pub fn run(&mut self) {
        self.handler.on_start();

        let mut next_sequence = self.sequence.load(Ordering::Relaxed) + 1;

        loop {
            let start_of_batch = next_sequence;

            match self.sequence_barrier.wait_for(next_sequence) {
                Ok(available_sequence) => {
                    match self.process_batch(next_sequence, available_sequence) {
                        Ok(()) => {
                            // Batch succeeded — advance sequence
                            self.sequence.store(available_sequence, Ordering::Release);
                            next_sequence = available_sequence + 1;
                        }
                        Err(rewind_error) => {
                            // Batch failed — consult strategy
                            match self.rewind_strategy.handle_rewind(
                                &rewind_error,
                                start_of_batch,
                            ) {
                                RewindAction::Rewind => {
                                    // Retry from start of batch
                                    next_sequence = start_of_batch;
                                }
                                RewindAction::Throw => {
                                    // Skip the batch (data loss!)
                                    eprintln!(
                                        "Skipping batch [{}, {}] after rewind failure: {}",
                                        start_of_batch, available_sequence, rewind_error
                                    );
                                    self.sequence.store(
                                        available_sequence,
                                        Ordering::Release,
                                    );
                                    next_sequence = available_sequence + 1;
                                }
                            }
                        }
                    }
                }
                Err(BarrierError::Alerted) => break,
                Err(_) => break,
            }
        }

        self.handler.on_shutdown();
    }

    /// Process events from `start` to `end` (inclusive).
    fn process_batch(&mut self, start: i64, end: i64) -> Result<(), RewindableError> {
        let mut seq = start;
        while seq <= end {
            let event = self.data_provider.get_mut(seq);
            let end_of_batch = seq == end;
            self.handler.process_rewindable(event, seq, end_of_batch)?;
            seq += 1;
        }
        Ok(())
    }
}
```

**Key differences from `BatchEventProcessor` (Part 6):**

| Aspect | `BatchEventProcessor` | `RewindableBatchProcessor` |
|--------|----------------------|---------------------------|
| Handler trait | `EventProcessor<T>` (infallible) | `RewindableEventHandler<T>` (returns `Result`) |
| Failure handling | `PanicGuard` catches panics | Strategy pattern for explicit errors |
| On failure | Rolls back to last committed | Rolls back to start of batch |
| Recovery | Automatic (resume from rollback) | Configurable (rewind, skip, backoff) |
| Idempotency required? | Yes (due to `PanicGuard`) | Yes (explicitly documented) |

---

## Step-by-Step Rewind Trace

Let's trace a batch rewind with `EventuallyGiveUpStrategy(max_attempts=3)`:

```
Buffer: [E0] [E1] [E2] [E3] [E4] [E5] [E6] [E7]
Consumer sequence: -1
Strategy: EventuallyGiveUp(max=3), attempts={}

── Round 1 ────────────────────────────────────
barrier.wait_for(0) → available=3
start_of_batch = 0

  process E0 (seq=0) → Ok     ✓
  process E1 (seq=1) → Ok     ✓
  process E2 (seq=2) → Err(RewindableError("DB timeout"))  ✗

strategy.handle_rewind(error, sequence=0):
  attempts[0] = 1  (< 3 → Rewind)
  → RewindAction::Rewind

next_sequence = 0  (rewind to start_of_batch)
consumer sequence: still -1  (not advanced)

── Round 2 (retry) ────────────────────────────
barrier.wait_for(0) → available=3  (same batch)
start_of_batch = 0

  process E0 (seq=0) → Ok     ✓  ← REPROCESSED (must be idempotent!)
  process E1 (seq=1) → Ok     ✓  ← REPROCESSED
  process E2 (seq=2) → Err(RewindableError("DB timeout"))  ✗

strategy.handle_rewind(error, sequence=0):
  attempts[0] = 2  (< 3 → Rewind)
  → RewindAction::Rewind

next_sequence = 0  (rewind again)

── Round 3 (final retry) ──────────────────────
barrier.wait_for(0) → available=3
start_of_batch = 0

  process E0 (seq=0) → Ok     ✓
  process E1 (seq=1) → Ok     ✓
  process E2 (seq=2) → Err(RewindableError("DB timeout"))  ✗

strategy.handle_rewind(error, sequence=0):
  attempts[0] = 3  (= max → Throw)
  attempts.remove(0)
  → RewindAction::Throw

consumer sequence: stored 3 (Release)  ← BATCH SKIPPED
next_sequence = 4

── Round 4 (normal processing resumes) ────────
barrier.wait_for(4) → available=7
  process E4..E7 → Ok ✓✓✓✓
consumer sequence: stored 7
```

**Critical observation:** Events E0 and E1 were processed *three times* each. If the handler increments a counter, the counter is off by 2× per event. This is why the idempotency contract is non-negotiable.

---

## Idempotency Patterns

### Pattern 1: Sequence-Based Deduplication

```rust
struct IdempotentDbWriter {
    db: DatabaseConnection,
    processed: HashSet<i64>,
}

impl RewindableEventHandler<OrderEvent> for IdempotentDbWriter {
    fn process_rewindable(
        &mut self, event: &mut OrderEvent, sequence: i64, _eob: bool,
    ) -> Result<(), RewindableError> {
        // Skip if already processed in a previous attempt
        if self.processed.contains(&sequence) {
            return Ok(());
        }

        self.db.upsert_order(event).map_err(|e| {
            RewindableError::with_source("DB write failed", e)
        })?;

        self.processed.insert(sequence);
        Ok(())
    }
}
```

**Tradeoff:** Requires O(batch_size) memory for the processed set. Fine for typical batch sizes (hundreds to thousands), problematic for millions.

### Pattern 2: Idempotency Key in the Event

```rust
impl RewindableEventHandler<OrderEvent> for DbWriter {
    fn process_rewindable(
        &mut self, event: &mut OrderEvent, _sequence: i64, _eob: bool,
    ) -> Result<(), RewindableError> {
        // Use the event's natural key for upsert (INSERT ON CONFLICT UPDATE)
        // The DB ensures idempotency — same order_id always produces same result
        self.db
            .execute(
                "INSERT INTO orders (order_id, price, qty) VALUES ($1, $2, $3)
                 ON CONFLICT (order_id) DO UPDATE SET price=$2, qty=$3",
                &[&event.order_id, &event.price, &event.quantity],
            )
            .map_err(|e| RewindableError::with_source("DB upsert failed", e))?;

        Ok(())
    }
}
```

**Best practice:** Push idempotency to the downstream system (database upsert, PUT instead of POST). This eliminates the need for local deduplication state.

### Pattern 3: Effect Tracking

```rust
struct NotificationSender {
    notifier: NotificationService,
    sent_notifications: HashSet<u64>, // key: order_id
}

impl RewindableEventHandler<OrderEvent> for NotificationSender {
    fn process_rewindable(
        &mut self, event: &mut OrderEvent, _sequence: i64, _eob: bool,
    ) -> Result<(), RewindableError> {
        if self.sent_notifications.contains(&event.order_id) {
            return Ok(()); // Already sent — skip
        }

        self.notifier.send(event).map_err(|e| {
            RewindableError::with_source("Notification failed", e)
        })?;

        self.sent_notifications.insert(event.order_id);
        Ok(())
    }
}
```

---

## Observability

Rewind events are invisible by default. In production, you need to know:
- **How often** rewinds occur (frequency)
- **Which sequences** are failing (hot spots)
- **Whether retries succeed** (recovery rate)

Wrap any strategy with instrumentation using the decorator pattern:

```rust
/// Decorator that adds metrics to any BatchRewindStrategy.
pub struct InstrumentedRewindStrategy<S: BatchRewindStrategy> {
    inner: S,
    rewind_count: u64,
    throw_count: u64,
}

impl<S: BatchRewindStrategy> InstrumentedRewindStrategy<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            rewind_count: 0,
            throw_count: 0,
        }
    }

    pub fn rewind_count(&self) -> u64 { self.rewind_count }
    pub fn throw_count(&self) -> u64 { self.throw_count }
}

impl<S: BatchRewindStrategy> BatchRewindStrategy for InstrumentedRewindStrategy<S> {
    fn handle_rewind(&mut self, error: &RewindableError, sequence: i64) -> RewindAction {
        let action = self.inner.handle_rewind(error, sequence);

        match action {
            RewindAction::Rewind => {
                self.rewind_count += 1;
                // In production: counter!("ryuo.rewind.retries").increment(1);
            }
            RewindAction::Throw => {
                self.throw_count += 1;
                // In production: counter!("ryuo.rewind.failures").increment(1);
            }
        }

        action
    }
}
```

**Why a decorator instead of adding metrics to each strategy?** Separation of concerns. The retry logic shouldn't know about your metrics system. The decorator works with *any* `BatchRewindStrategy` — including custom ones.

**Production metrics to track:**

| Metric | Type | Alert Threshold |
|--------|------|----------------|
| `ryuo.rewind.retries` | Counter | > 10/min (something is degraded) |
| `ryuo.rewind.failures` | Counter | > 0 (data loss occurring!) |
| `ryuo.rewind.retry_duration_ms` | Histogram | p99 > 1000ms (backoff getting long) |

---

## Common Pitfalls

### Pitfall 1: Non-Idempotent Handlers

```rust
// ❌ NOT idempotent — counter increments on retry
impl RewindableEventHandler<Event> for BadHandler {
    fn process_rewindable(&mut self, event: &mut Event, _seq: i64, _eob: bool)
        -> Result<(), RewindableError>
    {
        self.counter += 1; // BUG: increments on retry!
        self.db.write(event).map_err(|e| RewindableError::with_source("fail", e))
    }
}

// ✅ Idempotent — uses sequence as dedup key
impl RewindableEventHandler<Event> for GoodHandler {
    fn process_rewindable(&mut self, event: &mut Event, seq: i64, _eob: bool)
        -> Result<(), RewindableError>
    {
        if !self.processed.contains(&seq) {
            self.counter += 1;
            self.db.write(event).map_err(|e| RewindableError::with_source("fail", e))?;
            self.processed.insert(seq);
        }
        Ok(())
    }
}
```

### Pitfall 2: Infinite Rewind Loops

```rust
// ❌ AlwaysRewind with permanent failure = infinite loop
let strategy = AlwaysRewindStrategy;
// If the DB is permanently down, this spins forever.

// ✅ Always set a max
let strategy = EventuallyGiveUpStrategy::new(10);
```

### Pitfall 3: Ignoring Rewind Metrics

```rust
// ❌ No visibility into rewind behavior
let strategy = BackoffRewindStrategy::new(5, 100);

// ✅ Wrap with instrumentation
let strategy = InstrumentedRewindStrategy::new(
    BackoffRewindStrategy::new(5, 100)
);
```

---

## Testing

### Test 1: Rewind Retries on Transient Failure

```rust
#[test]
fn rewind_retries_on_transient_failure() {
    use std::cell::Cell;

    struct FlakeyHandler {
        fail_count: Cell<u32>,
        calls: Vec<i64>,
    }

    impl RewindableEventHandler<u64> for FlakeyHandler {
        fn process_rewindable(&mut self, _event: &mut u64, seq: i64, _eob: bool)
            -> Result<(), RewindableError>
        {
            self.calls.push(seq);

            if seq == 2 && self.fail_count.get() < 2 {
                self.fail_count.set(self.fail_count.get() + 1);
                return Err(RewindableError::new("transient failure"));
            }
            Ok(())
        }
    }

    // Setup ring buffer with events 0..4
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let sequencer = SingleProducerSequencer::new(8, vec![]);
    let wait_strategy = Arc::new(BusySpinWaitStrategy);

    for i in 0..4u64 {
        ring_buffer.publish_with(&sequencer, |event, _| { *event = i; });
    }

    let barrier = Arc::new(SequenceBarrier::from_sequencer(&sequencer, &wait_strategy, vec![]));
    let handler = FlakeyHandler {
        fail_count: Cell::new(0),
        calls: vec![],
    };

    let mut processor = RewindableBatchProcessor::new(
        ring_buffer,
        barrier,
        handler,
        Box::new(EventuallyGiveUpStrategy::new(5)),
    );

    // Process — will fail twice on seq=2, then succeed on third try
    processor.run_once(); // processes 0,1,2(fail) → rewind
    processor.run_once(); // processes 0,1,2(fail) → rewind
    processor.run_once(); // processes 0,1,2(ok),3 → success

    // Sequences 0 and 1 were processed 3 times each
    assert_eq!(processor.handler.calls.iter().filter(|&&s| s == 0).count(), 3);
    assert_eq!(processor.handler.calls.iter().filter(|&&s| s == 1).count(), 3);
    assert_eq!(processor.handler.calls.iter().filter(|&&s| s == 2).count(), 3);
    assert_eq!(processor.handler.calls.iter().filter(|&&s| s == 3).count(), 1);
}
```

### Test 2: Eventually Gives Up After Max Attempts

```rust
#[test]
fn eventually_gives_up_after_max_attempts() {
    let mut strategy = EventuallyGiveUpStrategy::new(3);
    let error = RewindableError::new("permanent failure");

    assert_eq!(strategy.handle_rewind(&error, 0), RewindAction::Rewind);  // attempt 1
    assert_eq!(strategy.handle_rewind(&error, 0), RewindAction::Rewind);  // attempt 2
    assert_eq!(strategy.handle_rewind(&error, 0), RewindAction::Throw);   // attempt 3 → give up

    // After giving up, counter is reset for sequence 0
    assert_eq!(strategy.handle_rewind(&error, 0), RewindAction::Rewind);  // fresh start
}
```

### Test 3: Backoff Strategy Delays Increase Exponentially

```rust
#[test]
fn backoff_delays_increase_exponentially() {
    let mut strategy = BackoffRewindStrategy::new(4, 10);
    let error = RewindableError::new("timeout");

    let start = Instant::now();
    strategy.handle_rewind(&error, 0); // 10ms delay
    let elapsed_1 = start.elapsed();

    let start = Instant::now();
    strategy.handle_rewind(&error, 0); // 20ms delay
    let elapsed_2 = start.elapsed();

    let start = Instant::now();
    strategy.handle_rewind(&error, 0); // 40ms delay
    let elapsed_3 = start.elapsed();

    // Verify delays are roughly doubling (allow 5ms tolerance)
    assert!(elapsed_1.as_millis() >= 8);
    assert!(elapsed_2.as_millis() >= 18);
    assert!(elapsed_3.as_millis() >= 38);

    // Fourth attempt gives up (no delay)
    assert_eq!(strategy.handle_rewind(&error, 0), RewindAction::Throw);
}
```

### Test 4: Independent Sequence Tracking

```rust
#[test]
fn strategy_tracks_sequences_independently() {
    let mut strategy = EventuallyGiveUpStrategy::new(2);
    let error = RewindableError::new("fail");

    // Sequence 0: attempt 1
    assert_eq!(strategy.handle_rewind(&error, 0), RewindAction::Rewind);

    // Sequence 5: attempt 1 (independent of sequence 0)
    assert_eq!(strategy.handle_rewind(&error, 5), RewindAction::Rewind);

    // Sequence 0: attempt 2 → give up
    assert_eq!(strategy.handle_rewind(&error, 0), RewindAction::Throw);

    // Sequence 5: attempt 2 → give up (independent)
    assert_eq!(strategy.handle_rewind(&error, 5), RewindAction::Throw);
}
```

### Test 5: Instrumented Strategy Tracks Counts

```rust
#[test]
fn instrumented_strategy_tracks_counts() {
    let inner = EventuallyGiveUpStrategy::new(2);
    let mut strategy = InstrumentedRewindStrategy::new(inner);
    let error = RewindableError::new("fail");

    strategy.handle_rewind(&error, 0); // Rewind (attempt 1)
    strategy.handle_rewind(&error, 0); // Throw  (attempt 2)
    strategy.handle_rewind(&error, 5); // Rewind (attempt 1)

    assert_eq!(strategy.rewind_count(), 2);
    assert_eq!(strategy.throw_count(), 1);
}
```

---

## Performance Characteristics

| Operation | Cost | Notes |
|-----------|------|-------|
| `RewindableError` creation | ~50ns | One `String` allocation for message |
| Strategy lookup (HashMap) | ~10ns | O(1) amortized |
| Rewind (no backoff) | ~0ns | Just resets `next_sequence` |
| Rewind (with backoff) | Base delay × 2^(attempt-1) | Intentional latency |
| Successful batch | ~0ns overhead | No strategy consulted on success |

**The key insight:** Rewind adds zero overhead on the happy path. The `Result` return type is zero-cost when `Ok` — the compiler optimizes it to the same code as a direct return. Strategy consultation only occurs on `Err`.

---

## Key Takeaways

1. **`RewindableError` separates transient failures from bugs.** Panics mean "something is wrong with the code." `RewindableError` means "something is wrong with the environment — try again."

2. **The strategy pattern decouples retry policy from business logic.** Handlers process events; strategies decide what to do on failure. You can change retry behavior without touching handler code.

3. **Three built-in strategies cover most cases.** `AlwaysRewind` (never give up), `EventuallyGiveUp` (bounded retries), `BackoffRewind` (bounded retries with exponential delay).

4. **Idempotency is non-negotiable.** Batch rewind reprocesses all events in the batch, including already-succeeded ones. Use sequence-based dedup, database upserts, or effect tracking.

5. **Observability is critical.** Wrap strategies with `InstrumentedRewindStrategy` to track rewind frequency and failure rates. Alert on any `Throw` — it means data loss.

6. **Zero overhead on the happy path.** `Result<(), RewindableError>` compiles to the same code as a direct return when the handler succeeds. Strategy lookup only occurs on error.

---

## Next Up: Multi-Producer Contention

In **Part 9**, we'll tackle the multi-producer sequencer:

- **CAS-based sequence claiming** — How `fetch_add` vs. CAS affects contention
- **Per-slot availability tracking** — The `available_buffer` that prevents out-of-order visibility
- **Contention analysis** — What happens when 4, 8, 16 producers compete for the same `AtomicI64`
- **Batching as contention reduction** — Why `claim(100)` is 100× faster than 100× `claim(1)`

---

## References

### Rust Documentation

- [std::error::Error](https://doc.rust-lang.org/std/error/trait.Error.html) — Error trait and error chain
- [HashMap](https://doc.rust-lang.org/std/collections/struct.HashMap.html) — Hash map for per-sequence tracking
- [saturating_mul](https://doc.rust-lang.org/std/primitive.u64.html#method.saturating_mul) — Overflow-safe multiplication

### Java Disruptor Reference

- [BatchRewindStrategy.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/BatchRewindStrategy.java)
- [RewindableException.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/RewindableException.java)
- [RewindAction.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/RewindAction.java)

### Design Patterns

- [Strategy Pattern](https://refactoring.guru/design-patterns/strategy) — Behavioral pattern for interchangeable algorithms
- [Decorator Pattern](https://refactoring.guru/design-patterns/decorator) — Structural pattern for adding behavior (used for instrumentation)
- [Idempotency](https://en.wikipedia.org/wiki/Idempotence) — Mathematical property critical for retry safety

---

**Next:** [Part 9 — Multi-Producer: CAS, Contention, and Coordination Costs →](post9.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*