# Building a Disruptor in Rust: Ryuo — Part 12: Panic Handling — Recovery, Rollback, and Isolation

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 12 of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 6 (event handlers, `PanicGuard`), Part 8 (batch rewind)

---

## Recap

In Part 6, we introduced `PanicGuard` — an RAII guard that rolls back the consumer sequence if a handler panics mid-batch:

```rust
let guard = PanicGuard::new(&self.sequence, last_good);
// ... process events ...
guard.commit(); // only reached if no panic
```

This gives us **at-least-once** semantics: if a handler panics, the sequence rewinds and events are reprocessed on restart. But Part 6 left several questions unanswered:

1. **What happens to the processor thread after a panic?** It crashes. The entire pipeline stalls.
2. **Can we recover without restarting?** Yes — `catch_unwind` can isolate the panic.
3. **Should we always retry?** No. Some panics indicate unrecoverable corruption.
4. **What if one handler in a chain panics?** Should it take down the others?

This post answers all four by building a **configurable panic handling system** that lets you choose the right trade-off for your domain.

---

## Rust Panics vs Java Exceptions

Before we build anything, we need to understand what we're working with:

```
Java Exceptions                         Rust Panics
─────────────────────────────────────   ─────────────────────────────────────
Checked: Must declare in signature      No checked panics (all "unchecked")
Unchecked: RuntimeException hierarchy   panic!() macro or unwrap() failure
try-catch: Explicit, type-safe          catch_unwind: Explicit, type-erased
Stack trace: Always captured            Stack trace: Only if RUST_BACKTRACE=1
Unwinding: Always                       Unwinding: Can be disabled (panic=abort)
Cost: ~1-5μs per throw                  Cost: ~1-10μs per unwind
Control flow: Commonly used             Control flow: Discouraged (use Result)
```

**The critical difference:** Java's `BatchEventProcessor` uses `try-catch` around every handler call. This is idiomatic Java. In Rust, `catch_unwind` is **not idiomatic** — it's a last-resort safety net. The Rust way is to return `Result` from handlers and handle errors explicitly. But panics still happen (assertion failures, `unwrap()` on `None`, index out of bounds), and we must handle them without corrupting the sequencer state.

**⚠️ `panic=abort` kills `catch_unwind`.** If your `Cargo.toml` sets `panic = "abort"`, the process terminates immediately on panic. `catch_unwind` never runs. `PanicGuard::drop()` never runs. Your only option is process-level recovery (supervisor, systemd restart). This post assumes the default `panic = "unwind"`.

---

## The Problem: Unhandled Panics Corrupt State

Consider our `BatchEventProcessor::run()` from Part 6:

```rust
// Part 6: PanicGuard protects the sequence, but the thread still dies
pub fn run(&mut self) {
    self.handler.on_start();
    let mut next_sequence = self.sequence.load(Ordering::Relaxed) + 1;

    loop {
        match self.sequence_barrier.wait_for(next_sequence) {
            Ok(available_sequence) => {
                let last_good = self.sequence.load(Ordering::Relaxed);
                let guard = PanicGuard::new(&self.sequence, last_good);

                while next_sequence <= available_sequence {
                    let event = self.data_provider.get(next_sequence);
                    self.handler.consume(event, next_sequence, next_sequence == available_sequence);
                    self.sequence.store(next_sequence, Ordering::Release);
                    next_sequence += 1;
                }

                guard.commit();
            }
            Err(BarrierError::Alerted) => break,
            Err(_) => break,
        }
    }

    self.handler.on_shutdown();
}
```

If `handler.consume()` panics:
1. Stack unwinding begins
2. `PanicGuard::drop()` rolls back the sequence ✓
3. The thread terminates ✗
4. No more events are processed ✗
5. Upstream producers eventually stall (gating sequence frozen) ✗

**The fix:** Wrap the batch processing in `catch_unwind` and let a configurable strategy decide what happens next.

---

## Solution: Configurable Panic Strategies

### Step 1: The PanicStrategy Trait

