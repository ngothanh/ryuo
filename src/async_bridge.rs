//! # Async/Await Integration — Bridging sync and async worlds.
//!
//! Provides adapters for using Ryuo from async contexts:
//! - **Dedicated thread**: Ryuo runs on its own thread, async wrapper for publishing.
//! - **Stream adapter**: Consume events as a `futures::Stream`.
//! - **Hybrid**: Sync fast path for publishing, async slow path for backpressure.
//!
//! Trade-off: Adds ~100-500ns per publish due to channel overhead.
//! Not suitable for <100ns latency requirements.
//!
//! ## References
//! - Blog: Post 16 (async integration)

