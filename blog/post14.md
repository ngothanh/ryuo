# Building a Disruptor in Rust: Ryuo — Part 14: Production Patterns — Monitoring, Backpressure, and Graceful Shutdown

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 14 of 16
**Target Audience:** Systems engineers deploying high-performance pipelines in production
**Prerequisites:** Part 7 (publishing), Part 13 (builder DSL)

---

## Recap

We've built the entire Disruptor — ring buffer, sequencers, barriers, handlers, publishing, panic handling, and a type-safe builder. But **building** a high-performance system and **running** one in production are different problems. This post covers the three production concerns the Disruptor outline doesn't address until you hit them in a real deployment:

1. **Monitoring:** How full is the ring buffer? How fast are events flowing? Where are the bottlenecks?
2. **Backpressure:** What happens when the producer is faster than the consumer?
3. **Graceful shutdown:** How do we stop without losing in-flight events?

Plus: **common anti-patterns** that kill latency in production.

---

## Monitoring: What to Measure

A ring buffer is a fixed-size structure. Unlike an unbounded queue, it can't silently grow until OOM kills your process. Instead, it **blocks the producer** when full. This is better (bounded memory), but it means you must monitor utilization to detect slowdowns before they cause backpressure.

### The Five Essential Metrics

| Metric | What It Measures | Alert Threshold | Why |
|--------|-----------------|-----------------|-----|
| `ryuo.queue_depth` | Events in buffer (cursor − min consumer) | > 75% capacity | Consumer falling behind |
| `ryuo.publish_latency_ns` | Time to claim + write + publish | p99 > 1μs (SP) or > 10μs (MP) | Producer contention |
| `ryuo.consume_latency_ns` | Time from publish to consume | p99 > 10μs | Handler performance |
| `ryuo.events_published` | Throughput (events/sec) | < expected rate | Producer stall |
| `ryuo.batch_size` | Events per `wait_for` return | Consistently 1 | Batching not working |

### InstrumentedRingBuffer

Wrap the ring buffer with metrics collection. The wrapper adds **~30-50ns overhead per publish** (two `Instant::now()` calls + metrics recording). For systems where this overhead is acceptable (>90% of production deployments), this is the easiest path to observability.

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

/// Wraps a ring buffer with production metrics.
///
/// Metrics are emitted using the `metrics` crate's macros
/// (`gauge!`, `counter!`, `histogram!`), which are zero-cost
/// when no metrics recorder is installed.
pub struct InstrumentedRingBuffer<T> {
    inner: Arc<RingBuffer<T>>,
    sequencer: Arc<dyn Sequencer>,
    consumer_sequences: Vec<Arc<AtomicI64>>,
}

impl<T> InstrumentedRingBuffer<T> {
    pub fn new(
        inner: Arc<RingBuffer<T>>,
        sequencer: Arc<dyn Sequencer>,
        consumer_sequences: Vec<Arc<AtomicI64>>,
    ) -> Self {
        Self { inner, sequencer, consumer_sequences }
    }

    /// Publish with latency and throughput tracking.
    pub fn publish_with<F>(&self, f: F)
    where
        F: FnOnce(&mut T, i64),
    {
        let start = Instant::now();

        // Snapshot queue depth before publish
        let cursor = self.sequencer.cursor();
        let min_consumer = self.get_min_consumer_sequence();
        let queue_depth = cursor - min_consumer;

        gauge!("ryuo.queue_depth").set(queue_depth as f64);

        // Delegate to inner ring buffer
        self.inner.publish_with(&*self.sequencer, f);

        // Record publish latency
        let elapsed_ns = start.elapsed().as_nanos() as f64;
        histogram!("ryuo.publish_latency_ns").record(elapsed_ns);
        counter!("ryuo.events_published").increment(1);
    }