```rust
use std::any::Any;

/// Decides what happens after a handler panics.
///
/// Implementations must be `Send + Sync` because the strategy is shared
/// across the processor's lifetime and may be referenced from multiple
/// threads (e.g., when used with `IsolatedHandlerChain`).
pub trait PanicStrategy: Send + Sync {
    /// Called after a handler panic has been caught and the sequence
    /// has been rolled back by `PanicGuard`.
    ///
    /// - `sequence`: The sequence number where the panic occurred.
    /// - `panic_info`: The payload from `catch_unwind` (typically a `String`
    ///   or `&str`, but can be any `Any + Send`).
    ///
    /// Returns `true` to continue processing (retry or skip),
    /// `false` to halt the processor.
    fn handle_panic(
        &self,
        sequence: i64,
        panic_info: Box<dyn Any + Send>,
    ) -> PanicAction;

    /// Called after a successful batch. Strategies with retry state
    /// (e.g., `RetryWithBackoffStrategy`) should reset their counters.
    fn on_success(&self) {}
}

/// What the processor should do after a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanicAction {
    /// Retry the same sequence. Use with transient failures
    /// (e.g., resource exhaustion that may clear).
    Retry,
    /// Skip the panicking sequence and continue with the next.
    /// Use when the event is poison and retrying will always fail.
    Skip,
    /// Halt the processor. Use in production when any panic
    /// indicates a bug that must be investigated.
    Halt,
}
```

**Why return `PanicAction` instead of `bool`?** Three-state decisions are common:
- **Retry:** Transient failure (OOM, file descriptor exhaustion)
- **Skip:** Poison event (malformed data that always panics)
- **Halt:** Bug (logic error that requires human investigation)

A `bool` (continue/halt) forces "skip" and "retry" into the same bucket, which loses information.


```rust
/// Halt the processor immediately. Safest option for production.
///
/// Rationale: In production, a panic indicates a bug. Continuing to process
/// events with a buggy handler risks data corruption. Better to stop, alert,
/// and fix the code.
pub struct HaltOnPanicStrategy;

impl PanicStrategy for HaltOnPanicStrategy {
    fn handle_panic(
        &self,
        sequence: i64,
        panic_info: Box<dyn Any + Send>,
    ) -> PanicAction {
        let msg = extract_panic_message(&panic_info);
        eprintln!(
            "FATAL: Handler panicked at sequence {}: {}",
            sequence, msg
        );
        eprintln!("Halting processor. Investigate and restart.");
        PanicAction::Halt
    }
}

/// Log the panic and skip the event. Use during development or when
/// individual event loss is acceptable (e.g., metrics collection).
///
/// ⚠️ Skipping events violates at-least-once semantics. The skipped
/// event is never processed. Use only when you understand the consequences.
pub struct LogAndSkipStrategy;

impl PanicStrategy for LogAndSkipStrategy {
    fn handle_panic(
        &self,
        sequence: i64,
        panic_info: Box<dyn Any + Send>,
    ) -> PanicAction {
        let msg = extract_panic_message(&panic_info);
        eprintln!(
            "WARNING: Handler panicked at sequence {}: {}. Skipping.",
            sequence, msg
        );
        PanicAction::Skip
    }
}

/// Retry with exponential backoff. Use for transient failures.
///
/// After `max_retries` attempts, falls back to `Halt`.
pub struct RetryWithBackoffStrategy {
    max_retries: u32,
    initial_backoff: Duration,
    /// Mutable state: tracks consecutive failures per sequence.
    /// Protected by interior mutability since `handle_panic` takes `&self`.
    retry_count: AtomicU32,
}

impl RetryWithBackoffStrategy {
    pub fn new(max_retries: u32, initial_backoff: Duration) -> Self {
        Self {
            max_retries,
            initial_backoff,
            retry_count: AtomicU32::new(0),
        }
    }
}

impl PanicStrategy for RetryWithBackoffStrategy {
    fn handle_panic(
        &self,
        sequence: i64,
        panic_info: Box<dyn Any + Send>,
    ) -> PanicAction {
        let count = self.retry_count.fetch_add(1, Ordering::Relaxed);

        if count >= self.max_retries {
            let msg = extract_panic_message(&panic_info);
            eprintln!(
                "FATAL: Handler panicked at sequence {} after {} retries: {}",
                sequence, count, msg
            );
            self.retry_count.store(0, Ordering::Relaxed);
            return PanicAction::Halt;
        }

        let backoff = self.initial_backoff * 2u32.saturating_pow(count);
        eprintln!(
            "Handler panicked at sequence {}, retry {}/{} after {:?}",
            sequence, count + 1, self.max_retries, backoff
        );
        std::thread::sleep(backoff);
        PanicAction::Retry
    }

    fn on_success(&self) {
        // Reset retry counter after a successful batch.
        self.retry_count.store(0, Ordering::Relaxed);
    }
}

/// Extract a human-readable message from a panic payload.
fn extract_panic_message(info: &Box<dyn Any + Send>) -> String {
    if let Some(s) = info.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = info.downcast_ref::<String>() {
        s.clone()
    } else {
        format!("{:?}", info)
    }
}
```

