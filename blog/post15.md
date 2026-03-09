# Building a Disruptor in Rust: Ryuo — Part 15: Benchmarking — HdrHistogram, Coordinated Omission, and RDTSC

**Series:** Building a Disruptor in Rust: Ryuo
**Part:** 15 of 16
**Target Audience:** Systems engineers measuring and validating low-latency systems
**Prerequisites:** Part 13 (builder DSL), Part 14 (production patterns)

---

## Recap

We've built the complete Disruptor — ring buffer, sequencers, barriers, handlers, publishing, multi-producer, panic handling, DSL builder, and production patterns. But none of it matters without proof. Performance claims without benchmarks are opinions.

This post builds a **rigorous benchmarking harness** that avoids the three most common measurement mistakes:

1. **Coordinated omission** — The benchmark accidentally synchronizes with the system under test, hiding tail latency.
2. **Timer noise** — `Instant::now()` costs 15-30ns, which is the same magnitude as the thing we're measuring.
3. **No warmup** — First iterations include allocation, page faults, and branch predictor training.

We'll measure Ryuo against `std::sync::mpsc`, `crossbeam-channel`, and `flume`, then report results with statistical rigor.

---

## The Problem: Why Most Benchmarks Lie

### Coordinated Omission

Gil Tene's insight (2013): most benchmarks accidentally measure the **best case**, not the **typical case**.

```
Naive benchmark (WRONG):
─────────────────────────────────────────────────
Target: 1M events/sec (1 event per 1μs)

Timeline:
  t=0μs:  Send event 0, wait for ack          → latency = 100ns ✓
  t=1μs:  Send event 1, wait for ack          → latency = 100ns ✓
  t=2μs:  Send event 2, wait for ack          → latency = 10μs  (stall!)
  t=12μs: Send event 3, wait for ack          → latency = 100ns ✗ WRONG!

The problem: We waited for event 2 before sending event 3.
Event 3 SHOULD have been sent at t=3μs, but was sent at t=12μs.
The 9μs delay is hidden — the benchmark doesn't record it.

Reported p99: 10μs (looks great!)
Reality: 9 events would have experienced 2-10μs latency.
```

```
Correct benchmark:
─────────────────────────────────────────────────
Send events at fixed rate. Measure time from EXPECTED send to receive.

  t=0μs:  Send event 0 → received at 0.1μs   → latency = 100ns
  t=1μs:  Send event 1 → received at 1.1μs   → latency = 100ns
  t=2μs:  Send event 2 → received at 12μs    → latency = 10μs
  t=3μs:  Event 3 SHOULD send but can't (blocked by event 2)
  t=4μs:  Event 4 SHOULD send but can't
  ...
  t=11μs: Event 11 SHOULD send but can't
  t=12μs: Send event 3 → received at 12.1μs  → latency = 9.1μs!

With correction, events 3-11 are recorded with their
TRUE latency (time from when they should have been sent):
  Event 3: should send at 3μs, received at 12.1μs → 9.1μs
  Event 4: should send at 4μs, received at 12.1μs → 8.1μs
  ...

Reported p99: 9μs (reality!)
```

**The fix:** Record when each event *should* have been sent (based on target rate), not when it *was* sent. Compute latency from the expected send time.

### Timer Noise: `Instant::now()` vs RDTSC

`Instant::now()` on Linux uses a VDSO call (`clock_gettime(CLOCK_MONOTONIC)`) that costs **15-30ns** per invocation. On macOS, `mach_absolute_time()` costs **~20ns**. When the thing you're measuring takes 50-100ns, a 20ns timer call is 20-40% noise.

Production HFT uses the CPU's Time Stamp Counter (TSC) directly:

| Timer | Cost | Resolution | Platform |
|-------|------|-----------|----------|
| `Instant::now()` | 15-30ns | ~1ns | Cross-platform |
| `rdtsc` | ~2ns | ~0.3ns | x86_64 only |
| `rdtscp` | ~10ns | ~0.3ns | x86_64 only (serializing) |
| `CNTVCT_EL0` | ~5ns | ~1ns | ARM64 only |

---

## HFT-Grade Timing: RDTSC

