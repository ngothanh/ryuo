# Building a Disruptor in Rust: Ryuo — Part 3C: Usage, Testing & Performance

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 3C of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 3B (sequencer implementation)

---

## Recap

In Parts 3A and 3B, we built:
- **Memory ordering fundamentals** — Release/Acquire for visibility, SeqCst for StoreLoad fences
- **SingleProducerSequencer** — ~10ns claims with RAII auto-publish, `Cell` for zero-contention
- **MultiProducerSequencer** — `fetch_add`-based claiming with `available_buffer` for out-of-order publish tracking

Now let's see how to use them, test them, and understand their performance characteristics.

---

## Usage Examples

> **Note on types:** These examples use the cache-padded `Sequence` type from Part 3B for all sequence counters. Consumer sequences use `Arc<Sequence>` (not `Arc<AtomicI64>`) to prevent false sharing. The `Sequence` type provides `get()`, `set()`, and `set_volatile()` methods that wrap `AtomicI64` with the correct memory orderings. See Part 3B for the full definition.

### **Example 1: Single Producer, Single Consumer**

```rust
use std::sync::Arc;
use std::thread;

// Create ring buffer (shared between producer and consumer)
let ring_buffer = Arc::new(RingBuffer::new(1024));

// Create consumer sequence (tracks consumer position)
let consumer_seq = Arc::new(Sequence::new(-1));

// Create sequencer with consumer as gating sequence
let wait_strategy: Arc<dyn WaitStrategy> = Arc::new(BusySpinWaitStrategy);
let sequencer = SingleProducerSequencer::new(
    1024, vec![Arc::clone(&consumer_seq)], Arc::clone(&wait_strategy),
);

// Get a cursor handle for the consumer BEFORE moving sequencer to producer.
// The cursor is an Arc<Sequence> inside the sequencer — we clone it so the
// consumer can observe published sequences independently.
let cursor = sequencer.cursor_arc();

let rb_producer = Arc::clone(&ring_buffer);
let rb_consumer = Arc::clone(&ring_buffer);

// Producer thread (owns the sequencer — SingleProducerSequencer is !Sync)
let producer = thread::spawn(move || {
    for i in 0..1000 {
        let claim = sequencer.claim(1);
        let event = rb_producer.get_mut(claim.start());
        event.value = i;
        // Automatically published on drop!
    }
});

// Consumer thread (uses cursor Arc to observe published sequences)
let consumer = thread::spawn(move || {
    let mut next_sequence = 0i64;

    while next_sequence < 1000 {
        // Wait for sequence to be published
        while cursor.get() < next_sequence {
            std::hint::spin_loop();
        }

        let event = rb_consumer.get(next_sequence);
        println!("Consumed: {}", event.value);

        consumer_seq.set(next_sequence);
        next_sequence += 1;
    }
});

producer.join().unwrap();
consumer.join().unwrap();
```

---

### **Example 2: Multiple Producers, Single Consumer**

```rust
let consumer_seq = Arc::new(Sequence::new(-1));
let ring_buffer = Arc::new(RingBuffer::new(1024));
let wait_strategy: Arc<dyn WaitStrategy> = Arc::new(BusySpinWaitStrategy);
let sequencer = Arc::new(MultiProducerSequencer::new(
    1024, vec![Arc::clone(&consumer_seq)], Arc::clone(&wait_strategy),
));

let producers: Vec<_> = (0..4).map(|id| {
    let seq = Arc::clone(&sequencer);
    let rb = Arc::clone(&ring_buffer);

    thread::spawn(move || {
        for i in 0..250 {
            let claim = seq.claim(1);
            let event = rb.get_mut(claim.start());
            event.value = id * 1000 + i;
            // Auto-published on drop
        }
    })
}).collect();

// IMPORTANT: With MultiProducerSequencer, consumers must NOT use
// sequencer.cursor() to determine which sequences are readable.
// cursor represents the highest *claimed* sequence, not the highest
// *published* sequence. Consumers must check the available_buffer
// to determine which sequences have actually been published.
//
// Consumer pseudocode (full SequenceBarrier implementation in Part 5):
//
//   let cursor_val = sequencer.cursor();  // highest claimed
//   for seq in next_to_consume..=cursor_val {
//       let index = seq as usize & index_mask;
//       let expected_lap = (seq / buffer_size as i64) as i32;
//       // Spin until this specific slot is published
//       while available_buffer[index].load(Acquire) != expected_lap {
//           spin_loop();
//       }
//       let event = ring_buffer.get(seq);
//       // process event...
//   }

for p in producers {
    p.join().unwrap();
}
```

