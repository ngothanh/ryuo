# Building a Disruptor in Rust: Ryuo — Part 10: EventPoller — Pull-Based Consumption for Control

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 10 of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 6 (event handlers, `BatchEventProcessor`), Part 9 (multi-producer sequencer)

---

## Recap

In Part 6, we built `BatchEventProcessor` — a push-based event loop that calls `barrier.wait_for()` and dispatches events to a handler. It's the right choice for most use cases: simple, efficient, and handles batching automatically.

But it has a fundamental constraint: **it owns the thread.**

```rust
// BatchEventProcessor takes over the thread
processor.run(); // Blocks forever (until alerted)
// Nothing else can run on this thread
```

This is fine for dedicated consumer threads. But some architectures need more flexibility:

| Use Case | Problem with Push-Based |
|----------|------------------------|
| Tokio/async integration | `run()` blocks the async runtime's thread |
| Multi-buffer consumer | Can't poll two ring buffers from one thread |
| Custom event loops | Must integrate with `epoll`/`kqueue`/`io_uring` |
| Rate-limited consumer | Can't pause between polls |
| Testing | Hard to control exactly how many events are consumed |

This post builds `EventPoller` — a pull-based alternative where *you* control when and how often to poll.

---

## Push vs. Pull: Architecture Comparison

```
Push-based (BatchEventProcessor):
┌──────────┐     ┌──────────┐     ┌──────────┐
│ Producer │ ──→ │  Ring     │ ──→ │ Processor│ ──→ Handler
│          │     │  Buffer   │     │  (owns   │
│          │     │          │     │  thread)  │
└──────────┘     └──────────┘     └──────────┘
                                   ↑ blocks here

Pull-based (EventPoller):
┌──────────┐     ┌──────────┐     ┌──────────┐
│ Producer │ ──→ │  Ring     │ ←── │  Poller  │ ←── Your Code
│          │     │  Buffer   │     │  (no     │     (you call poll())
│          │     │          │     │  thread)  │
└──────────┘     └──────────┘     └──────────┘
                                   ↑ returns immediately
```

**The key difference:** `BatchEventProcessor` calls your handler. `EventPoller` waits for you to call it.

---

## Implementation

### Step 1: PollState

```rust
/// Result of a poll attempt.
///
/// Three states form a clean state machine:
/// - Processing → events were consumed
/// - Gating → waiting for upstream (producer or dependency)
/// - Idle → no new events, but not blocked
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollState {
    /// At least one event was processed in this poll.
    Processing,

    /// No events available because an upstream dependency
    /// (producer or preceding consumer) hasn't advanced far enough.
    /// The consumer is "gated" — it would process if it could.
    Gating,

    /// No new events available. The cursor hasn't moved since
    /// the last poll. The consumer is caught up.
    Idle,
}
```

**Why three states instead of two (`HasEvents` / `NoEvents`)?**

`Gating` vs `Idle` gives the caller actionable information:

### Step 2: EventPoller

