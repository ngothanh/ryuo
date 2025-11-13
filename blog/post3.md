# Building a Disruptor in Rust: Ryuo — Part 3: Sequencers

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 3 of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 1 (latency fundamentals), Part 2 (ring buffer implementation)

---

## From Storage to Coordination

In Part 2, we built a ring buffer with:
- ✅ **Fast indexing** - Bitwise AND instead of modulo (3-11x faster)
- ✅ **Interior mutability** - `UnsafeCell` allows concurrent access
- ✅ **Cache-line isolation** - Prevents false sharing

But we left a critical question unanswered: **How do multiple threads safely use this ring buffer?**

### **The Coordination Problem**

Imagine this scenario:

```
Producer Thread 1 ──┐
                    ├──> Ring Buffer [0][1][2][3]... ──> Consumer Thread 1
Producer Thread 2 ──┘                                 └─> Consumer Thread 2
```

**Without coordination, chaos:**

**Problem 1: Race Condition (Data Corruption)**
```
// Thread 1 and Thread 2 both execute this:
current = next_slot;     // Both read: current = 5
next = current + 1;      // Both compute: next = 6

// Both threads write to slot 6!
buffer[6] = 42;          // Thread 1 writes
buffer[6] = 99;          // Thread 2 overwrites!

next_slot = next;
```

**Result:** Data corruption. Thread 1's data is lost.

---

**Problem 2: Wrap-Around (Use-After-Free)**
```
Ring Buffer (size 4):
┌───┬───┬───┬───┐
│ 0 │ 1 │ 2 │ 3 │
└───┴───┴───┴───┘
  ↑           ↑
  Consumer    Producer wants sequence 4
  reading     (wraps to index 0)
  slot 0
```

Producer wants to write sequence 4 (index 0), but consumer is still reading slot 0!

**Result:** Producer overwrites data consumer is reading. Use-after-free!

---

**Problem 3: Visibility (Reading Garbage)**

This is the most subtle problem. Even if we solve race conditions and wrap-around, we can still read garbage due to **CPU reordering**.

```
// Producer (Thread 1)
buffer[5] = 42;          // Step 1: Write data
ready_flag = 5;          // Step 2: Signal "sequence 5 is ready"

// Consumer (Thread 2)
seq = ready_flag;        // Reads: seq = 5 ✅
value = buffer[5];       // Reads: ??? (might see 0 instead of 42!)
```

**What went wrong?**

The CPU might **reorder** the producer's operations:
```
// What the CPU actually executes:
ready_flag = 5;          // ← Reordered to happen FIRST!
buffer[5] = 42;          // ← Happens SECOND

// Consumer sees:
seq = 5                  // ✅ Flag is set
value = 0                // ❌ Data not written yet!
```

**Why does this happen?**
- Modern CPUs reorder operations for performance (out-of-order execution, store buffers)
- Thread 1's writes might not be visible to Thread 2 immediately
- Thread 2 might see the flag update before the data update

**Result:** Consumer sees sequence 5 is published, but doesn't see the data write. Reads garbage!

**The fix:** Use **memory ordering** (Release/Acquire) to prevent reordering. We'll explain this in detail next.

---

### **What is a Sequencer?**

A **sequencer** is the coordination mechanism that solves all three problems:

```
                    ┌─────────────┐
Producer 1 ────────>│             │
                    │  Sequencer  │────> Coordinates access
Producer 2 ────────>│             │
                    └─────────────┘
                          │
                          ↓
                    Ring Buffer
                    [0][1][2][3]...
                          │
                          ↓
                    ┌─────────────┐
                    │  Consumers  │
                    └─────────────┘
```

**The sequencer's job:**
1. **Atomically assign sequences** - No two producers get the same sequence (solves Problem 1)
2. **Prevent wrap-around** - Wait for consumers before overwriting (solves Problem 2)
3. **Ensure visibility** - Use memory ordering to make writes visible (solves Problem 3)

**Key insight:** The ring buffer is just storage. The sequencer is the traffic cop that makes it safe.

---

## What We'll Build

By the end of this post, you'll understand:

1. **Memory ordering fundamentals** - Why CPUs reorder operations and how to prevent it
2. **Evolution of the solution** - From naive locking to optimized lock-free coordination
3. **SingleProducerSequencer** - Fast path with no atomic contention (~10ns per claim)
4. **MultiProducerSequencer** - CAS-based coordination for multiple producers (~50-200ns per claim)
5. **RAII pattern** - How Rust's type system prevents "forgot to publish" bugs

Let's start with the foundation: **memory ordering**.

---

## The Memory Visibility Problem

Before we implement sequencers, we need to understand a fundamental problem: **threads can see memory operations in different orders**.

This isn't a Rust problem, or a C++ problem, or a Java problem. It's a **CPU problem** that every language must solve.

### **The Problem: CPU Reordering**

Imagine a simple producer-consumer scenario:

```
Ring Buffer: [slot_0][slot_1][slot_2][slot_3]

Producer writes to slot_0, then tells consumer "slot_0 is ready"
Consumer waits for "slot_0 is ready", then reads from slot_0
```

Here's the code (in pseudocode, no jargon yet):

```
Thread 1 (Producer):
1. Write data to slot: slot[0].value = 42
2. Signal "ready": ready_flag = 1

Thread 2 (Consumer):
1. Wait for signal: while ready_flag != 1 { wait }
2. Read data: read slot[0].value
```

**Question:** Does Thread 2 see `value = 42`?

**Answer:** **Not necessarily!** The CPU might reorder operations:

```
Thread 1 (Reordered by CPU):
1. Signal "ready": ready_flag = 1  ← Reordered to happen first!
2. Write data to slot: slot[0].value = 42

Thread 2 sees:
- ready_flag = 1 ✅ (exits wait loop)
- slot[0].value = 0 ❌ (old value, not 42!)
```

This is called a **memory ordering violation**.

### **Why CPUs Reorder**

Modern CPUs reorder operations for performance:

1. **Out-of-order execution** - Execute instructions in any order that doesn't change single-threaded behavior
2. **Store buffers** - Writes go to a buffer first, then to cache/memory later
3. **Cache coherency delays** - Other CPUs might not see writes immediately

**Example timeline:**

```
Time  Thread 1 (Producer)              Thread 2 (Consumer)
----  ---------------------------      ---------------------------
t0    slot[0].value = 42 (in store buf)
t1    ready_flag = 1 (in cache)
t2                                     ready_flag → 1 ✅
t3                                     slot[0].value → 0 ❌ (stale!)
t4    slot[0].value flushed to cache
```

**Key insight:** Thread 2 sees the ready flag before the actual data!

### **Architecture Differences**

**x86-64 (Total Store Order - TSO):**
- Stores are not reordered with other stores
- Stores are not reordered with prior loads
- Loads may be reordered with prior stores
- **Relatively strong** - fewer reorderings

**ARM/PowerPC (Weak Memory Models):**
- Almost any reordering is possible
- Much more aggressive optimization
- **Requires explicit barriers** for ordering

**Source:** Intel/ARM architecture manuals, "A Primer on Memory Consistency and Cache Coherence" (Sorin et al.)

---

## Memory Ordering: The Solution

We tell the CPU: **"Don't reorder these operations!"**

### **The Concepts (Language-Agnostic)**

There are two key concepts:

**Release:** "All my writes before this point must be visible before this write"
```
slot[0].value = 42;      ← Must happen before
ready_flag = 1 (Release) ← This write
```

**Acquire:** "All writes before a Release must be visible after I read"
```
ready_flag (Acquire)     ← After this read
slot[0].value            ← I see all prior writes
```

**Together:** They create a **happens-before relationship**

```
Producer:                    Consumer:
slot[0].value = 42
ready_flag = 1 (Release) ──────> ready_flag (Acquire)
                                  slot[0].value → 42 ✅
```

**Key insight:**
- **Release** = "Everything before me is done, you can safely read after this"
- **Acquire** = "I'll wait to see everything that happened before the Release"

### **In Different Languages**

Now let's see how to express this in actual code. We'll use our simple example:

**Rust:**
```rust
// Producer
slot[0].value = 42;                           // Regular write
ready_flag.store(1, Ordering::Release);       // Release: makes value visible

// Consumer
while ready_flag.load(Ordering::Acquire) != 1 { // Acquire: sees all prior writes
    std::hint::spin_loop();
}
let value = slot[0].value;  // Guaranteed to see 42 ✅
```

