//! Reading meaning out of what was collected.
//!
//! A layer between [`store`] and [`serve`], and one that writes nothing: everything here is a function of
//! rows already in the database, so a verdict can be recomputed at any time and no analysis result is ever
//! a fact that has to be migrated, invalidated or trusted from an older build. Nothing is materialised —
//! a seven-day band over a few hundred rows is cheap enough to compute per request, and a stored one would
//! be a second copy of the truth with its own staleness.
//!
//! The layer is deliberately thin on judgement and thick on disclosure. Every figure it returns carries the
//! count behind it, and where a covariate could explain a verdict on its own it says so in words rather
//! than quietly filtering the data until the verdict changes.
//!
//! [`store`]: crate::watch::store
//! [`serve`]: crate::watch::serve

pub mod baseline;
pub mod comparison;
pub mod day;
pub mod verdict;

pub use baseline::{Baseline, DayValue};
pub use comparison::{Comparison, Comparisons, evidence::PowerMix, today_against_baseline};
pub use day::Day;
pub use verdict::Verdict;
