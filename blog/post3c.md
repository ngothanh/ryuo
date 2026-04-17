# Building a Disruptor in Rust: Ryuo — Part 3C: The MultiProducerSequencer

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 3C of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 3B (SingleProducerSequencer, SequenceClaim, PublishStrategy)

---

## Recap

In Part 3B we built the `SingleProducerSequencer` — `Cell` for zero-overhead claims, `!Sync` at the type level, auto-publish via `Drop`. Now: what changes when you add a second producer?

---

## When Single Producer Isn't Enough

Some systems have one natural producer — a network socket, a file reader, a sensor. But many do not:

| Use Case | Why Multiple Producers |
|---|---|
| Market data aggregation | Multiple exchange feeds writing to a shared buffer |
| Web server request logging | Thread-per-core workers each producing log events |
| Telemetry pipeline | Independent subsystems (CPU, memory, network) emitting metrics |
| Game engine | Physics, AI, and input subsystems writing events in the same tick |

You could funnel everything through a single producer thread, but that serialization point becomes the bottleneck. The disruptor pattern supports multiple producers directly — if the sequencer can handle it.

Three things break when you add a second producer:

**1. `Cell` cannot be shared.** `Cell` is `!Sync`. Two threads cannot hold `&SingleProducerSequencer` simultaneously. The compiler rejects it. This is not a bug — it is the type system doing its job. `Cell::set` is a plain store; two threads calling it concurrently is a data race.

**2. The cursor's meaning collapses.** In single-producer mode, the cursor advances in `SequenceClaim::drop` after the data is written. The cursor always means "everything up to here is published and safe to read." With multiple producers, this guarantee evaporates.

**3. Contention on the claim counter.** Two producers calling `Cell::set` is a data race. Two producers calling `fetch_add` is correct but contested — the cache line bounces between cores on every claim. This is the cost of coordination, and it is unavoidable. The question is whether we pay it once (on claim) or twice (on claim and publish).

Let's start with problem 2, because it drives the most surprising design decisions.

---

## The Cursor Lies

In `SingleProducerSequencer`, the cursor is the source of truth. The producer claims a slot, writes data, and drops the `SequenceClaim`. Drop calls `cursor.set(self.end)` with Release ordering. The consumer loads the cursor with Acquire ordering, sees the new value, and reads the data. The cursor means "published."

What happens if we try the same approach with two producers?

Imagine two producers sharing an atomic cursor that starts at -1 (nothing published). Both call `claim(1)`. To avoid giving them the same slot, `claim` must use `fetch_add` — an atomic read-modify-write that returns the previous value and increments in a single instruction. No two callers get the same number.

But `fetch_add` advances the cursor **before** either producer has written anything. Here is the timeline:

```
t0: cursor = -1 (nothing published)

t1: Producer A calls claim(1)
    fetch_add(1) returns -1, cursor becomes 0
    A now owns sequence 0. A starts writing data.

t2: Producer B calls claim(1)
    fetch_add(1) returns 0, cursor becomes 1
    B now owns sequence 1. B starts writing data.

t3: B finishes writing, drops its SequenceClaim.
    B thinks "I should set cursor to 1" — but cursor is already 1.

t4: Consumer sees cursor = 1. It reads sequences 0 and 1.
    Sequence 1 has B's data — correct.
    Sequence 0 has GARBAGE — A hasn't finished writing yet.
```

The consumer trusted the cursor. The cursor lied.

The problem is structural. `fetch_add` does two things atomically: reserves the slot and advances the cursor. But reserving a slot and publishing data are different events separated by an unknown amount of time. The producer needs the slot number to know **where** to write, so the cursor must advance before the write. But the consumer needs to know when the write is **done**, so the cursor must advance after the write.

These two requirements contradict each other. A single counter cannot serve both purposes.

**The insight:** In multi-producer mode, the cursor means "claimed," not "published." It tells you which slots have been reserved, not which slots contain valid data. The consumer needs a different way to know what is safe to read.

---

## Tracking Publication Per-Slot

The consumer's question is simple: "Is sequence N safe to read?" The cursor cannot answer this anymore. So what can?

