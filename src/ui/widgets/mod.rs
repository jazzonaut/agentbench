//! Pieces shared by every screen.
//!
//! Each of these exists because two or more screens draw the same thing, and the versions that preceded
//! them had drifted: the live process view and the benchmark view both reported CPU, in different words,
//! at different precision, in different colours. A widget is how that stops recurring.

pub mod footer;
pub mod history;
pub mod meter;
pub mod reading;

pub use footer::Footer;
pub use history::{History, Series};
pub use meter::Meter;
pub use reading::Reading;

/// Displayed width of a label or value in cells.
///
/// Counts characters. Everything drawn through these widgets is ASCII — field names this crate chooses
/// and numbers it formats — so a full grapheme-width calculation would add a dependency to be correct
/// about input that never arrives. The one place non-ASCII text does reach a screen is a process name,
/// and that is rendered as ordinary text rather than measured for column alignment.
pub(crate) fn text_width(text: &str) -> u16 {
    u16::try_from(text.chars().count()).unwrap_or(u16::MAX)
}
