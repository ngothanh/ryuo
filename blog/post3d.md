# Building a Disruptor in Rust: Ryuo — Part 3D: Does It Actually Work?

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 3D of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 3C (MultiProducerSequencer complete design)

---

We have a ring buffer, two sequencers, and RAII publishing. Before we build anything else — wait strategies, barriers, handlers — we need to answer two questions: does this survive concurrent execution? And what does it cost?

---

## Wiring a Pipeline

The simplest pipeline: one producer, one consumer.

```rust
use std::sync::Arc;
use std::thread;

// Shared ring buffer and consumer tracking
let ring_buffer = Arc::new(RingBuffer::new(1024));
let consumer_seq = Arc::new(Sequence::new(-1));

// Create the sequencer with the consumer as a gating sequence
let wait_strategy: Arc<dyn WaitStrategy> = Arc::new(BusySpinWaitStrategy);
let sequencer = SingleProducerSequencer::new(
    1024, vec![Arc::clone(&consumer_seq)], Arc::clone(&wait_strategy),
);

// Clone the cursor Arc BEFORE moving the sequencer to the producer thread.
// SingleProducerSequencer is !Sync — once moved, you cannot access it.
let cursor = sequencer.cursor_arc();

let rb_producer = Arc::clone(&ring_buffer);
let rb_consumer = Arc::clone(&ring_buffer);

// Producer thread (owns the sequencer)
let producer = thread::spawn(move || {
    for i in 0..1000 {
        let claim = sequencer.claim(1);
        let event = rb_producer.get_mut(claim.start());
        event.value = i;
        // Published automatically when claim is dropped
    }
});

// Consumer thread (observes the cursor)
let consumer = thread::spawn(move || {
    let mut next_sequence = 0i64;
    while next_sequence < 1000 {
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

With multiple producers, two things change: the sequencer wraps in `Arc` (because `MultiProducerSequencer` is `Sync`), and consumers must check the available buffer instead of the cursor — because the cursor now tracks *claimed* sequences, not *published* ones.

```diff
- let sequencer = SingleProducerSequencer::new(
-     1024, vec![Arc::clone(&consumer_seq)], Arc::clone(&wait_strategy),
- );
+ let sequencer = Arc::new(MultiProducerSequencer::new(
+     1024, vec![Arc::clone(&consumer_seq)], Arc::clone(&wait_strategy),
+ ));

  // Spawn multiple producers — each gets an Arc clone
  let producers: Vec<_> = (0..4).map(|id| {
      let seq = Arc::clone(&sequencer);
      let rb = Arc::clone(&ring_buffer);
      thread::spawn(move || {
          for i in 0..250 {
              let claim = seq.claim(1);
              let event = rb.get_mut(claim.start());
              event.value = id * 1000 + i;
          }
      })
  }).collect();

- // Consumer reads when cursor.get() >= next_sequence
+ // Consumer must check available_buffer per slot, not just the cursor.
+ // Full SequenceBarrier implementation in Part 5.
```

---

## Batch Claiming

Claim 100 slots in one atomic operation. For single-producer, publish is one `cursor.store(end, Release)`. For multi-producer, each slot is marked individually in the available buffer.

```rust
let claim = sequencer.claim(100);
for seq in claim.start()..=claim.end() {
    let event = ring_buffer.get_mut(seq);
    event.value = seq;
}
// All 100 sequences published on drop
```

## Non-Blocking Claims

When you would rather drop an event than block on a full buffer:

```rust
match sequencer.try_claim(1) {
    Ok(claim) => {
        let event = ring_buffer.get_mut(claim.start());
        event.value = 42;
    }
    Err(InsufficientCapacity) => {
        metrics.increment("ring_buffer_full");
    }
}
```

---

## Does It Survive Concurrency?

Concurrent code is notoriously hard to test. Random stress tests might run a billion iterations and still miss the one interleaving that causes a bug. [Loom](https://github.com/tokio-rs/loom) takes a different approach: it is a deterministic model checker that controls the scheduler and systematically explores **every possible thread interleaving**. It also validates memory ordering — if you use `Relaxed` where `Acquire` is needed, Loom will find an interleaving where the read sees a stale value.

The tradeoff is that Loom tests must be small (few threads, few operations) because the state space grows exponentially. We test three specific properties.

```rust
#[cfg(test)]
mod tests {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicI64, Ordering};
    use loom::thread;
```

**Can two producers get the same slot?**

The claiming operation must be atomic — `fetch_add` should give each thread a unique range. Empty gating sequences so we are testing only atomicity, not wrap-around.

```rust
    #[test]
    fn no_duplicate_sequences() {
        loom::model(|| {
            let sequencer = Arc::new(MultiProducerSequencer::new(8, vec![]));
            let claimed = Arc::new(AtomicI64::new(0));

            let handles: Vec<_> = (0..2).map(|_| {
                let seq = Arc::clone(&sequencer);
                let claimed = Arc::clone(&claimed);
                thread::spawn(move || {
                    let claim = seq.claim(1);
                    let bit = 1i64 << (claim.start() as u32);
                    let prev = claimed.fetch_or(bit, Ordering::AcqRel);
                    assert_eq!(prev & bit, 0,
                        "Sequence {} claimed by multiple threads!", claim.start());
                })
            }).collect();

            for h in handles { h.join().unwrap(); }
        });
    }
