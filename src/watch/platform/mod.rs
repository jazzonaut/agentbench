//! Operating-system facilities the daemon needs and the honest fallbacks when they are missing.
//!
//! Every function here either succeeds or reports that the capability is unavailable. Nothing in this
//! module invents a value or silently pretends to have done something, matching the project's rule
//! that collectors annotate their provenance rather than fabricate zeros.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as imp;

#[cfg(not(any(windows, unix)))]
mod fallback;
#[cfg(not(any(windows, unix)))]
use fallback as imp;

use anyhow::{Context, Result};
use std::{
    env,
    fs::{self, File},
    path::PathBuf,
};

/// Outcome of asking the OS for a capability that may not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capability {
    /// The request was made and the OS accepted it.
    Applied,
    /// The platform offers no equivalent; the reason is recorded for the report's `unavailable` list.
    Unsupported(String),
}

impl Capability {
    pub fn is_applied(&self) -> bool {
        matches!(self, Self::Applied)
    }

    /// Reason this capability could not be applied, if it could not be.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Applied => None,
            Self::Unsupported(reason) => Some(reason),
        }
    }
}

/// Per-user directory for AgentBench state that is not a report.
///
/// Honours an explicit `AGENTBENCH_DATA_DIR` override first so tests and unusual setups never touch
/// the real directory. The directory is created if missing.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(explicit) = env::var_os("AGENTBENCH_DATA_DIR") {
        let path = PathBuf::from(explicit);
        fs::create_dir_all(&path)
            .with_context(|| format!("create data directory {}", path.display()))?;
        return Ok(path);
    }
    let path = imp::default_data_dir()?.join("agentbench");
    fs::create_dir_all(&path)
        .with_context(|| format!("create data directory {}", path.display()))?;
    Ok(path)
}

/// Take an exclusive, non-blocking lock on an open file.
///
/// Returns `Ok(false)` when another process already holds it, which the caller reports as "a daemon
/// is already running" rather than as an error.
pub fn try_lock_exclusive(file: &File) -> Result<bool> {
    imp::try_lock_exclusive(file)
}

/// Drop the calling thread to background CPU *and* I/O priority.
///
/// Applied only to threads whose timing does not matter. Deliberately never applied to the probe
/// thread: a throttled probe measures the throttle, not the machine.
///
/// **One-way on Unix.** Lowering priority needs no privileges; raising it back does. An unprivileged
/// process cannot undo this, so there is deliberately no counterpart function: a measured thread has
/// to be started at normal priority rather than restored to it. A restore that silently failed would
/// be the worst outcome available — every probe on that thread would read slow, consistently, and the
/// dashboard would report a machine getting worse while nothing about the machine had changed.
pub fn set_current_thread_background() -> Capability {
    imp::set_current_thread_background()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_honours_the_override_and_creates_it() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("nested").join("state");
        // SAFETY: single-threaded test scope; the variable is removed before returning.
        unsafe { env::set_var("AGENTBENCH_DATA_DIR", &target) };
        let resolved = data_dir().unwrap();
        unsafe { env::remove_var("AGENTBENCH_DATA_DIR") };
        assert_eq!(resolved, target);
        assert!(target.is_dir());
    }

    #[test]
    fn an_exclusive_lock_is_granted_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("watch.lock");
        let first = File::create(&path).unwrap();
        assert!(
            try_lock_exclusive(&first).unwrap(),
            "first lock should succeed"
        );

        let second = File::options()
            .read(true)
            .write(true)
            .open(&path)
            .expect("reopen for contention test");
        assert!(
            !try_lock_exclusive(&second).unwrap(),
            "a second holder must be refused"
        );
    }

    /// Lowering priority is all that is claimed, and all that is testable.
    ///
    /// An earlier version asserted that a platform able to lower priority could also restore it. On
    /// Unix that is simply untrue — raising a nice value back needs privileges an unprivileged daemon
    /// does not have — and the assertion only ever passed because it had never run on Unix.
    #[test]
    fn background_priority_reports_whether_it_applied() {
        let applied = set_current_thread_background();
        // A refusal has to explain itself: that reason is logged, and it is the only signal a user
        // gets that a collector is competing at normal priority.
        if !applied.is_applied() {
            assert!(applied.reason().is_some_and(|reason| !reason.is_empty()));
        }
    }
}
