# Building a Disruptor in Rust: Ryuo — Part 3B: The Sequencer Trait & SingleProducerSequencer

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 3B of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 3A (memory ordering fundamentals)

---

## Recap

In Part 3A, we learned three things:
- **Release/Acquire** creates happens-before relationships between producer and consumer
- **SeqCst** adds a StoreLoad fence — expensive, rarely needed
- **Relaxed** is safe only when no other data depends on the atomic

Now we build the component that puts those orderings to work.

---

## What is a Sequencer?

The ring buffer from Part 2 is raw storage. It knows how to index slots efficiently, but it has no idea who is reading, who is writing, or whether it is safe to do either. The **sequencer** is the coordination layer that sits between producers and consumers, solving the three problems we identified in Part 3A:

1. **Atomically assign sequence numbers** — no two producers get the same slot
2. **Prevent wrap-around** — the producer must not overwrite data consumers have not yet read
3. **Ensure visibility** — data written before publish must be visible to consumers after they observe the cursor advance

The ring buffer is the parking lot. The sequencer is the attendant who hands out numbered tickets and makes sure nobody parks in a spot that is still occupied.

```
Producer → Sequencer → Ring Buffer → Consumer
                ↑                        |
                └── gating sequences ────┘
```

The gating sequences form the feedback loop: consumers report their progress back to the sequencer so it knows when slots are safe to reuse.

---

## The Sequence Type

Every counter in the system — the producer cursor, each consumer's position, the cached gating sequence — is a sequence counter. Before we build the sequencer, we need a counter type that does not destroy performance through false sharing.

### The Problem

An `AtomicI64` is 8 bytes. On a 64-byte cache line, you can pack **eight** of them side by side:

```
Cache line (64 bytes):
┌──────────┬──────────┬──────────┬──────────┬────────────────────┐
│ cursor   │ consumer │ consumer │ cached   │     (unused)       │
│ (8 bytes)│ A (8b)   │ B (8b)   │ (8 bytes)│                    │
└──────────┴──────────┴──────────┴──────────┴────────────────────┘
  Core 0      Core 1     Core 2     Core 0
  writes      writes     writes     writes
```

When Core 1 updates `consumer_A`, the entire 64-byte cache line is invalidated on **all** cores. Core 0's read of `cursor` stalls. Core 2's write to `consumer_B` stalls. Everyone waits for a cache line they have no interest in.

This is **false sharing** — the most insidious performance killer in lock-free programming. There are no lock contentions to profile, no CAS failures to count. The code looks clean. But throughput drops 2-5x because the hardware coherency protocol is bouncing cache lines between cores on every write.

### Why 128 Bytes, Not 64?

You might expect 64-byte alignment to be sufficient — one counter per cache line. Two reasons it is not:

**Intel Spatial Prefetcher.** Since Sandy Bridge (2011), Intel CPUs prefetch **adjacent cache line pairs**. When you touch cache line N, the prefetcher automatically brings in cache line N+1 (or N-1 if N is odd). Two 64-byte-aligned counters on adjacent cache lines are treated as a single 128-byte unit:

```
64-byte alignment (INSUFFICIENT):
┌── Cache line 0 ──┬── Cache line 1 ──┐
│   cursor         │   consumer_A     │  ← Prefetcher treats as ONE 128-byte unit
└──────────────────┴──────────────────┘
Core 0 writes cursor → prefetcher invalidates the PAIR → Core 1's consumer_A is evicted

128-byte alignment (CORRECT):
┌── Cache line 0 ──┬── Cache line 1 ──┐
│   cursor         │   (padding)      │  ← Prefetch pair 1
├── Cache line 2 ──┼── Cache line 3 ──┤
│   consumer_A     │   (padding)      │  ← Prefetch pair 2 (isolated!)
└──────────────────┴──────────────────┘
No cross-contamination.
```

**ARM big cores.** Apple Silicon (M1+) and AWS Graviton big cores use 128-byte cache lines natively. A 64-byte-aligned counter would share a cache line with its neighbor on these architectures.

