# Building a Disruptor in Rust: Ryuo — Part 2A: The Ring Buffer

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 2A of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 1, understanding of unsafe Rust, basic knowledge of CPU cache architecture

---

## The Foundation of Speed

In Part 1, we identified the problems with traditional queues: locks, cache misses, and allocations. Now we build the solution's foundation: **the ring buffer**.

This isn't just any ring buffer. It's:
- **Pre-allocated** - Zero allocations after initialization
- **Power-of-2 sized** - Bitwise AND instead of modulo (35-90x lower latency)
- **Cache-aligned** - Prevents false sharing between producer/consumer
- **Panic-safe** - Correct cleanup even if initialization fails

By the end of this post, you'll understand why `Vec<T>` isn't enough and how to safely use `UnsafeCell` and `MaybeUninit` for interior mutability. In **Part 2B**, we'll deep-dive into cache-line padding strategies to prevent false sharing.

---

## Why Not Just Use `Vec<T>`?

The naive approach would be:

```rust
pub struct RingBuffer<T> {
    buffer: Vec<T>,
    capacity: usize,
}

impl<T> RingBuffer<T> {
    pub fn get(&self, index: usize) -> &T {
        &self.buffer[index % self.capacity]  // Problem #1: Modulo is slow
    }

    pub fn get_mut(&mut self, index: usize) -> &mut T {  // Problem #2: Requires &mut self
        &mut self.buffer[index % self.capacity]
    }
}
```

This has three critical problems for HFT:

### **Problem #1: Modulo Arithmetic Is Slow**

```rust
let index = sequence % capacity;  // Division instruction: 35-90 cycles
```

Modern CPUs are fast at addition, subtraction, and bitwise operations (1 cycle). Division is expensive: a 64-bit `DIV` instruction has a latency of **35–90 cycles** on modern x86-64 (varies by µarch: ~36 cycles on Skylake, ~35-88 cycles on Zen 3) and throughput of roughly 1 per 21–74 cycles. Bitwise AND has a latency of 1 cycle and can execute 3–4 per cycle.

**Source:** Agner Fog, *Instruction tables*, 2024 — tables for Skylake, Zen3, and Golden Cove; Intel 64 and IA-32 Architectures Optimization Reference Manual

**The fix:** Use power-of-2 sizes and bitwise AND:

```rust
let index = sequence & (capacity - 1);  // Bitwise AND: 1 cycle
```

This only works if `capacity` is a power of 2:
- `capacity = 1024` → `mask = 1023` → `0b1111111111`
- `sequence & mask` keeps only the lower 10 bits
- Equivalent to `sequence % 1024` but 35-90x faster

### **Problem #2: Exclusive Mutability Prevents Concurrent Access**

```rust
pub fn get_mut(&mut self, index: usize) -> &mut T {
    // Requires &mut self - can't have multiple readers while writing!
}
```

The Disruptor pattern requires:
- **One writer** modifying a slot
- **Multiple readers** reading other slots simultaneously

Rust's borrow checker prevents this with `&mut self`. We need **interior mutability** - the ability to mutate through a shared reference.

**The fix:** Use `UnsafeCell<T>`:

```rust
use std::cell::UnsafeCell;

pub struct RingBuffer<T> {
    buffer: Box<[UnsafeCell<T>]>,  // Interior mutability
}

impl<T> RingBuffer<T> {
    pub fn get_mut(&self, index: usize) -> &mut T {
        // SAFETY: Caller ensures exclusive access via sequencer
        unsafe { &mut *self.buffer[index].get() }
    }
}
```

`UnsafeCell<T>` is Rust's primitive for interior mutability. It tells the compiler: "I'm handling synchronization manually."

### **Problem #3: Uninitialized Memory and Drop Safety**

