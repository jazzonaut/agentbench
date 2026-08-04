//! Writing `watch.toml` back, with its comments intact.
//!
//! Separate from [`config`] because reading and editing are different jobs with different failure modes:
//! the loader turns a file into a validated [`WatchConfig`] and never writes, and this turns a set of
//! chosen values into an edit and never validates the whole file. Keeping them apart is also what keeps
//! `toml_edit` out of the read path, which has no use for it.
//!
//! The reason it is `toml_edit` and not `toml`: [`FileConfig`] derives `Serialize`, so re-serialising the
//! whole structure would be a two-line save — and it would delete every comment in the file. That file is
//! the configuration's documentation. It ships commented on purpose, and a control centre that silently
//! stripped it the first time someone changed a port would be trading the thing away for nothing.
//!
//! [`config`]: super::config
//! [`FileConfig`]: super::config
//! [`WatchConfig`]: super::WatchConfig

use super::config::{
    CONFIG_FILE, DEFAULT_CONFIG_TOML, clamp_probe_interval, clamp_sample_intervals, duration_text,
};
use crate::watch::WatchConfig;
use anyhow::{Context, Result, bail};
use std::{fs, path::Path, time::Duration};
use toml_edit::{DocumentMut, Item, Table, Value, value};

/// The values the control centre may change.
///
/// A struct rather than a list of independent edits, because two of the rules span fields: the idle
/// sampling interval is bounded by the active one, and neither may fall below the file's floor. A
/// per-setting API could not express that, and would let a screen write a pair the loader would then
/// silently correct — so the file would disagree with what the user was shown.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub server_enabled: bool,
    pub port: u16,
    pub sample_interval: Duration,
    pub sample_interval_idle: Duration,
    pub probes_enabled: bool,
    pub probe_network: bool,
    pub probe_interval: Duration,
    pub sessions_enabled: bool,
    pub samples_raw_days: u32,
    pub baseline_window_days: u32,
}

impl Draft {
    /// The values currently in force.
    pub fn from_config(config: &WatchConfig) -> Self {
        Self {
            server_enabled: config.server.enabled,
            port: config.server.port,
            sample_interval: config.collect.sample_interval,
            sample_interval_idle: config.collect.sample_interval_idle,
            probes_enabled: config.collect.probes_enabled,
            probe_network: config.collect.probe_network,
            probe_interval: config.collect.probe_interval,
            sessions_enabled: config.sessions.enabled,
            samples_raw_days: config.retention.samples_raw_days,
            baseline_window_days: config.analysis.baseline_window_days,
        }
    }

    /// Apply every floor and cross-field rule the loader applies.
    ///
    /// Called by [`save`] rather than trusted to the caller: the point of having one of these is that a
    /// screen cannot write a value the loader would quietly override.
    ///
    /// Note which rule is *not* applied. The CLI scales an untouched idle cadence down when a flag asks for
    /// faster active sampling, because there a flag was given and the file's idle value was not. Here both
    /// cadences are on screen and both were chosen, so rewriting the one the user can see would be the
    /// screen disagreeing with itself.
    ///
    /// [`save`]: Draft::save
    pub fn normalise(&mut self) {
        let (active, idle) =
            clamp_sample_intervals(self.sample_interval, self.sample_interval_idle);
        self.sample_interval = active;
        self.sample_interval_idle = idle;
        self.probe_interval = clamp_probe_interval(self.probe_interval);
        // A zero-day window compares today against nothing and reports every metric as insufficient,
        // which looks like a broken daemon rather than a setting.
        self.baseline_window_days = self.baseline_window_days.max(1);
        // `samples_raw_days` is deliberately *not* floored with it. Zero there is a coherent request —
        // keep no raw samples, summarise every minute as soon as it finishes — which the loader accepts
        // and the writer already implements; raising it to one here made this the only one of the three
        // paths that disagreed, so a user who asked for nothing raw got a day of it without being told.
    }

