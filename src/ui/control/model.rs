//! What the control centre shows, and what changing a row does.
//!
//! Changes apply immediately rather than accumulating behind a save key. A settings screen with a save
//! step has to answer "what happens if you quit without saving", and every answer is bad: discarding
//! silently loses work, prompting adds a dialog to a screen whose whole point is that it is quicker than
//! remembering flags, and saving on exit means a stray keypress persists. Applying on the spot also puts
//! each failure next to the thing that caused it — a `PATH` that could not be written says so on that row,
//! not at the end of a batch.

use crate::{
    install::{self, Autostart, AutostartState},
    watch::{WatchConfig, config, settings::Draft},
};
use anyhow::Result;
use std::{path::PathBuf, time::Duration};

/// Sections in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Startup,
    Install,
    Collection,
    Sessions,
    History,
    Server,
    Actions,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Self::Startup => "Startup",
            Self::Install => "Install",
            Self::Collection => "Collection",
            Self::Sessions => "Sessions",
            Self::History => "History",
            Self::Server => "Server",
            Self::Actions => "Actions",
        }
    }
}

/// Everything a row can be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    RunAtLogin,
    StartInTray,
    LoginDelay,
    InstallHere,
    OnPath,
    SampleInterval,
    SampleIntervalIdle,
    ProbesEnabled,
    ProbeNetwork,
    ProbeInterval,
    SessionsEnabled,
    SamplesRawDays,
    BaselineWindowDays,
    ServerEnabled,
    ServerPort,
    StartCollecting,
    OpenDashboard,
    RunBenchmark,
    RunBenchmarkElevated,
    CompareReports,
    EraseCollectedData,
}

/// How a row responds to a keypress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Space or Enter flips it.
    Toggle,
    /// Enter opens an inline editor.
    Value,
    /// Enter does it.
    Action,
}

impl Field {
    pub fn section(self) -> Section {
        match self {
            Self::RunAtLogin | Self::StartInTray | Self::LoginDelay => Section::Startup,
            Self::InstallHere | Self::OnPath => Section::Install,
            Self::SampleInterval
            | Self::SampleIntervalIdle
            | Self::ProbesEnabled
            | Self::ProbeNetwork
            | Self::ProbeInterval => Section::Collection,
            Self::SessionsEnabled => Section::Sessions,
            Self::SamplesRawDays | Self::BaselineWindowDays => Section::History,
            Self::ServerEnabled | Self::ServerPort => Section::Server,
            Self::StartCollecting
            | Self::OpenDashboard
            | Self::RunBenchmark
            | Self::RunBenchmarkElevated
            | Self::CompareReports
            | Self::EraseCollectedData => Section::Actions,
        }
    }

    pub fn kind(self) -> Kind {
        match self {
            Self::RunAtLogin
            | Self::StartInTray
            | Self::OnPath
            | Self::ProbesEnabled
            | Self::ProbeNetwork
            | Self::SessionsEnabled
            | Self::ServerEnabled => Kind::Toggle,
            Self::LoginDelay
            | Self::SampleInterval
            | Self::SampleIntervalIdle
            | Self::ProbeInterval
            | Self::SamplesRawDays
            | Self::BaselineWindowDays
            | Self::ServerPort => Kind::Value,
            Self::InstallHere
            | Self::StartCollecting
            | Self::OpenDashboard
            | Self::RunBenchmark
            | Self::RunBenchmarkElevated
            | Self::CompareReports
            | Self::EraseCollectedData => Kind::Action,
        }
    }

