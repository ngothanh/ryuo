# Building a Disruptor in Rust: Ryuo — Part 5: Sequence Barriers — Coordinating Consumer Dependencies

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 5 of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 4 (wait strategies), Part 3B (sequencer implementation)

---

## Recap

In Part 4, we built six wait strategies — from `BusySpinWaitStrategy` (~50ns, 100% CPU) to `BlockingWaitStrategy` (~10μs, 0% CPU) — and introduced a simplified `SequenceBarrier` to delegate waiting.

But we left three critical questions unanswered:

1. **How do consumers in a pipeline depend on each other?** If Handler B must see Handler A's output, how does B know what A has processed?
2. **How does the barrier handle multi-producer?** With `MultiProducerSequencer`, the cursor can advance past unpublished slots. How does the barrier prevent consumers from reading holes?
3. **How do we wire up complex topologies?** Pipeline, diamond, multicast — what changes?

This post answers all three.

---

## The Coordination Problem

### Single Consumer: Simple

With one producer and one consumer, the consumer just watches the cursor:

```
Producer ──> cursor: 7 ──> Consumer (reads up to 7)
```

The cursor tells the consumer exactly how far it can read. Done.

### Multi-Stage Pipeline: Not Simple

Now add a second consumer that depends on the first:

```
Producer ──> cursor: 7 ──> Consumer A (processed up to 5) ──> Consumer B
```

**Consumer B cannot read sequence 6 or 7.** Even though the producer has published them, Consumer A hasn't processed them yet. If B reads ahead of A:

- **Data hazard:** B might process events that A hasn't enriched/validated yet.
- **Ordering violation:** B sees events out of the order A would have emitted them.
- **Semantic corruption:** If A writes derived data into the event (e.g., a computed field), B reads stale data.

**The rule:** A consumer can only read up to the **minimum** of the cursor and all upstream consumer sequences.

### Multi-Producer: Even Less Simple

With `MultiProducerSequencer`, the cursor advances when a slot is *claimed*, not when it's *published*:

```
Time →
Producer 1: claims seq 5, publishes immediately     ✓ available
Producer 2: claims seq 6, still writing...           ✗ NOT available
Producer 3: claims seq 7, publishes immediately     ✓ available

Cursor: 7 (advanced by claim, not by publish)
```

If the consumer reads up to cursor (7), it reads unpublished data at sequence 6. **Undefined behavior.**

The barrier must check each slot's availability via the `available_buffer` from Part 3B.

---

## Dependency Graphs

The Disruptor supports three fundamental topologies. Every real-world pipeline is a composition of these three.

### Topology 1: Pipeline (Sequential)

```
Producer ──> [Ring Buffer] ──> A ──> B ──> C
```

Each consumer waits for the one before it:
- **A** depends on: `cursor` (the producer)
- **B** depends on: `A.sequence`
- **C** depends on: `B.sequence`

**Use case:** Enrichment pipeline. A parses raw bytes → B validates → C writes to database.

### Topology 2: Multicast (Parallel Fan-Out)

```
                         ┌──> A
Producer ──> [Ring Buffer] ──> B
                         └──> C
```

All consumers depend only on the cursor:
- **A** depends on: `cursor`
- **B** depends on: `cursor`
- **C** depends on: `cursor`

**Use case:** One event, multiple independent handlers. A logs → B updates metrics → C sends alerts.

### Topology 3: Diamond (Fan-Out + Fan-In)

```
                         ┌──> A ──┐
Producer ──> [Ring Buffer] ──> B ──┤──> D
                         └──> C ──┘
```

## Implementation: The Full SequenceBarrier

In Part 4, we defined a simplified `SequenceBarrier`. Now we expand it to handle:

1. **Dependent sequences** — Consumer-to-consumer dependencies
2. **Multi-producer awareness** — Checking `available_buffer` for contiguous published sequences
3. **Alert propagation** — Shutdown signaling across the pipeline
4. **Dynamic gating** — Adding gating sequences after construction

### The Sequencer Trait Extension

First, we need a way for the barrier to ask the sequencer whether a given sequence has been published. For `SingleProducerSequencer`, this is trivial (cursor = published). For `MultiProducerSequencer`, it requires checking the `available_buffer`.

We add two methods to the `Sequencer` trait from Part 3B:

