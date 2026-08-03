//! Collectors: produce records, never persist them.
//!
//! Each collector receives a [`Sink`] and a [`Clock`] and knows nothing about SQL, HTTP, or the other
//! collectors. Probes and the transcript importer join this layer in later phases.
//!
//! [`Sink`]: crate::watch::store::Sink
//! [`Clock`]: crate::watch::clock::Clock

pub mod sampler;
pub mod targets;
