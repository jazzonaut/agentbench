//! Where probes do their filesystem work.
//!
//! This directory is not an implementation detail. A probe that writes to `%TEMP%` while the code the
//! user cares about lives on a different volume measures the wrong disk, and does so silently: the
//! series looks healthy, the machine is not, and nothing in the data says which drive was observed.

use crate::watch::config::CollectConfig;
use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Directory name created inside the resolved parent.
const SCRATCH_DIR: &str = "probe-scratch";

/// A scratch directory that empties itself between runs.
///
/// Held once prepared rather than re-created per probe. Preparing it can legitimately fail — a removable
/// volume, a full disk, a directory someone deleted — so the prober treats it as something to keep trying
/// for rather than a startup precondition.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Where probes will write, whether or not it exists yet.
    ///
    /// Defaults to the data directory, which is the honest default rather than a convenient one: the
    /// database is already there, so the volume is one the user chose. `scratch_dir` overrides it for
    /// the common case where the data directory is on a system drive and the work is not.
    ///
    /// Separate from [`Scratch::prepare`] so the daemon can name the location in its startup log without
    /// creating anything, which matters because preparing happens on the probe cadence.
    pub fn location(config: &CollectConfig, data_dir: &Path) -> PathBuf {
        config
            .scratch_dir
            .clone()
            .unwrap_or_else(|| data_dir.into())
            .join(SCRATCH_DIR)
    }

    /// Create the directory, empty.
    pub fn prepare(config: &CollectConfig, data_dir: &Path) -> Result<Self> {
        let path = Self::location(config, data_dir);
        // A previous daemon killed mid-probe leaves files behind. Clearing on startup rather than
        // trusting each workload to clean up keeps the small-file pass measuring an empty directory,
        // which is what it did the first time and therefore what today's number is comparable to.
        if path.exists() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("clear scratch directory {}", path.display()))?;
        }
        fs::create_dir_all(&path)
            .with_context(|| format!("create scratch directory {}", path.display()))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove anything a failed probe left behind.
    ///
    /// Called after every probe, successful or not. A workload that fails part way through has no
    /// reason to have tidied up, and the next probe's small-file measurement would otherwise start
    /// against a directory holding a few hundred stale entries.
    ///
    /// Returns what could not be removed, for the caller to log. Reporting rather than failing: a
    /// locked leftover file is a reason to say so, not a reason to stop collecting.
    pub fn tidy(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let entries = match fs::read_dir(&self.path) {
            Ok(entries) => entries,
            Err(error) => return vec![format!("cannot read {}: {error}", self.path.display())],
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let removed = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            if let Err(error) = removed {
                problems.push(format!("cannot remove {}: {error}", path.display()));
            }
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn config(scratch_dir: Option<PathBuf>) -> CollectConfig {
        CollectConfig {
            sample_interval: Duration::from_secs(5),
            sample_interval_idle: Duration::from_secs(30),
            idle_cpu_percent: 10.0,
            discovery_interval: Duration::from_secs(60),
            agent_process_names: vec![],
            scanner_process_names: vec![],
            probes_enabled: true,
            probe_network: false,
            probe_interval: Duration::from_secs(900),
            scratch_dir,
        }
    }

    #[test]
    fn the_default_location_is_inside_the_data_directory() {
        let temp = tempfile::tempdir().unwrap();
        let expected = temp.path().join(SCRATCH_DIR);
        // Naming the location must not create it: the daemon logs the path at startup, long before the
        // first probe prepares anything.
        assert_eq!(Scratch::location(&config(None), temp.path()), expected);
        assert!(!expected.exists());

        let scratch = Scratch::prepare(&config(None), temp.path()).unwrap();
        assert_eq!(scratch.path(), expected);
        assert!(scratch.path().is_dir());
    }

    #[test]
    fn an_explicit_location_wins_and_is_created() {
        let temp = tempfile::tempdir().unwrap();
        let elsewhere = temp.path().join("other").join("volume");
        let data_dir = temp.path().join("data");
        let scratch = Scratch::prepare(&config(Some(elsewhere.clone())), &data_dir).unwrap();
        assert_eq!(scratch.path(), elsewhere.join(SCRATCH_DIR));
        assert!(scratch.path().is_dir());
        assert!(!data_dir.exists(), "the data directory must not be touched");
    }

    /// The reason startup clears rather than trusting the previous run: a killed daemon leaves files,
    /// and a small-file measurement against a dirty directory is not comparable to one against a clean.
    #[test]
    fn startup_clears_what_a_killed_daemon_left_behind() {
        let temp = tempfile::tempdir().unwrap();
        let first = Scratch::prepare(&config(None), temp.path()).unwrap();
        fs::write(first.path().join("sequential.bin"), b"leftover").unwrap();
        fs::create_dir(first.path().join("small-files")).unwrap();

        let second = Scratch::prepare(&config(None), temp.path()).unwrap();
        assert_eq!(
            fs::read_dir(second.path()).unwrap().count(),
            0,
            "the scratch directory should start empty"
        );
    }

    #[test]
    fn tidying_removes_files_and_directories_without_removing_the_scratch_itself() {
        let temp = tempfile::tempdir().unwrap();
        let scratch = Scratch::prepare(&config(None), temp.path()).unwrap();
        fs::write(scratch.path().join("stray.bin"), b"x").unwrap();
        let nested = scratch.path().join("small-files");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("f-0.dat"), b"x").unwrap();

        assert!(scratch.tidy().is_empty());
        assert!(scratch.path().is_dir(), "the directory itself must survive");
        assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
    }

    #[test]
    fn tidying_a_directory_that_has_vanished_reports_rather_than_panics() {
        let temp = tempfile::tempdir().unwrap();
        let scratch = Scratch::prepare(&config(None), temp.path()).unwrap();
        fs::remove_dir_all(scratch.path()).unwrap();
        let problems = scratch.tidy();
        assert_eq!(problems.len(), 1, "{problems:?}");
    }
}