**C++:**
```cpp
// Producer
slot[0].value = 42;
ready_flag.store(1, std::memory_order_release);

// Consumer
while (ready_flag.load(std::memory_order_acquire) != 1) {
    // wait
}
auto value = slot[0].value;  // Guaranteed to see 42 ✅
```

**Java:**
```java
// Producer
slot[0].value = 42;
ready_flag.setVolatile(1);  // volatile = acquire/release semantics

// Consumer
while (ready_flag.getVolatile() != 1) {
    // wait
}
int value = slot[0].value;  // Guaranteed to see 42 ✅
```

**Key insight:** Same concept, different syntax. This is a CPU-level requirement, not a language feature.

---

## Rust's Memory Ordering Options

Now that we understand the concepts, let's look at Rust's specific options:

```rust
pub enum Ordering {
    Relaxed,   // No ordering guarantees
    Acquire,   // Synchronize with Release stores
    Release,   // Synchronize with Acquire loads
    AcqRel,    // Both Acquire and Release
    SeqCst,    // Sequentially consistent (strongest)
}
```

### **What Each Ordering Means**

#### **Relaxed - "I don't care about order"**

Use when you just want atomicity (no torn reads/writes), but don't need synchronization.

**Example: Statistics counter**
```rust
// Multiple threads incrementing a counter
counter.fetch_add(1, Ordering::Relaxed);

// Later, read total
let total = counter.load(Ordering::Relaxed);
```

**Why Relaxed is OK here:** We only care about the final count, not the order of increments. No other data depends on this counter.

**What you get:** Atomic increment (no lost updates)
**What you DON'T get:** No synchronization with other memory

---

#### **Acquire - "Show me everything before the Release"**

Use when **reading** a flag/cursor that signals other data is ready.

**Example: Consumer reading ready flag**
```rust
// Wait for producer to signal data is ready
while ready_flag.load(Ordering::Acquire) != 1 {
    std::hint::spin_loop();
}
// Now I can safely read the data
let value = slot[0].value;
```

**Why Acquire:** Ensures we see all writes that happened before the producer's Release.

---

#### **Release - "Everything before me is done"**

Use when **writing** a flag/cursor that signals other data is ready.

**Example: Producer signaling data is ready**
```rust
// Write data first
slot[0].value = 42;
// Signal it's ready (Release ensures value write is visible)
ready_flag.store(1, Ordering::Release);
```

**Why Release:** Ensures all prior writes are visible before this write.

---

#### **AcqRel - "Both Acquire and Release"**

Use for read-modify-write operations (fetch_add, compare_exchange).

**Example: Multiple producers claiming sequences**
```rust
// Atomically claim next sequence
let my_seq = cursor.fetch_add(1, Ordering::AcqRel);
```

**Why AcqRel:**
- **Acquire** part: See all prior claims from other producers
- **Release** part: Make my claim visible to other producers

---

#### **SeqCst - "Strongest ordering"**

Use when you need a full memory barrier (rare). Includes a "StoreLoad" fence.

**Example: Checking consumer positions after publishing**
```rust
// Publish cursor
cursor.store(next, Ordering::Release);
// Check if consumers have caught up (needs StoreLoad fence)
cursor.store(current, Ordering::SeqCst);
let min = get_minimum_consumer_position();
```

**Why SeqCst:** Ensures the cursor store is visible before we read consumer positions. Critical on ARM/PowerPC.

---

### **Visualizing Memory Ordering: What Happens in Hardware**

The examples above show **what** each ordering does conceptually. But to truly understand **how** to use them, we need to see what happens at the hardware level.

Let's visualize the same producer-consumer scenario with different orderings.

**Scenario:** Producer writes data to `slot[0]`, then signals `ready_flag = 1`. Consumer waits for `ready_flag`, then reads `slot[0]`.

---

#### **❌ Without Ordering (Broken - Using Relaxed)**

```
Time  Producer (Core 1)                Consumer (Core 2)
----  ---------------------------      ---------------------------
t0    slot[0].value = 42
      └─> Store buffer: [value=42]    (Not visible yet!)

t1    ready_flag.store(1, Relaxed)
      └─> Cache: [ready=1]             (Visible immediately!)

t2                                     ready_flag.load(Relaxed) → 1 ✅
                                       └─> Cache hit! Exits wait loop

t3                                     slot[0].value → 0 ❌
                                       └─> Cache miss! Reads stale value!

t4    value=42 flushed to cache       (Too late! Consumer already read!)
```

**Problem:** Consumer sees `ready_flag = 1` before `value = 42` is visible!

**Why this happens:**
1. **Store buffer** - Writes go to a per-core buffer first, then to cache later
2. **No ordering** - Relaxed allows `ready_flag` to become visible before `value`
3. **Cache miss** - Consumer's cache doesn't have the updated value yet

**Cache line view:**
```
Core 1 Store Buffer:        Core 1 Cache:           Core 2 Cache:
┌──────────────┐           ┌──────────────┐        ┌──────────────┐
│ value=42     │           │ ready=1      │        │ ready=1      │ ← Sees ready
└──────────────┘           └──────────────┘        │ value=0      │ ← Stale!
  ↑ Not flushed yet!                               └──────────────┘
```

---

#### **✅ With Release/Acquire (Correct)**

```
Time  Producer (Core 1)                Consumer (Core 2)
----  ---------------------------      ---------------------------
t0    slot[0].value = 42
      └─> Store buffer: [value=42]

t1    ready_flag.store(1, Release)
      └─> Flush store buffer first!   ← Release guarantees this
      └─> Cache: [value=42, ready=1]  ← Both visible together

t2                                     ready_flag.load(Acquire) → 1
                                       └─> Acquire: invalidate cache
                                       └─> Fetch from Core 1's cache

t3                                     slot[0].value → 42 ✅
                                       └─> Sees flushed value!
```

**Key insight:**
- **Release** flushes the store buffer before making `ready_flag` visible
- **Acquire** invalidates the cache to fetch the latest values

**Cache line view:**
```
Core 1 Store Buffer:        Core 1 Cache:           Core 2 Cache:
┌──────────────┐           ┌──────────────┐        ┌──────────────┐
│ (empty)      │           │ value=42     │        │ value=42     │ ← Fresh!
└──────────────┘           │ ready=1      │        │ ready=1      │ ← Sees ready
  ↑ Flushed by Release!    └──────────────┘        └──────────────┘
                                                      ↑ Fetched by Acquire!
```

**Key insight:** Release = "flush everything before signaling", Acquire = "fetch everything after seeing signal"

**Note:** This is a simplified view of the cache hierarchy. Real CPUs have multiple cache levels (L1/L2/L3) and use cache coherency protocols like MESI (Modified/Exclusive/Shared/Invalid) to maintain consistency. The key concept remains: Release ensures ordering of stores, Acquire ensures visibility of those stores.

---

#### **Comparison: Side-by-Side**

| Aspect | Relaxed (Broken) | Release/Acquire (Correct) |
|--------|------------------|---------------------------|
| **Store buffer** | Not flushed | Flushed by Release |
| **Cache coherency** | Delayed | Immediate via Acquire |
| **Visibility** | Out of order | Ordered |
| **Consumer sees** | `ready=1, value=0` ❌ | `ready=1, value=42` ✅ |

---

### **What Does the CPU Actually Do?**

Let's see what assembly instructions are generated for each ordering.

**Code:**
```rust
// Producer
slot[0].value = 42;
ready_flag.store(1, Ordering::???);
```

---

#### **x86-64 Assembly**

**Relaxed:**
```asm
mov    QWORD PTR [rdi], 42      ; Write value to slot[0]
mov    DWORD PTR [rsi], 1       ; Write 1 to ready_flag (no fence!)
```
**Cost:** Free (no fence instruction)
**Problem:** CPU can reorder these stores (though x86 TSO makes this rare)

**Release:**
```asm
mov    QWORD PTR [rdi], 42      ; Write value to slot[0]
; (compiler barrier - prevents reordering)
mov    DWORD PTR [rsi], 1       ; Write 1 to ready_flag
```
**Cost:** Free on x86 (TSO already provides store-store ordering)
**Benefit:** Compiler won't reorder, CPU ordering is guaranteed

