//! `agentbench top`: what this machine and the agent's process tree are doing right now.

use crate::{
    process_tree::{self, TreeUsage},
    ui::{
        Screen, format, theme,
        widgets::{Footer, History, Meter, Reading, Series},
    },
};
use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    widgets::Block,
};
use std::time::{Duration, Instant};
use sysinfo::{ProcessesToUpdate, System};

/// Readings kept per sparkline.
///
/// At the 500ms default interval this is a minute of history, which is the timescale a human watching for
/// "did that command cause this" actually works on. A wider window would be plotted into the same handful
/// of columns and read as noise.
const WINDOW: usize = 120;

/// Column reserved for field names, so every bar on the screen starts at the same offset.
const LABEL_WIDTH: u16 = 13;

/// Keys this screen answers to.
const HINTS: &[(&str, &str)] = &[("q", "quit")];

/// Watch the machine and one process tree until the user quits.
pub fn top(pid: Option<u32>, name: Option<&str>, interval_ms: u64) -> Result<()> {
    if interval_ms < 100 {
        bail!("--interval-ms must be at least 100");
    }
    let interval = Duration::from_millis(interval_ms);
    let mut app = App::new(pid, name.unwrap_or("claude"));
    let mut screen = Screen::enter()?;
    loop {
        app.refresh();
        screen.terminal().draw(|frame| app.draw(frame))?;
        // The poll interval is the redraw cadence: a keypress ends the wait early, so the screen stays
        // responsive without sampling faster than asked.
        if event::poll(interval)?
            && let Event::Key(key) = event::read()?
            // Press only. Windows reports a release event for the same key, so without this a single
            // keystroke arrives twice — harmless for quitting, and the reason to establish the habit here
            // rather than discover it on a screen where it matters.
            && key.kind == KeyEventKind::Press
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            return Ok(());
        }
    }
}

/// Everything the screen needs, refreshed in one place and drawn from another.
struct App<'a> {
    system: System,
    pid: Option<u32>,
    name: &'a str,
    started: Instant,
    /// `100 * cores`: the ceiling a whole-tree CPU figure is measured against.
    cpu_ceiling: u64,
    cpu: Series,
    memory: Series,
    tree_cpu: Series,
    /// The most recent reading, so drawing never touches `sysinfo`.
    latest: Latest,
}

/// One refresh's worth of derived values.
#[derive(Default)]
struct Latest {
    elapsed: Duration,
    cpu_percent: f32,
    used_memory: u64,
    total_memory: u64,
    used_swap: u64,
    total_swap: u64,
    /// The selected root's pid and name, if one was found.
    root: Option<(u32, String)>,
    usage: TreeUsage,
}

impl<'a> App<'a> {
    fn new(pid: Option<u32>, name: &'a str) -> Self {
        let system = System::new_all();
        let cores = system.cpus().len().max(1) as u64;
        Self {
            system,
            pid,
            name,
            started: Instant::now(),
            cpu_ceiling: cores * 100,
            cpu: Series::new(WINDOW),
            memory: Series::new(WINDOW),
            tree_cpu: Series::new(WINDOW),
            latest: Latest::default(),
        }
    }

    fn refresh(&mut self) {
        self.system.refresh_processes(ProcessesToUpdate::All, true);
        self.system.refresh_cpu_all();
        self.system.refresh_memory();

        let root = process_tree::select(&self.system, self.pid, self.name);
        let usage = root
            .map(|root| {
                let tree = process_tree::descendants(&self.system, root);
                process_tree::usage(&self.system, &tree)
            })
            .unwrap_or_default();
        self.latest = Latest {
            elapsed: self.started.elapsed(),
            cpu_percent: self.system.global_cpu_usage(),
            used_memory: self.system.used_memory(),
            total_memory: self.system.total_memory(),
            used_swap: self.system.used_swap(),
            total_swap: self.system.total_swap(),
            root: root.map(|pid| {
                let name = self
                    .system
                    .process(pid)
                    .map(|process| process.name().to_string_lossy().into_owned())
                    // Selected a moment ago and gone already: worth saying so rather than showing a bare
                    // pid, since a tree that exits mid-watch is exactly what a user is often looking for.
                    .unwrap_or_else(|| "exited".into());
                (pid.as_u32(), name)
            }),
            usage,
        };

        self.cpu.push(self.latest.cpu_percent.max(0.0) as u64);
        self.memory.push(self.latest.used_memory);
        self.tree_cpu
            .push(self.latest.usage.cpu_percent.max(0.0) as u64);
    }

