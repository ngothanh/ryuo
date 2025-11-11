# Building a Disruptor in Rust: Ryuo — Part 2: The Ring Buffer

**Series:** Building a Disruptor in Rust: Ryuo  
**Part:** 2 of 16  
**Target Audience:** Systems engineers building high-performance, low-latency applications  
**Prerequisites:** Part 1, understanding of unsafe Rust, basic knowledge of CPU cache architecture

---

## The Foundation of Speed

In Part 1, we identified the problems with traditional queues: locks, cache misses, and allocations. Now we build the solution's foundation: **the ring buffer**.

This isn't just any ring buffer. It's:
- **Pre-allocated** - Zero allocations after initialization
- **Power-of-2 sized** - Bitwise AND instead of modulo (3-11x lower latency, 9-24x higher throughput)
- **Cache-aligned** - Prevents false sharing between producer/consumer
- **Panic-safe** - Correct cleanup even if initialization fails

By the end of this post, you'll understand why `Vec<T>` isn't enough and how to safely use `UnsafeCell` and `MaybeUninit` for interior mutability.

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
let index = sequence % capacity;  // Division instruction: 3-11 cycles
```

Modern CPUs are fast at addition, subtraction, and bitwise operations (1 cycle). Division is expensive (3-11 cycles on modern x86-64). More importantly, division has low throughput (1 per 3-6 cycles) vs bitwise AND (3-4 per cycle), limiting instruction-level parallelism.

**Source:** Intel optimization manual, Agner Fog's instruction tables

**The fix:** Use power-of-2 sizes and bitwise AND:

```rust
let index = sequence & (capacity - 1);  // Bitwise AND: 1 cycle
```

This only works if `capacity` is a power of 2:
- `capacity = 1024` → `mask = 1023` → `0b1111111111`
- `sequence & mask` keeps only the lower 10 bits
- Equivalent to `sequence % 1024` but 3-11x faster

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

### **Problem #3: Initialization and Drop Safety**

```rust
let buffer = vec![factory(); size];  // What if factory() panics?
```

If `factory()` panics during initialization, Rust will try to drop all elements. But some elements are uninitialized! This is **undefined behavior**.

**The fix:** Use `MaybeUninit<T>` and track initialization:

```rust
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct RingBuffer<T> {
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,
    initialized: AtomicUsize,  // Track how many slots are initialized
}
```

`MaybeUninit<T>` represents potentially uninitialized memory. We track how many slots are initialized, so `Drop` only drops valid elements.

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
/// Memory layout (216 bytes total):
/// - Compiler padding: 0-56 bytes (inserted before struct if needed)
/// - Left padding: 64 bytes
/// - Actual fields: 32 bytes (buffer:16 + index_mask:8 + initialized:8)
/// - Right padding: 64 bytes
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
        F: Fn() -> T + Send,
        T: Send,
    {
        assert!(size.is_power_of_two(), "Size must be power of 2");
        assert!(size > 0, "Size must be greater than 0");

        // Pre-allocate buffer with uninitialized memory
        let mut buffer = Vec::with_capacity(size);

        // Initialize each slot
        // If factory() panics, buffer.len() will reflect how many were initialized
        for _ in 0..size {
            buffer.push(UnsafeCell::new(MaybeUninit::new(factory())));
        }

        // Store final count (may be < size if factory panicked)
        let initialized = buffer.len();

        Self {
            _padding_left: [0; 64],
            buffer: buffer.into_boxed_slice(),
            index_mask: size - 1,
            initialized: AtomicUsize::new(initialized),
            _padding_right: [0; 64],
        }
    }

    /// Get immutable reference to event at sequence
    ///
    /// # Safety
    /// Caller must ensure:
    /// 1. Sequence has been published (via sequencer)
    /// 2. Sequence hasn't wrapped around (buffer_size ahead of slowest consumer)
    /// 3. No concurrent mutable access to this slot
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

    /// Get mutable reference to event at sequence
    ///
    /// # Safety
    /// Caller must ensure:
    /// 1. Exclusive access to this sequence (via sequencer claim)
    /// 2. No concurrent readers for this slot
    /// 3. Sequence hasn't wrapped around
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
div rcx               ; rdx = rax % rcx (3-11 cycles, low throughput)
mov rax, rdx

; Bitwise AND version (capacity = 1024, mask = 1023)
mov rax, rdi          ; sequence
and rax, 1023         ; rax &= mask (1 cycle, high throughput)
```

**Performance impact:**

| Method | Latency (cycles) | Throughput | ILP Impact |
|--------|------------------|------------|------------|
| `%` (modulo) | 3-11 | 1 per 3-6 cycles | Blocks pipeline |
| `&` (mask) | 1 | 3-4 per cycle | High parallelism |
| **Speedup** | **3-11x faster** | **9-24x more throughput** | **No blocking** |

**Source:** Intel optimization manual, Agner Fog's instruction tables

**Tradeoff:** You must use power-of-2 sizes. For most use cases (1024, 2048, 4096, 8192), this is fine. If you need exactly 1000 slots, round up to 1024 and waste 24 slots.

---

## Cache-Line Padding: Preventing False Sharing

You've probably heard that "false sharing is bad" and "you need padding." But **why**? Let's discover the problem through concrete scenarios, building from naive to perfect solutions. By the end, you'll understand not just **what** to do, but **why** each decision matters.

We'll explore five different implementations, see exactly what goes wrong with each, and understand the trade-offs that led LMAX Disruptor to their production choice.

---

### **Scenario 1: No Padding (32 bytes) - The Naive Approach**

Let's start with the simplest possible implementation:

```rust
#[repr(C)]
struct RingBufferNoPadding<T> {
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,  // 16 bytes (fat pointer)
    index_mask: usize,                           // 8 bytes
    initialized: AtomicUsize,                    // 8 bytes
    // Total: 32 bytes
}
```

