//! Synthetic workloads that produce comparable metrics.
//!
//! Each module owns one class of measurement, takes explicit parameters rather than reading a preset,
//! and returns [`Metric`]s built from [`crate::metrics::catalog`] specs. Keeping them parameterised
//! and preset-agnostic is what lets the background daemon reuse the same code at micro scale.
//!
//! [`Metric`]: crate::model::Metric

pub mod cpu;
pub mod filesystem;
pub mod memory;
pub mod network;
pub mod process;
pub mod soak;
pub mod sqlite;
