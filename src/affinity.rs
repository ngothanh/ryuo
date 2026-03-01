//! # Thread/Core Affinity — Pin threads to specific CPU cores.
//!
//! For lowest latency, disruptor threads should be pinned to dedicated cores
//! to avoid context switches and cache pollution. This module provides:
//! - Core pinning via `sched_setaffinity` (Linux) / platform equivalents.
//! - NUMA-aware allocation hints.
//!
//! ## References
//! - Mastery Plan: `disruptor::affinity`
//! - Blog: Post 1 (NUMA discussion)

