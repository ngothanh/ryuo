# Building a Disruptor in Rust: Ryuo — Part 9: Multi-Producer — CAS, Contention, and Coordination Costs

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 9 of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 3B (single-producer sequencer), Part 5 (sequence barriers), Part 7 (publishing patterns)

---

## Recap

In Part 3B, we built the `SingleProducerSequencer` — a single-threaded sequencer that uses `Cell` instead of atomics. In Part 5, we introduced the concept of multi-producer gap detection. In Part 7, we built the publishing API that works with *any* sequencer.

But we never built the multi-producer sequencer itself. We've been hand-waving:

```rust
// Part 5 mentioned this, but never showed the implementation
let sequencer = MultiProducerSequencer::new(1024, gating_sequences);
```

This post builds it from scratch and explains every atomic operation.

---

## The Problem

Single-producer is simple: one thread, one cursor, no contention. But many systems have multiple producers:

| System | Producers |
|--------|-----------|
| Market data gateway | One thread per exchange feed |
| Web server | One thread per connection |
| Logging framework | Every application thread |
| Sensor network | One thread per sensor |

With multiple producers, we need three things the single-producer sequencer doesn't provide:

1. **Atomic sequence claiming** — Two producers calling `claim(1)` simultaneously must get different sequences. `Cell` can't do this.
2. **Out-of-order publish detection** — Producer A claims seq 10, Producer B claims seq 11. If B publishes first, consumers must not read seq 11 until seq 10 is also published.
3. **Contention management** — N producers competing for one `AtomicI64` creates contention that degrades with scale.

---

## Why Not fetch_add?

The simplest approach to atomic claiming:

```rust
fn claim(&self, count: usize) -> i64 {
    self.cursor.fetch_add(count as i64, Ordering::AcqRel)
}
```

`fetch_add` is faster than CAS under contention because it never fails — the hardware guarantees exactly one winner per cycle. So why does the Java Disruptor use CAS?

**Because `fetch_add` doesn't check capacity.**

```
Buffer size: 4, Consumer at sequence 2

Producer A: fetch_add(1) → gets seq 7  (slot 7 & 3 = slot 3)
Producer B: fetch_add(1) → gets seq 8  (slot 8 & 3 = slot 0)

But the consumer is only at seq 2! Slots 3 and 0 still have
unread events. fetch_add blindly overwrites them.

With CAS:
Producer A: load cursor=6, check capacity, CAS(6→7) → OK
Producer B: load cursor=7, check capacity... wrap_point=4 > consumer=2
            → spin and wait for consumer to advance
```

**CAS allows us to check capacity *before* advancing the cursor.** This is the fundamental reason for CAS over `fetch_add`.

**However:** In our Rust implementation, we actually *can* use `fetch_add` — if we add a separate capacity check beforehand and accept that rare oversubscription is possible (which the Java Disruptor also handles). We'll use the CAS approach for correctness parity with the Java Disruptor.

---

## Implementation

> **Note on types:** This post uses `Arc<AtomicI64>` in code listings for clarity — showing the raw atomic operations without abstraction layers. In the production implementation (Part 3B), all sequence counters use the cache-padded `Sequence` type (`#[repr(align(128))]`) wrapped in `Arc<Sequence>`. The `Sequence` type provides `get()` (Acquire load), `set()` (Release store), and `compare_and_set()` methods. When integrating with the rest of Ryuo, replace `Arc<AtomicI64>` with `Arc<Sequence>` and use the `Sequence` methods instead of direct `load`/`store` calls.

### Step 1: Data Structure

