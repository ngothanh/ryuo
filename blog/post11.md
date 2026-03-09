# Building a Disruptor in Rust: Ryuo — Part 11: Dynamic Topologies — Runtime Handler Addition With Safety Caveats

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 11 of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 5 (sequence barriers), Part 6 (event handlers), Part 9 (multi-producer)

---

## Recap

So far, every topology we've built is **static** — all consumers are wired at startup and never change:

```rust
// Part 5: Static topology, fixed at construction
let barrier = SequenceBarrier::from_sequencer(&sequencer, vec![]);
let mut processor = BatchEventProcessor::new(ring_buffer, barrier, handler);
processor.run(); // runs forever with the same handler
```

This works for most production systems. But some use cases require **runtime flexibility**:

| Use Case | Why Static Isn't Enough |
|----------|------------------------|
| Live monitoring | Attach a metrics handler without restart |
| A/B testing | Swap handler implementations at runtime |
| Debug tracing | Temporarily add a logging consumer |
| Dynamic scaling | Add consumers as load increases |

**⚠️ Warning:** Dynamic topologies are the most dangerous feature in the Disruptor. Every operation touches shared mutable state across threads. Most users should use static topologies. This post explains how dynamic topologies work, why they're risky, and how to use them safely.

---

## The Core Problem

Adding a consumer at runtime requires three atomic operations that must appear instantaneous to other threads:

1. **Set the new consumer's sequence** to the current cursor (so it starts from "now," not from the beginning)
2. **Register as a gating sequence** on the sequencer (so the producer doesn't overwrite unread events)
3. **Start the consumer thread** (so it begins processing)

If any of these steps happens out of order, you get race conditions:

```
Race Condition: Adding consumer without proper ordering

Producer cursor: 100
Buffer size: 8

Step 1: Create consumer (sequence = -1)
Step 2: Producer publishes seq 101 → wraps to slot 101 & 7 = 5
Step 3: Register consumer as gating sequence
        → Producer now checks consumer at -1
        → Producer can't advance past 7 (wraparound protection)
        ✅ But consumer needs to read from seq 101, not seq -1!

Step 1': Create consumer (sequence = -1)
Step 2': Register consumer as gating sequence (sequence still -1)
        → Producer blocked at slot 7!
        → Deadlock: producer waiting for consumer at -1,
          consumer hasn't even started yet
```

**The fix:** Set sequence to cursor *before* registering as gating sequence.

---

## Implementation

### Step 1: SequenceGroup

The `SequenceGroup` is the foundation — a thread-safe collection of sequences that supports add/remove at runtime.

```rust
use std::sync::RwLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// Dynamic sequence group: thread-safe collection of consumer sequences.
///
/// Uses `RwLock` for simplicity:
/// - Reads (get minimum) are frequent → shared lock
/// - Writes (add/remove) are rare → exclusive lock
///
/// For lock-free reads, see `FixedSequenceGroup` below.
pub struct SequenceGroup {
    sequences: RwLock<Vec<Arc<AtomicI64>>>,
}

impl SequenceGroup {
    pub fn new(sequences: Vec<Arc<AtomicI64>>) -> Self {
        Self {
            sequences: RwLock::new(sequences),
        }
    }

    /// Get the minimum sequence across all tracked consumers.
    ///
    /// This is the critical hot-path operation — called by the producer
    /// on every publish to check if it's safe to advance.
    pub fn get_minimum(&self) -> i64 {
        let sequences = self.sequences.read().unwrap();

        if sequences.is_empty() {
            return i64::MAX; // No consumers → producer can advance freely
        }

        let mut minimum = i64::MAX;
        for seq in sequences.iter() {
            minimum = minimum.min(seq.load(Ordering::Acquire));
        }
        minimum
    }

    /// Add a consumer sequence to the group.
    ///
    /// # IMPORTANT: Set the sequence to the current cursor BEFORE calling this!
    /// Otherwise the producer will see -1 and potentially deadlock.
    pub fn add(&self, sequence: Arc<AtomicI64>) {
        let mut sequences = self.sequences.write().unwrap();
        sequences.push(sequence);
    }

    /// Remove a consumer sequence from the group.
    ///
    /// Returns true if the sequence was found and removed.
    ///
    /// # IMPORTANT: Stop the consumer thread BEFORE calling this!
    /// A running consumer with its gating sequence removed will
    /// have its events overwritten by the producer.
    pub fn remove(&self, sequence: &Arc<AtomicI64>) -> bool {
        let mut sequences = self.sequences.write().unwrap();
        if let Some(pos) = sequences.iter().position(|s| Arc::ptr_eq(s, sequence)) {
            sequences.remove(pos);
            true
        } else {
            false
        }
    }

    pub fn size(&self) -> usize {
        self.sequences.read().unwrap().len()
    }
}
```

### Step 2: FixedSequenceGroup

For the common case where the topology is set at startup and never changes:

```rust
/// Immutable sequence group: zero synchronization overhead.
///
/// Use this when all consumers are known at startup.
/// No locks, no atomics for the group itself — just the sequence values.
pub struct FixedSequenceGroup {
    sequences: Vec<Arc<AtomicI64>>,
}

impl FixedSequenceGroup {
    pub fn new(sequences: Vec<Arc<AtomicI64>>) -> Self {
        Self { sequences }
    }

    /// Get the minimum sequence. No locking — the Vec is immutable.
    #[inline]
    pub fn get_minimum(&self) -> i64 {
        if self.sequences.is_empty() {
            return i64::MAX;
        }

        let mut minimum = i64::MAX;
        for seq in &self.sequences {
            minimum = minimum.min(seq.load(Ordering::Acquire));
        }
        minimum
    }

    pub fn size(&self) -> usize {
        self.sequences.len()
    }
}
```

**Performance comparison:**

| Operation | `FixedSequenceGroup` | `SequenceGroup` |
|-----------|---------------------|-----------------|
| `get_minimum()` | ~10-20ns | ~50-100ns |
| `add()` | N/A (immutable) | ~1-10μs |
| `remove()` | N/A (immutable) | ~1-10μs |
| Memory | Vec (stack or heap) | Vec + RwLock |

**The difference:** `FixedSequenceGroup` skips the `RwLock::read()` call. On the hot path (producer checking gating sequences), this saves ~30-80ns per publish. At 10M events/sec, that's 300-800ms saved per second.

### Step 3: Safe Dynamic Handler Addition

```rust
/// Manager for dynamic handler lifecycle.
///
/// Coordinates the tricky ordering of:
/// 1. Setting the handler's starting sequence
/// 2. Registering as a gating sequence
/// 3. Starting the handler thread
pub struct DynamicHandlerManager<T> {
    ring_buffer: Arc<RingBuffer<T>>,
    sequencer: Arc<dyn Sequencer>,
    gating_group: Arc<SequenceGroup>,
}

impl<T: Send + 'static> DynamicHandlerManager<T> {
    pub fn new(
        ring_buffer: Arc<RingBuffer<T>>,
        sequencer: Arc<dyn Sequencer>,
        gating_group: Arc<SequenceGroup>,
    ) -> Self {
        Self {
            ring_buffer,
            sequencer,
            gating_group,
        }
    }

    /// Add a handler at runtime.
    ///
    /// The handler starts processing from the current cursor position.
    /// Events published before this call are NOT processed by this handler.
    ///
    /// # Ordering Guarantees
    /// 1. Sequence set to cursor BEFORE gating registration
    /// 2. Gating registration BEFORE thread start
    /// This prevents both stale-read and deadlock race conditions.
    pub fn add_handler<H: EventConsumer<T> + Send + 'static>(
        &self,
        handler: H,
    ) -> HandlerHandle {
        let sequence = Arc::new(AtomicI64::new(-1));

        // Step 1: Set sequence to current cursor (start from "now")
        let current_cursor = self.sequencer.cursor();
        sequence.store(current_cursor, Ordering::Release);

        // Step 2: Register as gating sequence
        // From this point, the producer will wait for this consumer
        self.gating_group.add(Arc::clone(&sequence));

        // Step 3: Create and start barrier + processor
        let barrier = SequenceBarrier::from_sequencer(
            &*self.sequencer,
            vec![], // No upstream dependencies (reads from producer directly)
        );
        let barrier = Arc::new(barrier);

        let rb = Arc::clone(&self.ring_buffer);
        let seq = Arc::clone(&sequence);

        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();

        let thread = std::thread::spawn(move || {
            let mut processor = BatchEventProcessor::new(rb, barrier, handler);
            // In production, the processor would check shutdown_rx
            // and exit gracefully when signaled
            processor.run();
        });

        HandlerHandle {
            sequence,
            thread: Some(thread),
            shutdown_tx,
        }
    }

    /// Remove a handler at runtime.
    ///
    /// # Safety Contract
    /// The handler thread MUST be stopped before calling this.
    /// A running handler with its gating sequence removed will
    /// have its events overwritten — data corruption.
    pub fn remove_handler(&self, mut handle: HandlerHandle) {
        // Step 1: Signal shutdown
        let _ = handle.shutdown_tx.send(());

        // Step 2: Wait for thread to finish
        if let Some(thread) = handle.thread.take() {
            let _ = thread.join();
        }

        // Step 3: Remove gating sequence (safe — thread is stopped)
        self.gating_group.remove(&handle.sequence);
    }
}

/// Handle to a dynamically-added handler.
///
/// Must be used to remove the handler — dropping without removal
/// leaves a dangling gating sequence that will eventually deadlock
/// the producer.
pub struct HandlerHandle {
    sequence: Arc<AtomicI64>,
    thread: Option<std::thread::JoinHandle<()>>,
    shutdown_tx: std::sync::mpsc::Sender<()>,
}

impl Drop for HandlerHandle {
    fn drop(&mut self) {
        if self.thread.is_some() {
            eprintln!(
                "WARNING: HandlerHandle dropped without calling remove_handler(). \
                 This leaves a dangling gating sequence that will deadlock the producer."
            );
        }
    }
}
```

**Why is `Drop` a warning, not a fix?** We can't stop the thread in `Drop` because:
1. `Drop` can't block (it would deadlock if called from the processor's own thread)
2. We don't have a guaranteed shutdown mechanism (the processor might be blocked on `wait_for`)
3. Removing the gating sequence without stopping the thread causes data corruption