    /// Whether Enter arms the row and a second Enter carries it out.
    ///
    /// Reserved for the one thing on this screen that cannot be undone. Every other row either changes
    /// a setting that can be changed back or starts a process that can be closed; this one deletes
    /// measurements that only exist here. A row next to "Run a benchmark" that erased months of history
    /// on a single keypress would be a trap, and a modal dialog for one row would be a heavier answer
    /// than the screen needs.
    pub fn needs_confirmation(self) -> bool {
        matches!(self, Self::EraseCollectedData)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::RunAtLogin => "Run at login",
            Self::StartInTray => "Start in tray",
            Self::LoginDelay => "Delay after login",
            Self::InstallHere => "Install a durable copy",
            Self::OnPath => "On PATH",
            Self::SampleInterval => "Sample interval",
            Self::SampleIntervalIdle => "Sample interval (idle)",
            Self::ProbesEnabled => "Controlled probes",
            Self::ProbeNetwork => "Probe the network",
            Self::ProbeInterval => "Probe interval",
            Self::SessionsEnabled => "Read transcripts",
            Self::SamplesRawDays => "Keep raw samples",
            Self::BaselineWindowDays => "Baseline window",
            Self::ServerEnabled => "Serve the dashboard",
            Self::ServerPort => "Port",
            Self::StartCollecting => "Start collecting",
            Self::OpenDashboard => "Open the dashboard",
            Self::RunBenchmark => "Run a benchmark",
            Self::RunBenchmarkElevated => "Run a benchmark, elevated",
            Self::CompareReports => "Compare two reports",
            Self::EraseCollectedData => "Erase collected data",
        }
    }

    /// One line explaining what the row is for, shown for the focused row.
    pub fn help(self) -> &'static str {
        match self {
            Self::RunAtLogin => {
                "Registers an unelevated logon task, so no administrator prompt appears."
            }
            Self::StartInTray => {
                "Starts the windowless build with a tray icon instead of a console window."
            }
            Self::LoginDelay => {
                "Probes during the login storm count as contended and drop out of the baseline."
            }
            Self::InstallHere => {
                "Copies this executable somewhere cargo clean will not delete. PATH and the logon task \
                 both point at that copy."
            }
            Self::OnPath => "Appends the install directory to your user PATH. Never prepends it.",
            Self::SampleInterval => "How often the passive sampler takes a reading while busy.",
            Self::SampleIntervalIdle => {
                "Cadence once the machine looks idle. Pulled down automatically if it exceeds the \
                 active interval."
            }
            Self::ProbesEnabled => {
                "The only collector that loads the machine, and the only way to tell a slower disk \
                 from a busier one."
            }
            Self::ProbeNetwork => {
                "One HTTPS request per probe. The only part of the daemon that leaves this machine."
            }
            Self::ProbeInterval => "How often the controlled micro-workload runs.",
            Self::SessionsEnabled => {
                "Reads Claude Code transcripts. Costs nothing and is where real agent timings come from."
            }
            Self::SamplesRawDays => {
                "Raw samples are rolled up to per-minute aggregates after this."
            }
            Self::BaselineWindowDays => "Trailing days today is compared against.",
            Self::ServerEnabled => "Serve the web dashboard on loopback while collecting.",
            Self::ServerPort => "Loopback port for the dashboard.",
            Self::StartCollecting => {
                "Starts the daemon in its own window, against this data directory. Closing that \
                 window stops collecting."
            }
            Self::OpenDashboard => {
                "Opens the dashboard in your browser. Requires the daemon running."
            }
            Self::RunBenchmark => "Runs the standard preset in a new window.",
            Self::RunBenchmarkElevated => {
                "Prompts for administrator rights, which adds Defender diagnostics to the report."
            }
            Self::CompareReports => {
                "Compares the two newest reports in this directory, the older as the baseline, and \
                 opens the result."
            }
            Self::EraseCollectedData => {
                "Deletes every sample, probe and derived session row. Transcripts are re-read from \
                 disk, so session history returns; probe and sample history does not."
            }
        }
    }
}

/// The state every row is rendered from, read once per refresh.
pub struct State {
    pub config: WatchConfig,
    pub draft: Draft,
    /// The registered logon task, or why it could not be read.
    ///
    /// A `Result` rather than folding a failure into "absent". Reporting "off" for "cannot tell" would
    /// invite the user to switch on a task that already exists, and the screen is refreshed repeatedly —
    /// so this also avoids needing a `'static` string per read, which an earlier version obtained by
    /// leaking one every time.
    pub autostart: Result<AutostartState, String>,
    pub install_dir: Option<PathBuf>,
    pub origin: Option<install::Origin>,
    pub on_path: Option<bool>,
    pub login_delay: Duration,
    pub start_in_tray: bool,
    pub elevated: bool,
    /// Whether a daemon holds the instance lock.
    ///
    /// Refreshed from the status band rather than probed again here, so the row that starts collection
    /// and the heading that says whether anything is collecting cannot disagree.
    pub daemon_running: bool,
    /// Size of the database, or `None` when there is not one yet.
    pub database_bytes: Option<u64>,
    /// Benchmark reports in the working directory, newest first.
    pub reports: Vec<PathBuf>,
}