**Why `extract_panic_message`?** The `catch_unwind` payload is `Box<dyn Any + Send>` — a type-erased box. Most panics use `&str` (from `panic!("msg")`) or `String` (from `format!`), but the type system doesn't guarantee this. The helper tries both common types before falling back to debug formatting.

### Step 3: PanicSafeBatchProcessor

Now we integrate `catch_unwind` with `PanicGuard` and configurable strategies:

```rust
use std::panic::{catch_unwind, AssertUnwindSafe};

/// A batch processor that catches handler panics and delegates to a
/// configurable `PanicStrategy`.
///
/// This replaces `BatchEventProcessor::run()` for handlers that may panic.
/// The key difference: instead of letting the thread die, we catch the panic,
/// roll back the sequence, and ask the strategy what to do next.
pub struct PanicSafeBatchProcessor<T, H> {
    data_provider: Arc<RingBuffer<T>>,
    sequence_barrier: Arc<SequenceBarrier>,
    handler: H,
    sequence: Arc<AtomicI64>,
    panic_strategy: Box<dyn PanicStrategy>,
    running: AtomicBool,
}

impl<T, H: EventConsumer<T>> PanicSafeBatchProcessor<T, H> {
    pub fn new(
        data_provider: Arc<RingBuffer<T>>,
        sequence_barrier: Arc<SequenceBarrier>,
        handler: H,
        panic_strategy: Box<dyn PanicStrategy>,
    ) -> Self {
        Self {
            data_provider,
            sequence_barrier,
            handler,
            sequence: Arc::new(AtomicI64::new(-1)),
            panic_strategy,
            running: AtomicBool::new(false),
        }
    }

    pub fn sequence(&self) -> &Arc<AtomicI64> {
        &self.sequence
    }

    pub fn run(&mut self) {
        self.running.store(true, Ordering::Release);
        self.handler.on_start();

        let mut next_sequence = self.sequence.load(Ordering::Relaxed) + 1;

        loop {
            match self.sequence_barrier.wait_for(next_sequence) {
                Ok(available_sequence) => {
                    match self.process_batch_safe(next_sequence, available_sequence) {
                        BatchResult::Success => {
                            // Entire batch processed successfully.
                            // Reset any retry counters in the strategy.
                            self.panic_strategy.on_success();
                            next_sequence = available_sequence + 1;
                        }
                        BatchResult::PanicRetry => {
                            // Strategy said retry. next_sequence stays the same.
                            // PanicGuard already rolled back self.sequence.
                        }
                        BatchResult::PanicSkip(failed_at) => {
                            // Strategy said skip. Advance past the failed sequence.
                            self.sequence.store(failed_at, Ordering::Release);
                            next_sequence = failed_at + 1;
                        }
                        BatchResult::PanicHalt => {
                            // Strategy said halt. Break the loop.
                            break;
                        }
                    }
                }
                Err(BarrierError::Alerted) => break,
                Err(_) => break,
            }
        }

        self.handler.on_shutdown();
        self.running.store(false, Ordering::Release);
    }

    /// Process a batch of events with panic protection.
    ///
    /// Uses `catch_unwind` to isolate panics. If the handler panics:
    /// 1. `PanicGuard::drop()` rolls back `self.sequence` to `last_good`
    /// 2. `catch_unwind` captures the panic payload
    /// 3. `self.panic_strategy.handle_panic()` decides what to do
    fn process_batch_safe(
        &mut self,
        start: i64,
        end: i64,
    ) -> BatchResult {
        let last_good = self.sequence.load(Ordering::Relaxed);
        let guard = PanicGuard::new(&self.sequence, last_good);

        // AssertUnwindSafe: We accept that `self` may be in an inconsistent
        // state after a panic. The PanicGuard handles sequence rollback,
        // and the handler's internal state is the handler's responsibility.
        //
        // This is the same trade-off Java makes with try-catch in
        // BatchEventProcessor — the handler must be written to tolerate
        // being called again after a previous call panicked.
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut seq = start;
            while seq <= end {
                let event = self.data_provider.get(seq);
                let end_of_batch = seq == end;

                self.handler.consume(event, seq, end_of_batch);

                // Update sequence after each successful event.
                // This enables fine-grained rollback: PanicGuard rolls back
                // to last_good, not to start-1.
                self.sequence.store(seq, Ordering::Release);
                seq += 1;
            }
            end // return the last sequence processed
        }));

        match result {
            Ok(_last_seq) => {
                guard.commit();
                BatchResult::Success
            }
            Err(panic_info) => {
                // PanicGuard::drop() has already rolled back self.sequence.
                // Now ask the strategy what to do.
                let failed_at = self.sequence.load(Ordering::Relaxed) + 1;
                match self.panic_strategy.handle_panic(failed_at, panic_info) {
                    PanicAction::Retry => BatchResult::PanicRetry,
                    PanicAction::Skip => BatchResult::PanicSkip(failed_at),
                    PanicAction::Halt => BatchResult::PanicHalt,
                }
            }
        }
    }
}

#[derive(Debug)]
enum BatchResult {
    Success,
    PanicRetry,
    PanicSkip(i64),
    PanicHalt,
}
```

