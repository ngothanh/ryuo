//! # Batch Rewind — Retry without data loss.
//!
//! When an event handler encounters a transient error mid-batch, batch rewind
//! allows rewinding the consumer's sequence so the entire batch (or partial
//! batch) can be retried without losing events.
//!
//! ## References
//! - Blog: Post 8 (batch rewind)
//! - LMAX: `com.lmax.disruptor.RewindableEventHandler`