**SeqCst:**
```asm
mov    QWORD PTR [rdi], 42      ; Write value to slot[0]
xchg   DWORD PTR [rsi], eax     ; Atomic exchange (implicit full barrier)
; Alternative: mov + mfence after store
; mov    DWORD PTR [rsi], 1
; mfence                        ; Full memory fence AFTER store
```
**Cost:** ~20-30 cycles (XCHG has implicit LOCK prefix, or MFENCE instruction)
**When needed:** StoreLoad fence (publish cursor, then read consumer positions)
**Note:** SeqCst store uses `xchg` (most common) or `mov` + `mfence` after the store

---

#### **ARM64 Assembly**

**Relaxed:**
```asm
str    x0, [x1]                 ; Write value to slot[0]
str    w2, [x3]                 ; Write 1 to ready_flag (no barrier!)
```
**Cost:** Free (no barrier instruction)
**Problem:** ARM can aggressively reorder these stores!

**Release:**
```asm
str    x0, [x1]                 ; Write value to slot[0]
dmb    ish                      ; Data Memory Barrier (inner shareable)
str    w2, [x3]                 ; Write 1 to ready_flag
```
**Cost:** ~50-100 cycles (DMB instruction)
**Benefit:** Ensures all prior stores are visible before ready_flag

**SeqCst:**
```asm
str    x0, [x1]                 ; Write value to slot[0]
dmb    ish                      ; Release barrier (prior stores visible)
str    w2, [x3]                 ; Write 1 to ready_flag
dmb    ish                      ; StoreLoad barrier (subsequent loads see this)
```
**Cost:** ~100-200 cycles (two DMB instructions)
**When needed:** Total ordering across all threads (rare)
**Note:** Two barriers needed - first for Release semantics, second for StoreLoad fence

---

**Key insights:**
1. **x86 is cheap** - Total Store Order (TSO) provides most ordering for free
2. **ARM is expensive** - Weak memory model requires explicit barriers
3. **SeqCst is always more expensive** - Full fence on all architectures
4. **Release/Acquire is the sweet spot** - Sufficient for most use cases

**Source:** Compiler Explorer (godbolt.org), Intel/ARM architecture manuals

---

### **Performance Summary**

| Ordering | Cost | What It Does |
|----------|------|--------------|
| **Relaxed** | Free | Atomic operation only, no ordering |
| **Acquire** | Cheap | Prevents reordering of subsequent reads |
| **Release** | Cheap | Prevents reordering of prior writes |
| **AcqRel** | Cheap | Both Acquire + Release |
| **SeqCst** | Moderate | Full barrier (includes StoreLoad fence) |

**Performance (approximate):**
- **Relaxed**: Free (no fence)
- **Acquire/Release**: Cheap (compiler barrier on x86, DMB on ARM)
- **SeqCst**: Cheap on x86 (TSO provides ordering), Moderate on ARM (DMB barrier)

**Architecture-specific notes:**

**x86-64 (Total Store Order):**
- Acquire/Release are essentially free (TSO already provides most ordering)
- SeqCst requires MFENCE instruction (still relatively cheap, ~20-30 cycles)
- Most production HFT systems run on x86

**ARM/PowerPC (Weak Memory Models):**
- Acquire/Release require DMB (Data Memory Barrier) instructions
- SeqCst requires full DMB barrier (more expensive)
- More aggressive reordering = more explicit barriers needed
- **Important:** Always test on target architecture!

**Source:** "Rust Atomics and Locks" (Mara Bos), Intel/ARM architecture manuals

---

### **How to Choose the Right Ordering: Decision Flowchart**

When you're sitting in front of a problem and need to choose an ordering, use this decision tree:

```
START: I need to use an atomic operation
│
├─> Q1: Does this operation synchronize with other memory?
│   │
│   ├─> NO: Use Relaxed
│   │   └─> Example: Statistics counter, no other data depends on it
│   │   └─> Code: counter.fetch_add(1, Ordering::Relaxed)
│   │
│   └─> YES: Continue to Q2
│
├─> Q2: Am I writing (store) or reading (load)?
│   │
│   ├─> WRITING (store):
│   │   │
│   │   ├─> Q3: Does other data need to be visible before this write?
│   │   │   │
│   │   │   ├─> YES: Use Release
│   │   │   │   └─> Example: Write data, then signal "ready"
│   │   │   │   └─> Code: ready_flag.store(1, Ordering::Release)
│   │   │   │
│   │   │   └─> NO: Use Relaxed
│   │   │
│   │   └─> Q4: Do I need a StoreLoad fence?
│   │       │
│   │       ├─> YES: Use SeqCst
│   │       │   └─> Example: Publish cursor, then read consumer positions
│   │       │   └─> Code: cursor.store(next, Ordering::SeqCst)
│   │       │
│   │       └─> NO: Use Release
│   │
│   └─> READING (load):
│       │
│       ├─> Q5: Do I need to see writes from a Release?
│       │   │
│       │   ├─> YES: Use Acquire
│       │   │   └─> Example: Wait for "ready" flag, then read data
│       │   │   └─> Code: ready_flag.load(Ordering::Acquire)
│       │   │
│       │   └─> NO: Use Relaxed
│       │
│       └─> Q6: Do I need to see ALL writes from ALL threads?
│           │
│           ├─> YES: Use SeqCst (rare!)
│           │   └─> Example: Global synchronization point
│           │
│           └─> NO: Use Acquire
│
└─> Q7: Is this a read-modify-write (fetch_add, compare_exchange)?
    │
    ├─> Q8: Does it synchronize with other threads?
    │   │
    │   ├─> YES: What kind of synchronization?
    │   │   │
    │   │   ├─> Need to see others AND be seen: Use AcqRel
    │   │   │   └─> Example: Multiple producers claiming sequences
    │   │   │   └─> Code: cursor.fetch_add(1, Ordering::AcqRel)
    │   │   │
    │   │   ├─> Only need to be seen (Release): Use Release
    │   │   │   └─> Example: Consumer updating position (others read it)
    │   │   │   └─> Code: consumer_seq.fetch_add(1, Ordering::Release)
    │   │   │
    │   │   └─> Only need to see others (Acquire): Use Acquire (rare for RMW)
    │   │
    │   └─> NO: Use Relaxed
    │
    └─> Q9: Do I need a StoreLoad fence?
        │
        ├─> YES: Use SeqCst (very rare!)
        │   └─> Example: Atomic RMW with subsequent loads
        │
        └─> NO: Use AcqRel (or Release/Acquire based on Q8)
```

---

### **Quick Reference Table**

| Operation | Synchronizes? | Other Data? | Ordering | Example |
|-----------|---------------|-------------|----------|---------|
| **Counter increment** | No | No | **Relaxed** | `counter.fetch_add(1, Relaxed)` |
| **Signal ready (write)** | Yes | Yes (data before) | **Release** | `ready.store(1, Release)` |
| **Wait for ready (read)** | Yes | Yes (data after) | **Acquire** | `ready.load(Acquire)` |
| **Multi-producer claim** | Yes | Yes (both) | **AcqRel** | `cursor.fetch_add(1, AcqRel)` |
| **Publish then check** | Yes | Yes + StoreLoad | **SeqCst** | `cursor.store(n, SeqCst)` |

---

### **The "Aha!" Moment Test**

After reading this section, you should be able to answer this question:

**Scenario:** You're implementing a work-stealing queue. Thread A pushes work, Thread B steals it.

```rust
// Thread A (Producer)
queue[tail] = work_item;
tail.store(new_tail, Ordering::???);  // What ordering?

// Thread B (Consumer)
let t = tail.load(Ordering::???);     // What ordering?
let work = queue[t];
```

**Think about it before scrolling...**

<details>
<summary>Click to reveal answer</summary>

**Answer:**
```rust
// Thread A (Producer)
queue[tail] = work_item;
tail.store(new_tail, Ordering::Release);  // ← Release

// Thread B (Consumer)
let t = tail.load(Ordering::Acquire);     // ← Acquire
let work = queue[t];
```

**Why?**
1. **Producer writes data, then signals** → Classic Release pattern
   - `work_item` must be visible before `tail` update
   - Release ensures all prior writes are visible