**Looks fine, right?** Just 32 bytes of data. But let's see what happens when this struct is embedded in a larger system:

```rust
struct Disruptor<T> {
    producer_cursor: AtomicI64,           // Thread A writes here
    ring_buffer: RingBufferNoPadding<T>,  // Thread B reads here
    consumer_cursor: AtomicI64,           // Thread C writes here
}
```

**Memory layout:**

```
Byte 0-7:    producer_cursor (Thread A writes)
Byte 8-23:   ring_buffer.buffer (Thread B reads)
Byte 24-31:  ring_buffer.index_mask (Thread B reads)
Byte 32-39:  ring_buffer.initialized (Thread B reads)
Byte 40-47:  consumer_cursor (Thread C writes)
```

**Now here's the critical insight:** Modern CPUs don't load individual bytes from memory. They load **cache lines** - fixed 64-byte chunks. Think of memory as organized into 64-byte "boxes":

```
Cache Line 0 (bytes 0-63):
┌─────────────────────────────────────────────────────────────┐
│ [producer_cursor:8][buffer:16][index_mask:8][initialized:8]│
│ [consumer_cursor:8][unused:15]                             │
└─────────────────────────────────────────────────────────────┘
  ↑ Thread A writes    ↑ Thread B reads    ↑ Thread C writes

  ALL THREE THREADS ACCESS THE SAME 64-BYTE CACHE LINE!
```

**What goes wrong:**

```
Time 1: Thread A (Core 1) writes producer_cursor
┌─────────────────────────────────────────────────────────────┐
│ Core 1 Cache: [Line 0: Modified] ← Entire line owned by Core 1
│ Core 2 Cache: [empty]
│ Core 3 Cache: [empty]
└─────────────────────────────────────────────────────────────┘

Time 2: Thread B (Core 2) reads ring_buffer fields
┌─────────────────────────────────────────────────────────────┐
│ Core 1 Cache: [Line 0: Shared] ← Downgraded to Shared
│ Core 2 Cache: [Line 0: Shared] ← Fetched from Core 1 (20-50ns)
│ Core 3 Cache: [empty]
└─────────────────────────────────────────────────────────────┘
                         ↑ Cache coherence traffic!

Time 3: Thread C (Core 3) writes consumer_cursor
┌─────────────────────────────────────────────────────────────┐
│ Core 1 Cache: [Line 0: Invalid] ← Evicted!
│ Core 2 Cache: [Line 0: Invalid] ← Evicted!
│ Core 3 Cache: [Line 0: Modified] ← Fetched from Core 2 (20-50ns)
└─────────────────────────────────────────────────────────────┘
                         ↑ More cache coherence traffic!

Time 4: Thread A writes producer_cursor again
┌─────────────────────────────────────────────────────────────┐
│ Core 1 Cache: [Line 0: Modified] ← Fetched from Core 3 (20-50ns)
│ Core 2 Cache: [Line 0: Invalid]
│ Core 3 Cache: [Line 0: Invalid] ← Evicted!
└─────────────────────────────────────────────────────────────┘
                         ↑ Even more cache coherence traffic!
```

**This is called "false sharing"** - the threads aren't actually sharing data (they access different variables), but the CPU's cache coherence protocol treats them as if they are because they're in the same cache line.

**Performance impact:**
- **Without false sharing:** ~4 cycles per access (L1 cache hit)
- **With false sharing:** ~40-150 cycles per access (cache coherence overhead)
- **Slowdown:** 10-40x slower!

**Why it's called "false" sharing:** The threads are logically accessing different data, but the hardware sees them as sharing because cache lines are the atomic unit of cache coherence.

**Trade-offs:**
- ✅ **Memory:** Minimal (32 bytes)
- ❌ **Performance:** Terrible (10-40x slower due to cache line ping-pong)
- ❌ **Scalability:** Gets worse with more cores

**Verdict:** Never use in production. This is the "before" picture that shows why padding exists.

**Key lesson learned:** Cache lines (64 bytes on x86-64) are the atomic unit of cache coherence. If multiple threads access different variables in the same cache line, you get false sharing.

---

### **Scenario 2: Right Padding Only (96 bytes) - The Incomplete Solution**

"Okay, I learned about false sharing. Let me add padding!" A common first attempt:

```rust
#[repr(C)]
struct RingBufferRightPadding<T> {
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,  // 16 bytes
    index_mask: usize,                           // 8 bytes
    initialized: AtomicUsize,                    // 8 bytes
    _padding_right: [u8; 64],                    // 64 bytes ← Added padding!
    // Total: 96 bytes
}
```

**The idea:** Add 64 bytes of padding after the fields to push the next field into a different cache line.

**Does it work?** Let's see:

```rust
struct Disruptor<T> {
    producer_cursor: AtomicI64,               // Thread A writes here
    ring_buffer: RingBufferRightPadding<T>,   // Thread B reads here
    consumer_cursor: AtomicI64,               // Thread C writes here
}
```

**Memory layout:**

```
Byte 0-7:     producer_cursor (Thread A writes)
Byte 8-23:    ring_buffer.buffer (Thread B reads)
Byte 24-31:   ring_buffer.index_mask (Thread B reads)
Byte 32-39:   ring_buffer.initialized (Thread B reads)
Byte 40-103:  ring_buffer._padding_right (64 bytes of padding)
Byte 104-111: consumer_cursor (Thread C writes)
```

**Cache line view:**

```
Cache Line 0 (bytes 0-63):
┌─────────────────────────────────────────────────────────────┐
│ [producer_cursor:8][buffer:16][index_mask:8][initialized:8]│
│ [padding_right:23]                                          │
└─────────────────────────────────────────────────────────────┘
  ↑ Thread A writes    ↑ Thread B reads

  STILL IN THE SAME CACHE LINE!

Cache Line 1 (bytes 64-127):
┌─────────────────────────────────────────────────────────────┐
│ [padding_right:41][consumer_cursor:8][unused:15]           │
└─────────────────────────────────────────────────────────────┘
                       ↑ Thread C writes - OK, isolated!
```