impl State {
    /// Read everything the screen displays.
    ///
    /// Failures in the optional parts are folded into `None` rather than propagated: a machine where the
    /// registry cannot be read should still be able to change its sampling interval.
    pub fn read(config: WatchConfig) -> Self {
        let autostart = install::autostart_state().map_err(|error| format!("{error:#}"));
        let install_dir = install::install_dir().ok();
        let on_path = install_dir
            .as_ref()
            .and_then(|dir| install::on_path(dir).ok());
        let (login_delay, start_in_tray) =
            recorded_choices(&autostart).unwrap_or((install::DEFAULT_DELAY, false));
        Self {
            daemon_running: crate::watch::is_running(&config),
            database_bytes: std::fs::metadata(config.database_path())
                .ok()
                .map(|meta| meta.len()),
            reports: reports_in_working_directory(),
            draft: Draft::from_config(&config),
            config,
            autostart,
            install_dir,
            origin: install::origin().ok(),
            on_path,
            login_delay,
            start_in_tray,
            elevated: install::is_elevated(),
        }
    }

    /// Read the login task again and adopt what it says.
    ///
    /// Called after every change to the task, and after every attempt at one. A screen that changed the task
    /// and then went on describing the previous reading is wrong twice over: the row reports the old answer,
    /// and the rows that only describe the task — the tray choice and the delay — re-register nothing,
    /// because they believe there is no task to re-register.
    pub fn reread_autostart(&mut self) {
        self.adopt_autostart(install::autostart_state().map_err(|error| format!("{error:#}")));
    }

    /// Take a reading of the login task as the new truth.
    ///
    /// A reading that finds a task carries the delay and tray choice with it, for the reason
    /// [`Self::read`] takes them from one: the screen should describe what will happen at login rather than a
    /// preference kept beside the task that may have drifted from it. That also undoes an optimistic change
    /// whose registration was refused, since the reading is of the task that is still there.
    ///
    /// A reading that finds no task leaves both alone, so a choice made while autostart is off survives
    /// until it is switched on.
    fn adopt_autostart(&mut self, read: Result<AutostartState, String>) {
        if let Some((delay, tray)) = recorded_choices(&read) {
            self.login_delay = delay;
            self.start_in_tray = tray;
        }
        self.autostart = read;
    }

    /// Whether a login task is registered.
    ///
    /// False when the task could not be read at all, which is the safe way round for the callers: the rows
    /// that would act on a task are already disabled with that reason, and treating "cannot tell" as "on"
    /// would have them replace a task nobody has seen.
    pub fn autostart_enabled(&self) -> bool {
        matches!(&self.autostart, Ok(state) if state.is_enabled())
    }