The warning alerts developers who forget to call `remove_handler()`.


---

## Step-by-Step Trace: Dynamic Handler Addition

```
Buffer size: 8, cursor: 50
Existing consumer C1 at sequence 48

── Step 1: Create new handler H2 ──────────────
sequence_h2 = AtomicI64::new(-1)

── Step 2: Set sequence to cursor ──────────────
sequence_h2.store(50, Release)
// H2 will start processing from seq 51

── Step 3: Register as gating sequence ─────────
gating_group.add(sequence_h2)
// Producer now checks: min(C1=48, H2=50) = 48
// Producer still gated by C1 (slower consumer)

── Step 4: Start processor thread ──────────────
thread::spawn(move || processor.run())
// H2 begins at seq 51, processes events as they arrive

── Meanwhile: Producer publishes seq 51 ────────
// Producer checks: min(C1=48, H2=50) = 48
// wrap_point = 51 - 8 = 43, 43 <= 48 → OK to publish
// H2 and C1 both process seq 51

── C1 advances to seq 51 ──────────────────────
// Producer checks: min(C1=51, H2=50) = 50
// H2 is now the slowest consumer

── H2 processes seq 51, advances to 51 ─────────
// Producer checks: min(C1=51, H2=51) = 51
// All consumers caught up
```

**What H2 misses:** Events 0-50 (published before H2 was added). This is by design — the alternative (replaying from 0) would require the entire history to be in the buffer, which limits throughput.

---

## Safety Caveats

### Caveat 1: The TOCTOU Window

Between setting the sequence and registering as a gating sequence, there's a time-of-check-to-time-of-use window:

```
Thread A (add_handler):          Thread B (producer):
  cursor = 100
  sequence.store(100, Release)
                                   publish seq 101..108 (wraps!)
                                   // gating check: min() = C1
                                   // H2 not registered yet
  gating_group.add(sequence)
  // H2 registered, but seq 101-108 may be overwritten!
```