---

### **Example 3: Batch Claiming**

```rust
// Claim 100 slots in one atomic operation
let claim = sequencer.claim(100);

// Write all events
for seq in claim.start()..=claim.end() {
    let event = ring_buffer.get_mut(seq);
    event.value = seq;
}

// All 100 sequences published on drop!
// For SingleProducer: one cursor.store(end, Release) — atomic.
// For MultiProducer: each slot is marked individually in available_buffer.
```

---

### **Example 4: Non-Blocking with try_claim**

```rust
match sequencer.try_claim(1) {
    Ok(claim) => {
        let event = ring_buffer.get_mut(claim.start());
        event.value = 42;
        // Auto-published on drop
    }
    Err(InsufficientCapacity) => {
        // Buffer is full — handle backpressure
        metrics.increment("ring_buffer_full");
    }
}
```

---

## Testing with Loom

Concurrent code is notoriously hard to test. [Loom](https://github.com/tokio-rs/loom) is a testing framework that explores all possible thread interleavings.

```rust
#[cfg(test)]
mod tests {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicI64, Ordering};
    use loom::thread;

    /// Test: No two producers claim the same sequence.
    ///
    /// This test only verifies claiming atomicity — gating_sequences is empty,
    /// so wrap-around prevention is not exercised. Wrap-around is tested
    /// separately with non-empty gating sequences.
    #[test]
    fn test_multi_producer_no_duplicate_sequences() {
        loom::model(|| {
            let sequencer = Arc::new(MultiProducerSequencer::new(8, vec![]));
            let claimed = Arc::new(AtomicI64::new(0));

            let handles: Vec<_> = (0..2).map(|_| {
                let seq = Arc::clone(&sequencer);
                let claimed = Arc::clone(&claimed);

                thread::spawn(move || {
                    let claim = seq.claim(1);
                    let my_seq = claim.start();
                    assert!(my_seq >= 0, "Sequence must be non-negative");

                    // Atomically mark this sequence as claimed using bitfield
                    let bit = 1i64 << (my_seq as u32);
                    let prev = claimed.fetch_or(bit, Ordering::AcqRel);

                    // Assert no other thread claimed this sequence
                    assert_eq!(prev & bit, 0,
                        "Sequence {} claimed by multiple threads!", my_seq);
                })
            }).collect();

            for h in handles {
                h.join().unwrap();
            }
        });
    }

    /// Test: Event data written before publishing is visible to a consumer
    /// thread after it observes the published sequence.
    ///
    /// Verifies the cross-thread happens-before relationship:
    ///   Producer: write data → drop(claim) [Release store]
    ///   Consumer: observe sequence [Acquire load] → read data
    ///
    /// The consumer must see the data the producer wrote.
    #[test]
    fn test_publish_happens_before() {
        loom::model(|| {
            let data = Arc::new(AtomicI64::new(0));
            let cursor = Arc::new(AtomicI64::new(-1));

            let data_producer = Arc::clone(&data);
            let cursor_producer = Arc::clone(&cursor);
            let data_consumer = Arc::clone(&data);
            let cursor_consumer = Arc::clone(&cursor);

            // Producer thread: write data, then publish (Release store to cursor)
            let producer = thread::spawn(move || {
                data_producer.store(42, Ordering::Relaxed);
                cursor_producer.store(0, Ordering::Release); // Publish
            });

            // Consumer thread: wait for publish, then read data
            let consumer = thread::spawn(move || {
                // Spin until we see the published sequence
                if cursor_consumer.load(Ordering::Acquire) >= 0 {
                    // Happens-before: we observed the Release store,
                    // so the Relaxed store to `data` must be visible.
                    let value = data_consumer.load(Ordering::Relaxed);
                    assert_eq!(value, 42,
                        "Consumer must see producer's write after observing publish!");
                }
                // If cursor is still -1, the producer hasn't published yet —
                // this interleaving is valid and doesn't need assertion.
            });

            producer.join().unwrap();
            consumer.join().unwrap();
        });
    }
    /// Test: Gating sequences prevent wrap-around overwrite.
    ///
    /// With a tiny buffer (size=2) and a consumer that hasn't advanced,
    /// the producer should block (claim) or fail (try_claim) rather than
    /// overwrite unread data. We use try_claim here to avoid blocking
    /// inside the Loom model.
    #[test]
    fn test_wrap_around_prevented_by_gating() {
        loom::model(|| {
            let consumer_seq = Arc::new(Sequence::new(-1)); // consumer hasn't read anything
            let sequencer = Arc::new(MultiProducerSequencer::new(
                2, vec![Arc::clone(&consumer_seq)]
            ));

            // Claim both slots (sequences 0 and 1) — fills the buffer
            let claim1 = sequencer.claim(1); // seq 0
            let claim2 = sequencer.claim(1); // seq 1

            // Buffer is full. Consumer is at -1 (hasn't read anything).
            // Attempting to claim a 3rd slot must fail (would overwrite seq 0).
            let result = sequencer.try_claim(1);
            assert!(result.is_err(),
                "try_claim must fail when buffer is full and consumer hasn't advanced");

            // Now simulate consumer advancing past sequence 0
            consumer_seq.set(0);

            // Drop the first two claims (publishes them)
            drop(claim1);
            drop(claim2);

            // Now try_claim should succeed — slot 0 is free (consumer passed it)
            let result = sequencer.try_claim(1);
            assert!(result.is_ok(),
                "try_claim must succeed after consumer advances past wrap point");
        });
    }
}
```

**How Loom works:** Unlike random stress testing, Loom is a *deterministic model checker*. It controls the scheduler and systematically explores **every possible thread interleaving** — including the rare interleavings that cause bugs once per billion runs in production. It also validates memory ordering: if you use `Relaxed` where `Acquire` is needed, Loom will find an interleaving where the read sees a stale value. The tradeoff is that Loom tests must be small (few threads, few operations) because the state space grows exponentially.

**What Loom tests:**
- **Test 1:** No two producers claim the same sequence (atomicity). Empty gating sequences — wrap-around not exercised.
- **Test 2:** Event data written before Release is visible after Acquire (cross-thread happens-before).
- **Test 3:** Gating sequences prevent wrap-around overwrite — producer cannot claim when the buffer is full and the consumer hasn't advanced.

---

## Performance Summary

| Sequencer | Claim Latency | Use Case |
|-----------|---------------|----------|
| **SingleProducer** | ~10ns | One producer thread |
| **MultiProducer** | ~50-200ns | Multiple producer threads |

**Detailed breakdown (theoretical, Intel Xeon/i9-class @ 3.5GHz+):**

**SingleProducerSequencer:**
- **P50**: ~10ns — `Cell::get` + `Cell::set` (~1-2ns), cached gating check via `AtomicI64::load(Relaxed)` (~1ns), no contention
- **P95**: ~20-30ns — L1 cache miss on `cached_gating_sequence` (~4ns L2 hit) + consumer iteration
- **P99.9**: ~50ns-1μs — L3 cache miss (~40ns), cross-NUMA access (~100-200ns), or consumer spin-wait

**MultiProducerSequencer:**
- **P50 (no contention)**: ~50ns — `AtomicI64::fetch_add(AcqRel)` costs ~10-20ns uncontended (LOCK XADD on x86), plus cached gating check
- **P50 (4 producers)**: ~100-150ns — cache-line bouncing: each `fetch_add` invalidates the cursor's cache line on other cores (~30-50ns per MESI invalidation round-trip)
- **P99.9 (high contention)**: ~200ns-1μs — multiple cache-line transfers + occasional L3/cross-socket latency

**Underlying hardware costs (Intel x86-64):**

| Operation | Typical Latency | Notes |
|---|---|---|
| L1 cache hit | ~1ns | `Cell::get/set`, `Relaxed` load |
| `LOCK XADD` (uncontended) | ~10-20ns | `fetch_add` — atomic bus lock |
| `LOCK CMPXCHG` (uncontended) | ~10-20ns | `compare_exchange` — CAS |
| L2 cache hit | ~4ns | Cached gating sequence miss |
| L3 cache hit | ~12-40ns | Cross-core access |
| Cache-line transfer (same socket) | ~30-50ns | MESI invalidation round-trip |
| Cross-NUMA | ~100-200ns | Remote socket access |

**Note:** These are theoretical estimates based on published hardware latencies. Actual performance depends on CPU microarchitecture, memory hierarchy, system load, and compiler optimizations. We'll measure actual performance with proper benchmarking in Post 15.

**When to use each:**
- **SingleProducer**: Exactly one producer thread. 5-20x faster.
- **MultiProducer**: Multiple producer threads. Supports concurrency at the cost of atomic contention.

---

## What We Haven't Covered

The sequencer handles producer coordination, but several questions remain:

1. **Wait strategies** — How do consumers wait efficiently? (Post 4)
2. **Sequence barriers** — How do multi-stage pipelines work? (Post 5)
3. **Event handlers** — How do consumers process events? (Post 6)
4. **Backpressure** — What happens when buffer is full? (Post 7)

**Advanced performance topics (Post 15):**
- CPU pinning, NUMA awareness, huge pages, wait strategy tuning

---

## Key Takeaways (Parts 3A–3C)

1. **Memory ordering is a CPU problem** — Not specific to Rust, C++, or Java. All languages must solve it.

2. **RAII prevents bugs** — Rust's Drop trait makes "forgot to publish" impossible.

3. **Single-producer is 5-20x faster** — No atomic contention. Use it when possible.

4. **Caching is critical** — Cached gating sequence avoids iterating consumers on every claim.

5. **fetch_add > CAS loop** — Faster, fairer, battle-tested (Java Disruptor).

6. **Avoid hot-path allocations** — Enum dispatch instead of Box. `Arc::clone` (~5ns) is acceptable for soundness.

7. **Multi-producer cursor ≠ published** — Consumers must check available_buffer, not cursor.

---

## Next Up: Wait Strategies

In **Part 4**, we'll build the consumer side:

- **Wait strategies** — Busy-spin vs blocking vs hybrid
- **Sequence barriers** — How consumers track dependencies
- **Multi-stage pipelines** — How to chain event handlers

---

## References

### Papers & Documentation

1. **LMAX Disruptor Paper** (2011)
   https://lmax-exchange.github.io/disruptor/files/Disruptor-1.0.pdf

2. **"A Primer on Memory Consistency and Cache Coherence"** (Sorin et al., 2011)
   https://www.morganclaypool.com/doi/abs/10.2200/S00346ED1V01Y201104CAC016

### Rust Documentation

- [std::sync::atomic](https://doc.rust-lang.org/std/sync/atomic/)
- [Rust Atomics and Locks](https://marabos.nl/atomics/) (Mara Bos, 2023)

### Tools

- [Loom](https://github.com/tokio-rs/loom) — Concurrency testing framework

---

**Next:** [Part 4 — Wait Strategies: Trading Latency for CPU →](post4.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*