### Why Booleans Don't Work

The most obvious idea: an array of flags, one per ring buffer slot. When a producer finishes writing to slot 3, it sets `published[3] = true`. The consumer checks `published[3]` before reading.

```
published: [false, false, false, false]   // buffer_size = 4

Producer A publishes sequence 2 -> published[2] = true
Consumer checks published[2] -> true -> safe to read
```

This works for the first lap around the ring. But what happens when the buffer wraps?

With `buffer_size = 4`, sequence 2 maps to index 2. Sequence 6 also maps to index 2 (`6 & 3 = 2`). Producer A publishes sequence 2 and sets `published[2] = true`. The ring wraps. Producer B claims sequence 6 (same index). Before B finishes writing, the consumer checks `published[2]` — it still says `true` from the previous lap.

The consumer reads stale data. The boolean cannot distinguish "published in this lap" from "published in a previous lap."

### Lap Counters

Instead of storing true/false, store **which lap** published this slot. The consumer does not ask "is this slot published?" — it asks "was this slot published in the lap I expect?"

The lap for a given sequence is how many times the ring has wrapped:

```rust
fn availability_flag(sequence: i64, buffer_size: usize) -> i32 {
    (sequence / buffer_size as i64) as i32
}
```

Concrete example with `buffer_size = 4`:

```
sequence  0 -> index 0, lap 0
sequence  1 -> index 1, lap 0
sequence  3 -> index 3, lap 0
sequence  4 -> index 0, lap 1    <- ring wraps
sequence  6 -> index 2, lap 1
sequence 10 -> index 2, lap 2    <- second wrap for this index
```

When Producer A publishes sequence 2, it stores `lap = 0` at index 2. When Producer B later publishes sequence 6 at the same index, it stores `lap = 1`. The consumer expecting sequence 6 computes `expected_lap = 6 / 4 = 1`, loads the stored lap, and compares. If the stored lap is still 0 (from the old publish), the consumer knows B has not finished yet. Each lap overwrites the previous lap's marker with a strictly larger value.

### Initialization and Data Types

The first sequence is 0. Its lap is `0 / buffer_size = 0`. If we initialize all entries to 0, every slot looks published in lap 0 — the consumer would read unpublished data from the start. Initializing to -1 solves this. No valid lap is ever -1 (sequences start at 0, so the minimum lap is 0). Every slot starts as "not yet published," and the first real publish overwrites -1 with 0.

The sequence counter is i64, but the lap counter only needs i32. i32 is large enough — at 1 billion events/sec with `buffer_size = 1024`, the lap wraps every ~4 seconds, but both sides compute the same truncation via `as i32`, so comparisons stay correct across wraps. And it halves the memory footprint vs i64.

The available buffer is written by producers (on publish) and read by consumers (to check safety). It must be shared across threads, fixed-size, and atomic per entry — so it lives in an `Arc<[AtomicI32]>`:

```rust
available_buffer: Arc<[AtomicI32]>,
```

### Publishing: The Drop Path

On publish, the `SequenceClaim` drops and marks each claimed sequence as available. This is the `AvailableBuffer` variant of `PublishStrategy` that we previewed in Part 3B:

```rust
// In SequenceClaim::drop
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
```

Release ordering on the store. This is the same pattern as the single-producer cursor — the producer's data writes must be visible before the availability flag becomes visible. The flag is the synchronization point.

### Consuming: Scanning for Gaps

On the consumer side, the sequencer exposes `get_highest_published_sequence`. The consumer knows the cursor value (the highest claimed sequence) and asks: "starting from where I left off, how far can I actually read?"

```rust
pub fn get_highest_published_sequence(
    &self,
    lower_bound: i64,
    available_sequence: i64,
) -> i64 {
    for sequence in lower_bound..=available_sequence {
        let index = (sequence as usize) & self.index_mask;
        let expected_lap = (sequence / self.buffer_size as i64) as i32;
        if self.available_buffer[index].load(Ordering::Acquire) != expected_lap {
            return sequence - 1;
        }
    }
    available_sequence
}
```