```rust
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::Arc;

/// Multi-producer sequencer using CAS-based sequence claiming
/// and per-slot availability tracking.
///
/// # Architecture
/// ```text
///   Producer 0    Producer 1    Producer 2
///       |              |              |
///       └──────────────┼──────────────┘
///                      ↓
///              ┌───────────────┐
///              │   cursor      │  ← CAS contention point
///              │   (AtomicI64) │
///              └───────────────┘
///                      ↓
///    ┌────┬────┬────┬────┬────┬────┬────┬────┐
///    │ -1 │ -1 │ -1 │ -1 │ -1 │ -1 │ -1 │ -1 │  available_buffer
///    └────┴────┴────┴────┴────┴────┴────┴────┘
///      0    1    2    3    4    5    6    7
/// ```
pub struct MultiProducerSequencer {
    /// The current cursor position. Producers advance this via CAS.
    cursor: Arc<AtomicI64>,

    /// Consumer sequences that gate how far producers can advance.
    /// Producers cannot overwrite slots that consumers haven't read.
    gating_sequences: Vec<Arc<AtomicI64>>,

    /// Buffer size (must be power of 2).
    buffer_size: usize,

    /// Bitmask for fast modulo: `sequence & index_mask` == `sequence % buffer_size`.
    index_mask: usize,

    /// Per-slot availability flags.
    ///
    /// Each entry stores a "lap number" (how many times the ring buffer
    /// has wrapped past this slot). This is more subtle than a boolean —
    /// a boolean only stores "written" or "not written." After the ring
    /// buffer wraps, *every* slot has been written — so a boolean can't
    /// distinguish between "written in this lap" and "written in the
    /// previous lap." The lap number solves this: if `available_buffer[3] == 2`,
    /// slot 3 was published on lap 2. A consumer expecting lap 3 sees
    /// `2 != 3` and knows the slot isn't ready yet.
    available_buffer: Vec<AtomicI32>,
}
```

**Why `-1` initialization works:** The first sequence published is 0, which maps to lap `0 / buffer_size = 0`. Since all slots are initialized to `-1`, no slot appears published until explicitly set. Sequences always start at 0 (cursor begins at -1, and `claim()` returns `[current+1, next]`), so lap 0 is always the first valid lap.

```
Lap-based availability:

Buffer size = 4, sequence = 10
  index = 10 & 3 = 2
  flag  = 10 / 4 = 2