128-byte alignment handles both cases with a single strategy.

### Implementation

```rust
use std::sync::atomic::{AtomicI64, Ordering};

/// Cache-padded atomic sequence counter.
///
/// Each `Sequence` occupies exactly 128 bytes (one prefetch pair on x86_64,
/// one cache line on aarch64 big cores), ensuring that no two frequently-written
/// counters share a cache line — even with adjacent-line prefetching.
///
/// This mirrors Java Disruptor's `Sequence` class (LhsPadding → Value → RhsPadding).
#[repr(align(128))]
pub struct Sequence {
    value: AtomicI64,
}

impl Sequence {
    /// Create a new sequence with the given initial value.
    /// `const fn` allows static initialization and compile-time construction.
    pub const fn new(initial: i64) -> Self {
        Self {
            value: AtomicI64::new(initial),
        }
    }

    /// Load the current value with Acquire ordering.
    /// Pairs with Release stores to establish happens-before relationships.
    /// Used by: consumers reading the cursor, producer reading gating sequences.
    #[inline]
    pub fn get(&self) -> i64 {
        self.value.load(Ordering::Acquire)
    }

    /// Store a value with Release ordering.
    /// All writes before this store are visible to threads that Acquire-load
    /// the stored value. Used by: SequenceClaim::drop to publish, consumers
    /// to update their position.
    #[inline]
    pub fn set(&self, value: i64) {
        self.value.store(value, Ordering::Release);
    }

    /// Store a value with SeqCst ordering.
    /// Provides a full StoreLoad fence — ensures that this store is visible
    /// to all threads before any subsequent loads execute. Used by:
    /// SingleProducerSequencer::claim to make the cursor visible before
    /// reading consumer positions (prevents livelock).
    #[inline]
    pub fn set_volatile(&self, value: i64) {
        self.value.store(value, Ordering::SeqCst);
    }

    /// Compare-and-swap with AcqRel success ordering and Acquire failure ordering.
    /// Returns true if the swap succeeded. Used by: MultiProducerSequencer
    /// for try_claim's CAS loop.
    #[inline]
    pub fn compare_and_set(&self, expected: i64, new: i64) -> bool {
        self.value
            .compare_exchange(expected, new, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Atomic fetch-and-add. Returns the previous value.
    /// Used by MultiProducerSequencer::claim to atomically reserve a range
    /// of sequence numbers. The caller specifies the ordering — typically
    /// AcqRel to synchronize with other producers. See Part 3C.
    #[inline]
    pub fn fetch_add(&self, n: i64, ordering: Ordering) -> i64 {
        self.value.fetch_add(n, ordering)
    }

    /// Relaxed load — only safe when the caller is the sole writer.
    /// No ordering guarantees. Used by: SingleProducerSequencer reading its
    /// own cursor (it wrote the value, so it is already visible).
    #[inline]
    pub fn get_relaxed(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }

    /// Relaxed store — only safe when no other thread reads this value with
    /// ordering expectations. Used by: updating thread-local caches where
    /// the value will be re-read with proper ordering before use.
    #[inline]
    pub fn set_relaxed(&self, value: i64) {
        self.value.store(value, Ordering::Relaxed);
    }
}

// Compile-time assertions: Sequence must be exactly 128 bytes to guarantee
// cache-line isolation. If the compiler ever changes padding behavior, these
// assertions will catch it at compile time — not as a runtime performance regression.
const _: () = assert!(std::mem::size_of::<Sequence>() == 128);
const _: () = assert!(std::mem::align_of::<Sequence>() == 128);
```

**Memory layout of `Sequence`:**
```
Offset 0x00: ┌─ AtomicI64 (8 bytes)  ─┐
             │   the actual counter   │
             ├────────────────────────┤
Offset 0x08: │                        │
             │   120 bytes padding    │  ← Compiler inserts automatically
             │   (from #[repr(        │     due to align(128) + next
             │    align(128))])       │     Sequence starts at 0x80
             │                        │
Offset 0x80: └────────────────────────┘  ← Next Sequence starts here
```