The consumer scans forward from `lower_bound` (its current position + 1) to `available_sequence` (the cursor, which represents the highest claimed slot). For each sequence, it loads the lap counter with Acquire ordering and compares it to the expected lap. The moment it finds a gap — a slot where the producer has not yet called drop — it stops and returns the last safe sequence.

Acquire on the load pairs with Release on the store. When the consumer sees the expected lap value, all data writes that happened before the store are guaranteed visible. Same happens-before relationship as the single-producer cursor, but per-slot instead of per-cursor.

### Proving It Works: An Out-of-Order Trace

The available buffer exists to handle out-of-order publishing. Let's trace through a complete scenario.

Setup: `buffer_size = 4`, cursor starts at -1, available buffer initialized to `[-1, -1, -1, -1]`.

```
t0: Producer A calls claim(1)
    fetch_add(1) -> returns -1, cursor becomes 0
    A owns sequence 0 (index 0, lap 0)
    A starts writing data to slot 0

t1: Producer B calls claim(1)
    fetch_add(1) -> returns 0, cursor becomes 1
    B owns sequence 1 (index 1, lap 0)
    B starts writing data to slot 1

t2: B finishes first, drops SequenceClaim
    available_buffer[1].store(0, Release)    // lap 0 for sequence 1
    Buffer: [-1, 0, -1, -1]

t3: Consumer calls get_highest_published_sequence(0, 1)
    Check sequence 0: index 0, expected lap 0
      available_buffer[0].load(Acquire) -> -1
      -1 != 0 -> STOP
    Returns -1 (nothing safe to read yet)

t4: A finishes, drops SequenceClaim
    available_buffer[0].store(0, Release)    // lap 0 for sequence 0
    Buffer: [0, 0, -1, -1]

t5: Consumer calls get_highest_published_sequence(0, 1)
    Check sequence 0: index 0, expected lap 0
      available_buffer[0].load(Acquire) -> 0
      0 == 0 -> continue
    Check sequence 1: index 1, expected lap 0
      available_buffer[1].load(Acquire) -> 0
      0 == 0 -> continue
    Returns 1 (sequences 0 and 1 are safe to read)
```

At t3, the consumer sees that B published sequence 1 but A has not published sequence 0. It does not skip ahead to read sequence 1 — it stops at the gap. At t5, both slots are published and the consumer reads both.

The critical property: **the consumer never saw garbage.** The gap at sequence 0 stopped the scan, even though the cursor said "1" (claimed through sequence 1). The available buffer told the truth that the cursor could not.

Now we know how publishing works per-slot. But we have not yet addressed how multiple producers claim those slots in the first place.

---

## Claiming with `fetch_add`

On x86, `fetch_add` compiles to a single `LOCK XADD` instruction. It atomically reads the old value, adds the operand, and stores the result — all in one bus-locked cycle. It never fails. Every producer gets a unique slot in O(1):

```rust
let current = self.cursor.fetch_add(count as i64, Ordering::AcqRel);
// current = old value. Our range is [current + 1, current + count].
// No other thread can get overlapping values — the hardware guarantees it.
```

**Why `AcqRel`, not `SeqCst`?** In `SingleProducerSequencer`, `claim()` called `cursor.set_volatile(current - 1)` — a SeqCst store — to create a StoreLoad fence before reading consumer positions. The producer needed to ensure its cursor write was visible before loading consumer sequences. Without the fence, the CPU could reorder the store past the loads, creating a livelock.

`fetch_add` with `AcqRel` does not need a separate fence because it is both a store AND a load in one atomic operation. The Acquire component ensures we see all prior writes from other threads. The Release component ensures our cursor advance is visible to others. The hardware cannot reorder the two halves of a single atomic read-modify-write instruction — the store is inherently ordered after the load. This gives us the same StoreLoad guarantee that single-producer achieved with an explicit `SeqCst` fence, but without paying for global sequential consistency.

### Backpressure After the Claim

`fetch_add` advances the cursor unconditionally. What if the buffer is full? You cannot un-advance. Other producers have already seen the new cursor value and claimed sequences beyond it. Rolling back with `fetch_add(-count)` creates a window where the cursor is in an inconsistent state — other producers could claim into the rolled-back range.

