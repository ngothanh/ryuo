# Building a Disruptor in Rust: Ryuo — Part 3A: Memory Ordering Fundamentals

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 3A of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 1 (latency fundamentals), Part 2A (ring buffer implementation), Part 2B (cache-line padding)

---

## From Storage to Coordination

In Parts 2A and 2B, we built a ring buffer with:
- **Fast indexing** — Bitwise AND instead of modulo (35-90x faster)
- **Interior mutability** — `UnsafeCell` provides mutable access through shared references
- **Cache-line isolation** — Prevents false sharing

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

**Problem 2: Overwrite-While-Reading (Data Hazard)**
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

**Result:** Producer overwrites data while the consumer is reading it. A write-read data hazard.

---

**Problem 3: Visibility (Reading Garbage)**

This is the most subtle problem. Even if we solve race conditions and wrap-around, we can still read garbage due to **CPU reordering**.

```
// Producer (Thread 1)
buffer[5] = 42;          // Step 1: Write data
ready_flag = 5;          // Step 2: Signal "sequence 5 is ready"

// Consumer (Thread 2)
seq = ready_flag;        // Reads: seq = 5
value = buffer[5];       // Reads: ??? (might see 0 instead of 42!)
```

**What went wrong?**

The CPU might **reorder** the producer's operations:
```
// What the CPU actually executes:
ready_flag = 5;          // ← Reordered to happen FIRST!
buffer[5] = 42;          // ← Happens SECOND

// Consumer sees:
seq = 5                  // Flag is set
value = 0                // Data not written yet!
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
1. **Atomically assign sequences** — No two producers get the same sequence (solves Problem 1)
2. **Prevent wrap-around** — Wait for consumers before overwriting (solves Problem 2)
3. **Ensure visibility** — Use memory ordering to make writes visible (solves Problem 3)

**Key insight:** The ring buffer is just storage. The sequencer is the traffic cop that makes it safe.

---

## What We'll Build (Parts 3A–3C)

This topic is split across three posts:

1. **Part 3A (this post):** Memory ordering fundamentals — why CPUs reorder, how to prevent it, and how to choose the right ordering
2. **Part 3B:** Sequencer implementation — from naive locking to optimized lock-free coordination
3. **Part 3C:** Usage examples, concurrency testing with Loom, and performance analysis

Let's start with the foundation: **memory ordering**.

---

## The Memory Visibility Problem

Before we implement sequencers, we need to understand a fundamental problem: **threads can see memory operations in different orders**.

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
- ready_flag = 1 (exits wait loop)
- slot[0].value = 0 (old value, not 42!)
```

This is called a **memory ordering violation**.

### **Why CPUs Reorder**

Modern CPUs reorder operations for performance:

1. **Out-of-order execution** — Execute instructions in any order that doesn't change single-threaded behavior
2. **Store buffers** — Writes go to a local buffer first, then drain to the cache hierarchy later. Other cores can't see buffered writes until they drain.
3. **Compiler reordering** — The compiler may reorder memory operations for optimization, as long as single-threaded semantics are preserved

**Example timeline:**

```
Time  Thread 1 (Producer)              Thread 2 (Consumer)
----  ---------------------------      ---------------------------
t0    slot[0].value = 42 (in store buf)
t1    ready_flag = 1 (in cache)
t2                                     ready_flag → 1
t3                                     slot[0].value → 0 (not yet visible!)
t4    slot[0].value drained from store buffer (visible to Core 2)
```

**Key insight:** Thread 2 sees the ready flag before the actual data!

### **Architecture Differences**

**x86-64 (Total Store Order — TSO):**
- Stores are never reordered with other stores
- Stores are not reordered with prior loads
- Loads may be reordered with prior stores (StoreLoad reordering)
- **Relatively strong** — fewer reorderings possible

**ARM/PowerPC (Weak Memory Models):**
- Almost any reordering is possible
- Much more aggressive optimization
- **Requires explicit barriers** for ordering