available_buffer[2] = 2  → sequence 10 is published
available_buffer[2] = 1  → sequence 10 is NOT published (still lap 1's data)
available_buffer[2] = -1 → slot 2 has never been published
```

### Step 2: Constructor

```rust
impl MultiProducerSequencer {
    pub fn new(buffer_size: usize, gating_sequences: Vec<Arc<AtomicI64>>) -> Self {
        assert!(buffer_size.is_power_of_two(), "buffer_size must be power of 2");
        assert!(buffer_size >= 2, "buffer_size must be at least 2");

        // Initialize all flags to -1 (no slot has been published)
        let available_buffer = (0..buffer_size)
            .map(|_| AtomicI32::new(-1))
            .collect();

        Self {
            cursor: Arc::new(AtomicI64::new(-1)),
            gating_sequences,
            buffer_size,
            index_mask: buffer_size - 1,
            available_buffer,
        }
    }
}
```

**Why initialize to -1?** The first sequence published is 0, which maps to flag `0 / buffer_size = 0`. If we initialized to 0, every slot would *appear* to be already published. Initializing to -1 ensures no slot is marked available until explicitly published.

### Step 3: Availability Flag Calculation

```rust
/// Calculate the availability flag for a given sequence.
///
/// This is the "lap number" — how many complete wraps have occurred
/// before this sequence was assigned to this slot.
///
/// # Examples
/// buffer_size=8:
///   seq 0  → flag 0  (lap 0)
///   seq 7  → flag 0  (lap 0)
///   seq 8  → flag 1  (lap 1)
///   seq 15 → flag 1  (lap 1)
///   seq 16 → flag 2  (lap 2)
fn calculate_availability_flag(sequence: i64, buffer_size: usize) -> i32 {
    (sequence / buffer_size as i64) as i32
}
```

### Step 4: Capacity Check

```rust
impl MultiProducerSequencer {
    /// Returns the minimum sequence among all gating (consumer) sequences.
    fn get_minimum_gating_sequence(&self) -> i64 {
        let mut minimum = i64::MAX;
        for seq in &self.gating_sequences {
            minimum = minimum.min(seq.load(Ordering::Acquire));
        }
        minimum
    }

    /// Check whether `required_capacity` slots are available.
    ///
    /// A slot is available if the slowest consumer has advanced past
    /// the wrap point:
    ///   wrap_point = cursor + required_capacity - buffer_size
    ///
    /// If wrap_point > min_gating_sequence, the buffer is full.
    fn has_available_capacity(&self, required_capacity: usize) -> bool {
        let current = self.cursor.load(Ordering::Relaxed);
        let wrap_point = current + required_capacity as i64 - self.buffer_size as i64;

        let min_gating = self.get_minimum_gating_sequence();
        wrap_point <= min_gating
    }
}
```

**Why `Ordering::Relaxed` for the cursor load?** The cursor value is only used to *estimate* the wrap point. If it's stale (another producer advanced it since we loaded), we'll just re-check on the next CAS attempt. The CAS itself uses `AcqRel`, which provides the actual synchronization.

### Step 5: CAS-Based Claiming

```rust
impl Sequencer for MultiProducerSequencer {
    fn claim(&self, count: usize) -> SequenceClaim {
        loop {
            let current = self.cursor.load(Ordering::Acquire);
            let next = current + count as i64;

            // Check capacity before attempting CAS.
            // This avoids wasting a CAS on a full buffer.
            if !self.has_available_capacity(count) {
                // Backpressure: buffer full, wait for consumers.
                // park_timeout matches Java's LockSupport.parkNanos(1L) —
                // yields CPU instead of busy-spinning.
                std::thread::park_timeout(std::time::Duration::from_nanos(1));
                continue;
            }

            // Attempt to advance the cursor from `current` to `next`.
            //
            // CAS semantics:
            // - If cursor == current: set cursor = next, return Ok
            // - If cursor != current: another producer won, return Err
            //
            // Memory ordering:
            // - Success (AcqRel): Acquire reads from prior publishes,
            //   Release makes our cursor advance visible
            // - Failure (Acquire): Read the updated cursor value
            match self.cursor.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // We claimed sequences [current+1, next].
                    // The SequenceClaim will call publish_internal on drop.
                    return SequenceClaim::new(current + 1, next, /* sequencer ref */);
                }
                Err(_) => {
                    // Another producer advanced the cursor.
                    // Retry with updated cursor value.
                    std::hint::spin_loop();
                }
            }
        }
    }

    fn try_claim(&self, count: usize) -> Result<SequenceClaim, InsufficientCapacity> {
        let current = self.cursor.load(Ordering::Acquire);
        let next = current + count as i64;

        if !self.has_available_capacity(count) {
            return Err(InsufficientCapacity);
        }

        match self.cursor.compare_exchange(
            current,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(SequenceClaim::new(current + 1, next, /* sequencer ref */)),
            Err(_) => Err(InsufficientCapacity), // CAS failed → report as insufficient
        }
    }
}
```

**Why `compare_exchange_weak` in `claim` but `compare_exchange` in `try_claim`?**

- `compare_exchange_weak` can spuriously fail (return `Err` even when the value matches). This is faster on ARM because it avoids an unnecessary exclusive-access fence. In `claim`, we're already in a retry loop, so spurious failure is free.
- `compare_exchange` never spuriously fails. In `try_claim`, we only attempt once. A spurious failure would incorrectly report `InsufficientCapacity` when capacity exists. So we use the strong variant.

**CAS vs. fetch_add — the contention tradeoff:**

| Operation | Uncontended | 4 Producers | 8 Producers |
|-----------|-------------|-------------|-------------|
| `fetch_add` | ~5ns | ~15ns | ~30ns |
| `compare_exchange` | ~8ns | ~30-50ns | ~100-200ns |
| CAS retry rate | 0% | ~30% | ~60% |

CAS is slower because it can fail and retry. But it's the only way to check capacity *before* advancing the cursor.


### Step 6: Publishing (Per-Slot Availability)

```rust
impl MultiProducerSequencer {
    /// Mark a single slot as available (published).
    ///
    /// Stores the lap number for this sequence into the availability buffer.
    /// Consumers check this flag to determine if a slot has been published.
    fn set_available(&self, sequence: i64) {
        let index = (sequence as usize) & self.index_mask;
        let flag = calculate_availability_flag(sequence, self.buffer_size);
        self.available_buffer[index].store(flag, Ordering::Release);
    }