    fn get_min_consumer_sequence(&self) -> i64 {
        self.consumer_sequences
            .iter()
            .map(|s| s.load(Ordering::Acquire))
            .min()
            .unwrap_or(-1)
    }
}
```

**Why `Acquire` on consumer sequences?** We need to see the consumer's latest progress. `Relaxed` could give us a stale value, causing the queue depth calculation to overestimate — leading to false alerts.

### Health Checks

For dashboards and alerting systems, a structured health check:

```rust
/// Ring buffer health status for monitoring dashboards.
#[derive(Debug, Clone)]
pub enum HealthStatus {
    /// Utilization < 75%. Normal operation.
    Healthy {
        utilization: f64,
        queue_depth: i64,
    },
    /// Utilization 75-90%. Consumer is falling behind.
    /// Action: Investigate consumer latency, consider adding capacity.
    Warning {
        utilization: f64,
        queue_depth: i64,
    },
    /// Utilization > 90%. Near backpressure.
    /// Action: Immediate investigation. Producer will stall soon.
    Critical {
        utilization: f64,
        queue_depth: i64,
    },
}

impl<T> InstrumentedRingBuffer<T> {
    pub fn health_check(&self) -> HealthStatus {
        let cursor = self.sequencer.cursor();
        let min_consumer = self.get_min_consumer_sequence();
        let queue_depth = cursor - min_consumer;
        let capacity = self.inner.buffer_size() as i64;

        let utilization = (queue_depth as f64 / capacity as f64) * 100.0;

        if utilization > 90.0 {
            HealthStatus::Critical { utilization, queue_depth }
        } else if utilization > 75.0 {
            HealthStatus::Warning { utilization, queue_depth }
        } else {
            HealthStatus::Healthy { utilization, queue_depth }
        }
    }
}
```

### Instrumented Consumer

Monitoring the consumer side is equally important:


---

## Backpressure: When the Producer Outpaces the Consumer

The ring buffer's fixed size is both its strength (bounded memory) and its constraint (backpressure). When all slots are occupied by unread events, the producer must wait. The default behavior — spinning until space is available — is often not what you want.

### The BackpressureStrategy Trait

```rust
/// Decides what to do when the ring buffer is full.
///
/// Called in the `Err(InsufficientCapacity)` branch of `try_claim`.
/// At this point, no slot has been claimed — the event has NOT been
/// written. The strategy operates purely on policy, not event content.
pub trait BackpressureStrategy: Send + Sync {
    fn handle_full(&self) -> BackpressureAction;
}

/// What the producer should do when the ring buffer is full.
#[derive(Debug, Clone)]
pub enum BackpressureAction {
    /// Spin/block until space is available. Default behavior.
    /// Guarantees no data loss but may stall the producer.
    Block,
    /// Drop the event. Accept data loss in exchange for
    /// non-blocking behavior. Track with `ryuo.events_dropped`.
    Drop,
    /// Statistical sampling: keep 1 in N events. Useful for
    /// metrics/logging where approximate data is acceptable.
    Sample(u32),
    /// Stop accepting events entirely. Use when downstream
    /// is unhealthy and sending more data would make it worse.
    CircuitBreak,
}

/// Publish error types for backpressure-aware publishing.
#[derive(Debug)]
pub enum PublishError {
    Dropped,
    Sampled,
    CircuitOpen,
    ShuttingDown,
}
```

### Built-in Strategies

```rust
/// Always block until space is available. Zero data loss,
/// but the producer may stall indefinitely if the consumer is dead.
pub struct BlockingBackpressure;

impl BackpressureStrategy for BlockingBackpressure {
    fn handle_full(&self) -> BackpressureAction {
        BackpressureAction::Block
    }
}

/// Drop events when the buffer is full. Use for non-critical
/// data (metrics, debug logging) where availability matters
/// more than completeness.
pub struct DropOnFullBackpressure;

impl BackpressureStrategy for DropOnFullBackpressure {
    fn handle_full(&self) -> BackpressureAction {
        counter!("ryuo.events_dropped").increment(1);
        BackpressureAction::Drop
    }
}

/// Adaptive strategy: adjusts behavior based on utilization.
///
/// - Below `sample_threshold`: block (normal operation)
/// - Between `sample_threshold` and `drop_threshold`: sample (1 in N)
/// - Above `drop_threshold`: drop all
pub struct AdaptiveBackpressure {
    sample_threshold: f64,  // Start sampling above this (e.g., 0.85)
    drop_threshold: f64,    // Start dropping above this (e.g., 0.95)
    sample_rate: u32,       // Keep 1 in N when sampling
    /// Shared utilization value updated externally (e.g., by a
    /// monitoring thread). Using `AtomicU32` scaled ×100 to avoid
    /// floating-point atomics.
    utilization_pct: Arc<AtomicU32>,
}