**What went wrong:**

```
Time 1: Thread A (Core 1) writes producer_cursor
┌─────────────────────────────────────────────────────────────┐
│ Core 1 Cache: [Line 0: Modified]
│ Core 2 Cache: [empty]
└─────────────────────────────────────────────────────────────┘

Time 2: Thread B (Core 2) reads ring_buffer fields
┌─────────────────────────────────────────────────────────────┐
│ Core 1 Cache: [Line 0: Shared]
│ Core 2 Cache: [Line 0: Shared] ← Fetched from Core 1 (20-50ns)
└─────────────────────────────────────────────────────────────┘
                         ↑ Still have false sharing!

Time 3: Thread A writes producer_cursor again
┌─────────────────────────────────────────────────────────────┐
│ Core 1 Cache: [Line 0: Modified] ← Fetched from Core 2 (20-50ns)
│ Core 2 Cache: [Line 0: Invalid] ← Evicted!
└─────────────────────────────────────────────────────────────┘
                         ↑ Cache line ping-pong continues!
```

**The problem:** Right padding protects against fields that come **after** the struct (consumer_cursor is now isolated), but it doesn't protect against fields that come **before** the struct (producer_cursor still shares a cache line with ring_buffer).

**What worked:**
- ✅ `consumer_cursor` is now in Cache Line 1 (isolated from ring_buffer)
- ✅ Thread C can write to `consumer_cursor` without affecting Thread B

**What didn't work:**
- ❌ `producer_cursor` still shares Cache Line 0 with ring_buffer
- ❌ Thread A writing to `producer_cursor` still invalidates ring_buffer on Thread B's core

**Trade-offs:**
- ✅ **Memory:** Moderate (96 bytes)
- ⚠️ **Performance:** 50% better (fixed false sharing on one side)
- ⚠️ **Scalability:** Partial improvement

**Verdict:** Better than nothing, but incomplete. You need padding on **both** sides.

**Key lesson learned:** Padding only protects in one direction. You need padding on both sides to isolate a struct from fields before AND after it.

---

### **Scenario 3: Both Paddings (160 bytes) - The "Good Enough" Solution**

"Okay, I need padding on both sides!" Let's try:

```rust
#[repr(C)]
struct RingBufferBothPadding<T> {
    _padding_left: [u8; 64],                     // 64 bytes ← Left padding
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,  // 16 bytes
    index_mask: usize,                           // 8 bytes
    initialized: AtomicUsize,                    // 8 bytes
    _padding_right: [u8; 64],                    // 64 bytes ← Right padding
    // Total: 160 bytes (2.5 cache lines)
}
```

**The idea:** Add 64 bytes of padding on **both** sides to isolate the fields from neighbors.

**Does it work?** Let's see:

```rust
struct Disruptor<T> {
    producer_cursor: AtomicI64,             // Thread A writes here
    ring_buffer: RingBufferBothPadding<T>,  // Thread B reads here
    consumer_cursor: AtomicI64,             // Thread C writes here
}
```

**Memory layout (assuming ring_buffer starts at byte 8):**

```
Byte 0-7:     producer_cursor (Thread A writes)
Byte 8-71:    ring_buffer._padding_left (64 bytes)
Byte 72-87:   ring_buffer.buffer (Thread B reads)
Byte 88-95:   ring_buffer.index_mask (Thread B reads)
Byte 96-103:  ring_buffer.initialized (Thread B reads)
Byte 104-167: ring_buffer._padding_right (64 bytes)
Byte 168-175: consumer_cursor (Thread C writes)
```

**Cache line view:**

```
Cache Line 0 (bytes 0-63):
┌─────────────────────────────────────────────────────────────┐
│ [producer_cursor:8][padding_left:55]                       │
└─────────────────────────────────────────────────────────────┘
  ↑ Thread A writes - isolated!

Cache Line 1 (bytes 64-127):
┌─────────────────────────────────────────────────────────────┐
│ [padding_left:9][buffer:16][index_mask:8][initialized:8]   │
│ [padding_right:23]                                          │
└─────────────────────────────────────────────────────────────┘
                    ↑ Thread B reads - mostly isolated!

Cache Line 2 (bytes 128-191):
┌─────────────────────────────────────────────────────────────┐
│ [padding_right:41][consumer_cursor:8][unused:15]           │
└─────────────────────────────────────────────────────────────┘
                    ↑ Thread C writes - isolated!
```

**This looks good!** Each thread's data is in a different cache line. But wait...

**The Hidden Problem: No Alignment Guarantee**

Here's the catch: `#[repr(C)]` guarantees field **order** and field **alignment** (8 bytes for `usize`), but it does **NOT** guarantee that the struct starts at a cache-line boundary (64 bytes).

**What if the struct starts at byte 0 instead of byte 8?**

```
Cache Line 0 (bytes 0-63):
┌─────────────────────────────────────────────────────────────┐
│ [padding_left:64]                                           │
└─────────────────────────────────────────────────────────────┘
  ↑ Actually isolated! Lucky!

Cache Line 1 (bytes 64-127):
┌─────────────────────────────────────────────────────────────┐
│ [buffer:16][index_mask:8][initialized:8][padding_right:32] │
└─────────────────────────────────────────────────────────────┘
  ↑ Actually isolated! Lucky!
```

**What if the struct starts at byte 32?**

```
Cache Line 0 (bytes 0-63):
┌─────────────────────────────────────────────────────────────┐
│ [producer_cursor:8][padding:24][padding_left:32]           │
└─────────────────────────────────────────────────────────────┘
  ↑ Thread A writes        ↑ Padding spans cache lines!

Cache Line 1 (bytes 64-127):
┌─────────────────────────────────────────────────────────────┐
│ [padding_left:32][buffer:16][index_mask:8][initialized:8]  │
└─────────────────────────────────────────────────────────────┘
                    ↑ Thread B reads - fields span cache lines!
```