```rust
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// Pull-based event consumer.
///
/// Unlike `BatchEventProcessor`, the caller controls the event loop.
/// Each call to `poll()` processes available events and returns immediately.
///
/// # Thread Safety
/// `EventPoller` is `Send` but should only be polled from one thread at a time.
/// It does not internally synchronize poll calls.
pub struct EventPoller<T> {
    data_provider: Arc<RingBuffer<T>>,
    sequencer: Arc<dyn Sequencer>,
    sequence: Arc<AtomicI64>,
    gating_sequence: Arc<AtomicI64>,
}

impl<T> EventPoller<T> {
    pub fn new(
        data_provider: Arc<RingBuffer<T>>,
        sequencer: Arc<dyn Sequencer>,
    ) -> Self {
        let sequence = Arc::new(AtomicI64::new(-1));
        let gating_sequence = Arc::new(AtomicI64::new(-1));

        // Register this poller as a gating sequence on the sequencer.
        // This prevents the producer from overwriting events that
        // this poller hasn't consumed yet.
        sequencer.add_gating_sequence(Arc::clone(&gating_sequence));

        Self {
            data_provider,
            sequencer,
            sequence,
            gating_sequence,
        }
    }

    /// Returns this poller's sequence for use in dependency chains.
    pub fn sequence(&self) -> Arc<AtomicI64> {
        Arc::clone(&self.sequence)
    }

    /// Poll for available events.
    ///
    /// The handler closure receives:
    /// - `&T`: reference to the event
    /// - `i64`: the sequence number
    /// - `bool`: true if this is the last event in the current batch
    ///
    /// The handler returns `true` to continue processing, `false` to stop early.
    /// This enables rate-limiting: process at most N events per poll.
    ///
    /// # Returns
    /// - `PollState::Processing` if at least one event was processed
    /// - `PollState::Gating` if waiting for upstream dependency
    /// - `PollState::Idle` if no new events
    pub fn poll<F>(&self, mut handler: F) -> PollState
    where
        F: FnMut(&T, i64, bool) -> bool,
    {
        let current_sequence = self.sequence.load(Ordering::Relaxed);
        let next_sequence = current_sequence + 1;
        let cursor = self.sequencer.cursor();

        // Get the highest contiguously published sequence.
        // For single-producer, this is just the cursor.
        // For multi-producer, this scans the available_buffer.
        let available_sequence = self.sequencer.get_highest_published_sequence(
            next_sequence,
            cursor,
        );

        if available_sequence >= next_sequence {
            // Events available — process them
            let mut seq = next_sequence;

            while seq <= available_sequence {
                let event = self.data_provider.get(seq);
                let end_of_batch = seq == available_sequence;

                if !handler(event, seq, end_of_batch) {
                    // Handler requested stop (rate limiting)
                    break;
                }

                seq += 1;
            }

            // Advance our sequence to the last processed event
            let last_processed = seq - 1;
            self.sequence.store(last_processed, Ordering::Release);
            self.gating_sequence.store(last_processed, Ordering::Release);

            PollState::Processing
        } else if cursor < next_sequence {
            // Cursor hasn't advanced — producer hasn't published anything new
            PollState::Idle
        } else {
            // Cursor advanced but events not available yet
            // (multi-producer: claimed but not yet published)
            PollState::Gating
        }
    }
}
```

**Why two sequences (`sequence` and `gating_sequence`)?**

- `sequence` tracks the poller's progress for internal use and for downstream consumers in a dependency chain.
- `gating_sequence` is registered with the sequencer to prevent the producer from overwriting unread events.

In most cases they advance together. They're separate to allow future optimizations (e.g., batch-gating where the gating sequence advances less frequently to reduce atomic store overhead).

**Why `FnMut` instead of `FnOnce`?** The handler is called once per event in the batch. `FnOnce` can only be called once. `FnMut` allows repeated calls and mutable captured state (e.g., a counter for rate limiting).

**Why `&T` (immutable) instead of `&mut T`?** Pull-based consumers typically observe events. Mutable access is the domain of push-based processors where the handler has exclusive ownership of the processing loop.

---

## Usage Examples

### Example 1: Simple Polling Loop

```rust
let ring_buffer = Arc::new(RingBuffer::<OrderEvent>::new(1024));
let sequencer: Arc<dyn Sequencer> = Arc::new(
    SingleProducerSequencer::new(1024, vec![])
);

let poller = EventPoller::new(Arc::clone(&ring_buffer), Arc::clone(&sequencer));

loop {
    match poller.poll(|event, seq, _end_of_batch| {
        println!("Event {}: order_id={}", seq, event.order_id);
        true // continue processing
    }) {
        PollState::Processing => {
            // Events consumed — poll again immediately
        }
        PollState::Idle => {
            // No events — sleep briefly to avoid busy-waiting
            std::thread::sleep(Duration::from_micros(10));
        }
        PollState::Gating => {
            // Upstream slow — hint the CPU
            std::hint::spin_loop();
        }
    }
}
```

### Example 2: Rate-Limited Consumption

```rust
/// Process at most `max_per_poll` events per call.
fn poll_with_limit(poller: &EventPoller<Event>, max_per_poll: usize) -> PollState {
    let mut count = 0;

    poller.poll(|event, seq, _eob| {
        process_event(event);
        count += 1;
        count < max_per_poll // stop after max_per_poll events
    })
}

// Usage: process 10 events, then do other work
loop {
    poll_with_limit(&poller, 10);
    do_other_work();
}
```

