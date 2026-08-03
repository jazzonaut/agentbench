//! A labelled utilisation bar: name on the left, bar in the middle, number on the right.

use super::text_width;
use crate::ui::theme;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{Gauge, Widget},
};

/// Narrowest bar worth drawing. Below this it conveys nothing the number does not.
const MIN_BAR: u16 = 8;

/// A utilisation reading.
pub struct Meter<'a> {
    label: &'a str,
    value: &'a str,
    ratio: f64,
    label_width: u16,
}

impl<'a> Meter<'a> {
    /// `ratio` is a fraction, not a percentage.
    ///
    /// Out-of-range and non-finite input is absorbed here rather than rejected. [`Gauge::ratio`] panics
    /// outside `0.0..=1.0`, and both bad values arrive from real sources: `sysinfo` reports CPU above 100%
    /// for a process spanning several cores, and a ratio computed against a total of zero is `NaN`. A
    /// panic in a drawing routine takes down a diagnostic tool mid-measurement, so the guard lives in the
    /// one place every caller passes through instead of at each of them.
    pub fn new(label: &'a str, ratio: f64, value: &'a str) -> Self {
        Self {
            label,
            value,
            ratio: if ratio.is_finite() {
                ratio.clamp(0.0, 1.0)
            } else {
                0.0
            },
            label_width: text_width(label),
        }
    }

    /// Align this meter's bar with others by reserving a fixed label column.
    pub fn label_width(mut self, width: u16) -> Self {
        self.label_width = width;
        self
    }
}

impl Widget for Meter<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let value_width = text_width(self.value);
        // Degradation is decided here rather than left to layout constraints, so that a cramped terminal
        // drops the least informative element first and predictably: the bar, then the label. The number
        // is the last thing standing because it is the measurement.
        let needs_bar = self.label_width + 1 + MIN_BAR + 1 + value_width;
        let needs_label = self.label_width + 1 + value_width;
        if area.width >= needs_bar {
            let [label, bar, value] = Layout::horizontal([
                Constraint::Length(self.label_width + 1),
                Constraint::Min(MIN_BAR),
                Constraint::Length(value_width + 1),
            ])
            .areas(area);
            Line::styled(self.label, theme::label()).render(label, buf);
            Gauge::default()
                .ratio(self.ratio)
                .label("")
                .gauge_style(theme::pressure(self.ratio))
                .render(bar, buf);
            Line::styled(self.value, theme::value())
                .right_aligned()
                .render(value, buf);
        } else if area.width >= needs_label {
            let [label, value] = Layout::horizontal([
                Constraint::Length(self.label_width + 1),
                Constraint::Min(value_width),
            ])
            .areas(area);
            Line::styled(self.label, theme::label()).render(label, buf);
            Line::styled(self.value, theme::value())
                .right_aligned()
                .render(value, buf);
        } else {
            Line::styled(self.value, theme::value())
                .right_aligned()
                .render(area, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    /// Render one meter into a buffer of the given size and return it as lines of text.
    fn render(width: u16, height: u16, meter: Meter<'_>) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| frame.render_widget(meter, frame.area()))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|row| {
                (0..width)
                    .map(|column| buffer[(column, row)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_wide_meter_shows_label_bar_and_value() {
        let lines = render(40, 1, Meter::new("System CPU", 0.5, "50.0 %"));
        assert!(lines[0].starts_with("System CPU"), "{:?}", lines[0]);
        assert!(lines[0].trim_end().ends_with("50.0 %"), "{:?}", lines[0]);
    }

    /// The narrow case: the number survives when nothing else fits.
    #[test]
    fn a_narrow_meter_keeps_the_number() {
        let lines = render(8, 1, Meter::new("System CPU", 0.5, "50.0 %"));
        assert!(lines[0].contains("50.0 %"), "{:?}", lines[0]);
    }

    /// Zero-width and zero-height areas reach widgets during a resize; neither may panic.
    #[test]
    fn an_empty_area_renders_nothing_without_panicking() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        Meter::new("System CPU", 0.5, "50.0 %").render(Rect::new(0, 0, 0, 0), &mut buffer);
    }

    /// The two inputs that would panic inside `Gauge::ratio` if they were passed straight through.
    #[test]
    fn out_of_range_and_non_finite_ratios_are_absorbed() {
        render(40, 1, Meter::new("Tree CPU", 4.0, "400.0 %"));
        render(40, 1, Meter::new("Memory", f64::NAN, "— GiB"));
        render(40, 1, Meter::new("Memory", -1.0, "0.0 GiB"));
    }
}