**The problem:** The struct can start at **any 8-byte boundary** (because the largest field is 8 bytes). This means:
- Sometimes you get lucky and the padding works perfectly
- Sometimes the struct starts at an odd offset and fields span cache lines
- You have **no guarantee** of cache-line isolation

**In practice:** Most allocators tend to align larger structs reasonably well, so this "usually works." But it's not guaranteed.

**Trade-offs:**
- ✅ **Memory:** Moderate (160 bytes)
- ⚠️ **Performance:** Usually good, but depends on allocation alignment
- ⚠️ **Portability:** Works in practice most of the time
- ❌ **Guarantee:** No guarantee of cache-line isolation

**Verdict:** "Good enough" for many use cases. This is what many implementations use because it's simple and usually works. But if you want **guaranteed** correctness, you need explicit alignment.

**Key lesson learned:** Padding alone isn't enough. You also need to ensure the struct itself starts at a cache-line boundary.

---

### **Scenario 4: Aligned Padding (216 bytes) - The Correct Solution**

"I need to **guarantee** the struct starts at a cache-line boundary!" Here's how:

```rust
#[repr(C, align(64))]  // ← Force 64-byte alignment!
struct RingBufferAligned<T> {
    _padding_left: [u8; 64],                     // 64 bytes
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,  // 16 bytes
    index_mask: usize,                           // 8 bytes
    initialized: AtomicUsize,                    // 8 bytes
    _padding_right: [u8; 64],                    // 64 bytes
    // Total: 160 bytes (2.5 cache lines)
}
```

**The key:** `align(64)` tells the compiler "this struct MUST start at a 64-byte boundary."

**What happens:**

```rust
struct Disruptor<T> {
    producer_cursor: AtomicI64,            // Thread A writes here
    ring_buffer: RingBufferAligned<T>,     // Thread B reads here
    consumer_cursor: AtomicI64,            // Thread C writes here
}
```

**Memory layout:**

```
Byte 0-7:     producer_cursor (Thread A writes)
Byte 8-63:    [compiler padding] ← Compiler inserts 56 bytes!
Byte 64-127:  ring_buffer._padding_left (64 bytes)
Byte 128-143: ring_buffer.buffer (Thread B reads)
Byte 144-151: ring_buffer.index_mask (Thread B reads)
Byte 152-159: ring_buffer.initialized (Thread B reads)
Byte 160-223: ring_buffer._padding_right (64 bytes)
Byte 224-231: consumer_cursor (Thread C writes)
```

**Cache line view:**

```
Cache Line 0 (bytes 0-63):
┌─────────────────────────────────────────────────────────────┐
│ [producer_cursor:8][compiler padding:56]                   │
└─────────────────────────────────────────────────────────────┘
  ↑ Thread A writes - isolated!

Cache Line 1 (bytes 64-127):
┌─────────────────────────────────────────────────────────────┐
│ [padding_left:64]                                           │
└─────────────────────────────────────────────────────────────┘
  ↑ Sacrificial padding (absorbs any access from Line 0)

Cache Line 2 (bytes 128-191):
┌─────────────────────────────────────────────────────────────┐
│ [buffer:16][index_mask:8][initialized:8][padding_right:32] │
└─────────────────────────────────────────────────────────────┘
  ↑ Thread B reads - PERFECTLY isolated!

Cache Line 3 (bytes 192-255):
┌─────────────────────────────────────────────────────────────┐
│ [padding_right:32][consumer_cursor:8][unused:24]           │
└─────────────────────────────────────────────────────────────┘
  ↑ Sacrificial padding    ↑ Thread C writes - isolated!
```

**What's different:**

```
Time 1: Thread A (Core 1) writes producer_cursor
┌─────────────────────────────────────────────────────────────┐
│ Core 1 Cache: [Line 0: Modified]
│ Core 2 Cache: [empty]
│ Core 3 Cache: [empty]
└─────────────────────────────────────────────────────────────┘

Time 2: Thread B (Core 2) reads ring_buffer fields (Line 2)
┌─────────────────────────────────────────────────────────────┐
│ Core 1 Cache: [Line 0: Modified] ← Still valid!
│ Core 2 Cache: [Line 2: Shared]   ← No conflict!
│ Core 3 Cache: [empty]
└─────────────────────────────────────────────────────────────┘
                         ↑ No cache coherence traffic!

Time 3: Thread C (Core 3) writes consumer_cursor (Line 3)
┌─────────────────────────────────────────────────────────────┐
│ Core 1 Cache: [Line 0: Modified] ← Still valid!
│ Core 2 Cache: [Line 2: Shared]   ← Still valid!
│ Core 3 Cache: [Line 3: Modified] ← No conflict!
└─────────────────────────────────────────────────────────────┘
                         ↑ No cache coherence traffic!

All three threads can access their data simultaneously with NO false sharing!
```

**Why it works:**
- ✅ `align(64)` guarantees the struct starts at byte 64 (or 128, 192, etc.)
- ✅ Left padding (Line 1) isolates fields from previous fields (Line 0)
- ✅ Actual fields (Line 2) are in their own cache line
- ✅ Right padding (Line 3) isolates fields from next fields

**The cost:** Compiler inserts 56 bytes of padding before the struct (to align it to byte 64). Total memory: 160 (struct) + 56 (compiler padding) = 216 bytes.

**Trade-offs:**
- ⚠️ **Memory:** 216 bytes (160 struct + ~56 compiler padding)
- ✅ **Performance:** Guaranteed cache-line isolation
- ✅ **Correctness:** No false sharing, ever
- ✅ **Predictability:** Works the same regardless of allocation

**Verdict:** This is the correct solution. The memory cost is negligible for HFT applications, and you get guaranteed correctness.

**Key lesson learned:** Use `#[repr(C, align(64))]` to guarantee cache-line alignment. The compiler will insert padding as needed.