    /// Check if a specific sequence has been published.
    fn is_available(&self, sequence: i64) -> bool {
        let index = (sequence as usize) & self.index_mask;
        let flag = calculate_availability_flag(sequence, self.buffer_size);
        self.available_buffer[index].load(Ordering::Acquire) == flag
    }

    /// Called when a SequenceClaim is dropped — publish all claimed sequences.
    fn publish_internal(&self, lo: i64, hi: i64) {
        for seq in lo..=hi {
            self.set_available(seq);
        }
    }

    /// Find the highest contiguous published sequence starting from `lo`.
    ///
    /// Scans the availability buffer from `lo` up to `available` (the cursor).
    /// Returns the last sequence where all prior sequences are also published.
    ///
    /// This is the key method that prevents consumers from reading
    /// "holes" — sequences that have been claimed but not yet published.
    pub fn get_highest_published_sequence(&self, lo: i64, available: i64) -> i64 {
        for seq in lo..=available {
            if !self.is_available(seq) {
                return seq - 1;
            }
        }
        available
    }
}
```

**Why `Ordering::Release` in `set_available`?** The `Release` store ensures that all writes to the event data (inside the publish closure) are visible before the availability flag is set. A consumer that sees `is_available(seq) == true` (via `Acquire` load) is guaranteed to see the event data.

**Why per-slot instead of a single "published up to" cursor?** Because producers publish *out of order*. Producer A claims seq 10, Producer B claims seq 11. If B finishes first and writes a single "published up to 11" cursor, consumers would try to read seq 10 — which isn't ready yet. Per-slot flags avoid this entirely.

---

## Out-of-Order Publishing: Step-by-Step Trace

This is the most critical race condition to understand. Let's trace it:

```
Buffer size: 8, initial cursor: -1
Two producers, one consumer.

── Step 1: Both producers claim simultaneously ────────
Producer A: load cursor=-1, CAS(-1→0) → SUCCESS
            → claims seq 0
Producer B: load cursor=0, CAS(0→1) → SUCCESS
            → claims seq 1

Cursor is now 1. But neither event is published yet!

── Step 2: Producer B publishes FIRST (out of order) ──
Producer B: writes event at seq 1
            set_available(1): available_buffer[1] = 0  (lap 0)

Available buffer: [-1, 0, -1, -1, -1, -1, -1, -1]
                   ↑    ↑
                   seq0  seq1
                   NOT   published
                   ready

── Step 3: Consumer tries to read ──────────────────────
Consumer: barrier.wait_for(0) → cursor is 1
          get_highest_published_sequence(0, 1):
            is_available(0)? available_buffer[0]=-1, flag=0 → -1 != 0 → NO
            → returns -1 (nothing available!)

Consumer: blocks — cannot read seq 0 or seq 1.
          Even though seq 1 is published, seq 0 creates a "hole."

── Step 4: Producer A finishes ─────────────────────────
Producer A: writes event at seq 0
            set_available(0): available_buffer[0] = 0  (lap 0)

Available buffer: [0, 0, -1, -1, -1, -1, -1, -1]
                   ↑   ↑
                   seq0 seq1
                   OK   OK

── Step 5: Consumer reads both events ──────────────────
Consumer: get_highest_published_sequence(0, 1):
            is_available(0)? available_buffer[0]=0, flag=0 → 0 == 0 → YES
            is_available(1)? available_buffer[1]=0, flag=0 → 0 == 0 → YES
            → returns 1

