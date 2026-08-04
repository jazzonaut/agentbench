//! The logon task, driven through `schtasks.exe`.
//!
//! Shelling out rather than binding the Task Scheduler 2.0 COM API. The COM route means initialising COM,
//! four interfaces and a registration call to do what one command line does, and this crate already spawns
//! `powershell` for Defender status — so `schtasks` is the established shape rather than a new one.
//!
//! The task runs unelevated, at `LeastPrivilege`. Elevation would buy the daemon nothing, since it collects
//! the same data either way, and would turn any fault in its loopback HTTP server into an
//! elevation-of-privilege one. It also means registration needs no administrator rights, so switching this
//! on never produces a UAC prompt — which matters because Windows refuses to show one at logon anyway.
//!
//! Registration goes through `/Create /XML` rather than a `/Create /SC ONLOGON` command line, and that is
//! the load-bearing detail: `schtasks` cannot scope a logon trigger to a user, so `/SC ONLOGON` registers
//! one that fires at *any* user's logon — an administrator-only operation that fails unelevated and, when it
//! does succeed from an elevated session, leaves behind a task the unelevated program can read but not
//! remove. [`super::super::taskxml::document`] carries the rest of that reasoning.

use super::launch::run_elevated;
use crate::install::{
    Autostart, AutostartState, Support,
    taskxml::{document, element, parse_delay},
};
use anyhow::{Context, Result, bail};
use std::{
    env,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use tempfile::{Builder, TempPath};

/// Name of the registered task.
const TASK_NAME: &str = crate::install::TASK_NAME;

/// Subcommand the console build is launched with.
const DASHBOARD_ARGUMENT: &str = "dashboard";

/// How long to watch for an elevated `schtasks` to finish before reporting only what is known.
///
/// `ShellExecuteW` returns when the prompt is answered, not when the program it launched has finished, so
/// the outcome has to be observed. Ten seconds is far longer than a task deletion takes and short enough
/// that a screen waiting on it has not appeared to hang.
const ELEVATED_TIMEOUT: Duration = Duration::from_secs(10);

/// Gap between checks while waiting for that.
const ELEVATED_POLL: Duration = Duration::from_millis(250);

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
    // Unquoted: `schtasks` keeps whatever quoting the definition used inside `<Command>`, and a path with
    // literal quotation marks around it is not a path — it compares unequal to the one on the screen and
    // `is_file` says it does not exist.
    let program = PathBuf::from(command.trim().trim_matches('"'));
    let tray = crate::install::is_tray_build(&program);
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
    // Refused rather than attempted. Registering from an elevated session works, and produces a task whose
    // security descriptor grants Administrators full control and this account only read access — so the row
    // would report success and then never be able to switch itself off again. Better to say so now than to
    // leave behind a task only an administrator can remove.
    if crate::install::is_elevated() {
        bail!(
            "this is running elevated, and a logon task registered from an elevated session can only be \
             removed from one. Start the control centre without elevation to switch this on — the task \
             needs no administrator rights."
        );
    }
    // The console build takes a subcommand; the tray build is the daemon and takes none.
    let arguments = (!autostart.tray).then_some(DASHBOARD_ARGUMENT);
    let xml = document(
        &autostart.program.to_string_lossy(),
        arguments,
        &current_user()?,
        autostart.delay,
    );
    let definition = write_definition(&xml)?;
    let output = Command::new("schtasks")
        .args(["/Create", "/TN", TASK_NAME, "/XML"])
        .arg(&*definition)
        // Replace rather than fail, so saving the same screen twice is not an error.
        .arg("/F")
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
    let Err(message) = delete() else {
        return Ok(true);
    };
    // Why the task is still there matters more than what `schtasks` printed, and the printed reason is
    // localised — so the state of the world is asked for instead. A task that is gone was either removed by
    // this call or never there; one that is still registered after a `/Delete` that failed is one this
    // account may read but not change, which is what a task registered from an elevated session looks like.
    match autostart_state() {
        Ok(AutostartState::Absent) => Ok(false),
        _ if crate::install::is_elevated() => {
            bail!("schtasks could not remove the logon task, even elevated: {message}")
        }
        _ => {
            remove_elevated(&message)?;
            Ok(true)
        }
    }
}

