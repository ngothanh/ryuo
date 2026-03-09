# Building a Disruptor in Rust: Ryuo — Part 3B: Sequencer Design & Implementation

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 3B of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 3A (memory ordering fundamentals)

---

## Recap

In Part 3A, we learned:
- CPUs reorder memory operations for performance
- Release/Acquire orderings create happens-before relationships
- 90% of synchronization problems are flag-based (Pattern 1) or counter-based (Pattern 2)

Now let's use that knowledge to build the sequencer — the component that coordinates access to the ring buffer.

---

## Evolution of the Solution

Let's build a sequencer step by step. We'll start naive and progressively optimize.

### **Attempt 1: Manual Locking → RAII Guard**

The simplest approach is a mutex. But mutexes have two problems for our use case: they're slow (10-50ns uncontended, 1-10μs contended), and forgetting to release the lock causes deadlock. The "forgot to release" problem applies to any claim/publish pattern:

```rust
// Mutex approach — can forget to publish
let lock = sequencer.claim(1);
ring_buffer[lock.sequence()].value = 42;
// Oops! If we forget lock.publish() → next claim() blocks forever → DEADLOCK!

// Atomic approach — same problem
let seq = sequencer.claim(1);
ring_buffer[seq].value = 42;
// Oops! If we forget sequencer.publish(seq) → cursor never updated → consumer deadlock!
```

**Solution: RAII.** Use Rust's `Drop` trait to automatically publish when the claim goes out of scope:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

pub struct SequenceClaim {
    start: i64,
    end: i64,
    cursor: Arc<AtomicI64>,
}

impl SequenceClaim {
    pub fn start(&self) -> i64 { self.start }
    pub fn end(&self) -> i64 { self.end }
}

impl Drop for SequenceClaim {
    fn drop(&mut self) {
        // Automatically publish when claim goes out of scope!
        self.cursor.store(self.end, Ordering::Release);
    }
}

// Usage — impossible to forget to publish!
{
    let claim = sequencer.claim(1);
    ring_buffer[claim.start()].value = 42;
}  // claim.drop() automatically publishes!

// Works with early returns, panics, etc.
```

**Key insight:** Rust's ownership system makes "forgot to publish" a *default impossibility* — the `Drop` runs automatically when the claim goes out of scope, even on early returns and panics. This is a significant advantage over Java (no equivalent) and a stronger default than C++ (where you must explicitly choose to use the RAII wrapper).

**Caveat:** `std::mem::forget` can deliberately skip `Drop`, leaking the claim without publishing. This is safe (no UB) but will stall consumers. In practice, nobody calls `mem::forget` on domain types accidentally — it's an explicit opt-in.

### **Attempt 2: Single-Producer Optimization**

If there's only one producer, we don't need atomic operations to claim sequences:

```rust
use std::cell::Cell;

pub struct SingleProducerSequencer {
    cursor: Arc<AtomicI64>,          // For publishing (consumers read this)
    next_sequence: Cell<i64>,        // For claiming (only producer writes — not Sync!)
    buffer_size: usize,
}

impl SingleProducerSequencer {
    pub fn claim(&self, count: usize) -> SequenceClaim {
        let current = self.next_sequence.get();
        let next = current + count as i64;
        self.next_sequence.set(next);

        SequenceClaim {
            start: current,
            end: next - 1,
            cursor: Arc::clone(&self.cursor),
        }
    }
}
```

**Why `Cell` instead of `UnsafeCell`?** `Cell<i64>` provides interior mutability via safe `get()`/`set()` methods — no `unsafe` blocks needed. It also makes the struct `!Sync` automatically, which enforces the single-producer invariant at the type level: the compiler will reject any attempt to share this struct across threads. This is strictly better than `UnsafeCell` for `Copy` types like `i64`.

**Performance:** ~10ns per claim (just a load/store) vs ~50-200ns for multi-producer (atomic contention).

### **Attempt 3: Wrap-Around Prevention**

Without wrap-around prevention, the producer can overwrite data the consumer hasn't read yet. Track consumer positions and wait:

```rust
pub struct SingleProducerSequencer {
    cursor: Arc<AtomicI64>,
    next_sequence: Cell<i64>,
    buffer_size: usize,
    gating_sequences: Vec<Arc<AtomicI64>>,  // Consumer positions
}

