//! One place for the colours every screen draws with.
//!
//! The screens this replaced chose a colour at each of about twenty print sites, so CPU was cyan in two
//! places by coincidence rather than by rule, and a whole line — its label as much as its number — was
//! tinted. Both are fixed here by giving colour a job:
//!
//! - **Data text wears text ink.** Labels, values and hints use [`label`], [`value`] and [`hint`]. A number
//!   is never tinted to mean something; the mark beside it carries that. Chrome is the exception, and only
//!   because chrome cannot be misread: a heading or a key hint is structure, so [`accent`] tints it without
//!   any risk of reading as a measurement or a verdict.
//! - **Status colours are reserved.** [`good`] through [`critical`] mean state and nothing else, so a
//!   series can never be drawn in a colour that reads as "critical". [`accent`] is reserved the same way,
//!   in the other direction: no series or status may use it, so it always means "structure".
//! - **Series colours are a fixed order, never cycled.** [`series`] runs out rather than wrapping around
//!   to repeat a hue that is already on screen meaning something else.
//! - **No backgrounds, and no black or white.** The terminal's own palette is the user's, and a hardcoded
//!   background is the one thing guaranteed to look broken in half of them. Selection uses
//!   [`Modifier::REVERSED`], which swaps whatever their foreground and background actually are, so light
//!   and dark themes both work without this module knowing which is in use.
//!
//! That last point is also why the palette here is not machine-validated the way a web chart's would be:
//! `Color::Cyan` has no fixed hex value to check a contrast ratio against — it resolves to whatever the
//! terminal emulator's theme says. The guarantee is structural instead: meaning is never carried by colour
//! alone, so a palette that renders badly still reads correctly.

use ratatui::style::{Color, Modifier, Style};

/// Utilisation at which a meter stops reading as healthy.
const WARNING_AT: f64 = 0.75;

/// Utilisation at which a meter reads as a problem rather than a load.
const SERIOUS_AT: f64 = 0.90;

/// Chrome: headings, key hints, the focus marker.
///
/// One hue for everything that frames the data rather than being it, which is what stops the accent from
/// becoming decoration — a reader who learns it once can tell structure from reading anywhere on any screen.
pub fn accent() -> Color {
    Color::Cyan
}

/// Text ink, stated explicitly.
///
/// [`value`] leaves the foreground alone, which is right for body text and wrong inside a heading: a span
/// with no colour of its own inherits the line's, so a word in a panel title comes out [`accent`] whether it
/// meant to or not. This is how a span says "not the accent" without naming a colour of its own.
pub fn ink() -> Color {
    Color::Reset
}

/// A screen or panel heading.
///
/// `DIM` is removed rather than merely absent: a block's title is drawn over its border area, so a bordered
/// panel's recessive [`border`] style reaches the title and dims it unless this says otherwise. That was
/// costing every panel heading in the tool its weight.
pub fn heading() -> Style {
    Style::default()
        .fg(accent())
        .add_modifier(Modifier::BOLD)
        .remove_modifier(Modifier::DIM)
}

/// The name of a field. Secondary ink: present, not competing with its value.
pub fn label() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// A measurement. Deliberately untinted — see the module docs.
pub fn value() -> Style {
    Style::default()
}

/// Key hints and other text that should recede until looked for.
pub fn hint() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// Panel borders and rules. Recessive by design: the data is the content, the frame is not.
pub fn border() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// The focused row or control.
///
/// `REVERSED` rather than a chosen background, so this follows the terminal's own colours.
pub fn selected() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// Text that is unavailable rather than merely low — a missing process, an unsupported platform.
///
/// Drops `BOLD` as well as adding dimness, because the two contradict each other and a style that only adds
/// cannot say so: used inside a heading, this would otherwise come out bold and dim at once, which terminals
/// resolve however they like.
pub fn absent() -> Style {
    Style::default()
        .add_modifier(Modifier::DIM | Modifier::ITALIC)
        .remove_modifier(Modifier::BOLD)
}

/// Status: nothing to report.
pub fn good() -> Color {
    Color::Green
}

/// Status: worth knowing about.
pub fn warning() -> Color {
    Color::Yellow
}

/// Status: worth acting on.
pub fn serious() -> Color {
    Color::LightRed
}

/// Status: the thing this tool exists to find.
pub fn critical() -> Color {
    Color::Red
}

/// The status colour for a utilisation ratio, so no call site decides where "busy" begins.
///
/// Takes a ratio rather than a percentage because both callers have one: a gauge is a fraction of its
/// width either way, and a percentage argument invites the 0.9-versus-90 mistake.
pub fn pressure(ratio: f64) -> Color {
    if ratio >= SERIOUS_AT {
        critical()
    } else if ratio >= WARNING_AT {
        serious()
    } else if ratio >= WARNING_AT / 2.0 {
        warning()
    } else {
        good()
    }
}

/// Identity colours for distinct data series, in fixed order.
///
/// Returns `None` past the end rather than wrapping. A repeated hue on one screen says two series are
/// the same thing, and none of these overlap the status colours above or [`accent`], so a series can never
/// accidentally read as a verdict or as part of the frame.
pub fn series(index: usize) -> Option<Color> {
    const ORDER: [Color; 4] = [
        Color::Magenta,
        Color::Blue,
        Color::LightMagenta,
        Color::LightBlue,
    ];
    ORDER.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_escalates_with_load() {
        assert_eq!(pressure(0.10), good());
        assert_eq!(pressure(0.50), warning());
        assert_eq!(pressure(0.80), serious());
        assert_eq!(pressure(0.95), critical());
    }

    /// A ratio arriving above 1.0 or below 0.0 must still yield a colour rather than panic: CPU
    /// percentages from `sysinfo` can exceed 100 across multiple cores.
    #[test]
    fn pressure_handles_ratios_outside_the_unit_range() {
        assert_eq!(pressure(4.0), critical());
        assert_eq!(pressure(-1.0), good());
    }

    #[test]
    fn series_colours_run_out_rather_than_repeating() {
        let mut seen = Vec::new();
        let mut index = 0;
        while let Some(colour) = series(index) {
            assert!(!seen.contains(&colour), "series {index} repeats a hue");
            seen.push(colour);
            index += 1;
        }
        assert!(index > 1, "there should be more than one series colour");
    }

    /// The reservation that keeps the frame from reading as data, and data from reading as the frame.
    #[test]
    fn the_accent_is_neither_a_status_nor_a_series_colour() {
        let statuses = [good(), warning(), serious(), critical()];
        assert!(
            !statuses.contains(&accent()),
            "the accent uses a reserved status colour"
        );
        let mut index = 0;
        while let Some(colour) = series(index) {
            assert_ne!(colour, accent(), "series {index} uses the chrome accent");
            index += 1;
        }
    }

    /// The reservation that keeps a series from reading as a verdict.
    #[test]
    fn no_series_colour_is_also_a_status_colour() {
        let statuses = [good(), warning(), serious(), critical()];
        let mut index = 0;
        while let Some(colour) = series(index) {
            assert!(
                !statuses.contains(&colour),
                "series {index} uses a reserved status colour"
            );
            index += 1;
        }
    }
}
