//! # Wait Strategies — Trading latency for CPU usage.
//!
//! Consumers need a strategy for what to do when no events are available.
//! Each strategy makes a different trade-off:
//!
//! - **`BusySpinWaitStrategy`**: Lowest latency (~1ns), highest CPU usage (100% core).
//!   Best for dedicated cores in HFT systems.
//! - **`YieldingWaitStrategy`**: Low latency (~10-50ns), moderate CPU.
//!   Yields to OS scheduler between spins.
//! - **`BlockingWaitStrategy`**: Higher latency (~1-10μs), lowest CPU.
//!   Uses `Condvar` for OS-level blocking. Best for background consumers.
//!
//! ## References
//! - Blog: Post 4 (wait strategies)
//! - Mastery Plan: `disruptor::wait_strategy`
//! - LMAX: `com.lmax.disruptor.WaitStrategy`