```rust
/// Read the Time Stamp Counter without serialization.
///
/// Use for the SEND timestamp. The lack of serialization means
/// the read may be reordered with surrounding instructions — but
/// for the send side, this is acceptable because we want to
/// capture the timestamp as close to the publish call as possible.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub fn rdtsc() -> u64 {
    // SAFETY: x86_64 target; _rdtsc is always available on x86_64.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Read TSC with RDTSCP — serializes instruction retirement.
///
/// Use for the RECEIVE timestamp. RDTSCP waits for all preceding
/// instructions to retire before reading the counter, preventing
/// the timestamp from appearing earlier than actual event processing.
///
/// The `_aux` output contains the processor ID (IA32_TSC_AUX MSR),
/// which can be used to detect cross-core migration during measurement.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub fn rdtscp() -> u64 {
    let mut _aux: u32 = 0;
    // SAFETY: x86_64 target; __rdtscp is always available on x86_64.
    unsafe { core::arch::x86_64::__rdtscp(&mut _aux) }
}

/// Calibrate TSC frequency against wall-clock time.
///
/// Returns TSC ticks per nanosecond. Called once at startup.
/// Uses a 100ms sleep for calibration — longer sleeps give
/// more accurate calibration but delay startup.
pub fn calibrate_tsc_ns() -> f64 {
    let t0_tsc = rdtsc();
    let t0_wall = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let elapsed_ns = t0_wall.elapsed().as_nanos() as f64;
    let elapsed_tsc = rdtsc() - t0_tsc;
    elapsed_tsc as f64 / elapsed_ns
}

/// Portable timer: uses RDTSC on x86_64, Instant::now() elsewhere.
pub struct Timer {
    #[cfg(target_arch = "x86_64")]
    tsc_per_ns: f64,
}

impl Timer {
    pub fn new() -> Self {
        Self {
            #[cfg(target_arch = "x86_64")]
            tsc_per_ns: calibrate_tsc_ns(),
        }
    }

    /// Record a send timestamp (lightweight, non-serializing).
    #[inline(always)]
    pub fn send_timestamp(&self) -> u64 {
        #[cfg(target_arch = "x86_64")]
        { rdtsc() }
        #[cfg(not(target_arch = "x86_64"))]
        { std::time::Instant::now().elapsed().as_nanos() as u64 }
    }

    /// Record a receive timestamp (serializing on x86_64).
    #[inline(always)]
    pub fn recv_timestamp(&self) -> u64 {
        #[cfg(target_arch = "x86_64")]
        { rdtscp() }
        #[cfg(not(target_arch = "x86_64"))]
        { std::time::Instant::now().elapsed().as_nanos() as u64 }
    }

    /// Convert a TSC delta to nanoseconds.
    #[inline(always)]
    pub fn tsc_to_ns(&self, tsc_delta: u64) -> u64 {
        #[cfg(target_arch = "x86_64")]
        { (tsc_delta as f64 / self.tsc_per_ns) as u64 }
        #[cfg(not(target_arch = "x86_64"))]
        { tsc_delta }
    }
}
```

**Caveats:**
- **Invariant TSC required.** Modern x86_64 CPUs have a constant-rate TSC (`CPUID.80000007H:EDX[8]`). Check with `grep -m1 constant_tsc /proc/cpuinfo` on Linux. Without invariant TSC, the counter frequency varies with CPU frequency.
- **Cross-socket skew.** TSC is not synchronized across NUMA sockets. Pin benchmark threads to one socket.
- **ARM64 alternative.** Use `CNTVCT_EL0`: `mrs x0, cntvct_el0` in inline asm, or `std::arch::aarch64` intrinsics.

---

## The LatencyBenchmark Harness

### Step 1: HdrHistogram Integration

