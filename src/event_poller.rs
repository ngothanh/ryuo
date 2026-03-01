//! # Event Poller — Pull-based consumption for control.
//!
//! An alternative to push-based `EventHandler` that gives consumers explicit
//! control over when and how many events to process. Useful for:
//!
//! - Integration with external event loops
//! - Rate-limited consumption
//! - Bridging with async runtimes
//!
//! ## References
//! - Blog: Post 10 (event poller)
//! - LMAX: `com.lmax.disruptor.EventPoller`