impl SingleProducerSequencer {
    pub fn claim(&self, count: usize) -> SequenceClaim {
        let current = self.next_sequence.get();
        let next = current + count as i64;
        let wrap_point = next - 1 - self.buffer_size as i64;

        // Wait for all consumers to pass wrap_point
        while self.get_minimum_sequence() < wrap_point {
            std::hint::spin_loop();
        }

        self.next_sequence.set(next);

        SequenceClaim {
            start: current,
            end: next - 1,
            cursor: Arc::clone(&self.cursor),
        }
    }

    fn get_minimum_sequence(&self) -> i64 {
        self.gating_sequences
            .iter()
            .map(|seq| seq.load(Ordering::Acquire))
            .min()
            .unwrap_or(i64::MAX)
    }
}
```

### **Attempt 4: Cached Gating Sequence (Production-Ready)**

Checking all consumers on every claim is expensive. Cache the minimum consumer position:

```rust
pub struct SingleProducerSequencer {
    cursor: Arc<AtomicI64>,
    next_sequence: Cell<i64>,
    buffer_size: usize,
    gating_sequences: Vec<Arc<AtomicI64>>,
    cached_gating_sequence: AtomicI64,  // Cache!
}

impl SingleProducerSequencer {
    pub fn claim(&self, count: usize) -> SequenceClaim {
        let current = self.next_sequence.get();
        let next = current + count as i64;
        let wrap_point = next - 1 - self.buffer_size as i64;

        // Check cache first (fast path — no atomic loads of consumer positions!)
        let cached = self.cached_gating_sequence.load(Ordering::Relaxed);
        if wrap_point > cached || cached > current {
            // Cache is stale — recompute minimum.
            // The `cached > current` check handles sequence wrap-around:
            // if cached is ahead of where we are, the cache is invalid.
            let mut min = self.get_minimum_sequence();
            self.cached_gating_sequence.store(min, Ordering::Relaxed);

            while min < wrap_point {
                std::hint::spin_loop();
                min = self.get_minimum_sequence();
                self.cached_gating_sequence.store(min, Ordering::Relaxed);
            }
        }

        self.next_sequence.set(next);
        // ... create SequenceClaim
    }
}
```

**Cache hit rate:** High in typical workloads — consumers advance slowly relative to producer claims, so the cached minimum is usually still valid. The actual hit rate depends on your buffer size, number of consumers, and producer/consumer speed ratio. Result: ~10ns per claim (cache hit) vs ~50-100ns (cache miss with consumer iteration).

---

### **Evolution Summary**

| Attempt | Key Change | Performance |
|---------|-----------|-------------|
| 1. Mutex → RAII | Drop trait auto-publishes | 10-50ns (lock) |
| 2. Single-producer | No atomic in claim path | ~10ns |
| 3. Wrap-around | Check consumer positions | ~10ns (no wait) |
| 4. Cached gating | Skip consumer check on cache hit | ~10ns (high cache hit rate) |

**Key insights:**
1. **RAII** solves the "forgot to publish" bug — Rust's type system enforces it
2. **Single-producer** is 5-20x faster than multi-producer (no atomic contention)
3. **Caching** makes consumer checks 10x faster (avoid iteration)
4. **Wrap-around prevention is orthogonal** — can be added to any approach

Now let's implement both sequencers in detail.

---

## The Sequencer Trait

Let's define the interface all sequencers must implement:

```rust
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::Arc;

pub trait Sequencer: Send {
    /// Claim `count` slots. Blocks until available.
    fn claim(&self, count: usize) -> SequenceClaim;

    /// Try to claim `count` slots. Returns immediately.
    fn try_claim(&self, count: usize) -> Result<SequenceClaim, InsufficientCapacity>;

    /// Get current cursor position
    fn cursor(&self) -> i64;
}

#[derive(Debug, Clone, Copy)]
pub struct InsufficientCapacity;

