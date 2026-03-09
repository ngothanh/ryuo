# Building a Disruptor in Rust: Ryuo — Part 4: Wait Strategies — Trading Latency for CPU

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 4 of 16
**Target Audience:** Systems engineers building high-performance, low-latency applications
**Prerequisites:** Part 3B (sequencer implementation), Part 3C (usage & testing)

---

## Recap

In Parts 3A–3C, we built:
- **Memory ordering fundamentals** — Release/Acquire for visibility, SeqCst for StoreLoad fences
- **SingleProducerSequencer** — ~10ns claims with RAII auto-publish
- **MultiProducerSequencer** — `fetch_add`-based claiming with `available_buffer` for out-of-order publish tracking

But we left a critical question unanswered: **When a consumer has no data to process, what should it do?**

---

## The Waiting Problem

In our consumer loop from Part 3C, we wrote this:

```rust
while cursor.load(Ordering::Acquire) < next_sequence {
    std::hint::spin_loop();
}
```

This is a **busy-spin** — the consumer burns 100% of a CPU core waiting for data. That's fine for HFT where every nanosecond matters and you have dedicated cores. But what about:

- **Telemetry pipelines** — Processing 10K events/sec with occasional bursts. Burning a core 24/7 for 10K events is wasteful.
- **Game engines** — Multiple subsystems sharing cores. Burning a core on the audio consumer starves the physics thread.
- **Cloud deployments** — You pay per vCPU-hour. Burning cores on idle consumers is literally burning money.

**The fundamental trade-off:**

```
Low Latency ◄──────────────────────────────► Low CPU Usage

BusySpin     Yielding     Sleeping     Blocking
~50ns        ~200ns       ~1μs         ~10μs
100% CPU     ~80% CPU     ~20% CPU     ~0% CPU
```

We need a **pluggable strategy** so users can choose the right trade-off for their workload.

---

## Architecture: Where Wait Strategies Fit

```
Producer ──> Sequencer ──> Ring Buffer ──> SequenceBarrier ──> Consumer
                                               │
                                          WaitStrategy
                                          (pluggable!)
```