    /// The value shown on the right of a row.
    pub fn value(&self, field: Field) -> String {
        match field {
            Field::RunAtLogin => match &self.autostart {
                Ok(state) => on_off(state.is_enabled()),
                Err(_) => "unknown".into(),
            },
            Field::StartInTray => on_off(self.start_in_tray),
            Field::LoginDelay => config::duration_text(self.login_delay),
            Field::InstallHere => match &self.origin {
                Some(install::Origin::Installed(_)) => "installed".into(),
                Some(install::Origin::BuildTree(_)) => "running from a build directory".into(),
                Some(install::Origin::Elsewhere(_)) => "running from elsewhere".into(),
                None => "unknown".into(),
            },
            Field::OnPath => match self.on_path {
                Some(value) => on_off(value),
                None => "unknown".into(),
            },
            Field::SampleInterval => config::duration_text(self.draft.sample_interval),
            Field::SampleIntervalIdle => config::duration_text(self.draft.sample_interval_idle),
            Field::ProbesEnabled => on_off(self.draft.probes_enabled),
            Field::ProbeNetwork => on_off(self.draft.probe_network),
            Field::ProbeInterval => config::duration_text(self.draft.probe_interval),
            Field::SessionsEnabled => on_off(self.draft.sessions_enabled),
            Field::SamplesRawDays => format!("{}d", self.draft.samples_raw_days),
            Field::BaselineWindowDays => format!("{}d", self.draft.baseline_window_days),
            Field::ServerEnabled => on_off(self.draft.server_enabled),
            Field::ServerPort => self.draft.port.to_string(),
            Field::StartCollecting => if self.daemon_running {
                "running"
            } else {
                "not running"
            }
            .into(),
            Field::OpenDashboard => format!("http://127.0.0.1:{}/", self.draft.port),
            Field::RunBenchmark => "standard preset".into(),
            Field::RunBenchmarkElevated => {
                if self.elevated {
                    "already elevated".into()
                } else {
                    "prompts for consent".into()
                }
            }
            // Names the pair rather than counting the directory: the row acts on two specific files and
            // a reader is entitled to know which two before pressing Enter.
            Field::CompareReports => match self.reports.as_slice() {
                [candidate, baseline, ..] => {
                    format!("{} → {}", file_label(baseline), file_label(candidate))
                }
                [_] => "only one report here".into(),
                [] => "no reports here".into(),
            },
            Field::EraseCollectedData => match self.database_bytes {
                Some(bytes) => format!("{} collected", crate::ui::format::mib(bytes)),
                None => "nothing collected yet".into(),
            },
        }
    }

    /// Why a row cannot be used, when it cannot.
    ///
    /// Rows are disabled rather than hidden. A startup section that simply vanished on an unsupported
    /// platform would read as a missing feature instead of an unsupported one.
    pub fn unavailable(&self, field: Field) -> Option<String> {
        match field {
            Field::RunAtLogin | Field::StartInTray | Field::LoginDelay => {
                match &self.autostart {
                    Err(error) => {
                        return Some(format!("the logon task could not be read: {error}"));
                    }
                    Ok(AutostartState::Unsupported(reason)) => return Some((*reason).to_string()),
                    Ok(_) => {}
                }
                // A task pointing into a build directory breaks at the next `cargo clean`, so the row that
                // would create one says why instead of creating it.
                if matches!(self.origin, Some(install::Origin::BuildTree(_))) {
                    return Some(
                        "install a durable copy first: a task pointing into target/ breaks at the \
                         next cargo clean"
                            .into(),
                    );
                }
                None
            }
            Field::OnPath => match (install::path_support().reason(), &self.origin) {
                (Some(reason), _) => Some(reason.to_string()),
                (None, Some(install::Origin::BuildTree(_))) => Some(
                    "install a durable copy first: a PATH entry into target/ breaks at the next \
                     cargo clean"
                        .into(),
                ),
                (None, _) => None,
            },
            Field::InstallHere => self.install_dir.as_ref().map_or(
                Some("no per-user programs directory is known here".into()),
                |_| None,
            ),
            Field::RunBenchmarkElevated if self.elevated => Some(
                "this process is already elevated, so plain \"Run a benchmark\" is enough".into(),
            ),
            Field::StartCollecting if self.daemon_running => {
                Some("collection is already running".into())
            }
            Field::CompareReports if self.reports.len() < 2 => Some(format!(
                "two reports are needed and {} {} in {}",
                self.reports.len(),
                if self.reports.len() == 1 { "is" } else { "are" },
                std::env::current_dir()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_else(|_| "this directory".into())
            )),
            // Refused here as well as by `watch::reset_collected_data`, so the row reads as disabled
            // rather than failing on the second Enter. The check there is the one that matters: this one
            // is a snapshot from the last status refresh.
            Field::EraseCollectedData if self.daemon_running => {
                Some("stop collection first: the daemon has the database open".into())
            }
            Field::EraseCollectedData if self.database_bytes.is_none() => {
                Some("there is nothing collected to erase".into())
            }
            _ => None,
        }
    }