/// RAII guard that automatically publishes on drop
pub struct SequenceClaim {
    start: i64,
    end: i64,
    strategy: PublishStrategy,
}

/// Publish strategy — avoids heap allocation in the hot path.
///
/// Note: In a pedagogical context, you might use `Box<dyn FnOnce>` for clarity.
/// We use an enum here because this is called on every claim (~10ns target),
/// and a heap allocation (~20-50ns) would dominate the cost.
///
/// We use `Arc` references to ensure soundness — the `SequenceClaim` holds
/// a strong reference to the data it publishes to, preventing use-after-free
/// if the sequencer is dropped while a claim is outstanding. The cost of
/// `Arc::clone` (~5ns for atomic increment) is acceptable given the safety
/// guarantee.
enum PublishStrategy {
    /// Single-producer: just store to the cursor
    Cursor {
        cursor: Arc<AtomicI64>,
    },
    /// Multi-producer: mark individual slots in the available buffer
    AvailableBuffer {
        available_buffer: Arc<[AtomicI32]>,
        index_mask: usize,
        buffer_size: usize,
    },
}

impl SequenceClaim {
    pub fn start(&self) -> i64 { self.start }
    pub fn end(&self) -> i64 { self.end }

    /// Iterate over claimed sequences
    pub fn iter(&self) -> impl Iterator<Item = i64> {
        self.start..=self.end
    }
}

impl Drop for SequenceClaim {
    fn drop(&mut self) {
        match &self.strategy {
            PublishStrategy::Cursor { cursor } => {
                cursor.store(self.end, Ordering::Release);
            }
            PublishStrategy::AvailableBuffer { available_buffer, index_mask, buffer_size } => {
                for seq in self.start..=self.end {
                    let index = (seq as usize) & index_mask;
                    let lap = (seq / *buffer_size as i64) as i32;
                    available_buffer[index].store(lap, Ordering::Release);
                }
            }
        }
    }
}
```

**Key design decisions:**

1. **`Send` but not `Sync` on the trait** — `SingleProducerSequencer` uses `Cell` and is `!Sync` by design (only one thread should call `claim`). `MultiProducerSequencer` is `Sync` since all its fields are atomic or immutable.
2. **Enum-based publishing** — No heap allocation (`Box<dyn FnOnce>`) in the hot path. `Arc::clone` adds ~5ns for the atomic ref count increment, but guarantees soundness (no use-after-free if the sequencer is dropped while a claim exists).
3. **Range support** — Can claim multiple sequences at once (batch claiming).

---

## SingleProducerSequencer: The Fast Path

Now let's implement the single-producer sequencer with all optimizations:

```rust
use std::cell::Cell;

pub struct SingleProducerSequencer {
    cursor: Arc<AtomicI64>,                // Published position (consumers read)
    next_sequence: Cell<i64>,              // Next sequence to claim (only producer writes)
    buffer_size: usize,
    gating_sequences: Vec<Arc<AtomicI64>>, // Consumer positions
    cached_gating_sequence: AtomicI64,     // Cached minimum (optimization)
}

// SingleProducerSequencer is Send (can be moved to the producer thread)
// but NOT Sync (must not be shared across threads — only one producer).
// Cell<i64> already makes it !Sync automatically — the compiler will reject
// any attempt to share this struct via Arc or similar.

impl SingleProducerSequencer {
    pub fn new(buffer_size: usize, gating_sequences: Vec<Arc<AtomicI64>>) -> Self {
        assert!(buffer_size.is_power_of_two(), "buffer_size must be power of 2");
        Self {
            cursor: Arc::new(AtomicI64::new(-1)),  // Start at -1 so first sequence is 0
            next_sequence: Cell::new(0),            // First claim will be 0
            buffer_size,
            gating_sequences,
            cached_gating_sequence: AtomicI64::new(-1),
        }
    }

    /// Get a clone of the cursor Arc — consumers use this to observe published sequences.
    /// Must be called before moving the sequencer to the producer thread.
    pub fn cursor_arc(&self) -> Arc<AtomicI64> {
        Arc::clone(&self.cursor)
    }