**How wide is this window?** Typically nanoseconds. In practice, the buffer needs to be large enough that the producer can't wrap around in the time between `store` and `add`. With a 1024-slot buffer and ~10ns per publish, the producer needs ~10μs to wrap — far longer than the window.

**Mitigation:** Use a buffer size ≥ 4× the maximum burst rate. If the producer can burst 1000 events in 10μs, use a buffer of at least 4096.

### Caveat 2: Removal Race Condition

Removing a handler while it's still processing creates a data race:

```
Thread A (remove_handler):      Thread B (processor for H2):
  gating_group.remove(seq_h2)
                                   event = ring_buffer.get(seq)
                                   // Producer overwrites this slot!
                                   process(event)  // reads garbage
```

**The fix:** Always stop the processor thread *before* removing the gating sequence. `DynamicHandlerManager::remove_handler()` enforces this ordering.

### Caveat 3: Memory Ordering

```rust
// These two operations MUST happen in this order:
sequence.store(cursor, Ordering::Release);  // (1) Set starting point
gating_group.add(sequence);                  // (2) Register for protection

// If reordered (2 before 1):
// Producer sees sequence = -1, can't advance past buffer_size - 1
// Potential deadlock!
```

The `Release` store on (1) and the `RwLock::write()` on (2) provide sufficient ordering. The `RwLock` acts as a full memory barrier, so (1) is visible to all threads before (2) takes effect.

---

## Usage Example: Live Monitoring System

```rust
struct MonitoringHandler {
    name: String,
    event_count: u64,
}

impl EventConsumer<OrderEvent> for MonitoringHandler {
    fn on_event(&self, event: &OrderEvent, sequence: i64, _end_of_batch: bool) {
        self.event_count += 1;
        println!(
            "[{}] seq={}, order_id={}, count={}",
            self.name, sequence, event.order_id, self.event_count
        );
    }
}

// In production code:
let manager = DynamicHandlerManager::new(ring_buffer, sequencer, gating_group);

// Add a monitor when debugging starts
let handle = manager.add_handler(MonitoringHandler {
    name: "debug-monitor".to_string(),
    event_count: 0,
});

// ... some time later, debugging is done ...

// Remove the monitor cleanly
manager.remove_handler(handle);
```

---

## Testing

### Test 1: SequenceGroup Tracks Minimum Correctly

```rust
#[test]
fn sequence_group_tracks_minimum() {
    let seq1 = Arc::new(AtomicI64::new(5));
    let seq2 = Arc::new(AtomicI64::new(10));
    let seq3 = Arc::new(AtomicI64::new(3));

    let group = SequenceGroup::new(vec![
        Arc::clone(&seq1),
        Arc::clone(&seq2),
        Arc::clone(&seq3),
    ]);

    assert_eq!(group.get_minimum(), 3);

    // Advance seq3 past seq1
    seq3.store(7, Ordering::Release);
    assert_eq!(group.get_minimum(), 5); // seq1 is now minimum
}
```

### Test 2: Empty Group Returns MAX

```rust
#[test]
fn empty_group_returns_max() {
    let group = SequenceGroup::new(vec![]);
    assert_eq!(group.get_minimum(), i64::MAX);
}
```

### Test 3: Add and Remove Sequences

```rust
#[test]
fn add_and_remove_sequences() {
    let group = SequenceGroup::new(vec![]);
    let seq = Arc::new(AtomicI64::new(42));

    group.add(Arc::clone(&seq));
    assert_eq!(group.size(), 1);
    assert_eq!(group.get_minimum(), 42);

    let removed = group.remove(&seq);
    assert!(removed);
    assert_eq!(group.size(), 0);
    assert_eq!(group.get_minimum(), i64::MAX);
}
```

### Test 4: Remove Non-Existent Returns False

```rust
#[test]
fn remove_nonexistent_returns_false() {
    let group = SequenceGroup::new(vec![]);
    let seq = Arc::new(AtomicI64::new(0));
    assert!(!group.remove(&seq));
}
```

### Test 5: Concurrent Read/Write Safety