Consumer: reads seq 0 and seq 1 (both fully written). ✓
```

**The key invariant:** `get_highest_published_sequence` returns a contiguous range. Even if sequences 5, 7, and 8 are published, the consumer only reads up to 4 if sequence 5 isn't ready. This guarantees consumers never see partially-written data.

---

## Contention Analysis

### What Happens Under High Contention

When N producers compete for one `AtomicI64`:

```
N=2:  A claims, B retries once    → ~10% CAS failures
N=4:  3 retries per success       → ~30% CAS failures
N=8:  5-7 retries per success     → ~60% CAS failures
N=16: 10+ retries per success     → ~80% CAS failures
```

Each failed CAS wastes ~10-20ns (load + compare + fail + hint::spin_loop). At 80% failure rate with 16 producers, the average claim takes 5-10× longer than uncontended.

### Fairness Warning

CAS-based claiming is **not fair**. Under high contention, a slow producer (one that gets preempted or stalls between the load and CAS) can be starved indefinitely because faster producers keep winning the race. The LMAX Java Disruptor accepts this tradeoff — in practice, with pinned threads and dedicated cores, starvation is rare.

If fairness is critical (e.g., multiple producers with hard latency SLAs), consider:
- **Partitioning producers** to separate single-producer rings (sharding, see below)
- **Ticket-based claiming** using `fetch_add` with a separate capacity semaphore
- **Reducing producer count** to ≤4, where CAS failure rates stay manageable (~30%)

### Benchmark Data

```
Multi-Producer Sequencer Throughput (theoretical estimates, 64-byte events):

Producers | Throughput  | p50 Latency | p99 Latency | CAS Failures/sec
----------|-------------|-------------|-------------|------------------
1         | 25M ops/sec | 40ns        | 80ns        | 0
2         | 22M ops/sec | 50ns        | 120ns       | 2.5M
4         | 18M ops/sec | 80ns        | 300ns       | 7.5M
8         | 12M ops/sec | 150ns       | 800ns       | 18M
16        | 8M ops/sec  | 300ns       | 2μs         | 50M
```

*These are theoretical estimates based on typical x86-64 CAS contention characteristics. Actual numbers depend on CPU, cache topology, and workload.*

---

## Contention Mitigation

### Strategy 1: Batch Claiming

The single most effective optimization. Instead of N separate CAS operations, claim N slots with one CAS:

```rust
// ❌ 1000 CAS operations — O(N) contention
for msg in messages.iter() {
    ring_buffer.publish_with(&*sequencer, |event, _| {
        *event = msg.clone();
    });
}

// ✅ 1 CAS operation — O(1) contention
ring_buffer.publish_batch(&*sequencer, messages.iter(), |event, _seq, msg| {
    *event = msg.clone();
});
```

**Cost reduction:**

```
1000 events with 4 producers:

Individual claims:
  1000 × CAS @ ~50ns avg (30% retry rate) = ~50μs per producer
  Total: 200μs wall clock (interleaved)

Batch claim:
  1 × CAS @ ~50ns + 1000 × write @ ~5ns = ~5.05μs per producer
  Total: ~20μs wall clock

Speedup: ~10× for claiming phase
```

### Strategy 2: Thread-Local Batching

For producers that generate events one at a time, buffer locally and flush periodically:

```rust
thread_local! {
    static BATCH_BUFFER: RefCell<Vec<Event>> = RefCell::new(Vec::with_capacity(128));
}

pub fn publish_event(
    ring_buffer: &RingBuffer<Event>,
    sequencer: &dyn Sequencer,
    event: Event,
) {
    BATCH_BUFFER.with(|buf| {
        let mut batch = buf.borrow_mut();
        batch.push(event);

        if batch.len() >= 128 {
            // Flush: 1 CAS for 128 events
            ring_buffer.publish_batch(sequencer, batch.drain(..), |slot, _seq, item| {
                *slot = item;
            });
        }
    });
}

