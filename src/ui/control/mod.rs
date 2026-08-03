//! The screen `agentbench` opens with no arguments.
//!
//! It answers the questions the command line makes awkward: is collection working, how do I change
//! something without remembering twelve flags, and how do I start, compare or reset without looking up a
//! subcommand. The status band comes from [`status_report`], the same code `dashboard --status` prints, so
//! the screen and the command can never disagree about a verdict.
//!
//! Every action row is a shortcut for something that can also be typed, and deliberately so: a screen that
//! was the only way to reach a capability would put it out of reach of a script. The two exceptions are
//! about installing the tool rather than using it - copying the executable somewhere durable, and putting
//! that directory on `PATH` - which have nowhere sensible to live on a command line that has not been
//! installed yet.
//!
//! [`status_report`]: crate::status_report

mod apply;
mod model;

use crate::{
    status_report,
    ui::{
        Screen, theme,
        widgets::{Footer, Reading},
    },
    watch::{self, WatchConfig},
};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use model::{Field, Kind, Section, State};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};
use std::time::{Duration, Instant};

/// How often the status band is re-read.
///
/// Slow on purpose: it opens the database and probes the instance lock, and neither is worth doing four
/// times a second to update a figure that changes once every sampling interval.
const STATUS_REFRESH: Duration = Duration::from_secs(2);

/// Redraw cadence.
const TICK: Duration = Duration::from_millis(200);

/// Column reserved for row labels.
const LABEL_WIDTH: u16 = 30;

/// How long a row that needs confirming stays armed.
///
/// Short enough that an Enter pressed a minute later belongs to whatever the user is doing now rather
/// than to a question they have forgotten answering.
const CONFIRM_WINDOW: Duration = Duration::from_secs(5);

const HINTS: &[(&str, &str)] = &[
    ("↑↓", "move"),
    ("space", "toggle"),
    ("enter", "edit or run"),
    ("q", "quit"),
];

const EDIT_HINTS: &[(&str, &str)] = &[("enter", "confirm"), ("esc", "cancel")];

/// Open the control centre.
pub fn run(data_dir: Option<std::path::PathBuf>) -> Result<()> {
    // Loaded before the screen opens, so a configuration error is an ordinary message on stderr rather than
    // something flashed inside an alternate buffer that is about to be torn down.
    let config = WatchConfig::load(data_dir)?;
    let mut app = App::new(config)?;
    let mut screen = Screen::enter()?;
    loop {
        app.refresh_status_if_due();
        screen.terminal().draw(|frame| app.draw(frame))?;
        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && app.handle(key.code, key.modifiers) == Flow::Quit
        {
            return Ok(());
        }
    }
}

/// Whether the loop continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

struct App {
    state: State,
    fields: Vec<Field>,
    selected: usize,
    /// Present while a value is being typed.
    editing: Option<String>,
    /// The last thing that happened, and whether it went wrong.
    message: Option<Result<String, String>>,
    status: Option<Result<status_report::Report, String>>,
    status_read_at: Option<Instant>,
    /// A row awaiting a second Enter, and when it was armed.
    armed: Option<(Field, Instant)>,
}

impl App {
    fn new(config: WatchConfig) -> Result<Self> {
        Ok(Self {
            state: State::read(config),
            fields: State::fields(),
            selected: 0,
            editing: None,
            message: None,
            status: None,
            status_read_at: None,
            armed: None,
        })
    }

    fn field(&self) -> Field {
        self.fields[self.selected.min(self.fields.len() - 1)]
    }

    /// Re-read the status band when it is stale, ignoring the outcome's shape.
    fn refresh_status_if_due(&mut self) {
        let due = self
            .status_read_at
            .is_none_or(|read_at| read_at.elapsed() >= STATUS_REFRESH);
        if !due {
            return;
        }
        self.status_read_at = Some(Instant::now());
        // A missing database is the ordinary first-run case, not a failure worth a red message: the daemon
        // creates it when it starts. Reported in the band as the reason there are no numbers.
        self.status = Some(
            watch::open_for_reading(&self.state.config)
                .and_then(|reader| status_report::Report::build(&self.state.config, &reader))
                .map_err(|error| format!("{error:#}")),
        );
        // The rows that depend on whether anything is collecting read the same answer the band above
        // them shows, rather than probing the lock a second time and possibly disagreeing with it. When
        // the band could not be built at all - a first run with no database, most often - the lock is
        // probed directly, because "start collecting" has to be right even then.
        let running = match &self.status {
            Some(Ok(report)) => report.daemon_running,
            _ => watch::is_running(&self.state.config),
        };
        self.state.refresh_volatile(running);
    }