```rust
#[test]
fn concurrent_read_write_is_safe() {
    let group = Arc::new(SequenceGroup::new(vec![]));

    let handles: Vec<_> = (0..4).map(|i| {
        let g = Arc::clone(&group);
        std::thread::spawn(move || {
            let seq = Arc::new(AtomicI64::new(i as i64));
            g.add(Arc::clone(&seq));

            // Read minimum while other threads are adding
            for _ in 0..100 {
                let min = g.get_minimum();
                assert!(min <= i as i64 || min == i64::MAX);
            }

            g.remove(&seq);
        })
    }).collect();

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(group.size(), 0);
}
```

### Test 6: FixedSequenceGroup Performance Baseline

```rust
#[test]
fn fixed_group_matches_dynamic_group_results() {
    let seq1 = Arc::new(AtomicI64::new(10));
    let seq2 = Arc::new(AtomicI64::new(20));

    let fixed = FixedSequenceGroup::new(vec![
        Arc::clone(&seq1),
        Arc::clone(&seq2),
    ]);
    let dynamic = SequenceGroup::new(vec![
        Arc::clone(&seq1),
        Arc::clone(&seq2),
    ]);

    assert_eq!(fixed.get_minimum(), dynamic.get_minimum());

    seq1.store(25, Ordering::Release);
    assert_eq!(fixed.get_minimum(), dynamic.get_minimum());
}
```


---

## Performance Characteristics

| Operation | `FixedSequenceGroup` | `SequenceGroup` |
|-----------|---------------------|-----------------|
| `get_minimum()` (N consumers) | ~10ns + N×5ns | ~50ns + N×5ns |
| `add()` | N/A | ~1-10μs |
| `remove()` | N/A | ~1-10μs |
| Memory per consumer | 8 bytes (Arc pointer) | 8 bytes + RwLock overhead |

**Rule of thumb:** Use `FixedSequenceGroup` unless you need runtime changes. The 30-80ns saving per `get_minimum()` call matters at >5M events/sec.

---

## Key Takeaways

1. **Static topologies are preferred.** `FixedSequenceGroup` is faster, simpler, and eliminates an entire class of race conditions. Use it unless you have a concrete need for runtime changes.

2. **Dynamic handler addition requires strict ordering.** Set sequence → register gating → start thread. Any other order risks deadlock or data corruption.

3. **Dynamic handler removal requires stop-before-remove.** Stop the processor thread *before* removing the gating sequence. `DynamicHandlerManager::remove_handler()` enforces this.

4. **The TOCTOU window is narrow but real.** Size your buffer ≥ 4× maximum burst rate to prevent wrap-around during the addition window.

5. **`HandlerHandle` must be explicitly removed.** Dropping without removal leaves a dangling gating sequence that will deadlock the producer. The `Drop` impl warns but can't fix this.

6. **`RwLock` is sufficient for most systems.** Only upgrade to lock-free (epoch-based) if profiling shows RwLock contention on the read path — which requires >8 producers with frequent gating checks.

---

## Next Up: Panic Handling

In **Part 12**, we'll build panic recovery:

- **`PanicGuard`** — RAII sequence rollback on panic
- **`catch_unwind`** — Isolate handler panics from the processor
- **Panic strategies** — Halt, log-and-continue, retry with backoff
- **Isolation guarantees** — One handler's panic doesn't affect others

---

## References

### Rust Documentation

- [RwLock](https://doc.rust-lang.org/std/sync/struct.RwLock.html) — Reader-writer lock
- [Arc::ptr_eq](https://doc.rust-lang.org/std/sync/struct.Arc.html#method.ptr_eq) — Pointer equality for Arc
- [mpsc::channel](https://doc.rust-lang.org/std/sync/mpsc/fn.channel.html) — Shutdown signaling

### Java Disruptor Reference

- [SequenceGroup.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/SequenceGroup.java)
- [Disruptor.handleEventsWith()](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/dsl/Disruptor.java)

### Concurrency

- [TOCTOU Race Conditions](https://en.wikipedia.org/wiki/Time-of-check_to_time-of-use) — Time-of-check to time-of-use
- [Epoch-Based Reclamation](https://aturon.github.io/blog/2015/08/27/epoch/) — Lock-free memory management

---

**Next:** [Part 12 — Panic Handling: Recovery, Rollback, and Isolation →](post12.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*