/// Must be called before thread exits to flush remaining events!
pub fn flush(ring_buffer: &RingBuffer<Event>, sequencer: &dyn Sequencer) {
    BATCH_BUFFER.with(|buf| {
        let mut batch = buf.borrow_mut();
        if !batch.is_empty() {
            ring_buffer.publish_batch(sequencer, batch.drain(..), |slot, _seq, item| {
                *slot = item;
            });
        }
    });
}
```

**Tradeoff:** Adds latency (up to 128 events of delay before flush) in exchange for dramatically reduced contention. Suitable for logging, metrics, and telemetry — not for latency-critical order entry.

### Strategy 3: Sharded Ring Buffers

For extreme contention (>8 producers), eliminate contention entirely by giving each producer its own ring buffer:

```rust
/// N ring buffers, one per producer. Zero contention on the write path.
/// Consumers must merge events from all shards.
pub struct ShardedDisruptor<T> {
    shards: Vec<(Arc<RingBuffer<T>>, Arc<dyn Sequencer>)>,
    shard_mask: usize,
}

impl<T: Default> ShardedDisruptor<T> {
    pub fn new(num_shards: usize, buffer_size: usize) -> Self {
        assert!(num_shards.is_power_of_two());

        let shards = (0..num_shards)
            .map(|_| {
                let rb = Arc::new(RingBuffer::<T>::new(buffer_size));
                let seq: Arc<dyn Sequencer> = Arc::new(
                    SingleProducerSequencer::new(buffer_size, vec![])
                );
                (rb, seq)
            })
            .collect();

        Self {
            shards,
            shard_mask: num_shards - 1,
        }
    }