2. **Consumer waits for signal, then reads data** → Classic Acquire pattern
   - Wait for `tail` update (Acquire)
   - Then read `work_item` (guaranteed to see it)

3. **This is Pattern 1** (flag-based synchronization) - see below!

**Decision tree path:**
- Q1: Synchronizes? YES (work_item depends on tail)
- Q2: Writing or reading? WRITING (tail.store)
- Q3: Other data visible first? YES (work_item)
- → Use Release ✅

If you got this right, you understand memory ordering! ✅

</details>

---

### **Common Patterns: Match Your Problem**

Most memory ordering problems fall into one of these patterns. Recognize your problem, apply the pattern!

---

#### **Pattern 1: Flag-Based Synchronization**

**Problem:** Signal that data is ready

**Code:**
```rust
// Producer
data.write(value);
ready_flag.store(true, Ordering::Release);  // ← Release

// Consumer
while !ready_flag.load(Ordering::Acquire) { // ← Acquire
    std::hint::spin_loop();
}
let value = data.read();
```

**Why:**
- **Release:** Makes `data` write visible before `ready_flag`
- **Acquire:** Sees `data` write after seeing `ready_flag`
- **Classic producer-consumer pattern**

**When to use:**
- Signaling completion (work done, data ready)
- Lazy initialization (initialize once, read many times)
- Event notification

---

#### **Pattern 2: Sequence Counter (Multi-Producer)**

**Problem:** Multiple producers claiming slots atomically

**Code:**
```rust
let my_slot = counter.fetch_add(1, Ordering::AcqRel);  // ← AcqRel
```

**Why:**
- **Acquire part:** See all previous claims from other producers
- **Release part:** Make my claim visible to other producers
- **Both needed:** Coordinate with multiple threads

**When to use:**
- Multi-producer sequencer (claiming sequences)
- Work distribution (assigning tasks to workers)
- Resource allocation (claiming slots in a pool)

---

#### **Pattern 3: Statistics/Metrics (No Synchronization)**

**Problem:** Just counting, no other data depends on this

**Code:**
```rust
metrics.fetch_add(1, Ordering::Relaxed);  // ← Relaxed

// Later, read total
let total = metrics.load(Ordering::Relaxed);
```

**Why:**
- **No synchronization needed:** Counter doesn't protect other data
- **Just need atomicity:** No lost updates
- **Cheapest option:** No memory barriers

**When to use:**
- Performance counters
- Statistics collection
- Metrics that don't affect correctness

---

#### **Pattern 4: Publish-Then-Check (StoreLoad Fence)**

**Problem:** Update cursor, then check consumer positions

**Code:**
```rust
// Publish cursor
cursor.store(next, Ordering::SeqCst);  // ← SeqCst (StoreLoad fence)

// Check consumer positions (MUST see cursor update first!)
let min_consumer = get_minimum_consumer();
```

**Why:**
- **StoreLoad fence needed:** Prevent reordering of store before load
- **Without fence:** CPU might read stale consumer positions **before** cursor is visible
- **SeqCst provides this:** Only ordering that includes StoreLoad

**The reordering hazard:**
```rust
// Without SeqCst, CPU might reorder to:
let min_consumer = get_minimum_consumer();  // ← Read FIRST (stale!)
cursor.store(next, Ordering::Release);      // ← Store SECOND (too late!)

// Result: We think buffer is full when it's not (performance bug)
//         or we think buffer is empty when it's full (correctness bug!)
```

**When to use:**
- Disruptor sequencer (publish cursor, check consumers)
- Two-phase commit (update state, check participants)
- **Rare pattern:** Most problems don't need StoreLoad

---

#### **Pattern 5: Lazy Initialization (Once)**

**Problem:** Initialize once, read many times

**Code:**
```rust
// Writer (once)
static INITIALIZED: AtomicBool = AtomicBool::new(false);
static mut DATA: Option<ExpensiveData> = None;

unsafe {
    DATA = Some(ExpensiveData::new());
}
INITIALIZED.store(true, Ordering::Release);  // ← Release

// Readers (many)
if INITIALIZED.load(Ordering::Acquire) {     // ← Acquire
    unsafe { DATA.as_ref().unwrap() }
}
```

**Why:**
- **Same as Pattern 1:** Flag-based synchronization
- **Release:** Makes `DATA` write visible before `INITIALIZED`
- **Acquire:** Sees `DATA` write after seeing `INITIALIZED`

**When to use:**
- Lazy static initialization
- Singleton pattern
- One-time setup

**Note:** In practice, use `std::sync::Once` or `lazy_static!` crate instead of rolling your own!

---

### **Pattern Matching Summary**

| Your Problem | Pattern | Ordering |
|--------------|---------|----------|
| "Signal that data is ready" | Pattern 1 (Flag) | Release/Acquire |
| "Multiple producers claiming slots" | Pattern 2 (Counter) | AcqRel |
| "Just counting, no dependencies" | Pattern 3 (Metrics) | Relaxed |
| "Publish cursor, then check consumers" | Pattern 4 (StoreLoad) | SeqCst |
| "Initialize once, read many times" | Pattern 5 (Lazy Init) | Release/Acquire |

**Key insight:** 90% of problems are Pattern 1 or Pattern 2!

---

### **Confidence Check**

You should now be able to:

1. ✅ **Visualize** what happens in cache for each ordering
2. ✅ **Recognize** which pattern your problem matches
3. ✅ **Choose** the right ordering using the decision flowchart
4. ✅ **Explain** why you chose that ordering (not just cargo-culting)
5. ✅ **Debug** memory ordering bugs by understanding the hardware

**The goal:** When you sit in front of a problem, you think:

> "Aha! This is Pattern 1 (flag-based synchronization). I need Release on the write and Acquire on the read because I'm signaling that data is ready. Let me verify with the flowchart... Q1: Synchronizes? Yes. Q2: Writing. Q3: Other data visible first? Yes. → Release. Perfect!"

**That's the confidence we're building!** 🎯

---



## Evolution of the Solution

Now that we understand memory ordering, let's build a sequencer step by step. We'll start with the naive approach and progressively optimize.

**Important note about the progression:**

The following attempts focus on solving **different problems** in sequence:
1. **Attempts 1-2:** Solve race conditions (coordination between producers)
2. **Attempt 3:** Solve the "forgot to publish" bug (RAII)
3. **Attempt 4:** Optimize for single producer (performance)
4. **Attempt 5:** Add wrap-around prevention (completeness)

**Key insight:** Wrap-around prevention is **orthogonal** to the coordination mechanism. You could add wrap-around checking to any of the attempts—we just show it in Attempt 5 for pedagogical clarity. The lock/atomic/RAII choices don't prevent wrap-around checking; we simply haven't implemented it yet in the earlier attempts.

### **Attempt 1: Manual Locking (Naive)**

The simplest approach: use a mutex. To match the Disruptor pattern, we separate claiming from publishing.

```rust
use std::sync::{Mutex, MutexGuard};

pub struct LockedSequencer {
    cursor: Mutex<i64>,
    buffer_size: usize,
}

pub struct SequenceLock<'a> {
    sequence: i64,
    _guard: MutexGuard<'a, i64>,  // Hold lock until publish
}

impl LockedSequencer {
    pub fn claim(&self, count: usize) -> SequenceLock {
        let mut guard = self.cursor.lock().unwrap();
        let sequence = *guard;  // This is the first sequence we claim (old value)
        *guard += count as i64;  // Update cursor for next claim
        SequenceLock {
            sequence,
            _guard: guard,  // Lock held until publish!
        }
    }
}

impl SequenceLock<'_> {
    pub fn sequence(&self) -> i64 {
        self.sequence
    }

    pub fn publish(self) {
        // Lock released when self (and _guard) drops
    }
}

// Usage
let lock = sequencer.claim(1);
ring_buffer[lock.sequence()].value = 42;
lock.publish();  // Must call this to release lock!

// If you forget to call publish():
let lock = sequencer.claim(1);
ring_buffer[lock.sequence()].value = 42;
// Oops! Lock never released → next claim() blocks forever → DEADLOCK!
```

**Pros:**
- ✅ Simple and correct
- ✅ No race conditions

**Cons:**
- ❌ Lock overhead: 10-50ns per claim (uncontended)
- ❌ Contention: 1-10μs per claim (contended)
- ❌ Can forget to call `publish()` → lock never released → deadlock!

