//! Collectors: produce records, never persist them.
//!
//! Each collector receives a [`Sink`] and a [`Clock`] and knows nothing about HTTP or about the other
//! collectors. Probes join this layer in a later phase.
//!
//! The transcript importer is the one collector that also reads: it has to recover how far it got last
//! time. It does so through a read-only [`Reader`], so it still cannot write anything except by
//! sending a record like everything else here.
//!
//! [`Sink`]: crate::watch::store::Sink
//! [`Clock`]: crate::watch::clock::Clock
//! [`Reader`]: crate::watch::store::Reader

pub mod sampler;
pub mod sessions;
pub mod targets;