```rust
pub trait Sequencer: Send + Sync {
    // ... existing methods from Part 3B ...

    /// Check if a specific sequence has been published.
    ///
    /// - SingleProducer: always true if sequence <= cursor (no out-of-order)
    /// - MultiProducer: checks available_buffer[index] == expected_flag
    fn is_available(&self, sequence: i64) -> bool;

    /// Find the highest *contiguously* published sequence in [lo..=hi].
    ///
    /// Returns the highest sequence `s` where all sequences in [lo..=s]
    /// have been published. If lo itself is not published, returns lo - 1.
    ///
    /// This is the key method that prevents consumers from reading holes
    /// in multi-producer mode.
    fn get_highest_published_sequence(&self, lo: i64, hi: i64) -> i64;

    /// Dynamically add a gating sequence.
    ///
    /// Used when wiring topologies after construction. The sequencer
    /// must check all gating sequences before overwriting ring buffer slots.
    fn add_gating_sequence(&self, sequence: Arc<AtomicI64>);
}
```

**Why two methods?** `is_available` checks a single slot — useful for diagnostic tools and health checks. `get_highest_published_sequence` scans a range — used by the barrier on every `wait_for` call to find contiguous data.

**Why `add_gating_sequence`?** In Part 3B, gating sequences were passed to the constructor. But when wiring topologies (see "Wiring Up Topologies" below), consumers are created *after* the sequencer. We need a way to register them dynamically. The implementation uses interior mutability (e.g., `Mutex<Vec<Arc<AtomicI64>>>`) — acceptable because `add_gating_sequence` is called once during setup, never on the hot path.

### SingleProducerSequencer: Trivial

For single-producer, publishing is always in-order. If the cursor says "published up to 7", then sequences 0–7 are all available:

```rust
impl Sequencer for SingleProducerSequencer {
    // ... existing methods ...

    fn is_available(&self, sequence: i64) -> bool {
        sequence <= self.cursor.load(Ordering::Acquire)
    }

    fn get_highest_published_sequence(&self, _lo: i64, hi: i64) -> i64 {
        // Single producer publishes in-order.
        // If cursor >= hi, all of [lo..=hi] are published.
        // If cursor < hi, cursor is the highest published.
        hi.min(self.cursor.load(Ordering::Acquire))
    }
}
```

**Why is this O(1)?** Single-producer guarantees monotonic publishing. There are no holes. The cursor *is* the highest published sequence.

### MultiProducerSequencer: Scan the Available Buffer

For multi-producer, we must scan slot-by-slot because producers can publish out of order:

```rust
impl Sequencer for MultiProducerSequencer {
    // ... existing methods ...

    fn is_available(&self, sequence: i64) -> bool {
        let index = (sequence as usize) & self.index_mask;
        let flag = (sequence / self.buffer_size as i64) as i32;
        self.available_buffer[index].load(Ordering::Acquire) == flag
    }

    fn get_highest_published_sequence(&self, lo: i64, hi: i64) -> i64 {
        for sequence in lo..=hi {
            if !self.is_available(sequence) {
                return sequence - 1;
            }
        }
        hi
    }
}
```

**Why Acquire?** Each `available_buffer[index].load(Acquire)` pairs with the producer's `available_buffer[index].store(flag, Release)` from `SequenceClaim::drop()`. This ensures that when the consumer sees "slot available", it also sees the event data the producer wrote before marking the slot.

**Why scan linearly?** We need *contiguous* availability. If sequences 5 and 7 are published but 6 is not, the consumer can only read up to 5. A bitmap or tree structure could accelerate this for very large gaps, but in practice gaps are rare and small (a few slots during a context switch), so the linear scan is optimal.

**Cost analysis:**

| Scenario | Scan Length | Cost |
|----------|-----------|------|
| No gap (common case) | hi - lo + 1 = batch size | ~5ns per slot (one Acquire load per cache line) |
| Small gap (context switch) | 1-3 slots | ~5-15ns |
| Large gap (thread preempted) | Rare, bounded by buffer_size | Bounded by ring buffer size |

In practice, `get_highest_published_sequence` scans 1–10 slots on average. With 64-byte cache lines holding 16 `AtomicI32` values, the first ~16 checks are in the same cache line (~1ns each after the first load).

---

### The Full SequenceBarrier

