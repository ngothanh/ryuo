//! # Builder DSL — Type-safe, ergonomic configuration.
//!
//! A type-state builder that validates disruptor configuration at compile time:
//!
//! ```ignore
//! let disruptor = Disruptor::builder()
//!     .buffer_size(1024)
//!     .single_producer()
//!     .wait_strategy(BusySpinWaitStrategy)
//!     .handle_events(MyHandler)
//!     .build();
//! ```
//!
//! Missing required fields (buffer size, producer type) are compile errors,
//! not runtime panics.
//!
//! ## References
//! - Blog: Post 13 (builder DSL)
//! - LMAX: `com.lmax.disruptor.dsl.Disruptor`