    /// Publish to the shard for this producer.
    /// Each producer MUST use a unique, stable producer_id.
    pub fn publish_with<F>(&self, producer_id: usize, f: F)
    where
        F: FnOnce(&mut T, i64),
    {
        let shard_idx = producer_id & self.shard_mask;
        let (rb, seq) = &self.shards[shard_idx];
        rb.publish_with(&**seq, f);
    }
}
```

**Tradeoff analysis:**

| Aspect | Multi-Producer | Sharded |
|--------|---------------|---------|
| Write contention | O(N) producers on one cursor | Zero (each has own cursor) |
| Write latency | ~50-300ns (depends on N) | ~10ns (single-producer speed) |
| Consumer complexity | Simple (one ring buffer) | Must merge N shards |
| Event ordering | Total order (single cursor) | Per-shard order only |
| Memory | 1 × buffer_size | N × buffer_size |

**When to shard:**
- ≤4 producers → `MultiProducerSequencer` (contention is manageable)
- 5-8 producers → `MultiProducerSequencer` with batch claiming
- >8 producers → Sharding (contention dominates)

---

## SingleProducer vs. MultiProducer: Complete Comparison

| Aspect | `SingleProducerSequencer` | `MultiProducerSequencer` |
|--------|--------------------------|--------------------------|
| Claiming | `Cell::set` (no atomic) | CAS loop |
| Publishing | Single cursor store | Per-slot `available_buffer` write |
| Consumer visibility | Cursor == published | `get_highest_published_sequence` scan |
| Contention | None (single writer) | O(N) on cursor |
| Latency (p50) | ~10ns | ~40-300ns (depends on N) |
| Memory overhead | 0 | buffer_size × 4 bytes (availability flags) |
| Thread safety | `!Sync` (single thread only) | `Sync` (any thread) |
| Best for | Single-writer, lowest latency | Multiple concurrent writers |

---

## Testing

### Test 1: Sequential Claims Produce Unique Sequences

```rust
#[test]
fn sequential_claims_are_unique() {
    let consumer_seq = Arc::new(AtomicI64::new(-1));
    let sequencer = MultiProducerSequencer::new(8, vec![Arc::clone(&consumer_seq)]);

    let claim1 = sequencer.claim(1);
    assert_eq!(claim1.start(), 0);
    drop(claim1); // publish

    let claim2 = sequencer.claim(1);
    assert_eq!(claim2.start(), 1);
    drop(claim2);

    let claim3 = sequencer.claim(3);
    assert_eq!(claim3.start(), 2);
    assert_eq!(claim3.end(), 4);
}
```

### Test 2: Availability Flags Track Publication

```rust
#[test]
fn availability_flags_track_publication() {
    let consumer_seq = Arc::new(AtomicI64::new(-1));
    let sequencer = MultiProducerSequencer::new(8, vec![Arc::clone(&consumer_seq)]);

    // Before publish, nothing is available
    assert!(!sequencer.is_available(0));
    assert!(!sequencer.is_available(1));

    // Claim and publish seq 0
    let claim = sequencer.claim(1);
    assert!(!sequencer.is_available(0)); // Not available until drop
    drop(claim);
    assert!(sequencer.is_available(0));  // Now available

    // Seq 1 still not available
    assert!(!sequencer.is_available(1));
}
```

### Test 3: get_highest_published_sequence Detects Gaps

```rust
#[test]
fn highest_published_detects_gaps() {
    let consumer_seq = Arc::new(AtomicI64::new(-1));
    let sequencer = MultiProducerSequencer::new(8, vec![Arc::clone(&consumer_seq)]);

    // Claim sequences 0, 1, 2
    let claim0 = sequencer.claim(1); // seq 0
    let claim1 = sequencer.claim(1); // seq 1
    let claim2 = sequencer.claim(1); // seq 2

    // Publish out of order: 0, then 2 (skip 1)
    drop(claim0);
    drop(claim2);

    // Highest contiguous from 0: only 0 (gap at 1)
    assert_eq!(sequencer.get_highest_published_sequence(0, 2), 0);

    // Now publish 1
    drop(claim1);

    // All three are contiguous
    assert_eq!(sequencer.get_highest_published_sequence(0, 2), 2);
}
```

### Test 4: Concurrent Multi-Producer Correctness

```rust
#[test]
fn concurrent_multi_producer_no_data_loss() {
    let buffer_size = 1024;
    let consumer_seq = Arc::new(AtomicI64::new(-1));
    let ring_buffer = Arc::new(RingBuffer::<u64>::new(buffer_size));
    let sequencer = Arc::new(
        MultiProducerSequencer::new(buffer_size, vec![Arc::clone(&consumer_seq)])
    );

    let num_producers = 4;
    let events_per_producer = 1000;

    let handles: Vec<_> = (0..num_producers).map(|pid| {
        let rb = Arc::clone(&ring_buffer);
        let seq = Arc::clone(&sequencer);
        std::thread::spawn(move || {
            for i in 0..events_per_producer {
                rb.publish_with(&*seq, |event, _| {
                    *event = pid * 10000 + i;
                });
            }
        })
    }).collect();

    for h in handles {
        h.join().unwrap();
    }

    let total = num_producers * events_per_producer;

    // Verify all sequences are published
    for seq in 0..total as i64 {
        assert!(sequencer.is_available(seq),
            "Sequence {} not available", seq);
    }

    // Verify no duplicate values
    let mut values: Vec<u64> = (0..total as i64)
        .map(|i| *ring_buffer.get(i))
        .collect();
    values.sort();
    values.dedup();
    assert_eq!(values.len(), total as usize);
}
```

### Test 5: Capacity Check Prevents Overwrite

```rust
#[test]
fn try_claim_fails_when_buffer_full() {
    let consumer_seq = Arc::new(AtomicI64::new(-1));
    let sequencer = MultiProducerSequencer::new(4, vec![Arc::clone(&consumer_seq)]);

    // Fill the buffer (4 slots)
    for _ in 0..4 {
        let claim = sequencer.claim(1);
        drop(claim); // publish
    }

    // Buffer full — try_claim should fail
    assert!(sequencer.try_claim(1).is_err());

    // Advance consumer — frees slots
    consumer_seq.store(1, Ordering::Release);

    // Now should succeed
    assert!(sequencer.try_claim(1).is_ok());
}
```

### Test 6: Lap-Based Availability After Wrap

```rust
#[test]
fn availability_flags_handle_wrap_correctly() {
    let consumer_seq = Arc::new(AtomicI64::new(-1));
    let sequencer = MultiProducerSequencer::new(4, vec![Arc::clone(&consumer_seq)]);

    // Fill and consume one full lap (sequences 0-3)
    for _ in 0..4 {
        let claim = sequencer.claim(1);
        drop(claim);
    }
    consumer_seq.store(3, Ordering::Release);

    // Publish sequence 4 (wraps to slot 0, lap 1)
    let claim = sequencer.claim(1);
    assert_eq!(claim.start(), 4);
    drop(claim);

    // Slot 0 now has lap 1 flag
    assert!(sequencer.is_available(4));

    // Old sequence 0 at this slot is no longer "available" as seq 0
    // (The flag is now 1, but seq 0 expects flag 0)
    // This is correct — seq 0 has been overwritten
}
```

---

## Key Takeaways

1. **CAS is necessary for capacity-checked claiming.** `fetch_add` is faster but can't prevent buffer overwrites. CAS lets us check capacity before advancing the cursor.

2. **Per-slot availability flags prevent out-of-order visibility.** The `available_buffer` uses lap numbers to distinguish between "never written," "written on this lap," and "written on a previous lap."

3. **`get_highest_published_sequence` enforces contiguous reads.** Consumers never see holes in the sequence space, even when producers publish out of order.

4. **Contention scales linearly with producer count.** At 4 producers, ~30% of CAS attempts fail. At 16, ~80% fail. Batch claiming is the primary mitigation.

5. **Three contention mitigation strategies:** Batch claiming (10× improvement), thread-local batching (amortized CAS), and sharding (zero contention at the cost of ordering).

6. **`compare_exchange_weak` vs. `compare_exchange`:** Use `_weak` in retry loops (faster on ARM), `_strong` for single-attempt operations like `try_claim`.

7. **When in doubt, use SingleProducerSequencer.** It's 10× faster. Only use MultiProducer when you genuinely have multiple threads that can't be consolidated.

---

## Next Up: EventPoller

In **Part 10**, we'll build a pull-based consumer:

- **`EventPoller`** — Poll for events instead of blocking
- **`PollState`** — Distinguish between `Processing`, `Gating`, and `Idle`
- **Integration with external event loops** — `epoll`, `kqueue`, Tokio
- **Multi-buffer polling** — Read from multiple ring buffers in one thread

---

## References

### Rust Documentation

- [compare_exchange_weak](https://doc.rust-lang.org/std/sync/atomic/struct.AtomicI64.html#method.compare_exchange_weak) — CAS with spurious failure
- [compare_exchange](https://doc.rust-lang.org/std/sync/atomic/struct.AtomicI64.html#method.compare_exchange) — CAS without spurious failure
- [hint::spin_loop](https://doc.rust-lang.org/std/hint/fn.spin_loop.html) — CPU hint for spin-wait loops
- [Ordering::AcqRel](https://doc.rust-lang.org/std/sync/atomic/enum.Ordering.html#variant.AcqRel) — Combined acquire-release ordering

### Java Disruptor Reference

- [MultiProducerSequencer.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/MultiProducerSequencer.java)
- [Util.getMinimumSequence()](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/util/Util.java)

### Architecture

- [LMAX Architecture](https://martinfowler.com/articles/lmax.html) — Martin Fowler's overview (discusses multi-producer tradeoffs)
- [Mechanical Sympathy: CAS](https://mechanical-sympathy.blogspot.com/) — Hardware-level analysis of CAS performance

---

**Next:** [Part 10 — EventPoller: Pull-Based Consumption for Control →](post10.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*