### Example 3: Multi-Buffer Polling

```rust
/// One thread consuming from two ring buffers.
fn poll_multiple(
    poller_orders: &EventPoller<OrderEvent>,
    poller_market: &EventPoller<MarketDataEvent>,
) {
    loop {
        let state1 = poller_orders.poll(|event, seq, _| {
            handle_order(event, seq);
            true
        });

        let state2 = poller_market.poll(|event, seq, _| {
            handle_market_data(event, seq);
            true
        });

        // Only sleep if BOTH buffers are quiet
        if state1 == PollState::Idle && state2 == PollState::Idle {
            std::thread::sleep(Duration::from_micros(10));
        }
    }
}
```

### Example 4: Tokio Integration

```rust
use tokio::time::{sleep, Duration};

/// Poll the disruptor from a Tokio task.
/// Non-blocking — yields to the scheduler when no events are available.
async fn poll_async(poller: &EventPoller<Event>) {
    loop {
        match poller.poll(|event, seq, _| {
            process_event(event, seq);
            true
        }) {
            PollState::Processing => {
                // Yield to let other tasks run
                tokio::task::yield_now().await;
            }
            PollState::Idle | PollState::Gating => {
                // No events — sleep briefly without blocking the thread
                sleep(Duration::from_micros(100)).await;
            }
        }
    }
}
```

**Why not implement `Stream`?** A `Stream` implementation would require `T: Clone` (to move events out of the ring buffer) and would hide the batching behavior. The closure-based `poll()` API is zero-copy and preserves batch boundaries via `end_of_batch`.

---

## Push vs. Pull: When to Use Each

| Criterion | `BatchEventProcessor` (Push) | `EventPoller` (Pull) |
|-----------|------------------------------|---------------------|
| Thread model | Owns dedicated thread | Caller controls thread |
| Blocking | Yes (`barrier.wait_for()`) | Never blocks |
| Latency | Lowest (no poll interval) | Depends on poll frequency |
| Complexity | Simple — just implement handler | More complex — manage poll loop |
| Batching | Automatic (barrier returns batch) | Manual (process until false) |
| Multi-buffer | One processor per buffer | One thread, multiple pollers |
| Async compat | ❌ Blocks async runtime | ✅ Works with any async runtime |
| Rate limiting | Not built-in | Handler returns false |
| Best for | Dedicated consumer threads | Integration with external systems |

---

## Gating Behavior: Step-by-Step Trace

```
Buffer size: 8, poller sequence: -1

── Poll 1: Buffer empty ────────────────────────
poller.poll(handler):
  current_sequence = -1
  next_sequence = 0
  cursor = -1  (no events published)
  available_sequence = -1  (cursor < next_sequence)
  → PollState::Idle

── Producer publishes events 0, 1, 2 ───────────

── Poll 2: Events available ────────────────────
poller.poll(handler):
  current_sequence = -1
  next_sequence = 0
  cursor = 2
  available_sequence = 2  (all published)
  → handler(event[0], 0, false) → true
  → handler(event[1], 1, false) → true
  → handler(event[2], 2, true)  → true
  → sequence = 2, gating_sequence = 2
  → PollState::Processing

── Poll 3: No new events ──────────────────────
poller.poll(handler):
  current_sequence = 2
  next_sequence = 3
  cursor = 2  (hasn't moved)
  → cursor < next_sequence
  → PollState::Idle

── Multi-producer: Cursor advanced but gap exists ─
(Producer A claims seq 3, Producer B claims seq 4.
 B publishes first. Cursor = 4.)

── Poll 4: Gating on gap ──────────────────────
poller.poll(handler):
  current_sequence = 2
  next_sequence = 3
  cursor = 4  (advanced)
  available_sequence = 2  (seq 3 not published yet!)
  → available_sequence < next_sequence → but cursor >= next_sequence
  → PollState::Gating  ← waiting for seq 3

── Producer A publishes seq 3 ──────────────────

── Poll 5: Gap filled ─────────────────────────
poller.poll(handler):
  current_sequence = 2
  next_sequence = 3
  cursor = 4
  available_sequence = 4  (all published)
  → handler(event[3], 3, false) → true
  → handler(event[4], 4, true)  → true
  → sequence = 4
  → PollState::Processing
```

