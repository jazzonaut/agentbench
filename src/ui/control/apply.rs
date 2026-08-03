//! Carrying out what a row was asked to do.
//!
//! Separated from the screen so that each change is a plain function of the state and the field, testable
//! and readable without a terminal in the picture. Every one of these returns a sentence for the message
//! line: a settings screen that changed something silently gives the user no way to tell a working toggle
//! from a decorative one.

use super::model::{Field, State};
use crate::{
    install,
    watch::{config, settings::Draft},
};
use anyhow::{Context, Result, bail};

/// Preset a benchmark launched from here runs.
const BENCHMARK_PRESET: &str = "standard";

/// Flip a boolean row and carry the change out.
pub fn toggle(state: &mut State, field: Field) -> Result<String> {
    if let Some(reason) = state.unavailable(field) {
        bail!("{reason}");
    }
    match field {
        Field::RunAtLogin => {
            let enabled = state
                .autostart
                .as_ref()
                .map(|current| current.is_enabled())
                .unwrap_or(false);
            if enabled {
                install::disable_autostart()?;
                Ok("collection will no longer start at login".into())
            } else {
                let desired = state
                    .desired_autostart()
                    .context("there is no durable executable to point the task at")?;
                install::enable_autostart(&desired)?;
                Ok(format!(
                    "collection will start {} after login",
                    config::duration_text(desired.delay)
                ))
            }
        }
        // Both of these describe a task rather than being one, so they re-register whatever is registered.
        // Changing them while autostart is off just records the choice for when it is switched on.
        Field::StartInTray => {
            state.start_in_tray = !state.start_in_tray;
            reregister(
                state,
                if state.start_in_tray {
                    "the login task will start in the tray with no console window"
                } else {
                    "the login task will start with a console window"
                },
            )
        }
        Field::OnPath => {
            let directory = state
                .install_dir
                .clone()
                .context("there is no install directory on this platform")?;
            if state.on_path == Some(true) {
                let changed = install::remove_from_path(&directory)?;
                state.on_path = Some(false);
                Ok(if changed {
                    format!("{} removed from PATH", directory.display())
                } else {
                    "it was not on PATH".into()
                })
            } else {
                let changed = install::add_to_path(&directory)?;
                state.on_path = Some(true);
                Ok(if changed {
                    format!(
                        "{} added to PATH — open a new terminal for it to take effect",
                        directory.display()
                    )
                } else {
                    "it was already on PATH".into()
                })
            }
        }
        Field::ProbesEnabled => {
            state.draft.probes_enabled = !state.draft.probes_enabled;
            save(state, "controlled probes")
        }
        Field::ProbeNetwork => {
            state.draft.probe_network = !state.draft.probe_network;
            save(state, "the probe's network request")
        }
        Field::SessionsEnabled => {
            state.draft.sessions_enabled = !state.draft.sessions_enabled;
            save(state, "transcript reading")
        }
        Field::ServerEnabled => {
            state.draft.server_enabled = !state.draft.server_enabled;
            save(state, "the web dashboard")
        }
        _ => bail!("{} is not a toggle", field.label()),
    }
}

/// Parse and store an edited value.
///
/// Rejecting rather than correcting an unreadable entry. The floors are applied on save and reported, but a
/// value that cannot be parsed at all is a typo, and silently substituting a default for a typo is how a
/// user ends up believing they set something they did not.
pub fn commit(state: &mut State, field: Field, text: &str) -> Result<String> {
    if let Some(reason) = state.unavailable(field) {
        bail!("{reason}");
    }
    let text = text.trim();
    match field {
        Field::LoginDelay => {
            state.login_delay = config::parse_duration(text)?;
            reregister(
                state,
                "the login delay was recorded for the next time autostart is switched on",
            )
        }
        Field::SampleInterval => {
            state.draft.sample_interval = config::parse_duration(text)?;
            save(state, "the sampling interval")
        }
        Field::SampleIntervalIdle => {
            state.draft.sample_interval_idle = config::parse_duration(text)?;
            save(state, "the idle sampling interval")
        }
        Field::ProbeInterval => {
            state.draft.probe_interval = config::parse_duration(text)?;
            save(state, "the probe interval")
        }
        Field::SamplesRawDays => {
            state.draft.samples_raw_days = days(text)?;
            save(state, "raw sample retention")
        }
        Field::BaselineWindowDays => {
            state.draft.baseline_window_days = days(text)?;
            save(state, "the baseline window")
        }
        Field::ServerPort => {
            state.draft.port = text
                .parse::<u16>()
                .with_context(|| format!("{text:?} is not a port number"))?;
            if state.draft.port == 0 {
                bail!(
                    "port 0 asks the operating system to choose, which the dashboard cannot publish"
                );
            }
            save(state, "the dashboard port")
        }
        _ => bail!("{} has no value to edit", field.label()),
    }
}

