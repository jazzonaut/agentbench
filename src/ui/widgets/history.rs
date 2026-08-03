//! A recent-values sparkline with its current reading.
//!
//! The screens this replaced showed instantaneous numbers only, which meant a spike you had just caused
//! was gone before you could read it. A few seconds of history is the difference between a screen worth
//! watching and one worth glancing at.

use super::text_width;
use crate::ui::theme;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Sparkline, Widget},
};
use std::collections::VecDeque;

/// Narrowest sparkline worth drawing.
const MIN_PLOT: u16 = 8;

/// A bounded window of the most recent values, oldest first.
#[derive(Debug, Clone)]
pub struct Series {
    values: VecDeque<u64>,
    capacity: usize,
}

impl Series {
    /// A window holding at most `capacity` values.
    ///
    /// A capacity of zero would make [`push`] a silent no-op and every plot permanently empty, so it is
    /// raised to one instead of being rejected: a screen with a mis-sized series should look wrong, not
    /// refuse to open.
    ///
    /// [`push`]: Series::push
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            values: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Record a value, discarding the oldest once full.
    pub fn push(&mut self, value: u64) {
        while self.values.len() >= self.capacity {
            self.values.pop_front();
        }
        self.values.push_back(value);
    }

    /// The values, oldest first.
    pub fn values(&self) -> impl Iterator<Item = u64> + '_ {
        self.values.iter().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// A labelled sparkline with the latest value alongside it.
pub struct History<'a> {
    label: &'a str,
    value: &'a str,
    series: &'a Series,
    colour: Color,
    label_width: u16,
    /// Upper bound for scaling, when the natural maximum of the window would mislead.
    max: Option<u64>,
}

impl<'a> History<'a> {
    pub fn new(label: &'a str, series: &'a Series, value: &'a str) -> Self {
        Self {
            label,
            value,
            series,
            colour: theme::series(0).unwrap_or(Color::Cyan),
            label_width: text_width(label),
            max: None,
        }
    }

    pub fn colour(mut self, colour: Color) -> Self {
        self.colour = colour;
        self
    }

    pub fn label_width(mut self, width: u16) -> Self {
        self.label_width = width;
        self
    }

    /// Scale against a fixed ceiling rather than the window's own peak.
    ///
    /// Worth setting for anything with a real maximum, such as a CPU percentage. Left unset, a window of
    /// values between 3% and 4% is drawn full-height and reads as saturation.
    pub fn max(mut self, max: u64) -> Self {
        self.max = Some(max);
        self
    }
}

impl Widget for History<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let value_width = text_width(self.value);
        // Same degradation order as `Meter`: the plot goes first, the label second, the number last.
        if area.width >= self.label_width + 1 + MIN_PLOT + 1 + value_width {
            let [label, plot, value] = Layout::horizontal([
                Constraint::Length(self.label_width + 1),
                Constraint::Min(MIN_PLOT),
                Constraint::Length(value_width + 1),
            ])
            .areas(area);
            Line::styled(self.label, theme::label()).render(label, buf);
            let mut sparkline = Sparkline::default()
                .data(self.series.values().collect::<Vec<_>>())
                .style(Style::default().fg(self.colour));
            if let Some(max) = self.max {
                sparkline = sparkline.max(max);
            }
            sparkline.render(plot, buf);
            Line::styled(self.value, theme::value())
                .right_aligned()
                .render(value, buf);
        } else if area.width >= self.label_width + 1 + value_width {
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

    fn render(width: u16, height: u16, history: History<'_>) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| frame.render_widget(history, frame.area()))
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
    fn a_series_keeps_the_most_recent_values_in_order() {
        let mut series = Series::new(3);
        for value in 1..=5 {
            series.push(value);
        }
        assert_eq!(series.values().collect::<Vec<_>>(), vec![3, 4, 5]);
    }

    #[test]
    fn a_zero_capacity_series_still_records_something() {
        let mut series = Series::new(0);
        series.push(7);
        assert_eq!(series.values().collect::<Vec<_>>(), vec![7]);
    }

    #[test]
    fn a_wide_history_shows_label_and_value() {
        let mut series = Series::new(32);
        for value in [10, 20, 30, 40] {
            series.push(value);
        }
        let lines = render(
            40,
            1,
            History::new("System CPU", &series, "40.0 %").max(100),
        );
        assert!(lines[0].starts_with("System CPU"), "{:?}", lines[0]);
        assert!(lines[0].trim_end().ends_with("40.0 %"), "{:?}", lines[0]);
    }

    /// An empty window is the first frame of every screen, before any sample has been taken.
    #[test]
    fn an_empty_series_renders_without_panicking() {
        let series = Series::new(32);
        assert!(series.is_empty());
        render(40, 1, History::new("System CPU", &series, "— %"));
    }

    #[test]
    fn an_empty_area_renders_nothing_without_panicking() {
        let series = Series::new(4);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        History::new("System CPU", &series, "0 %").render(Rect::new(0, 0, 0, 0), &mut buffer);
    }
}
