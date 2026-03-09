# Building a Disruptor in Rust: Ryuo — Part 7: Publishing Patterns — Closures Over Translators

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 7 of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 6 (event handlers), Part 3B (sequencer implementation)

---

## Recap

In Part 6, we built the consumer side:
- **`EventConsumer<T>` / `EventProcessor<T>`** — Two handler traits for read-only and mutable event access
- **`BatchEventProcessor`** — The event loop that calls `barrier.wait_for()` and dispatches events
- **`PanicGuard`** — At-least-once semantics via sequence rollback on panic

But we've been hand-waving the producer side. In every example so far, we've written:

```rust
let claim = sequencer.claim(1);
let event = ring_buffer.get_mut(claim.start());
event.value = 42;
// claim drops → auto-publish
```

This works, but it has problems:

1. **Boilerplate** — Four lines to publish one event. Every call site repeats the same claim-write-drop pattern.
2. **Error-prone** — Forgetting to write the event before the claim drops publishes uninitialized data.
3. **No batch API** — Publishing 100 events requires 100 separate `claim(1)` calls, each with its own atomic operation.
4. **No backpressure signal** — `claim()` blocks forever if the ring buffer is full. Sometimes you need to know *immediately* that the buffer is full.

This post solves all four with a closure-based publishing API.

---

## Why Not EventTranslator?

In the Java Disruptor, publishing uses the `EventTranslator` hierarchy:

```java
// Java: One argument
interface EventTranslatorOneArg<T, A> {
    void translateTo(T event, long sequence, A arg0);
}

// Java: Two arguments
interface EventTranslatorTwoArg<T, A, B> {
    void translateTo(T event, long sequence, A arg0, B arg1);
}

// Java: Three arguments
interface EventTranslatorThreeArg<T, A, B, C> {
    void translateTo(T event, long sequence, A arg0, B arg1, C arg2);
}

// Java: Variable arguments
interface EventTranslatorVararg<T> {
    void translateTo(T event, long sequence, Object... args);
}
```

**Why four interfaces?** Java's type erasure prevents a single generic interface from handling different argument counts. Each `translateTo` overload needs its own interface.

**Rust doesn't have this problem.** Closures capture any number of variables with zero overhead:

```rust
// Rust: Any number of captured variables — one API
let user_id = 123u64;
let action = "login";
let timestamp = Instant::now();
let metadata = HashMap::new();

ring_buffer.publish_with(&sequencer, |event, seq| {
    event.user_id = user_id;        // captured by copy
    event.action = action;           // captured by reference
    event.timestamp = timestamp;     // captured by copy
    event.metadata = metadata.clone(); // explicit clone
    event.sequence = seq;
});
```

**Zero overhead:** The closure is monomorphized — the compiler inlines it at the call site. No vtable, no heap allocation, no type erasure. The generated code is identical to the manual claim-write-drop pattern.

| Aspect | Java EventTranslator | Rust Closure |
|--------|---------------------|--------------|
| Argument count | Fixed per interface (1, 2, 3, vararg) | Unlimited (captured variables) |
| Type safety | Erased at runtime (vararg uses `Object...`) | Full compile-time checking |
| Allocation | Translator object on heap | Zero — stack-allocated, inlined |
| Dispatch | Virtual (interface method) | Static (monomorphized) |
| Boilerplate | 4+ interfaces, factory methods | One `FnOnce` parameter |

---

## The Publishing API

### publish_with: Single Event

```rust
impl<T> RingBuffer<T> {
    /// Publish a single event using a closure.
    ///
    /// The closure receives a mutable reference to the pre-allocated event
    /// and the sequence number. The event is published automatically when
    /// the closure returns (via RAII `SequenceClaim` drop).
    ///
    /// # Panics
    /// Blocks if the ring buffer is full (all slots occupied by
    /// unprocessed events). Use `try_publish_with` for non-blocking.
    pub fn publish_with<F>(&self, sequencer: &dyn Sequencer, f: F)
    where
        F: FnOnce(&mut T, i64),
    {
        let claim = sequencer.claim(1);
        let sequence = claim.start();
        let event = self.get_mut(sequence);
        f(event, sequence);
        // claim drops here → publishes the sequence
    }
}
```