Now we expand the simplified `SequenceBarrier` from Part 4. The key change: the barrier now holds a reference to the `Sequencer` so it can call `get_highest_published_sequence` for multi-producer support.

```rust
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

/// Full sequence barrier with multi-producer support and dependency tracking.
///
/// Changes from the simplified version in Part 4:
/// - Added `sequencer: Arc<dyn Sequencer>` for multi-producer gap detection
/// - `wait_for` now calls `get_highest_published_sequence` to find
///   contiguous published sequences, not just the raw cursor value
pub struct SequenceBarrier {
    /// The producer's cursor — how far the sequencer has *claimed*.
    /// For single-producer, claimed = published.
    /// For multi-producer, claimed ≥ published (there may be gaps).
    cursor: Arc<AtomicI64>,

    /// The sequencer — used to check slot availability in multi-producer mode.
    sequencer: Arc<dyn Sequencer>,

    /// Shutdown signal.
    alerted: AtomicBool,

    /// Pluggable wait strategy from Part 4.
    wait_strategy: Arc<dyn WaitStrategy>,

    /// Sequences of upstream consumers this barrier depends on.
    /// Empty for first-stage consumers (depend only on the producer).
    /// Non-empty for pipeline/diamond consumers.
    dependent_sequences: Vec<Arc<AtomicI64>>,
}

impl SequenceBarrier {
    pub fn new(
        cursor: Arc<AtomicI64>,
        sequencer: Arc<dyn Sequencer>,
        wait_strategy: Arc<dyn WaitStrategy>,
        dependent_sequences: Vec<Arc<AtomicI64>>,
    ) -> Self {
        Self {
            cursor,
            sequencer,
            alerted: AtomicBool::new(false),
            wait_strategy,
            dependent_sequences,
        }
    }

    /// Check if the barrier has been alerted (shutdown signal).
    pub fn check_alert(&self) -> Result<(), BarrierError> {
        if self.alerted.load(Ordering::Acquire) {
            Err(BarrierError::Alerted)
        } else {
            Ok(())
        }
    }

    /// Signal the barrier to stop waiting (used for graceful shutdown).
    pub fn alert(&self) {
        self.alerted.store(true, Ordering::Release);
        self.wait_strategy.signal_all_when_blocking();
    }

    /// Clear the alert flag (used for restarting after a transient shutdown).
    pub fn clear_alert(&self) {
        self.alerted.store(false, Ordering::Release);
    }

    /// Wait for a sequence to become available, respecting both
    /// producer publishing and upstream consumer dependencies.
    ///
    /// Returns the highest *contiguously published* sequence that is
    /// safe to read — which may be higher than `sequence` (enabling
    /// batch processing).
    pub fn wait_for(&self, sequence: i64) -> Result<i64, BarrierError> {
        self.check_alert()?;

        // Step 1: Wait until the cursor and all dependencies have
        //         advanced past `sequence`. This is where we block/spin/yield.
        let available_sequence = self.wait_strategy.wait_for(
            sequence,
            &self.cursor,
            &self.dependent_sequences,
            self,
        )?;

        // Step 2 (multi-producer only): The cursor may have advanced
        //         past unpublished slots. Find the highest *contiguously*
        //         published sequence starting from `sequence`.
        //
        // For single-producer, get_highest_published_sequence is O(1)
        // and always returns available_sequence (no gaps possible).
        //
        // Defensive: all standard wait strategies return >= sequence,
        // but a custom strategy (e.g., one that returns early on timeout
        // without setting Err) could return less. Guard against that.
        if available_sequence >= sequence {
            return Ok(self.sequencer.get_highest_published_sequence(
                sequence,
                available_sequence,
            ));
        }

        Ok(available_sequence)
    }

    /// Get the cursor reference (useful for constructing downstream barriers).
    pub fn cursor(&self) -> &Arc<AtomicI64> {
        &self.cursor
    }
}
```

**Why `Arc<dyn WaitStrategy>` instead of `Box<dyn WaitStrategy>`?**

In Part 4, we used `Box<dyn WaitStrategy>` because each barrier owned its strategy exclusively. But in a real pipeline, all barriers in the same Disruptor instance share the *same* wait strategy — the producer calls `signal_all_when_blocking()` on it, and all consumers use it for waiting. `Arc` enables this shared ownership without copying.

