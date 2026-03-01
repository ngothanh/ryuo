//! # Publishing Patterns — Closures over translators.
//!
//! Provides ergonomic APIs for publishing events into the ring buffer.
//! Uses closure-based "translators" instead of Java's `EventTranslator` interface:
//!
//! ```ignore
//! disruptor.publish(|event, sequence| {
//!     event.price = 42.0;
//!     event.quantity = 100;
//! });
//! ```
//!
//! This is zero-copy: the closure writes directly into the pre-allocated slot.
//!
//! ## References
//! - Blog: Post 7 (publishing patterns)
//! - LMAX: `com.lmax.disruptor.EventTranslator`