impl BackpressureStrategy for AdaptiveBackpressure {
    fn handle_full(&self) -> BackpressureAction {
        let util = self.utilization_pct.load(Ordering::Relaxed) as f64 / 100.0;

        if util > self.drop_threshold {
            counter!("ryuo.events_dropped").increment(1);
            BackpressureAction::Drop
        } else if util > self.sample_threshold {
            counter!("ryuo.events_sampled").increment(1);
            BackpressureAction::Sample(self.sample_rate)
        } else {
            BackpressureAction::Block
        }
    }
}
```

### Backpressure-Aware Publishing

```rust
impl<T> RingBuffer<T> {
    /// Try to publish with a backpressure policy.
    ///
    /// If the ring buffer is full, consults the `BackpressureStrategy`
    /// instead of spinning indefinitely.
    pub fn try_publish_with_backpressure<F>(
        &self,
        sequencer: &dyn Sequencer,
        backpressure: &dyn BackpressureStrategy,
        f: F,
    ) -> Result<(), PublishError>
    where
        F: FnOnce(&mut T, i64),
    {
        match sequencer.try_claim(1) {
            Ok(sequence) => {
                // Slot claimed — write the event
                let event = self.get_mut(sequence);
                f(event, sequence);
                sequencer.publish(sequence);
                Ok(())
            }
            Err(_insufficient_capacity) => {
                // No slot available — consult the strategy
                match backpressure.handle_full() {
                    BackpressureAction::Block => {
                        // Fall back to blocking publish
                        self.publish_with(sequencer, f);
                        Ok(())
                    }
                    BackpressureAction::Drop => {
                        Err(PublishError::Dropped)
                    }
                    BackpressureAction::Sample(n) => {
                        // Deterministic sampling using a fast PRNG
                        // (avoid rand dependency on the hot path)
                        let sample = self.sample_counter.fetch_add(1, Ordering::Relaxed);
                        if sample % n == 0 {
                            self.publish_with(sequencer, f);
                            Ok(())
                        } else {
                            Err(PublishError::Sampled)
                        }
                    }
                    BackpressureAction::CircuitBreak => {
                        Err(PublishError::CircuitOpen)
                    }
                }
            }
        }
    }
}
```

**Why deterministic sampling instead of `rand`?** On the hot path, calling `rand::random()` adds ~15-20ns (PRNG state update + branch). A simple modular counter (`fetch_add` + `%`) costs ~5ns and is deterministic — useful for reproducible benchmarks. For production, you could swap in a thread-local Xorshift if statistical properties matter.

### When to Use Each Strategy

| Strategy | Use Case | Data Loss | Latency Impact |
|----------|----------|-----------|---------------|
| `Block` | Financial transactions, order processing | None | Producer may stall |
| `Drop` | Metrics, debug logging, analytics | Yes | None — producer never blocks |
| `Sample(N)` | High-volume telemetry, monitoring | Partial (1/N kept) | None |
| `CircuitBreak` | Downstream failure, health check failed | Yes | None — fast failure |
| `Adaptive` | Mixed workloads, variable load | Depends on util | Degrades gracefully |

**Decision tree:**

```
Is every event critical?
├── Yes → Block (accept producer stalls)
│        └── Can you tolerate stalls?
│            ├── Yes → Block
│            └── No  → Increase buffer size or add consumers
└── No → What fraction of events can you lose?
         ├── None occasionally → Sample(100) for 1% loss
         ├── Up to 90% → Sample(10) for 90% loss
         └── All if needed → Drop (best-effort delivery)
```

---

## Graceful Shutdown: No Lost Events

The Disruptor has three shutdown phases:

```
Phase 1: Stop Accepting ──→ Phase 2: Drain ──→ Phase 3: Terminate
    │                           │                    │
    ├─ Set shutdown flag        ├─ Wait until        ├─ Alert barriers
    ├─ Reject new publishes     │  cursor == min     ├─ Join threads
    └─ Return error to caller   │  consumer          └─ Drop resources
                                └─ Timeout check