    /// Normalise, then write the changed keys into `watch.toml`.
    ///
    /// Returns the values actually written, which may differ from what was asked for.
    pub fn save(&self, data_dir: &Path) -> Result<Self> {
        let mut normalised = self.clone();
        normalised.normalise();
        let path = data_dir.join(CONFIG_FILE);
        // Falling back to the shipped template rather than an empty document: a file deleted between
        // loading and saving should come back documented, not as ten bare assignments.
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                DEFAULT_CONFIG_TOML.to_string()
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", path.display()));
            }
        };
        let mut document = text
            .parse::<DocumentMut>()
            .with_context(|| format!("parse {}", path.display()))?;

        set(
            &mut document,
            "server",
            "enabled",
            normalised.server_enabled,
        )?;
        set(&mut document, "server", "port", i64::from(normalised.port))?;
        set(
            &mut document,
            "collect",
            "sample_interval",
            duration_text(normalised.sample_interval),
        )?;
        set(
            &mut document,
            "collect",
            "sample_interval_idle",
            duration_text(normalised.sample_interval_idle),
        )?;
        set(
            &mut document,
            "collect",
            "probes_enabled",
            normalised.probes_enabled,
        )?;
        set(
            &mut document,
            "collect",
            "probe_network",
            normalised.probe_network,
        )?;
        set(
            &mut document,
            "collect",
            "probe_interval",
            duration_text(normalised.probe_interval),
        )?;
        set(
            &mut document,
            "sessions",
            "enabled",
            normalised.sessions_enabled,
        )?;
        set(
            &mut document,
            "retention",
            "samples_raw_days",
            i64::from(normalised.samples_raw_days),
        )?;
        set(
            &mut document,
            "analysis",
            "baseline_window_days",
            i64::from(normalised.baseline_window_days),
        )?;

        // Written through a temporary file in the same directory and renamed over the original, so an
        // interrupted save cannot leave a half-written configuration that the next start refuses to parse.
        let temporary = path.with_extension("toml.new");
        fs::write(&temporary, document.to_string())
            .with_context(|| format!("write {}", temporary.display()))?;
        fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))?;
        Ok(normalised)
    }
}