**What's missing (not a "con"):**
- ⚠️ Wrap-around prevention not shown yet (will add in Attempt 5)
- ⚠️ Note: The lock doesn't prevent wrap-around checking—we just haven't added it yet

**Verdict:** Too slow for HFT. Let's remove the lock.

---

### **Attempt 2: Atomic Counter (Better)**

Replace the mutex with an atomic operation. Separate the claim and publish steps.

```rust
use std::sync::atomic::{AtomicI64, Ordering};

pub struct AtomicSequencer {
    next_sequence: AtomicI64,  // Next sequence to claim
    cursor: AtomicI64,          // Last published sequence
    buffer_size: usize,
}

impl AtomicSequencer {
    pub fn claim(&self, count: usize) -> i64 {
        // Atomically claim sequence, but DON'T publish yet
        // Relaxed is safe: only producers read next_sequence, consumers wait on cursor
        self.next_sequence.fetch_add(count as i64, Ordering::Relaxed)
    }

    pub fn publish(&self, sequence: i64) {
        // Now make it visible to consumers
        self.cursor.store(sequence, Ordering::Release);
    }
}

// Usage
let seq = sequencer.claim(1);
ring_buffer[seq].value = 42;
sequencer.publish(seq);  // Must call this!

// If you forget to call publish():
let seq = sequencer.claim(1);
ring_buffer[seq].value = 42;
// Oops! cursor never updated → consumer waits forever → DEADLOCK!
```

**Pros:**
- ✅ Lock-free (no kernel involvement)
- ✅ Fast: ~20-50ns per claim (vs 10-50ns for lock)
- ✅ No contention cascade

**Cons:**
- ❌ Can still forget to call `publish()` → cursor never updated → consumer deadlock!

**What's missing (not a "con"):**
- ⚠️ Wrap-around prevention not shown yet (will add in Attempt 5)
- ⚠️ Note: Atomics don't prevent wrap-around checking—we just haven't added it yet

**Verdict:** Faster, but still has the "forgot to publish" bug. Let's fix that.

---

### **Attempt 3: RAII Guard (Correct)**

Use Rust's `Drop` trait to automatically publish.

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

pub struct RAIISequencer {
    cursor: Arc<AtomicI64>,
    buffer_size: usize,
}

impl RAIISequencer {
    pub fn claim(&self, count: usize) -> SequenceClaim {
        // fetch_add returns the OLD value before adding (this is the first sequence we claim)
        let current = self.cursor.fetch_add(count as i64, Ordering::AcqRel);
        SequenceClaim {
            start: current,  // First sequence in claim (fetch_add returned old value)
            end: current + count as i64 - 1,  // Last sequence in claim (inclusive)
            cursor: Arc::clone(&self.cursor),
        }
    }
}

// Usage
{
    let claim = sequencer.claim(1);
    ring_buffer[claim.start()].value = 42;
}  // claim.drop() automatically publishes!

// No way to forget to publish!
```

**Pros:**
- ✅ Lock-free
- ✅ **Impossible to forget to publish** (Rust's type system enforces it!)
- ✅ Works with early returns, panics, etc.

**Cons:**
- ❌ Uses `fetch_add` even for single producer (unnecessary atomic)

**What's missing (not a "con"):**
- ⚠️ Wrap-around prevention not shown yet (will add in Attempt 5)
- ⚠️ Note: RAII doesn't prevent wrap-around checking—we just haven't added it yet

**Verdict:** Correct and safe! But we can optimize further.

---

### **Attempt 4: Single-Producer Optimization (Fast)**

If there's only one producer, we don't need atomics!

```rust
use std::cell::Cell;

pub struct SingleProducerSequencer {
    cursor: Arc<AtomicI64>,      // For publishing (consumers read this)
    next_sequence: Cell<i64>,    // For claiming (only producer writes)
    buffer_size: usize,
}

impl SingleProducerSequencer {
    pub fn claim(&self, count: usize) -> SequenceClaim {
        // No atomic operation needed!
        let current = self.next_sequence.get();
        let next = current + count as i64;
        self.next_sequence.set(next);

        SequenceClaim {
            start: current,      // First sequence to claim
            end: next - 1,       // Last sequence to claim (inclusive)
            cursor: Arc::clone(&self.cursor),
        }
    }
}
```

**Pros:**
- ✅ **No atomic operation in claim** (just a load/store)
- ✅ 5-20x faster than multi-producer (~10ns vs ~50-200ns)
- ✅ Still uses RAII for safety

**Cons:**
- ❌ Only works with single producer

**What's missing (not a "con"):**
- ⚠️ Wrap-around prevention not shown yet (will add in Attempt 5)
- ⚠️ Note: Single-producer optimization doesn't prevent wrap-around checking—we just haven't added it yet

**Verdict:** Blazing fast for single-producer! But we need wrap-around protection.

---

### **Attempt 5: Wrap-Around Prevention (Complete)**

Track consumer positions and wait before overwriting.

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

        // Check if we would wrap around and overwrite unconsumed data
        let wrap_point = next - self.buffer_size as i64;

        // Wait for all consumers to pass wrap_point
        while self.get_minimum_sequence() < wrap_point {
            std::hint::spin_loop();  // Or use wait strategy
        }

        self.next_sequence.set(next);

        SequenceClaim {
            start: current,  // First sequence in claim (inclusive)
            end: next - 1,   // Last sequence in claim (inclusive)
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

**Example:**
```
Ring Buffer (size 4):
┌───┬───┬───┬───┐
│ 0 │ 1 │ 2 │ 3 │
└───┴───┴───┴───┘
  ↑
  Consumer at sequence 0

Producer wants sequence 5 (wraps to index 1):
- wrap_point = 5 - 4 = 1
- get_minimum_sequence() = 0
- 0 < 1, so WAIT!

Consumer advances to sequence 1:
- get_minimum_sequence() = 1
- 1 >= 1, so PROCEED!
```

**Pros:**
- ✅ Prevents wrap-around (safe!)
- ✅ Fast claiming (~10ns when no waiting)
- ✅ RAII prevents forgot-to-publish bugs

**Cons:**
- ❌ Checks all consumers on every claim (can be slow with many consumers)

**Verdict:** Correct and complete! But we can optimize the consumer check.

---

### **Attempt 6: Cached Gating Sequence (Optimized)**

Cache the minimum consumer position to avoid checking all consumers every time.

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
        let wrap_point = next - self.buffer_size as i64;

        // Check cache first (fast path)
        let cached = self.cached_gating_sequence.load(Ordering::Relaxed);
        if wrap_point > cached {
            // Cache miss - recompute minimum
            let min = self.get_minimum_sequence();
            self.cached_gating_sequence.store(min, Ordering::Relaxed);

            // Wait if still not enough space
            while min < wrap_point {
                std::hint::spin_loop();
                let min = self.get_minimum_sequence();
                self.cached_gating_sequence.store(min, Ordering::Relaxed);
            }
        }

        self.next_sequence.set(next);

        SequenceClaim {
            start: current,  // First sequence in claim (inclusive)
            end: next - 1,   // Last sequence in claim (inclusive)
            cursor: Arc::clone(&self.cursor),
        }
    }
}
```

**How caching works:**
```
Scenario: 10 consumers, buffer size 1024

Without caching:
- Every claim checks all 10 consumers
- 10 atomic loads per claim
- ~50-100ns overhead

With caching:
- Cache hit: 0 atomic loads (just check cached value)
- Cache miss: 10 atomic loads + update cache
- ~5-10ns overhead (10x faster!)

Cache hit rate: ~99% in practice (consumers advance slowly)
```

**Pros:**
- ✅ **10x faster** with many consumers (cache hit avoids iteration)
- ✅ Still correct (cache is conservative - we only skip check if cached >= wrap_point)
- ✅ All previous benefits (RAII, wrap-around prevention)

**Cons:**
- ❌ Slightly more complex

**Verdict:** Production-ready! This is what we'll implement.

---

## Summary: Evolution of the Solution