    /// The rows to display, in order.
    pub fn fields() -> Vec<Field> {
        vec![
            Field::RunAtLogin,
            Field::StartInTray,
            Field::LoginDelay,
            Field::InstallHere,
            Field::OnPath,
            Field::SampleInterval,
            Field::SampleIntervalIdle,
            Field::ProbesEnabled,
            Field::ProbeNetwork,
            Field::ProbeInterval,
            Field::SessionsEnabled,
            Field::SamplesRawDays,
            Field::BaselineWindowDays,
            Field::ServerEnabled,
            Field::ServerPort,
            // Starting collection comes first in the section because everything above it is a setting
            // that only takes effect once something is collecting, and erasing comes last because it is
            // the row nobody should reach by overshooting.
            Field::StartCollecting,
            Field::OpenDashboard,
            Field::RunBenchmark,
            Field::RunBenchmarkElevated,
            Field::CompareReports,
            Field::EraseCollectedData,
        ]
    }

    /// The executable a logon task or `PATH` entry should point at.
    ///
    /// The installed copy when there is one, otherwise whatever is running. Never a build-tree path: the
    /// rows that would use one are disabled before this is reached.
    pub fn durable_program(&self) -> Option<PathBuf> {
        match &self.origin {
            Some(install::Origin::Installed(path) | install::Origin::Elsewhere(path)) => {
                Some(path.clone())
            }
            _ => None,
        }
    }

    /// The task that would be registered for the current choices.
    ///
    /// The tray choice picks the executable, not just a flag on the command line. The two builds are separate
    /// binaries — the windowless one is the daemon and takes no subcommand — so a task that recorded "tray"
    /// while still pointing at the console build would launch the console build with no subcommand at all.
    pub fn desired_autostart(&self) -> Option<Autostart> {
        Some(Autostart {
            program: install::build_for(&self.durable_program()?, self.start_in_tray),
            tray: self.start_in_tray,
            delay: self.login_delay,
        })
    }

    /// Re-read the parts that change while the screen is open.
    ///
    /// Three facts about the world rather than about the configuration: whether anything is collecting,
    /// how much has been collected, and which reports exist. All three are what an action row acts on,
    /// so a screen left open while a benchmark finishes or a daemon starts has to notice.
    ///
    /// `running` is passed in rather than probed, so this and the status band cannot disagree.
    pub fn refresh_volatile(&mut self, running: bool) {
        self.daemon_running = running;
        self.database_bytes = std::fs::metadata(self.config.database_path())
            .ok()
            .map(|meta| meta.len());
        self.reports = reports_in_working_directory();
    }

    /// Persist the collection settings and adopt whatever the writer normalised them to.
    pub fn save_draft(&mut self) -> Result<()> {
        let written = self.draft.save(&self.config.data_dir)?;
        self.draft = written;
        Ok(())
    }
}

/// The delay and tray choice a reading of the login task implies, when it implies any.
///
/// The single place that derivation lives, so the first read when the screen opens and every re-read after a
/// change cannot disagree about where these two settings come from.
fn recorded_choices(read: &Result<AutostartState, String>) -> Option<(Duration, bool)> {
    match read {
        Ok(AutostartState::Present(task)) => Some((task.delay, task.tray)),
        _ => None,
    }
}

fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_string()
}

/// Prefix every report `report::write_report` names for itself.
const REPORT_PREFIX: &str = "agentbench-";

/// Benchmark reports in the working directory, newest first.
///
/// The working directory because that is where a benchmark writes when nobody passed `--output`, and
/// the benchmark rows above launch exactly that. Matched by the name the writer chooses rather than by
/// reading every `.json` file present: a directory of unrelated JSON would otherwise be parsed on every
/// refresh to find out what it was.
fn reports_in_working_directory() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(".") else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with(REPORT_PREFIX) && name.ends_with(".json")
        })
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    // Newest first, and by name where two share a timestamp so the order is at least deterministic.
    found.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.file_name().cmp(&right.1.file_name()))
    });
    found.into_iter().map(|(_, path)| path).collect()
}