> **Note:** For `Arc<dyn WaitStrategy>` to compile, the `WaitStrategy` trait must be `Send + Sync`. We retroactively add these supertraits: `pub trait WaitStrategy: Send + Sync { ... }`. All implementations from Part 4 (`BusySpinWaitStrategy`, `BlockingWaitStrategy`, etc.) already satisfy these bounds — `BusySpinWaitStrategy` is a unit struct, and `BlockingWaitStrategy` uses `Mutex`/`Condvar` which are both `Send + Sync`.

```
                                   Arc<dyn WaitStrategy>
                                        │
                    ┌───────────────────┼───────────────────┐
                    ▼                   ▼                   ▼
              SequenceBarrier A   SequenceBarrier B   Producer
              (consumer A)       (consumer B)        (calls signal)
```

**Virtual dispatch cost:** ~2-5ns per `wait_for` call (indirect call through vtable). This is negligible compared to the actual waiting time (~50ns for BusySpin, ~10μs for Blocking).

---

## How wait_for Works: Step by Step

Let's trace through `wait_for` for each producer type to see why Step 2 is critical.

### Single Producer Trace

```
Setup: SingleProducerSequencer, buffer_size=8
       cursor = 5, consumer depends on cursor only

Consumer calls: barrier.wait_for(3)

Step 1: wait_strategy.wait_for(3, cursor=5, deps=[], barrier)
        → get_minimum_sequence(cursor=5, deps=[]) → 5
        → 5 >= 3 → return Ok(5)

Step 2: sequencer.get_highest_published_sequence(3, 5)
        → SingleProducer: return min(5, cursor=5) → 5
        → No scan needed (O(1))

Result: Ok(5) — consumer can batch-process sequences 3, 4, 5
```

### Multi-Producer Trace (With Gap)

```
Setup: MultiProducerSequencer, buffer_size=8
       cursor = 7 (claimed up to 7)
       available_buffer: [✓,✓,✓,✓,✓,✓,✗,✓]
                          0  1  2  3  4  5  6  7
       (Producer for seq 6 hasn't published yet)

Consumer calls: barrier.wait_for(3)

Step 1: wait_strategy.wait_for(3, cursor=7, deps=[], barrier)
        → get_minimum_sequence(cursor=7, deps=[]) → 7
        → 7 >= 3 → return Ok(7)

Step 2: sequencer.get_highest_published_sequence(3, 7)
        → is_available(3)? ✓
        → is_available(4)? ✓
        → is_available(5)? ✓
        → is_available(6)? ✗ → return 5
        → (stops at first gap)

Result: Ok(5) — consumer processes sequences 3, 4, 5
        Sequence 6 is NOT safe to read yet!
```

**Without Step 2:** The consumer would receive `Ok(7)` and try to read sequence 6, which contains partially-written or stale data. **This is undefined behavior.**

### Pipeline Trace (Consumer Dependencies)

```
Setup: SingleProducerSequencer, buffer_size=8
       cursor = 7

       Consumer A: sequence = 5 (processed up to 5)
       Consumer B: depends on A.sequence

Consumer B calls: barrier_b.wait_for(3)
       barrier_b.dependent_sequences = [A.sequence]

Step 1: wait_strategy.wait_for(3, cursor=7, deps=[A.seq=5], barrier_b)
        → get_minimum_sequence(cursor=7, deps=[5]) → min(7, 5) → 5
        → 5 >= 3 → return Ok(5)

Step 2: sequencer.get_highest_published_sequence(3, 5) → 5

Result: Ok(5) — B can process sequences 3, 4, 5
        B cannot go past 5 because A hasn't processed 6 yet.
        When A advances to 7, B's next wait_for will return up to 7.
```

**The dependency chain is enforced by `get_minimum_sequence`.** The function takes the minimum of the cursor (producer's position) and all dependent sequences (upstream consumers' positions). This single function handles all topologies:

| Topology | dependent_sequences | Effect |
|----------|-------------------|--------|
| First-stage | `[]` | Consumer depends only on producer cursor |
| Pipeline (B after A) | `[A.sequence]` | B waits for both producer AND A |
| Diamond (D after A,B,C) | `[A.seq, B.seq, C.seq]` | D waits for ALL of them |

---

## Wiring Up Topologies

Now let's see how to construct barriers for each topology using real code.

### Factory Method