    fn get_minimum_sequence(&self) -> i64 {
        let mut minimum = i64::MAX;
        for seq in &self.gating_sequences {
            let value = seq.load(Ordering::Acquire);
            minimum = minimum.min(value);
        }
        self.cached_gating_sequence.store(minimum, Ordering::Relaxed);
        minimum
    }
}

impl Sequencer for SingleProducerSequencer {
    fn claim(&self, count: usize) -> SequenceClaim {
        assert!(count > 0 && count <= self.buffer_size,
            "count must be > 0 and <= buffer_size");
        let current = self.next_sequence.get();
        let next = current + count as i64;

        let wrap_point = next - 1 - self.buffer_size as i64;

        // Check cache first (fast path)
        let cached = self.cached_gating_sequence.load(Ordering::Relaxed);
        if wrap_point > cached || cached > current {
            // Cache is stale — need to check actual consumer positions.
            //
            // StoreLoad fence: make our cursor visible to consumers before
            // reading their positions. This prevents the CPU from reordering
            // the cursor store past subsequent consumer position loads.
            //
            // Note: cursor temporarily holds 'current - 1' (the last *published*
            // sequence, not the one we're about to claim). Consumers use cursor
            // to know which sequences are safe to read, so this is correct —
            // we're just ensuring they see our latest published position before
            // we check how far behind they are.
            //
            // This matches Java Disruptor's SingleProducerSequencer.next():
            //   cursor.setVolatile(nextValue) before getMinimumSequence()
            //
            // Cost: ~20-30 cycles on x86, ~50-100 cycles on ARM. Only on cache miss.
            self.cursor.store(current - 1, Ordering::SeqCst);

            while self.get_minimum_sequence() < wrap_point {
                std::hint::spin_loop();
            }
        }

        self.next_sequence.set(next);

        SequenceClaim {
            start: current,
            end: next - 1,
            strategy: PublishStrategy::Cursor {
                cursor: Arc::clone(&self.cursor),
            },
        }
    }

    fn try_claim(&self, count: usize) -> Result<SequenceClaim, InsufficientCapacity> {
        assert!(count > 0 && count <= self.buffer_size,
            "count must be > 0 and <= buffer_size");
        let current = self.next_sequence.get();
        let next = current + count as i64;
        let wrap_point = next - 1 - self.buffer_size as i64;

        let cached = self.cached_gating_sequence.load(Ordering::Relaxed);
        if wrap_point > cached || cached > current {
            self.cursor.store(current - 1, Ordering::SeqCst);

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
            },
        })
    }

    fn cursor(&self) -> i64 {
        self.cursor.load(Ordering::Acquire)
    }
}
```

**Key optimizations:**

1. **No atomic in claim** — `Cell<i64>` for next_sequence (single producer only, `!Sync` enforced by Cell)
2. **Cached gating sequence** — Avoids iterating consumers on every claim
3. **StoreLoad fence** — Ensures we see latest consumer positions (ARM/PowerPC correctness)
4. **`Arc::clone` in PublishStrategy** — Guarantees soundness (claim holds a strong reference, preventing use-after-free)
5. **Release on publish** — Makes event data visible to consumers

**⚠️ Usage constraint: one outstanding claim at a time.** The `cursor.store(current - 1, SeqCst)` on the slow path assumes all prior claims have been dropped (published). If you hold multiple `SequenceClaim`s simultaneously, the cursor store may publish a sequence before you've written data to it — a data race. This matches the Java Disruptor's `SingleProducerSequencer`, which has the same constraint. In practice, the natural pattern of `{ let claim = ...; write; }` enforces this automatically.

**Performance:**
- **Claim (cache hit)**: ~10ns (just Cell get/set + cache check)
- **Claim (cache miss)**: ~50-100ns (iterate consumers + update cache)
- **Claim (waiting)**: Depends on wait strategy


---

## MultiProducerSequencer: Coordinating Multiple Producers

When multiple producers need to write concurrently, we need atomic coordination.

### **The Available Buffer: Tracking Out-of-Order Publishing**

In multi-producer mode, sequences can be published out of order:

```
Time  Producer 1    Producer 2    Published Sequences
----  -----------   -----------   -------------------
t0    claim(5)      claim(6)      []
t1    write(5)      write(6)      []
t2                  publish(6)    [6]  ← 6 published, but 5 not yet!
t3    publish(5)                  [5, 6]
```

**Problem:** Consumer can't just check cursor — it needs to know which sequences are actually published.

**Solution:** An `available_buffer` that tracks each sequence individually using a "lap counter":

```rust
// For sequence 5 in buffer of size 4:
let index = 5 & 3;      // = 1 (which slot in ring buffer)
let lap = 5 / 4;        // = 1 (which "lap" around the ring)