The `SequenceBarrier` (which we'll build in Part 5) sits between the ring buffer and the consumer. When the consumer asks "is sequence N available?", the barrier delegates to the `WaitStrategy` to decide *how* to wait.

**Key insight:** The wait strategy is the *only* component that differs between "burn a core for 50ns latency" and "sleep and wake up in 10μs". Everything else — sequencer, ring buffer, consumer logic — stays the same.

---

## Supporting Types

Before we implement the strategies, we need two supporting types that the `WaitStrategy` trait depends on:

```rust
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

/// Errors returned by wait strategies
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarrierError {
    /// The barrier was alerted (shutdown signal)
    Alerted,
    /// The wait timed out
    Timeout,
}

/// Sequence barrier — coordinates consumer access to the ring buffer.
///
/// This is a simplified version for this post. The full implementation
/// (with dependency tracking and multi-producer support) comes in Part 5.
pub struct SequenceBarrier {
    cursor: Arc<AtomicI64>,
    alerted: AtomicBool,
    wait_strategy: Box<dyn WaitStrategy>,
    dependent_sequences: Vec<Arc<AtomicI64>>,
}

impl SequenceBarrier {
    pub fn new(
        cursor: Arc<AtomicI64>,
        wait_strategy: Box<dyn WaitStrategy>,
        dependent_sequences: Vec<Arc<AtomicI64>>,
    ) -> Self {
        Self {
            cursor,
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

    /// Wait for a sequence to become available.
    pub fn wait_for(&self, sequence: i64) -> Result<i64, BarrierError> {
        self.wait_strategy.wait_for(
            sequence,
            &self.cursor,
            &self.dependent_sequences,
            self,
        )
    }
}
```

**Why `BarrierError`?** Consumers need a way to stop waiting during shutdown. Without `Alerted`, a consumer blocked on `condvar.wait()` would hang forever when the application tries to shut down. The `Timeout` variant supports time-bounded waits (useful for health checks and watchdogs).

**Why `SequenceBarrier` here?** The wait strategy needs to check the alert flag on every iteration — otherwise a shutdown signal during a busy-spin would be ignored. The barrier owns the alert state and the wait strategy, keeping the shutdown path clean.

---

## Helper: get_minimum_sequence

Every wait strategy needs to check whether the requested sequence is available. This helper computes the minimum of the cursor and all dependent sequences:

```rust
/// Returns the minimum sequence value across the cursor and all
/// dependent sequences.
///
/// This determines the highest sequence that is safe to read:
/// - `cursor`: the producer's published position
/// - `dependent_sequences`: upstream consumers in a pipeline (Part 5)
///
/// For a simple single-consumer setup, `dependent_sequences` is empty,
/// and this just returns the cursor value.
fn get_minimum_sequence(
    cursor: &AtomicI64,
    dependent_sequences: &[Arc<AtomicI64>],
) -> i64 {
    let mut minimum = cursor.load(Ordering::Acquire);
    for seq in dependent_sequences {
        let value = seq.load(Ordering::Acquire);
        minimum = minimum.min(value);
    }
    minimum
}
```

**Why Acquire?** We need to see the producer's writes that happened before the cursor update. This is the consumer side of the Release/Acquire pair from Part 3A (Pattern 1: flag-based synchronization).

---

## The WaitStrategy Trait

```rust
use std::sync::{Condvar, Mutex};
use std::time::Duration;

pub trait WaitStrategy: Send + Sync {
    /// Wait until `sequence` is available.
    ///
    /// Returns the highest available sequence (may be > requested sequence,
    /// enabling batch processing — the consumer can process all sequences
    /// up to the returned value without re-checking the barrier).
    ///
    /// # Errors
    /// - `BarrierError::Alerted` — shutdown signal received
    /// - `BarrierError::Timeout` — wait timed out (only for timeout strategies)
    fn wait_for(
        &self,
        sequence: i64,
        cursor: &AtomicI64,
        dependent_sequences: &[Arc<AtomicI64>],
        barrier: &SequenceBarrier,
    ) -> Result<i64, BarrierError>;

    /// Wake up any threads that are blocking.
    ///
    /// Called by the producer after publishing new events. For non-blocking
    /// strategies (BusySpin, Yielding, Sleeping), this is a no-op.
    /// For BlockingWaitStrategy, this calls `condvar.notify_all()`.
    fn signal_all_when_blocking(&self);
}
```

**Key design decisions:**

1. **Returns `i64`, not `bool`** — Returns the *highest* available sequence, not just "is sequence N ready?". This enables batch processing: if the consumer asks for sequence 5 and sequences 5-50 are available, it gets back 50 and can process the entire batch without re-checking.

2. **`Send + Sync`** — The wait strategy is shared between the barrier (which the consumer calls) and the producer (which calls `signal_all_when_blocking`). Both traits are required.

3. **`signal_all_when_blocking`** — This is the producer's hook to wake sleeping consumers. Without it, a `BlockingWaitStrategy` consumer would only wake up on spurious wakeups.

---

## Strategy 1: BusySpinWaitStrategy — Lowest Latency

The simplest strategy: spin in a tight loop until data is available.

```rust
/// Lowest latency (~50ns), highest CPU usage (100%).
///
/// The consumer thread never yields, sleeps, or blocks. It continuously
/// polls the cursor in a tight loop with a PAUSE hint.
///
/// **Use when:** Latency is everything and you have dedicated CPU cores.
/// Typical in HFT systems where each consumer is pinned to its own core.
///
/// **Don't use when:** You share cores with other threads, or CPU cost matters.
pub struct BusySpinWaitStrategy;

impl WaitStrategy for BusySpinWaitStrategy {
    fn wait_for(
        &self,
        sequence: i64,
        cursor: &AtomicI64,
        dependent_sequences: &[Arc<AtomicI64>],
        barrier: &SequenceBarrier,
    ) -> Result<i64, BarrierError> {
        loop {
            barrier.check_alert()?;

            let available = get_minimum_sequence(cursor, dependent_sequences);
            if available >= sequence {
                return Ok(available);
            }

            // PAUSE on x86, YIELD on ARM.
            // Without this hint:
            //   - x86: speculative execution fills the pipeline with useless
            //     loads, causing a ~25-cycle penalty when the value changes
            //   - ARM: the core doesn't signal to the interconnect that it's
            //     spinning, wasting power and memory bandwidth
            std::hint::spin_loop();
        }
    }

    fn signal_all_when_blocking(&self) {
        // No-op: busy-spin never blocks, so there's nothing to wake up.
    }
}
```

**What does `spin_loop()` actually do?**

```
x86-64:  PAUSE instruction
         - Reduces power consumption during spin loops
         - Avoids memory-order violation pipeline flush (~25 cycles saved)
         - Signals to hyper-threading sibling that this thread is spinning

ARM64:   YIELD instruction
         - Hints to the core that this is a spin loop
         - May allow the other hardware thread to run (if SMT is enabled)
         - Reduces power consumption
```

**Performance:** ~50ns p50 latency. The consumer sees new data within 1-2 cache-line transfer times (~30-50ns) because it's continuously polling.

---

## Strategy 2: YieldingWaitStrategy — Balanced

Spin for a while, then yield the CPU. Good balance between latency and CPU usage.

```rust
/// Moderate latency (~200ns), moderate CPU usage (~80%).
///
/// Spins for `spin_tries` iterations, then calls `thread::yield_now()`.
/// Yielding tells the OS scheduler "I have nothing to do right now" —
/// other runnable threads on this core get a chance to execute.
///
/// **Use when:** You want low latency but can't dedicate a full core.
/// Good for real-time audio, game engines, and interactive applications.
pub struct YieldingWaitStrategy {
    spin_tries: u32,
}

impl YieldingWaitStrategy {
    pub fn new() -> Self {
        Self { spin_tries: 100 }
    }

    pub fn with_spin_tries(spin_tries: u32) -> Self {
        Self { spin_tries }
    }
}

impl WaitStrategy for YieldingWaitStrategy {
    fn wait_for(
        &self,
        sequence: i64,
        cursor: &AtomicI64,
        dependent_sequences: &[Arc<AtomicI64>],
        barrier: &SequenceBarrier,
    ) -> Result<i64, BarrierError> {
        let mut counter = self.spin_tries;

        loop {
            barrier.check_alert()?;

            let available = get_minimum_sequence(cursor, dependent_sequences);
            if available >= sequence {
                return Ok(available);
            }

            if counter > 0 {
                counter -= 1;
                std::hint::spin_loop();
            } else {
                // yield_now() behavior is OS-specific:
                //   Linux:   sched_yield() — moves thread to end of run queue
                //   macOS:   sched_yield() — same behavior
                //   Windows: SwitchToThread() — yields to another thread on same core
                //
                // Caveat: on Linux with SCHED_OTHER (default), sched_yield()
                // may return immediately if no other runnable threads exist.
                // This effectively degrades to busy-spin in low-contention
                // scenarios — which is actually desirable (low latency when
                // no other threads need the core).
                std::thread::yield_now();
                counter = self.spin_tries;
            }
        }
    }

    fn signal_all_when_blocking(&self) {
        // No-op: yield doesn't block, so there's nothing to wake up.
    }
}
```

**Why reset the counter after yield?** After yielding, we've given other threads a chance to run. The producer may have published new data during that time. We spin again briefly to catch it with low latency before yielding again.

**Performance:** ~200ns p50 latency. The spin phase catches most data with ~50ns latency; the yield phase adds ~100-200ns when data arrives during a yield.

---

## Strategy 3: SleepingWaitStrategy — Low CPU

Progressive backoff: spin → yield → sleep. Minimizes CPU usage at the cost of latency.

```rust
/// Higher latency (~1μs), low CPU usage (~20%).
///
/// Three phases:
/// 1. Spin (retries > 100): tight loop with PAUSE hint
/// 2. Yield (retries 1-100): yield to other threads
/// 3. Sleep (retries = 0): sleep for 1ns (actual sleep is OS-dependent)
///
/// **Use when:** Throughput matters more than latency. Telemetry pipelines,
/// log aggregation, batch processing.
///
/// **Don't use when:** You need sub-microsecond latency.
pub struct SleepingWaitStrategy {
    retries: u32,
}

impl SleepingWaitStrategy {
    pub fn new() -> Self {
        Self { retries: 200 }
    }

    pub fn with_retries(retries: u32) -> Self {
        Self { retries }
    }
}

impl WaitStrategy for SleepingWaitStrategy {
    fn wait_for(
        &self,
        sequence: i64,
        cursor: &AtomicI64,
        dependent_sequences: &[Arc<AtomicI64>],
        barrier: &SequenceBarrier,
    ) -> Result<i64, BarrierError> {
        let mut counter = self.retries;

        loop {
            barrier.check_alert()?;

            let available = get_minimum_sequence(cursor, dependent_sequences);
            if available >= sequence {
                return Ok(available);
            }

            if counter > 100 {
                // Phase 1: Spin — lowest latency, highest CPU
                counter -= 1;
                std::hint::spin_loop();
            } else if counter > 0 {
                // Phase 2: Yield — moderate latency, moderate CPU
                counter -= 1;
                std::thread::yield_now();
            } else {
                // Phase 3: Sleep — highest latency, lowest CPU
                //
                // sleep(1ns) doesn't actually sleep for 1ns!
                // Actual minimum sleep times:
                //   Linux:   ~50-100μs (timer resolution + scheduler overhead)
                //   macOS:   ~1ms (coarse timer resolution)
                //   Windows: ~1-15ms (timer resolution depends on timeBeginPeriod)
                //
                // This is why SleepingWaitStrategy has ~1μs *average* latency
                // but ~10-100μs *worst-case* latency — the sleep phase dominates
                // when data arrives during a sleep.
                std::thread::sleep(Duration::from_nanos(1));
            }
        }
    }

    fn signal_all_when_blocking(&self) {
        // No-op: sleep is not interruptible via condvar, so there's nothing
        // to signal. The thread will wake up after the sleep duration and
        // re-check the condition.
    }
}
```

**Why three phases?** Each phase trades latency for CPU savings:

| Phase | Duration | Latency | CPU | When Data Arrives |
|-------|----------|---------|-----|-------------------|
| Spin (100 iterations) | ~1-2μs | ~50ns | 100% | Caught immediately |
| Yield (100 iterations) | ~10-50μs | ~200ns | ~50% | Caught after yield returns |
| Sleep (indefinite) | Until data | ~1-100μs | ~0% | Caught after sleep expires |

**Performance:** ~1μs p50 latency. Most data arrives during the spin or yield phase. The sleep phase only activates during prolonged idle periods.

---

## Strategy 4: BlockingWaitStrategy — Lowest CPU

Uses a condition variable to block the thread entirely. The OS scheduler removes the thread from the run queue until the producer wakes it up.

```rust
/// Highest latency (~10μs), lowest CPU usage (~0% when idle).
///
/// Uses a Mutex + Condvar to block the consumer thread. The producer
/// calls `signal_all_when_blocking()` after publishing to wake consumers.
///
/// **Use when:** CPU cost matters more than latency. Background processing,
/// batch jobs, systems with many consumers sharing few cores.
///
/// **Don't use when:** You need latency below ~10μs.
pub struct BlockingWaitStrategy {
    mutex: Mutex<()>,
    condvar: Condvar,
}

impl BlockingWaitStrategy {
    pub fn new() -> Self {
        Self {
            mutex: Mutex::new(()),
            condvar: Condvar::new(),
        }
    }
}

impl WaitStrategy for BlockingWaitStrategy {
    fn wait_for(
        &self,
        sequence: i64,
        cursor: &AtomicI64,
        dependent_sequences: &[Arc<AtomicI64>],
        barrier: &SequenceBarrier,
    ) -> Result<i64, BarrierError> {
        // Fast path: avoid the lock if data is already available.
        barrier.check_alert()?;
        let available = get_minimum_sequence(cursor, dependent_sequences);
        if available >= sequence {
            return Ok(available);
        }

        // Slow path: re-check *under the lock* before sleeping.
        //
        // ⚠️ THE RACE THIS FIXES:
        //
        //   Without the mutex, this sequence of events causes deadlock:
        //
        //   1. Consumer checks condition → false (no data yet)
        //   2. Producer publishes event (Release store to cursor)
        //   3. Producer calls signal_all_when_blocking() → notify_all()
        //   4. Consumer calls condvar.wait() ← MISSED the signal!
        //
        //   The notify_all() at step 3 fires while no thread is waiting,
        //   so it's lost. The consumer then enters wait() and sleeps forever.
        //
        // THE FIX:
        //
        //   By holding the mutex across BOTH the condition check AND the
        //   wait, and by requiring signal_all_when_blocking() to also
        //   acquire the mutex before calling notify_all(), we close the
        //   window:
        //
        //   - If the producer publishes between our check and our wait,
        //     it must acquire the mutex first. Either:
        //     (a) It acquires before us → we see fresh data on re-check
        //     (b) It acquires after us → we're already in wait(), wake up
        //
        //   This is the standard "check-under-lock" pattern for condvars.
        let mut guard = self.mutex.lock().unwrap();
        loop {
            barrier.check_alert()?;
            let available = get_minimum_sequence(cursor, dependent_sequences);
            if available >= sequence {
                return Ok(available);
            }
            // condvar.wait() atomically: releases the lock AND suspends.
            // When woken, it re-acquires the lock before returning.
            guard = self.condvar.wait(guard).unwrap();
        }
    }

    fn signal_all_when_blocking(&self) {
        // MUST acquire the lock before notifying.
        // This serialises with the consumer's condition check above.
        let _guard = self.mutex.lock().unwrap();
        self.condvar.notify_all();
    }
}
```

**Why is `signal_all_when_blocking` so important?**

This is the most subtle part of the entire wait strategy design. Let's trace through the race condition step by step:

```
Time  Consumer                          Producer
----  --------                          --------
t0    available = cursor.load() → -1
      (data not ready, will block)

t1                                      buffer[0] = 42
                                        cursor.store(0, Release)

t2                                      signal_all_when_blocking()
                                        └─> notify_all()
                                        └─> But nobody is waiting!
                                        └─> Signal is LOST!

t3    condvar.wait()
      └─> Sleeps forever! DEADLOCK!
```

**With the mutex fix:**

```
Time  Consumer                          Producer
----  --------                          --------
t0    guard = mutex.lock()
      available = cursor.load() → -1
      condvar.wait(guard)
      └─> Atomically: release lock + sleep

t1                                      buffer[0] = 42
                                        cursor.store(0, Release)

t2                                      signal_all_when_blocking()
                                        └─> mutex.lock() ← waits for consumer
                                        └─> notify_all()
                                        └─> Consumer wakes up!

t3    (woken by notify_all)
      guard = mutex.lock() (re-acquired)
      available = cursor.load() → 0
      return Ok(0) ✓
```

**Performance:** ~10μs p50 latency. The context switch (kernel → user space) dominates. On Linux, `futex`-based condvars add ~2-5μs; on macOS, `psynch`-based condvars add ~5-15μs.

---

## Strategy 5: PhasedBackoffWaitStrategy — Hybrid

The most configurable strategy: spin → yield → sleep, with tunable phase durations.

```rust
/// Configurable latency (~500ns), moderate CPU usage (~40%).
///
/// Three phases with configurable durations:
/// 1. Spin phase: tight loop for `spin_tries` iterations
/// 2. Yield phase: yield for `yield_tries` iterations
/// 3. Sleep phase: sleep for `sleep_nanos` nanoseconds
///
/// **Use when:** You need fine-grained control over the latency/CPU trade-off.
/// Good for systems where the workload characteristics are well-understood.
pub struct PhasedBackoffWaitStrategy {
    spin_tries: u32,
    yield_tries: u32,
    sleep_nanos: u64,
}

impl PhasedBackoffWaitStrategy {
    pub fn new(spin_tries: u32, yield_tries: u32, sleep_nanos: u64) -> Self {
        Self { spin_tries, yield_tries, sleep_nanos }
    }

    /// Preset: optimized for low-latency workloads
    pub fn low_latency() -> Self {
        Self { spin_tries: 1000, yield_tries: 100, sleep_nanos: 1 }
    }

    /// Preset: optimized for balanced workloads
    pub fn balanced() -> Self {
        Self { spin_tries: 100, yield_tries: 100, sleep_nanos: 100 }
    }

    /// Preset: optimized for low-CPU workloads
    pub fn low_cpu() -> Self {
        Self { spin_tries: 10, yield_tries: 10, sleep_nanos: 1000 }
    }
}

impl WaitStrategy for PhasedBackoffWaitStrategy {
    fn wait_for(
        &self,
        sequence: i64,
        cursor: &AtomicI64,
        dependent_sequences: &[Arc<AtomicI64>],
        barrier: &SequenceBarrier,
    ) -> Result<i64, BarrierError> {
        let mut spin_counter = self.spin_tries;
        let mut yield_counter = self.yield_tries;

        loop {
            barrier.check_alert()?;

            let available = get_minimum_sequence(cursor, dependent_sequences);
            if available >= sequence {
                return Ok(available);
            }

            if spin_counter > 0 {
                spin_counter -= 1;
                std::hint::spin_loop();
            } else if yield_counter > 0 {
                yield_counter -= 1;
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_nanos(self.sleep_nanos));
            }
        }
    }

    fn signal_all_when_blocking(&self) {
        // No-op: sleep is not interruptible via condvar.
    }
}
```

**When to use PhasedBackoff over SleepingWaitStrategy?** When you need to tune the phase durations for your specific workload. `SleepingWaitStrategy` has fixed phase boundaries (100 spins, 100 yields); `PhasedBackoffWaitStrategy` lets you adjust all three independently.

---

## Strategy 6: TimeoutBlockingWaitStrategy — Blocking with Timeout

A variant of `BlockingWaitStrategy` that returns `BarrierError::Timeout` if data doesn't arrive within a deadline. Useful for health checks, watchdogs, and graceful degradation.

```rust
/// Blocking with timeout support.
///
/// Same as BlockingWaitStrategy, but returns `BarrierError::Timeout`
/// if the sequence isn't available within `timeout`.
///
/// **Use when:** You need to detect stalled producers or implement
/// health checks. The consumer can take corrective action on timeout
/// (log a warning, switch to a backup source, etc.).
pub struct TimeoutBlockingWaitStrategy {
    mutex: Mutex<()>,
    condvar: Condvar,
    timeout: Duration,
}

impl TimeoutBlockingWaitStrategy {
    pub fn new(timeout: Duration) -> Self {
        Self {
            mutex: Mutex::new(()),
            condvar: Condvar::new(),
            timeout,
        }
    }
}

impl WaitStrategy for TimeoutBlockingWaitStrategy {
    fn wait_for(
        &self,
        sequence: i64,
        cursor: &AtomicI64,
        dependent_sequences: &[Arc<AtomicI64>],
        barrier: &SequenceBarrier,
    ) -> Result<i64, BarrierError> {
        // Fast path
        barrier.check_alert()?;
        let available = get_minimum_sequence(cursor, dependent_sequences);
        if available >= sequence {
            return Ok(available);
        }

        // Slow path with timeout
        let mut guard = self.mutex.lock().unwrap();
        let deadline = std::time::Instant::now() + self.timeout;

        loop {
            barrier.check_alert()?;
            let available = get_minimum_sequence(cursor, dependent_sequences);
            if available >= sequence {
                return Ok(available);
            }

            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(BarrierError::Timeout);
            }

            let remaining = deadline - now;
            let (new_guard, wait_result) = self.condvar.wait_timeout(guard, remaining).unwrap();
            guard = new_guard;

            // Even if wait_timeout returns due to timeout, we re-check the
            // condition above. The timeout path only triggers if the deadline
            // has truly passed AND the sequence is still unavailable.
            let _ = wait_result; // Intentionally ignored — we re-check condition
        }
    }

    fn signal_all_when_blocking(&self) {
        let _guard = self.mutex.lock().unwrap();
        self.condvar.notify_all();
    }
}
```

**Why ignore `wait_result`?** `Condvar::wait_timeout` returns a `WaitTimeoutResult` indicating whether the wait timed out or was notified. We ignore it because:
1. Spurious wakeups can return `Ok` without data being ready
2. Timeout can return `TimedOut` but data may have arrived just before we check
3. The only reliable check is re-reading the atomic sequence — which we do at the top of the loop

---

## Performance Comparison

| Strategy | Latency (p50) | Latency (p99) | CPU (idle) | Power | Signal Needed? |
|---|---|---|---|---|---|
| BusySpin | ~50ns | ~100ns | 100% | High | No |
| Yielding | ~200ns | ~1μs | ~80% | Medium | No |
| Sleeping | ~1μs | ~10μs | ~20% | Low | No |
| Blocking | ~10μs | ~100μs | ~0% | Minimal | **Yes** |
| TimeoutBlocking | ~10μs | ~100μs | ~0% | Minimal | **Yes** |
| PhasedBackoff | ~500ns | ~5μs | ~40% | Medium | No |

**"Signal Needed?"** indicates whether the producer must call `signal_all_when_blocking()` after publishing. For non-blocking strategies, the consumer polls continuously and will see new data on its own. For blocking strategies, the consumer is suspended by the OS and *must* be explicitly woken.

**Note:** These are theoretical estimates based on published hardware latencies and OS scheduling characteristics. Actual performance depends on CPU microarchitecture, OS scheduler behavior, system load, and whether CPU pinning is used. We'll measure actual performance with proper benchmarking in Post 15.

---

## Platform-Specific Considerations

### spin_loop() Instruction Mapping

```
Platform    Instruction    Effect
--------    -----------    ------
x86-64      PAUSE          Reduces power, avoids pipeline flush (~25 cycles)
ARM64       YIELD          Hints spin loop to core, may yield to SMT sibling
RISC-V      PAUSE          Similar to x86 PAUSE (extension Zihintpause)
```

### yield_now() System Call Mapping

```
Platform    System Call       Behavior
--------    -----------       --------
Linux       sched_yield()     Moves thread to end of run queue.
                              May return immediately if no other
                              runnable threads (SCHED_OTHER).
macOS       sched_yield()     Same as Linux.
Windows     SwitchToThread()  Yields to another thread on same core.
                              Returns FALSE if no other thread is ready.
```

### sleep() Minimum Resolution

```
Platform    Minimum Actual Sleep    Why
--------    --------------------    ---
Linux       ~50-100μs               Timer resolution + scheduler overhead
macOS       ~1ms                    Coarse timer resolution (can improve
                                    with thread_policy_set)
Windows     ~1-15ms                 Depends on timeBeginPeriod() setting
```

**Key insight:** `sleep(Duration::from_nanos(1))` does NOT sleep for 1ns on any platform. The actual sleep time is dominated by OS timer resolution and scheduler overhead. This is why `SleepingWaitStrategy` has ~1μs *average* latency but ~10-100μs *worst-case* latency.

---

## How to Choose: Decision Flowchart

```
START: Which wait strategy should I use?
│
├─> Q1: Do I have dedicated CPU cores for consumers?
│   │
│   ├─> YES: Is sub-100ns latency critical?
│   │   │
│   │   ├─> YES → BusySpinWaitStrategy
│   │   │   └─> HFT, ultra-low-latency trading
│   │   │
│   │   └─> NO → YieldingWaitStrategy
│   │       └─> Real-time audio, game engines
│   │
│   └─> NO: Continue to Q2
│
├─> Q2: Is latency more important than CPU cost?
│   │
│   ├─> YES: How much latency can I tolerate?
│   │   │
│   │   ├─> < 1μs → PhasedBackoffWaitStrategy::low_latency()
│   │   │
│   │   └─> < 10μs → SleepingWaitStrategy
│   │       └─> Telemetry, log aggregation
│   │
│   └─> NO: Continue to Q3
│
├─> Q3: Do I need timeout/health-check support?
│   │
│   ├─> YES → TimeoutBlockingWaitStrategy
│   │   └─> Watchdog monitoring, graceful degradation
│   │
│   └─> NO → BlockingWaitStrategy
│       └─> Background processing, batch jobs
```

### Quick Reference

| Workload | Strategy | Why |
|---|---|---|
| **HFT / Ultra-low-latency** | BusySpin | Every nanosecond counts; dedicated cores available |
| **Real-time audio / Gaming** | Yielding | Low latency without starving other threads |
| **Telemetry / Log aggregation** | Sleeping | Throughput matters, not tail latency |
| **Background processing** | Blocking | CPU-friendly; latency doesn't matter |
| **Watchdog / Health checks** | TimeoutBlocking | Need to detect stalled producers |
| **Custom / Tunable** | PhasedBackoff | Fine-grained control over trade-offs |

---

## Integration with the Disruptor

Here's how wait strategies plug into the consumer loop from Part 3C:

```rust
// Before (hardcoded busy-spin from Part 3C):
while cursor.load(Ordering::Acquire) < next_sequence {
    std::hint::spin_loop();
}

// After (pluggable wait strategy):
let barrier = SequenceBarrier::new(
    cursor,
    Box::new(BusySpinWaitStrategy),  // ← Swap strategy here!
    vec![],  // dependent sequences (Part 5)
);

// Consumer loop
loop {
    match barrier.wait_for(next_sequence) {
        Ok(available) => {
            // Process all sequences from next_sequence..=available
            for seq in next_sequence..=available {
                let event = ring_buffer.get(seq);
                handler.on_event(event, seq, seq == available);
            }
            consumer_seq.store(available, Ordering::Release);
            next_sequence = available + 1;
        }
        Err(BarrierError::Alerted) => break,  // Shutdown
        Err(BarrierError::Timeout) => {
            handler.on_timeout(next_sequence);  // Health check
        }
    }
}
```

**Key insight:** The consumer loop doesn't know or care which strategy is being used. Swapping `BusySpinWaitStrategy` for `BlockingWaitStrategy` changes latency from ~50ns to ~10μs without touching any other code.

**Producer integration (for blocking strategies):**

```rust
// After publishing, wake any blocked consumers
{
    let claim = sequencer.claim(1);
    ring_buffer.get_mut(claim.start()).value = 42;
    // claim drops here → cursor.store(end, Release)
}
// Signal blocked consumers AFTER the claim is dropped (published)
wait_strategy.signal_all_when_blocking();
```

**⚠️ Important:** `signal_all_when_blocking()` must be called *after* the `SequenceClaim` is dropped (which publishes the sequence via Release store). If you signal before publishing, consumers wake up but see no new data — they go back to sleep, adding unnecessary latency.

---

## Testing

Wait strategies interact with shared atomic state across threads, so testing them is essential. We'll write two tests: a **Loom test** for `BlockingWaitStrategy` to exhaustively verify the race condition fix under all interleavings, and a **unit test** for `TimeoutBlockingWaitStrategy` to verify timeout behavior.

### Loom Test: BlockingWaitStrategy Race Condition

[Loom](https://github.com/tokio-rs/loom) is a concurrency testing tool that systematically explores all possible thread interleavings. This is the gold standard for verifying that the "check-under-lock" pattern in `BlockingWaitStrategy` is free of lost wakeups.

```rust
#[cfg(test)]
mod tests {
    use loom::sync::atomic::{AtomicI64, Ordering};
    use loom::sync::{Arc, Condvar, Mutex};

    /// Simplified BlockingWaitStrategy using loom primitives.
    ///
    /// We re-implement the core logic with loom::sync types so that loom
    /// can intercept every atomic operation and mutex acquisition, exploring
    /// all possible interleavings.
    struct LoomBlockingWait {
        mutex: Mutex<()>,
        condvar: Condvar,
    }

    impl LoomBlockingWait {
        fn new() -> Self {
            Self {
                mutex: Mutex::new(()),
                condvar: Condvar::new(),
            }
        }

        fn wait_for(&self, sequence: i64, cursor: &AtomicI64) -> i64 {
            // Fast path: check without lock
            let available = cursor.load(Ordering::Acquire);
            if available >= sequence {
                return available;
            }

            // Slow path: check-under-lock
            let mut guard = self.mutex.lock().unwrap();
            loop {
                let available = cursor.load(Ordering::Acquire);
                if available >= sequence {
                    return available;
                }
                guard = self.condvar.wait(guard).unwrap();
            }
        }

        fn signal(&self) {
            let _guard = self.mutex.lock().unwrap();
            self.condvar.notify_all();
        }
    }

    /// Verifies that BlockingWaitStrategy never loses a wakeup signal.
    ///
    /// Loom explores all interleavings of:
    ///   1. Consumer checks cursor → sees -1 (no data)
    ///   2. Producer publishes (cursor = 0) and signals
    ///   3. Consumer enters condvar.wait()
    ///
    /// Without the mutex in signal(), interleaving (1, 2, 3) loses the
    /// signal and the consumer hangs forever. With the mutex, loom proves
    /// that no interleaving causes a hang.
    #[test]
    fn blocking_wait_no_lost_wakeup() {
        loom::model(|| {
            let cursor = Arc::new(AtomicI64::new(-1));
            let strategy = Arc::new(LoomBlockingWait::new());

            let cursor_clone = cursor.clone();
            let strategy_clone = strategy.clone();

            let producer = loom::thread::spawn(move || {
                // Publish sequence 0
                cursor_clone.store(0, Ordering::Release);
                strategy_clone.signal();
            });

            // Consumer waits for sequence 0
            let result = strategy.wait_for(0, &cursor);
            assert!(result >= 0, "Consumer must see sequence 0");

            producer.join().unwrap();
        });
    }
}
```

**What loom proves:** Under all possible interleavings of the producer's `store` + `signal` and the consumer's `load` + `wait`, the consumer always wakes up and sees the published sequence. If we removed the `mutex.lock()` from `signal()`, loom would detect a deadlock (consumer stuck in `wait()` forever) and fail the test.

### Unit Test: TimeoutBlockingWaitStrategy

```rust
#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn timeout_blocking_returns_timeout_error() {
        let cursor = Arc::new(AtomicI64::new(-1));
        let strategy = TimeoutBlockingWaitStrategy::new(Duration::from_millis(50));

        let barrier = SequenceBarrier::new(
            cursor.clone(),
            Box::new(strategy),
            vec![],
        );

        let start = Instant::now();
        let result = barrier.wait_for(0);
        let elapsed = start.elapsed();

        // Must return Timeout, not hang forever
        assert_eq!(result, Err(BarrierError::Timeout));

        // Elapsed time should be ~50ms (± scheduler jitter)
        assert!(
            elapsed >= Duration::from_millis(40),
            "Elapsed {:?} is too short — timeout may not have fired",
            elapsed
        );
        assert!(
            elapsed < Duration::from_millis(200),
            "Elapsed {:?} is too long — possible deadlock or OS scheduling issue",
            elapsed
        );
    }

    #[test]
    fn timeout_blocking_returns_immediately_when_data_available() {
        let cursor = Arc::new(AtomicI64::new(5));
        let strategy = TimeoutBlockingWaitStrategy::new(Duration::from_secs(10));

        let barrier = SequenceBarrier::new(
            cursor.clone(),
            Box::new(strategy),
            vec![],
        );

        let start = Instant::now();
        let result = barrier.wait_for(3);
        let elapsed = start.elapsed();

        // Fast path: data already available, should return immediately
        assert_eq!(result, Ok(5));
        assert!(
            elapsed < Duration::from_millis(10),
            "Elapsed {:?} — should have hit the fast path",
            elapsed
        );
    }
}
```

**What these tests verify:**

1. **`timeout_blocking_returns_timeout_error`** — When the cursor stays at -1 (no data), `wait_for(0)` must return `Err(BarrierError::Timeout)` after ~50ms, not hang forever. The elapsed-time assertions catch both premature returns (timeout not firing) and excessive delays (deadlock).

2. **`timeout_blocking_returns_immediately_when_data_available`** — When the cursor is already past the requested sequence, `wait_for` must hit the fast path and return `Ok(5)` without ever touching the mutex or condvar. This verifies that the fast-path optimization works correctly.

---

## Key Takeaways

1. **Wait strategies are the latency/CPU knob** — The only component you change to trade latency for CPU usage. Everything else stays the same.

2. **BusySpin for HFT, Blocking for background** — Match the strategy to your workload. Don't burn cores on telemetry pipelines; don't block on trading systems.

3. **BlockingWaitStrategy has a subtle race** — The "check-under-lock" pattern with mutex serialization in `signal_all_when_blocking()` is essential. Without it, signals can be lost between the condition check and `condvar.wait()`, causing permanent deadlock.

4. **`sleep(1ns)` ≠ 1ns** — OS timer resolution dominates. Linux: ~50-100μs minimum. macOS: ~1ms. Windows: ~1-15ms. Factor this into your latency budget.

5. **`spin_loop()` is not just a no-op** — It compiles to PAUSE (x86) or YIELD (ARM), which reduces power consumption and improves performance on hyper-threaded/SMT cores.

6. **Signal after publish, not before** — For blocking strategies, call `signal_all_when_blocking()` after the `SequenceClaim` is dropped to ensure consumers see the published data.

---

## Next Up: Sequence Barriers

In **Part 5**, we'll build the full `SequenceBarrier`:

- **Dependency tracking** — How consumers in a pipeline depend on upstream consumers
- **Multi-producer support** — How the barrier checks `available_buffer` for out-of-order publishing
- **Multi-stage pipelines** — Chaining consumers with dependency graphs

---

## References

### Rust Documentation

- [std::hint::spin_loop](https://doc.rust-lang.org/std/hint/fn.spin_loop.html)
- [std::thread::yield_now](https://doc.rust-lang.org/std/thread/fn.yield_now.html)
- [std::sync::Condvar](https://doc.rust-lang.org/std/sync/struct.Condvar.html)

### Platform Documentation

- [Intel PAUSE instruction](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html) — Intel SDM Vol 2, PAUSE entry
- [ARM YIELD instruction](https://developer.arm.com/documentation/) — ARM ARM, YIELD entry
- [Linux sched_yield(2)](https://man7.org/linux/man-pages/man2/sched_yield.2.html)
- [Linux futex(2)](https://man7.org/linux/man-pages/man2/futex.2.html) — Underlying mechanism for Condvar on Linux

### Java Disruptor Reference

- [BusySpinWaitStrategy.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/BusySpinWaitStrategy.java)
- [YieldingWaitStrategy.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/YieldingWaitStrategy.java)
- [SleepingWaitStrategy.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/SleepingWaitStrategy.java)
- [BlockingWaitStrategy.java](https://github.com/LMAX-Exchange/disruptor/blob/master/src/main/java/com/lmax/disruptor/BlockingWaitStrategy.java)

---

**Next:** [Part 5 — Sequence Barriers: Coordinating Dependencies →](post5.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*
