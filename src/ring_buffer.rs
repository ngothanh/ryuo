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

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::AtomicUsize;

#[repr(C, align(64))]
struct RingBuffer<T> {
    _padding_left: [u8; 64],
    buffer: Box<[UnsafeCell<MaybeUninit<T>>]>,
    index_mask: usize,
    initialized: AtomicUsize,
    _padding_right: [u8; 64],
}

impl<T> RingBuffer<T> {

    pub fn new<F>(size: usize, factory: F) -> Self
    where
        F: Fn() -> T,
    {
        assert!(size > 0, "Size must be greater than 0");
        assert!(size.is_power_of_two(), "Size must be power of 2");

        let mut buffer = Vec::with_capacity(size);
        for _ in 0..size {
            buffer.push(UnsafeCell::new(MaybeUninit::new(factory())));
        }
        let initialized = buffer.len();

        RingBuffer {
            _padding_left: [0; 64],
            buffer: buffer.into_boxed_slice(),
            index_mask: size - 1,
            initialized: AtomicUsize::new(initialized),
            _padding_right: [0; 64],
        }
    }

    #[inline]
    pub fn get(&self, sequence: i64) -> &T {
        let index = (sequence as usize) & self.index_mask;

        // SAFETY:
        // 1. Index is masked to buffer size (can't overflow)
        // 2. Sequence barrier prevents wrap-around
        // 3. Only one writer per slot
        // 4. Slot is initialized (guaranteed by constructor)
        unsafe { (*self.buffer[index].get()).assume_init_ref() }
    }

    #[inline]
    pub fn get_mut(&self, sequence: i64) -> &mut T {
        let index = (sequence as usize) & self.index_mask;

        // SAFETY: Same as get(), plus:
        // 5. Caller has exclusive claim (via SequenceClaim)
        unsafe { (*self.buffer[index].get()).assume_init_mut() }
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.buffer.len()
    }
}
