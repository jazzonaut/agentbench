//! A notification-area icon for a daemon running without a console.
//!
//! Only the menu's shape lives here; the window, the icon and the message loop are in [`imp`]. The split is
//! the same one [`install`] makes and for the same reason: what the menu contains is decidable without a
//! desktop and is therefore tested on every platform, while `Shell_NotifyIconW` is not.
//!
//! [`install`]: crate::install

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

#[cfg(not(windows))]
mod fallback;
#[cfg(not(windows))]
use fallback as imp;

use anyhow::Result;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// What the user picked from the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// Open the dashboard in a browser.
    OpenDashboard,
    /// Open the control centre in a new console.
    Settings,
    /// Stop collecting and exit.
    Quit,
}

impl Item {
    /// Menu entries in display order.
    ///
    /// The default action — what a double-click does — is the first entry, which is why opening the
    /// dashboard is at the top: it is the thing a tray icon for this daemon is most often clicked for.
    pub const ALL: [Self; 3] = [Self::OpenDashboard, Self::Settings, Self::Quit];

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenDashboard => "Open dashboard",
            Self::Settings => "Settings…",
            Self::Quit => "Stop collecting and quit",
        }
    }

    /// Identifier used for the menu entry.
    ///
    /// Starts at one because zero is what `TrackPopupMenu` returns when the menu was dismissed without a
    /// choice, so a command with that identifier could never be told apart from a cancelled menu.
    pub fn command_id(self) -> u32 {
        match self {
            Self::OpenDashboard => 1,
            Self::Settings => 2,
            Self::Quit => 3,
        }
    }

    /// The entry a command identifier refers to.
    pub fn from_command_id(id: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|item| item.command_id() == id)
    }
}

/// What the tray shows about the daemon behind it.
pub struct Status {
    /// Tooltip text, kept short: Windows truncates a tooltip at 128 characters.
    pub tooltip: String,
}

/// Run the tray icon and its message loop until the user quits or `shutdown` is set elsewhere.
///
/// Blocks. The daemon runs on another thread and shares `shutdown`, so quitting from the menu and stopping
/// with a signal converge on the same cooperative stop rather than killing the process and leaving the
/// database's writer thread mid-transaction.
pub fn run(shutdown: Arc<AtomicBool>, status: Status, on: impl FnMut(Item)) -> Result<()> {
    imp::run(shutdown, status, on)
}

/// Whether a tray icon is possible here.
pub fn is_supported() -> bool {
    imp::is_supported()
}

/// A tooltip describing where the dashboard is, within the length Windows will show.
///
/// Truncated by this function rather than by the shell, which cuts silently mid-word.
pub fn tooltip(url: Option<&str>) -> String {
    /// Longest tooltip Windows will display, including its terminator.
    const LIMIT: usize = 127;
    let text = match url {
        Some(url) => format!("AgentBench — collecting, dashboard at {url}"),
        None => "AgentBench — collecting, dashboard disabled".to_string(),
    };
    if text.chars().count() <= LIMIT {
        return text;
    }
    text.chars().take(LIMIT - 1).chain(['…']).collect()
}

/// Whether the daemon has been asked to stop.
///
/// Not `#[cfg(windows)]`: reading an `AtomicBool` is not platform-specific, and the attribute would claim it
/// is. It simply has one caller today, in the Windows message loop, so on a platform whose [`imp`] is the
/// fallback it is unused — and `-D warnings` treats that as an error.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_stopping(shutdown: &AtomicBool) -> bool {
    shutdown.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_item_has_a_label_and_a_nonzero_identifier() {
        for item in Item::ALL {
            assert!(!item.label().is_empty(), "{item:?} has no label");
            assert_ne!(
                item.command_id(),
                0,
                "{item:?} uses the identifier a dismissed menu returns"
            );
        }
    }

    #[test]
    fn identifiers_are_unique_and_round_trip() {
        for item in Item::ALL {
            assert_eq!(Item::from_command_id(item.command_id()), Some(item));
        }
        let ids: Vec<u32> = Item::ALL.iter().map(|item| item.command_id()).collect();
        for (index, id) in ids.iter().enumerate() {
            assert!(!ids[index + 1..].contains(id), "identifier {id} is reused");
        }
    }

    /// Zero means "dismissed", and anything unrecognised must not be mistaken for an entry.
    #[test]
    fn an_unknown_identifier_is_not_an_item() {
        assert_eq!(Item::from_command_id(0), None);
        assert_eq!(Item::from_command_id(999), None);
    }

    /// Opening the dashboard is the default action, so it has to stay first.
    #[test]
    fn the_default_action_is_first() {
        assert_eq!(Item::ALL.first(), Some(&Item::OpenDashboard));
    }

    #[test]
    fn a_tooltip_names_the_dashboard_or_says_it_is_off() {
        assert!(tooltip(Some("http://127.0.0.1:7878/")).contains("7878"));
        assert!(tooltip(None).contains("disabled"));
    }

    /// The shell truncates silently at 128 characters; this must do it visibly and stay inside the limit.
    #[test]
    fn a_long_tooltip_is_truncated_with_an_ellipsis() {
        let long = format!("http://127.0.0.1:7878/{}", "x".repeat(200));
        let tooltip = tooltip(Some(&long));
        assert_eq!(tooltip.chars().count(), 127);
        assert!(tooltip.ends_with('…'), "{tooltip}");
    }
}