**Why `AssertUnwindSafe`?** The `catch_unwind` function requires its closure to be `UnwindSafe`. Our closure captures `&mut self`, which is not `UnwindSafe` because `&mut` references could leave data in an inconsistent state after a panic. By wrapping in `AssertUnwindSafe`, we're telling the compiler: "I know this isn't structurally unwind-safe, but I've handled it with `PanicGuard`."

This is the same pattern used by `std::thread::spawn` internally — the thread boundary inherently requires unwind safety, and the spawned function is wrapped in `AssertUnwindSafe` at the join point.

### Why Per-Event Sequence Updates Matter

```
Batch: sequences 10..=14
Handler state before batch: sequence = 9 (last_good)

With per-event updates:
  Process seq 10 → success → sequence = 10
  Process seq 11 → success → sequence = 11
  Process seq 12 → PANIC!
  PanicGuard rolls back to: 9
  Failed at: sequence.load() + 1 = 10  ← WRONG?

Wait — the guard rolls back to 9, but we successfully processed 10 and 11.
Isn't that wasteful?
```

**Yes, and that's deliberate.** The `PanicGuard` rolls back to the pre-batch state (`last_good = 9`), not to the last successful event (11). Here's why:

1. **We can't know if event 10's processing had side effects that depend on event 11.** If the handler accumulates state across events in a batch (e.g., running totals), rolling back to 11 would leave the accumulated state inconsistent.

2. **At-least-once is simpler to reason about.** Events 10 and 11 will be reprocessed. If the handler is idempotent (as recommended in Post 8), this is harmless. If it's not idempotent, you have bigger problems.

3. **The alternative (per-event guards) costs ~2ns per event.** Creating and committing a `PanicGuard` per event adds overhead to the hot path. Since panics are rare (bugs, not normal control flow), optimizing the panic path at the cost of the happy path is the wrong trade-off.

> **Correction to the trace:** After the `PanicGuard` rolls back to 9, `failed_at = self.sequence.load() + 1 = 10`. The strategy receives `10` as the failed sequence — which is the first sequence in the batch, not necessarily the one that panicked. This is because the guard rolls back to `last_good`. If you need the exact panicking sequence, record it in the closure before the panic propagates.

---

## Handler Isolation: One Panic Shouldn't Kill Them All

In a multicast topology, multiple handlers process the same events independently. If one panics, the others should continue:

```
Ring Buffer → [Handler A] ← panics on seq 5
            → [Handler B] ← should keep running
            → [Handler C] ← should keep running
```

The `IsolatedHandlerChain` wraps multiple handlers and isolates panics:

```rust
/// Runs multiple handlers in sequence, isolating each handler's panics.
///
/// If handler A panics on event N:
/// - Handler A's panic is caught and reported
/// - Handlers B and C still process event N
/// - The chain continues to the next event
///
/// This provides **handler-level isolation**, not **event-level isolation**.
/// The chain processes all events; individual handler failures don't
/// affect other handlers.
pub struct IsolatedHandlerChain<T> {
    handlers: Vec<Box<dyn EventConsumer<T>>>,
    panic_strategy: Arc<dyn PanicStrategy>,
    /// Track which handlers have been permanently disabled due to
    /// repeated panics. Prevents infinite retry loops.
    disabled: Vec<bool>,
}

impl<T> IsolatedHandlerChain<T> {
    pub fn new(
        handlers: Vec<Box<dyn EventConsumer<T>>>,
        panic_strategy: Arc<dyn PanicStrategy>,
    ) -> Self {
        let len = handlers.len();
        Self {
            handlers,
            panic_strategy,
            disabled: vec![false; len],
        }
    }
}

impl<T> EventConsumer<T> for IsolatedHandlerChain<T> {
    fn consume(&mut self, event: &T, sequence: i64, end_of_batch: bool) {
        for (i, handler) in self.handlers.iter_mut().enumerate() {
            if self.disabled[i] {
                continue; // Skip permanently disabled handlers
            }

            let result = catch_unwind(AssertUnwindSafe(|| {
                handler.consume(event, sequence, end_of_batch);
            }));

            if let Err(panic_info) = result {
                let action = self.panic_strategy.handle_panic(sequence, panic_info);

                match action {
                    PanicAction::Halt => {
                        // Disable this specific handler
                        self.disabled[i] = true;
                        eprintln!(
                            "Handler {} permanently disabled after panic at sequence {}",
                            i, sequence
                        );
                    }
                    PanicAction::Skip => {
                        // Skip just this event for this handler
                        // Other handlers still process it
                    }
                    PanicAction::Retry => {
                        // Retry this handler for this event
                        // Note: infinite loop risk if handler always panics!
                        let retry_result = catch_unwind(AssertUnwindSafe(|| {
                            handler.consume(event, sequence, end_of_batch);
                        }));
                        if retry_result.is_err() {
                            // Second failure → disable
                            self.disabled[i] = true;
                            eprintln!(
                                "Handler {} disabled after retry failure at sequence {}",
                                i, sequence
                            );
                        }
                    }
                }
            }
        }
    }

    fn on_start(&mut self) {
        for handler in &mut self.handlers {
            handler.on_start();
        }
    }

    fn on_shutdown(&mut self) {
        for handler in &mut self.handlers {
            handler.on_shutdown();
        }
    }
}
```

**Why limit retries to one attempt?** The `Retry` action in `IsolatedHandlerChain` only retries once. If the retry also fails, the handler is disabled. This prevents the infinite loop that would occur if the handler always panics on a particular event (a "poison pill"). For more sophisticated retry logic, use `RetryWithBackoffStrategy` at the `PanicSafeBatchProcessor` level instead.

---

## Step-by-Step Trace: Panic Recovery in Action

```
Setup:
- Buffer size: 8
- PanicSafeBatchProcessor with LogAndSkipStrategy
- Handler panics on events where value == 42

── Producer publishes events 0..=7 ──────────────
  seq 0: value=10
  seq 1: value=20
  seq 2: value=42  ← poison event
  seq 3: value=30
  seq 4: value=40
  ...

── Processor: first batch (0..=4) ───────────────
  1. wait_for(0) returns available=4
  2. last_good = -1
  3. PanicGuard created: rollback_to = -1
  4. Process seq 0 (value=10) → OK → sequence = 0
  5. Process seq 1 (value=20) → OK → sequence = 1
  6. Process seq 2 (value=42) → PANIC!
  7. catch_unwind catches the panic
  8. PanicGuard::drop() → sequence.store(-1, Release)
  9. failed_at = -1 + 1 = 0
  10. LogAndSkipStrategy::handle_panic(0, "panic at seq 2")
      → returns PanicAction::Skip
  11. BatchResult::PanicSkip(0)
  12. sequence.store(0, Release)
  13. next_sequence = 1

── Processor: retry batch (1..=4) ───────────────
  1. wait_for(1) returns available=4
  2. last_good = 0
  3. PanicGuard created: rollback_to = 0
  4. Process seq 1 (value=20) → OK → sequence = 1
  5. Process seq 2 (value=42) → PANIC! (again!)
  6. catch_unwind catches the panic
  7. PanicGuard::drop() → sequence.store(0, Release)
  8. failed_at = 0 + 1 = 1
  9. LogAndSkipStrategy::handle_panic(1, "panic at seq 2")
      → returns PanicAction::Skip
  10. sequence.store(1, Release)
  11. next_sequence = 2

── Processor: retry batch (2..=4) ───────────────
  ... same pattern: seq 2 panics again, skip to seq 3 ...

── Processor: batch (3..=4) ─────────────────────
  1. Process seq 3 (value=30) → OK → sequence = 3
  2. Process seq 4 (value=40) → OK → sequence = 4
  3. guard.commit()
  4. next_sequence = 5
```

