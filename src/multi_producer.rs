//! # Multi-Producer — CAS, contention, and coordination costs.
//!
//! The multi-producer sequencer allows multiple threads to publish concurrently
//! using compare-and-swap (CAS) operations. Key concerns:
//!
//! - **CAS contention**: Under high contention, CAS retries degrade throughput.
//! - **Publication tracking**: An availability buffer tracks which slots have been
//!   fully published (needed because producers may publish out of order).
//! - **ABA problem**: Handled via monotonically increasing sequences.
//!
//! ## References
//! - Blog: Post 9 (multi-producer)
//! - Mastery Plan: `disruptor::event_processor` (multi-producer path)
//! - LMAX: `com.lmax.disruptor.MultiProducerSequencer`

