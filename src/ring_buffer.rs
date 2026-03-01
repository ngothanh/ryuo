//! # Ring Buffer — Lock-free, pre-allocated, cache-aligned ring buffer.
//!
//! The core data structure of the Disruptor pattern. Uses a power-of-2 sized
//! pre-allocated array with bitwise AND indexing for O(1) access.
//!
//! ## Key design decisions
//! - `UnsafeCell<MaybeUninit<T>>` for interior mutability without ownership transfer
//! - `#[repr(C, align(64))]` for cache-line alignment and false sharing prevention
//! - Bitwise AND indexing (`sequence & mask`) instead of modulo (1 cycle vs 35-90 cycles)
//! - Panic-safe `Drop`: only drops slots that were successfully initialized
//!
//! ## References
//! - Blog: Post 2A (ring buffer), Post 2B (cache-line padding)
//! - Mastery Plan: `disruptor::ring_buffer`
//! - LMAX: `com.lmax.disruptor.RingBuffer`