/// Ask `schtasks` to remove the task, reporting its complaint when it will not.
fn delete() -> Result<(), String> {
    let output = Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .output()
        .map_err(|error| format!("schtasks could not be run: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(decode(&output.stderr).trim().to_string())
}

/// Remove a task this account cannot change, with one elevation prompt.
///
/// The situation is a leftover: versions of this program before the `/XML` registration used
/// `/Create /SC ONLOGON`, which needed an elevated session to succeed at all, and the task it left behind
/// belongs to Administrators. New tasks are registered unelevated and removable without any of this.
fn remove_elevated(message: &str) -> Result<()> {
    run_elevated(
        Path::new("schtasks.exe"),
        &format!("/Delete /TN \"{TASK_NAME}\" /F"),
    )
    .with_context(|| {
        format!(
            "the logon task belongs to Administrators, so removing it needs elevation ({message})"
        )
    })?;
    let deadline = Instant::now() + ELEVATED_TIMEOUT;
    loop {
        if matches!(autostart_state(), Ok(AutostartState::Absent)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "an elevated schtasks was asked to remove the logon task and it is still registered; \
                 reopen this screen to see whether it went"
            );
        }
        std::thread::sleep(ELEVATED_POLL);
    }
}

/// Write a task definition where `schtasks /Create /XML` can read it.
///
/// UTF-16LE with a byte-order mark, which is not a preference: `/XML` rejects a UTF-8 document outright, and
/// reports doing so as `The system cannot find the file specified` — about a file it has just opened. The
/// exact inverse of [`decode`], which exists because `/Query /XML` prints the same encoding.
///
/// Returns a path rather than the open file, and that is deliberate. `schtasks` opens the definition for
/// exclusive access and refuses one this process still holds open — "the process cannot access the file
/// because it is being used by another process", which reads as a busy disk rather than as our own handle.
/// [`TempPath`] keeps the file on disk with nothing open on it, and removes it when the caller is done.
fn write_definition(xml: &str) -> Result<TempPath> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    // The `.xml` suffix is for whoever finds one of these after a crash; `schtasks` does not care.
    let mut file = Builder::new()
        .prefix("agentbench-task-")
        .suffix(".xml")
        .tempfile()
        .context("create a temporary file for the task definition")?;
    file.write_all(&bytes)
        .and_then(|()| file.flush())
        .context("write the task definition")?;
    Ok(file.into_temp_path())
}

/// The account the logon trigger belongs to, as `DOMAIN\user`.
///
/// From the environment rather than `GetUserNameExW`, because Windows sets both of these in every
/// interactive session and this screen is only reachable from one. A name that does not resolve is not a
/// silent failure either: `schtasks` refuses the registration and the user is told, rather than being given
/// a task that belongs to nobody.
fn current_user() -> Result<String> {
    let name = env::var("USERNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .context("USERNAME is not set, so the account to register the task for cannot be named")?;
    match env::var("USERDOMAIN") {
        Ok(domain) if !domain.trim().is_empty() => Ok(format!("{domain}\\{name}")),
        _ => Ok(name),
    }
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

    /// `/Create /XML` rejects a UTF-8 document, so what is written has to be what `decode` reads.
    #[test]
    fn a_written_definition_is_utf16_with_a_byte_order_mark() {
        let xml = "<Task><Command>C:\\x.exe</Command></Task>";
        let definition = write_definition(xml).expect("a temporary file");
        let bytes = std::fs::read(&definition).expect("read it back");
        assert_eq!(&bytes[..2], &[0xFF, 0xFE], "no byte-order mark");
        assert_eq!(decode(&bytes), xml);
    }

    /// The account name has to be one Task Scheduler can resolve, which is the interactive session's own.
    #[test]
    fn the_current_user_is_named_as_domain_and_account() {
        let user = current_user().expect("an interactive session names its user");
        assert!(!user.trim().is_empty());
        // Whatever the shape, it has to be the name this session logs on under, so the scheduler resolves it.
        let expected = match (env::var("USERDOMAIN"), env::var("USERNAME")) {
            (Ok(domain), Ok(name)) if !domain.trim().is_empty() => format!("{domain}\\{name}"),
            (_, Ok(name)) => name,
            _ => unreachable!("current_user succeeded, so USERNAME is set"),
        };
        assert_eq!(user, expected);
    }

    /// A quoted `<Command>` is what earlier versions registered, and the path inside it is the answer.
    #[test]
    fn a_quoted_command_reads_as_a_bare_path() {
        let xml = r#"<Exec><Command>"C:\Programs\AgentBench\agentbench-tray.exe"</Command></Exec>"#;
        let command = element(xml, "Command").expect("a Command");
        let program = PathBuf::from(command.trim().trim_matches('"'));
        assert_eq!(
            program,
            PathBuf::from(r"C:\Programs\AgentBench\agentbench-tray.exe")
        );
        assert!(crate::install::is_tray_build(&program));
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

    /// The regression this module was rewritten for, checked against the real Task Scheduler.
    ///
    /// A `/Create /SC ONLOGON` command line fails here with "Access is denied", because the trigger it writes
    /// fires at any user's logon. The generated definition names a user and so registers with no elevation at
    /// all — and the account that registered it can remove it again, which the elevated form does not allow.
    ///
    /// Registered under its own name and removed in the same test, so it never collides with the real task
    /// and never survives the run. Skipped when elevated, where the denial being guarded against cannot
    /// happen and the task would come out owned by Administrators.
    #[test]
    fn a_generated_definition_registers_and_removes_without_elevation() {
        if crate::install::is_elevated() {
            return;
        }
        const PROBE: &str = "AgentBench dashboard (self test)";
        let program = std::env::current_exe().expect("the test binary has a path");
        let xml = document(
            &program.to_string_lossy(),
            Some(DASHBOARD_ARGUMENT),
            &current_user().expect("an interactive session"),
            Duration::from_secs(120),
        );
        let definition = write_definition(&xml).expect("a temporary definition");
        let created = Command::new("schtasks")
            .args(["/Create", "/TN", PROBE, "/XML"])
            .arg(&*definition)
            .arg("/F")
            .output()
            .expect("run schtasks");
        let removed = Command::new("schtasks")
            .args(["/Delete", "/TN", PROBE, "/F"])
            .output()
            .expect("run schtasks");
        assert!(
            created.status.success(),
            "registering unelevated failed: {}\n{xml}",
            decode(&created.stderr).trim()
        );
        assert!(
            removed.status.success(),
            "the account that registered the task could not remove it: {}",
            decode(&removed.stderr).trim()
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