**Why `FnOnce`?** The closure runs exactly once per publish. `FnOnce` is the most permissive — it accepts closures that move captured variables (consuming them). `Fn` or `FnMut` would reject closures that move ownership of captured data into the event.

**Why pass `sequence` to the closure?** Some events need to record their own sequence number (for deduplication, ordering, or correlation). The closure receives it as a parameter rather than requiring the caller to track it separately.

### try_publish_with: Non-Blocking

```rust
/// Error returned when the ring buffer has no available slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsufficientCapacity;

impl<T> RingBuffer<T> {
    /// Try to publish a single event without blocking.
    ///
    /// Returns `Err(InsufficientCapacity)` immediately if the ring buffer
    /// is full. The closure is never called in that case — no wasted work.
    pub fn try_publish_with<F>(&self, sequencer: &dyn Sequencer, f: F)
        -> Result<(), InsufficientCapacity>
    where
        F: FnOnce(&mut T, i64),
    {
        let claim = sequencer.try_claim(1)?;
        let sequence = claim.start();
        let event = self.get_mut(sequence);
        f(event, sequence);
        Ok(())
        // claim drops here → publishes
    }
}
```

**Why return `InsufficientCapacity` instead of `Option`?** A dedicated error type is self-documenting and enables `?` propagation through layers of code. It also leaves room for future error variants (e.g., `ShuttingDown`) without breaking the API.

**When to use `try_publish_with`:**

| Scenario | Use `publish_with` | Use `try_publish_with` |
|----------|-------------------|----------------------|
| HFT order entry | ✅ Must publish — blocking is acceptable | ❌ Dropping orders is unacceptable |
| Telemetry/metrics | ❌ Don't block the hot path | ✅ Drop metrics if buffer full |
| Logging | ❌ Don't block application threads | ✅ Drop logs under extreme load |
| Network I/O | Depends on protocol | ✅ Apply backpressure to sender |

### publish_batch: Zero-Allocation Batch Publishing

```rust
impl<T> RingBuffer<T> {
    /// Publish a batch of events from an iterator without heap allocation.
    ///
    /// Requires `ExactSizeIterator` so we know the count before claiming.
    /// This enables a single atomic `claim(count)` instead of N separate
    /// `claim(1)` calls — reducing atomic operations from N to 1.
    ///
    /// The closure receives:
    /// - `&mut T`: mutable reference to the pre-allocated event
    /// - `i64`: the sequence number for this event
    /// - `I::Item`: the source data item from the iterator
    pub fn publish_batch<I, F>(&self, sequencer: &dyn Sequencer, items: I, f: F)
    where
        I: IntoIterator,
        I::IntoIter: ExactSizeIterator,
        F: Fn(&mut T, i64, I::Item),
    {
        let iter = items.into_iter();
        let count = iter.len();
        if count == 0 {
            return;
        }

        // Single atomic claim for the entire batch.
        // For SingleProducer: one Cell::set (no atomic at all).
        // For MultiProducer: one fetch_add.
        let claim = sequencer.claim(count);

        for (i, item) in iter.enumerate() {
            let seq = claim.start() + i as i64;
            let event = self.get_mut(seq);
            f(event, seq, item);
        }
        // claim drops → publishes entire batch atomically
    }
}
```

**Why `ExactSizeIterator`?** We need to know the count *before* claiming slots. Without it, we'd have to either:
1. Collect into a `Vec` first (heap allocation — violates zero-allocation guarantee), or
2. Claim one slot at a time (N atomic operations instead of 1)

`ExactSizeIterator` is implemented by `Vec`, `&[T]`, `Range`, `std::array::IntoIter`, and most standard iterators. It's the right constraint.