    fn draw(&self, frame: &mut Frame) {
        // `Min(0)` on the tree panel is what lets a short terminal degrade: the system panel and the
        // footer keep their rows and the tree panel shrinks to nothing, rather than the whole layout
        // overflowing.
        let [system, tree, footer] = Layout::vertical([
            Constraint::Length(5),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.draw_system(frame, system);
        self.draw_tree(frame, tree);
        frame.render_widget(Footer::new(HINTS), footer);
    }

    fn draw_system(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_style(theme::border())
            .title_style(theme::heading())
            .title(format!(
                " System — {} ",
                format::seconds(self.latest.elapsed)
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(rows) = rows::<3>(inner) else {
            return;
        };
        frame.render_widget(
            History::new("CPU", &self.cpu, &format::percent(self.latest.cpu_percent))
                .max(100)
                .label_width(LABEL_WIDTH),
            rows[0],
        );
        frame.render_widget(
            Meter::new(
                "Memory",
                format::ratio(self.latest.used_memory, self.latest.total_memory),
                &format::gib_of(self.latest.used_memory, self.latest.total_memory),
            )
            .label_width(LABEL_WIDTH),
            rows[1],
        );
        frame.render_widget(
            Meter::new(
                "Swap",
                format::ratio(self.latest.used_swap, self.latest.total_swap),
                &format::gib_of(self.latest.used_swap, self.latest.total_swap),
            )
            .label_width(LABEL_WIDTH),
            rows[2],
        );
    }

    fn draw_tree(&self, frame: &mut Frame, area: Rect) {
        let title = match &self.latest.root {
            Some((pid, name)) => format!(" Process tree — {name} ({pid}) "),
            None => format!(" Process tree — no process matching {:?} ", self.name),
        };
        let block = Block::bordered()
            .border_style(theme::border())
            .title_style(theme::heading())
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if self.latest.root.is_none() {
            if let Some(rows) = rows::<1>(inner) {
                frame.render_widget(
                    Reading::new("", "showing system metrics until one appears")
                        .value_style(theme::absent()),
                    rows[0],
                );
            }
            return;
        }
        let Some(rows) = rows::<5>(inner) else {
            return;
        };
        let usage = &self.latest.usage;
        frame.render_widget(
            History::new("CPU", &self.tree_cpu, &format::percent(usage.cpu_percent))
                // Against every core, not against one. A four-thread compile legitimately reports 400%,
                // and a plot capped at 100 would show it as flat saturation from the first frame.
                .max(self.cpu_ceiling)
                .colour(theme::series(1).unwrap_or_default())
                .label_width(LABEL_WIDTH),
            rows[0],
        );
        frame.render_widget(
            Meter::new(
                "Resident",
                format::ratio(usage.rss_bytes, self.latest.total_memory),
                &format::gib(usage.rss_bytes),
            )
            .label_width(LABEL_WIDTH),
            rows[1],
        );
        for (row, (label, value)) in rows[2..].iter().zip([
            ("Processes", usage.process_count.to_string()),
            ("Read", format::mib(usage.read_bytes)),
            ("Written", format::mib(usage.written_bytes)),
        ]) {
            frame.render_widget(Reading::new(label, &value).label_width(LABEL_WIDTH), *row);
        }
    }
}

/// Split `area` into `N` single-row strips, or `None` if it cannot hold them.
///
/// Returning `None` rather than clamping is what makes a cramped panel draw nothing instead of drawing a
/// misleading subset: three meters in a two-row space would silently drop swap, and a reader has no way to
/// tell a hidden row from a zero one.
fn rows<const N: usize>(area: Rect) -> Option<[Rect; N]> {
    if area.height < N as u16 || area.width == 0 {
        return None;
    }
    Some(Layout::vertical([Constraint::Length(1); N]).areas(area))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn draw_at(width: u16, height: u16) {
        let mut app = App::new(Some(std::process::id()), "no-such-process");
        app.refresh();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
    }

    #[test]
    fn the_screen_draws_at_a_comfortable_size() {
        draw_at(100, 30);
    }

    /// The layout-degradation requirement: a terminal too small for the panels must still render.
    #[test]
    fn the_screen_draws_when_cramped() {
        for (width, height) in [(20, 6), (10, 3), (40, 1), (1, 1)] {
            draw_at(width, height);
        }
    }

    #[test]
    fn an_interval_below_the_floor_is_refused() {
        let error = top(None, None, 99).unwrap_err().to_string();
        assert!(error.contains("at least 100"), "{error}");
    }

    /// A pid that does not exist selects nothing, which must draw as "no process" rather than panic.
    #[test]
    fn a_missing_process_tree_still_draws() {
        let mut app = App::new(Some(u32::MAX), "\u{0}no-such-process\u{0}");
        app.refresh();
        assert!(app.latest.root.is_none());
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("test terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
    }
}