```

### Implementation

```rust
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Shutdown error types.
#[derive(Debug)]
pub enum ShutdownError {
    /// Ring buffer didn't drain within the timeout.
    /// In-flight events may be lost.
    DrainTimeout {
        remaining_events: i64,
    },
    /// A processor thread panicked during shutdown.
    ProcessorPanic(String),
}

/// Manages graceful shutdown of a Disruptor.
///
/// Usage:
/// ```rust
/// let shutdown = GracefulShutdown::new(Duration::from_secs(30));
/// // ... later ...
/// shutdown.shutdown(&disruptor)?;
/// ```
pub struct GracefulShutdown {
    /// Shared flag: producers check this before publishing.
    shutdown_signal: Arc<AtomicBool>,
    /// Maximum time to wait for consumers to catch up.
    drain_timeout: Duration,
}

impl GracefulShutdown {
    pub fn new(drain_timeout: Duration) -> Self {
        Self {
            shutdown_signal: Arc::new(AtomicBool::new(false)),
            drain_timeout,
        }
    }

    /// Returns a clone of the shutdown signal for use by publishers.
    pub fn signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown_signal)
    }

    /// Execute graceful shutdown.
    ///
    /// 1. Signal producers to stop publishing
    /// 2. Wait for ring buffer to drain (consumers catch up)
    /// 3. Alert all sequence barriers (wake up waiting consumers)
    /// 4. Join all processor threads
    pub fn shutdown<T>(
        &self,
        disruptor: DisruptorHandle<T>,
    ) -> Result<(), ShutdownError> {
        // Phase 1: Stop accepting new events
        self.shutdown_signal.store(true, Ordering::Release);

        // Phase 2: Wait for drain
        let start = Instant::now();
        loop {
            let cursor = disruptor.sequencer.cursor();
            let min_consumer = disruptor.get_min_consumer_sequence();

            if cursor == min_consumer {
                // All events consumed
                break;
            }

            if start.elapsed() > self.drain_timeout {
                let remaining = cursor - min_consumer;
                return Err(ShutdownError::DrainTimeout {
                    remaining_events: remaining,
                });
            }

            // Yield to let consumers make progress
            std::thread::sleep(Duration::from_millis(1));
        }

        // Phase 3: Terminate
        // Alert barriers to wake up any consumers blocked in wait_for()
        disruptor.sequencer.alert();

        // Join processor threads
        for handle in disruptor.processor_handles {
            handle.join().map_err(|e| {
                ShutdownError::ProcessorPanic(format!("{:?}", e))
            })?;
        }

        Ok(())
    }
}
```

**Why `sleep(1ms)` instead of busy-spin?** During shutdown, latency doesn't matter — we're winding down. Busy-spinning wastes CPU and delays consumer progress (especially on single-core machines or when the consumer shares a physical core with the shutdown thread).

### Publisher-Side Integration

```rust
/// A publisher that respects the shutdown signal.
pub struct GuardedPublisher<T> {
    ring_buffer: Arc<RingBuffer<T>>,
    sequencer: Arc<dyn Sequencer>,
    shutdown: Arc<AtomicBool>,
}

impl<T> GuardedPublisher<T> {
    /// Publish if not shutting down.
    pub fn publish_with<F>(&self, f: F) -> Result<(), PublishError>
    where
        F: FnOnce(&mut T, i64),
    {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(PublishError::ShuttingDown);
        }

        self.ring_buffer.publish_with(&*self.sequencer, f);
        Ok(())
    }
}
```

**TOCTOU note:** There's a tiny window between the `shutdown` check and the `publish_with` call where the shutdown signal could be set. This is acceptable — the event will be published and consumed during the drain phase. The alternative (locking around publish) would add latency to every publish, which defeats the purpose of the Disruptor.

---

## Circuit Breaker: Protecting Downstream Systems

When a handler calls an external service (database, network), that service may fail. Without protection, every event triggers a timeout, destroying pipeline throughput. The circuit breaker pattern prevents this:

```
Closed ─── failure ───→ Open ─── timeout ───→ Half-Open
  ↑                       │                       │
  └──── success ──────────┘                       │
  └──── success ──────────────────────────────────┘
  Open ←── failure ───────────────────────────────┘
