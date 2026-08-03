//! Collectors: produce records, never persist them.
//!
//! Each collector receives a [`Sink`] and a [`Clock`] and knows nothing about HTTP or about the other
//! collectors.
//!
//! They are genuinely dissimilar, which is why they share no trait. [`sampler`] is cheap and continuous;
//! [`probes`] is expensive and periodic and is the only one that loads the machine on purpose;
//! [`sessions`] measures work someone else already did. What they share is scheduling, and that belongs
//! to the supervisor.
//!
//! The transcript importer is the one collector that also reads: it has to recover how far it got last
//! time. It does so through a read-only [`Reader`], so it still cannot write anything except by
//! sending a record like everything else here.
//!
//! [`Sink`]: crate::watch::store::Sink
//! [`Clock`]: crate::watch::clock::Clock
//! [`Reader`]: crate::watch::store::Reader

pub mod probes;
pub mod sampler;
pub mod sessions;
pub mod targets;