---

## Common Pitfalls

### Pitfall 1: Busy-Wait Without Sleep

```rust
// ❌ 100% CPU when idle
loop {
    poller.poll(|event, _, _| { process(event); true });
}

// ✅ Sleep when idle
loop {
    match poller.poll(|event, _, _| { process(event); true }) {
        PollState::Idle => std::thread::sleep(Duration::from_micros(10)),
        _ => {}
    }
}
```

### Pitfall 2: Forgetting to Register as Gating Sequence

The `EventPoller::new()` constructor handles this automatically. But if you create a poller manually without calling `sequencer.add_gating_sequence()`, the producer can overwrite events before the poller reads them. **Always use the constructor.**

### Pitfall 3: Polling from Multiple Threads

```rust
// ❌ Two threads polling the same poller — race condition
let poller = Arc::new(EventPoller::new(rb, seq));
let p1 = Arc::clone(&poller);
let p2 = Arc::clone(&poller);

thread::spawn(move || { p1.poll(|_, _, _| true); }); // Data race!
thread::spawn(move || { p2.poll(|_, _, _| true); }); // Data race!

// ✅ One poller per consumer thread
let poller1 = EventPoller::new(Arc::clone(&rb), Arc::clone(&seq));
let poller2 = EventPoller::new(Arc::clone(&rb), Arc::clone(&seq));
```

---

## Testing

### Test 1: Poll Returns Idle When Buffer Empty

```rust
#[test]
fn poll_returns_idle_when_empty() {
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(8, vec![])
    );
    let poller = EventPoller::new(Arc::clone(&ring_buffer), Arc::clone(&sequencer));

    let state = poller.poll(|_, _, _| { panic!("should not be called"); true });
    assert_eq!(state, PollState::Idle);
}
```

### Test 2: Poll Processes Published Events

```rust
#[test]
fn poll_processes_published_events() {
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(8, vec![])
    );
    let poller = EventPoller::new(Arc::clone(&ring_buffer), Arc::clone(&sequencer));

    // Publish 3 events
    for i in 0..3u64 {
        ring_buffer.publish_with(&*sequencer, |event, _| { *event = i * 10; });
    }

    let mut processed = vec![];
    let state = poller.poll(|event, seq, _eob| {
        processed.push((*event, seq));
        true
    });

    assert_eq!(state, PollState::Processing);
    assert_eq!(processed, vec![(0, 0), (10, 1), (20, 2)]);
}
```

### Test 3: Handler Can Stop Early (Rate Limiting)

```rust
#[test]
fn poll_stops_when_handler_returns_false() {
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(8, vec![])
    );
    let poller = EventPoller::new(Arc::clone(&ring_buffer), Arc::clone(&sequencer));

    // Publish 5 events
    for i in 0..5u64 {
        ring_buffer.publish_with(&*sequencer, |event, _| { *event = i; });
    }

    let mut count = 0;
    let state = poller.poll(|_event, _seq, _eob| {
        count += 1;
        count < 2 // stop after 2 events
    });

    assert_eq!(state, PollState::Processing);
    assert_eq!(count, 2);

    // Remaining 3 events are available on next poll
    let mut remaining = 0;
    poller.poll(|_, _, _| { remaining += 1; true });
    assert_eq!(remaining, 3);
}
```

### Test 4: Gating Prevents Producer Overwrite

```rust
#[test]
fn poller_gates_producer() {
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(4));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(4, vec![])
    );
    let poller = EventPoller::new(Arc::clone(&ring_buffer), Arc::clone(&sequencer));

    // Fill buffer (4 slots)
    for i in 0..4u64 {
        ring_buffer.publish_with(&*sequencer, |event, _| { *event = i; });
    }

    // Buffer full — try_publish should fail (poller hasn't consumed)
    let result = ring_buffer.try_publish_with(&*sequencer, |event, _| { *event = 999; });
    assert!(result.is_err());

    // Poll consumes events → frees slots
    poller.poll(|_, _, _| true);

    // Now publish should succeed
    let result = ring_buffer.try_publish_with(&*sequencer, |event, _| { *event = 999; });
    assert!(result.is_ok());
}
```