```

```rust
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Circuit breaker for protecting downstream calls in event handlers.
///
/// Three states:
/// - `Closed`: Normal operation. Failures are counted.
/// - `Open`: All calls rejected. Transitions to HalfOpen after timeout.
/// - `HalfOpen`: One probe call allowed. Success → Closed, Failure → Open.
pub struct CircuitBreaker {
    state: Mutex<CircuitState>,
    failure_threshold: u32,
    reset_timeout: Duration,
}

struct CircuitState {
    status: CircuitStatus,
    failure_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CircuitStatus {
    Closed,
    Open { opened_at: Instant },
    HalfOpen,
}

#[derive(Debug)]
pub enum CircuitBreakerError<E> {
    /// Circuit is open — call was not attempted.
    Open,
    /// Call was attempted but failed.
    Failed(E),
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, reset_timeout: Duration) -> Self {
        Self {
            state: Mutex::new(CircuitState {
                status: CircuitStatus::Closed,
                failure_count: 0,
            }),
            failure_threshold,
            reset_timeout,
        }
    }

    /// Execute `f` through the circuit breaker.
    ///
    /// If the circuit is open, returns `Err(Open)` immediately
    /// without calling `f`.
    pub fn call<F, T, E>(&self, f: F) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Result<T, E>,
    {
        // Check state
        {
            let mut state = self.state.lock().unwrap();
            match state.status {
                CircuitStatus::Open { opened_at } => {
                    if opened_at.elapsed() > self.reset_timeout {
                        // Timeout expired — allow one probe
                        state.status = CircuitStatus::HalfOpen;
                    } else {
                        counter!("ryuo.circuit_breaker.rejected").increment(1);
                        return Err(CircuitBreakerError::Open);
                    }
                }
                _ => {}
            }
        }

        // Execute the call
        match f() {
            Ok(result) => {
                self.on_success();
                Ok(result)
            }
            Err(e) => {
                self.on_failure();
                Err(CircuitBreakerError::Failed(e))
            }
        }
    }

    fn on_success(&self) {
        let mut state = self.state.lock().unwrap();
        state.failure_count = 0;
        state.status = CircuitStatus::Closed;
    }

    fn on_failure(&self) {
        let mut state = self.state.lock().unwrap();
        state.failure_count += 1;

        if state.failure_count >= self.failure_threshold {
            state.status = CircuitStatus::Open {
                opened_at: Instant::now(),
            };
            counter!("ryuo.circuit_breaker.opened").increment(1);
            eprintln!(
                "Circuit breaker OPEN after {} consecutive failures",
                state.failure_count
            );
        }
    }
}
```

### Usage in a Handler

```rust
struct DatabasePersister {
    circuit_breaker: CircuitBreaker,
    db: DatabaseConnection,
}

impl EventConsumer<OrderEvent> for DatabasePersister {
    fn consume(&mut self, event: &OrderEvent, sequence: i64, _eob: bool) {
        match self.circuit_breaker.call(|| {
            self.db.insert_order(event)
        }) {
            Ok(()) => {
                // Persisted successfully
            }
            Err(CircuitBreakerError::Open) => {
                // Circuit is open — skip this event
                // It will be retried when the circuit closes
                // (if using RewindableError from Post 8)
                eprintln!("Circuit open, skipping seq {}", sequence);
            }
            Err(CircuitBreakerError::Failed(e)) => {
                eprintln!("DB write failed at seq {}: {:?}", sequence, e);
            }
        }
    }

