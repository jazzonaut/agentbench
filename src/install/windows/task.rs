//! The logon task, driven through `schtasks.exe`.
//!
//! Shelling out rather than binding the Task Scheduler 2.0 COM API. The COM route means initialising COM,
//! four interfaces and a registration call to do what one command line does, and this crate already spawns
//! `powershell` for Defender status — so `schtasks` is the established shape rather than a new one.
//!
//! The task is registered with `/RL LIMITED`: unelevated. Elevation would buy the daemon nothing, since it
//! collects the same data either way, and would turn any fault in its loopback HTTP server into an
//! elevation-of-privilege one. It also means registration needs no administrator rights, so switching this
//! on never produces a UAC prompt — which matters because Windows refuses to show one at logon anyway.

use crate::install::{
    Autostart, AutostartState, Support,
    taskxml::{delay_argument, element, parse_delay},
};
use anyhow::{Context, Result, bail};
use std::{path::PathBuf, process::Command, time::Duration};

/// Name of the registered task.
const TASK_NAME: &str = crate::install::TASK_NAME;

/// Suffix identifying the windowless build that shows a tray icon.
const TRAY_SUFFIX: &str = "-tray";

/// Subcommand the console build is launched with.
const DASHBOARD_ARGUMENT: &str = "dashboard";

pub(crate) fn autostart_support() -> Support {
    Support::Yes
}

pub(crate) fn autostart_state() -> Result<AutostartState> {
    let output = Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME, "/XML", "ONE"])
        .output()
        .context("run schtasks to read the logon task")?;
    if !output.status.success() {
        // A missing task is the ordinary case, not a failure: it is what "autostart is off" looks like.
        // Any other non-zero exit is worth surfacing, because it means the answer is unknown rather than
        // negative — and reporting "off" for "cannot tell" would invite the user to switch on a task that
        // already exists.
        let message = decode(&output.stderr);
        if is_missing_task(&message) {
            return Ok(AutostartState::Absent);
        }
        bail!("schtasks could not read the logon task: {}", message.trim());
    }
    let xml = decode(&output.stdout);
    let Some(exec) = element(&xml, "Exec") else {
        bail!("the registered task has no Exec block, so it was not created by this program");
    };
    let Some(command) = element(exec, "Command") else {
        bail!("the registered task has no Command, so what it starts cannot be determined");
    };
    let program = PathBuf::from(command.trim());
    let tray = program
        .file_stem()
        .is_some_and(|stem| stem.to_string_lossy().ends_with(TRAY_SUFFIX));
    // A task with no `<Delay>` starts immediately, which is a real configuration rather than an error.
    let delay = element(&xml, "LogonTrigger")
        .and_then(|trigger| element(trigger, "Delay"))
        .and_then(parse_delay)
        .unwrap_or(Duration::ZERO);
    Ok(AutostartState::Present(Autostart {
        program,
        tray,
        delay,
    }))
}

pub(crate) fn enable_autostart(autostart: &Autostart) -> Result<()> {
    if !autostart.program.is_file() {
        bail!(
            "{} does not exist, so a task pointing at it would start nothing",
            autostart.program.display()
        );
    }
    // Quoted so a path containing spaces survives `schtasks`' own parsing of `/TR`. The console build takes
    // a subcommand; the tray build is the daemon and takes none.
    let program = autostart.program.display();
    let run = if autostart.tray {
        format!("\"{program}\"")
    } else {
        format!("\"{program}\" {DASHBOARD_ARGUMENT}")
    };
    let output = Command::new("schtasks")
        .args([
            "/Create",
            "/TN",
            TASK_NAME,
            "/TR",
            &run,
            "/SC",
            "ONLOGON",
            // Unelevated, on purpose. See the module documentation.
            "/RL",
            "LIMITED",
            "/DELAY",
            &delay_argument(autostart.delay),
            // Replace rather than fail, so saving the same screen twice is not an error.
            "/F",
        ])
        .output()
        .context("run schtasks to register the logon task")?;
    if !output.status.success() {
        bail!(
            "schtasks could not register the logon task: {}",
            decode(&output.stderr).trim()
        );
    }
    Ok(())
}

pub(crate) fn disable_autostart() -> Result<bool> {
    let output = Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .output()
        .context("run schtasks to remove the logon task")?;
    if output.status.success() {
        return Ok(true);
    }
    let message = decode(&output.stderr);
    if is_missing_task(&message) {
        return Ok(false);
    }
    bail!(
        "schtasks could not remove the logon task: {}",
        message.trim()
    )
}

/// Whether a `schtasks` failure means "no such task" rather than something worth reporting.
///
/// Matched on the error code rather than the sentence, because the sentence is localised and this has to
/// work on a machine whose Windows is not in English.
fn is_missing_task(message: &str) -> bool {
    message.contains("ERROR_FILE_NOT_FOUND")
        || message.contains("0x80070002")
        || message.contains("cannot find the file specified")
}

/// Decode `schtasks` output, which is UTF-16 for `/XML` and the console code page otherwise.
///
/// `/XML` prints UTF-16LE with a byte-order mark — feed that to a UTF-8 decoder and every second byte is a
/// NUL, so the document parses as nothing at all and the task reads as unrecognised. Detected by the mark
/// rather than by which flag was passed, so one decoder serves every call.
fn decode(bytes: &[u8]) -> String {
    if let [0xFF, 0xFE, rest @ ..] = bytes {
        let units = rest
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decoding bug this function exists to avoid, reproduced: `/XML` output is UTF-16LE with a mark.
    #[test]
    fn utf16_output_with_a_byte_order_mark_decodes() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "<Command>x.exe</Command>".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode(&bytes), "<Command>x.exe</Command>");
    }

    #[test]
    fn plain_output_without_a_mark_decodes_as_utf8() {
        assert_eq!(decode(b"SUCCESS: task created"), "SUCCESS: task created");
    }

    /// An odd trailing byte must not panic; truncated output is possible from a killed process.
    #[test]
    fn truncated_utf16_output_does_not_panic() {
        decode(&[0xFF, 0xFE, 0x3C]);
    }

    #[test]
    fn a_missing_task_is_recognised_by_code_not_by_wording() {
        assert!(is_missing_task(
            "ERROR: The system cannot find the file specified."
        ));
        assert!(is_missing_task("ERROR_FILE_NOT_FOUND"));
        assert!(is_missing_task("Error code 0x80070002"));
        assert!(!is_missing_task("ERROR: Access is denied."));
    }

    /// Reading is safe to run for real: it queries a task and writes nothing. Either answer is valid — the
    /// machine running the tests may or may not have autostart enabled — so the assertion is that asking
    /// works and never reports "unsupported" on the platform that supports it.
    #[test]
    fn the_logon_task_can_be_queried() {
        let state = autostart_state().expect("querying the logon task should work on Windows");
        assert!(
            !matches!(state, AutostartState::Unsupported(_)),
            "Windows supports this, so it must not report otherwise"
        );
    }

    /// A task pointing at a path that does not exist would start nothing, silently, at every login.
    #[test]
    fn registering_a_task_for_a_missing_program_is_refused() {
        let error = enable_autostart(&Autostart {
            program: PathBuf::from(r"C:\no\such\agentbench.exe"),
            tray: false,
            delay: Duration::ZERO,
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("does not exist"), "{error}");
    }
}