**Source:** Intel/ARM architecture manuals, "A Primer on Memory Consistency and Cache Coherence" (Sorin et al.)

---

## Memory Ordering: The Solution

We tell the CPU **and the compiler**: **"Don't reorder these operations!"**

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
                                  slot[0].value → 42
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
let value = slot[0].value;  // Guaranteed to see 42
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
auto value = slot[0].value;  // Guaranteed to see 42
```

**Java:**
```java
// Producer — using VarHandle API (Java 9+)
// Traditional 'volatile' keyword also provides acquire/release semantics
// on every read/write, but VarHandle gives finer-grained control.
slot[0].value = 42;
READY_FLAG.setRelease(this, 1);  // Release semantics

// Consumer
while ((int) READY_FLAG.getAcquire(this) != 1) {
    Thread.onSpinWait();
}
int value = slot[0].value;  // Guaranteed to see 42
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

#### **Relaxed — "I don't care about order"**

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

#### **Acquire — "Show me everything before the Release"**

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

#### **Release — "Everything before me is done"**

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

#### **AcqRel — "Both Acquire and Release"**

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

#### **SeqCst — "Strongest ordering"**

Use when you need a full memory barrier (rare). Includes a "StoreLoad" fence.

**Example: Checking consumer positions after publishing**
```rust
// Publish cursor, then check if consumers have caught up
// SeqCst prevents the CPU from reordering the store before the subsequent loads
cursor.store(current, Ordering::SeqCst);
let min = get_minimum_consumer_position();
```

**Why SeqCst:** Ensures the cursor store is visible before we read consumer positions. Critical on ARM/PowerPC.

---

### **Visualizing Memory Ordering: A Conceptual Model**

The examples above show **what** each ordering does conceptually. To build intuition for **how** to use them, let's visualize the producer-consumer scenario with different orderings.

**Important caveat:** The diagrams below are a *conceptual model*, not a precise description of hardware behavior. Real CPUs use cache coherency protocols like MESI, multi-level cache hierarchies (L1/L2/L3), and various microarchitectural optimizations. The key concept is correct: Release constrains the *ordering* of stores, and Acquire constrains the *visibility* of those stores to the reading thread. The exact mechanisms differ by architecture.

**Scenario:** Producer writes data to `slot[0]`, then signals `ready_flag = 1`. Consumer waits for `ready_flag`, then reads `slot[0]`.

---

#### **Without Ordering (Broken — Using Relaxed)**

```
Time  Producer (Core 1)                Consumer (Core 2)
----  ---------------------------      ---------------------------
t0    slot[0].value = 42
      └─> Pending in store buffer      (Not visible yet!)

t1    ready_flag.store(1, Relaxed)
      └─> Propagated to cache           (Visible to other cores!)

t2                                     ready_flag.load(Relaxed) → 1
                                        └─> Cache hit! Exits wait loop

t3                                     slot[0].value → 0
                                        └─> Reads stale value!

t4    value=42 propagated to cache     (Too late! Consumer already read!)
```

**Problem:** Consumer sees `ready_flag = 1` before `value = 42` is visible!

**Why this happens:**
1. **No ordering constraint** — Relaxed allows `ready_flag` to become visible before `value`
2. **Store buffer reordering** — Without Release, stores may drain from the store buffer in any order (on weakly-ordered CPUs), or the compiler may reorder them (on all CPUs)
3. **No happens-before** — Without an Acquire/Release pair, the consumer has no guarantee of seeing the producer's prior writes

---

#### **With Release/Acquire (Correct)**

```
Time  Producer (Core 1)                Consumer (Core 2)
----  ---------------------------      ---------------------------
t0    slot[0].value = 42
      └─> Pending in store buffer

t1    ready_flag.store(1, Release)
      └─> Ordering constraint:         ← Release ensures this
          all prior stores ordered
          before this store

t2                                     ready_flag.load(Acquire) → 1
                                        └─> Ordering constraint:
                                            all stores from before
                                            the Release are now visible

t3                                     slot[0].value → 42
                                        └─> Sees the correct value!
```