We need `UnsafeCell<MaybeUninit<T>>` for interior mutability (Problem #2), but `MaybeUninit<T>` means Rust doesn't know which slots are initialized. If we manually allocate the buffer (e.g., via `alloc::alloc`) and a factory panics partway through initialization, we must drop only the successfully initialized slots — calling `assume_init_drop()` on uninitialized memory is **undefined behavior**.

Even with our `Vec`-based construction (where `Vec` itself correctly drops only its `len` elements on panic), tracking initialization count is defense-in-depth: it documents the invariant and protects against future refactoring that might change the allocation strategy.

**The fix:** Use `MaybeUninit<T>` and track initialization:

```rust
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct RingBuffer<T> {
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,
    initialized: AtomicUsize,  // Track how many slots are initialized
}
```

`MaybeUninit<T>` represents potentially uninitialized memory. We track how many slots are initialized, so our `Drop` impl only drops valid elements.

---

## The Implementation

Here's the complete ring buffer implementation:

```rust
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Pre-allocated, power-of-2, cache-aligned ring buffer
///
/// Uses cache-line padding on BOTH sides to prevent false sharing:
/// - `align(64)` ensures struct starts at cache-line boundary (64 bytes)
/// - Left padding protects actual fields from previous fields in parent struct
/// - Right padding protects actual fields from next fields in parent struct
///
/// Memory layout (192 bytes = 3 cache lines):
/// - Left padding: 64 bytes
/// - Actual fields: 32 bytes (buffer:16 + index_mask:8 + initialized:8)
/// - Right padding: 64 bytes
/// - Alignment tail padding: 32 bytes (compiler pads 160→192 for align(64))
///
/// When embedded as a field in another struct, the compiler may insert up to
/// 56 bytes of alignment padding *before* this struct to satisfy align(64).
/// This is NOT part of the struct's own size (std::mem::size_of = 192).
///
/// This guarantees complete cache-line isolation regardless of allocation location.
#[repr(C, align(64))]  // Guarantee cache-line alignment
pub struct RingBuffer<T> {
    /// Left padding (prevents false sharing with previous fields)
    _padding_left: [u8; 64],

    /// Buffer storage with interior mutability
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,

    /// Fast indexing mask (capacity - 1)
    index_mask: usize,

    /// Number of initialized slots (for panic-safe drop)
    /// AtomicUsize allows reading in Drop without explicit &mut self.
    /// No synchronization is needed - we only write during construction
    /// and read during drop (both have exclusive access).
    initialized: AtomicUsize,

    /// Right padding (prevents false sharing with next fields)
    _padding_right: [u8; 64],
}

impl<T> RingBuffer<T> {
    /// Create a new ring buffer with pre-allocated events
    ///
    /// # Panics
    /// - If `size` is not a power of 2
    /// - If `factory()` panics during initialization (safe - only drops initialized slots)
    pub fn new<F>(size: usize, factory: F) -> Self
    where
        F: Fn() -> T,
        T: Send,
    {
        assert!(size > 0, "Size must be greater than 0");
        assert!(size.is_power_of_two(), "Size must be power of 2");

        // Pre-allocate buffer with uninitialized memory
        let mut buffer = Vec::with_capacity(size);

        // Initialize each slot
        // If factory() panics, buffer.len() will reflect how many were initialized
        for _ in 0..size {
            buffer.push(UnsafeCell::new(MaybeUninit::new(factory())));
        }

        // Defense-in-depth: track initialized count for Drop.
        // In the current implementation, if factory() panics above, the Vec
        // drops its already-pushed elements and we never reach Self { .. }.
        // But this field protects against future refactoring (e.g., switching
        // to raw allocation) where Drop could run on a partially-initialized
        // buffer.
        let initialized = buffer.len();

        Self {
            _padding_left: [0; 64],
            buffer: buffer.into_boxed_slice(),
            index_mask: size - 1,
            initialized: AtomicUsize::new(initialized),
            _padding_right: [0; 64],
        }
    }

    /// Get immutable reference to event at sequence.
    ///
    /// # Caller Contract (enforced by sequencer, not by this method)
    ///
    /// This method is safe to call **only** through the sequencer API, which
    /// enforces all of the following at the system level:
    /// 1. Sequence has been published (via sequencer)
    /// 2. Sequence hasn't wrapped around (buffer_size ahead of slowest consumer)
    /// 3. No concurrent mutable access to this slot
    ///
    /// Calling this method directly with an invalid sequence is **undefined
    /// behavior** — even though the method signature is not `unsafe`. We keep
    /// these methods safe for ergonomic use within the sequencer; the ring
    /// buffer is not intended to be used standalone.
    #[inline]
    pub fn get(&self, sequence: i64) -> &T {
        let index = (sequence as usize) & self.index_mask;

        // SAFETY:
        // 1. Index is masked to buffer size (can't overflow)
        // 2. Sequence barrier prevents wrap-around (covered in Post 5)
        // 3. Only one writer per slot (enforced by sequencer in Post 3)
        // 4. Slot is initialized (guaranteed by constructor)
        unsafe {
            (*self.buffer[index].get()).assume_init_ref()
        }
    }

    /// Get mutable reference to event at sequence.
    ///
    /// # Caller Contract (enforced by sequencer, not by this method)
    ///
    /// Same contract as [`get`], plus:
    /// 1. Exclusive access to this sequence (via sequencer claim)
    /// 2. No concurrent readers for this slot
    /// 3. Sequence hasn't wrapped around
    ///
    /// See [`get`] for why this is not marked `unsafe`.
    #[inline]
    pub fn get_mut(&self, sequence: i64) -> &mut T {
        let index = (sequence as usize) & self.index_mask;

        // SAFETY: Same as get(), plus:
        // 5. Caller has exclusive claim (via SequenceClaim in Post 3)
        unsafe {
            (*self.buffer[index].get()).assume_init_mut()
        }
    }

    /// Get buffer size
    #[inline]
    pub fn size(&self) -> usize {
        self.buffer.len()
    }
}

// Panic-safe drop: Only drop initialized slots
impl<T> Drop for RingBuffer<T> {
    fn drop(&mut self) {
        // SAFETY: Only drop slots that were successfully initialized
        // If factory() panicked during construction, we only drop completed slots
        let initialized = *self.initialized.get_mut();

        for i in 0..initialized {
            unsafe {
                (*self.buffer[i].get()).assume_init_drop();
            }
        }
    }
}

// RingBuffer can be sent across threads if T can
unsafe impl<T: Send> Send for RingBuffer<T> {}

// RingBuffer can be shared across threads if T: Send + Sync
// SAFETY:
// - T: Send - Required because:
//   1. RingBuffer<T> can be sent to another thread
//   2. get_mut() returns &mut T, which gives mutable access to T
//   3. If T is not Send, moving RingBuffer across threads would violate Send safety
// - T: Sync - Required because:
//   1. Multiple threads can hold &RingBuffer simultaneously
//   2. get() returns &T, which can be shared across threads
//   3. If T is not Sync, sharing &T across threads would violate Sync safety
// - The sequencer (Post 3) ensures exclusive access to slots via SequenceClaim
// - Multiple threads can safely call get() on different sequences simultaneously
// - Only one thread can call get_mut() on a given sequence (enforced by sequencer)
unsafe impl<T: Send + Sync> Sync for RingBuffer<T> {}
```


---

## Understanding the Safety Invariants

The `unsafe` blocks require careful justification. Let's break down each safety requirement:

### **1. Index Bounds**

```rust
let index = (sequence as usize) & self.index_mask;
```

**Invariant:** `index` is always in bounds `[0, buffer.len())`

**Proof:**
- `index_mask = size - 1` (where `size` is power of 2)
- For `size = 1024`: `mask = 1023 = 0b1111111111`
- `sequence & mask` keeps only lower 10 bits
- Result is always `< 1024`

**Example:**
```rust
size = 1024, mask = 1023
sequence = 0    → index = 0
sequence = 1023 → index = 1023
sequence = 1024 → index = 0    (wraps around)
sequence = 2047 → index = 1023
```

### **2. Initialization**

```rust
(*self.buffer[index].get()).assume_init_ref()
```

**Invariant:** Slot at `index` is initialized

**Proof:**
- Constructor initializes all slots before returning
- `initialized` stores the final count after construction (via buffer.len())
- Drop only drops initialized slots if constructor panics
- After construction, all slots remain initialized (we never uninitialize)

### **3. No Data Races**

```rust
pub fn get_mut(&self, sequence: i64) -> &mut T
```

**Invariant:** No concurrent access to the same slot

**Proof (enforced by sequencer in Post 3):**
- Producer claims exclusive range via `Sequencer::claim()`
- Only one producer can claim a given sequence
- Consumers can't read until producer publishes
- Buffer size prevents wrap-around (producer can't lap slowest consumer)

**This is the critical invariant.** The ring buffer itself can't enforce this - it's the sequencer's job (Post 3).

**Concrete example of what goes wrong:**

```rust
// WRONG: Two mutable references to the same slot!
let event1 = rb.get_mut(seq);
let event2 = rb.get_mut(seq + buffer_size);  // Aliases event1!
event1.value = 42;  // Undefined behavior!

// RIGHT: Drop references before accessing the same slot again
{
    let event = rb.get_mut(seq);
    event.value = 42;
}  // Reference dropped
let event = rb.get_mut(seq + buffer_size);  // Now safe
```

Because `seq` and `seq + buffer_size` map to the same slot (via bitwise AND), holding both references violates Rust's aliasing rules. The sequencer prevents this by ensuring the producer can't lap the slowest consumer.

> **Golden Rule:** The ring buffer is safe if and only if the sequencer never allows two threads to access the same slot concurrently. This is the sequencer's responsibility, not the ring buffer's.

---

## Why Power-of-2 Sizes Matter

Let's measure the performance difference:

```rust
// Modulo (non-power-of-2)
fn index_modulo(sequence: i64, capacity: usize) -> usize {
    (sequence as usize) % capacity  // Division instruction
}

// Bitwise AND (power-of-2)
fn index_mask(sequence: i64, mask: usize) -> usize {
    (sequence as usize) & mask  // Bitwise AND
}
```

**Assembly comparison** (x86-64):

```asm
; Modulo version (capacity = 1000)
mov rax, rdi          ; sequence
xor edx, edx          ; clear upper bits
mov ecx, 1000
div rcx               ; rdx = rax % rcx (35-90 cycles, low throughput)
mov rax, rdx

; Bitwise AND version (capacity = 1024, mask = 1023)
mov rax, rdi          ; sequence
and rax, 1023         ; rax &= mask (1 cycle, high throughput)
```

**Performance impact:**

| Method | Latency (cycles) | Throughput | ILP Impact |
|--------|------------------|------------|------------|
| `%` (modulo) | 35-90 | 1 per 21-74 cycles | Blocks pipeline |
| `&` (mask) | 1 | 3-4 per cycle | High parallelism |
| **Speedup** | **35-90x faster** | **63-296x more throughput** | **No blocking** |

**Source:** Agner Fog, *Instruction tables*, 2024 — tables for Skylake, Zen3, and Golden Cove

**Tradeoff:** You must use power-of-2 sizes. For most use cases (1024, 2048, 4096, 8192), this is fine. If you need exactly 1000 slots, round up to 1024 and waste 24 slots. Rust makes this easy: `size.next_power_of_two()` rounds up to the nearest power of 2.

---

## Performance Characteristics

**Expected performance** (based on design principles):

| Operation | Latency | Notes |
|-----------|---------|-------|
| `get()` / `get_mut()` | 1-2 cycles | Bitwise AND + pointer dereference |
| Memory access (L1 hit) | ~1ns | If slot is in L1 cache |
| Memory access (L3 hit) | ~10-40ns | If slot is in L3 cache |
| Memory access (RAM) | ~50-300ns | If slot is not cached |

**Key insight:** The indexing is fast (1-2 cycles), but actual memory access depends on cache state. Sequential access patterns help the CPU prefetcher keep data in cache.

**We'll measure actual performance in Post 15** with proper benchmarking methodology.

---

## What We Haven't Covered

The ring buffer is just storage. It doesn't handle:
- **Cache-line padding** - How do we prevent false sharing? (Part 2B)
- **Coordination** - How do producers claim slots? (Post 3: Sequencers)
- **Synchronization** - How do consumers know when data is ready? (Post 4: Wait Strategies)
- **Dependencies** - How do multi-stage pipelines work? (Post 5: Sequence Barriers)

The ring buffer provides fast, cache-friendly storage. In **Part 2B**, we'll explore why the `#[repr(C, align(64))]` and padding fields in our struct are essential — through five progressively complex cache-line padding scenarios.

---

## Next Up: Cache-Line Padding

In **Part 2B**, we'll deep-dive into cache-line padding — the technique that prevents false sharing between producer and consumer:

- **Five scenarios** from naive (32 bytes) to paranoid (384 bytes)
- **Why LMAX chose "good enough" padding** — and why we chose different in Rust
- **The DCU prefetcher trap** — when 64-byte alignment isn't enough
- **Memory layout diagrams** with exact byte offsets

**Teaser:** False sharing can add 20-150ns per write. Our padding strategy eliminates it entirely.

---

## References

### Papers & Documentation

1. **LMAX Disruptor Paper** (2011)
   https://lmax-exchange.github.io/disruptor/files/Disruptor-1.0.pdf

2. **Intel 64 and IA-32 Architectures Optimization Reference Manual**
   https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html

3. **"What Every Programmer Should Know About Memory"** (Ulrich Drepper, 2007)
   https://people.freebsd.org/~lstewart/articles/cpumemory.pdf

4. **Agner Fog's Instruction Tables**
   https://www.agner.org/optimize/instruction_tables.pdf

### Rust Documentation

- [UnsafeCell](https://doc.rust-lang.org/std/cell/struct.UnsafeCell.html)
- [MaybeUninit](https://doc.rust-lang.org/std/mem/union.MaybeUninit.html)
- [The Rustonomicon - Atomics](https://doc.rust-lang.org/nomicon/atomics.html)

---

**Next:** [Part 2B — Cache-Line Padding: Preventing False Sharing →](post2B.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*
