# Building a Disruptor in Rust: Ryuo — Part 1: Why Queues Are Killing Your Latency

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 1 of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Basic understanding of concurrency, familiarity with Rust

---

## The 100-Microsecond Problem

Your algorithm is optimized. Every hot path is profiled. Memory allocations are hand-tuned. But your system still can't break the 100-microsecond latency barrier.

The culprit? **Your message queue.**

Traditional queues add significant latency per hop. In a system with multiple processing stages, this compounds quickly. For high-frequency trading, real-time gaming, or telemetry pipelines, this is unacceptable.

Here's why, and what we're going to build to fix it.

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
| **Lock (uncontended)** | 10-50ns | Atomic CAS + memory fence |
| **Lock (contended)** | 1-10μs | Futex syscall, context switch, scheduler |
| **Cache miss (L3)** | 10-40ns | Cache coherency protocol |
| **Cache miss (RAM)** | 50-200ns | Memory controller latency, NUMA effects |
| **Condvar notification** | 30-100ns | Atomic + potential futex wakeup |
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
- An **atomic operation** (special CPU instruction that can't be interrupted)
- A **memory fence** (forces the CPU to finish all pending memory operations before continuing)

These operations prevent the CPU from executing instructions out-of-order or in parallel, reducing throughput.

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

Under heavy contention, the majority of CPU time is spent in kernel futex operations rather than doing actual work.

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
// Allocate ring buffer on specific NUMA node (requires libnuma)
use libnuma_sys::*;

unsafe {
    numa_set_preferred(0);  // Prefer node 0
    let ring_buffer = RingBuffer::new(1024, || Event::default());
}
```

**Tradeoff:** NUMA pinning gives you **2-3x latency improvement** but reduces flexibility. If your workload is dynamic (threads move around), NUMA pinning can hurt. For HFT with fixed topology, it's essential.

**Bottom line:** A perfect lock-free queue on the wrong NUMA node is slower than a mutex on the right NUMA node. Fix your topology first, then optimize your queue.

---

## The Allocation Tax

Every `push()` might trigger a reallocation:

```rust
queue.push_back(item);  // Might call malloc() internally
```

Memory allocation is **expensive**:
- **Best case** (allocator's per-thread cache has free memory): ~50ns (200 cycles)
- **Typical case** (must acquire lock on global allocator): ~100-200ns (400-800 cycles)
- **Worst case** (must ask OS for more memory via system call): ~1-10μs (4,000-40,000 cycles)

Modern allocators (like jemalloc or mimalloc) maintain small caches of free memory per thread to avoid locking. But even the fast path adds 50ns.

Even worse, allocations cause **GC pressure** in garbage-collected languages (Java, Go). The LMAX team found that GC pauses were their #1 latency killer, which is why they invented the Disruptor pattern.

---

## What We're Going to Build: Ryuo

**Ryuo** (竜王, "Dragon King" in Japanese—the highest rank in shogi) is a Rust implementation of the LMAX Disruptor pattern. It achieves **sub-microsecond latency** by eliminating all the problems above:

### Design Principles

1. **Lock-free**: No mutexes, no kernel involvement
2. **Pre-allocated**: Zero allocations after initialization
3. **Cache-friendly**: Sequential access, cache-line padding
4. **Mechanical sympathy**: Designed with CPU architecture in mind

### Target Performance

- **Latency**: 50-200ns mean (p99 < 1μs)
- **Throughput**: 10M+ messages/sec per core
- **Scalability**: Linear scaling with cores (no contention)

### Why VecDeque Isn't Fast Enough

You might ask: "Why not just use `VecDeque<T>` with a `Mutex`?" `VecDeque` is actually pretty good—it's a ring buffer backed by contiguous memory. But it has three problems for HFT:

**1. False sharing:** Head and tail indices are in the same struct (likely same cache line)

```rust
struct VecDeque<T> {
    head: usize,  // Modified by consumer
    tail: usize,  // Modified by producer ← Same cache line!
    buf: Vec<T>,
}
```

Every push/pop causes cache line invalidation between cores. With Disruptor, we put producer and consumer metadata in separate cache lines:

```rust
#[repr(align(64))]  // Force 64-byte alignment (cache line size)
struct Producer {
    cursor: AtomicI64,
    _pad: [u8; 56],  // Padding to fill cache line
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

On modern CPUs, integer division takes ~3-11 cycles vs 1 cycle for AND. More importantly, division has lower throughput (1 per 3-6 cycles) vs AND (3-4 per cycle), limiting instruction-level parallelism.

**Source:** Intel optimization manual, Agner Fog's instruction tables

**3. No cache-line padding:** Producer/consumer metadata not isolated

```rust
// VecDeque: Everything in one struct (cache line ping-pong)
struct VecDeque { head, tail, buf }

// Disruptor: Separate cache lines (no ping-pong)
// Producer writes to cache line 0
// Consumer writes to cache line 1
// No invalidation!
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
- `crossbeam::channel` is already quite fast (typically 50-200ns per hop based on community benchmarks)
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
            Throughput > 100K/sec?
                   /     \
                 No       Yes
                 |         |
                 v         v
          Use crossbeam   Tail latency
          or std::mpsc    requirement?
                           /    \
                      p99 < 1ms  p99.9 < 100μs
                         |            |
                         v            v
                   Use crossbeam  Multi-stage
                                  pipeline?
                                   /    \
                                 No      Yes (3+ stages)
                                 |        |
                                 v        v
                           Maybe Ryuo   Use Ryuo
```

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

Java's generics use **type erasure** and **virtual dispatch** (looking up the function address in a vtable at runtime), adding 5-10ns per call.

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

This is critical for preventing **false sharing** (more on this in Post 2).

---

## What Ryuo Doesn't Solve

Let's be clear about what this series does **not** cover:

### 1. **Network Latency (The Real Bottleneck)**

Your queue is 50ns. Your network is 500ns-5μs. Optimizing the queue is pointless if you're not using kernel bypass (DPDK, Solarflare, Mellanox).

**Typical HFT latency breakdown** (example with 10μs network):
```
Network (kernel stack):     10μs      ← 96% of total
Event handler:              300ns     ← 3% of total
Queue (Disruptor):          50ns      ← 0.5% of total
Total:                      10.35μs
```

**Bottom line:** Fix your network first. Optimizing a 50ns queue when your network adds 10μs (200x more) has minimal impact on end-to-end latency.

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

How do you measure latency? `Instant::now()` uses CLOCK_MONOTONIC, which has ~20-50ns resolution. For sub-100ns latency, you need TSC (Time Stamp Counter):

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

- **Post 2**: Ring buffer (pre-allocated, cache-aligned)
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

In **Part 2**, we'll build the core data structure: a **pre-allocated, power-of-2, cache-aligned ring buffer**. You'll learn:

- Why `Vec<T>` is the wrong choice
- How to use `UnsafeCell` and `MaybeUninit` safely
- The magic of bitwise AND for indexing
- Cache-line padding to prevent false sharing

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

**Next:** [Part 2 — The Ring Buffer: Pre-Allocated, Power-of-2, Cache-Aligned →](post-02-ring-buffer.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*