**Key insight:**
- **Release** constrains store ordering: prior writes cannot be reordered past this point
- **Acquire** constrains load ordering: subsequent reads see all stores from before the paired Release

---

#### **Comparison: Side-by-Side**

| Aspect | Relaxed (Broken) | Release/Acquire (Correct) |
|--------|------------------|---------------------------|
| **Store ordering** | Unconstrained | Constrained by Release |
| **Load visibility** | May see stale data | Sees all prior stores via Acquire |
| **Visibility** | Out of order | Ordered |
| **Consumer sees** | `ready=1, value=0` | `ready=1, value=42` |


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
**⚠️ Warning:** x86 TSO guarantees store-store ordering at the *hardware* level, so the CPU won't reorder these two stores. However, `Relaxed` does **not** prevent the *compiler* from reordering non-atomic operations around these atomic stores. The compiler is free to move `slot[0].value = 42` (a non-atomic write) after the `ready_flag` store, because `Relaxed` imposes no ordering constraints on surrounding memory. This code is **broken even on x86** — you need `Release` to prevent compiler reordering.

**Release:**
```asm
mov    QWORD PTR [rdi], 42      ; Write value to slot[0]
; (compiler barrier — prevents compiler reordering)
mov    DWORD PTR [rsi], 1       ; Write 1 to ready_flag
```
**Cost:** Free on x86 (TSO already provides store-store ordering at the hardware level)
**Benefit:** Compiler won't reorder; behavior is portable across architectures

**SeqCst:**
```asm
mov    QWORD PTR [rdi], 42      ; Write value to slot[0]
xchg   DWORD PTR [rsi], eax     ; Atomic exchange (implicit full barrier)
; Alternative: mov + mfence
; mov    DWORD PTR [rsi], 1
; mfence                        ; Full memory fence AFTER store
```
**Cost:** ~20-30 cycles (XCHG has implicit LOCK prefix, or MFENCE instruction)
**When needed:** StoreLoad fence (publish cursor, then read consumer positions)

---