So backpressure happens AFTER the `fetch_add`. The producer claims the slot optimistically, then waits if necessary:

```rust
let current = self.cursor.fetch_add(count as i64, Ordering::AcqRel);
let end = current + count as i64;
let wrap_point = end - self.buffer_size as i64;

// If wrap_point > min consumer position, we've claimed past what consumers
// have read. We can't un-claim, so we park and wait for consumers to catch up.
```

This is safe because the claimed-but-not-published slot is invisible to consumers — they check the `available_buffer`, not the cursor. Other producers can still claim their own slots concurrently. The only cost is that the claiming producer blocks until consumers free up space.

### `try_claim`: When You Can't Wait

Sometimes you want "give me a slot or tell me no." `fetch_add` cannot do this — it always advances the cursor, even when the buffer is full. So `try_claim` uses CAS:

1. Load the current cursor value.
2. Check capacity — compute wrap point, compare against slowest consumer.
3. If no capacity, return `Err(InsufficientCapacity)`. The cursor is unchanged.
4. `compare_exchange(old, new)` — if success, we own the range; if fail, another producer won the race, retry from step 1.

CAS costs more under contention — O(N) retries under N-way contention, compared to `fetch_add`'s O(1). But it is the only option when you need to check before committing.

**Fairness note.** CAS-based claiming is not fair. Under high contention, a slow producer (one that gets preempted between the load and the CAS) can be starved indefinitely because faster producers keep winning the race. `fetch_add` does not have this problem — it is hardware-FIFO on x86. If fairness matters for your use case, shard the ring buffer (one single-producer ring per producer) or cap your producer count at 4 where CAS failure rates remain manageable (~30%).

Now we have all the pieces. Time to put them together.

---

## The Complete Struct

```rust
pub struct MultiProducerSequencer {
    cursor: Arc<Sequence>,                  // highest CLAIMED sequence (not published!)
    buffer_size: usize,                     // ring capacity (immutable, power of 2)
    index_mask: usize,                      // buffer_size - 1, for fast modulo
    gating_sequences: Vec<Arc<Sequence>>,   // consumer positions (same as single-producer)
    cached_gating_sequence: AtomicI64,      // cached min of gating_sequences
    available_buffer: Arc<[AtomicI32]>,     // per-slot lap counters for publication tracking
    wait_strategy: Arc<dyn WaitStrategy>,   // wake consumers on publish (same as single-producer)
}
```

Three things changed from `SingleProducerSequencer`, and each change traces back to the same root cause — multiple writers:

**`Cell<i64>` became `AtomicI64`** for `cached_gating_sequence`. In single-producer, only one thread ever reads or writes the cache, so `Cell` was sufficient. In multi-producer, any producer thread may update the cache during `claim()`. `Cell` under concurrent access is a data race; `AtomicI64` with Relaxed ordering adds negligible cost because the cache is a performance optimization, not a correctness mechanism.

**Two new fields appeared:** `index_mask` (precomputed `buffer_size - 1` for slot indexing) and `available_buffer` (the per-slot publication tracker). Single-producer did not need these because its cursor directly represented the published position.

**`!Sync` became `Sync`.** Every field is either atomic (`Arc<Sequence>`, `AtomicI64`, `Arc<[AtomicI32]>`) or immutable (`usize`). The compiler derives `Sync` automatically. Multiple threads can safely call `claim()` concurrently through `Arc<MultiProducerSequencer>`.

### Constructor