**Why `Fn` instead of `FnOnce`?** The closure is called once per item in the batch. `FnOnce` can only be called once. `Fn` allows repeated calls while still permitting captured references.

**Cost comparison:**

```
Publishing 100 events:

Individual claims:                    Batch claim:
  100 × claim(1)                       1 × claim(100)
  = 100 atomic ops (multi-producer)    = 1 atomic op (multi-producer)
  = 100 × ~15ns = ~1,500ns            = 1 × ~15ns = ~15ns
  + 100 × publish overhead             + 1 × publish overhead

Speedup: ~100x for the claiming phase
```

### try_publish_batch: Non-Blocking Batch

```rust
impl<T> RingBuffer<T> {
    /// Try to publish a batch without blocking.
    ///
    /// Returns `Err(InsufficientCapacity)` if there aren't enough
    /// contiguous slots available. No partial publish — either the
    /// entire batch succeeds or nothing is published.
    pub fn try_publish_batch<I, F>(
        &self,
        sequencer: &dyn Sequencer,
        items: I,
        f: F,
    ) -> Result<(), InsufficientCapacity>
    where
        I: IntoIterator,
        I::IntoIter: ExactSizeIterator,
        F: Fn(&mut T, i64, I::Item),
    {
        let iter = items.into_iter();
        let count = iter.len();
        if count == 0 {
            return Ok(());
        }

        let claim = sequencer.try_claim(count)?;

        for (i, item) in iter.enumerate() {
            let seq = claim.start() + i as i64;
            let event = self.get_mut(seq);
            f(event, seq, item);
        }
        Ok(())
    }
}
```

**All-or-nothing semantics:** If the buffer has 50 free slots and you try to publish 100, the entire batch fails. No partial writes, no torn state. This is critical for maintaining event ordering — a partial batch would leave gaps in the sequence space.

---

## Usage Examples

### Example 1: Simple Event Publishing

```rust
struct OrderEvent {
    order_id: u64,
    price: f64,
    timestamp: Instant,
}

// Publish a single order
ring_buffer.publish_with(&*sequencer, |event, _seq| {
    event.order_id = 12345;
    event.price = 99.95;
    event.timestamp = Instant::now();
});
```

### Example 2: Publishing with Captured Variables

```rust
fn process_incoming_order(
    ring_buffer: &RingBuffer<OrderEvent>,
    sequencer: &dyn Sequencer,
    order_id: u64,
    price: f64,
) {
    // Variables are captured by the closure — no intermediate struct needed.
    ring_buffer.publish_with(sequencer, |event, seq| {
        event.order_id = order_id;   // captured by copy (u64 is Copy)
        event.price = price;          // captured by copy (f64 is Copy)
        event.timestamp = Instant::now();
    });
}
```

### Example 3: Batch Publishing from a Network Buffer

```rust
fn publish_network_batch(
    ring_buffer: &RingBuffer<MarketDataEvent>,
    sequencer: &dyn Sequencer,
    raw_messages: &[RawMessage],
) {
    // raw_messages.iter() implements ExactSizeIterator
    ring_buffer.publish_batch(sequencer, raw_messages.iter(), |event, seq, msg| {
        event.symbol = msg.parse_symbol();
        event.price = msg.parse_price();
        event.quantity = msg.parse_quantity();
        event.sequence = seq;
    });
}
```

### Example 4: Backpressure-Aware Publishing

```rust
fn publish_with_backpressure(
    ring_buffer: &RingBuffer<LogEvent>,
    sequencer: &dyn Sequencer,
    message: &str,
) -> bool {
    match ring_buffer.try_publish_with(sequencer, |event, seq| {
        event.message = message.to_string();
        event.level = LogLevel::Info;
        event.sequence = seq;
    }) {
        Ok(()) => true,
        Err(InsufficientCapacity) => {
            // Buffer full — apply backpressure.
            // Options: drop the message, buffer locally, signal upstream.
            eprintln!("Ring buffer full — dropping log message");
            false
        }
    }
}
```

---

## Advanced: Reusable Publisher Types

