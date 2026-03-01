//! # Dynamic Topologies — Runtime handler addition (with safety caveats).
//!
//! Allows adding and removing event handlers at runtime without stopping
//! the disruptor. This is inherently unsafe in a lock-free system and
//! requires careful sequence coordination to avoid data races.
//!
//! ## References
//! - Blog: Post 11 (dynamic topologies)
//! - LMAX: Dynamic consumer addition via `handleEventsWith` at runtime