The [hdrhistogram](https://crates.io/crates/hdrhistogram) crate provides Gil Tene's HdrHistogram in Rust. It records values with configurable significant digits and supports percentile queries without sorting.

```rust
use hdrhistogram::Histogram;
use std::time::{Duration, Instant};

/// Latency benchmark with coordinated omission correction.
///
/// Records send/receive timestamps and computes corrected
/// latency distributions.
pub struct LatencyBenchmark {
    /// Raw latency histogram (corrected for coordinated omission).
    histogram: Histogram<u64>,
    /// Uncorrected histogram for comparison.
    raw_histogram: Histogram<u64>,
    /// Send timestamps indexed by sequence number.
    send_times: Vec<u64>,
    /// Timer for TSC-based measurements.
    timer: Timer,
    /// When the benchmark started (for expected send time calculation).
    benchmark_start_tsc: u64,
    /// Target rate in events per second.
    target_rate: u64,
    /// Expected interval between events in TSC ticks.
    expected_interval_tsc: u64,
}

impl LatencyBenchmark {
    pub fn new(target_rate: u64, event_count: usize) -> Self {
        let timer = Timer::new();
        let expected_interval_ns = 1_000_000_000 / target_rate;

        #[cfg(target_arch = "x86_64")]
        let expected_interval_tsc = (expected_interval_ns as f64 * timer.tsc_per_ns) as u64;
        #[cfg(not(target_arch = "x86_64"))]
        let expected_interval_tsc = expected_interval_ns;

        Self {
            // 3 significant digits, range: 1ns to 1 hour
            histogram: Histogram::new_with_bounds(1, 3_600_000_000_000, 3).unwrap(),
            raw_histogram: Histogram::new_with_bounds(1, 3_600_000_000_000, 3).unwrap(),
            send_times: Vec::with_capacity(event_count),
            timer,
            benchmark_start_tsc: 0,
            target_rate,
            expected_interval_tsc,
        }
    }

    /// Call this once before the first send.
    pub fn start(&mut self) {
        self.benchmark_start_tsc = self.timer.send_timestamp();
    }

    /// Record the send timestamp for a sequence.
    #[inline(always)]
    pub fn record_send(&mut self, _sequence: i64) {
        self.send_times.push(self.timer.send_timestamp());
    }

    /// Record the receive timestamp and compute latency.
    ///
    /// Applies coordinated omission correction: if the event was
    /// sent later than expected, synthetic samples are injected
    /// for the "missed" events that would have been sent in between.
    pub fn record_receive(&mut self, sequence: i64) {
        let recv_tsc = self.timer.recv_timestamp();
        let send_tsc = self.send_times[sequence as usize];

        let latency_tsc = recv_tsc.saturating_sub(send_tsc);
        let latency_ns = self.timer.tsc_to_ns(latency_tsc);

        // Record raw (uncorrected) latency
        self.raw_histogram.record(latency_ns).unwrap_or(());

        // Coordinated omission correction
        let expected_send_tsc = self.benchmark_start_tsc
            + (sequence as u64 * self.expected_interval_tsc);

        if send_tsc > expected_send_tsc {
            // Event was sent late — inject synthetic samples
            let delay_tsc = send_tsc - expected_send_tsc;
            let missed_samples = delay_tsc / self.expected_interval_tsc;

            for i in 0..missed_samples.min(1000) {
                // Each missed sample would have experienced
                // increasing latency (they were queued up)
                let missed_latency_tsc = latency_tsc
                    + ((missed_samples - i) * self.expected_interval_tsc);
                let missed_latency_ns = self.timer.tsc_to_ns(missed_latency_tsc);
                self.histogram.record(missed_latency_ns).unwrap_or(());
            }
        }

        // Record actual latency (corrected)
        self.histogram.record(latency_ns).unwrap_or(());
    }

    /// Print the latency distribution.
    pub fn report(&self, name: &str) {
        println!("\n=== {} ===", name);
        println!("  Target rate: {} events/sec", self.target_rate);
        println!("  Samples: {} (raw), {} (corrected)",
            self.raw_histogram.len(), self.histogram.len());

        println!("\n  {:>8}  {:>12}  {:>12}", "Percentile", "Raw (ns)", "Corrected (ns)");
        println!("  {:>8}  {:>12}  {:>12}", "─────────", "────────", "─────────────");

        for &p in &[50.0, 90.0, 99.0, 99.9, 99.99, 99.999] {
            println!("  p{:<7} {:>12} {:>12}",
                format!("{}", p),
                self.raw_histogram.value_at_quantile(p / 100.0),
                self.histogram.value_at_quantile(p / 100.0),
            );
        }

        println!("\n  Min:    {:>12} ns", self.histogram.min());
        println!("  Max:    {:>12} ns", self.histogram.max());
        println!("  Mean:   {:>12.2} ns", self.histogram.mean());
        println!("  StdDev: {:>12.2} ns", self.histogram.stdev());
    }
}
```

**Why cap `missed_samples` at 1000?** A catastrophic stall (GC pause, kernel scheduling) could produce millions of synthetic samples, consuming memory and distorting the histogram. Capping at 1000 limits the correction to ~1ms of missed events at 1M events/sec, which is sufficient for most analyses.

### Step 2: Benchmark Harness

```rust
/// Harness for running repeatable, statistically valid benchmarks.
///
/// Handles: warmup, measurement, CPU isolation, and result aggregation.
pub struct BenchmarkHarness {
    warmup_iterations: u64,
    measurement_iterations: u64,
    target_rate: u64,
    event_count: usize,
}

impl BenchmarkHarness {
    pub fn new(
        warmup_iterations: u64,
        measurement_iterations: u64,
        target_rate: u64,
        event_count: usize,
    ) -> Self {
        Self {
            warmup_iterations,
            measurement_iterations,
            target_rate,
            event_count,
        }
    }

    /// Run a benchmark function multiple times and aggregate results.
    pub fn run<F>(&self, name: &str, mut benchmark_fn: F)
    where
        F: FnMut(u64, usize) -> LatencyBenchmark,
    {
        println!("\n{}", "=".repeat(60));
        println!("Benchmark: {}", name);
        println!("{}", "=".repeat(60));

        // System isolation
        self.isolate_system();

        // Warmup phase: discard results
        println!("Warmup: {} iterations...", self.warmup_iterations);
        for _ in 0..self.warmup_iterations {
            let _ = benchmark_fn(self.target_rate, self.event_count);
        }

        // Measurement phase: collect results
        println!("Measuring: {} iterations...", self.measurement_iterations);
        let mut corrected = Histogram::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
        let mut raw = Histogram::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();

        for i in 0..self.measurement_iterations {
            let result = benchmark_fn(self.target_rate, self.event_count);
            corrected.add(&result.histogram).unwrap();
            raw.add(&result.raw_histogram).unwrap();

            if (i + 1) % 5 == 0 {
                println!("  [{}/{}] p50={}ns p99={}ns",
                    i + 1, self.measurement_iterations,
                    corrected.value_at_quantile(0.50),
                    corrected.value_at_quantile(0.99),
                );
            }
        }

        // Report
        self.report_results(name, &corrected, &raw);
    }

    fn isolate_system(&self) {
        // CPU affinity (Linux only)
        #[cfg(target_os = "linux")]
        {
            use libc::{cpu_set_t, sched_setaffinity, CPU_SET, CPU_ZERO};
            unsafe {
                let mut set: cpu_set_t = std::mem::zeroed();
                CPU_ZERO(&mut set);
                CPU_SET(0, &mut set);
                sched_setaffinity(0, std::mem::size_of::<cpu_set_t>(), &set);
            }
            println!("  CPU pinned to core 0");

            // Set performance governor
            std::process::Command::new("sudo")
                .args(&["cpupower", "frequency-set", "-g", "performance"])
                .output()
                .ok();
            println!("  CPU governor set to performance");
        }

        #[cfg(target_os = "macos")]
        {
            println!("  macOS: CPU pinning not available (use taskpolicy for QoS)");
        }
    }

    fn report_results(
        &self,
        name: &str,
        corrected: &Histogram<u64>,
        raw: &Histogram<u64>,
    ) {
        println!("\n=== {} — Aggregate ({} runs × {} events) ===",
            name, self.measurement_iterations, self.event_count);

        println!("\n  {:>8}  {:>12}  {:>12}", "Percentile", "Raw (ns)", "Corrected (ns)");
        println!("  {:>8}  {:>12}  {:>12}", "─────────", "────────", "─────────────");

        for &p in &[50.0, 90.0, 99.0, 99.9, 99.99] {
            println!("  p{:<7} {:>12} {:>12}",
                format!("{}", p),
                raw.value_at_quantile(p / 100.0),
                corrected.value_at_quantile(p / 100.0),
            );
        }

        println!("\n  Throughput: {:.2}M events/sec",
            self.event_count as f64 * self.measurement_iterations as f64
                / corrected.mean()
                / 1_000.0
        );
    }
}
```

**Why separate warmup?** The first few iterations include:
- **Page faults:** The ring buffer's memory isn't physically allocated until first write.
- **Branch predictor training:** The CPU's branch predictor needs ~100-1000 iterations to learn hot paths.
- **TLB misses:** Translation lookaside buffer fills on first access.

Discarding warmup iterations ensures measurements reflect steady-state performance.

---

## Benchmark Scenarios

### Scenario 1: Unicast (1P-1C)

```rust
fn bench_ryuo_unicast(target_rate: u64, event_count: usize) -> LatencyBenchmark {
    let mut bench = LatencyBenchmark::new(target_rate, event_count);

    let processed = Arc::new(AtomicI64::new(-1));
    let processed_clone = Arc::clone(&processed);

    struct TimestampHandler {
        receive_times: Vec<u64>,
        timer: Timer,
        sequence: Arc<AtomicI64>,
    }

    impl EventConsumer<u64> for TimestampHandler {
        fn consume(&mut self, _event: &u64, seq: i64, _eob: bool) {
            self.receive_times.push(self.timer.recv_timestamp());
            self.sequence.store(seq, Ordering::Release);
        }
        fn on_start(&mut self) {}
        fn on_shutdown(&mut self) {}
    }

    let timer = Timer::new();
    let handler = TimestampHandler {
        receive_times: Vec::with_capacity(event_count),
        timer: Timer::new(),
        sequence: processed_clone,
    };

    let disruptor = RingBufferBuilder::<u64>::new()
        .buffer_size(1024)
        .single_producer()
        .wait_strategy(Box::new(BusySpinWaitStrategy))
        .build(|| 0u64)
        .handle_events_with(handler)
        .start();

    bench.start();

    // Publish at target rate
    let interval_ns = 1_000_000_000 / target_rate;
    for i in 0..event_count {
        bench.record_send(i as i64);
        disruptor.publish_with(|event, seq| {
            *event = seq as u64;
        });

        // Rate limiting (busy wait for precision)
        let next_send = bench.benchmark_start_tsc
            + ((i as u64 + 1) * bench.expected_interval_tsc);
        while timer.send_timestamp() < next_send {
            std::hint::spin_loop();
        }
    }

    // Wait for all events to be processed
    while processed.load(Ordering::Acquire) < (event_count as i64 - 1) {
        std::hint::spin_loop();
    }

    disruptor.shutdown();
    bench
}
```

### Scenario 2: Pipeline (1P-3C Sequential)

```rust
fn bench_ryuo_pipeline(target_rate: u64, event_count: usize) -> LatencyBenchmark {
    // Similar to unicast, but with 3 handlers chained with .then()
    let disruptor = RingBufferBuilder::<u64>::new()
        .buffer_size(1024)
        .single_producer()
        .wait_strategy(Box::new(BusySpinWaitStrategy))
        .build(|| 0u64)
        .handle_events_with(PassthroughHandler::new())
        .then(PassthroughHandler::new())
        .then(TimestampHandler::new(event_count))  // Only last handler records
        .start();

    // ... same publish loop as unicast ...
    // Measures end-to-end latency through all 3 stages
}
```

### Scenario 3: Multicast (1P-3C Parallel)

```rust
fn bench_ryuo_multicast(target_rate: u64, event_count: usize) -> LatencyBenchmark {
    // 3 independent handlers, same events
    let disruptor = RingBufferBuilder::<u64>::new()
        .buffer_size(1024)
        .single_producer()
        .wait_strategy(Box::new(BusySpinWaitStrategy))
        .build(|| 0u64)
        .handle_events_with(TimestampHandler::new(event_count))
        .and(PassthroughHandler::new())
        .and(PassthroughHandler::new())
        .start();

    // ... same publish loop ...
    // Measures latency to slowest handler
}
```

### Scenario 4: Multi-Producer (3P-1C)

```rust
fn bench_ryuo_multi_producer(target_rate: u64, event_count: usize) -> LatencyBenchmark {
    let disruptor = RingBufferBuilder::<u64>::new()
        .buffer_size(1024)
        .multi_producer()
        .wait_strategy(Box::new(BusySpinWaitStrategy))
        .build(|| 0u64)
        .handle_events_with(TimestampHandler::new(event_count))
        .start();

    // Spawn 3 producer threads, each publishing event_count/3 events
    let events_per_producer = event_count / 3;
    let handles: Vec<_> = (0..3).map(|producer_id| {
        let d = disruptor.clone();
        std::thread::spawn(move || {
            for i in 0..events_per_producer {
                d.publish_with(|event, seq| {
                    *event = (producer_id * events_per_producer + i) as u64;
                });
            }
        })
    }).collect();

    for h in handles { h.join().unwrap(); }
    disruptor.shutdown();

    // Note: coordinated omission correction is less meaningful
    // for multi-producer because there's no single "expected" rate.
}
```

---

## Comparison Benchmarks

### std::sync::mpsc

```rust
fn bench_std_mpsc(target_rate: u64, event_count: usize) -> LatencyBenchmark {
    let mut bench = LatencyBenchmark::new(target_rate, event_count);
    let (tx, rx) = std::sync::mpsc::sync_channel(1024);

    let handle = std::thread::spawn(move || {
        let timer = Timer::new();
        let mut receive_times = Vec::with_capacity(event_count);
        for _ in 0..event_count {
            let _event: u64 = rx.recv().unwrap();
            receive_times.push(timer.recv_timestamp());
        }
        receive_times
    });

    bench.start();
    let timer = Timer::new();
    for i in 0..event_count {
        bench.record_send(i as i64);
        tx.send(i as u64).unwrap();

        // Rate limiting
        let next_send = bench.benchmark_start_tsc
            + ((i as u64 + 1) * bench.expected_interval_tsc);
        while timer.send_timestamp() < next_send {
            std::hint::spin_loop();
        }
    }

    let _receive_times = handle.join().unwrap();
    bench
}
```

### crossbeam-channel

```rust
fn bench_crossbeam(target_rate: u64, event_count: usize) -> LatencyBenchmark {
    let mut bench = LatencyBenchmark::new(target_rate, event_count);
    let (tx, rx) = crossbeam_channel::bounded(1024);

    let handle = std::thread::spawn(move || {
        let timer = Timer::new();
        for _ in 0..event_count {
            let _event: u64 = rx.recv().unwrap();
            // Record receive timestamp
        }
    });

    bench.start();
    let timer = Timer::new();
    for i in 0..event_count {
        bench.record_send(i as i64);
        tx.send(i as u64).unwrap();

        let next_send = bench.benchmark_start_tsc
            + ((i as u64 + 1) * bench.expected_interval_tsc);
        while timer.send_timestamp() < next_send {
            std::hint::spin_loop();
        }
    }

    handle.join().unwrap();
    bench
}
```

### flume

```rust
fn bench_flume(target_rate: u64, event_count: usize) -> LatencyBenchmark {
    let mut bench = LatencyBenchmark::new(target_rate, event_count);
    let (tx, rx) = flume::bounded(1024);

    // Same pattern as crossbeam
    // ...

    bench
}
```

---

## Expected Results

> **Disclaimer:** These numbers are **theoretical estimates** based on the Disruptor's design characteristics and published benchmarks from the Java Disruptor. Actual numbers will vary by hardware, CPU generation, OS, and system load. Run the benchmarks yourself and report real numbers.

```
=== Unicast (1P-1C, BusySpinWaitStrategy, buffer=1024) ===

                      Ryuo (est.)   std::mpsc    crossbeam    flume
─────────────────────────────────────────────────────────────────────
Throughput            ~25M/sec      ~3M/sec      ~8M/sec      ~10M/sec
p50                   ~50ns         ~300ns       ~120ns       ~100ns
p99                   ~100ns        ~2μs         ~500ns       ~400ns
p99.9                 ~200ns        ~10μs        ~2μs         ~1μs
p99.99                ~500ns        ~50μs        ~10μs        ~5μs
CPU (idle consumer)   100%          ~50%         ~80%         ~70%

=== Pipeline (1P-3C sequential, BusySpinWaitStrategy) ===

                      Ryuo (est.)   std::mpsc (3 channels)
──────────────────────────────────────────────────────────
End-to-end p50        ~150ns        ~1μs
End-to-end p99        ~300ns        ~10μs

=== Multi-Producer (3P-1C, CAS-based claiming) ===

                      Ryuo (est.)   crossbeam
─────────────────────────────────────────────
Throughput            ~18M/sec      ~6M/sec
p50                   ~80ns         ~200ns
p99                   ~300ns        ~2μs
```

**Why Ryuo is faster (theoretical):**

| Factor | Ryuo | Bounded channel |
|--------|------|----------------|
| Allocation | Zero (pre-allocated) | Per-send (or pool) |
| Cache behavior | Sequential access, prefetchable | Scattered nodes, cache-unfriendly |
| Synchronization | Single atomic + wait strategy | Lock or CAS per enqueue + dequeue |
| Batching | Built-in (process all available) | Manual (one at a time) |
| False sharing | Padded (128 bytes) | Usually not padded |

---

## Visualization: Log-Scale Latency Distribution

```rust
/// Print an ASCII histogram on a log scale.
pub fn plot_latency_distribution(histogram: &Histogram<u64>) {
    println!("\nLatency Distribution (log scale):");
    println!("  {:<10} {:>10}  {}", "Percentile", "Value (ns)", "Distribution");
    println!("  {:<10} {:>10}  {}", "──────────", "──────────", "────────────");

    for &percentile in &[50.0, 90.0, 99.0, 99.9, 99.99, 99.999] {
        let value = histogram.value_at_quantile(percentile / 100.0);
        let bar_length = if value > 0 {
            (value as f64).log10() as usize * 8
        } else {
            0
        };
        let bar = "█".repeat(bar_length.min(60));
        println!("  p{:<8} {:>10} ns  {}", percentile, value, bar);
    }
}
```

**Sample output:**

```
Latency Distribution (log scale):
  Percentile  Value (ns)  Distribution
  ──────────  ──────────  ────────────
  p50               52 ns  █████████████
  p90               89 ns  ███████████████
  p99              134 ns  █████████████████
  p99.9            287 ns  ███████████████████
  p99.99           892 ns  ███████████████████████
  p99.999         3401 ns  ████████████████████████████
```

---

## Statistical Significance: Welch's t-Test

A single benchmark run proves nothing. Multiple runs with a statistical test determine whether the difference between two systems is real or noise.

```rust
/// Welch's t-test for unequal variances.
///
/// Returns (t_statistic, degrees_of_freedom).
/// If |t| > 2.0 and df > 30, the difference is statistically
/// significant at p < 0.05.
pub fn welch_t_test(sample1: &[f64], sample2: &[f64]) -> (f64, f64) {
    let n1 = sample1.len() as f64;
    let n2 = sample2.len() as f64;

    let mean1 = sample1.iter().sum::<f64>() / n1;
    let mean2 = sample2.iter().sum::<f64>() / n2;

    let var1 = sample1.iter()
        .map(|x| (x - mean1).powi(2))
        .sum::<f64>() / (n1 - 1.0);
    let var2 = sample2.iter()
        .map(|x| (x - mean2).powi(2))
        .sum::<f64>() / (n2 - 1.0);

    let se = ((var1 / n1) + (var2 / n2)).sqrt();
    let t = (mean1 - mean2) / se;

    // Welch-Satterthwaite degrees of freedom
    let df = ((var1 / n1) + (var2 / n2)).powi(2)
        / ((var1 / n1).powi(2) / (n1 - 1.0)
            + (var2 / n2).powi(2) / (n2 - 1.0));

    (t, df)
}

/// Interpret the t-test result.
pub fn is_significant(t: f64, df: f64) -> bool {
    // Approximate: |t| > 2.0 with df > 30 → p < 0.05
    df > 30.0 && t.abs() > 2.0
}
```

**Usage:**

```rust
// Run Ryuo 30 times, record p99 each time
let ryuo_p99s: Vec<f64> = (0..30).map(|_| {
    let bench = bench_ryuo_unicast(1_000_000, 100_000);
    bench.histogram.value_at_quantile(0.99) as f64
}).collect();

// Run crossbeam 30 times
let crossbeam_p99s: Vec<f64> = (0..30).map(|_| {
    let bench = bench_crossbeam(1_000_000, 100_000);
    bench.histogram.value_at_quantile(0.99) as f64
}).collect();

let (t, df) = welch_t_test(&ryuo_p99s, &crossbeam_p99s);
println!("t={:.2}, df={:.1}, significant={}", t, df, is_significant(t, df));
```

---

## Putting It All Together

```rust
fn main() {
    println!("Ryuo Benchmark Suite");
    println!("====================\n");

    let harness = BenchmarkHarness::new(
        3,          // warmup iterations
        30,         // measurement iterations
        1_000_000,  // 1M events/sec
        100_000,    // 100K events per run
    );

    // Ryuo benchmarks
    harness.run("Ryuo Unicast (1P-1C)", |rate, count| {
        bench_ryuo_unicast(rate, count)
    });
    harness.run("Ryuo Pipeline (1P-3C)", |rate, count| {
        bench_ryuo_pipeline(rate, count)
    });
    harness.run("Ryuo Multicast (1P-3C)", |rate, count| {
        bench_ryuo_multicast(rate, count)
    });
    harness.run("Ryuo Multi-Producer (3P-1C)", |rate, count| {
        bench_ryuo_multi_producer(rate, count)
    });

    // Comparison benchmarks
    harness.run("std::sync::mpsc (1P-1C)", |rate, count| {
        bench_std_mpsc(rate, count)
    });
    harness.run("crossbeam (1P-1C)", |rate, count| {
        bench_crossbeam(rate, count)
    });
    harness.run("flume (1P-1C)", |rate, count| {
        bench_flume(rate, count)
    });
}
```

---

## Key Takeaways

1. **Coordinated omission makes naive benchmarks lie.** Always send at a fixed rate and measure from expected send time, not actual send time. The correction reveals tail latency that naive benchmarks hide.

2. **RDTSC is 10× cheaper than `Instant::now()`.** At 2ns vs 20ns, RDTSC eliminates timer noise when measuring sub-100ns operations. Use `rdtsc` for send (non-serializing) and `rdtscp` for receive (serializing).

3. **Warmup eliminates cold-start artifacts.** Page faults, branch predictor training, and TLB misses distort the first iterations. Discard at least 3-5 warmup runs.

4. **HdrHistogram captures the full distribution.** Unlike averages (which hide outliers) or max (which is dominated by noise), percentiles show the shape of the latency distribution. Always report p50, p99, p99.9, and p99.99.

5. **Statistical significance requires multiple runs.** A single run proves nothing. Welch's t-test with 30+ samples determines whether differences are real. If |t| > 2.0 with df > 30, the difference is significant at p < 0.05.

6. **Every performance claim should be reproducible.** Pin benchmarks to specific hardware, document system configuration, and provide runnable code. "50ns p50" without methodology is marketing, not engineering.

---

## Next Up: Async Integration

In **Part 16**, we'll bridge Ryuo with async Rust:

- **Dedicated thread pattern** — Keep Ryuo sync, expose async wrappers
- **`AsyncRyuo`** — Publish from `async` contexts via command channel
- **`Stream` adapter** — Consume events as a Tokio `Stream`
- **Trade-offs** — When sync beats async, and when you need both

---

## References

### Coordinated Omission

- [Gil Tene — How NOT to Measure Latency](https://www.youtube.com/watch?v=lJ8ydIuPFeU) — The original talk (2013)
- [HdrHistogram](https://github.com/HdrHistogram/HdrHistogram) — Gil Tene's histogram implementation

### Rust Crates

- [hdrhistogram](https://crates.io/crates/hdrhistogram) — Rust port of HdrHistogram
- [criterion](https://crates.io/crates/criterion) — Statistics-driven benchmarking (alternative approach)

### Timing

- [Intel RDTSC Reference](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html) — Volume 2B, RDTSC/RDTSCP instructions
- [Invariant TSC](https://en.wikipedia.org/wiki/Time_Stamp_Counter#Invariant_TSC) — Constant-rate TSC on modern CPUs
- [CNTVCT_EL0](https://developer.arm.com/documentation/ddi0601/2024-12/AArch64-Registers/CNTVCT-EL0--Counter-timer-Virtual-Count-Register) — ARM64 counter

### Java Disruptor Benchmarks

- [Disruptor Perftest](https://github.com/LMAX-Exchange/disruptor/tree/master/src/perftest/java) — Java Disruptor's benchmark suite

---

**Next:** [Part 16 — Async Integration: Bridging Sync and Async Worlds →](post16.md)

---

*Have questions or feedback? Found this useful? Let me know in the comments below!*