```rust
impl MultiProducerSequencer {
    pub fn new(
        buffer_size: usize,
        gating_sequences: Vec<Arc<Sequence>>,
        wait_strategy: Arc<dyn WaitStrategy>,
    ) -> Self {
        assert!(buffer_size.is_power_of_two(), "buffer_size must be power of 2");

        // Every slot starts as -1 (unpublished).
        // First published sequence 0 has lap = 0 / buffer_size = 0.
        // -1 != 0, so no slot appears published until explicitly set.
        let available_buffer: Arc<[AtomicI32]> = (0..buffer_size)
            .map(|_| AtomicI32::new(-1))
            .collect();

        Self {
            cursor: Arc::new(Sequence::new(-1)),       // first claim returns 0
            buffer_size,
            index_mask: buffer_size - 1,
            gating_sequences,
            cached_gating_sequence: AtomicI64::new(-1), // forces first claim to scan consumers
            available_buffer,
            wait_strategy,
        }
    }

    /// Clone the cursor Arc for consumers to observe claimed sequences.
    pub fn cursor_arc(&self) -> Arc<Sequence> {
        Arc::clone(&self.cursor)
    }

    fn get_minimum_sequence(&self) -> i64 {
        let mut minimum = i64::MAX;
        for seq in &self.gating_sequences {
            minimum = minimum.min(seq.get());
        }
        self.cached_gating_sequence.store(minimum, Ordering::Relaxed);
        minimum
    }
}
```

Both `cursor` and `available_buffer` start at -1. The cursor starts at -1 so `fetch_add(1)` returns -1, giving `start = 0` for the first claim. The available buffer starts at -1 because the first valid lap number is 0 — if slots started at 0, every slot would appear already published on lap 0 before any producer has written anything.

### `claim()` — Full Code

```rust
impl Sequencer for MultiProducerSequencer {
    fn claim(&self, count: usize) -> SequenceClaim {
        assert!(count > 0 && count <= self.buffer_size,
            "count must be > 0 and <= buffer_size");

        // Step 1: Atomically claim a range. fetch_add returns the OLD value.
        // cursor starts at -1, so first claim(1) gets current = -1 -> start = 0.
        let current = self.cursor.fetch_add(count as i64, Ordering::AcqRel);
        let end = current + count as i64;

        // Step 2: Backpressure — ensure we haven't lapped the slowest consumer.
        let wrap_point = end - self.buffer_size as i64;

        // Fast path: check the cached minimum consumer position (Relaxed load).
        let cached = self.cached_gating_sequence.load(Ordering::Relaxed);
        if wrap_point > cached || cached > current {
            // Cache miss — consumers might be too far behind.
            // No StoreLoad fence needed: fetch_add(AcqRel) already made our
            // cursor advance visible before we read consumer positions.
            while self.get_minimum_sequence() < wrap_point {
                std::thread::park_timeout(std::time::Duration::from_nanos(1));
            }
        }

        // Step 3: Return the RAII claim. Drop writes lap counters to available_buffer.
        SequenceClaim {
            start: current + 1,
            end,
            strategy: PublishStrategy::AvailableBuffer {
                available_buffer: Arc::clone(&self.available_buffer),
                index_mask: self.index_mask,
                buffer_size: self.buffer_size,
                wait_strategy: Arc::clone(&self.wait_strategy),
            },
        }
    }
```

**Fast path:** `fetch_add` succeeds (it always does), Relaxed load of the cache shows plenty of room, create `SequenceClaim`. Cost: one atomic read-modify-write, one Relaxed load, one comparison, two `Arc::clone`s. Total: ~50ns.

**Slow path:** Cache miss triggers `get_minimum_sequence()` — O(N) Acquire loads across all consumer sequences. If consumers are still behind the wrap point, `park_timeout(1ns)` yields the CPU. Same rationale as single-producer: we are waiting for consumers to process events, which takes microseconds. Busy-spinning here would starve consumer threads on HyperThreading cores.

### `try_claim()` — Full Code

```rust
    fn try_claim(&self, count: usize) -> Result<SequenceClaim, InsufficientCapacity> {
        assert!(count > 0 && count <= self.buffer_size,
            "count must be > 0 and <= buffer_size");

        loop {
            let current = self.cursor.get();
            let end = current + count as i64;
            let wrap_point = end - self.buffer_size as i64;

            // Check capacity before attempting to advance the cursor.
            let cached = self.cached_gating_sequence.load(Ordering::Relaxed);
            if wrap_point > cached || cached > current {
                if self.get_minimum_sequence() < wrap_point {
                    return Err(InsufficientCapacity);
                }
            }

            // CAS: advance cursor only if no other producer claimed since our load.
            if self.cursor.compare_and_set(current, end) {
                return Ok(SequenceClaim {
                    start: current + 1,
                    end,
                    strategy: PublishStrategy::AvailableBuffer {
                        available_buffer: Arc::clone(&self.available_buffer),
                        index_mask: self.index_mask,
                        buffer_size: self.buffer_size,
                        wait_strategy: Arc::clone(&self.wait_strategy),
                    },
                });
            }
            // CAS failed — another producer won. Retry with fresh cursor value.
        }
    }
```