The padding is wasteful in isolation — 120 bytes per counter. But the alternative is false sharing, which costs 2-5x throughput. In a system with 4 consumers, the overhead is 4 x 128 = 512 bytes. That is a rounding error compared to the ring buffer itself.

---

## The Sequencer Trait

```rust
use std::sync::Arc;

pub trait Sequencer: Send {
    /// Claim `count` slots. Blocks until available.
    fn claim(&self, count: usize) -> SequenceClaim;

    /// Try to claim `count` slots. Returns immediately.
    fn try_claim(&self, count: usize) -> Result<SequenceClaim, InsufficientCapacity>;

    /// Get current cursor position (the highest published sequence).
    fn cursor(&self) -> i64;
}

#[derive(Debug, Clone, Copy)]
pub struct InsufficientCapacity;
```

The trait is **`Send` but not `Sync`**. `Send` means a sequencer can be moved to a producer thread. `Sync` is deliberately absent from the trait bound: `SingleProducerSequencer` is `!Sync` (due to `Cell`), so requiring `Sync` on the trait would exclude it. `MultiProducerSequencer` happens to be `Sync` — it implements the trait and gains `Sync` from its own field types — but the trait does not demand it.

The trait is the contract. Implementations differ in how they coordinate: `SingleProducerSequencer` uses `Cell` (no atomics on the claim path), while `MultiProducerSequencer` uses `fetch_add` (atomic coordination between producers). Both implement the same interface, so consumers do not care which sequencer produced the events.

---

## The SequenceClaim: How Publishing Works

The sequencer hands out a **ticket** — a `SequenceClaim`. The producer uses this ticket to know which slot(s) to write. When the producer is done writing, it must **publish** — telling consumers that new data is available.

But what happens if the producer forgets to publish?

```rust
let seq = sequencer.claim(1);
ring_buffer[seq].value = 42;
// Forgot to call sequencer.publish(seq)
// → cursor never advances → consumers wait forever → DEADLOCK
```

This is the "forgot to unlock" problem applied to lock-free code. In Java, you must remember. In Rust, we can make forgetting impossible.

### Auto-Publish via Drop

When the `SequenceClaim` goes out of scope, Rust's `Drop` trait runs automatically — on normal exit, early return, or panic:

```rust
{
    let claim = sequencer.claim(1);
    ring_buffer.get_mut(claim.start()).value = 42;
}  // ← claim is dropped here, publish happens automatically
```

The producer cannot forget. But this raises a question: **how does the claim know what to do on drop?**

In a normal design, the producer would call back to the sequencer: `sequencer.publish(seq)`. But the `SequenceClaim` is an independent value — it has been moved out of the sequencer. It cannot call back because it does not hold a reference to the sequencer (and the sequencer might not be `Sync`, so sharing a reference across threads is impossible for `SingleProducerSequencer`).

The solution: **the claim carries its own publish action**. The sequencer injects the necessary state at construction time, so the claim can publish independently.

### What the Claim Carries

```rust
#[must_use = "dropping a SequenceClaim without writing event data publishes stale bytes"]
pub struct SequenceClaim {
    start: i64,
    end: i64,
    strategy: PublishStrategy,
}

impl SequenceClaim {
    pub fn start(&self) -> i64 { self.start }
    pub fn end(&self) -> i64 { self.end }
    pub fn iter(&self) -> impl Iterator<Item = i64> {
        self.start..=self.end
    }
}
```

`#[must_use]` warns at compile time if you ignore the return value of `claim()` — because dropping without writing publishes stale bytes.

The `strategy` field is where the publish action lives. You might think of using a closure:

```rust
publish: Box<dyn FnOnce()>,  // ~20-50ns heap allocation — per claim!
```

But the entire claim targets ~10ns. A heap allocation per claim would cost more than the claim itself. Instead, we use an enum — inline, zero allocation:

```rust
enum PublishStrategy {
    /// Single-producer: just advance the cursor
    Cursor {
        cursor: Arc<Sequence>,
        wait_strategy: Arc<dyn WaitStrategy>,
    },
    /// Multi-producer: mark individual slots as published (covered in Part 3C)
    AvailableBuffer {
        available_buffer: Arc<[AtomicI32]>,
        index_mask: usize,
        buffer_size: usize,
        wait_strategy: Arc<dyn WaitStrategy>,
    },
}
```

The `Cursor` variant is all we need for this post. It holds an `Arc` clone of the sequencer's cursor — so Drop can advance the cursor without calling back to the sequencer. The `AvailableBuffer` variant handles multi-producer publishing and is covered in Part 3C.

Both variants include `wait_strategy` so that Drop can wake consumers that may be sleeping on a condvar (Part 4). For non-blocking strategies like `BusySpin`, this call is a no-op.

### The Drop: Publish + Wake

```rust
impl Drop for SequenceClaim {
    fn drop(&mut self) {
        match &self.strategy {
            PublishStrategy::Cursor { cursor, wait_strategy } => {
                cursor.set(self.end);                   // Release store — Pattern 1 from Part 3A
                wait_strategy.signal_all_when_blocking(); // Wake sleeping consumers (no-op for BusySpin)
            }
            PublishStrategy::AvailableBuffer {
                available_buffer, index_mask, buffer_size, wait_strategy,
            } => {
                for seq in self.start..=self.end {
                    let index = (seq as usize) & index_mask;
                    let lap = (seq / *buffer_size as i64) as i32;
                    available_buffer[index].store(lap, Ordering::Release);
                }
                wait_strategy.signal_all_when_blocking();
            }
        }
    }
}
```

Two steps: (1) make the data visible with Release ordering, (2) wake any blocking consumers. This matches Java Disruptor's `waitStrategy.signalAllWhenBlocking()`, called after every publish.

---

## Designing the SingleProducerSequencer

We have the trait and the claim guard. Now we need a concrete sequencer for the simplest case: exactly one producer thread.

### Two Counters, Two Audiences

A sequencer needs to answer two questions:
- **Producer asks:** "Which slot should I write to next?"
- **Consumer asks:** "How far can I safely read?"

These are different questions with different answers. The producer might have *claimed* slot 7 but not yet *published* it — the consumer must not read slot 7 until the producer says it is ready.

This means we need two counters:

```rust
cursor: Arc<Sequence>,       // "consumers can read up to here" (published position)
next_sequence: Cell<i64>,    // "producer will claim this slot next" (internal bookkeeping)
```

The `cursor` is shared — the producer writes it (on publish), consumers read it. So it lives behind `Arc`, and it uses our 128-byte-aligned `Sequence` type to prevent the cursor's cache line from bouncing between producer and consumer cores on every publish-read cycle.

The `next_sequence` is private — only the producer reads and writes it. No other thread ever touches it. So it does not need `Arc`, does not need atomic operations, and does not need cache-line padding.

But `claim()` takes `&self`, not `&mut self` (because the sequencer is stored in a struct that consumers also reference). We need interior mutability — the ability to modify `next_sequence` through a shared reference. Two options:

- `AtomicI64` — works, but pays for synchronization we do not need (no other thread reads this)
- `Cell<i64>` — interior mutability with zero overhead. `Cell::get` and `Cell::set` compile to plain `MOV` instructions on x86. No atomic, no barrier, no bus lock.

`Cell` wins. And it gives us a bonus: `Cell` is `!Sync` by design. This means `SingleProducerSequencer` cannot be shared across threads via `Arc`. The compiler enforces the single-producer invariant at the type level. Try to share it — compile error.

**Why `i64`, not `u64`?** The cursor starts at -1 (meaning "nothing published yet"). First publish advances it to 0. Signed arithmetic makes this sentinel natural. At 1 billion events/sec, `i64` overflow takes ~292 years.

### Preventing Wrap-Around

The ring buffer has a fixed number of slots. If the producer races ahead of consumers, it will wrap around and overwrite data consumers have not yet read.

To prevent this, the sequencer needs to know where the consumers are:

