//! A label and its value on one row, with no bar.
//!
//! For quantities that have no meaningful ceiling — a process count, cumulative bytes read — where a
//! proportional bar would invent a denominator that does not exist.

use super::text_width;
use crate::ui::theme;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::Line,
    widgets::Widget,
};

pub struct Reading<'a> {
    label: &'a str,
    value: &'a str,
    value_style: Style,
    label_width: u16,
}

impl<'a> Reading<'a> {
    pub fn new(label: &'a str, value: &'a str) -> Self {
        Self {
            label,
            value,
            value_style: theme::value(),
            label_width: text_width(label),
        }
    }

    /// Override the value's ink, for a value that is a state rather than a measurement.
    pub fn value_style(mut self, style: Style) -> Self {
        self.value_style = style;
        self
    }

    pub fn label_width(mut self, width: u16) -> Self {
        self.label_width = width;
        self
    }
}

impl Widget for Reading<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let value_width = text_width(self.value);
        if area.width >= self.label_width + 1 + value_width {
            let [label, value] = Layout::horizontal([
                Constraint::Length(self.label_width + 1),
                Constraint::Min(value_width),
            ])
            .areas(area);
            Line::styled(self.label, theme::label()).render(label, buf);
            Line::styled(self.value, self.value_style)
                .right_aligned()
                .render(value, buf);
        } else {
            Line::styled(self.value, self.value_style)
                .right_aligned()
                .render(area, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(width: u16, reading: Reading<'_>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).expect("test terminal");
        terminal
            .draw(|frame| frame.render_widget(reading, frame.area()))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..width)
            .map(|column| buffer[(column, 0)].symbol().to_string())
            .collect()
    }

    #[test]
    fn a_reading_puts_the_label_left_and_the_value_right() {
        let line = render(20, Reading::new("Processes", "3"));
        assert!(line.starts_with("Processes"), "{line:?}");
        assert!(line.trim_end().ends_with('3'), "{line:?}");
    }

    #[test]
    fn a_narrow_reading_keeps_the_value() {
        let line = render(4, Reading::new("Processes", "3"));
        assert!(line.contains('3'), "{line:?}");
    }

    #[test]
    fn an_empty_area_renders_nothing_without_panicking() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        Reading::new("Processes", "3").render(Rect::new(0, 0, 0, 0), &mut buffer);
    }
}