We add a convenience constructor to `SequenceBarrier` to reduce boilerplate when wiring topologies:

```rust
impl SequenceBarrier {
    // ... new(), check_alert(), alert(), wait_for() from above ...

    /// Convenience constructor: creates a barrier from a sequencer.
    ///
    /// `dependent_sequences` is the list of upstream consumer sequences
    /// that this consumer must wait for. Pass an empty vec for
    /// first-stage consumers (those that depend only on the producer).
    pub fn from_sequencer(
        sequencer: &Arc<dyn Sequencer>,
        wait_strategy: &Arc<dyn WaitStrategy>,
        dependent_sequences: Vec<Arc<AtomicI64>>,
    ) -> Self {
        Self::new(
            sequencer.cursor_arc(),
            Arc::clone(sequencer),
            Arc::clone(wait_strategy),
            dependent_sequences,
        )
    }
}
```

### Example 1: Pipeline (A → B → C)

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

// Shared infrastructure
let sequencer: Arc<dyn Sequencer> = Arc::new(
    SingleProducerSequencer::new(1024, vec![])
);
let wait_strategy: Arc<dyn WaitStrategy> = Arc::new(BusySpinWaitStrategy);

// Consumer sequences (each consumer tracks its own position)
let seq_a = Arc::new(AtomicI64::new(-1));
let seq_b = Arc::new(AtomicI64::new(-1));
let seq_c = Arc::new(AtomicI64::new(-1));

// Wire dependencies:
// A depends on producer only
let barrier_a = SequenceBarrier::from_sequencer(&sequencer, &wait_strategy, vec![]);

// B depends on A
let barrier_b = SequenceBarrier::from_sequencer(&sequencer, &wait_strategy, vec![Arc::clone(&seq_a)]);

// C depends on B
let barrier_c = SequenceBarrier::from_sequencer(&sequencer, &wait_strategy, vec![Arc::clone(&seq_b)]);

// Register the LAST consumer as the gating sequence.
// The producer checks this to avoid overwriting unprocessed events.
// In a pipeline, only the slowest (last) consumer matters.
sequencer.add_gating_sequence(Arc::clone(&seq_c));
```

**Why only register C as a gating sequence?** Because C is the last consumer in the pipeline. If C has processed up to sequence N, then A and B have necessarily processed past N (they're upstream of C). The producer only needs to check the slowest consumer.

### Example 2: Diamond (A, B → D)

```rust
let seq_a = Arc::new(AtomicI64::new(-1));
let seq_b = Arc::new(AtomicI64::new(-1));
let seq_d = Arc::new(AtomicI64::new(-1));

// A and B depend on producer only (parallel)
let barrier_a = SequenceBarrier::from_sequencer(&sequencer, &wait_strategy, vec![]);
let barrier_b = SequenceBarrier::from_sequencer(&sequencer, &wait_strategy, vec![]);

// D depends on BOTH A and B
let barrier_d = SequenceBarrier::from_sequencer(&sequencer, &wait_strategy, vec![
    Arc::clone(&seq_a),
    Arc::clone(&seq_b),
]);

// D is the terminal consumer — register as gating sequence
sequencer.add_gating_sequence(Arc::clone(&seq_d));
```

**D's `wait_for` guarantees:** `get_minimum_sequence` returns `min(cursor, A.seq, B.seq)`. D won't read any sequence until BOTH A and B have processed it.

### Example 3: Multicast (A, B, C Independent)

```rust
let seq_a = Arc::new(AtomicI64::new(-1));
let seq_b = Arc::new(AtomicI64::new(-1));
let seq_c = Arc::new(AtomicI64::new(-1));

// All three depend on producer only
let barrier_a = SequenceBarrier::from_sequencer(&sequencer, &wait_strategy, vec![]);
let barrier_b = SequenceBarrier::from_sequencer(&sequencer, &wait_strategy, vec![]);
let barrier_c = SequenceBarrier::from_sequencer(&sequencer, &wait_strategy, vec![]);

