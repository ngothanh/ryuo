//! # Monitoring — Production patterns for observability.
//!
//! Provides metrics and instrumentation for production deployments:
//! - **Backpressure detection**: Monitor remaining capacity.
//! - **Throughput counters**: Events published/consumed per second.
//! - **Latency histograms**: End-to-end event processing latency.
//! - **Graceful shutdown**: Drain in-flight events before stopping.
//!
//! ## References
//! - Blog: Post 14 (production patterns)
//! - Mastery Plan: Production infrastructure (Prometheus + Grafana)