#### **ARM64 (AArch64) Assembly**

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
stlr   w2, [x3]                 ; Store-release: ready_flag
```
**Cost:** Modest (STLR instruction has ordering constraints built in)
**Note:** ARMv8+ uses dedicated `STLR` (store-release) and `LDAR` (load-acquire) instructions rather than separate barriers. These are more efficient than the older ARMv7 approach of `STR` + `DMB`.

**SeqCst:**
```asm
str    x0, [x1]                 ; Write value to slot[0]
stlr   w2, [x3]                 ; Store-release: ready_flag
dmb    ish                      ; Full barrier for StoreLoad ordering
```
**Cost:** Higher (STLR + DMB barrier)
**Note:** The additional `DMB ISH` provides the StoreLoad fence that SeqCst requires beyond Release semantics. Exact instruction sequences vary by compiler and context.

---

**Key insights:**
1. **x86 is cheap** — Total Store Order (TSO) provides store-store ordering for free at the hardware level. Release is a compiler-only fence on x86.
2. **ARM requires explicit instructions** — Weak memory model requires `STLR`/`LDAR` or barrier instructions
3. **SeqCst is always more expensive** — Full fence on all architectures
4. **Release/Acquire is the sweet spot** — Sufficient for most use cases, cheapest correct option

**Source:** Compiler Explorer (godbolt.org), Intel/ARM architecture manuals

---

### **Performance Summary**

| Ordering | Cost (x86) | Cost (ARM) | What It Does |
|----------|------------|------------|--------------|
| **Relaxed** | Free | Free | Atomic operation only, no ordering |
| **Acquire** | Free (compiler barrier) | Modest (LDAR) | Prevents reordering of subsequent reads |
| **Release** | Free (compiler barrier) | Modest (STLR) | Prevents reordering of prior writes |
| **AcqRel** | Free (compiler barrier) | Modest (STLR/LDAR) | Both Acquire + Release |
| **SeqCst** | ~20-30 cycles (XCHG/MFENCE) | Higher (STLR + DMB) | Full barrier (includes StoreLoad fence) |

**Architecture-specific notes:**

**x86-64 (Total Store Order):**
- Acquire/Release are compiler-only fences (TSO provides hardware ordering)
- SeqCst requires MFENCE or XCHG (~20-30 cycles)
- Most production HFT systems have historically run on x86 (though ARM adoption is growing)

**ARM/PowerPC (Weak Memory Models):**
- Acquire/Release use dedicated instructions (LDAR/STLR on ARMv8+)
- SeqCst requires additional barrier (DMB)
- **Important:** Always test on target architecture!

**Source:** "Rust Atomics and Locks" (Mara Bos), Intel/ARM architecture manuals

---

### **How to Choose the Right Ordering: Decision Flowchart**

When you need to choose an ordering, use this decision tree:

```
START: I need to use an atomic operation
│
├─> Q1: Is this a read-modify-write (fetch_add, compare_exchange)?
│   │
│   ├─> YES → go to Q6 (RMW section)
│   │
│   └─> NO → continue to Q2
│
├─> Q2: Does this operation synchronize with other memory?
│   │   (i.e., does other data depend on this atomic?)
│   │
│   ├─> NO: Use Relaxed
│   │   └─> Example: Statistics counter, no other data depends on it
│   │
│   └─> YES: Continue to Q3
│
├─> Q3: Am I writing (store) or reading (load)?
│   │
│   ├─> WRITING (store):
│   │   │
│   │   ├─> Q4: Does other data need to be visible before this write?
│   │   │   │
│   │   │   ├─> YES:
│   │   │   │   │
│   │   │   │   ├─> Q5: Do I also need a StoreLoad fence?
│   │   │   │   │   (i.e., will I read other atomics immediately after?)
│   │   │   │   │
│   │   │   │   ├─> YES: Use SeqCst
│   │   │   │   │   └─> Example: Publish cursor, then read consumer positions
│   │   │   │   │
│   │   │   │   └─> NO: Use Release
│   │   │   │       └─> Example: Write data, then signal "ready"
│   │   │   │
│   │   │   └─> NO: Use Relaxed
│   │
│   └─> READING (load):
│       │
│       ├─> Need to see writes from a Release? → Use Acquire
│       │   └─> Example: Wait for "ready" flag, then read data
│       │
│       └─> Need total ordering across all threads? → Use SeqCst (rare!)
│
└─> Q6: Read-modify-write operations (fetch_add, compare_exchange)
    │
    ├─> Does it synchronize with other threads?
    │   │
    │   ├─> NO: Use Relaxed
    │   │   └─> Example: Statistics counter via fetch_add
    │   │
    │   └─> YES: What kind of synchronization?
    │       │
    │       ├─> Need to see others AND be seen: Use AcqRel
    │       │   └─> Example: Multiple producers claiming sequences
    │       │
    │       ├─> Only need to be seen (Release): Use Release
    │       │   └─> Example: Consumer updating position (others read it)
    │       │
    │       └─> Only need to see others (Acquire): Use Acquire (rare for RMW)
    │
    └─> Do I need a StoreLoad fence? → Use SeqCst (very rare!)
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

3. **This is Pattern 1** (flag-based synchronization) — see below!

**Decision tree path:**
- Q2: Synchronizes? YES (work_item depends on tail)
- Q3: Writing or reading? WRITING (tail.store)
- Q4: Other data visible first? YES (work_item)
- → Use Release

If you got this right, you understand memory ordering!

</details>

---

### **Common Patterns: Match Your Problem**

Most memory ordering problems fall into one of these patterns. Recognize your problem, apply the pattern!

---

#### **Pattern 1: Flag-Based Synchronization**

**Problem:** Signal that data is ready

```rust
// Producer
data.write(value);
ready_flag.store(true, Ordering::Release);

// Consumer
while !ready_flag.load(Ordering::Acquire) {
    std::hint::spin_loop();
}
let value = data.read();
```