For applications that publish the same event type frequently, wrapping the ring buffer and sequencer in a dedicated publisher reduces boilerplate and enforces consistency:

```rust
/// Type-safe publisher for a specific event type.
///
/// Encapsulates the ring buffer and sequencer, providing
/// domain-specific publish methods.
pub struct LogEventPublisher {
    ring_buffer: Arc<RingBuffer<LogEvent>>,
    sequencer: Arc<dyn Sequencer>,
}

impl LogEventPublisher {
    pub fn new(
        ring_buffer: Arc<RingBuffer<LogEvent>>,
        sequencer: Arc<dyn Sequencer>,
    ) -> Self {
        Self { ring_buffer, sequencer }
    }

    /// Publish a log event. Blocks if the buffer is full.
    pub fn publish_log(&self, level: LogLevel, message: &str) {
        self.ring_buffer.publish_with(&*self.sequencer, |event, seq| {
            event.level = level;
            event.message.clear();
            event.message.push_str(message); // Reuse allocation!
            event.timestamp = Instant::now();
            event.sequence = seq;
        });
    }

    /// Publish an error log from any Error type.
    pub fn publish_error(&self, error: &dyn std::error::Error) {
        self.publish_log(LogLevel::Error, &error.to_string());
    }

    /// Try to publish — returns false if buffer is full.
    pub fn try_publish_log(&self, level: LogLevel, message: &str) -> bool {
        self.ring_buffer.try_publish_with(&*self.sequencer, |event, seq| {
            event.level = level;
            event.message.clear();
            event.message.push_str(message);
            event.timestamp = Instant::now();
            event.sequence = seq;
        }).is_ok()
    }
}
```

**Why `message.clear()` + `push_str()` instead of `message = msg.to_string()`?** The ring buffer pre-allocates events. Each `LogEvent.message` is a `String` that was allocated once during initialization. `clear()` + `push_str()` reuses the existing heap allocation if the new message fits. `to_string()` allocates a new `String` every time — exactly the kind of hot-path allocation we want to avoid.

**When the message is longer than the existing capacity:**

```
Existing allocation: [h|e|l|l|o|_|_|_]  capacity=8, len=5
New message:         "this is a longer message"  len=24

clear() + push_str():
  → Reallocates to capacity=24 (one allocation)
  → Future messages ≤24 chars reuse this allocation

to_string():
  → Allocates new String with capacity=24
  → Old allocation is dropped
  → Every call allocates, even if message is short
```

Over time, `clear()` + `push_str()` converges to zero allocations as the capacity grows to accommodate the largest message seen.

---

## Panic Safety in Publishing

What happens if the closure panics?

```rust
ring_buffer.publish_with(&*sequencer, |event, seq| {
    event.field1 = compute_something(); // succeeds
    event.field2 = panic!("oops");      // panics!
});
```

**The `SequenceClaim` is dropped during stack unwinding.** This means the sequence is *published* even though the event is only partially written. Downstream consumers will read a partially-initialized event.

**Is this a problem?** It depends:

| Scenario | Impact | Mitigation |
|----------|--------|------------|
| Panic in pure computation | Partially-written event published | Consumer sees garbage in unpopulated fields |
| Panic in I/O (unlikely in closure) | Same as above | Don't do I/O in publish closures |
| Panic in `Default::default()` | Very unlikely | Use simple types in events |

**The Java Disruptor has the same behavior.** If `translateTo()` throws, the sequence is still published. The rationale: the alternative — rolling back the claim — would leave a permanent gap in the sequence space, which is worse than a bad event.

**Best practice:** Keep publish closures simple. Copy data in, set fields, return. No I/O, no fallible operations, no complex logic. If you need complex transformation, do it *before* the closure and capture the result:

