//! The user's `PATH`, read and written through `HKCU\Environment`.
//!
//! Deliberately not `setx`, and deliberately not .NET's `SetEnvironmentVariable`. `setx` truncates the
//! value it writes at 1024 characters, which on a developer machine silently destroys most of a `PATH`.
//! The .NET call cannot be relied upon to preserve the value's registry type, and a `PATH` that was
//! `REG_EXPAND_SZ` and comes back `REG_SZ` stops expanding every `%USERPROFILE%` in it — also silently,
//! also irreversibly. Reading the existing type and writing the same one back is the only version of this
//! that cannot quietly break a working machine, so it is worth the FFI.

use crate::install::Support;
use anyhow::{Context, Result, bail};
use std::{ffi::OsStr, iter::once, os::windows::ffi::OsStrExt, ptr};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
    System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ, RegCloseKey,
        RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    },
    UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    },
};

/// Registry key holding the user's environment.
const ENVIRONMENT_KEY: &str = "Environment";

/// Value within it.
const PATH_VALUE: &str = "Path";

/// How long to wait for each window to acknowledge the environment change, in milliseconds.
///
/// A broadcast is best-effort by nature: one hung application must not hold up a settings change, which is
/// what `SMTO_ABORTIFHUNG` and a short timeout together guarantee.
const BROADCAST_TIMEOUT_MS: u32 = 1_000;

pub(crate) fn path_support() -> Support {
    Support::Yes
}

/// A registry key that closes itself.
struct Key(HKEY);

impl Key {
    /// Open `HKCU\Environment` with the given access, or report that it is missing.
    fn open(access: u32) -> Result<Option<Self>> {
        let name = wide(ENVIRONMENT_KEY);
        let mut key: HKEY = ptr::null_mut();
        // SAFETY: `name` is a NUL-terminated wide string that outlives the call, and `key` is a valid
        // out-pointer the function writes only on success.
        let status =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, name.as_ptr(), 0, access, &raw mut key) };
        match status {
            ERROR_SUCCESS => Ok(Some(Self(key))),
            ERROR_FILE_NOT_FOUND => Ok(None),
            other => Err(std::io::Error::from_raw_os_error(other as i32))
                .context("open HKCU\\Environment"),
        }
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful `RegOpenKeyExW` and is closed exactly once.
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

/// The current `Path` value and its registry type, if the value exists.
fn read_value() -> Result<Option<(String, u32)>> {
    let Some(key) = Key::open(KEY_READ)? else {
        return Ok(None);
    };
    let name = wide(PATH_VALUE);
    let mut kind: u32 = 0;
    let mut bytes: u32 = 0;
    // First call sizes the buffer. Asking twice rather than guessing: a developer's `PATH` routinely runs
    // to several kilobytes, and a fixed buffer that was almost always big enough would fail only on the
    // machines where getting this wrong matters most.
    // SAFETY: null data pointer with a valid size out-pointer is the documented way to query the size.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            ptr::null_mut(),
            &raw mut kind,
            ptr::null_mut(),
            &raw mut bytes,
        )
    };
    match status {
        ERROR_SUCCESS => {}
        ERROR_FILE_NOT_FOUND => return Ok(None),
        other => {
            return Err(std::io::Error::from_raw_os_error(other as i32))
                .context("read the size of the user's PATH");
        }
    }
    // Rounded up so an odd byte count cannot truncate the last unit.
    let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
    let mut bytes_out = bytes;
    // SAFETY: `buffer` has `bytes_out` bytes of capacity, which is what the size query reported.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            ptr::null_mut(),
            &raw mut kind,
            buffer.as_mut_ptr().cast(),
            &raw mut bytes_out,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32))
            .context("read the user's PATH");
    }
    let units = (bytes_out as usize) / 2;
    buffer.truncate(units.min(buffer.len()));
    // The stored value is NUL-terminated; the terminator is not part of it.
    while buffer.last() == Some(&0) {
        buffer.pop();
    }
    Ok(Some((String::from_utf16_lossy(&buffer), kind)))
}

