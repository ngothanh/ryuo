//! # Ryuo (竜王) — A production-grade Disruptor pattern implementation in Rust.
//!
//! Ryuo achieves sub-microsecond latency by eliminating locks, allocations,
//! and cache contention from inter-thread messaging.
//!
//! ## Architecture
//!
//! ```text
//! Producer → Sequencer → RingBuffer → SequenceBarrier → EventHandler
//!                            ↑                              |
//!                            └──── gating sequences ────────┘
//! ```
//!
//! ## Module Map (by blog post)
//!
//! | Module | Blog Post | Description |
//! |--------|-----------|-------------|
//! | [`ring_buffer`] | Post 2A, 2B | Lock-free, cache-aligned ring buffer |
//! | [`sequencer`] | Post 3 | Single-producer sequencer with RAII claims |
//! | [`wait_strategy`] | Post 4 | Busy-spin, yielding, blocking strategies |
//! | [`sequence_barrier`] | Post 5 | Consumer dependency coordination |
//! | [`event_handler`] | Post 6 | Zero-cost event dispatch and batching |
//! | [`publisher`] | Post 7 | Closure-based publishing (zero-copy) |
//! | [`batch_rewind`] | Post 8 | Retry failed batches without data loss |
//! | [`multi_producer`] | Post 9 | CAS-based multi-producer sequencer |
//! | [`event_poller`] | Post 10 | Pull-based consumption |
//! | [`dynamic_topology`] | Post 11 | Runtime handler addition/removal |
//! | [`panic_handler`] | Post 12 | Recovery, rollback, isolation |
//! | [`builder`] | Post 13 | Type-state builder DSL |
//! | [`monitoring`] | Post 14 | Backpressure, metrics, graceful shutdown |
//! | [`benchmark`] | Post 15 | HdrHistogram, coordinated omission |
//! | [`async_bridge`] | Post 16 | Async/await integration |
//! | [`affinity`] | — | Thread/core pinning |

// ── Core data structure (Post 2A, 2B) ──────────────────────────────────
pub mod ring_buffer;

// ── Coordination (Post 3, 5) ────────────────────────────────────────────
pub mod sequencer;
pub mod sequence_barrier;

// ── Consumer strategies (Post 4, 6) ─────────────────────────────────────
pub mod wait_strategy;
pub mod event_handler;

// ── Publishing (Post 7) ─────────────────────────────────────────────────
pub mod publisher;

// ── Resilience (Post 8, 12) ─────────────────────────────────────────────
pub mod batch_rewind;
pub mod panic_handler;

// ── Multi-producer (Post 9) ─────────────────────────────────────────────
pub mod multi_producer;

// ── Advanced consumption (Post 10) ──────────────────────────────────────
pub mod event_poller;

// ── Runtime flexibility (Post 11) ───────────────────────────────────────
pub mod dynamic_topology;

// ── Configuration (Post 13) ─────────────────────────────────────────────
pub mod builder;

// ── Production (Post 14, 15) ────────────────────────────────────────────
pub mod monitoring;
pub mod benchmark;

// ── Async integration (Post 16) ─────────────────────────────────────────
pub mod async_bridge;

// ── Infrastructure ──────────────────────────────────────────────────────
pub mod affinity;