```rust
gating_sequences: Vec<Arc<Sequence>>,   // each consumer's current position
```

Each consumer owns an `Arc<Sequence>` and updates it from its own thread after processing events. The sequencer holds `Arc` clones of these sequences. Before claiming a slot, the producer checks: "would this slot overwrite something the slowest consumer has not read?"

The check: `wrap_point = next - 1 - buffer_size`. If any consumer's position is less than `wrap_point`, the producer must wait.

But iterating all consumers on every `claim()` is expensive — O(N) atomic loads with Acquire ordering. Most of the time, consumers are keeping up and the check is unnecessary. So we cache the minimum:

```rust
cached_gating_sequence: Cell<i64>,   // cached minimum of all gating sequences
```

`Cell` again — only the producer reads and writes the cache. The cache is checked first (fast path: one plain load, one comparison). Only when the producer is about to wrap past the cached minimum does it rescan all consumers (slow path: O(N) Acquire loads).

The cache invalidation condition is `wrap_point > cached || cached > current`. The first check catches "we might be lapping a consumer." The second catches a stale cache from a previous wrap.

### The Remaining Pieces

Two more fields:

```rust
buffer_size: usize,                  // ring buffer capacity (power of 2, immutable)
wait_strategy: Arc<dyn WaitStrategy>, // how to wake consumers after publishing
```

`buffer_size` is stored here — not as a reference to the ring buffer — because the sequencer is deliberately decoupled from the ring buffer's type `T`. It only needs the size for wrap-around arithmetic.

`wait_strategy` is passed into every `SequenceClaim` so that `Drop` can call `signal_all_when_blocking()`. Without this, consumers using `BlockingWaitStrategy` (condvar) would sleep forever after events are published.

### The Complete Struct

```rust
use std::cell::Cell;

pub struct SingleProducerSequencer {
    cursor: Arc<Sequence>,                  // published position (consumers read this)
    next_sequence: Cell<i64>,               // next slot to claim (producer-only)
    buffer_size: usize,                     // ring capacity (immutable, power of 2)
    gating_sequences: Vec<Arc<Sequence>>,   // consumer positions (producer reads, consumers write)
    cached_gating_sequence: Cell<i64>,      // cached min of gating_sequences (producer-only)
    wait_strategy: Arc<dyn WaitStrategy>,   // passed into SequenceClaim for wake-on-publish
}
```

Every field arrived because the previous decision created a need for it: we needed two counters → the private one uses `Cell` → the public one needs `Arc<Sequence>` → we need consumer tracking → we cache the minimum → we need to wake consumers on publish.

---

## The Constructor