    fn handle(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Flow {
        if self.editing.is_some() {
            return self.handle_edit(code);
        }
        // Anything other than the confirming keystroke disarms. Moving off the row, or pressing a key
        // that does something else, is not a decision to go ahead - and a row that stayed armed while
        // the selection moved would act on whatever the user landed on next.
        if code != KeyCode::Enter {
            self.armed = None;
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Flow::Quit,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return Flow::Quit,
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1) % self.fields.len();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self
                    .selected
                    .checked_sub(1)
                    .unwrap_or(self.fields.len() - 1);
            }
            KeyCode::Char(' ') => self.activate(false),
            KeyCode::Enter => self.activate(true),
            KeyCode::Char('r') => {
                self.state = State::read(self.state.config.clone());
                self.status_read_at = None;
                self.message = Some(Ok("reloaded".into()));
            }
            _ => {}
        }
        Flow::Continue
    }

    /// Space toggles; Enter toggles, edits, or acts depending on the row.
    fn activate(&mut self, enter: bool) {
        let field = self.field();
        let outcome = match field.kind() {
            Kind::Toggle => apply::toggle(&mut self.state, field),
            Kind::Value if enter => {
                // Seeded with the current value so a small change is an edit rather than a retype.
                self.editing = Some(self.state.value(field));
                return;
            }
            Kind::Action if enter => {
                if let Some(prompt) = self.arm(field) {
                    self.message = Some(Ok(prompt));
                    return;
                }
                apply::act(&mut self.state, field)
            }
            _ => return,
        };
        self.armed = None;
        self.message = Some(outcome.map_err(|error| format!("{error:#}")));
    }

    /// Arm a row that needs confirming, or report that it is already armed.
    ///
    /// Returns the sentence to show while it waits, and `None` when the action should go ahead — either
    /// because the row needs no confirmation or because this is the second Enter. The reason for asking
    /// before acting rather than afterwards is that the row underneath the benchmark rows deletes
    /// history, and the two are one keystroke apart.
    fn arm(&mut self, field: Field) -> Option<String> {
        if !field.needs_confirmation() {
            return None;
        }
        // An unavailable row is refused by `apply` with a reason, which is more useful than a
        // confirmation prompt for something that will not happen.
        if self.state.unavailable(field).is_some() {
            return None;
        }
        match self.armed {
            Some((armed, at)) if armed == field && at.elapsed() < CONFIRM_WINDOW => None,
            _ => {
                self.armed = Some((field, Instant::now()));
                Some(format!(
                    "press enter again to {}",
                    field.label().to_lowercase()
                ))
            }
        }
    }

    fn handle_edit(&mut self, code: KeyCode) -> Flow {
        let Some(buffer) = self.editing.as_mut() else {
            return Flow::Continue;
        };
        match code {
            KeyCode::Esc => {
                self.editing = None;
            }
            KeyCode::Enter => {
                let text = buffer.clone();
                self.editing = None;
                let field = self.field();
                self.message = Some(
                    apply::commit(&mut self.state, field, &text).map_err(|e| format!("{e:#}")),
                );
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            // Control characters would be invisible in the field and meaningless to every parser behind it.
            KeyCode::Char(character) if !character.is_control() => buffer.push(character),
            _ => {}
        }
        Flow::Continue
    }

    fn draw(&self, frame: &mut Frame) {
        let [status, rows, help, footer] = Layout::vertical([
            Constraint::Length(6),
            Constraint::Min(0),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.draw_status(frame, status);
        self.draw_rows(frame, rows);
        self.draw_help(frame, help);
        frame.render_widget(
            if self.editing.is_some() {
                Footer::new(EDIT_HINTS)
            } else {
                Footer::new(HINTS)
            },
            footer,
        );
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let running = self
            .status
            .as_ref()
            .and_then(|status| status.as_ref().ok())
            .is_some_and(|report| report.daemon_running);
        // Spans rather than one formatted string, because the state word is the only part of this title that
        // means something: it wears a status colour while the name around it keeps the heading's. Not
        // collecting is dim rather than a warning — a daemon that is off is often off on purpose.
        // Both arms name a colour, because a span inside a title inherits the heading's: `absent` alone sets
        // modifiers only, which would leave "not collecting" italic in the accent rather than out of the way.
        let (state, state_style) = if running {
            ("collecting", Style::default().fg(theme::good()))
        } else {
            ("not collecting", theme::absent().fg(theme::ink()))
        };
        let block = Block::bordered()
            .border_style(theme::border())
            .title_style(theme::heading())
            .title(Line::from(vec![
                Span::raw(" AgentBench — "),
                Span::styled(state, state_style),
                Span::raw(" "),
            ]));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 {
            return;
        }
        let lines: Vec<Line> = match &self.status {
            Some(Ok(report)) => report
                .summary()
                .into_iter()
                // The data directory is on the status band's first line elsewhere; here the rows below are
                // about changing things, so the band stays to what is happening.
                .filter(|(label, _)| *label != "Data directory")
                .take(inner.height as usize)
                .map(|(label, value)| {
                    Line::from(vec![
                        Span::styled(format!("{label:<16}"), theme::label()),
                        Span::styled(value, theme::value()),
                    ])
                })
                .collect(),
            Some(Err(error)) => vec![Line::styled(error.as_str(), theme::absent())],
            None => vec![Line::styled("reading…", theme::absent())],
        };
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_rows(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        // Section headings are interleaved with rows, so the visible window is computed over the combined
        // list. Scrolled to keep the selected row on screen rather than paging, because the list is short
        // enough that a moving window reads better than a jumping one.
        let mut entries: Vec<Entry> = Vec::new();
        let mut current: Option<Section> = None;
        let mut selected_line = 0;
        for (index, field) in self.fields.iter().enumerate() {
            let section = field.section();
            if current != Some(section) {
                entries.push(Entry::Heading(section));
                current = Some(section);
            }
            if index == self.selected {
                selected_line = entries.len();
            }
            entries.push(Entry::Row(index, *field));
        }
        let height = area.height as usize;
        let first = selected_line.saturating_sub(height.saturating_sub(1) / 2);
        let first = first.min(entries.len().saturating_sub(height));
        let visible = entries.iter().skip(first).take(height);
        let strips = Layout::vertical(vec![Constraint::Length(1); height]).split(area);
        for (strip, entry) in strips.iter().zip(visible) {
            match entry {
                Entry::Heading(section) => {
                    frame.render_widget(Line::styled(section.title(), theme::heading()), *strip);
                }
                Entry::Row(index, field) => self.draw_row(frame, *strip, *index, *field),
            }
        }
    }

    fn draw_row(&self, frame: &mut Frame, area: Rect, index: usize, field: Field) {
        let focused = index == self.selected;
        let editing = focused && self.editing.is_some();
        let value = match &self.editing {
            Some(buffer) if focused => format!("{buffer}▏"),
            _ => self.state.value(field),
        };
        let unavailable = self.state.unavailable(field).is_some();
        let value_style = if editing {
            theme::selected()
        } else if unavailable {
            theme::absent()
        } else {
            theme::value()
        };
        let mut reading = Reading::new(field.label(), &value)
            .value_style(value_style)
            .label_width(LABEL_WIDTH);
        if focused && !editing {
            reading = reading.value_style(if unavailable {
                theme::absent()
            } else {
                theme::value()
            });
        }
        // The focus marker is the row's own inset rather than a reversed line: reversing a whole row in a
        // terminal whose theme this module does not know can make the value harder to read, not easier.
        let [marker, body] =
            Layout::horizontal([Constraint::Length(2), Constraint::Min(0)]).areas(area);
        frame.render_widget(
            Line::styled(
                if focused { "▸" } else { " " },
                if focused {
                    Style::default().fg(theme::accent())
                } else {
                    theme::hint()
                },
            ),
            marker,
        );
        frame.render_widget(reading, body);
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let field = self.field();
        // A failure outranks the help text: it is about something the user just did, and the help will
        // still be there next time they land on the row.
        let (text, style) = match &self.message {
            Some(Err(error)) => (error.clone(), Style::default().fg(theme::critical())),
            Some(Ok(message)) => (message.clone(), Style::default().fg(theme::good())),
            None => match self.state.unavailable(field) {
                Some(reason) => (reason, theme::absent()),
                None => (field.help().to_string(), theme::hint()),
            },
        };
        frame.render_widget(
            Paragraph::new(Line::styled(text, style)).wrap(Wrap { trim: true }),
            area,
        );
    }
}

/// A line in the settings list: either a heading or a row.
enum Entry {
    Heading(Section),
    Row(usize, Field),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use tempfile::tempdir;

    fn app() -> App {
        let temp = tempdir().expect("a temporary data directory");
        let config =
            WatchConfig::load(Some(temp.path().to_path_buf())).expect("defaults should load");
        // The directory is dropped here, which is deliberate: the screen must draw against a data directory
        // that has no database, since that is what a first run looks like.
        App::new(config).expect("the app should build")
    }

    fn draw_at(app: &App, width: u16, height: u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
    }

    #[test]
    fn the_screen_draws_at_a_comfortable_size() {
        draw_at(&app(), 100, 40);
    }

    /// Layout degradation, including sizes smaller than the status band alone.
    #[test]
    fn the_screen_draws_when_cramped() {
        let app = app();
        for (width, height) in [(80, 12), (40, 8), (20, 6), (10, 3), (1, 1)] {
            draw_at(&app, width, height);
        }
    }

    #[test]
    fn the_screen_draws_while_a_value_is_being_edited() {
        let mut app = app();
        app.selected = State::fields()
            .iter()
            .position(|field| *field == Field::SampleInterval)
            .expect("the sample interval row");
        app.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.editing.is_some(), "Enter should open the editor");
        draw_at(&app, 100, 40);
    }

    /// The state word is the only part of the title that means anything, so it has to leave the heading's
    /// accent behind rather than inherit it — a span that sets modifiers only would come out italic in cyan.
    #[test]
    fn the_state_word_in_the_title_is_not_drawn_in_the_accent() {
        let app = app();
        let mut terminal = Terminal::new(TestBackend::new(60, 40)).expect("test terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        // No daemon runs against this data directory, so the band reports "not collecting" in text ink.
        // Found past the em dash rather than by letter: "AgentBench" has an n of its own, in the accent.
        let row: Vec<_> = (0..60).map(|column| &buffer[(column, 0)]).collect();
        let dash = row
            .iter()
            .position(|cell| cell.symbol() == "—")
            .expect("the title should hold an em dash");
        let state = row[dash..]
            .iter()
            .find(|cell| cell.symbol().starts_with('n'))
            .expect("the state word should follow the em dash");
        assert_eq!(state.fg, theme::ink(), "the off state wears text ink");
        assert_ne!(state.fg, theme::accent(), "and not the accent around it");
    }

    #[test]
    fn navigation_wraps_at_both_ends() {
        let mut app = app();
        let last = app.fields.len() - 1;
        app.handle(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            app.selected, last,
            "up from the first row wraps to the last"
        );
        app.handle(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.selected, 0, "down from the last row wraps to the first");
    }

    #[test]
    fn q_and_ctrl_c_quit_but_a_bare_c_does_not() {
        let mut app = app();
        assert_eq!(
            app.handle(KeyCode::Char('q'), KeyModifiers::NONE),
            Flow::Quit
        );
        assert_eq!(
            app.handle(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Flow::Quit
        );
        assert_eq!(
            app.handle(KeyCode::Char('c'), KeyModifiers::NONE),
            Flow::Continue
        );
    }

    /// While editing, `q` is a character and must not quit the screen.
    #[test]
    fn keys_are_captured_by_the_editor_while_it_is_open() {
        let mut app = app();
        app.selected = State::fields()
            .iter()
            .position(|field| *field == Field::ServerPort)
            .expect("the port row");
        app.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            app.handle(KeyCode::Char('q'), KeyModifiers::NONE),
            Flow::Continue
        );
        assert_eq!(app.editing.as_deref(), Some("7878q"));
        app.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.editing.as_deref(), Some("7878"));
        app.handle(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.editing.is_none(), "Esc should close the editor");
    }

    /// An unreadable entry has to be reported rather than substituted.
    #[test]
    fn committing_an_unparseable_value_reports_an_error() {
        let mut app = app();
        app.selected = State::fields()
            .iter()
            .position(|field| *field == Field::ServerPort)
            .expect("the port row");
        app.handle(KeyCode::Enter, KeyModifiers::NONE);
        app.editing = Some("not-a-port".into());
        app.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            matches!(&app.message, Some(Err(error)) if error.contains("not a port number")),
            "{:?}",
            app.message
        );
    }

    /// A label wider than its column is silently truncated by the row widget, so the screen would show
    /// "Run a benchmark, elevat" and nobody would know why. Caught here rather than by looking at it,
    /// because it only shows up for the longest label and only once someone adds one.
    #[test]
    fn every_label_fits_the_column_reserved_for_it() {
        for field in State::fields() {
            let width = u16::try_from(field.label().chars().count()).expect("a short label");
            assert!(
                width <= LABEL_WIDTH,
                "{:?} is {width} columns wide but only {LABEL_WIDTH} are reserved",
                field.label()
            );
        }
    }

    /// Select a row by field, failing loudly if it is no longer in the list.
    fn select(app: &mut App, field: Field) {
        app.selected = State::fields()
            .iter()
            .position(|candidate| *candidate == field)
            .unwrap_or_else(|| panic!("{field:?} should be a row"));
    }

    /// The destructive row asks first, and one Enter alone does nothing.
    #[test]
    fn erasing_needs_a_second_enter() {
        let mut app = app();
        // A database to erase, so the row is not disabled for lack of one.
        app.state.database_bytes = Some(4_096);
        app.state.daemon_running = false;
        select(&mut app, Field::EraseCollectedData);

        app.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.armed.is_some(), "the first Enter should arm the row");
        assert!(
            matches!(&app.message, Some(Ok(text)) if text.contains("press enter again")),
            "{:?}",
            app.message
        );

        app.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.armed.is_none(), "acting disarms");
        // The temporary data directory is already gone, so there is nothing to erase and the action
        // says so. What matters here is that it ran at all.
        assert!(app.message.is_some());
    }

    /// Moving away from an armed row must not leave it armed for whatever is selected next.
    #[test]
    fn navigating_away_disarms_the_row() {
        let mut app = app();
        app.state.database_bytes = Some(4_096);
        app.state.daemon_running = false;
        select(&mut app, Field::EraseCollectedData);
        app.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.armed.is_some());

        app.handle(KeyCode::Up, KeyModifiers::NONE);
        assert!(
            app.armed.is_none(),
            "the selection moved, so the answer lapses"
        );
    }

    /// A stale confirmation is not a confirmation.
    #[test]
    fn an_expired_arming_asks_again_rather_than_acting() {
        let mut app = app();
        app.state.database_bytes = Some(4_096);
        app.state.daemon_running = false;
        select(&mut app, Field::EraseCollectedData);
        app.armed = Some((
            Field::EraseCollectedData,
            Instant::now() - CONFIRM_WINDOW - Duration::from_secs(1),
        ));

        app.handle(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            matches!(&app.message, Some(Ok(text)) if text.contains("press enter again")),
            "an Enter outside the window should re-arm, not erase: {:?}",
            app.message
        );
    }

    /// Rows that only start something must not ask for confirmation.
    #[test]
    fn only_the_destructive_row_confirms() {
        for field in State::fields() {
            assert_eq!(
                field.needs_confirmation(),
                field == Field::EraseCollectedData,
                "{field:?}"
            );
        }
    }

    /// Space on an action row must do nothing rather than fall through to something else.
    #[test]
    fn space_does_not_trigger_an_action_row() {
        let mut app = app();
        app.selected = State::fields()
            .iter()
            .position(|field| *field == Field::OpenDashboard)
            .expect("the open dashboard row");
        app.handle(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(app.message.is_none(), "{:?}", app.message);
    }
}