    fn on_start(&mut self) {}
    fn on_shutdown(&mut self) {}
}
```

---

## Common Anti-Patterns

### ❌ Anti-Pattern 1: Blocking in Handlers

```rust
// BAD: Blocks the entire pipeline for 1 second per event
impl EventConsumer<Event> for SlowHandler {
    fn consume(&mut self, event: &Event, _seq: i64, _eob: bool) {
        std::thread::sleep(Duration::from_secs(1));
        // At 1 event/sec, your 10M events/sec pipeline is dead
    }
}
```

**Why it's dangerous:** The handler runs on a dedicated thread, but it blocks the sequencer's gating. If this handler holds sequence 5 for 1 second, the producer can't advance more than `buffer_size` slots past 5. The entire pipeline stalls.

**Fix:** Use the early release pattern from Part 6:

```rust
// GOOD: Release the sequence immediately, process asynchronously
impl EventConsumer<Event> for AsyncHandler {
    fn consume(&mut self, event: &Event, seq: i64, _eob: bool) {
        let data = event.data.clone(); // Clone the data we need
        let callback = Arc::clone(&self.done_signal);

        // Spawn async work — the event slot is released immediately
        std::thread::spawn(move || {
            // Do slow work here (network, disk I/O)
            process_slowly(data);
            callback.store(seq, Ordering::Release);
        });
    }
}
```

### ❌ Anti-Pattern 2: Ignoring Backpressure

```rust
// BAD: If the consumer is dead, this spins forever
loop {
    ring_buffer.publish_with(&sequencer, |event, _| {
        *event = get_next_event();
    });
}
```

**Fix:** Use `try_publish_with` or `try_publish_with_backpressure`:

```rust
// GOOD: Detect and handle full buffer
match ring_buffer.try_publish_with(&sequencer, |event, _| {
    *event = get_next_event();
}) {
    Ok(()) => { /* published */ }
    Err(InsufficientCapacity) => {
        counter!("ryuo.backpressure_events").increment(1);
        // Strategy: drop, log, backoff, alert, etc.
    }
}
```

### ❌ Anti-Pattern 3: Not Monitoring Queue Depth

```rust
// BAD: No visibility into ring buffer health
let disruptor = RingBufferBuilder::new()
    .buffer_size(1024)
    .single_producer()
    .build(|| Event::default())
    .handle_events_with(handler)
    .start();
// ... and hope for the best
```

**Fix:** Always monitor queue depth:

```rust
// GOOD: Track queue depth
let queue_depth = cursor - min_consumer;
gauge!("ryuo.queue_depth").set(queue_depth as f64);

// Alert on high utilization
let utilization = queue_depth as f64 / capacity as f64;
if utilization > 0.75 {
    warn!(
        "Ring buffer at {:.1}% capacity ({}/{})",
        utilization * 100.0, queue_depth, capacity
    );
}
```

### ❌ Anti-Pattern 4: Using the Wrong Wait Strategy in Production

```rust
// BAD: BusySpinWaitStrategy in a non-latency-critical service
// Burns 100% CPU on the consumer thread, even when idle
let disruptor = RingBufferBuilder::new()
    .buffer_size(1024)
    .single_producer()
    .wait_strategy(Box::new(BusySpinWaitStrategy))  // 100% CPU!
    .build(|| Event::default())
    .handle_events_with(log_handler)
    .start();
```

**Fix:** Match wait strategy to latency requirements:

| Wait Strategy | CPU Usage | Latency | Use Case |
|--------------|-----------|---------|----------|
| `BusySpinWaitStrategy` | 100% per consumer thread | ~50ns p99 | HFT, market data |
| `YieldingWaitStrategy` | 50-100% | ~100-500ns p99 | Low-latency services |
| `BlockingWaitStrategy` | <1% when idle | ~1-10μs p99 | Background processing, logging |

### ❌ Anti-Pattern 5: Undersized Buffer

```rust
// BAD: Buffer too small for burst traffic
let disruptor = RingBufferBuilder::new()
    .buffer_size(64)  // Only 64 slots!
    .single_producer()
    .build(|| Event::default())
    .handle_events_with(handler)
    .start();
// With 3 consumers, the producer can only be 64 events ahead
// of the slowest consumer. Any 65-event burst causes backpressure.
```

**Fix:** Size the buffer to absorb bursts:

```
Rule of thumb:
  buffer_size ≥ 4 × (burst_rate × max_consumer_latency)

Example:
  Burst rate: 100,000 events/sec
  Slowest consumer: 10ms processing time
  100,000 × 0.010 = 1,000 events in flight
  4 × 1,000 = 4,096 → use buffer_size = 4096