```rust
impl SingleProducerSequencer {
    pub fn new(
        buffer_size: usize,
        gating_sequences: Vec<Arc<Sequence>>,
        wait_strategy: Arc<dyn WaitStrategy>,
    ) -> Self {
        assert!(buffer_size.is_power_of_two(), "buffer_size must be power of 2");
        Self {
            cursor: Arc::new(Sequence::new(-1)),   // nothing published yet
            next_sequence: Cell::new(0),            // first claim returns 0
            buffer_size,
            gating_sequences,
            cached_gating_sequence: Cell::new(-1),  // forces first claim to check consumers
            wait_strategy,
        }
    }

    /// Clone the cursor Arc for consumers to observe published sequences.
    /// Call this before moving the sequencer to the producer thread —
    /// once moved, you cannot access it from the original thread (!Sync).
    pub fn cursor_arc(&self) -> Arc<Sequence> {
        Arc::clone(&self.cursor)
    }

    fn get_minimum_sequence(&self) -> i64 {
        let mut minimum = i64::MAX;
        for seq in &self.gating_sequences {
            minimum = minimum.min(seq.get());
        }
        self.cached_gating_sequence.set(minimum);
        minimum
    }
}

---

## `claim()` — The Hot Path

The `claim()` method is called on every event the producer publishes. It must be fast — the target is ~10ns for the common case.

```rust
impl Sequencer for SingleProducerSequencer {
    fn claim(&self, count: usize) -> SequenceClaim {
        assert!(count > 0 && count <= self.buffer_size,
            "count must be > 0 and <= buffer_size");

        // Step 1: Read the next sequence to claim (Cell — plain load).
        let current = self.next_sequence.get();
        let next = current + count as i64;

        // Step 2: Compute wrap point — the sequence that would overwrite
        // the oldest unconsumed slot.
        //
        // Example: buffer_size=4, claiming sequence 7 (count=1).
        //   wrap_point = 7 - 1 - 4 = 2
        //   If any consumer is at sequence 2, we cannot write sequence 7
        //   because sequence 7 maps to the same slot as sequence 3
        //   (index = 7 & 3 = 3), and writing it would overwrite slot 3
        //   while the consumer at sequence 2 has not yet read slot 3.
        //
        //   Wait — actually, wrap_point=2 means the consumer must be PAST 2.
        //   The consumer at 2 has read slots 0,1,2 and is about to read 3.
        //   We need the consumer to have read slot 3 (sequence 3) before
        //   we overwrite it with sequence 7. So we need min_gating > 2.
        let wrap_point = next - 1 - self.buffer_size as i64;

        // Step 3: Check the cache (Cell — plain load).
        let cached = self.cached_gating_sequence.get();

        if wrap_point > cached || cached > current {
            // Cache miss — consumers might be too far behind.

            // StoreLoad fence: publish our latest cursor value so consumers
            // can see it before we read their positions.
            // Note: 'current - 1' is the last PUBLISHED sequence (not the
            // one we are about to claim). This is correct — we are telling
            // consumers what is safe to read.
            self.cursor.set_volatile(current - 1);

            // Spin-wait with CPU yield until consumers catch up.
            while self.get_minimum_sequence() < wrap_point {
                std::thread::park_timeout(std::time::Duration::from_nanos(1));
            }
        }

        // Step 4: Advance next_sequence (Cell — plain store).
        self.next_sequence.set(next);

        // Step 5: Return the RAII claim. Drop will publish.
        SequenceClaim {
            start: current,
            end: next - 1,
            strategy: PublishStrategy::Cursor {
                cursor: Arc::clone(&self.cursor),
                wait_strategy: Arc::clone(&self.wait_strategy),
            },
        }
    }
```

The code has two paths. Let's walk through each.

**Fast path (cache hit):** When the cached gating sequence tells us there is room, the entire operation is Cell reads and writes — no atomics, no memory barriers. `Cell::get` on `next_sequence` (plain load), compute `wrap_point = next - 1 - buffer_size`, `Cell::get` on `cached_gating_sequence` (plain load), compare `wrap_point <= cached && cached <= current` — cache is valid, plenty of room. Then `Cell::set` on `next_sequence` (plain store) to advance, and create `SequenceClaim` with `Arc::clone` on cursor and wait_strategy (~5ns each). Total cost: ~10ns. Two plain loads, one plain store, two atomic increments (the `Arc::clone`s). No cache line bouncing, no memory barriers.

**Slow path (cache miss):** When `wrap_point > cached || cached > current`, the producer is at risk of lapping a consumer. First, `cursor.set_volatile(current - 1)` — a SeqCst store. This is the StoreLoad fence from Part 3A. It ensures the cursor value (the last published sequence) is visible to all consumers **before** we read their positions. Without this fence, the CPU could reorder the cursor write past the subsequent consumer reads, creating a livelock: the producer waits for consumers to advance, but consumers cannot advance because they never saw the published data. This matches Java Disruptor's `cursor.setVolatile(nextValue)` in `SingleProducerSequencer.next()`. Then iterate all gating sequences — O(N) Acquire loads to find the minimum consumer position, updating the cache. If consumers are still too far behind, the producer yields the CPU via `park_timeout(1ns)`. Why `park_timeout(1ns)` instead of `spin_loop`? The producer is waiting for consumers to process events. This is backpressure, not a short spin. Busy-spinning here starves consumer threads on the same physical core (HyperThreading shares execution resources), causes thermal throttling under sustained load, and wastes power for zero benefit (consumers need microseconds, not nanoseconds). Java's `LockSupport.parkNanos(1L)` is a self-waking park — it suspends the thread briefly and wakes on timeout. Nobody calls `unpark()` on the producer; the 1ns timeout is the sole wakeup mechanism. Rust's `park_timeout` is the direct equivalent.

**Usage constraint: one outstanding claim at a time.** The `cursor.set_volatile(current - 1)` on the slow path assumes all prior claims have been dropped (published). If you hold multiple `SequenceClaim`s simultaneously, the cursor store may advertise a sequence before you have written its data — a data race. This matches the Java Disruptor's `SingleProducerSequencer`, which has the same constraint. In practice, the natural pattern of `{ let claim = sequencer.claim(1); write_data(&claim); }` enforces this automatically.

**Performance:**
- **Cache hit**: ~10ns (Cell get/set + cache comparison)
- **Cache miss**: ~50-100ns (iterate consumers + update cache)
- **Waiting on consumers**: depends on consumer throughput and wait strategy

---

## `try_claim()` — Non-Blocking Variant

`try_claim()` follows the same logic as `claim()` but returns `Err(InsufficientCapacity)` instead of parking when consumers are too far behind. This is useful for producers that can drop events or apply backpressure upstream.

```rust
    fn try_claim(&self, count: usize) -> Result<SequenceClaim, InsufficientCapacity> {
        assert!(count > 0 && count <= self.buffer_size,
            "count must be > 0 and <= buffer_size");

        let current = self.next_sequence.get();
        let next = current + count as i64;
        let wrap_point = next - 1 - self.buffer_size as i64;

        let cached = self.cached_gating_sequence.get();
        if wrap_point > cached || cached > current {
            // StoreLoad fence, same as claim().
            self.cursor.set_volatile(current - 1);

            // Non-blocking: check once and return immediately.
            if self.get_minimum_sequence() < wrap_point {
                return Err(InsufficientCapacity);
            }
        }

        self.next_sequence.set(next);

        Ok(SequenceClaim {
            start: current,
            end: next - 1,
            strategy: PublishStrategy::Cursor {
                cursor: Arc::clone(&self.cursor),
                wait_strategy: Arc::clone(&self.wait_strategy),
            },
        })
    }