```

**Does Release/Acquire actually work?**

The producer writes data, then publishes with a Release store. The consumer observes the publish with an Acquire load. If the happens-before relationship holds, the consumer must see the data. This test uses raw `AtomicI64` — not `Sequence` — to test the ordering pattern in isolation, independent of the wrapper type.

```rust
    #[test]
    fn publish_happens_before() {
        loom::model(|| {
            let data = Arc::new(AtomicI64::new(0));
            let cursor = Arc::new(AtomicI64::new(-1));

            let d = Arc::clone(&data);
            let c = Arc::clone(&cursor);
            let producer = thread::spawn(move || {
                d.store(42, Ordering::Relaxed);
                c.store(0, Ordering::Release);
            });

            let d = Arc::clone(&data);
            let c = Arc::clone(&cursor);
            let consumer = thread::spawn(move || {
                if c.load(Ordering::Acquire) >= 0 {
                    assert_eq!(d.load(Ordering::Relaxed), 42,
                        "Consumer must see producer's write after observing publish!");
                }
                // If cursor is still -1, producer hasn't published yet — valid interleaving.
            });

            producer.join().unwrap();
            consumer.join().unwrap();
        });
    }
```

**Does backpressure prevent overwrites?**

With a tiny buffer (size=2) and a consumer that has not advanced, the producer must not overwrite unread data. We use `try_claim` to avoid blocking inside the Loom model.

```rust
    #[test]
    fn gating_prevents_overwrite() {
        loom::model(|| {
            let consumer_seq = Arc::new(Sequence::new(-1));
            let sequencer = Arc::new(MultiProducerSequencer::new(
                2, vec![Arc::clone(&consumer_seq)]
            ));

            let claim1 = sequencer.claim(1); // seq 0
            let claim2 = sequencer.claim(1); // seq 1

            // Buffer full. Consumer at -1. Third claim must fail.
            let result = sequencer.try_claim(1);
            assert!(result.is_err(),
                "try_claim must fail when buffer is full");

            // Consumer advances past sequence 0
            consumer_seq.set(0);
            drop(claim1);
            drop(claim2);

            // Slot 0 is free — claim should succeed
            let result = sequencer.try_claim(1);
            assert!(result.is_ok(),
                "try_claim must succeed after consumer advances");
        });
    }
}
```

---

## What Does It Cost?

The single-producer claim is `Cell::get` + `Cell::set` + a cached gating check. How fast is that?

About **10ns**. Two plain loads, one plain store, two `Arc::clone`s for the RAII claim. No atomics on the fast path, no memory barriers, no cache-line bouncing.

The multi-producer adds `fetch_add` + cache-line bouncing between cores. How much slower?

**SingleProducerSequencer:**

| Percentile | Latency | What happens |
|---|---|---|
| P50 | ~10ns | `Cell::get` + `Cell::set` (~1-2ns), cached gating check (~1ns) |
| P95 | ~20-30ns | L1 miss on cached gating sequence, L2 hit (~4ns) + consumer iteration |
| P99.9 | ~50ns-1us | L3 miss (~40ns), cross-NUMA (~100-200ns), or consumer spin-wait |

**MultiProducerSequencer:**

| Percentile | Latency | What happens |
|---|---|---|
| P50 (uncontended) | ~50ns | `fetch_add` (LOCK XADD) ~10-20ns + cached gating check |
| P50 (4 producers) | ~100-150ns | Cache-line bouncing: each `fetch_add` invalidates the cursor's line on other cores |
| P99.9 (high contention) | ~200ns-1us | Multiple cache-line transfers + occasional L3/cross-socket latency |

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

These are theoretical estimates based on published hardware latencies. Actual performance depends on CPU microarchitecture, memory hierarchy, system load, and compiler optimizations. We will measure with proper benchmarking in Post 15.

---

## When to Use Which

Use `SingleProducerSequencer` whenever you can guarantee one writer thread — it is 5-20x faster because it avoids all atomic contention on the claim path. Use `MultiProducerSequencer` only when you genuinely need concurrent writes. Most systems can be restructured to have a single writer per ring buffer: fan-in to a dedicated producer thread, or partition across multiple ring buffers. Reach for multi-producer as a last resort, not a default.

---

## Next Up

The sequencers handle the producer side. But the consumer is still busy-spinning on the cursor — burning a full core to check a single atomic variable. In Part 4, we will build wait strategies that let consumers trade latency for CPU: busy-spin for sub-microsecond response, yielding for moderate loads, and blocking for background pipelines where throughput matters more than tail latency.

---

## References

- [LMAX Disruptor Technical Paper](https://lmax-exchange.github.io/disruptor/files/Disruptor-1.0.pdf) (2011)
- [Rust Atomics and Locks](https://marabos.nl/atomics/) — Mara Bos (O'Reilly, 2023)
- [Loom](https://github.com/tokio-rs/loom) — Deterministic concurrency testing framework

---

**Next:** [Part 4 — Wait Strategies: Trading Latency for CPU -->](post4.md)
