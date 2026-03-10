//! # Sequencers — Coordinate producer access to the ring buffer.
//!
//! Sequencers claim slots in the ring buffer and publish sequences to make
//! events visible to consumers. Two variants:
//!
//! - **`SingleProducerSequencer`**: Simple counter, no atomics in the fast path.
//!   Optimal when only one thread publishes.
//! - **`MultiProducerSequencer`**: Uses `AtomicI64` with CAS for concurrent producers.
//!   Covered in depth in Post 9.
//!
//! Both use RAII `SequenceClaim` guards that auto-publish on drop, preventing
//! forgotten publishes.
//!
//! ## References
//! - Blog: Post 3 (single-producer sequencer + RAII), Post 9 (multi-producer CAS)
//! - Mastery Plan: `disruptor::sequence_barrier` (gating sequences)
//! - LMAX: `com.lmax.disruptor.SingleProducerSequencer`, `MultiProducerSequencer`

pub trait Sequencer: Send {
    /// Claim `count` slots. Blocks until available.
    fn claim(&self, count: usize) -> SequenceClaim;

    /// Try to claim `count` slots. Returns immediately.
    fn try_claim(&self, count: usize) -> Result<SequenceClaim, InsufficientCapacity>;

    /// Get current cursor position
    fn cursor(&self) -> i64;
}

#[derive(Debug, Clone, Copy)]
pub struct InsufficientCapacity;

pub struct SequenceClaim {}