// Producer stores lap number when publishing
available_buffer[index].store(lap, Ordering::Release);

// Consumer checks:
let expected_lap = sequence / buffer_size;
let actual_lap = available_buffer[index].load(Ordering::Acquire);
if actual_lap == expected_lap {
    // Sequence is published!
}
```

This lets consumers distinguish "sequence 1 published (lap 0)" from "sequence 5 published (lap 1)" — both map to index 1.

### **Implementation**

```rust
pub struct MultiProducerSequencer {
    cursor: Arc<AtomicI64>,                // Highest claimed sequence
    buffer_size: usize,
    index_mask: usize,
    gating_sequences: Vec<Arc<AtomicI64>>,
    cached_gating_sequence: AtomicI64,
    available_buffer: Arc<[AtomicI32]>,    // Tracks which sequences are published
}

// MultiProducerSequencer IS Sync — multiple producers can call claim() concurrently.
// All fields are either atomic (Arc<AtomicI64>, AtomicI64, Arc<[AtomicI32]>),
// immutable after construction (usize), or Vec<Arc<AtomicI64>> which is Sync
// because Arc<AtomicI64> is Sync. The compiler derives Sync automatically.

impl MultiProducerSequencer {
    pub fn new(buffer_size: usize, gating_sequences: Vec<Arc<AtomicI64>>) -> Self {
        assert!(buffer_size.is_power_of_two(), "buffer_size must be power of 2");

        let available_buffer: Arc<[AtomicI32]> = (0..buffer_size)
            .map(|_| AtomicI32::new(-1))
            .collect();

        Self {
            cursor: Arc::new(AtomicI64::new(-1)),
            buffer_size,
            index_mask: buffer_size - 1,
            gating_sequences,
            cached_gating_sequence: AtomicI64::new(-1),
            available_buffer,
        }
    }

    fn get_minimum_sequence(&self) -> i64 {
        let mut minimum = i64::MAX;
        for seq in &self.gating_sequences {
            let value = seq.load(Ordering::Acquire);
            minimum = minimum.min(value);
        }
        self.cached_gating_sequence.store(minimum, Ordering::Relaxed);
        minimum
    }
}

impl Sequencer for MultiProducerSequencer {
    fn claim(&self, count: usize) -> SequenceClaim {
        assert!(count > 0 && count <= self.buffer_size,
            "count must be > 0 and <= buffer_size");
        // Atomically claim sequence range (always succeeds, no retry!).
        // fetch_add returns the OLD cursor value. Since cursor starts at -1,
        // the first claimed sequence is current + 1 = 0.
        //
        // Example: cursor = -1, count = 3
        //   current = fetch_add(3) → returns -1, cursor becomes 2
        //   claimed range = [0, 1, 2] → start = 0, end = 2
        let current = self.cursor.fetch_add(count as i64, Ordering::AcqRel);
        let end = current + count as i64;  // = new cursor value

        let wrap_point = end - self.buffer_size as i64;

        let cached = self.cached_gating_sequence.load(Ordering::Relaxed);
        if wrap_point > cached || cached > current {
            // Note: Unlike SingleProducer, no separate StoreLoad fence needed.
            // fetch_add with AcqRel already updated the cursor atomically.
            while self.get_minimum_sequence() < wrap_point {
                std::hint::spin_loop();
            }
        }

        SequenceClaim {
            start: current + 1,
            end,
            strategy: PublishStrategy::AvailableBuffer {
                available_buffer: Arc::clone(&self.available_buffer),
                index_mask: self.index_mask,
                buffer_size: self.buffer_size,
            },
        }
    }