/// Set `table.key`, creating the table if the file does not have it yet.
fn set(
    document: &mut DocumentMut,
    table: &str,
    key: &str,
    new_value: impl Into<Value>,
) -> Result<()> {
    let entry = document
        .as_table_mut()
        .entry(table)
        .or_insert_with(|| Item::Table(Table::new()));
    // A file where `[collect]` has been replaced by `collect = 5` is not something to overwrite silently:
    // the user wrote it, and the loader would reject it too. Say which key is wrong rather than losing it.
    let Some(table_mut) = entry.as_table_mut() else {
        bail!("{table} in the configuration file is not a table, so {table}.{key} cannot be set");
    };
    table_mut[key] = value(new_value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::config;
    use std::time::Duration;

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

    /// The whole reason this module exists rather than a `toml::to_string` call.
    #[test]
    fn saving_preserves_the_files_comments() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        fs::write(&path, DEFAULT_CONFIG_TOML).unwrap();
        let mut changed = draft();
        changed.port = 9000;
        changed.save(temp.path()).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("port = 9000"), "{text}");
        assert!(
            text.contains("# The dashboard is loopback-only by design"),
            "a comment was lost:\n{text}"
        );
        assert!(
            text.contains("# Every value here is optional"),
            "the header comment was lost:\n{text}"
        );
    }

    /// The multi-line array a line-oriented editor would mangle.
    #[test]
    fn saving_leaves_untouched_keys_exactly_as_they_were() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        fs::write(&path, DEFAULT_CONFIG_TOML).unwrap();
        draft().save(temp.path()).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"sentinelone\", \"clamd\", \"eset\", \"avast\", \"avg\","),
            "the multi-line scanner array did not survive:\n{text}"
        );
        assert!(
            text.contains("# scratch_dir ="),
            "the commented-out example was lost:\n{text}"
        );
    }

    #[test]
    fn what_was_saved_loads_back_as_what_was_asked_for() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(CONFIG_FILE), DEFAULT_CONFIG_TOML).unwrap();
        let mut changed = draft();
        changed.port = 9100;
        changed.probes_enabled = false;
        changed.probe_interval = Duration::from_secs(1_800);
        changed.baseline_window_days = 21;
        changed.save(temp.path()).unwrap();

        let loaded = WatchConfig::load(Some(temp.path().to_path_buf())).unwrap();
        assert_eq!(loaded.server.port, 9100);
        assert!(!loaded.collect.probes_enabled);
        assert_eq!(loaded.collect.probe_interval, Duration::from_secs(1_800));
        assert_eq!(loaded.analysis.baseline_window_days, 21);
        assert_eq!(Draft::from_config(&loaded), changed);
    }

    /// A screen must not be able to write a value the loader would then override.
    #[test]
    fn intervals_below_the_floor_are_clamped_before_being_written() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(CONFIG_FILE), DEFAULT_CONFIG_TOML).unwrap();
        let mut absurd = draft();
        absurd.sample_interval = Duration::from_millis(1);
        absurd.probe_interval = Duration::from_millis(1);
        absurd.baseline_window_days = 0;
        let written = absurd.save(temp.path()).unwrap();

        assert_eq!(written.sample_interval, config::SHORTEST_SAMPLE);
        assert_eq!(written.probe_interval, config::SHORTEST_PROBE);
        assert_eq!(written.baseline_window_days, 1);
        let loaded = WatchConfig::load(Some(temp.path().to_path_buf())).unwrap();
        assert_eq!(Draft::from_config(&loaded), written);
    }

    /// Keeping no raw samples is a choice, and the file, the save path and the loader all read it the same.
    #[test]
    fn a_retention_window_of_no_days_survives_the_save_and_the_loader() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(CONFIG_FILE), DEFAULT_CONFIG_TOML).unwrap();
        let mut nothing_raw = draft();
        nothing_raw.samples_raw_days = 0;
        let written = nothing_raw.save(temp.path()).unwrap();
        assert_eq!(
            written.samples_raw_days, 0,
            "the save path used to raise this to a day without saying so"
        );

        let loaded = WatchConfig::load(Some(temp.path().to_path_buf())).unwrap();
        assert_eq!(loaded.retention.samples_raw_days, 0);
        assert_eq!(Draft::from_config(&loaded), written);
    }

    /// Both cadences are on screen, so the one the user can see is written as chosen.
    #[test]
    fn a_faster_active_interval_leaves_a_chosen_idle_cadence_alone() {
        let mut fast = draft();
        fast.sample_interval = Duration::from_secs(1);
        fast.sample_interval_idle = Duration::from_secs(30);
        fast.normalise();
        assert_eq!(fast.sample_interval, Duration::from_secs(1));
        assert_eq!(
            fast.sample_interval_idle,
            Duration::from_secs(30),
            "the screen showed 30s and 30s is what it must save"
        );
    }

    /// The cross-field rule that does apply: an idle cadence faster than the active one is not one.
    #[test]
    fn an_idle_cadence_faster_than_the_active_one_is_raised_to_it() {
        let mut slow = draft();
        slow.sample_interval = Duration::from_secs(60);
        slow.sample_interval_idle = Duration::from_secs(30);
        slow.normalise();
        assert_eq!(slow.sample_interval_idle, Duration::from_secs(60));
    }

    /// What a saved pair has to survive: the loader applying the same rule to it and changing nothing.
    #[test]
    fn a_slow_idle_cadence_round_trips_through_the_loader_unscaled() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(CONFIG_FILE), DEFAULT_CONFIG_TOML).unwrap();
        let mut patient = draft();
        patient.sample_interval = Duration::from_secs(5);
        patient.sample_interval_idle = Duration::from_secs(300);
        let written = patient.save(temp.path()).unwrap();
        assert_eq!(written.sample_interval_idle, Duration::from_secs(300));

        let loaded = WatchConfig::load(Some(temp.path().to_path_buf())).unwrap();
        assert_eq!(Draft::from_config(&loaded), written);
    }

    #[test]
    fn saving_creates_a_documented_file_when_none_exists() {
        let temp = tempfile::tempdir().unwrap();
        draft().save(temp.path()).unwrap();
        let text = fs::read_to_string(temp.path().join(CONFIG_FILE)).unwrap();
        assert!(
            text.contains("# AgentBench dashboard configuration."),
            "{text}"
        );
    }

    #[test]
    fn a_key_whose_table_is_not_a_table_is_reported_rather_than_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(CONFIG_FILE), "server = 5\n").unwrap();
        let error = draft().save(temp.path()).unwrap_err().to_string();
        assert!(error.contains("is not a table"), "{error}");
    }

    /// Every duration this module writes has to be readable by the parser that will read it back.
    #[test]
    fn written_durations_round_trip_through_the_parser() {
        for seconds in [1, 5, 30, 60, 90, 900, 3_600, 5_400, 86_400] {
            let original = Duration::from_secs(seconds);
            let text = duration_text(original);
            assert_eq!(
                config::parse_duration(&text).unwrap(),
                original,
                "{text:?} did not round-trip"
            );
        }
    }
}
