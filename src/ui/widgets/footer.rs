//! The key-hint line every screen ends with.

use crate::ui::theme;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Widget,
};

/// A key and what it does.
pub type Hint<'a> = (&'a str, &'a str);

/// `[q] quit  [space] toggle`, in recessive ink.
pub struct Footer<'a> {
    hints: &'a [Hint<'a>],
    /// Extra text shown ahead of the hints, for a transient state such as "cancelling".
    status: Option<&'a str>,
}

impl<'a> Footer<'a> {
    pub fn new(hints: &'a [Hint<'a>]) -> Self {
        Self {
            hints,
            status: None,
        }
    }

    /// Replace the hints with a message while something is in progress.
    ///
    /// Replaces rather than prepends: the moment there is something to say, the keys are no longer the
    /// most useful thing on the line, and a footer that shows both is a footer that overflows.
    pub fn status(mut self, status: &'a str) -> Self {
        self.status = Some(status);
        self
    }
}

impl Widget for Footer<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        if let Some(status) = self.status {
            Line::styled(status, theme::hint()).render(area, buf);
            return;
        }
        let mut spans = Vec::with_capacity(self.hints.len() * 3);
        for (index, (key, action)) in self.hints.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled("  ", theme::hint()));
            }
            // The key is the affordance and the action is the explanation, so the key wears the accent: it
            // is what the eye needs to find on a line it is otherwise meant to ignore.
            spans.push(Span::styled(
                format!("[{key}]"),
                Style::default().fg(theme::accent()),
            ));
            spans.push(Span::styled(format!(" {action}"), theme::hint()));
        }
        // Truncation is ratatui's: a `Line` longer than its area is cut at the edge rather than wrapped,
        // which is the right failure for a footer. Dropping whole hints instead would hide the one key a
        // cramped user most needs, and which one that is depends on the screen, not on this widget.
        Line::from(spans).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(width: u16, footer: Footer<'_>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).expect("test terminal");
        terminal
            .draw(|frame| frame.render_widget(footer, frame.area()))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..width)
            .map(|column| buffer[(column, 0)].symbol().to_string())
            .collect()
    }

    #[test]
    fn hints_render_as_bracketed_keys() {
        let line = render(40, Footer::new(&[("q", "quit"), ("space", "toggle")]));
        assert_eq!(line.trim_end(), "[q] quit  [space] toggle");
    }

    #[test]
    fn a_status_replaces_the_hints() {
        let line = render(
            40,
            Footer::new(&[("q", "quit")]).status("Cancelling; cleaning up…"),
        );
        assert_eq!(line.trim_end(), "Cancelling; cleaning up…");
    }

    #[test]
    fn a_narrow_footer_truncates_rather_than_panicking() {
        let line = render(6, Footer::new(&[("q", "quit"), ("space", "toggle")]));
        assert_eq!(line.chars().count(), 6);
    }

    #[test]
    fn an_empty_footer_is_harmless() {
        let line = render(10, Footer::new(&[]));
        assert_eq!(line.trim_end(), "");
    }
}
