//! # Event Handlers — Zero-cost dispatch, batching, and lifecycle.
//!
//! Event handlers process events from the ring buffer. Key features:
//! - **Batching**: Process multiple events per wake-up for amortized overhead.
//! - **Zero-cost dispatch**: No virtual dispatch in the hot path (monomorphization).
//! - **Lifecycle hooks**: `on_start`, `on_shutdown`, `on_batch_start`.
//!
//! ## References
//! - Blog: Post 6 (event handlers)
//! - Mastery Plan: `disruptor::event_processor`
//! - LMAX: `com.lmax.disruptor.EventHandler`, `BatchEventProcessor`

