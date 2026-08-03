//! The screen a benchmark runs behind: phase progress plus what the machine is doing while it happens.

use crate::{
    bench::{Phase, Progress},
    ui::{
        Screen, format, theme,
        widgets::{Footer, History, Meter, Series},
    },
};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    widgets::{Block, Gauge},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};
use sysinfo::System;

/// Readings kept per sparkline.
const WINDOW: usize = 120;

/// Column reserved for field names.
const LABEL_WIDTH: u16 = 13;

/// Redraw cadence.
const TICK: Duration = Duration::from_millis(250);

/// Keys this screen answers to.
const HINTS: &[(&str, &str)] = &[("q", "cancel safely"), ("Ctrl+C", "cancel safely")];

/// Run `task` on a worker thread, drawing its progress until it finishes.
///
/// `task` receives the cancellation flag and the progress sink to report phases through. Both are handed
/// in rather than created by the caller so that the channel's receiving end stays with the screen: a
/// benchmark cannot announce a phase to a screen that has not been drawn yet.
pub fn run_task<T, F>(title: &str, task: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(Arc<AtomicBool>, Progress) -> Result<T> + Send + 'static,
{
    let (result_sender, result_receiver) = mpsc::sync_channel(1);
    let (phase_sender, phase_receiver) = mpsc::channel::<Phase>();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    thread::spawn(move || {
        let _ = result_sender.send(task(worker_cancel, Progress::Channel(phase_sender)));
    });

    let mut app = App::new(title);
    let mut screen = Screen::enter()?;
    loop {
        // Drain rather than take one: a fast phase can be announced twice inside one tick, and the screen
        // wants the latest, not a backlog it will never catch up on. Both error cases end the drain — an
        // empty channel because there is nothing more to read, a disconnected one because there never will
        // be, and the result channel below is what distinguishes those.
        while let Ok(phase) = phase_receiver.try_recv() {
            app.phase = Some(phase);
        }
        app.refresh(cancel.load(Ordering::Relaxed));
        screen.terminal().draw(|frame| app.draw(frame))?;

        // Checked after drawing, so the last phase reached is on screen when the run ends rather than
        // being skipped by an immediate return.
        match result_receiver.try_recv() {
            Ok(result) => return result,
            Err(TryRecvError::Empty) => {}
            // The worker died without sending, which `catch_unwind` in the supervisor would have reported
            // for a collector but nothing reports here. Falling through would spin this loop forever on a
            // screen that never changes.
            Err(TryRecvError::Disconnected) => {
                anyhow::bail!("the benchmark thread ended without producing a result")
            }
        }

        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && matches!(
                key.code,
                KeyCode::Char('q') | KeyCode::Char('c') | KeyCode::Esc
            )
        {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

struct App<'a> {
    title: &'a str,
    system: System,
    started: Instant,
    phase: Option<Phase>,
    cancelling: bool,
    cpu: Series,
    cpu_percent: f32,
    used_memory: u64,
    total_memory: u64,
    elapsed: Duration,
}

impl<'a> App<'a> {
    fn new(title: &'a str) -> Self {
        Self {
            title,
            system: System::new_all(),
            started: Instant::now(),
            phase: None,
            cancelling: false,
            cpu: Series::new(WINDOW),
            cpu_percent: 0.0,
            used_memory: 0,
            total_memory: 0,
            elapsed: Duration::ZERO,
        }
    }

    fn refresh(&mut self, cancelling: bool) {
        // CPU and memory only. The screen this replaced called `refresh_all`, which enumerates the whole
        // process table four times a second — the most expensive thing this crate does per unit time, and
        // it was being done *during* the measurement it would then distort.
        self.system.refresh_cpu_all();
        self.system.refresh_memory();
        self.cancelling = cancelling;
        self.cpu_percent = self.system.global_cpu_usage();
        self.used_memory = self.system.used_memory();
        self.total_memory = self.system.total_memory();
        self.elapsed = self.started.elapsed();
        self.cpu.push(self.cpu_percent.max(0.0) as u64);
    }

    fn draw(&self, frame: &mut Frame) {
        let [body, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());
        let block = Block::bordered()
            .border_style(theme::border())
            .title_style(theme::heading())
            .title(format!(
                " {} — {} ",
                self.title,
                format::seconds(self.elapsed)
            ));
        let inner = block.inner(body);
        frame.render_widget(block, body);
        self.draw_body(frame, inner);
        let footer_widget = if self.cancelling {
            Footer::new(HINTS).status("Cancellation requested; cleaning up temporary data…")
        } else {
            Footer::new(HINTS)
        };
        frame.render_widget(footer_widget, footer);
    }

    fn draw_body(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        // A blank row between the phase gauge and the system readings: the gauge is the thing being
        // watched, and a bar butted straight against a sparkline reads as one control.
        let [phase, _gap, cpu, memory] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_phase(frame, phase);
        if cpu.height > 0 {
            frame.render_widget(
                History::new("CPU", &self.cpu, &format::percent(self.cpu_percent))
                    .max(100)
                    .label_width(LABEL_WIDTH),
                cpu,
            );
        }
        if memory.height > 0 {
            frame.render_widget(
                Meter::new(
                    "Memory",
                    format::ratio(self.used_memory, self.total_memory),
                    &format::gib_of(self.used_memory, self.total_memory),
                )
                .label_width(LABEL_WIDTH),
                memory,
            );
        }
    }

    fn draw_phase(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let (ratio, label) = match &self.phase {
            Some(phase) => (
                f64::from(u32::try_from(phase.number).unwrap_or(0))
                    / f64::from(u32::try_from(phase.total.max(1)).unwrap_or(1)),
                phase.line(),
            ),
            // Before the first announcement. Preflight checks free space and sizes the workloads, which on
            // a slow volume is a visible pause, so it is worth naming rather than showing an idle bar.
            None => (0.0, "Preparing…".to_string()),
        };
        frame.render_widget(
            Gauge::default()
                .ratio(ratio.clamp(0.0, 1.0))
                .label(label)
                // A series colour, not a status colour. `theme::pressure` would paint an almost-finished
                // benchmark red, which reads as a fault at the exact moment the run is going well.
                .gauge_style(Style::default().fg(theme::series(0).unwrap_or_default())),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn app_with_phase(phase: Option<Phase>) -> App<'static> {
        let mut app = App::new("AgentBench");
        app.refresh(false);
        app.phase = phase;
        app
    }

    fn draw_at(app: &App<'_>, width: u16, height: u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
    }

    #[test]
    fn the_screen_draws_before_the_first_phase_arrives() {
        draw_at(&app_with_phase(None), 100, 20);
    }

    #[test]
    fn the_screen_draws_with_a_phase() {
        let phase = Phase {
            number: 3,
            total: crate::bench::PHASE_COUNT,
            label: "Filesystem benchmark".into(),
        };
        draw_at(&app_with_phase(Some(phase)), 100, 20);
    }

    #[test]
    fn the_screen_draws_when_cramped() {
        let app = app_with_phase(None);
        for (width, height) in [(20, 6), (10, 3), (40, 1), (1, 1)] {
            draw_at(&app, width, height);
        }
    }

    /// A phase claiming more steps than exist, or a zero total, must not panic inside `Gauge::ratio`.
    #[test]
    fn an_impossible_phase_still_draws() {
        for (number, total) in [(99, 8), (1, 0), (0, 0)] {
            let phase = Phase {
                number,
                total,
                label: "Impossible".into(),
            };
            draw_at(&app_with_phase(Some(phase)), 80, 10);
        }
    }
}