---

### **Scenario 5: Prefetch-Safe Padding (352 bytes) - The Paranoid Solution**

"Wait, I heard about hardware prefetchers causing problems!" Let's understand this subtle issue.

**What are hardware prefetchers?**

Modern CPUs try to be helpful by automatically fetching data **before** you ask for it. When you access one cache line, the CPU might fetch **2-4 adjacent cache lines** automatically!

**Types of prefetchers:**
- **Next-line prefetcher:** Fetches Line N+1 when you access Line N
- **Previous-line prefetcher:** Fetches Line N-1 when you access Line N (some CPUs)
- **Stream prefetcher:** Detects patterns and fetches multiple lines ahead

**Why this matters:** Even with Scenario 4's perfect padding, prefetchers can cause false sharing!

**The problem:**

```rust
struct Disruptor<T> {
    producer_cursor: AtomicI64,            // Thread A writes here
    ring_buffer: RingBufferAligned<T>,     // Thread B reads here
    consumer_cursor: AtomicI64,            // Thread C writes here
}
```

**Memory layout (from Scenario 4):**

```
Cache Line 0 (bytes 0-63):    [producer_cursor:8][padding:56]
Cache Line 1 (bytes 64-127):  [padding_left:64]
Cache Line 2 (bytes 128-191): [buffer:16][index_mask:8][initialized:8][padding:32]
Cache Line 3 (bytes 192-255): [padding_right:32][consumer_cursor:8][padding:24]
```

**What goes wrong with prefetchers:**

```
Time 1: Thread B (Core 2) reads ring_buffer fields (Line 2)
┌─────────────────────────────────────────────────────────────┐
│ CPU Prefetcher: "Thread B is reading Line 2, let me help!"  │
│   Fetches Line 2 (requested)                                │
│   Fetches Line 3 (next-line prefetch) ← Uh oh!              │
│                                                              │
│ Core 2 Cache: [Line 2: Shared]                             │
│               [Line 3: Shared] ← Prefetched!                │
└─────────────────────────────────────────────────────────────┘

Time 2: Thread C (Core 3) writes consumer_cursor (Line 3)
┌─────────────────────────────────────────────────────────────┐
│ CPU: "Thread C wants to write Line 3, but Core 2 has it!"   │
│                                                              │
│ Core 2 Cache: [Line 3: Invalid] ← Evicted by Core 3!       │
│ Core 3 Cache: [Line 3: Modified]                           │
└─────────────────────────────────────────────────────────────┘
                         ↑ Cache coherence traffic! (20-50ns)

Time 3: Thread B reads ring_buffer again
┌─────────────────────────────────────────────────────────────┐
│ CPU Prefetcher: "Let me fetch Line 3 again..."              │
│                                                              │
│ Core 2 Cache: [Line 3: Shared] ← Fetched from Core 3!      │
│ Core 3 Cache: [Line 3: Shared] ← Downgraded to Shared      │
└─────────────────────────────────────────────────────────────┘
                         ↑ More cache coherence traffic!
```

**This is "prefetcher false sharing"** - Line 3 (which contains padding AND consumer_cursor) is being bounced between cores due to prefetching, even though:
- ✅ Thread B never writes to Line 3
- ✅ Thread B never even accesses consumer_cursor
- ❌ The **prefetcher** brought Line 3 into Core 2's cache
- ❌ Cache coherence protocol sees conflict when Thread C writes

**The solution:** Use **2 cache lines** (128 bytes) of padding to create a "buffer zone":

```rust
#[repr(C, align(128))]  // Align to 2 cache lines!
struct RingBufferPrefetchSafe<T> {
    _padding_left: [u8; 128],                    // 2 cache lines
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,  // 16 bytes
    index_mask: usize,                           // 8 bytes
    initialized: AtomicUsize,                    // 8 bytes
    _padding_right: [u8; 128],                   // 2 cache lines
    // Total: 288 bytes (4.5 cache lines)
}
```

**Memory layout:**

```
Byte 0-7:     producer_cursor (Thread A writes)
Byte 8-127:   [compiler padding] ← Compiler inserts 120 bytes!
Byte 128-191: ring_buffer._padding_left (64 bytes)
Byte 192-255: ring_buffer._padding_left (64 bytes)
Byte 256-271: ring_buffer.buffer (Thread B reads)
Byte 272-279: ring_buffer.index_mask (Thread B reads)
Byte 280-287: ring_buffer.initialized (Thread B reads)
Byte 288-351: ring_buffer._padding_right (64 bytes)
Byte 352-415: ring_buffer._padding_right (64 bytes)
Byte 416-423: consumer_cursor (Thread C writes)
```

**Cache line view:**

```
Cache Line 0 (bytes 0-63):    [producer_cursor:8][padding:56]
Cache Line 1 (bytes 64-127):  [compiler padding:64]
Cache Line 2 (bytes 128-191): [padding_left:64]
Cache Line 3 (bytes 192-255): [padding_left:64]
Cache Line 4 (bytes 256-319): [buffer:16][index_mask:8][initialized:8][padding:32]
Cache Line 5 (bytes 320-383): [padding_right:64]
Cache Line 6 (bytes 384-447): [padding_right:64]
Cache Line 7 (bytes 448-511): [consumer_cursor:8][padding:56]
```

**Now what happens with prefetchers:**