    fn cursor(&self) -> i64 {
        self.cursor.get()
    }
}
```

The only difference from `claim()` is the absence of the `while` loop. If consumers have not caught up, `try_claim()` returns `Err` immediately. The caller decides what to do: drop the event, buffer it elsewhere, or retry later.

---

## Key Takeaways

1. **One outstanding claim at a time.** The `cursor.set_volatile(current - 1)` on the slow path assumes all prior claims have been dropped. Holding two `SequenceClaim`s simultaneously is a data race. The natural `{ let claim = ...; write; }` pattern enforces this automatically.

2. **The producer parks, it doesn't spin.** When waiting for slow consumers, `park_timeout(1ns)` yields the CPU. Busy-spinning here starves consumer threads on HyperThreading cores and wastes power. This is backpressure, not a short wait.

3. **The cache saves ~10x on the fast path.** Without `cached_gating_sequence`, every claim iterates all consumers (O(N) atomic loads). With caching, most claims compare two `Cell` values (O(1) plain loads). The cache invalidates only at the wrap point.

---

## Next Up

In **Part 3C**, we will build the `MultiProducerSequencer` — where every field choice changes because contention enters the picture. `Cell` becomes `AtomicI64`. The cursor represents claimed (not published) sequences. And a new data structure, the `available_buffer`, tracks out-of-order publishing.

---

**Next:** [Part 3C — MultiProducerSequencer -->](post3c.md)

---

## References

- [LMAX Disruptor Technical Paper](https://lmax-exchange.github.io/disruptor/disruptor.html)
- [Rust Atomics and Locks](https://marabos.nl/atomics/) — Mara Bos (O'Reilly, 2023)
- [Java Disruptor Source: SingleProducerSequencer.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/SingleProducerSequencer.java)
