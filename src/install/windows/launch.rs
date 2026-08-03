//! Handing something to the shell: a URL for the browser, or this executable for an elevation prompt.

use anyhow::{Context, Result, bail};
use std::{ffi::OsStr, iter::once, os::windows::ffi::OsStrExt, path::Path, ptr};
use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

/// `ShellExecuteW` returns a value above this on success; at or below it, the value is an error code.
///
/// A quirk of a function that predates a sensible error convention: the return is an `HINSTANCE` for
/// historical reasons and small values are error codes cast into it.
const SUCCESS_THRESHOLD: isize = 32;

/// Open a URL or a file with whatever the user has associated with it.
pub(crate) fn open(target: &str) -> Result<()> {
    execute(None, target, None)
}

/// Launch a program with arguments, in its own window.
///
/// Through the shell rather than [`std::process::Command`] because the caller is a full-screen terminal
/// application: a child inheriting this console would draw its output over the screen. The shell gives it a
/// console of its own.
pub(crate) fn run_detached(program: &Path, arguments: &str) -> Result<()> {
    execute(None, &program.to_string_lossy(), Some(arguments))
}

/// Re-launch a program with an elevation prompt.
///
/// The `runas` verb is the only way a desktop application can ask for elevation, and the prompt appears
/// synchronously — the user either accepts it or the call fails with `ERROR_CANCELLED`, which is reported
/// as an ordinary refusal rather than a fault.
pub(crate) fn run_elevated(program: &Path, arguments: &str) -> Result<()> {
    execute(Some("runas"), &program.to_string_lossy(), Some(arguments))
}

fn execute(verb: Option<&str>, file: &str, arguments: Option<&str>) -> Result<()> {
    let verb_wide = verb.map(wide);
    let file_wide = wide(file);
    let arguments_wide = arguments.map(wide);
    // SAFETY: every pointer is either null or into a NUL-terminated wide string that outlives the call, and
    // a null window handle is documented as "no parent window".
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb_wide
                .as_ref()
                .map_or(ptr::null(), |value| value.as_ptr()),
            file_wide.as_ptr(),
            arguments_wide
                .as_ref()
                .map_or(ptr::null(), |value| value.as_ptr()),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if result > SUCCESS_THRESHOLD {
        return Ok(());
    }
    // 1223 is `ERROR_CANCELLED`: the user was shown the prompt and declined. That is an answer, not a
    // malfunction, so it is worth saying plainly instead of as an OS error string.
    if result == 1223 {
        bail!("the elevation prompt was declined");
    }
    Err(std::io::Error::from_raw_os_error(result as i32))
        .with_context(|| format!("ask the shell to open {file}"))
}

/// A NUL-terminated UTF-16 string.
fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_nul_terminated() {
        let encoded = wide("https://127.0.0.1:7878/");
        assert_eq!(encoded.last(), Some(&0));
        assert_eq!(
            String::from_utf16_lossy(&encoded[..encoded.len() - 1]),
            "https://127.0.0.1:7878/"
        );
    }

    /// Opening something that cannot be opened must produce an error rather than appearing to work.
    #[test]
    fn opening_a_nonexistent_file_is_an_error() {
        assert!(open(r"C:\no\such\file.agentbench-nonexistent").is_err());
    }
}