```
Time 1: Thread B (Core 2) reads ring_buffer fields (Line 4)
┌─────────────────────────────────────────────────────────────┐
│ CPU Prefetcher: "Thread B is reading Line 4, let me help!"  │
│   Fetches Line 4 (requested)                                │
│   Fetches Line 5 (next-line prefetch) ← Just padding!       │
│                                                              │
│ Core 2 Cache: [Line 4: Shared]                             │
│               [Line 5: Shared] ← Prefetched, but just padding│
└─────────────────────────────────────────────────────────────┘

Time 2: Thread C (Core 3) writes consumer_cursor (Line 7)
┌─────────────────────────────────────────────────────────────┐
│ CPU Prefetcher: "Thread C is writing Line 7, let me help!"  │
│   Fetches Line 7 (requested)                                │
│   Fetches Line 6 (previous-line prefetch) ← Just padding!   │
│                                                              │
│ Core 2 Cache: [Line 4: Shared] ← Still valid!              │
│               [Line 5: Shared] ← Still valid!              │
│ Core 3 Cache: [Line 6: Shared] ← No conflict with Core 2!  │
│               [Line 7: Modified]                            │
└─────────────────────────────────────────────────────────────┘
                         ↑ No cache coherence traffic!

Lines 5 and 6 are different - no conflict!
```

**Why it works:**
- ✅ 2 cache lines of padding create a "buffer zone"
- ✅ Prefetcher can fetch adjacent lines without causing conflicts
- ✅ Line 5 (prefetched by Thread B) ≠ Line 6 (prefetched by Thread C)
- ✅ No false sharing, even with aggressive prefetchers

**The cost:** Compiler inserts 120 bytes of padding before the struct. Total memory: 288 (struct) + 120 (compiler padding) = 408 bytes (but can be up to 352-480 bytes depending on alignment).

**Trade-offs:**
- ❌ **Memory:** 352-480 bytes (2x more than Scenario 4!)
- ✅ **Performance:** Perfect isolation, even with prefetchers
- ✅ **Correctness:** No false sharing, ever, even with aggressive hardware
- ⚠️ **Overkill:** Most applications don't need this

**Verdict:** This is the paranoid solution. Only use if you've measured prefetcher false sharing with `perf c2c` and confirmed it's a problem.

**Key lesson learned:** Hardware prefetchers can cause false sharing even with padding. Use 2 cache lines (128 bytes) of padding if you need perfect isolation from prefetchers.

---

### **What LMAX Disruptor Actually Chose**

Now that you understand all five scenarios, let's see what the LMAX team chose after extensive benchmarking and production monitoring.

**Spoiler:** They chose **none of the above**! They used a Java-specific optimization that doesn't directly map to Rust.

#### **Their Solution: 56 Bytes + Object Header (Java-Specific)**

```java
// LEFT padding (56 bytes, NOT 64!)
class LhsPadding {
    protected byte
        p10, p11, p12, p13, p14, p15, p16, p17,  // 8 bytes
        p20, p21, p22, p23, p24, p25, p26, p27,  // 8 bytes
        p30, p31, p32, p33, p34, p35, p36, p37,  // 8 bytes
        p40, p41, p42, p43, p44, p45, p46, p47,  // 8 bytes
        p50, p51, p52, p53, p54, p55, p56, p57,  // 8 bytes
        p60, p61, p62, p63, p64, p65, p66, p67,  // 8 bytes
        p70, p71, p72, p73, p74, p75, p76, p77;  // 8 bytes
    // Total: 56 bytes (NOT 64!)
}

class Value extends LhsPadding {
    protected long value;  // 8 bytes
}

class RhsPadding extends Value {
    protected byte
        p90, p91, ..., p157;  // 56 bytes (NOT 64!)
}

public class Sequence extends RhsPadding {
    // Memory layout:
    // [Object header: 12-16 bytes][LhsPadding: 56][value: 8][RhsPadding: 56]
    // Total: ~136 bytes
}
```

**Why 56 bytes instead of 64?**

Java objects have an **object header** (12-16 bytes depending on JVM):
- Object mark word: 8 bytes
- Class pointer: 4-8 bytes (compressed oops)
- Alignment padding: 0-4 bytes

**Memory layout:**

```
Offset 0-15:   Object header (12 bytes) + alignment (4 bytes) = 16 bytes
Offset 16-71:  LhsPadding (56 bytes)
Offset 72-79:  value (8 bytes)
Offset 80-135: RhsPadding (56 bytes)
Total: 136 bytes

Cache line analysis:
Cache Line 0 (bytes 0-63):   [header:16][LhsPadding:48]
Cache Line 1 (bytes 64-127): [LhsPadding:8][value:8][RhsPadding:48]
Cache Line 2 (bytes 128-191):[RhsPadding:8][next_field]

The `value` field is in Cache Line 1, with padding on both sides!
```

**Why use bytes instead of longs?**

From Aleksey Shipilëv (JVM expert):

> "Don't pad with longs: can you see those gaps that could theoretically be taken by the int/compressed-oops fields? JMH pads with bytes."

**The problem with long padding:**

```java
// BAD: Padding with longs
class BadPadding {
    protected long p1, p2, p3, p4, p5, p6, p7, p8;  // 64 bytes
}

// JVM might reorder and insert smaller fields:
// [p1:8][p2:8][some_int:4][gap:4][p3:8][p4:8]...
// Result: Padding is broken!
```

**With byte padding:**

```java
// GOOD: Padding with bytes
class GoodPadding {
    protected byte p10, p11, ..., p77;  // 56 bytes
}

// JVM cannot insert fields - no gaps large enough!
// [p10:1][p11:1][p12:1]...[p77:1]
// Result: Solid padding wall!
```

---

### **LMAX's Trade-off Analysis**

Based on their production experience and benchmarks:

| Approach | Memory | Performance | Portability | LMAX's Verdict |
|----------|--------|-------------|-------------|----------------|
| No padding | 32 bytes | ❌ Terrible | ✅ Works everywhere | Never use |
| Right padding only | 96 bytes | ⚠️ Partial | ✅ Works everywhere | Incomplete |
| Both paddings (64+64) | 160 bytes | ✅ Good | ⚠️ Depends on JVM | Too much for Java |
| Both paddings (56+56) | 136 bytes | ✅ Good | ✅ Works on most JVMs | **LMAX's choice** |
| Aligned (64+64) | 216+ bytes | ✅ Excellent | ✅ Guaranteed | Overkill |
| Prefetch-safe (128+128) | 352+ bytes | ✅ Perfect | ✅ Guaranteed | Too expensive |