Same structure as `claim()` but with CAS instead of `fetch_add`. The loop retries only on CAS failure (another producer moved the cursor between our load and our exchange). It does NOT retry on insufficient capacity — that returns `Err` immediately. We use strong `compare_exchange`, not `compare_exchange_weak`. A spurious failure (which `_weak` permits on ARM) after passing the capacity check would force an unnecessary retry.

### `cursor()`

```rust
    fn cursor(&self) -> i64 {
        self.cursor.get()
    }
}
```

---

## SingleProducer vs MultiProducer

| Aspect | SingleProducer | MultiProducer |
|--------|---------------|---------------|
| Claiming | `Cell::set` (plain store) | `fetch_add` (LOCK XADD) |
| Publishing | `cursor.set` (one Release store) | Per-slot `available_buffer` writes |
| Consumer visibility | `cursor` = published position | Must scan `available_buffer` |
| Contention | None (single writer) | O(N) cache-line bouncing on cursor |
| Thread safety | `!Sync` (compile-time enforced) | `Sync` (multi-thread safe) |
| `cached_gating_sequence` | `Cell<i64>` (no atomic needed) | `AtomicI64` (Relaxed) |
| StoreLoad fence in claim | `set_volatile` (explicit SeqCst) | Implicit in `fetch_add(AcqRel)` |
| p50 latency (uncontended) | ~10ns | ~50ns |
| p50 latency (4 producers) | N/A | ~100-200ns |
| Memory overhead | 0 extra | `buffer_size * 4` bytes |
| Out-of-order publish | Impossible (single writer) | Handled by available buffer |
| `try_claim` mechanism | `Cell` read + capacity check | CAS loop + capacity check |

**Rule of thumb:** Use `SingleProducerSequencer` whenever you can guarantee one writer. It is 5-20x faster. The multi-producer overhead comes from three sources: atomic claiming (~5x), per-slot publishing (~2x), and cache-line contention under load (unbounded). Most systems can be restructured to have a single writer per ring buffer — route events through a dedicated ingest thread, or shard into multiple single-producer rings.

---

## Key Takeaways

1. **Every field change from SingleProducer traces to one cause: multiple writers.** `Cell` becomes `AtomicI64`. A single cursor becomes cursor + available buffer. `!Sync` becomes `Sync`. There are no arbitrary choices — the design follows directly from the concurrency requirements.

2. **The cursor changes roles, and that role change forces the biggest design decision.** In single-producer, cursor = published. In multi-producer, cursor = claimed. That single semantic shift is what creates the need for the available buffer, lap counters, and per-slot Release/Acquire pairs. Everything else is a consequence.

3. **`fetch_add` for `claim()`, CAS for `try_claim()` — and the asymmetry is fundamental.** `fetch_add` always succeeds, so backpressure happens after the cursor advances. CAS can fail, so capacity is checked before the cursor moves. The two operations serve different contracts: "give me a slot, I'll wait" vs "give me a slot or tell me no."

---

## Next Up

In **Part 3D**, we will see both sequencers in action with usage examples, Loom concurrency testing, and performance measurements.

**Next:** [Part 3D — Usage, Testing & Performance -->](post3d.md)

---

## References

- [LMAX Disruptor Technical Paper](https://lmax-exchange.github.io/disruptor/disruptor.html) — Original design rationale
- [Java Disruptor MultiProducerSequencer.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/MultiProducerSequencer.java) — Reference implementation
- Mara Bos, *Rust Atomics and Locks* (O'Reilly, 2023) — Chapters 2-3 on `fetch_add`, CAS, and memory ordering
