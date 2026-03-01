# Building a Disruptor in Rust: Ryuo — Part 2B: Cache-Line Padding

**Series:** Building a Disruptor in Rust: Ryuo  
**Part:** 2B of 16  
**Target Audience:** Systems engineers building high-performance, low-latency applications  
**Prerequisites:** Part 2A (ring buffer implementation), basic knowledge of CPU cache architecture  

---

## Why Cache-Line Padding Matters

In Part 2A, we built a pre-allocated, power-of-2, cache-aligned ring buffer. You may have noticed the `_padding_left` and `_padding_right` fields and the `#[repr(C, align(64))]` attribute — but we didn't explain *why* they're essential.

In this post, we'll discover the problem through concrete scenarios, building from naive to perfect solutions. By the end, you'll understand not just **what** to do, but **why** each decision matters.

We'll explore four progressively better Rust implementations, see exactly what goes wrong with each, and then compare our choice against LMAX Disruptor's Java-specific approach.

---

## The Four Scenarios

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
│ [consumer_cursor:8][unused:16]                             │
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
│ [producer_cursor:8][buffer:16][index_mask:8][initialized:8] │
│ [padding_right:24]                                          │
└─────────────────────────────────────────────────────────────┘
  ↑ Thread A writes    ↑ Thread B reads

  STILL IN THE SAME CACHE LINE!

Cache Line 1 (bytes 64-127):
┌─────────────────────────────────────────────────────────────┐
│ [padding_right:40][consumer_cursor:8][unused:16]            │
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
    // Total: 160 bytes data (no alignment guarantee on struct start)
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
│ [producer_cursor:8][padding_left:56]                       │
└─────────────────────────────────────────────────────────────┘
  ↑ Thread A writes - isolated!

Cache Line 1 (bytes 64-127):
┌─────────────────────────────────────────────────────────────┐
│ [padding_left:8][buffer:16][index_mask:8][initialized:8]   │
│ [padding_right:24]                                          │
└─────────────────────────────────────────────────────────────┘
                    ↑ Thread B reads - mostly isolated!

Cache Line 2 (bytes 128-191):
┌─────────────────────────────────────────────────────────────┐
│ [padding_right:40][consumer_cursor:8][unused:16]           │
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

### **Scenario 4: Aligned Padding (192 bytes) - The Correct Solution**

"I need to **guarantee** the struct starts at a cache-line boundary!" Here's how:

```rust
#[repr(C, align(64))]  // ← Force 64-byte alignment!
struct RingBufferAligned<T> {
    _padding_left: [u8; 64],                     // 64 bytes
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,  // 16 bytes
    index_mask: usize,                           // 8 bytes
    initialized: AtomicUsize,                    // 8 bytes
    _padding_right: [u8; 64],                    // 64 bytes
    // Data: 160 bytes, but align(64) pads to 192 bytes (3 cache lines)
}
```

**The key:** `align(64)` tells the compiler "this struct MUST start at a 64-byte boundary." The struct size is also rounded up to a multiple of 64 (from 160 to 192 bytes).

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
Byte 224-255: [alignment tail padding] ← Compiler pads to 192 bytes (align 64)
Byte 256-263: consumer_cursor (Thread C writes)
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
│ [padding_right:32][tail_padding:32]                        │
└─────────────────────────────────────────────────────────────┘
  ↑ Sacrificial padding (padding_right + alignment tail)

Cache Line 4 (bytes 256-319):
┌─────────────────────────────────────────────────────────────┐
│ [consumer_cursor:8][unused:56]                             │
└─────────────────────────────────────────────────────────────┘
  ↑ Thread C writes - isolated!
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

Time 3: Thread C (Core 3) writes consumer_cursor (Line 4)
┌─────────────────────────────────────────────────────────────┐
│ Core 1 Cache: [Line 0: Modified] ← Still valid!
│ Core 2 Cache: [Line 2: Shared]   ← Still valid!
│ Core 3 Cache: [Line 4: Modified] ← No conflict!
└─────────────────────────────────────────────────────────────┘
                         ↑ No cache coherence traffic!

