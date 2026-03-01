//! # Sequence Barriers — Coordinating consumer dependencies.
//!
//! A sequence barrier tracks which sequences are safe for a consumer to read.
//! It combines:
//! - The cursor sequence (what the producer has published)
//! - Dependent sequences (what upstream consumers have processed)
//!
//! This enables **diamond** and **pipeline** topologies where consumers
//! can depend on other consumers, not just the producer.
//!
//! ## References
//! - Blog: Post 5 (sequence barriers)
//! - Mastery Plan: `disruptor::sequence_barrier`
//! - LMAX: `com.lmax.disruptor.SequenceBarrier`, `ProcessingSequenceBarrier`

