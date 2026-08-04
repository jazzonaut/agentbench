//! The daemon with no console window and a notification-area icon.
//!
//! A second binary rather than a flag on the first. The Windows subsystem is chosen at link time, not at
//! run time: setting it on `agentbench` would give the whole tool no console, which takes out `top`'s live
//! screen, the control centre, and every line of report output the other subcommands print. So the choice
//! is made by which executable gets started, and the two share everything through the library.
//!
//! Both threads are load-bearing. The message loop has to own the main thread — a notification icon belongs
//! to the thread that created its window, and the shell delivers to that thread — so collection runs behind
//! it. They meet at one shared shutdown flag, which means quitting from the menu takes the same cooperative
//! path as a signal: the writer thread finishes its transaction and the database closes cleanly, rather than
//! the process being torn down mid-write.

// No console. This is the entire reason the binary exists, and it is why nothing here prints: there would be
// nowhere for it to go. What the daemon would have narrated goes to its own event log, which the dashboard
// and `dashboard --status` both show.
#![cfg_attr(windows, windows_subsystem = "windows")]

use agentbench::{
    install, tray,
    watch::{self, Narrator, WatchConfig},
};
use anyhow::{Context, Result};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// The one argument this binary reads, before it decides it is a daemon.
///
/// Named the same as the console build's hidden subcommand, and answered the same way: by doing nothing
/// and exiting successfully. Nothing here should ever be asked for it — the process workload resolves the
/// console build precisely so it is not — but "start a daemon" is the wrong answer to a request to launch
/// and exit, and getting it wrong cost a notification-area icon per launch and a failed phase per probe.
const NOOP_ARGUMENT: &str = "internal-noop";

fn main() -> Result<()> {
    if std::env::args_os().any(|argument| argument == NOOP_ARGUMENT) {
        return Ok(());
    }
    let config = WatchConfig::load(None)?;
    if !tray::is_supported() {
        // Falling back to running the daemon rather than refusing. On a platform with no notification area
        // this binary is still a perfectly good windowless daemon, and a scheduled task pointing at it should
        // not start failing because the icon is unavailable.
        return watch::run(config);
    }

    let url = config
        .server
        .enabled
        .then(|| format!("http://{}:{}/", config.server.bind, config.server.port));
    let tooltip = tray::tooltip(url.as_deref());
    let shutdown = Arc::new(AtomicBool::new(false));

    // Collection runs behind the icon. The flag is set when it returns for any reason, so a daemon that stops
    // on its own — a lost instance lock, a fatal store error — takes the icon down with it instead of leaving
    // one that claims to be collecting.
    let daemon = {
        let config = config.clone();
        let shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("daemon".into())
            .spawn(move || {
                let outcome = watch::run_with(config, &Narrator::Silent, shutdown.clone());
                shutdown.store(true, Ordering::Relaxed);
                outcome
            })
            .context("start the collection thread")?
    };

    let program = std::env::current_exe().ok();
    tray::run(
        shutdown.clone(),
        tray::Status { tooltip },
        |item| match item {
            tray::Item::OpenDashboard => {
                if let Some(url) = &url {
                    // Errors are dropped rather than reported: with no console and no window there is nowhere
                    // to report them to, and a failed browser launch is not a reason to stop collecting.
                    let _ = install::open(url);
                }
            }
            tray::Item::Settings => {
                // The console build, with no arguments, which is the control centre. It gets a console of its
                // own from the shell — this process has none to lend it.
                if let Some(program) = program.as_deref().and_then(console_sibling) {
                    let _ = install::run_detached(&program, "");
                }
            }
            // Handled by the tray loop, which sets the shutdown flag. Nothing to add here.
            tray::Item::Quit => {}
        },
    )?;

    // Joined rather than abandoned, so the store's writer finishes and the lock is released before this
    // process exits. A panic inside the daemon thread is re-raised here, which is the same bargain
    // `Supervisor::shutdown` makes: a fault that a restart would not survive should be loud.
    match daemon.join() {
        Ok(outcome) => outcome,
        Err(_) => anyhow::bail!("the collection thread panicked"),
    }
}

/// The console executable sitting beside this one.
///
/// `agentbench-tray.exe` and `agentbench.exe` are installed together, so the settings screen is found by
/// name next to this binary rather than by searching `PATH` — which may not contain either of them, since
/// putting them there is one of the things the settings screen is for.
fn console_sibling(tray: &std::path::Path) -> Option<std::path::PathBuf> {
    let stem = tray.file_stem()?.to_string_lossy();
    let console = stem.strip_suffix("-tray")?;
    let mut path = tray.with_file_name(console);
    if let Some(extension) = tray.extension() {
        path.set_extension(extension);
    }
    path.is_file().then_some(path)
}