```rust
// ✅ Good: compute before, copy in closure
let validated_price = validate_and_normalize(raw_price)?;
let enriched_data = enrich(order)?;

ring_buffer.publish_with(&*sequencer, |event, _seq| {
    event.price = validated_price;     // infallible copy
    event.data = enriched_data;        // infallible move
});

// ❌ Bad: complex logic inside closure
ring_buffer.publish_with(&*sequencer, |event, _seq| {
    event.price = validate_and_normalize(raw_price).unwrap(); // can panic!
    event.data = enrich(order).unwrap();                       // can panic!
});
```


---

## Testing

### Test 1: publish_with Writes and Publishes

```rust
#[test]
fn publish_with_writes_event_and_advances_cursor() {
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let consumer_seq = Arc::new(AtomicI64::new(-1));
    let sequencer = SingleProducerSequencer::new(8, vec![Arc::clone(&consumer_seq)]);

    ring_buffer.publish_with(&sequencer, |event, seq| {
        *event = 42;
        assert_eq!(seq, 0);
    });

    assert_eq!(sequencer.cursor(), 0);
    assert_eq!(*ring_buffer.get(0), 42);
}
```

### Test 2: try_publish_with Returns Error When Full

```rust
#[test]
fn try_publish_returns_insufficient_capacity_when_full() {
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(4));
    let consumer_seq = Arc::new(AtomicI64::new(-1));
    let sequencer = SingleProducerSequencer::new(4, vec![Arc::clone(&consumer_seq)]);

    // Fill the buffer
    for i in 0..4u64 {
        ring_buffer.publish_with(&sequencer, |event, _| { *event = i; });
    }

    // Buffer full — should fail
    let result = ring_buffer.try_publish_with(&sequencer, |event, _| { *event = 999; });
    assert_eq!(result, Err(InsufficientCapacity));

    // Advance consumer — frees one slot
    consumer_seq.store(0, Ordering::Release);

    // Now should succeed
    let result = ring_buffer.try_publish_with(&sequencer, |event, _| { *event = 999; });
    assert_eq!(result, Ok(()));
}
```

### Test 3: publish_batch Publishes All Events

```rust
#[test]
fn publish_batch_writes_all_events() {
    let ring_buffer = Arc::new(RingBuffer::<String>::new(16));
    let consumer_seq = Arc::new(AtomicI64::new(-1));
    let sequencer = SingleProducerSequencer::new(16, vec![Arc::clone(&consumer_seq)]);

    let messages = vec!["hello", "world", "foo"];
    ring_buffer.publish_batch(&sequencer, messages.iter(), |event, _seq, msg| {
        event.clear();
        event.push_str(msg);
    });

    assert_eq!(sequencer.cursor(), 2);
    assert_eq!(ring_buffer.get(0).as_str(), "hello");
    assert_eq!(ring_buffer.get(1).as_str(), "world");
    assert_eq!(ring_buffer.get(2).as_str(), "foo");
}
```

### Test 4: Empty Batch Is a No-Op

```rust
#[test]
fn publish_batch_empty_is_noop() {
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(8));
    let consumer_seq = Arc::new(AtomicI64::new(-1));
    let sequencer = SingleProducerSequencer::new(8, vec![Arc::clone(&consumer_seq)]);

    let empty: Vec<u64> = vec![];
    ring_buffer.publish_batch(&sequencer, empty, |event, _seq, item| { *event = item; });

    assert_eq!(sequencer.cursor(), -1);
}
```

### Test 5: Multi-Producer Concurrent Publishing

```rust
#[test]
fn multi_producer_concurrent_publish() {
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(1024));
    let consumer_seq = Arc::new(AtomicI64::new(-1));
    let sequencer = Arc::new(
        MultiProducerSequencer::new(1024, vec![Arc::clone(&consumer_seq)])
    );

    let num_producers = 4u64;
    let events_per_producer = 100u64;

    let handles: Vec<_> = (0..num_producers).map(|pid| {
        let rb = Arc::clone(&ring_buffer);
        let seq = Arc::clone(&sequencer);
        std::thread::spawn(move || {
            for i in 0..events_per_producer {
                rb.publish_with(&*seq, |event, _| { *event = pid * 1000 + i; });
            }
        })
    }).collect();

    for h in handles { h.join().unwrap(); }

    let total = num_producers * events_per_producer;
    assert_eq!(sequencer.cursor(), (total - 1) as i64);

    // Verify no duplicates
    let mut values: Vec<u64> = (0..total)
        .map(|i| *ring_buffer.get(i as i64))
        .collect();
    values.sort();
    values.dedup();
    assert_eq!(values.len(), total as usize);
}
```