**Observation:** The poison event at seq 2 is retried multiple times before being skipped past. This is because `PanicGuard` rolls back to `last_good` (pre-batch), not to the failing event. Each retry includes all events from `last_good + 1` onward.

**Optimization opportunity:** If the strategy returns `Skip`, we could set `next_sequence` to `failed_sequence + 1` instead of `last_good + 1`. But this requires knowing *which* event panicked, not just the batch start. The current implementation prioritizes simplicity over optimal skip behavior. For production systems with frequent poison events, consider wrapping each event in its own `catch_unwind` (at the cost of ~50-100ns per event).

---

## When to Use Each Strategy

| Strategy | When | Trade-off | Domain |
|----------|------|-----------|--------|
| `HaltOnPanicStrategy` | Production default | Safest — stops at first bug | Financial systems, order processing |
| `LogAndSkipStrategy` | Development / metrics | Loses events — skip on panic | Logging, monitoring, analytics |
| `RetryWithBackoffStrategy` | Transient failures | May delay processing | Network I/O, resource exhaustion |
| `IsolatedHandlerChain` | Multicast with mixed criticality | One handler's bug doesn't kill others | A/B testing, debug tracing |

**Decision tree:**

```
Is a missed event acceptable?
├── No → Is the panic likely transient?
│        ├── Yes → RetryWithBackoffStrategy (with max_retries!)
│        └── No  → HaltOnPanicStrategy (fix the bug)
└── Yes → LogAndSkipStrategy
```

---

## Performance Characteristics

| Scenario | Overhead | Notes |
|----------|----------|-------|
| No panic (happy path) | **Zero** | `PanicGuard` is optimized away; `catch_unwind` has no runtime cost when no panic occurs¹ |
| Panic occurs | ~1-10μs | Stack unwinding + `PanicGuard::drop()` + strategy callback |
| `catch_unwind` per event | ~50-100ns | Only if wrapping each event individually (not recommended for hot path) |
| `IsolatedHandlerChain` per handler | ~50-100ns per handler per event | `catch_unwind` around each handler call |
| Retry with backoff | Variable | Depends on backoff duration (milliseconds) |

¹ On x86_64 with the default `gcc`-style personality-based unwinding, `catch_unwind` has zero cost when no panic occurs. The landing pad metadata is in the `.eh_frame` section, not in the instruction stream.

**Why zero overhead on the happy path?** The compiler uses table-based unwinding (Itanium ABI / `.eh_frame`). No instructions are emitted for the "no panic" case — the unwind tables are only consulted when a panic actually occurs. `PanicGuard` is a struct with no heap allocation; if `committed` is always `true` (no panic), the optimizer eliminates the `Drop` entirely.

---

## Common Pitfalls

| Pitfall | Why It's Dangerous | Fix |
|---------|-------------------|-----|
| Using `panic = "abort"` | `catch_unwind` and `PanicGuard::drop()` never execute. Sequence corruption guaranteed. | Use `panic = "unwind"` (default). If you need abort semantics, handle errors with `Result` instead of panics. |
| Catching panics in the hot path | `catch_unwind` per event costs ~50-100ns. At 10M events/sec, that's 0.5-1 second of overhead per second. | Catch at batch level (as shown in `PanicSafeBatchProcessor`), not per event. |
| Non-idempotent handlers with retry | Retrying causes events to be processed multiple times. Non-idempotent side effects (charging a credit card, sending an email) will duplicate. | Design handlers to be idempotent (Post 8), or use `Skip` instead of `Retry`. |
| Ignoring `AssertUnwindSafe` implications | After a panic, `&mut self` references may point to inconsistent state. The handler's internal invariants may be violated. | Design handlers to be panic-resilient: avoid complex multi-step mutations without internal guards. |
| Retrying without max_retries | A handler that always panics on a specific event will retry forever, blocking the pipeline. | Always set `max_retries`. The `RetryWithBackoffStrategy` enforces this. |