**Why:** Release makes `data` write visible before `ready_flag`. Acquire sees `data` after seeing `ready_flag`.

**When to use:** Signaling completion, event notification, lazy initialization (though in practice, prefer `std::sync::OnceLock` for lazy init).

---

#### **Pattern 2: Sequence Counter (Multi-Producer)**

**Problem:** Multiple producers claiming slots atomically

```rust
let my_slot = counter.fetch_add(1, Ordering::AcqRel);
```

**Why:** Acquire part sees all previous claims. Release part makes this claim visible.

**When to use:** Multi-producer sequencer, work distribution, resource allocation.

---

#### **Pattern 3: Statistics/Metrics (No Synchronization)**

**Problem:** Just counting, no other data depends on this

```rust
metrics.fetch_add(1, Ordering::Relaxed);
let total = metrics.load(Ordering::Relaxed);
```

**Why:** No synchronization needed. Just need atomicity (no lost updates).

**When to use:** Performance counters, statistics, metrics that don't affect correctness.

---

#### **Pattern 4: Publish-Then-Check (StoreLoad Fence)**

**Problem:** Update cursor, then check consumer positions

```rust
cursor.store(next, Ordering::SeqCst);  // StoreLoad fence
let min_consumer = get_minimum_consumer();
```

**Why:** Without SeqCst, the CPU might read stale consumer positions *before* the cursor store is visible. SeqCst prevents this StoreLoad reordering.

**When to use:** Disruptor sequencer (publish cursor, check consumers). Rare pattern — most problems don't need StoreLoad.

---

### **Pattern Matching Summary**

| Your Problem | Pattern | Ordering |
|--------------|---------|----------|
| "Signal that data is ready" | Pattern 1 (Flag) | Release/Acquire |
| "Multiple producers claiming slots" | Pattern 2 (Counter) | AcqRel |
| "Just counting, no dependencies" | Pattern 3 (Metrics) | Relaxed |
| "Publish cursor, then check consumers" | Pattern 4 (StoreLoad) | SeqCst |

**Key insight:** 90% of problems are Pattern 1 or Pattern 2!

---

### **Confidence Check**

You should now be able to:

1. **Visualize** what happens conceptually for each ordering
2. **Recognize** which pattern your problem matches
3. **Choose** the right ordering using the decision flowchart
4. **Explain** why you chose that ordering (not just cargo-culting)
5. **Distinguish** between compiler reordering and CPU reordering

---

## Key Takeaways

1. **Memory ordering is a CPU problem** — Not specific to Rust, C++, or Java. All languages must solve it.

2. **Release/Acquire is the workhorse** — Sufficient for most producer-consumer patterns. Free on x86, modest cost on ARM.

3. **SeqCst is rarely needed** — Only for StoreLoad fences. If you're using it everywhere, you're probably over-synchronizing.

4. **x86 is forgiving, ARM is not** — Code that "works" on x86 with wrong orderings will break on ARM. Always use correct orderings for portability.

5. **Match patterns, don't guess** — Most problems are flag-based (Pattern 1) or counter-based (Pattern 2). Recognize the pattern, apply the solution.

---

## Next Up: Sequencer Implementation

In **Part 3B**, we'll use these memory ordering concepts to build actual sequencers:

- **Evolution from naive to optimized** — Mutex → Atomic → RAII → Single-producer
- **SingleProducerSequencer** — Fast path with no atomic contention (~10ns per claim)
- **MultiProducerSequencer** — CAS-based coordination for multiple producers (~50-200ns per claim)
- **RAII pattern** — How Rust's type system prevents "forgot to publish" bugs

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
- [The Rustonomicon — Atomics](https://doc.rust-lang.org/nomicon/atomics.html)
- [Rust Atomics and Locks](https://marabos.nl/atomics/) (Mara Bos, 2023)

---

**Next:** [Part 3B — Sequencer Design & Implementation →](post3b.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*