// ALL consumers are terminal — register all as gating sequences.
// Producer must wait for the SLOWEST consumer before overwriting.
sequencer.add_gating_sequence(Arc::clone(&seq_a));
sequencer.add_gating_sequence(Arc::clone(&seq_b));
sequencer.add_gating_sequence(Arc::clone(&seq_c));
```

**Why register all three?** Unlike a pipeline, there's no ordering between A, B, and C. Any one of them could be the slowest at any given moment. The producer must check all of them to prevent overwriting an event that any consumer hasn't processed yet.

### Gating Sequence Summary

| Topology | Gating Sequences | Why |
|----------|-----------------|-----|
| Pipeline (A→B→C) | `[C]` | C is last; A and B are always ahead |
| Diamond (A,B→D) | `[D]` | D is last; A and B are always ahead |
| Multicast (A,B,C) | `[A, B, C]` | No ordering; any could be slowest |
| Mixed (A→B, A→C) | `[B, C]` | Both are terminal; either could be slowest |

---

## Testing

### Test 1: Single-Producer Barrier Returns Correct Sequence

```rust
#[test]
fn barrier_returns_available_sequence_single_producer() {
    let cursor = Arc::new(AtomicI64::new(-1));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(8, vec![])
    );
    let wait_strategy: Arc<dyn WaitStrategy> = Arc::new(BusySpinWaitStrategy);

    let barrier = SequenceBarrier::new(
        Arc::clone(&cursor),
        Arc::clone(&sequencer),
        Arc::clone(&wait_strategy),
        vec![],
    );

    // Simulate: producer publishes sequences 0..=4
    cursor.store(4, Ordering::Release);

    let result = barrier.wait_for(0).unwrap();
    assert_eq!(result, 4); // Can batch-read 0, 1, 2, 3, 4
}
```

### Test 2: Dependency Chain Prevents Reading Ahead

```rust
#[test]
fn barrier_respects_upstream_dependency() {
    let cursor = Arc::new(AtomicI64::new(9));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(16, vec![])
    );
    let wait_strategy: Arc<dyn WaitStrategy> = Arc::new(BusySpinWaitStrategy);

    // Consumer A has only processed up to sequence 3
    let seq_a = Arc::new(AtomicI64::new(3));

    // Consumer B depends on A
    let barrier_b = SequenceBarrier::new(
        Arc::clone(&cursor),
        Arc::clone(&sequencer),
        Arc::clone(&wait_strategy),
        vec![Arc::clone(&seq_a)],
    );

    // B asks for sequence 0
    let result = barrier_b.wait_for(0).unwrap();
    // B can only read up to 3 (A's position), not 9 (cursor)
    assert_eq!(result, 3);

    // A advances to 7
    seq_a.store(7, Ordering::Release);
    let result = barrier_b.wait_for(4).unwrap();
    assert_eq!(result, 7);
}
```

### Test 3: Diamond Topology — D Waits for All Upstream

```rust
#[test]
fn diamond_topology_waits_for_slowest_upstream() {
    let cursor = Arc::new(AtomicI64::new(10));
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(16, vec![])
    );
    let wait_strategy: Arc<dyn WaitStrategy> = Arc::new(BusySpinWaitStrategy);

    let seq_a = Arc::new(AtomicI64::new(7));
    let seq_b = Arc::new(AtomicI64::new(4)); // B is the slowest

    // D depends on both A and B
    let barrier_d = SequenceBarrier::new(
        Arc::clone(&cursor),
        Arc::clone(&sequencer),
        Arc::clone(&wait_strategy),
        vec![Arc::clone(&seq_a), Arc::clone(&seq_b)],
    );

    let result = barrier_d.wait_for(0).unwrap();
    // D is limited by B (the slowest at 4), not A (at 7)
    assert_eq!(result, 4);

    // B catches up past A
    seq_b.store(8, Ordering::Release);
    let result = barrier_d.wait_for(5).unwrap();
    // Now A is the slowest at 7
    assert_eq!(result, 7);
}
```

### Test 4: Multi-Producer Gap Detection

```rust
#[test]
fn barrier_detects_multi_producer_gap() {
    // Create a multi-producer sequencer with buffer_size=8
    let consumer_seq = Arc::new(AtomicI64::new(-1));
    let sequencer = Arc::new(
        MultiProducerSequencer::new(8, vec![Arc::clone(&consumer_seq)])
    );
    let wait_strategy: Arc<dyn WaitStrategy> = Arc::new(BusySpinWaitStrategy);

    // Simulate: three producers each claim one slot.
    // In production, these claims happen from different threads.
    // Here we call claim() sequentially to test gap detection logic
    // in isolation — concurrent claiming is tested in Post 3C.
    //
    // Producer 1: claims and publishes seq 0
    // Producer 2: claims seq 1, has NOT published yet
    // Producer 3: claims and publishes seq 2

    // Claim and publish seq 0
    {
        let claim = sequencer.claim(1); // claims seq 0
        // drop publishes seq 0
    }

    // Claim seq 1 but hold the claim (don't publish)
    let held_claim = sequencer.claim(1); // claims seq 1

    // Claim and publish seq 2
    {
        let claim = sequencer.claim(1); // claims seq 2
        // drop publishes seq 2
    }
    // Now: seq 0 published, seq 1 NOT published, seq 2 published
    // Cursor = 2

    let barrier = SequenceBarrier::new(
        sequencer.cursor_arc(),
        Arc::clone(&sequencer) as Arc<dyn Sequencer>,
        Arc::clone(&wait_strategy),
        vec![],
    );

    let result = barrier.wait_for(0).unwrap();
    // Barrier returns 0 — seq 1 is a gap, so contiguous range is [0..=0]
    assert_eq!(result, 0);

    // Now publish seq 1 (drop the held claim)
    drop(held_claim);

    let result = barrier.wait_for(0).unwrap();
    // All three published — contiguous range is [0..=2]
    assert_eq!(result, 2);
}
```

**Why this test matters:** This is the exact scenario that would cause undefined behavior without `get_highest_published_sequence`. The cursor says "2", but reading sequence 1 would access uninitialized or partially-written data.

### Test 5: Alert Interrupts Waiting

```rust
#[test]
fn alert_interrupts_barrier() {
    let cursor = Arc::new(AtomicI64::new(-1)); // Nothing published
    let sequencer: Arc<dyn Sequencer> = Arc::new(
        SingleProducerSequencer::new(8, vec![])
    );
    let wait_strategy: Arc<dyn WaitStrategy> = Arc::new(BusySpinWaitStrategy);

    let barrier = Arc::new(SequenceBarrier::new(
        Arc::clone(&cursor),
        sequencer,
        wait_strategy,
        vec![],
    ));

    let barrier_clone = Arc::clone(&barrier);
    let handle = std::thread::spawn(move || {
        // This will spin waiting for sequence 0
        barrier_clone.wait_for(0)
    });

    // Give the consumer thread time to start spinning
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Alert the barrier — consumer should wake up with Err(Alerted)
    barrier.alert();

    let result = handle.join().unwrap();
    assert_eq!(result, Err(BarrierError::Alerted));
}
```

### Test 6: Concurrent Pipeline (Loom)

```rust
#[cfg(loom)]
#[test]
fn loom_pipeline_ordering() {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicI64, Ordering};

    loom::model(|| {
        let cursor = Arc::new(AtomicI64::new(-1));
        let seq_a = Arc::new(AtomicI64::new(-1));

        let cursor_producer = Arc::clone(&cursor);
        let cursor_consumer_a = Arc::clone(&cursor);
        let cursor_consumer_b = Arc::clone(&cursor);
        let seq_a_writer = Arc::clone(&seq_a);
        let seq_a_reader = Arc::clone(&seq_a);

        // Producer: publish sequence 0
        let producer = loom::thread::spawn(move || {
            cursor_producer.store(0, Ordering::Release);
        });

        // Consumer A: waits for cursor, then updates seq_a
        let consumer_a = loom::thread::spawn(move || {
            // Spin until cursor >= 0
            while cursor_consumer_a.load(Ordering::Acquire) < 0 {
                loom::thread::yield_now();
            }
            // "Process" and update our sequence
            seq_a_writer.store(0, Ordering::Release);
        });

        // Consumer B: waits for seq_a (depends on A)
        let consumer_b = loom::thread::spawn(move || {
            // Spin until A has processed seq 0
            while seq_a_reader.load(Ordering::Acquire) < 0 {
                loom::thread::yield_now();
            }
            // At this point, seq_a >= 0, which means A has processed seq 0.
            // Verify cursor is also >= 0 (it must be, since A read it).
            let cursor_val = cursor_consumer_b.load(Ordering::Acquire);
            assert!(cursor_val >= 0, "cursor must be visible after A processed");
        });

        producer.join().unwrap();
        consumer_a.join().unwrap();
        consumer_b.join().unwrap();
    });
}
```

**What Loom proves:** Under all possible interleavings, if Consumer B sees `seq_a >= 0`, the cursor is also visible (`>= 0`). This is the transitivity of Release/Acquire — Producer's `Release` store to cursor → A's `Acquire` load of cursor + `Release` store to `seq_a` → B's `Acquire` load of `seq_a` creates a happens-before chain.

---

## Performance Characteristics

| Operation | Single-Producer | Multi-Producer |
|-----------|----------------|----------------|
| `wait_for` Step 1 (wait strategy) | Same | Same |
| `wait_for` Step 2 (gap check) | O(1) — `min(hi, cursor)` | O(N) — scan `available_buffer` |
| Memory loads per `wait_for` | 1 + len(deps) | 1 + len(deps) + batch_size |
| Cache impact | 1 cache line (cursor) | 1 + batch_size/16 cache lines |

**Practical impact:** For a batch size of 64 with multi-producer, Step 2 scans 64 `AtomicI32` values across ~4 cache lines (~20ns total on modern hardware). This is dwarfed by the event processing time itself.

**The key optimization:** `wait_for` returns the highest contiguous sequence, enabling **batch processing**. Instead of calling `wait_for` for each individual event, the consumer calls it once and processes a batch:

```rust
let mut next_sequence = 0i64;
loop {
    // One wait_for call — may return a batch of sequences
    let available = barrier.wait_for(next_sequence)?;

    // Process the entire batch without waiting again
    for seq in next_sequence..=available {
        let event = ring_buffer.get(seq);
        handler.on_event(event, seq, seq == available);
    }

    // Advance past the batch
    next_sequence = available + 1;
    consumer_sequence.store(available, Ordering::Release);
}
```

This amortizes the cost of `wait_for` (and `get_highest_published_sequence`) across many events.

---

## Key Takeaways

1. **`get_minimum_sequence` is the universal coordinator** — One function handles all topologies by taking the minimum of the cursor and all dependent sequences. Pipeline, diamond, multicast — the same code path.

2. **Multi-producer requires Step 2** — The cursor tracks *claimed* sequences, not *published* sequences. Without `get_highest_published_sequence`, consumers would read holes containing uninitialized data.

3. **`is_available` uses flag-based detection** — Each slot stores a "generation" flag. The flag increments each time the buffer wraps, so the consumer can distinguish "published in this lap" from "stale data from a previous lap".

4. **Gating sequences protect the producer** — Register terminal consumers (those with no downstream dependents) as gating sequences. The producer checks them to avoid overwriting unprocessed events.

5. **`Arc<dyn WaitStrategy>` enables shared signaling** — All barriers and the producer share the same wait strategy instance. When the producer publishes, it calls `signal_all_when_blocking()` once to wake all blocked consumers.

6. **Batch processing amortizes barrier cost** — `wait_for` returns the highest available sequence, not just the requested one. Process the entire batch without additional barrier calls.

---

## Next Up: Event Handlers

In **Part 6**, we'll build the event handler abstraction:

- **`EventHandler` trait** — The user-facing API for processing events
- **`BatchEventProcessor`** — The runnable that owns a barrier, calls `wait_for`, and dispatches events to the handler
- **Lifecycle hooks** — `on_start`, `on_shutdown`, `on_event` with batch-end flag

---

## References

### Rust Documentation

- [std::sync::atomic::Ordering](https://doc.rust-lang.org/std/sync/atomic/enum.Ordering.html)
- [std::sync::Arc](https://doc.rust-lang.org/std/sync/struct.Arc.html)
- [Rust Atomics and Locks](https://marabos.nl/atomics/) — Mara Bos, 2023 (Chapter 3: Memory Ordering)

### Java Disruptor Reference

- [SequenceBarrier.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/SequenceBarrier.java)
- [ProcessingSequenceBarrier.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/ProcessingSequenceBarrier.java)
- [AbstractSequencer.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/AbstractSequencer.java) — `isAvailable()` and `getHighestPublishedSequence()`
- [MultiProducerSequencer.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/MultiProducerSequencer.java) — `availableBuffer` scan

### Tools

- [Loom](https://github.com/tokio-rs/loom) — Concurrency testing framework

---

**Next:** [Part 6 — Event Handlers: Zero-Cost Dispatch, Batching & Lifecycle →](post6.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*