pub(crate) fn read_user_path() -> Result<String> {
    Ok(read_value()?.map(|(value, _)| value).unwrap_or_default())
}

pub(crate) fn write_user_path(value: &str) -> Result<()> {
    let existing = read_value()?;
    // A refusal rather than a write. Every edit this module makes adds or removes one directory, so an
    // empty result from a non-empty starting point is a bug in the caller — and the consequence of writing
    // it is a machine with no usable `PATH` until the user rebuilds it by hand. Cheap guard, unrecoverable
    // failure avoided.
    if value.trim().is_empty()
        && existing
            .as_ref()
            .is_some_and(|(current, _)| !current.trim().is_empty())
    {
        bail!("refusing to write an empty PATH over a non-empty one");
    }
    // Keep whatever type is already there. Only when creating the value from nothing is there a choice to
    // make, and then it follows the content: a value containing `%` is meant to be expanded.
    let kind = existing.map(|(_, kind)| kind).unwrap_or({
        if value.contains('%') {
            REG_EXPAND_SZ
        } else {
            REG_SZ
        }
    });
    let Some(key) = Key::open(KEY_SET_VALUE)? else {
        bail!("HKCU\\Environment does not exist, so the user's PATH cannot be written");
    };
    let name = wide(PATH_VALUE);
    let data = wide(value);
    let bytes = u32::try_from(std::mem::size_of_val(data.as_slice()))
        .context("the new PATH is too long to write")?;
    // SAFETY: `data` is a NUL-terminated wide string and `bytes` is its exact size in bytes, terminator
    // included, which is what `RegSetValueExW` expects for a string type.
    let status =
        unsafe { RegSetValueExW(key.0, name.as_ptr(), 0, kind, data.as_ptr().cast(), bytes) };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32))
            .context("write the user's PATH");
    }
    drop(key);
    broadcast_environment_change();
    Ok(())
}

/// Tell running applications the environment changed.
///
/// Without this the new `PATH` is in the registry but no shell learns about it until the next login, which
/// makes "add to PATH" look broken. Explorer picks the change up and hands it to the shells it launches
/// afterwards; shells that are *already* open never see it, and no broadcast can change that.
fn broadcast_environment_change() {
    let parameter = wide("Environment");
    // SAFETY: a broadcast with a NUL-terminated wide string parameter that outlives the call. The result is
    // ignored on purpose — see below.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            parameter.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            BROADCAST_TIMEOUT_MS,
            ptr::null_mut(),
        );
    }
    // Deliberately not checked. The registry write already succeeded, which is the part that persists; a
    // failed or ignored broadcast costs the user a new terminal window, not their setting. Reporting it as
    // an error would mean a successful change looked like a failed one.
}

/// A NUL-terminated UTF-16 string, as every `W` function expects.
fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-tripping through the encoder is the only part of this that can be checked without writing to
    /// the registry of the machine running the tests.
    #[test]
    fn wide_strings_are_nul_terminated_and_decode_back() {
        let encoded = wide(r"C:\Program Files\AgentBench");
        assert_eq!(encoded.last(), Some(&0));
        let decoded = String::from_utf16_lossy(&encoded[..encoded.len() - 1]);
        assert_eq!(decoded, r"C:\Program Files\AgentBench");
    }

    #[test]
    fn a_path_with_a_variable_in_it_encodes_unchanged() {
        let value = r"%USERPROFILE%\bin;C:\Windows";
        let encoded = wide(value);
        assert_eq!(
            String::from_utf16_lossy(&encoded[..encoded.len() - 1]),
            value
        );
    }

    /// Reading is safe to exercise for real: it opens a well-known key read-only and writes nothing.
    #[test]
    fn the_users_path_can_be_read() {
        let path = read_user_path().expect("HKCU\\Environment should be readable");
        // An empty user `PATH` is unusual but legal — a fresh account has none — so the assertion is only
        // that the call works and returns something sane, not that it is populated.
        assert!(!path.contains('\0'), "the terminator should be stripped");
    }
}