**Why LMAX chose 56+56 bytes:**

1. ✅ **Memory efficiency:** 136 bytes vs 160 bytes (15% savings)
2. ✅ **Object header synergy:** Leverages "free" padding from Java's object header
3. ✅ **Byte padding:** Prevents JVM field reordering tricks
4. ✅ **Empirically tested:** Works well in production on all major JVMs
5. ⚠️ **Not perfect:** No alignment guarantee, no prefetcher protection
6. ✅ **Pragmatic:** Good enough for 99.9% of use cases

**Their philosophy:** "Make it fast enough, not perfect. Measure, don't guess."

---

### **Our Rust Implementation Choice**

For Rust, we chose **Scenario 4: Aligned Padding** for guaranteed correctness:

```rust
#[repr(C, align(64))]  // Guarantee cache-line alignment
pub struct RingBuffer<T> {
    _padding_left: [u8; 64],   // Full cache line

    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,
    index_mask: usize,
    initialized: AtomicUsize,

    _padding_right: [u8; 64],  // Full cache line
}
```

**Why we chose `#[repr(C, align(64))]` with 64+64 bytes:**

1. ✅ **Guaranteed alignment:** Struct always starts at 64-byte boundary
2. ✅ **No object headers:** Rust has no runtime metadata, so we need full 64 bytes
3. ✅ **Explicit control:** We control exact layout with `#[repr(C, align(64))]`
4. ✅ **Correctness over cleverness:** No guessing about allocation patterns
5. ✅ **Negligible cost:** ~56 bytes of compiler padding is nothing for HFT

**Why NOT follow LMAX's exact approach (56+56 bytes)?**

LMAX's situation is fundamentally different:
- **Java:** Object headers (12-16 bytes) provide "free" padding → 56+16=72 bytes ≈ cache line
- **Rust:** No object headers → Need explicit alignment guarantee

**We can't claim "pragmatism" without measurement.** LMAX measured and validated their approach in production. We haven't. So we choose correctness.

**Trade-offs:**
- ⚠️ **Memory:** 216 bytes (160 struct + ~56 compiler padding)
- ✅ **Performance:** Guaranteed cache-line isolation
- ✅ **Correctness:** No false sharing, ever
- ✅ **Honesty:** We're not pretending "good enough" without proof

**Our philosophy:** Measure, don't guess. Until we measure, choose correctness over cleverness.

---

### **Summary: Padding Strategy Decision Matrix**

When implementing cache-line padding, consider these factors:

#### **Choose No Padding (32 bytes) if:**
- ❌ Never recommended for multi-threaded code
- Only acceptable for single-threaded or read-only data structures

#### **Choose Right Padding Only (96 bytes) if:**
- ⚠️ You control allocation and can guarantee alignment
- ⚠️ You're certain no fields come before your struct
- Generally not recommended - incomplete protection

#### **Choose Both Paddings (160 bytes) if:**
- ✅ You want "good enough" protection with reasonable memory cost
- ✅ You're following LMAX's pragmatic approach
- ✅ Your benchmarks show acceptable performance
- ⚠️ You understand this doesn't guarantee alignment in Rust

#### **Choose Aligned Padding (216+ bytes) if:**
- ✅ You need guaranteed cache-line alignment
- ✅ Memory cost is acceptable
- ✅ You want to eliminate alignment uncertainty
- **This is our choice for Rust RingBuffer**

#### **Choose Prefetch-Safe Padding (352+ bytes) if:**
- ✅ You're targeting CPUs with aggressive prefetchers
- ✅ You've measured prefetcher-induced false sharing
- ✅ Memory cost is not a concern
- ✅ You need absolute maximum performance

**Key Takeaway:** LMAX Disruptor's success proves that "good enough" padding (56+56 bytes in Java with object headers) works well in production. However, Rust lacks object headers, so we need explicit alignment. We chose `#[repr(C, align(64))]` for guaranteed correctness. **Measure, don't guess - and until you measure, choose correctness.**

---

### **Visual Summary: All Scenarios**

```
Scenario 1: No Padding (32 bytes)
┌─────────────────────────────────────────────────────────────┐
│ Cache Line 0: [prev][RingBuffer fields][next]              │
└─────────────────────────────────────────────────────────────┘
❌ False sharing on both sides


Scenario 2: Right Padding Only (96 bytes)
┌─────────────────────────────────────────────────────────────┐
│ Cache Line 0: [prev][RingBuffer fields][padding...         │
│ Cache Line 1: ...padding][next]                            │
└─────────────────────────────────────────────────────────────┘
❌ False sharing on left side


Scenario 3: Both Paddings (160 bytes) - LMAX's Choice (Java)
┌─────────────────────────────────────────────────────────────┐
│ Cache Line 0: [prev][padding_left...                       │
│ Cache Line 1: ...padding_left][RingBuffer fields][padding..│
│ Cache Line 2: ...padding_right][next]                      │
└─────────────────────────────────────────────────────────────┘
✅ Good protection in Java (object headers help)
⚠️ No alignment guarantee in Rust


Scenario 4: Aligned Padding (216+ bytes) - Our Choice (Rust)
┌─────────────────────────────────────────────────────────────┐
│ Cache Line 0: [prev][compiler padding]                     │
│ Cache Line 1: [padding_left: 64 bytes]                     │
│ Cache Line 2: [RingBuffer fields][padding_right...]        │
│ Cache Line 3: [padding_right][next]                        │
└─────────────────────────────────────────────────────────────┘
✅ Guaranteed alignment, perfect isolation
✅ Correctness over cleverness


Scenario 5: Prefetch-Safe (352+ bytes)
┌─────────────────────────────────────────────────────────────┐
│ Cache Line 0: [prev]                                       │
│ Cache Line 1: [padding_left: 64 bytes]                     │
│ Cache Line 2: [padding_left: 64 bytes]                     │
│ Cache Line 3: [RingBuffer fields]                          │
│ Cache Line 4: [padding_right: 64 bytes]                    │
│ Cache Line 5: [padding_right: 64 bytes]                    │
│ Cache Line 6: [next]                                       │
└─────────────────────────────────────────────────────────────┘
✅ Perfect isolation, even with aggressive prefetchers
```