Always round up to next power of 2.
```

---

## Production Checklist

Before deploying a Disruptor-based pipeline:

| Category | Check | Status |
|----------|-------|--------|
| **Monitoring** | Queue depth metric exported | ☐ |
| | Publish latency histogram exported | ☐ |
| | Consumer latency histogram exported | ☐ |
| | Health check endpoint | ☐ |
| | Alert on >75% utilization | ☐ |
| **Backpressure** | Backpressure strategy chosen | ☐ |
| | `try_publish_with` used (not infinite publish loop) | ☐ |
| | Dropped/sampled events counted | ☐ |
| **Shutdown** | Graceful shutdown implemented | ☐ |
| | Drain timeout configured | ☐ |
| | Shutdown signal integrated with publisher | ☐ |
| **Sizing** | Buffer sized for burst traffic (4× rule) | ☐ |
| | Wait strategy matches latency requirements | ☐ |
| | Consumer count matches core count | ☐ |
| **Error Handling** | Panic strategy configured (Post 12) | ☐ |
| | Circuit breaker on external calls | ☐ |
| | Handler idempotency verified (Post 8) | ☐ |

---

## Testing

### Test 1: Health Check Returns Correct Status

```rust
#[test]
fn health_check_returns_correct_status() {
    let ring_buffer = Arc::new(RingBuffer::new(128, || 0u64)); // capacity 128
    let sequencer = Arc::new(SingleProducerSequencer::new(128, vec![]));

    // Simulate: cursor at 100, consumer at 0 → 100/128 = 78% → Warning
    let consumer_seq = Arc::new(AtomicI64::new(0));
    let instrumented = InstrumentedRingBuffer::new(
        ring_buffer,
        sequencer,
        vec![consumer_seq],
    );

    // After publishing 100 events with consumer at 0:
    match instrumented.health_check() {
        HealthStatus::Warning { utilization, queue_depth } => {
            assert!(utilization > 75.0);
            assert!(utilization <= 90.0);
        }
        other => panic!("Expected Warning, got {:?}", other),
    }
}
```

### Test 2: Circuit Breaker Opens After Threshold

```rust
#[test]
fn circuit_breaker_opens_after_failures() {
    let cb = CircuitBreaker::new(3, Duration::from_secs(10));

    // 3 failures → circuit opens
    for _ in 0..3 {
        let result: Result<(), CircuitBreakerError<&str>> =
            cb.call(|| Err("fail"));
        assert!(matches!(result, Err(CircuitBreakerError::Failed("fail"))));
    }

    // Next call should be rejected (circuit open)
    let result: Result<(), CircuitBreakerError<&str>> =
        cb.call(|| Ok(()));
    assert!(matches!(result, Err(CircuitBreakerError::Open)));
}
```

### Test 3: Circuit Breaker Resets After Timeout

```rust
#[test]
fn circuit_breaker_resets_after_timeout() {
    let cb = CircuitBreaker::new(1, Duration::from_millis(50));

    // 1 failure → circuit opens
    let _: Result<(), CircuitBreakerError<&str>> = cb.call(|| Err("fail"));

    // Rejected while open
    let result: Result<(), CircuitBreakerError<&str>> = cb.call(|| Ok(()));
    assert!(matches!(result, Err(CircuitBreakerError::Open)));

    // Wait for timeout
    std::thread::sleep(Duration::from_millis(60));

    // Should allow probe call (half-open)
    let result: Result<(), CircuitBreakerError<&str>> = cb.call(|| Ok(()));
    assert!(result.is_ok());

    // Circuit should be closed now
    let result: Result<(), CircuitBreakerError<&str>> = cb.call(|| Ok(()));
    assert!(result.is_ok());
}
```

### Test 4: Graceful Shutdown Drains Events

```rust
#[test]
fn graceful_shutdown_drains_events() {
    use std::sync::atomic::AtomicU64;

    struct CountingHandler {
        count: Arc<AtomicU64>,
    }

    impl EventConsumer<u64> for CountingHandler {
        fn consume(&mut self, _: &u64, _: i64, _: bool) {
            self.count.fetch_add(1, Ordering::Relaxed);
            // Simulate slow processing
            std::thread::sleep(Duration::from_millis(1));
        }
        fn on_start(&mut self) {}
        fn on_shutdown(&mut self) {}
    }

    let count = Arc::new(AtomicU64::new(0));
    let disruptor = RingBufferBuilder::<u64>::new()
        .buffer_size(256)
        .single_producer()
        .build(|| 0u64)
        .handle_events_with(CountingHandler { count: Arc::clone(&count) })
        .start();

    // Publish 50 events
    for i in 0..50 {
        disruptor.publish_with(|event, _| { *event = i; });
    }

    // Graceful shutdown with 10s timeout
    let shutdown = GracefulShutdown::new(Duration::from_secs(10));
    shutdown.shutdown(disruptor).expect("Shutdown should succeed");

    // All 50 events should have been processed
    assert_eq!(count.load(Ordering::Relaxed), 50);
}
```

### Test 5: Shutdown Timeout Returns Error

```rust
#[test]
fn shutdown_timeout_returns_error() {
    struct StuckHandler;

    impl EventConsumer<u64> for StuckHandler {
        fn consume(&mut self, _: &u64, _: i64, _: bool) {
            // Simulate a handler that never finishes
            std::thread::sleep(Duration::from_secs(60));
        }
        fn on_start(&mut self) {}
        fn on_shutdown(&mut self) {}
    }

    let disruptor = RingBufferBuilder::<u64>::new()
        .buffer_size(64)
        .single_producer()
        .build(|| 0u64)
        .handle_events_with(StuckHandler)
        .start();

    // Publish one event that will block the handler
    disruptor.publish_with(|event, _| { *event = 1; });

    // Shutdown with very short timeout
    let shutdown = GracefulShutdown::new(Duration::from_millis(100));
    let result = shutdown.shutdown(disruptor);
    assert!(matches!(result, Err(ShutdownError::DrainTimeout { .. })));
}
```

---

## Key Takeaways

1. **Monitor queue depth as the primary health indicator.** It tells you whether consumers are keeping up. Alert at 75%, investigate at 90%.

2. **Choose your backpressure strategy before deployment.** `Block` for critical data, `Drop` for metrics/logging, `Adaptive` for mixed workloads. Never use an infinite publish loop without backpressure awareness.

3. **Graceful shutdown has three phases.** Stop → Drain → Terminate. The drain phase is bounded by a timeout — don't wait forever for a stuck consumer.

4. **Circuit breakers protect the pipeline from downstream failures.** A 100ms database timeout × 1M events/sec = the entire pipeline stalls. The circuit breaker fast-fails and lets healthy handlers continue.

5. **Match wait strategy to workload.** `BusySpinWaitStrategy` is only justified for latency-critical paths where you can dedicate CPU cores. Everything else should use `BlockingWaitStrategy`.

6. **Size buffers for burst traffic, not average traffic.** The 4× rule: buffer_size ≥ 4 × (burst_rate × max_consumer_latency), rounded up to the next power of 2.

---

## Next Up: Benchmarking

In **Part 15**, we'll build a rigorous benchmarking harness:

- **HdrHistogram** — Accurate latency percentiles
- **Coordinated omission** — Why naive benchmarks lie
- **RDTSC** — Sub-nanosecond timing on x86_64
- **Comparison** — Ryuo vs `crossbeam`, `flume`, `tokio::mpsc`

---

## References

### Rust Crates

- [metrics](https://docs.rs/metrics/) — Metrics facade (like `log` but for metrics)
- [metrics-exporter-prometheus](https://docs.rs/metrics-exporter-prometheus/) — Prometheus exporter

### Patterns

- [Circuit Breaker Pattern](https://martinfowler.com/bliki/CircuitBreaker.html) — Martin Fowler's original description
- [Backpressure](https://mechanical-sympathy.blogspot.com/2012/05/apply-back-pressure-when-overloaded.html) — Martin Thompson on backpressure in messaging systems

### Java Disruptor Reference

- [Disruptor Shutdown](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/dsl/Disruptor.java#L198) — Java's `shutdown()` implementation

---

**Next:** [Part 15 — Benchmarking: HdrHistogram, Coordinated Omission, and RDTSC →](post15.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*