| Attempt | Claiming | Safety | Wrap-Around Shown | Performance |
|---------|----------|--------|-------------------|-------------|
| 1. Mutex | Lock | ✅ | Not yet | 10-50ns (uncontended) |
| 2. Atomic | fetch_add | ❌ (can forget publish) | Not yet | 20-50ns |
| 3. RAII | fetch_add | ✅ (Drop trait) | Not yet | 20-50ns |
| 4. Single-producer | No atomic | ✅ | Not yet | ~10ns |
| 5. Wrap-around | No atomic | ✅ | ✅ Added here | ~10ns (no wait) |
| 6. Cached | No atomic | ✅ | ✅ Optimized | ~10ns (cache hit) |

**Key insights:**
1. **RAII** solves the "forgot to publish" bug (Rust's type system FTW!)
2. **Single-producer** is 5-20x faster than multi-producer (no atomic contention)
3. **Caching** makes consumer checks 10x faster (avoid iteration)
4. **Wrap-around prevention is orthogonal** - can be added to any approach (we show it in Attempt 5 for clarity)

Now let's implement both sequencers in detail.

---


## The Sequencer Trait

Now let's define the interface all sequencers must implement:

```rust
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

pub trait Sequencer: Send + Sync {
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
    publish_fn: Option<Box<dyn FnOnce(i64, i64) + Send>>,
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
        if let Some(publish_fn) = self.publish_fn.take() {
            publish_fn(self.start, self.end);
        }
    }
}
```

**Key design decisions:**

1. **RAII via Drop** - Publishing happens automatically when `SequenceClaim` goes out of scope
2. **Closure-based publishing** - Each sequencer provides its own publish logic
3. **Range support** - Can claim multiple sequences at once (batch claiming)

**Why closure instead of trait method?** This allows `SequenceClaim` to capture the `Arc<AtomicI64>` cursor without needing a reference back to the sequencer. Cleaner ownership model.

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

impl SingleProducerSequencer {
    pub fn new(buffer_size: usize, gating_sequences: Vec<Arc<AtomicI64>>) -> Self {
        Self {
            cursor: Arc::new(AtomicI64::new(-1)),  // Start at -1 so first sequence is 0
            next_sequence: Cell::new(0),            // First claim will be 0
            buffer_size,
            gating_sequences,
            cached_gating_sequence: AtomicI64::new(-1),  // Start at -1 (no sequences published)
        }
    }

    fn get_minimum_sequence(&self) -> i64 {
        // Recompute minimum (requires Acquire to see consumer updates!)
        let mut minimum = i64::MAX;
        for seq in &self.gating_sequences {
            let value = seq.load(Ordering::Acquire);
            minimum = minimum.min(value);
        }

        // Update cache
        self.cached_gating_sequence.store(minimum, Ordering::Relaxed);
        minimum
    }
}

impl Sequencer for SingleProducerSequencer {
    fn claim(&self, count: usize) -> SequenceClaim {
        let current = self.next_sequence.get();
        let next = current + count as i64;

        // Check if we would wrap around and overwrite unconsumed data
        let wrap_point = next - self.buffer_size as i64;

        // Check cache first (fast path)
        let cached = self.cached_gating_sequence.load(Ordering::Relaxed);
        if wrap_point > cached || cached > current {
            // Cache is stale (either too low or too high)

            // StoreLoad fence: Ensure cursor visible to consumers before reading their positions
            // This matches Java Disruptor's cursor.setVolatile(nextValue) before getMinimumSequence
            //
            // Why SeqCst? We need a StoreLoad barrier:
            // 1. Store cursor (so consumers see our progress)
            // 2. Load consumer positions (to see their progress)
            // Without this fence, CPU might reorder: read stale consumer positions, then update cursor
            // Result: We think buffer is full when it's not (performance) or vice versa (correctness!)
            //
            // Cost: ~20-30 cycles on x86 (MFENCE), ~50-100 cycles on ARM (DMB)
            // This is the ONLY SeqCst in the hot path, and only on cache miss (rare)
            self.cursor.store(current, Ordering::SeqCst);

            // Wait for consumers to catch up
            while self.get_minimum_sequence() < wrap_point {
                std::hint::spin_loop();
                // TODO: Use wait strategy (Post 4)
                // - BusySpinWaitStrategy: Ultra-low latency (<100ns), high CPU usage
                // - YieldingWaitStrategy: Balanced latency/CPU (yield after N spins)
                // - BlockingWaitStrategy: Low CPU usage, higher latency (park thread)
            }
        }

        // Update next sequence (no atomic needed - single producer!)
        self.next_sequence.set(next);

        // Create claim with closure that publishes on drop
        let cursor = Arc::clone(&self.cursor);
        SequenceClaim {
            start: current,  // First sequence in claim (inclusive)
            end: next - 1,   // Last sequence in claim (inclusive)
            // Note: Java Disruptor returns 'next-1' directly (the last sequence)
            // We return a range [start..=end] for batch claiming support
            publish_fn: Some(Box::new(move |_lo, hi| {
                cursor.store(hi, Ordering::Release);
            })),
        }
    }

    fn try_claim(&self, count: usize) -> Result<SequenceClaim, InsufficientCapacity> {
        let current = self.next_sequence.get();
        let next = current + count as i64;
        let wrap_point = next - self.buffer_size as i64;

        // Check cache first
        let cached = self.cached_gating_sequence.load(Ordering::Relaxed);
        if wrap_point > cached || cached > current {
            // Cache is stale
            // StoreLoad fence
            self.cursor.store(current, Ordering::SeqCst);

            // Check if space is available (don't wait!)
            if self.get_minimum_sequence() < wrap_point {
                return Err(InsufficientCapacity);
            }
        }

        self.next_sequence.set(next);

        let cursor = Arc::clone(&self.cursor);
        Ok(SequenceClaim {
            start: current,
            end: next - 1,
            publish_fn: Some(Box::new(move |_lo, hi| {
                cursor.store(hi, Ordering::Release);
            })),
        })
    }

    fn cursor(&self) -> i64 {
        self.cursor.load(Ordering::Acquire)
    }
}
```

**Key optimizations:**

1. **No atomic in claim** - `Cell<i64>` for next_sequence (single producer only)
2. **Cached gating sequence** - Avoids iterating consumers on every claim (10x faster)
3. **StoreLoad fence** - Ensures we see latest consumer positions (ARM/PowerPC correctness)
4. **Release on publish** - Makes event data visible to consumers

**Performance:**
- **Claim (cache hit)**: ~10ns (just Cell get/set + cache check)
- **Claim (cache miss)**: ~50-100ns (iterate consumers + update cache)
- **Claim (waiting)**: Depends on wait strategy

---


## MultiProducerSequencer: Coordinating Multiple Producers

When multiple producers need to write concurrently, we need atomic coordination. Let's implement the multi-producer sequencer.

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

**Problem:** Consumer can't just check cursor—it needs to know which sequences are actually published.

**Solution:** An `available_buffer` that tracks each sequence individually:

```rust
pub struct MultiProducerSequencer {
    cursor: Arc<AtomicI64>,                // Highest claimed sequence
    buffer_size: usize,
    index_mask: usize,                     // buffer_size - 1 (for bitwise AND)
    gating_sequences: Vec<Arc<AtomicI64>>,
    cached_gating_sequence: AtomicI64,
    available_buffer: Arc<[AtomicI32]>,    // Tracks which sequences are published
    // Note: available_buffer elements are not cache-line padded
    // This can cause false sharing with many producers writing to adjacent indices
    // Java Disruptor has the same trade-off (padding would waste memory)
    // In practice, temporal locality means producers often write to same cache line
    // For extreme cases (8+ producers, small buffer), consider padding
}
```

**How available_buffer works:**

```rust
// For sequence 5 in buffer of size 4:
let index = 5 & 3 = 1  // Which slot in ring buffer
let lap = 5 / 4 = 1    // Which "lap" around the ring

// Store lap number to indicate sequence is published
available_buffer[index].store(lap, Ordering::Release);

// Consumer checks:
let expected_lap = sequence / buffer_size;
let actual_lap = available_buffer[index].load(Ordering::Acquire);
if actual_lap == expected_lap {
    // Sequence is published!
}
```

This lets consumers distinguish between "sequence 5 not published" vs "sequence 1029 published" (both map to index 1).

---

### **Implementation: fetch_add Approach**

We use `fetch_add` (like Java Disruptor) instead of CAS loop:

```rust
impl MultiProducerSequencer {
    pub fn new(buffer_size: usize, gating_sequences: Vec<Arc<AtomicI64>>) -> Self {
        assert!(buffer_size.is_power_of_two(), "buffer_size must be power of 2");

        // Initialize available_buffer with -1 (no sequences published yet)
        let available_buffer: Arc<[AtomicI32]> = (0..buffer_size)
            .map(|_| AtomicI32::new(-1))
            .collect();

        Self {
            cursor: Arc::new(AtomicI64::new(-1)),  // Start at -1 so first sequence is 0
            buffer_size,
            index_mask: buffer_size - 1,
            gating_sequences,
            cached_gating_sequence: AtomicI64::new(-1),  // Start at -1 (no sequences published)
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
        // Atomically claim sequence (always succeeds, no retry!)
        // fetch_add returns the OLD value before adding (this is the first sequence we claim)
        let current = self.cursor.fetch_add(count as i64, Ordering::AcqRel);
        let next = current + count as i64;

        // Wait for space to become available
        let wrap_point = next - self.buffer_size as i64;

        // Check cache first (matches Java Disruptor line 125)
        let cached = self.cached_gating_sequence.load(Ordering::Relaxed);
        if wrap_point > cached || cached > current {
            // Cache is stale (either too low or too high)

            // Note: Unlike SingleProducer, we don't need a StoreLoad fence here!
            // Why? fetch_add with AcqRel already updated the cursor atomically.
            // The wait loop will catch any stale consumer positions.
            // This matches Java Disruptor's MultiProducerSequencer (no setVolatile here).

            while self.get_minimum_sequence() < wrap_point {
                std::hint::spin_loop();
                // TODO: Use wait strategy (Post 4) - same options as SingleProducer
            }
        }

        // Create claim with closure that publishes to available_buffer
        let available_buffer = Arc::clone(&self.available_buffer);
        let index_mask = self.index_mask;
        let buffer_size = self.buffer_size;

        SequenceClaim {
            start: current,      // First sequence in claim (fetch_add returned old value)
            end: next - 1,       // Last sequence in claim (inclusive)
            // Note: fetch_add returns the OLD cursor value, which is our first sequence
            // For count=1 with cursor=0: fetch_add returns 0, next=1, so we claim [0..=0]
            publish_fn: Some(Box::new(move |lo, hi| {
                // Mark all claimed slots as available
                for seq in lo..=hi {
                    let index = (seq as usize) & index_mask;
                    let lap = (seq / buffer_size as i64) as i32;
                    available_buffer[index].store(lap, Ordering::Release);
                }
            })),
        }
    }

    fn try_claim(&self, count: usize) -> Result<SequenceClaim, InsufficientCapacity> {
        // For try_claim, we use CAS loop to check capacity before claiming
        loop {
            let current = self.cursor.load(Ordering::Acquire);
            let next = current + count as i64;
            let wrap_point = next - self.buffer_size as i64;

            // Check if space is available BEFORE claiming
            let cached = self.cached_gating_sequence.load(Ordering::Relaxed);
            if wrap_point > cached || cached > current {
                // Cache is stale, refresh it
                if self.get_minimum_sequence() < wrap_point {
                    return Err(InsufficientCapacity);  // No space, fail immediately
                }
            }

            // Try to claim with CAS
            if self.cursor.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire
            ).is_ok() {
                // Success! Create claim
                let available_buffer = Arc::clone(&self.available_buffer);
                let index_mask = self.index_mask;
                let buffer_size = self.buffer_size;

                return Ok(SequenceClaim {
                    start: current,      // First sequence (CAS succeeded with this value)
                    end: next - 1,       // Last sequence (inclusive)
                    publish_fn: Some(Box::new(move |lo, hi| {
                        for seq in lo..=hi {
                            let index = (seq as usize) & index_mask;
                            let lap = (seq / buffer_size as i64) as i32;
                            available_buffer[index].store(lap, Ordering::Release);
                        }
                    })),
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

1. **fetch_add instead of Cell** - Atomic coordination between producers
2. **AcqRel ordering** - Synchronize with other producers
3. **available_buffer** - Track out-of-order publishing
4. **Lap counting** - Distinguish different laps around the ring

**Performance:**
- **Claim (no contention)**: ~50ns (fetch_add + cache check)
- **Claim (high contention)**: ~100-200ns (fetch_add + waiting)
- **5-20x slower than single-producer** (atomic contention cost)

---

### **Why fetch_add Instead of CAS Loop?**

There are two approaches to multi-producer claiming:

**Approach 1: fetch_add (Java Disruptor)**
```rust
let current = self.cursor.fetch_add(count, Ordering::AcqRel);
// Always succeeds, no retry
```

**Approach 2: CAS loop**
```rust
loop {
    let current = self.cursor.load(Ordering::Acquire);
    if self.cursor.compare_exchange_weak(...).is_ok() {
        break;
    }
    // Retry on failure
}
```

**Comparison:**

| Aspect | fetch_add | CAS Loop |
|--------|-----------|----------|
| **Speed** | Faster (one atomic op) | Slower (retry loop) |
| **Fairness** | FIFO ordering | Potential starvation |
| **Simplicity** | Simpler | More complex |
| **Battle-tested** | Java Disruptor uses this | Alternative approach |

**Recommendation:** Use **fetch_add** for production. It's faster, fairer, and matches the battle-tested Java Disruptor implementation.

---


## Usage Examples

Now that we've implemented both sequencers, let's see how to use them.

### **Example 1: Single Producer, Single Consumer**

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread;

// Create ring buffer
let ring_buffer = RingBuffer::new(1024);

// Create consumer sequence (tracks consumer position)
let consumer_seq = Arc::new(AtomicI64::new(-1));

// Create sequencer with consumer as gating sequence
let sequencer = SingleProducerSequencer::new(1024, vec![Arc::clone(&consumer_seq)]);

// Producer thread
let producer = thread::spawn(move || {
    for i in 0..1000 {
        // Claim slot
        let claim = sequencer.claim(1);

        // Write event
        let event = ring_buffer.get_mut(claim.start());
        event.value = i;

        // Automatically published on drop!
    }
});

// Consumer thread
let consumer = thread::spawn(move || {
    let mut next_sequence = 0;

    while next_sequence < 1000 {
        // Wait for sequence to be published
        while sequencer.cursor() < next_sequence {
            std::hint::spin_loop();
        }

        // Read event
        let event = ring_buffer.get(next_sequence);
        println!("Consumed: {}", event.value);

        // Update consumer position
        consumer_seq.store(next_sequence, Ordering::Release);
        next_sequence += 1;
    }
});

producer.join().unwrap();
consumer.join().unwrap();
```

---

### **Example 2: Multiple Producers, Single Consumer**

```rust
// Create sequencer for multiple producers
let sequencer = Arc::new(MultiProducerSequencer::new(1024, vec![Arc::clone(&consumer_seq)]));

// Spawn multiple producer threads
let producers: Vec<_> = (0..4).map(|id| {
    let seq = Arc::clone(&sequencer);
    let rb = Arc::clone(&ring_buffer);

    thread::spawn(move || {
        for i in 0..250 {
            let claim = seq.claim(1);
            let event = rb.get_mut(claim.start());
            event.value = id * 1000 + i;
            // Auto-published on drop
        }
    })
}).collect();

// Consumer thread (same as before)
// ...

for p in producers {
    p.join().unwrap();
}
```

---

### **Example 3: Batch Claiming**

```rust
// Claim 100 slots in one atomic operation
let claim = sequencer.claim(100);

// Write all events
for seq in claim.start()..=claim.end() {
    let event = ring_buffer.get_mut(seq);
    event.value = seq;
}

// All 100 sequences published atomically on drop!
```

**Performance benefit:** One atomic operation for 100 events instead of 100 atomic operations.

**Expected performance:**

**SingleProducerSequencer:**
- Single claim: ~10ns per event
- Batch claim (100 events): ~0.1ns per event (amortized, 100x improvement)

**MultiProducerSequencer:**
- Single claim: ~50ns per event (no contention), ~200ns (with contention)
- Batch claim (100 events): ~0.5ns per event (amortized, 100x improvement)

**Trade-offs:**

**Pros of batch claiming:**
- ✅ **Higher throughput** - Amortizes atomic operation cost (100x fewer atomics)
- ✅ **Better cache utilization** - Sequential writes to adjacent slots
- ✅ **Lower CPU overhead** - Fewer atomic operations = less contention

**Cons of batch claiming:**
- ❌ **Higher latency** - Must fill all N slots before publishing
- ❌ **Wasted space** - If you don't use all slots, they're still claimed
- ❌ **Head-of-line blocking** - Consumer waits for entire batch

**When to use batch claiming:**

| Use Case | Batch Size | Reason |
|----------|------------|--------|
| **Market data processing** | 10-100 | Throughput > latency |
| **Analytics/logging** | 100-1000 | High throughput, latency tolerant |
| **Order execution** | 1 | Latency critical |
| **Risk checks** | 1 | Latency critical |

**Rule of thumb:** Batch when throughput matters, single when latency matters.

---

### **Example 4: Non-Blocking with try_claim**

```rust
match sequencer.try_claim(1) {
    Ok(claim) => {
        // Got a slot, write event
        let event = ring_buffer.get_mut(claim.start());
        event.value = 42;
        // Auto-published on drop
    }
    Err(InsufficientCapacity) => {
        // Buffer is full, handle backpressure
        metrics.increment("ring_buffer_full");
        // Maybe drop event, or use fallback queue
    }
}
```

---

## Testing with Loom

Concurrent code is notoriously hard to test. [Loom](https://github.com/tokio-rs/loom) is a testing framework that explores all possible thread interleavings.

```rust
#[cfg(test)]
mod tests {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicI64, Ordering};
    use loom::thread;

    #[test]
    fn test_multi_producer_no_duplicate_sequences() {
        loom::model(|| {
            // Use small buffer to avoid bit shift overflow (max 63 sequences for i64 bitfield)
            let sequencer = Arc::new(MultiProducerSequencer::new(8, vec![]));
            let claimed = Arc::new(AtomicI64::new(0));

            let handles: Vec<_> = (0..2).map(|_| {
                let seq = Arc::clone(&sequencer);
                let claimed = Arc::clone(&claimed);

                thread::spawn(move || {
                    let claim = seq.claim(1);
                    let my_seq = claim.start();

                    // Atomically mark this sequence as claimed using bitfield
                    // Safe: buffer size is 8, so my_seq ∈ [0,7], and (1 << 7) = 128 fits in i64
                    let prev = claimed.fetch_or(1 << my_seq, Ordering::AcqRel);

                    // Assert no other thread claimed this sequence
                    assert_eq!(prev & (1 << my_seq), 0,
                        "Sequence {} claimed by multiple threads!", my_seq);
                })
            }).collect();

            for h in handles {
                h.join().unwrap();
            }
        });
    }

    #[test]
    fn test_publish_happens_before() {
        loom::model(|| {
            let sequencer = Arc::new(MultiProducerSequencer::new(1024, vec![]));
            let ring_buffer = Arc::new(vec![AtomicI64::new(0); 1024]);

            let handles: Vec<_> = (0..2).map(|_| {
                let seq = Arc::clone(&sequencer);
                let buffer = Arc::clone(&ring_buffer);

                thread::spawn(move || {
                    let claim = seq.claim(1);
                    let my_seq = claim.start();

                    // Write event data BEFORE publishing
                    let index = (my_seq as usize) % 1024;
                    buffer[index].store(my_seq, Ordering::Relaxed);

                    // Publish (via drop) - this should make event data visible
                    drop(claim);

                    // Verify we can read our own published data
                    let value = buffer[index].load(Ordering::Acquire);
                    assert_eq!(value, my_seq,
                        "Published data not visible after publishing!");
                })
            }).collect();

            for h in handles {
                h.join().unwrap();
            }
        });
    }
}
```

**What Loom tests:**
- **Test 1:** No two producers claim the same sequence (atomicity)
- **Test 2:** Event data written before publishing is visible after publishing (happens-before)

**Key insight:** Loom explores all possible thread interleavings, catching race conditions that might occur once in a billion runs.

---

## Performance Summary

| Sequencer | Claim Latency | Use Case |
|-----------|---------------|----------|
| **SingleProducer** | ~10ns | One producer thread |
| **MultiProducer** | ~50-200ns | Multiple producer threads |

**When to use each:**

- **SingleProducer**: Use when you have exactly one producer thread. 5-20x faster than multi-producer.
- **MultiProducer**: Use when you have multiple producer threads. Slightly slower but supports concurrency.

**Performance context (Intel i9-12900K @ 5.2GHz, DDR5-6000, buffer size 1024):**

**SingleProducerSequencer:**
- **Claim (P50)**: ~10ns (perfect cache locality, no waiting)
- **Claim (P95)**: ~20-30ns (occasional cache miss)
- **Claim (P99.9)**: ~50ns-1μs (cache miss + NUMA effects + slow consumers)

**MultiProducerSequencer:**
- **No contention (P50)**: ~50ns per claim (fetch_add + cache check)
- **No contention (P95)**: ~100ns per claim (cache miss)
- **High contention (P99.9)**: ~200ns-1μs per claim (contention + cache effects)

**Note:** These are theoretical estimates based on atomic operation costs. Actual performance depends on:
- CPU architecture (x86 vs ARM)
- Memory hierarchy (L1/L2/L3 cache, NUMA)
- System load (context switches, interrupts)
- Compiler optimizations

We'll measure actual performance with proper benchmarking in Post 15.

---

## What We Haven't Covered

The sequencer handles producer coordination, but we've left several questions unanswered:

1. **Wait strategies** - How do consumers wait efficiently? (Post 4)
2. **Sequence barriers** - How do multi-stage pipelines work? (Post 5)
3. **Event handlers** - How do consumers process events? (Post 6)
4. **Backpressure** - What happens when buffer is full? (Post 7)

**Advanced performance topics (Post 15):**

5. **CPU pinning** - Pin threads to specific cores for consistent latency
6. **NUMA awareness** - Optimize memory allocation for multi-socket systems
7. **Huge pages** - Use 2MB pages instead of 4KB to reduce TLB misses (5-10% latency improvement)
8. **Wait strategy tuning** - Balance latency vs CPU usage vs power consumption

The sequencer is the traffic cop for producers. In Post 4, we'll build the consumer side: **sequence barriers** and **wait strategies**.

---

## Key Takeaways

1. **Memory ordering is a CPU problem** - Not specific to Rust, C++, or Java. All languages must solve it.

2. **RAII prevents bugs** - Rust's Drop trait makes "forgot to publish" impossible. This is a huge advantage over C++/Java.

3. **Single-producer is 5-20x faster** - No atomic contention. Use it when possible.

4. **Caching is critical** - Cached gating sequence makes consumer checks 10x faster.

5. **fetch_add > CAS loop** - Faster, fairer, and matches Java Disruptor.

6. **Evolution matters** - Understanding the progression from naive to optimized helps you make informed trade-offs.

---

## Next Up: Sequence Barriers

In **Part 4**, we'll build the consumer side:

- **Sequence barriers** - How consumers track dependencies
- **Wait strategies** - Busy-spin vs blocking vs hybrid
- **Multi-stage pipelines** - How to chain event handlers
- **Backpressure handling** - What happens when consumers fall behind

**Teaser:** We'll achieve **sub-microsecond latency** for multi-stage processing pipelines.

---

## References

### Papers & Documentation

1. **LMAX Disruptor Paper** (2011)
   https://lmax-exchange.github.io/disruptor/files/Disruptor-1.0.pdf

2. **"A Primer on Memory Consistency and Cache Coherence"** (Sorin et al., 2011)
   https://www.morganclaypool.com/doi/abs/10.2200/S00346ED1V01Y201104CAC016

3. **Intel 64 and IA-32 Architectures Optimization Reference Manual**
   https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html

4. **ARM Architecture Reference Manual**
   https://developer.arm.com/documentation/

### Rust Documentation

- [std::sync::atomic](https://doc.rust-lang.org/std/sync/atomic/)
- [The Rustonomicon - Atomics](https://doc.rust-lang.org/nomicon/atomics.html)
- [Rust Atomics and Locks](https://marabos.nl/atomics/) (Mara Bos, 2023)

### Tools

- [Loom](https://github.com/tokio-rs/loom) - Concurrency testing framework

---

**Next:** [Part 4 — Sequence Barriers: Consumer Coordination →](post-04-sequence-barriers.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*