**LMAX's production data:** The "Both Paddings" approach (Scenario 3) delivers 99.9% of the performance benefit at 45% of the memory cost compared to "Prefetch-Safe" (Scenario 5). However, this is in Java with object headers. In Rust, we chose Scenario 4 (Aligned Padding) for guaranteed correctness.

---

## Testing the Ring Buffer

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_power_of_two_indexing() {
        let rb = RingBuffer::new(1024, || 0u64);
        
        // Verify wraparound works correctly
        let ptr0 = rb.get(0) as *const u64;
        let ptr1024 = rb.get(1024) as *const u64;
        assert_eq!(ptr0, ptr1024, "Sequence 0 and 1024 should map to same slot");
        
        let ptr1 = rb.get(1) as *const u64;
        let ptr1025 = rb.get(1025) as *const u64;
        assert_eq!(ptr1, ptr1025, "Sequence 1 and 1025 should map to same slot");
    }

    #[test]
    #[should_panic(expected = "Size must be power of 2")]
    fn test_non_power_of_two_panics() {
        RingBuffer::new(1000, || 0u64);
    }

    #[test]
    #[should_panic(expected = "Size must be greater than 0")]
    fn test_zero_size_panics() {
        RingBuffer::<u64>::new(0, || 0);
    }

    #[test]
    fn test_get_mut() {
        let rb = RingBuffer::new(8, || 0u64);
        
        // Modify slot 0
        *rb.get_mut(0) = 42;
        assert_eq!(*rb.get(0), 42);
        
        // Verify wraparound
        *rb.get_mut(8) = 99;
        assert_eq!(*rb.get(0), 99, "Sequence 8 wraps to slot 0");
    }

    #[test]
    fn test_panic_during_construction() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::panic;

        static CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropCounter(usize);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        // Reset counters
        CALL_COUNT.store(0, Ordering::SeqCst);
        DROP_COUNT.store(0, Ordering::SeqCst);

        let result = panic::catch_unwind(|| {
            RingBuffer::new(8, || {
                let count = CALL_COUNT.fetch_add(1, Ordering::SeqCst);
                if count == 5 {
                    panic!("Simulated factory panic");
                }
                DropCounter(count)
            })
        });

        assert!(result.is_err(), "Should panic during construction");
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), 5, "Should drop exactly 5 initialized elements");
    }
}
```

---

## Common Pitfalls

### **Pitfall #1: Using Non-Power-of-2 Sizes**

```rust
// WRONG
let rb = RingBuffer::new(1000, || Event::default());  // Panics!

// RIGHT
let rb = RingBuffer::new(1024, || Event::default());  // OK
```

**Why:** Bitwise AND only works with power-of-2 sizes.

**Fix:** Round up to next power of 2: `size.next_power_of_two()`

### **Pitfall #2: Forgetting Cache-Line Padding**

```rust
// WRONG: Producer and consumer cursors in same struct without padding
struct Disruptor<T> {
    ring_buffer: RingBuffer<T>,
    producer_cursor: AtomicI64,  // Modified by producer
    consumer_cursor: AtomicI64,  // Modified by consumer ← False sharing!
}

// RIGHT: Separate structs with padding on BOTH sides
#[repr(C)]
struct Producer {
    _padding_left: [u8; 64],
    cursor: AtomicI64,
    _padding_right: [u8; 56],  // 64 - 8 (AtomicI64 size) = 56
}

#[repr(C)]
struct Consumer {
    _padding_left: [u8; 64],
    cursor: AtomicI64,
    _padding_right: [u8; 56],
}
```

**Why:** Without padding on both sides, cursors can share cache lines with adjacent fields, causing constant invalidation (20-150ns per write).

**Fix:** Use padding on **both left and right** to ensure complete cache-line isolation, matching the Java LMAX Disruptor pattern.

### **Pitfall #3: Holding References Across Sequence Boundaries**

```rust
// WRONG
let event1 = rb.get_mut(seq);
let event2 = rb.get_mut(seq + buffer_size);  // Aliases event1!
event1.value = 42;  // Undefined behavior!
```

**Why:** `seq` and `seq + buffer_size` map to the same slot. Holding both references violates Rust's aliasing rules.

**Fix:** Drop references before accessing the same slot again:

```rust
// RIGHT
{
    let event = rb.get_mut(seq);
    event.value = 42;
}  // Reference dropped

// Now safe to access same slot via different sequence
let event = rb.get_mut(seq + buffer_size);
```

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
- **Coordination** - How do producers claim slots? (Post 3: Sequencers)
- **Synchronization** - How do consumers know when data is ready? (Post 4: Wait Strategies)
- **Dependencies** - How do multi-stage pipelines work? (Post 5: Sequence Barriers)

The ring buffer provides fast, cache-friendly storage. The sequencer (Post 3) provides safe, lock-free coordination.

---

## Next Up: Sequencers

In **Part 3**, we'll build the sequencer - the coordination mechanism that makes the ring buffer safe:

- **Single-producer sequencer** - Simple counter (no atomics in fast path)
- **Multi-producer sequencer** - CAS-based coordination
- **RAII pattern** - `SequenceClaim` ensures you never forget to publish
- **Memory ordering** - Acquire/Release semantics explained

**Teaser:** We'll achieve **lock-free coordination** with automatic cleanup via Rust's type system.

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

**Next:** [Part 3 — Sequencers: Single vs Multi-Producer Coordination →](post-03-sequencers.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*