/// Do what an action row says.
pub fn act(state: &mut State, field: Field) -> Result<String> {
    if let Some(reason) = state.unavailable(field) {
        bail!("{reason}");
    }
    match field {
        Field::InstallHere => {
            let installed = install::install()?;
            // Re-read rather than assume: the copy may have landed somewhere the next `origin()` call
            // classifies differently, and every startup row depends on that answer.
            state.origin = install::origin().ok();
            Ok(format!("installed to {}", installed.display()))
        }
        Field::OpenDashboard => {
            let url = format!("http://127.0.0.1:{}/", state.draft.port);
            install::open(&url)?;
            Ok(format!("opened {url}"))
        }
        Field::RunBenchmark | Field::RunBenchmarkElevated => {
            let program = std::env::current_exe().context("locate the running executable")?;
            let arguments = format!("bench --preset {BENCHMARK_PRESET}");
            if field == Field::RunBenchmarkElevated {
                install::run_elevated(&program, &arguments)?;
                Ok("the elevated benchmark is running in its own window".into())
            } else {
                install::run_detached(&program, &arguments)?;
                Ok("the benchmark is running in its own window".into())
            }
        }
        _ => bail!("{} is not an action", field.label()),
    }
}

/// Save the collection settings and report what was actually written.
///
/// The comparison matters. Intervals are clamped on the way to disk, so a value the user typed can differ
/// from the value now in force — and a screen that reported success while showing a different number than
/// was requested would look like it had ignored them.
fn save(state: &mut State, what: &str) -> Result<String> {
    let requested = state.draft.clone();
    state.save_draft()?;
    if state.draft == requested {
        Ok(format!("{what} saved"))
    } else {
        Ok(format!(
            "{what} saved, adjusted to {}",
            describe_differences(&requested, &state.draft)
        ))
    }
}

/// Name the fields a save changed, so a clamp is visible rather than mysterious.
fn describe_differences(requested: &Draft, written: &Draft) -> String {
    let mut changes = Vec::new();
    if requested.sample_interval != written.sample_interval {
        changes.push(format!(
            "sample interval {}",
            config::duration_text(written.sample_interval)
        ));
    }
    if requested.sample_interval_idle != written.sample_interval_idle {
        changes.push(format!(
            "idle interval {}",
            config::duration_text(written.sample_interval_idle)
        ));
    }
    if requested.probe_interval != written.probe_interval {
        changes.push(format!(
            "probe interval {}",
            config::duration_text(written.probe_interval)
        ));
    }
    if requested.baseline_window_days != written.baseline_window_days {
        changes.push(format!("baseline window {}d", written.baseline_window_days));
    }
    if requested.samples_raw_days != written.samples_raw_days {
        changes.push(format!("retention {}d", written.samples_raw_days));
    }
    if changes.is_empty() {
        "the nearest permitted values".into()
    } else {
        changes.join(", ")
    }
}

/// Re-register the login task if one exists, so a changed delay or tray choice takes effect.
fn reregister(state: &mut State, message: &str) -> Result<String> {
    let enabled = state
        .autostart
        .as_ref()
        .map(|current| current.is_enabled())
        .unwrap_or(false);
    if !enabled {
        return Ok(message.to_string());
    }
    let desired = state
        .desired_autostart()
        .context("there is no durable executable to point the task at")?;
    install::enable_autostart(&desired)?;
    state.autostart = install::autostart_state().map_err(|error| format!("{error:#}"));
    Ok(message.to_string())
}

/// A whole number of days, with or without a trailing `d`.
fn days(text: &str) -> Result<u32> {
    let trimmed = text.trim().trim_end_matches(['d', 'D']);
    let value: u32 = trimmed
        .parse()
        .with_context(|| format!("{text:?} is not a number of days"))?;
    if value == 0 {
        bail!("a window of zero days compares today against nothing");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn days_accept_a_bare_number_or_a_suffix() {
        assert_eq!(days("7").unwrap(), 7);
        assert_eq!(days("14d").unwrap(), 14);
        assert_eq!(days(" 21D ").unwrap(), 21);
    }

    #[test]
    fn days_reject_nonsense_and_zero() {
        for text in ["", "d", "seven", "-3", "0", "0d"] {
            assert!(days(text).is_err(), "{text:?} should be refused");
        }
    }

    fn draft() -> Draft {
        Draft {
            server_enabled: true,
            port: 7878,
            sample_interval: Duration::from_secs(5),
            sample_interval_idle: Duration::from_secs(30),
            probes_enabled: true,
            probe_network: true,
            probe_interval: Duration::from_secs(900),
            sessions_enabled: true,
            samples_raw_days: 14,
            baseline_window_days: 7,
        }
    }

    #[test]
    fn identical_drafts_describe_no_differences() {
        assert_eq!(
            describe_differences(&draft(), &draft()),
            "the nearest permitted values"
        );
    }

    /// The message a user sees after typing an interval below the floor.
    #[test]
    fn a_clamped_interval_is_named_in_the_message() {
        let requested = Draft {
            sample_interval: Duration::from_millis(1),
            ..draft()
        };
        let written = Draft {
            sample_interval: Duration::from_secs(1),
            ..draft()
        };
        assert_eq!(
            describe_differences(&requested, &written),
            "sample interval 1s"
        );
    }

    #[test]
    fn several_clamped_fields_are_all_named() {
        let requested = Draft {
            probe_interval: Duration::from_millis(1),
            baseline_window_days: 0,
            ..draft()
        };
        let written = Draft {
            probe_interval: Duration::from_secs(1),
            baseline_window_days: 1,
            ..draft()
        };
        let description = describe_differences(&requested, &written);
        assert!(description.contains("probe interval 1s"), "{description}");
        assert!(description.contains("baseline window 1d"), "{description}");
    }
}