All three threads can access their data simultaneously with NO false sharing!
```

**Why it works:**
- ✅ `align(64)` guarantees the struct starts at any multiple of 64 (0, 64, 128, etc.)
- ✅ Left padding (Line 1) isolates fields from previous fields (Line 0)
- ✅ Actual fields (Line 2) are in their own cache line
- ✅ Right padding + tail padding (Line 3) isolates fields from next fields
- ✅ Consumer cursor (Line 4) is completely isolated

**The cost:** The struct itself is 192 bytes (`std::mem::size_of`, including 32 bytes of alignment tail padding). When embedded as a field after a smaller type, the compiler may also insert up to 56 bytes of alignment padding *before* the struct.

**Trade-offs:**
- ⚠️ **Memory:** 192 bytes (+ up to 56 bytes external alignment padding when embedded)
- ✅ **Performance:** Guaranteed cache-line isolation
- ✅ **Correctness:** No false sharing, ever
- ✅ **Predictability:** Works the same regardless of allocation

**Verdict:** This is the correct solution. The memory cost is negligible for HFT applications, and you get guaranteed correctness.

**Key lesson learned:** Use `#[repr(C, align(64))]` to guarantee cache-line alignment. The compiler will insert padding as needed.

---

### **What LMAX Disruptor Actually Chose**

Now that you understand all four scenarios, let's see what the LMAX team chose after extensive benchmarking and production monitoring.

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
| Both paddings (64+64) | 160 bytes | ✅ Good | ⚠️ Depends on JVM | Redundant with object headers |
| Both paddings (56+56) | 136 bytes | ✅ Good | ✅ Works on most JVMs | **LMAX's choice** |
| Aligned (64+64) | 192 bytes | ✅ Excellent | ✅ Guaranteed | **Our Rust choice** |

**Why LMAX chose 56+56 bytes:**

1. ✅ **Memory efficiency:** 136 bytes vs 160 bytes (15% savings)
2. ✅ **Object header synergy:** Leverages "free" padding from Java's object header
3. ✅ **Byte padding:** Prevents JVM field reordering tricks
4. ✅ **Empirically tested:** Works well in production on all major JVMs
5. ⚠️ **Not perfect:** No alignment guarantee
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
- ⚠️ **Memory:** 192 bytes (+ up to 56 bytes external alignment padding when embedded)
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

#### **Choose Aligned Padding (192 bytes) if:**
- ✅ You need guaranteed cache-line alignment
- ✅ Memory cost is acceptable
- ✅ You want to eliminate alignment uncertainty
- **This is our choice for Rust RingBuffer**

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


Scenario 3: Both Paddings (160 bytes) - Closest to LMAX (Java uses 56+56)
┌─────────────────────────────────────────────────────────────┐
│ Cache Line 0: [prev][padding_left...                       │
│ Cache Line 1: ...padding_left][RingBuffer fields][padding..│
│ Cache Line 2: ...padding_right][next]                      │
└─────────────────────────────────────────────────────────────┘
✅ Good protection in Java (object headers help)
⚠️ No alignment guarantee in Rust


Scenario 4: Aligned Padding (192 bytes) - Our Choice (Rust)
┌─────────────────────────────────────────────────────────────┐
│ Cache Line 0: [prev][compiler padding]                     │
│ Cache Line 1: [padding_left: 64 bytes]                     │
│ Cache Line 2: [RingBuffer fields][padding_right...]        │
│ Cache Line 3: [padding_right + tail padding]               │
│ Cache Line 4: [next]                                       │
└─────────────────────────────────────────────────────────────┘
✅ Guaranteed alignment, perfect isolation
✅ Correctness over cleverness
```

**LMAX's production data:** The "Both Paddings" approach (Scenario 3) delivers nearly all of the performance benefit. However, this is in Java with object headers. In Rust, we chose Scenario 4 (Aligned Padding) for guaranteed correctness.

---

## What We Haven't Covered

The ring buffer (Part 2A) and cache-line padding (this post) give us fast, isolated storage. But we still need:
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