    fn try_claim(&self, count: usize) -> Result<SequenceClaim, InsufficientCapacity> {
        assert!(count > 0 && count <= self.buffer_size,
            "count must be > 0 and <= buffer_size");
        // For try_claim, use CAS loop to check capacity before claiming
        loop {
            let current = self.cursor.load(Ordering::Acquire);
            let end = current + count as i64;
            let wrap_point = end - self.buffer_size as i64;

            let cached = self.cached_gating_sequence.load(Ordering::Relaxed);
            if wrap_point > cached || cached > current {
                if self.get_minimum_sequence() < wrap_point {
                    return Err(InsufficientCapacity);
                }
            }

            if self.cursor.compare_exchange_weak(
                current, end, Ordering::AcqRel, Ordering::Acquire
            ).is_ok() {
                return Ok(SequenceClaim {
                    start: current + 1,
                    end,
                    strategy: PublishStrategy::AvailableBuffer {
                        available_buffer: Arc::clone(&self.available_buffer),
                        index_mask: self.index_mask,
                        buffer_size: self.buffer_size,
                    },
                });
            }
            // CAS failed, retry
        }
    }

    fn cursor(&self) -> i64 {
        self.cursor.load(Ordering::Acquire)
    }
}
```

**Key differences from single-producer:**

1. **`fetch_add` instead of `Cell`** — Atomic coordination between producers
2. **`AcqRel` ordering** — Synchronize with other producers (Pattern 2 from Part 3A)
3. **`available_buffer`** — Track out-of-order publishing via lap counting
4. **Inherently `Sync`** — All fields are atomic or immutable, so the compiler auto-derives `Sync`

**Important:** In multi-producer mode, `cursor` represents the highest *claimed* (not published) sequence. Consumers must check `available_buffer` to determine which sequences are actually published — they cannot simply read `cursor`. We'll build the consumer's `SequenceBarrier` to handle this in Part 5.

**Performance:**
- **Claim (no contention)**: ~50ns (fetch_add + cache check)
- **Claim (high contention)**: ~100-200ns (cache line bouncing)
- **5-20x slower than single-producer** (atomic contention cost)

---

### **Why fetch_add Instead of CAS Loop?**

| Aspect | fetch_add | CAS Loop |
|--------|-----------|----------|
| **Speed** | Faster (one atomic op) | Slower (retry loop) |
| **Fairness** | FIFO ordering | Potential starvation |
| **Simplicity** | Simpler | More complex |
| **Battle-tested** | Java Disruptor uses this | Alternative approach |

We use `fetch_add` for `claim()` (always succeeds) and CAS loop only for `try_claim()` (needs to check capacity before claiming). This matches the Java Disruptor approach.

---

## Key Takeaways

1. **RAII prevents bugs** — Rust's Drop trait makes "forgot to publish" impossible. This is a significant advantage over C++/Java.

2. **Single-producer is 5-20x faster** — No atomic contention. Use it when possible.

3. **Caching is critical** — Cached gating sequence makes consumer checks 10x faster.

4. **Avoid hot-path allocations** — Use enum dispatch instead of `Box<dyn FnOnce>`. `Arc::clone` (~5ns) is acceptable for soundness; `Box` allocation (~20-50ns) is not.

5. **`!Sync` enforces single-producer** — Rust's type system prevents accidentally sharing a `SingleProducerSequencer` across threads.

6. **Multi-producer cursor ≠ published** — In multi-producer mode, consumers must check `available_buffer`, not just `cursor`.

---

## Next Up: Usage, Testing & Performance

In **Part 3C**, we'll put it all together:

- **Usage examples** — Single producer, multi-producer, batch claiming, non-blocking
- **Loom testing** — Exhaustive concurrency testing
- **Performance analysis** — Expected latencies and when to use each sequencer

---

**Next:** [Part 3C — Usage, Testing & Performance →](post3c.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*