### Test 5: Consecutive Polls Advance Correctly

```rust
#[test]
fn consecutive_polls_advance_sequence() {
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(8, vec![])
    );
    let poller = EventPoller::new(Arc::clone(&ring_buffer), Arc::clone(&sequencer));

    // Publish 2 events
    ring_buffer.publish_with(&*sequencer, |event, _| { *event = 100; });
    ring_buffer.publish_with(&*sequencer, |event, _| { *event = 200; });

    // First poll processes both
    let mut values = vec![];
    poller.poll(|event, _, _| { values.push(*event); true });
    assert_eq!(values, vec![100, 200]);

    // Second poll with no new events
    let state = poller.poll(|_, _, _| { panic!("no events"); true });
    assert_eq!(state, PollState::Idle);

    // Publish more, third poll picks up from where we left off
    ring_buffer.publish_with(&*sequencer, |event, _| { *event = 300; });

    let mut values = vec![];
    poller.poll(|event, _, _| { values.push(*event); true });
    assert_eq!(values, vec![300]);
}
```


---

## Performance Characteristics

| Operation | Cost | Notes |
|-----------|------|-------|
| `poll()` — events available | ~50-200ns per event | Same as push-based (no overhead from pull model) |
| `poll()` — idle | ~10ns | Two atomic loads (cursor + sequence), no handler call |
| `poll()` — gating | ~15ns | Three atomic loads (cursor + sequence + available scan) |
| Sequence update | ~5ns per poll | Two `Release` stores (sequence + gating_sequence) |

**The key insight:** `EventPoller` has the same per-event performance as `BatchEventProcessor`. The difference is *who controls the loop*:
- Push: the processor loops internally (lowest latency, simplest code)
- Pull: the caller loops externally (most flexible, slightly more complex)

---

## Key Takeaways

1. **`EventPoller` is the pull-based counterpart to `BatchEventProcessor`.** Same performance, different control model. Use it when you need to integrate with external event loops or poll multiple buffers.

2. **Three return states provide actionable information.** `Processing` = work done, `Gating` = upstream slow, `Idle` = nothing to do. Each state maps to a different caller response.

3. **Handler return value enables rate limiting.** Return `false` to stop processing mid-batch. The remaining events will be available on the next `poll()`.

4. **Automatic gating prevents producer overwrite.** `EventPoller::new()` registers with the sequencer. No manual gating setup needed.

5. **Never poll from multiple threads.** Each `EventPoller` should be owned by exactly one consumer thread. Create multiple pollers for multiple consumers.

6. **Avoid busy-waiting.** Always sleep or yield when `poll()` returns `Idle`. The `PollState` enum makes this explicit.

---

## Next Up: Dynamic Topologies

In **Part 11**, we'll build the Disruptor DSL:

- **`DisruptorBuilder`** — Fluent API for constructing disruptor pipelines
- **`SequenceGroup`** — Dynamic consumer registration and deregistration
- **Pipeline, multicast, and diamond topologies** — Built declaratively
- **Runtime topology changes** — Adding/removing consumers without restart

---

## References

### Rust Documentation

- [FnMut trait](https://doc.rust-lang.org/std/ops/trait.FnMut.html) — Closure trait for repeated calls with mutable state
- [hint::spin_loop](https://doc.rust-lang.org/std/hint/fn.spin_loop.html) — CPU hint for spin-wait loops

### Java Disruptor Reference

- [EventPoller.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/EventPoller.java)
- [PollState.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/EventPoller.java) — Inner enum for poll results

### Async Integration

- [tokio::task::yield_now](https://docs.rs/tokio/latest/tokio/task/fn.yield_now.html) — Cooperative yielding in Tokio
- [futures::Stream](https://docs.rs/futures/latest/futures/stream/trait.Stream.html) — Async iterator trait

---

**Next:** [Part 11 — Dynamic Topologies: DisruptorBuilder & SequenceGroups →](post11.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*