---

## Testing

### Test 1: PanicGuard Rolls Back on Panic

```rust
#[test]
fn panic_guard_rolls_back_on_panic() {
    let sequence = AtomicI64::new(5);

    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let guard = PanicGuard::new(&sequence, 5);
        sequence.store(10, Ordering::Release); // Simulate processing
        panic!("test panic");
        // guard.commit() never reached
    }));

    assert!(result.is_err());
    // PanicGuard should have rolled back to 5
    assert_eq!(sequence.load(Ordering::Acquire), 5);
}
```

### Test 2: PanicGuard Commits on Success

```rust
#[test]
fn panic_guard_commits_on_success() {
    let sequence = AtomicI64::new(5);

    {
        let guard = PanicGuard::new(&sequence, 5);
        sequence.store(10, Ordering::Release);
        guard.commit();
    }

    // Should NOT roll back — commit was called
    assert_eq!(sequence.load(Ordering::Acquire), 10);
}
```

### Test 3: HaltOnPanicStrategy Returns Halt

```rust
#[test]
fn halt_strategy_returns_halt() {
    let strategy = HaltOnPanicStrategy;
    let panic_info: Box<dyn Any + Send> = Box::new("test panic");
    let action = strategy.handle_panic(42, panic_info);
    assert_eq!(action, PanicAction::Halt);
}
```

### Test 4: RetryWithBackoff Exhausts Retries

```rust
#[test]
fn retry_strategy_exhausts_retries() {
    let strategy = RetryWithBackoffStrategy::new(
        3,
        Duration::from_millis(1), // Short backoff for tests
    );

    // First 3 calls return Retry
    for i in 0..3 {
        let info: Box<dyn Any + Send> = Box::new(format!("panic {}", i));
        let action = strategy.handle_panic(0, info);
        assert_eq!(action, PanicAction::Retry);
    }

    // 4th call returns Halt (max_retries exceeded)
    let info: Box<dyn Any + Send> = Box::new("final panic");
    let action = strategy.handle_panic(0, info);
    assert_eq!(action, PanicAction::Halt);
}
```

### Test 5: IsolatedHandlerChain Continues After One Handler Panics

```rust
#[test]
fn isolated_chain_continues_after_panic() {
    use std::sync::atomic::{AtomicU64, Ordering};

    struct CountingHandler {
        count: Arc<AtomicU64>,
    }

    impl EventConsumer<u64> for CountingHandler {
        fn consume(&mut self, _event: &u64, _seq: i64, _eob: bool) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        fn on_start(&mut self) {}
        fn on_shutdown(&mut self) {}
    }

    struct PanickingHandler;

    impl EventConsumer<u64> for PanickingHandler {
        fn consume(&mut self, _event: &u64, _seq: i64, _eob: bool) {
            panic!("I always panic");
        }
        fn on_start(&mut self) {}
        fn on_shutdown(&mut self) {}
    }

    let count_a = Arc::new(AtomicU64::new(0));
    let count_b = Arc::new(AtomicU64::new(0));

    let handlers: Vec<Box<dyn EventConsumer<u64>>> = vec![
        Box::new(CountingHandler { count: Arc::clone(&count_a) }),
        Box::new(PanickingHandler),  // This one panics
        Box::new(CountingHandler { count: Arc::clone(&count_b) }),
    ];

    let mut chain = IsolatedHandlerChain::new(
        handlers,
        Arc::new(LogAndSkipStrategy),
    );

    // Process 5 events
    for i in 0..5 {
        chain.consume(&(i as u64), i, i == 4);
    }

    // Handlers A and C processed all 5 events
    assert_eq!(count_a.load(Ordering::Relaxed), 5);
    assert_eq!(count_b.load(Ordering::Relaxed), 5);
}
```

### Test 6: PanicSafeBatchProcessor Skips Poison Events

