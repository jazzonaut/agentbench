//! The interactive terminal screens.
//!
//! Everything here draws with [`ratatui`] and takes its colours from [`theme`]. The batch output this
//! crate produces — report paths, comparison tables, `--status` — deliberately does not: that output is
//! piped and diffed by scripts, and a screen is not something you can redirect to a file.

pub mod control;
pub mod format;
pub mod theme;
pub mod widgets;

mod task;
mod top;

pub use task::run_task;
pub use top::top;

use anyhow::{Context, Result};
use ratatui::DefaultTerminal;

/// An initialised terminal that restores itself when dropped.
///
/// Both halves of the restore are load-bearing and neither is redundant. [`ratatui::try_init`] installs a
/// panic hook, which covers a panic inside a drawing routine; the [`Drop`] here covers the ordinary paths,
/// including an early return through `?` partway down a screen's setup. Without the guard, a failed
/// refresh would leave the caller's terminal in raw mode with the alternate screen still active — which
/// looks to the user like the tool hung the shell.
pub struct Screen {
    terminal: DefaultTerminal,
}

impl Screen {
    /// Enable raw mode, switch to the alternate screen, and take ownership of the terminal.
    pub fn enter() -> Result<Self> {
        let terminal = ratatui::try_init().context("initialise the terminal")?;
        Ok(Self { terminal })
    }

    pub fn terminal(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        ratatui::restore();
    }
}