---

## Performance Characteristics

| Operation | Cost | Notes |
|-----------|------|-------|
| `publish_with` (single-producer) | ~10ns | One `Cell::set` (no atomic) + closure call (inlined) |
| `publish_with` (multi-producer) | ~15-25ns | One `fetch_add` + per-slot `available_buffer` store |
| `try_publish_with` (success) | Same as `publish_with` | Plus one `try_claim` check |
| `try_publish_with` (failure) | ~5ns | One load to check capacity, no closure call |
| `publish_batch` (N events, single) | ~10ns + N×(closure) | One claim for entire batch |
| `publish_batch` (N events, multi) | ~15ns + N×(closure + flag store) | One `fetch_add` + N flag stores |
| Closure overhead | ~0ns | Monomorphized and inlined |

**The key insight:** The publishing API adds zero overhead beyond the sequencer's `claim()` cost. The closure is inlined, the RAII publish is a single store, and batch publishing amortizes the atomic operation across all events.

---

## API Summary

| Method | Blocking? | Batch? | Use Case |
|--------|-----------|--------|----------|
| `publish_with` | Yes (waits for space) | No | Must-publish events (orders, transactions) |
| `try_publish_with` | No | No | Best-effort events (logs, metrics) |
| `publish_batch` | Yes (waits for space) | Yes | High-throughput bulk ingestion |
| `try_publish_batch` | No | Yes | Bulk ingestion with backpressure |

---

## Key Takeaways

1. **Closures replace Java's EventTranslator hierarchy** — One `FnOnce` parameter handles any number of captured variables. No `OneArg`, `TwoArg`, `ThreeArg` interfaces needed.

2. **Zero-cost abstraction** — Closures are monomorphized and inlined. The generated code is identical to hand-written claim-write-drop.

3. **Batch publishing reduces atomic operations from N to 1** — `publish_batch` claims all slots with a single `fetch_add`, then writes events without further synchronization.

4. **`ExactSizeIterator` enables zero-allocation batches** — The count is known upfront, so no intermediate `Vec` is needed.

5. **`try_publish_with` enables backpressure** — Returns `InsufficientCapacity` immediately instead of blocking. The closure is never called on failure — no wasted computation.

6. **Keep publish closures simple** — Copy data in, set fields, return. Do complex computation *before* the closure to avoid panics inside the publish path.

7. **Reuse event allocations** — Use `clear()` + `push_str()` instead of `= String::new()` to avoid hot-path heap allocations.

---

## Next Up: Batch Rewind

In **Part 8**, we'll handle transient failures:

- **`RewindableError`** — Signal that a batch should be retried
- **`BatchRewindStrategy`** — Configurable retry logic (immediate, backoff, max retries)
- **Idempotency requirements** — Why handlers must be safe to re-execute

---

## References

### Rust Documentation

- [FnOnce trait](https://doc.rust-lang.org/std/ops/trait.FnOnce.html)
- [Fn trait](https://doc.rust-lang.org/std/ops/trait.Fn.html)
- [ExactSizeIterator](https://doc.rust-lang.org/std/iter/trait.ExactSizeIterator.html)
- [Closures in Rust](https://doc.rust-lang.org/book/ch13-01-closures.html) — The Rust Book, Chapter 13

### Java Disruptor Reference

- [EventTranslator.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/EventTranslator.java)
- [EventTranslatorOneArg.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/EventTranslatorOneArg.java)
- [RingBuffer.publishEvent()](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/RingBuffer.java)

---

**Next:** [Part 8 — Batch Rewind: Retry Without Data Loss →](post8.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*