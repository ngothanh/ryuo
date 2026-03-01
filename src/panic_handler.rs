//! # Panic Handling — Recovery, rollback, and isolation.
//!
//! Defines how the disruptor responds when an event handler panics:
//! - **Isolation**: A panicking handler doesn't crash other handlers.
//! - **Recovery**: The handler's sequence is advanced past the failing event.
//! - **Rollback**: For batch rewind scenarios, the batch can be retried.
//!
//! ## References
//! - Blog: Post 12 (panic handling)
//! - LMAX: `com.lmax.disruptor.ExceptionHandler`

