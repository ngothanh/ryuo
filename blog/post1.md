# Building a Disruptor in Rust: Ryuo — Part 1: Why Queues Are Killing Your Latency

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 1 of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Basic understanding of concurrency, familiarity with Rust

---

## The Sub-Microsecond Problem

Your algorithm is optimized. Every hot path is profiled. Memory allocations are hand-tuned. But your system still can't reliably hit **sub-microsecond p99.9 latency**.

The culprit? **Your message queue.**

To put the stakes in concrete terms: a co-located market maker targeting < 1μs round-trip, a risk engine that must clear orders in < 5μs, or a market-data pipeline at 50 M updates/sec — all of them share the same ceiling. A single queue hop through a traditional `Mutex`-backed queue can cost **1–10μs** under contention, an eternity when your entire latency budget is measured in hundreds of nanoseconds.

Traditional queues add latency at every pipeline stage, and that compounds. Here's why — and what we're going to build to fix it.

---

## The Hidden Costs of Traditional Queues

Consider a typical bounded queue implementation:

```rust
use std::sync::{Arc, Mutex, Condvar};
use std::collections::VecDeque;

pub struct BoundedQueue<T> {
    queue: Mutex<VecDeque<T>>,
    not_empty: Condvar,
    not_full: Condvar,
    capacity: usize,
}

impl<T> BoundedQueue<T> {
    pub fn push(&self, item: T) {
        let mut queue = self.queue.lock().unwrap();
        
        // Wait if full
        while queue.len() >= self.capacity {
            queue = self.not_full.wait(queue).unwrap();
        }
        
        queue.push_back(item);
        self.not_empty.notify_one();
    }
    
    pub fn pop(&self) -> T {
        let mut queue = self.queue.lock().unwrap();
        
        // Wait if empty
        while queue.is_empty() {
            queue = self.not_empty.wait(queue).unwrap();
        }
        
        let item = queue.pop_front().unwrap();
        self.not_full.notify_one();
        item
    }
}
```

This is thread-safe, bounded, and uses condition variables for efficient waiting. But here's what happens on **every single operation**:

### Latency Breakdown: A Single `push()` Call

**Typical costs on modern x86-64 hardware** (based on published CPU documentation and academic papers):

| Operation | Typical Range | Why It Matters |
|-----------|---------------|----------------|
| **Lock (uncontended, L1 hit)** | 20-50ns | Full `pthread_mutex_lock` path: function call + `cmpxchg` + Acquire barrier. The raw `cmpxchg` instruction alone is ~1-3ns, but `Mutex::lock()` is not just the instruction. |
| **Lock (uncontended, cache cold)** | 50-150ns | Same lock path, but must first fetch the lock's cache line from L2/L3 before the CAS |
| **Lock (contended)** | 1-10μs | Futex syscall, context switch, scheduler involvement |
| **Cache miss (L3)** | 10-40ns | Cache coherency protocol |
| **Cache miss (RAM)** | 50-200ns | Memory controller latency, NUMA effects |
| **Condvar notification (no waiter)** | 5-30ns | Just an atomic write to the futex word — no syscall needed |
| **Condvar notification (thread waiting)** | 200-500ns | Must call `futex(FUTEX_WAKE)` to unpark the sleeping thread. In the queue scenario, the consumer IS waiting, so this is the path taken. |
| **Context switch** | 1-10μs | Kernel scheduler, TLB flush |

**Sources:**
- Intel 64 and IA-32 Architectures Optimization Reference Manual
- "What Every Programmer Should Know About Memory" (Ulrich Drepper, 2007)
- Linux kernel futex documentation

**Key insight:** Latency varies significantly based on:
- CPU architecture (Intel vs AMD vs ARM)
- NUMA topology (same socket vs cross-socket: 2-3x difference)
- System load (contention multiplies latency)

In a multi-stage pipeline, these costs compound. The exact impact depends on your specific hardware and workload.

---

## Why Locks Are the Enemy of Latency

### The Kernel Arbitration Problem

When a thread can't acquire a lock, the kernel must:

1. **Put the thread to sleep** (context switch: ~1-10μs)
2. **Wake it up later** (another context switch)
3. **Flush the TLB** (translation lookaside buffer - the CPU's cache of virtual-to-physical address mappings)
4. **Pollute the cache** (new thread's working set evicts old data)

Even "uncontended" locks have overhead. Acquiring a lock requires:
- An **atomic operation** (a `lock`-prefixed instruction that serializes access across all cores)
- A **memory fence** (prevents the CPU and compiler from reordering memory accesses across the barrier; on x86-64, `lock cmpxchg` implies a full fence implicitly)

These operations constrain out-of-order execution and instruction-level parallelism, reducing throughput even when no other thread is competing.

### When Locks Are Actually Fine

Before we optimize, let's be honest about when locks are acceptable:

**Locks are fine when:**
- Your critical section is >1μs (lock overhead is <10% of total time)
- Throughput is <10K ops/sec (contention is negligible)
- Your bottleneck is elsewhere (database: 1-10ms, network: 100μs-10ms)

**Example:** A web API handling 1K requests/sec with 10ms database queries doesn't need lock-free queues. The lock adds 50ns, the database adds 10,000,000ns. You're optimizing the wrong thing.

**Locks become a problem when:**
- Critical section is tiny (<100ns) → lock overhead dominates
- High throughput (>100K ops/sec) → contention becomes likely
- Tail latency matters (p99.9+) → occasional contention causes spikes

**This is why HFT needs lock-free:** HFT systems process 1M+ events/sec with <100ns per event. A single lock contention event (5μs) ruins p99.9.

### The Contention Cascade

Under load, contention creates a **positive feedback loop**:

```
More load → More contention → Longer lock hold times → More threads blocked
→ More context switches → More cache misses → Even longer lock hold times
```

Under heavy contention, a significant fraction of CPU time can shift to kernel futex operations rather than doing actual work — the exact proportion depends on lock-hold time and thread count, but the feedback loop is the mechanism that causes p99.9 spikes to be orders of magnitude worse than p50.

---

## The Cache Miss Catastrophe

Modern CPUs are **fast**—4-5 GHz, multiple instructions per cycle. But memory is **slow**:

**Typical latencies on modern x86-64 CPUs** (approximate, varies by architecture):

| Memory Level | Latency | Cycles @ 4GHz | Bandwidth |
|--------------|---------|---------------|-----------|
| L1 cache | ~1ns | 4 cycles | ~1 TB/s |
| L2 cache | ~3-5ns | 12-20 cycles | ~500 GB/s |
| L3 cache | ~10-40ns | 40-160 cycles | ~100-200 GB/s |
| RAM (local NUMA) | ~50-100ns | 200-400 cycles | ~50 GB/s |
| RAM (remote NUMA) | ~100-300ns | 400-1200 cycles | ~25 GB/s |

**Source:** Intel optimization manuals, "Computer Architecture: A Quantitative Approach" (Hennessy & Patterson)

A single RAM access costs **50-300ns**—the same as executing **200-1200 instructions**. Traditional queues cause cache misses because:

1. **False sharing**: Producer and consumer modify adjacent memory, causing cache line invalidation
2. **Unpredictable access patterns**: CPU prefetcher can't help with random access
3. **Poor locality**: Queue metadata scattered across multiple cache lines

---

## The NUMA Problem (Bigger Than Your Queue)

Modern servers have multiple CPU sockets, each with its own memory controller. This is called **NUMA** (Non-Uniform Memory Access). Accessing memory on a remote socket is typically **2-3x slower**:

```
┌─────────────────────┐         ┌─────────────────────┐
│   Socket 0          │         │   Socket 1          │
│  ┌───────────────┐  │         │  ┌───────────────┐  │
│  │ CPUs          │  │         │  │ CPUs          │  │
│  └───────────────┘  │         │  └───────────────┘  │
│  ┌───────────────┐  │  Inter- │  ┌───────────────┐  │
│  │ Local RAM     │  │  connect│  │ Local RAM     │  │
│  └───────────────┘  │ <-----> │  └───────────────┘  │
└─────────────────────┘         └─────────────────────┘
   Local: ~50-100ns               Remote: ~100-300ns
```

**Typical impact** (based on Intel/AMD NUMA systems):

| Operation | Local NUMA | Remote NUMA | Typical Penalty |
|-----------|------------|-------------|-----------------|
| Memory read | 50-100ns | 100-300ns | **2-3x** |
| Atomic operation | 10-50ns | 50-200ns | **3-5x** |
| Cache line transfer | 20-50ns | 50-150ns | **2-3x** |

**Source:** "NUMA (Non-Uniform Memory Access): An Overview" (Christoph Lameter, 2013), Intel optimization guides

**The fix:** Pin your threads and allocate memory on the same NUMA node.

```bash
# Pin producer to CPU 0 (socket 0), allocate on node 0
numactl --cpunodebind=0 --membind=0 ./producer

# Pin consumer to CPU 1 (same socket!), allocate on node 0
numactl --cpunodebind=0 --membind=0 ./consumer
```

**In Rust:**
```rust
// Allocate ring buffer on a specific NUMA node (requires libnuma)
use libnuma_sys::*;

unsafe {
    // ⚠️  numa_set_preferred() is ADVISORY — the kernel may still allocate
    // on a remote node under memory pressure. For HFT systems that require
    // a hard guarantee, use numa_alloc_onnode() directly, or mbind() with
    // MPOL_BIND on the allocated region. The numactl(8) command-line wrapper
    // uses MPOL_BIND and is the safest starting point.
    numa_set_preferred(0);
    let ring_buffer = RingBuffer::new(1024, || Event::default());
}
```

**For production HFT**, prefer a hard binding:
```bash
# Hard-bind both CPU and memory to node 0 (MPOL_BIND — no remote fallback)
numactl --cpunodebind=0 --membind=0 ./your_process
```

**Tradeoff:** NUMA pinning gives you **2-3x latency improvement** but reduces scheduling flexibility. For HFT with a fixed thread topology, it's essential.

**Bottom line:** A perfect lock-free queue on the wrong NUMA node is slower than a mutex on the right NUMA node. Fix your topology first, then optimize your queue.

---

## The Allocation Tax

Every `push()` might trigger a reallocation:

```rust
queue.push_back(item);  // Might call malloc() internally
```

Memory allocation is **expensive**:
- **Best case** (allocator's per-thread cache has free memory): ~10-30ns — jemalloc and mimalloc tcache fast paths benchmark in this range on modern x86-64
- **Typical case** (must acquire lock on global allocator): ~100-200ns (400-800 cycles)
- **Worst case** (must ask OS for more memory via system call): ~1-10μs (4,000-40,000 cycles)

Modern allocators (like jemalloc or mimalloc) maintain small caches of free memory per thread to avoid locking. But even the fast path adds tens of nanoseconds.

Even worse, allocations cause **GC pressure** in garbage-collected languages (Java, Go). The LMAX team found that GC pauses were their #1 latency killer, which is why they invented the Disruptor pattern.

---

## What We're Going to Build: Ryuo

**Ryuo** (竜王, "Dragon King" in Japanese—the highest rank in shogi) is a Rust implementation of the LMAX Disruptor pattern. It achieves **sub-microsecond latency** by eliminating all the problems above:

### Design Principles

1. **Lock-free**: No mutexes, no kernel involvement
2. **Pre-allocated**: Zero allocations after initialization
3. **Cache-friendly**: Sequential access, cache-line padding
4. **Mechanical sympathy**: Designed with CPU architecture in mind

### Design Targets (Not Yet Measured)

These are the goals the design is optimized toward. Actual numbers will be measured in Post 15 using rigorous methodology (HdrHistogram, coordinated omission correction, multiple hardware configurations). They are derived from the original LMAX Java Disruptor's published results on 2011 hardware — a Rust implementation on modern hardware may differ.

- **Latency**: targeting 50-200ns mean (p99 < 1μs) — based on LMAX paper baseline; Rust has no GC, which removes the main noise source
- **Throughput**: targeting 10M+ messages/sec per core — based on LMAX's published 25M ops/sec single-producer result
- **Scalability**: designed for linear scaling (no shared lock in the hot path), but CAS contention with multiple producers degrades this

### Why VecDeque Isn't Fast Enough

You might ask: "Why not just use `VecDeque<T>` with a `Mutex`?" `VecDeque` is actually pretty good—it's a ring buffer backed by contiguous memory. But it has three problems for HFT:

**1. False sharing:** Head and length counters are in the same struct (same cache line)

```rust
// Actual Rust std::collections::VecDeque layout (simplified for illustration):
struct VecDeque<T> {
    head: usize,  // Modified by consumer (pop_front)
    len:  usize,  // Modified by both producer and consumer ← Same cache line!
    buf: RawVec<T>,
}
// Note: There is no explicit `tail` field. Tail is computed as (head + len) & mask.
// Both `head` and `len` are hot on every operation — false sharing is real.
```

Every push/pop causes cache line invalidation between cores. With Disruptor, we put producer and consumer metadata in separate cache lines:

```rust
// NOTE: 64-byte cache lines are standard on x86-64.
// ARM64 (Apple Silicon, Ampere, AWS Graviton) uses 128-byte lines.
// Post 2B covers the full cross-platform story; for now we assume x86-64.
#[repr(align(64))]  // Force 64-byte alignment (one x86-64 cache line)
struct Producer {
    cursor: AtomicI64,
    _pad: [u8; 56],  // AtomicI64 = 8 bytes; pad to fill the 64-byte line
}

#[repr(align(64))]
struct Consumer {
    sequence: AtomicI64,
    _pad: [u8; 56],
}
```

**2. Modulo arithmetic:** Uses `index % capacity` instead of `index & mask`

```rust
// VecDeque (slow - division instruction)
let index = self.head % self.capacity;

// Disruptor (fast - bitwise AND, 1 cycle)
let index = self.head & self.mask;  // Only works if capacity is power-of-2
```

On modern x86-64, a 64-bit `DIV` instruction has a latency of **35–90 cycles** (varies by µarch: ~36 cycles on Skylake, ~35-88 cycles on Zen 3) and a throughput of roughly one per 21–74 cycles. A 64-bit `AND` has a latency of **1 cycle** and can execute 3–4 per cycle. The difference is not "3-11 cycles" — it is an order of magnitude or more.

**Source:** Agner Fog, *Instruction tables*, 2024 — tables for Skylake, Zen3, and Golden Cove; Intel 64 and IA-32 Architectures Optimization Reference Manual, §3.5.1

**3. No cache-line padding:** Producer/consumer metadata not isolated

```rust
// VecDeque: head + len + buf all in one struct → single hot cache line
//   → producer and consumer fight over the same line on every op

// Disruptor: producer and consumer live on different cache lines entirely
//   → producer writes cache line 0, consumer writes cache line 1
//   → no MESI invalidation traffic between them
```

**Expected performance characteristics** (based on design principles, not measured):
- `VecDeque` with `Mutex`: Lock overhead + false sharing
- `crossbeam::channel`: Lock-free but still has some coordination overhead
- Disruptor-style: Minimal coordination, cache-friendly

**Tradeoff:** Disruptor-style design is more complex but eliminates several sources of overhead. Worth it when you need the absolute lowest latency.

**We'll measure actual performance in Post 15** with proper benchmarking methodology.

### What the LMAX Paper Shows

The original LMAX Disruptor paper (2011) compared Java implementations:

| Implementation | Mean Latency | Throughput |
|----------------|--------------|------------|
| ArrayBlockingQueue (Java) | 32,757ns | ~1M ops/sec |
| LinkedBlockingQueue (Java) | 43,194ns | ~700K ops/sec |
| **LMAX Disruptor (Java)** | **52ns** | **25M ops/sec** |

**Source:** "Disruptor: High performance alternative to bounded queues for exchanging data between concurrent threads" (LMAX, 2011)

**Test conditions:** Single producer, single consumer, 3 event handlers, Intel Core i7 920 @ 2.67GHz

**Note:** These numbers are from 2011 (14 years ago). Modern hardware is faster, but the relative improvement (~600x) remains similar.

**Key insight:** The Disruptor pattern achieved **~600x lower latency** than traditional Java queues. This is primarily due to:
1. Lock-free design (no kernel involvement)
2. Pre-allocation (no GC pressure)
3. Cache-line padding (no false sharing)
4. Sequential memory access (CPU prefetcher friendly)

**Important caveats:**
- These are Java numbers (GC overhead affects traditional queues more)
- Synthetic benchmark (minimal event handler work)
- Single-threaded producer/consumer (contention-free)
- Specific hardware (results vary by CPU architecture)

**What to expect in Rust:**
- Rust has no GC, so the gap between traditional queues and Disruptor is smaller
- `crossbeam::channel` is already quite fast
- Disruptor-style design can achieve ~20-100ns per hop
- Actual performance depends heavily on your workload and hardware

**We'll benchmark Ryuo properly in Post 15** with:
- Multiple hardware configurations
- Real-world workloads (not just counter increment)
- Proper methodology (HdrHistogram, coordinated omission correction)
- Comparison with crossbeam, flume, and std::mpsc

---

## Decision Tree: Should You Use Ryuo?

```
                         Start
                           |
                           v
              Tail latency SLA < 1ms (p99.9)?
                        /        \
                      No          Yes
                       |           |
                       v           v
               Use crossbeam   Throughput > 1M/sec?
               or std::mpsc       /        \
                                No          Yes
                                 |           |
                                 v           v
                          crossbeam       Multi-stage pipeline
                          (simpler)       (3+ stages) OR
                                          single stage < 500ns budget?
                                               /       \
                                              No        Yes
                                               |         |
                                               v         v
                                          crossbeam   Use Ryuo
```

> **Why latency first?** In HFT, throughput and latency are coupled but latency is the primary constraint. A risk engine processing 50K orders/sec with a p99.9 < 5μs budget needs Ryuo. A telemetry pipeline at 5M/sec with p99 < 10ms does not — `crossbeam` handles it cleanly with far less complexity.

### Concrete Examples

| Use Case | Throughput | Tail Latency | Stages | Recommendation | Why |
|----------|------------|--------------|--------|----------------|-----|
| Web API | 10K/sec | p99 < 100ms | 1 | `std::sync::mpsc` | Database is bottleneck (10ms) |
| Metrics pipeline | 500K/sec | p99 < 10ms | 3 | `crossbeam` | Good balance of speed and simplicity |
| Game engine | 1M/sec | p99 < 1ms | 5 | `crossbeam` or Ryuo | Depends on frame budget |
| HFT order book | 10M/sec | p99.9 < 100μs | 10 | **Ryuo** | Latency compounds across stages |
| Market data feed | 50M/sec | p99.9 < 10μs | 3 | **Ryuo** | Extreme throughput + strict latency |

### Rule of Thumb

**Consider Ryuo when ALL of these are true:**
1. Throughput > 1M/sec
2. Tail latency requirement < 1ms (p99 or p99.9)
3. Multi-stage pipeline (3+ stages) OR single stage with very tight latency budget

**Otherwise, start with crossbeam.** It's simpler and fast enough for most use cases.

### Example: Market Data Feed (Hypothetical)

**Scenario:** Processing market data from an exchange:
- 10M price updates/sec
- 5-stage pipeline: Parse → Normalize → Aggregate → Strategy → Order
- Target: p99.9 < 1μs end-to-end

**Analysis:**

If each queue hop adds latency, and you have 5 stages, the queue overhead compounds. Based on the LMAX paper's findings:
- Traditional queues: Hundreds of nanoseconds per hop
- Disruptor-style: Tens of nanoseconds per hop

In a tight latency budget (1μs total), every nanosecond matters. This is where Disruptor-style design becomes valuable.

**Tradeoff:** Disruptor adds significant complexity. Only worth it when:
- You've measured your current system and queues are the bottleneck
- Your latency budget is tight enough that the improvement matters
- You have the engineering resources to maintain the complexity

---

## Why Rust?

The original Disruptor is written in Java. Why rewrite it in Rust?

### 1. **No Garbage Collection**

Java's GC causes unpredictable pauses:
- **Minor GC**: 1-10ms
- **Major GC**: 10-100ms+

Even with tuning (G1GC, ZGC), you can't eliminate pauses entirely. Rust has **no GC**—memory is freed deterministically via RAII.

### 2. **Zero-Cost Abstractions**

Rust's generics are **monomorphized** (specialized) at compile time. The compiler generates a separate copy of the function for each concrete type:

```rust
// Generic function
fn process<H: EventHandler>(handler: &H, event: &Event) {
    handler.on_event(event);  // Direct function call, often inlined
}

// Compiler generates specialized versions:
// fn process_for_LogHandler(handler: &LogHandler, event: &Event) { ... }
// fn process_for_MetricsHandler(handler: &MetricsHandler, event: &Event) { ... }
```

This means **zero runtime overhead**—the CPU knows exactly which function to call.

Java's generics use **type erasure** and **virtual dispatch** (looking up the function address in a vtable at runtime). With a warm branch predictor and a JIT-compiled monomorphic call site, this is **~2-5ns** per call. Cold or megamorphic call sites — where the JIT cannot predict the target — run at **10-20ns**. Steady-state HFT hot paths are typically in the 2-5ns range, but cannot be eliminated by the JVM the way Rust eliminates them at compile time.

### 3. **Fearless Concurrency**

Rust's ownership system **prevents data races at compile time**:

```rust
// This won't compile (can't have &mut and & simultaneously)
let event = ring_buffer.get_mut(seq);  // &mut Event
let reader = ring_buffer.get(seq);     // ERROR: already borrowed as mutable
```

In Java/C++, data races are **undefined behavior**. In Rust, they're **compile errors**.

### 4. **Explicit Memory Layout**

Rust gives you control over memory layout:

```rust
#[repr(align(64))]  // Force 64-byte alignment (cache line)
struct CachePadding([u8; 64]);
```

This is critical for preventing **false sharing** (more on this in Post 2B).

---

## What Ryuo Doesn't Solve

Let's be clear about what this series does **not** cover:

### 1. **Network Latency (The Real Bottleneck)**

Your queue is 50ns. Your network is 500ns-5μs. Optimizing the queue is pointless if you're not using kernel bypass (DPDK, Solarflare, Mellanox).

**Illustrative HFT latency breakdown** (these are rough proportional estimates to show where time goes — not measured numbers from Ryuo):
```
Network (kernel stack):     ~10μs     ← well-documented; kernel UDP stack on a stock NIC
Event handler:              varies    ← depends entirely on your logic; often 100ns–2μs
Queue (Disruptor):          ~50-200ns ← LMAX published range; Ryuo targets this once built
Total:                      ~10.2-12μs (at this example network latency)
```

The exact handler and queue numbers will be measured in Post 15. The point — that kernel-stack networking dominates the budget — holds for any realistic handler.

**Bottom line:** Fix your network first. Optimizing a queue when your NIC adds 10μs (100-200× more than the queue) has minimal impact on end-to-end latency.

### 2. **CPU Pinning & Isolation**

Ryuo assumes your threads are pinned to specific cores and isolated from other processes. If the kernel scheduler moves your thread, you'll see 10-50μs spikes.

**You need:**
```bash
# Isolate CPUs 0-3 from kernel scheduler (add to kernel boot params)
isolcpus=0-3

# Pin producer to CPU 0
taskset -c 0 ./producer

# Disable frequency scaling (turbo boost causes jitter)
cpupower frequency-set -g performance
```

**We won't cover this.** It's system administration, not Rust code.

### 3. **Clock Synchronization**

How do you measure latency? `Instant::now()` uses `CLOCK_MONOTONIC` via the Linux VDSO. The clock's **resolution** is 1ns (backed by TSC on modern hardware), but the **call overhead** — the time spent executing `Instant::now()` — is ~15-30ns. That overhead is larger than the ring buffer's own p50 latency, so for sub-100ns benchmarks you need TSC directly:

```rust
#[inline(always)]
fn rdtsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}
```

But TSC has its own problems (drift between cores, frequency scaling). **We won't cover this.**

### 4. **Memory Allocation**

Ryuo pre-allocates the ring buffer, but your event handlers might allocate. A single `Vec::push()` can add 50-200ns.

**You need a custom allocator:**
- Arena allocator (bump pointer, no free)
- Object pool (pre-allocate, reuse)
- Stack allocation (no heap at all)

**We won't cover this.** It's orthogonal to the queue design.

### 5. **Tail Latency Debugging**

When you see a 10ms spike at p99.99, how do you debug it? You need:
- Tracing (but tracing adds overhead)
- Histograms (HdrHistogram)
- Flamegraphs (perf, bpftrace)

**We'll cover benchmarking in Post 15, but not production debugging.**

---

**The point:** Ryuo is **one piece** of an HFT system. It's necessary but not sufficient. Don't expect 50ns end-to-end latency just because your queue is 50ns.

---

## The Road Ahead

Over the next 15 posts, we'll build Ryuo from scratch:

- **Post 2A**: Ring buffer (pre-allocated, power-of-2, unsafe interior mutability)
- **Post 2B**: Cache-line padding (preventing false sharing)
- **Post 3**: Sequencers (single/multi-producer coordination)
- **Post 4**: Wait strategies (busy-spin, yielding, blocking)
- **Post 5**: Sequence barriers (dependency tracking)
- **Post 6**: Event handlers (consumer interface)
- **Post 7**: Publishing API (ergonomic producer interface)
- **Post 8**: Batch rewind (error handling)
- **Posts 9-13**: Advanced features (multi-producer, polling, dynamic handlers, etc.)
- **Post 14**: Production patterns (monitoring, backpressure, graceful shutdown)
- **Post 15**: Benchmarking (rigorous methodology, coordinated omission)
- **Post 16**: Async integration (bonus: bridging sync/async worlds)

Each post will include:
- **Complete, working code** (no hand-waving)
- **Safety proofs** (why `unsafe` is actually safe)
- **Performance analysis** (with benchmarks)
- **Common pitfalls** (what NOT to do)

---

## Next Up: The Ring Buffer

In **Part 2A**, we'll build the core data structure: a **pre-allocated, power-of-2, cache-aligned ring buffer**. You'll learn:

- Why `Vec<T>` is the wrong choice
- How to use `UnsafeCell` and `MaybeUninit` safely
- The magic of bitwise AND for indexing

Then in **Part 2B**, we'll deep-dive into cache-line padding to prevent false sharing.

**Teaser:** We'll achieve **O(1) indexing with bitwise AND (1-2 CPU cycles)** and zero allocations. Actual access time depends on cache state (1ns if in L1, 50-300ns if in RAM).

---

## References

### Papers & Documentation

1. **LMAX Disruptor Paper** (2011)
   "Disruptor: High performance alternative to bounded queues for exchanging data between concurrent threads"
   https://lmax-exchange.github.io/disruptor/files/Disruptor-1.0.pdf

2. **Intel 64 and IA-32 Architectures Optimization Reference Manual**
   https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html

3. **"What Every Programmer Should Know About Memory"** (Ulrich Drepper, 2007)
   https://people.freebsd.org/~lstewart/articles/cpumemory.pdf

4. **"NUMA (Non-Uniform Memory Access): An Overview"** (Christoph Lameter, 2013)
   https://queue.acm.org/detail.cfm?id=2513149

5. **"Computer Architecture: A Quantitative Approach"** (Hennessy & Patterson, 6th Edition)
   Standard reference for CPU performance characteristics

### Blogs & Resources

- [Mechanical Sympathy Blog](https://mechanical-sympathy.blogspot.com/) - Martin Thompson (LMAX architect)
- [Rust Atomics and Locks](https://marabos.nl/atomics/) - Mara Bos
- [Linux futex documentation](https://man7.org/linux/man-pages/man2/futex.2.html)

---

**Next:** [Part 2A — The Ring Buffer: Pre-Allocated, Power-of-2, Cache-Aligned →](post2A.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*