/// A report's file name without the shared prefix or extension, for a row that has to fit a column.
fn file_label(path: &std::path::Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .map(|stem| stem.trim_start_matches(REPORT_PREFIX).to_string())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A state against an empty data directory, as a first run has.
    fn state() -> State {
        let temp = tempdir().expect("a temporary data directory");
        let config =
            WatchConfig::load(Some(temp.path().to_path_buf())).expect("defaults should load");
        State::read(config)
    }

    fn task(tray: bool, delay: Duration) -> AutostartState {
        AutostartState::Present(Autostart {
            program: install::build_for(
                &PathBuf::from(r"C:\Programs\AgentBench\agentbench.exe"),
                tray,
            ),
            tray,
            delay,
        })
    }

    /// The regression the re-read exists for: a change to the task used to leave every startup row showing
    /// the reading taken when the screen opened, so switching autostart on looked like it had done nothing.
    #[test]
    fn a_reading_of_a_registered_task_becomes_what_the_rows_show() {
        let mut state = state();
        state.autostart = Ok(AutostartState::Absent);
        state.start_in_tray = false;
        state.login_delay = Duration::from_secs(1);

        state.adopt_autostart(Ok(task(true, Duration::from_secs(120))));

        assert!(state.autostart_enabled());
        assert_eq!(state.value(Field::RunAtLogin), "on");
        assert_eq!(state.value(Field::StartInTray), "on");
        assert_eq!(state.login_delay, Duration::from_secs(120));
    }

    /// A choice made while autostart is off lives nowhere but here, so a reading that finds no task must
    /// leave it alone rather than reset it to the defaults.
    #[test]
    fn a_reading_that_finds_no_task_keeps_the_recorded_choice() {
        let mut state = state();
        state.start_in_tray = true;
        state.login_delay = Duration::from_secs(300);

        state.adopt_autostart(Ok(AutostartState::Absent));

        assert!(!state.autostart_enabled());
        assert_eq!(state.value(Field::RunAtLogin), "off");
        assert_eq!(state.value(Field::StartInTray), "on");
        assert_eq!(state.login_delay, Duration::from_secs(300));
    }

    /// "Cannot tell" is not "on": the rows that would replace a task are disabled with the reason instead.
    #[test]
    fn an_unreadable_task_does_not_count_as_registered() {
        let mut state = state();
        state.adopt_autostart(Err("schtasks could not read the logon task".into()));

        assert!(!state.autostart_enabled());
        assert_eq!(state.value(Field::RunAtLogin), "unknown");
        assert!(state.unavailable(Field::StartInTray).is_some());
    }

    #[test]
    fn every_field_belongs_to_a_section_and_has_a_kind() {
        for field in State::fields() {
            assert!(!field.label().is_empty(), "{field:?} has no label");
            assert!(!field.help().is_empty(), "{field:?} has no help");
            // Exercising both keeps the matches exhaustive in practice as well as at compile time.
            let _ = field.section();
            let _ = field.kind();
        }
    }

    /// The list drives navigation, so a duplicate would make one row unreachable.
    #[test]
    fn the_field_list_has_no_duplicates() {
        let fields = State::fields();
        for (index, field) in fields.iter().enumerate() {
            assert!(
                !fields[index + 1..].contains(field),
                "{field:?} appears twice"
            );
        }
    }

    /// Rows are grouped by section, so the sections must appear in contiguous runs — otherwise a heading
    /// would be drawn more than once.
    #[test]
    fn fields_are_grouped_into_contiguous_sections() {
        let mut seen: Vec<Section> = Vec::new();
        for field in State::fields() {
            let section = field.section();
            if seen.last() != Some(&section) {
                assert!(
                    !seen.contains(&section),
                    "{section:?} is split into more than one run"
                );
                seen.push(section);
            }
        }
        assert_eq!(seen.first(), Some(&Section::Startup));
        assert_eq!(seen.last(), Some(&Section::Actions));
    }

    #[test]
    fn actions_are_not_toggles() {
        for field in [
            Field::InstallHere,
            Field::OpenDashboard,
            Field::RunBenchmark,
        ] {
            assert_eq!(field.kind(), Kind::Action, "{field:?}");
        }
        for field in [Field::RunAtLogin, Field::OnPath, Field::ProbesEnabled] {
            assert_eq!(field.kind(), Kind::Toggle, "{field:?}");
        }
    }
}