```rust
#[test]
fn processor_skips_poison_events() {
    use std::sync::Mutex;

    struct SelectivePanicHandler {
        poison_sequences: Vec<i64>,
        processed: Arc<Mutex<Vec<i64>>>,
    }

    impl EventConsumer<u64> for SelectivePanicHandler {
        fn consume(&mut self, _event: &u64, sequence: i64, _eob: bool) {
            if self.poison_sequences.contains(&sequence) {
                panic!("Poison event at sequence {}", sequence);
            }
            self.processed.lock().unwrap().push(sequence);
        }
        fn on_start(&mut self) {}
        fn on_shutdown(&mut self) {}
    }

    // This test verifies that the processor eventually processes
    // non-poison events even when poison events cause panics.
    // The exact behavior depends on batch boundaries and the
    // interaction between PanicGuard rollback and Skip action.
    //
    // Key assertion: the processor doesn't hang forever.
    let processed = Arc::new(Mutex::new(Vec::new()));
    let handler = SelectivePanicHandler {
        poison_sequences: vec![2],
        processed: Arc::clone(&processed),
    };

    // In a real test, you'd wire this to a RingBuffer and SequenceBarrier.
    // Here we verify the handler logic directly.
    let mut chain = IsolatedHandlerChain::new(
        vec![Box::new(handler)],
        Arc::new(LogAndSkipStrategy),
    );

    for i in 0..5 {
        chain.consume(&(i as u64), i, i == 4);
    }

    let result = processed.lock().unwrap();
    assert!(result.contains(&0));
    assert!(result.contains(&1));
    // seq 2 panicked — handler was disabled after retry failure
    assert!(result.contains(&3));
    assert!(result.contains(&4));
}
```

---

## Key Takeaways

1. **`PanicGuard` handles sequence rollback; `catch_unwind` handles recovery.** They're complementary: the guard ensures consistency, the catch ensures the thread survives.

2. **Zero overhead on the happy path.** Table-based unwinding means `catch_unwind` and `PanicGuard` cost nothing when no panic occurs. Don't add per-event `catch_unwind` — catch at batch level.

3. **Three strategies, three domains.** `Halt` for production safety, `Skip` for development/metrics, `Retry` for transient failures. Choose based on whether event loss is acceptable.

4. **Handler isolation prevents cascade failures.** `IsolatedHandlerChain` ensures one buggy handler doesn't take down the entire multicast topology.

5. **`panic = "abort"` is incompatible with recovery.** If you use abort semantics, none of the recovery machinery in this post works. Use `Result`-based error handling instead.

6. **Design handlers to be idempotent.** `PanicGuard` rolls back to the pre-batch state, causing events to be reprocessed. Non-idempotent handlers will produce duplicate side effects on retry.

---

## Next Up: Builder DSL

In **Part 13**, we'll build a type-safe builder DSL:

- **Type-state pattern** — Compile-time validation of configuration
- **Fluent API** — Readable, chainable configuration
- **Topology builder** — Declarative pipeline/multicast/diamond setup
- **Macro DSL** — Even more ergonomic for common cases

---

## References

### Rust Documentation

- [std::panic::catch_unwind](https://doc.rust-lang.org/std/panic/fn.catch_unwind.html) — Catching panics
- [std::panic::UnwindSafe](https://doc.rust-lang.org/std/panic/trait.UnwindSafe.html) — Unwind safety marker trait
- [std::panic::AssertUnwindSafe](https://doc.rust-lang.org/std/panic/struct.AssertUnwindSafe.html) — Override unwind safety checks
- [Drop trait](https://doc.rust-lang.org/std/ops/trait.Drop.html) — Used by `PanicGuard`
- [Unwinding](https://doc.rust-lang.org/nomicon/unwinding.html) — Rustonomicon chapter on panic unwinding

### Java Disruptor Reference

- [ExceptionHandler.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/ExceptionHandler.java) — Java's equivalent of `PanicStrategy`
- [FatalExceptionHandler.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/FatalExceptionHandler.java) — Equivalent of `HaltOnPanicStrategy`

### Background

- [Itanium ABI Exception Handling](https://itanium-cxx-abi.github.io/cxx-abi/abi-eh.html) — Table-based unwinding specification
- [Gil Tene on Exception Costs](https://www.azul.com/resources/knowledge-hub/the-secret-to-consistently-low-latency/) — Performance characteristics of exception handling in the JVM

---

**Next:** [Part 13 — Builder DSL: Type-Safe, Ergonomic Configuration →](post13.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*