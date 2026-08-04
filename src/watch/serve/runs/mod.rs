//! Starting a benchmark from the dashboard, and reporting on the one that is running.
//!
//! The daemon owns one [`Registry`]. It holds at most one benchmark at a time and remembers the last one
//! that finished, which is what lets a page reloaded after a run still show its outcome rather than an idle
//! form and no explanation.
//!
//! ### Why one at a time
//!
//! Not a simplification. Two benchmarks on one machine measure each other: every workload here is sized to
//! use a documented fraction of the machine, and two of them contending produce two reports that are both
//! wrong and neither obviously so. A second request is refused with `409` rather than queued, because a
//! queue would silently start a stress run twenty minutes after somebody clicked, on a machine whose state
//! nobody was watching by then.
//!
//! ### What is deliberately absent
//!
//! There is no way to ask for an elevated run. The control centre has one, because a person is at the
//! keyboard and can connect the consent prompt to the button they just pressed; a UAC dialog raised by a
//! web page is the thing ADR 0001's autostart reasoning goes out of its way to avoid.

pub mod child;
pub mod request;

pub use request::{BenchRequest, MAX_COST_CAP_USD, Summary, ValidRequest};

use crate::{
    bench::{PHASE_COUNT, Phase, Preset},
    watch::store::{Level, Sink},
};
use anyhow::{Context, Result, bail};
use child::Running;
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

/// Subdirectory of the data directory that reports written by the dashboard land in.
pub const REPORTS_DIR: &str = "reports";

/// What the registry is doing, as the page sees it.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunState {
    /// Nothing has been started in this daemon's lifetime.
    Idle,
    Running {
        run_id: String,
        started_ms: i64,
        request: Summary,
        /// The phase most recently announced, absent until the first one is.
        phase: Option<PhaseReport>,
    },
    Finished {
        run_id: String,
        started_ms: i64,
        ended_ms: i64,
        request: Summary,
        /// Whether the child exited successfully *and* left a report behind.
        ok: bool,
        /// `None` when the process was ended by a signal or a kill rather than by returning.
        exit_code: Option<i32>,
        /// The report it wrote, when it wrote one.
        report_path: Option<String>,
        /// The markdown summary written beside it.
        markdown_path: Option<String>,
        /// Whether this run was stopped on request rather than left to finish.
        cancelled: bool,
        /// The tail of the child's stderr, which is where a failure explains itself.
        stderr: Vec<String>,
    },
}

/// One phase announcement, as the page draws it.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseReport {
    pub number: usize,
    pub total: usize,
    pub label: String,
}

impl From<Phase> for PhaseReport {
    fn from(phase: Phase) -> Self {
        Self {
            number: phase.number,
            total: phase.total,
            label: phase.label,
        }
    }
}

/// What a preset commits the machine to, for the form to describe before anything is started.
#[derive(Debug, Clone, Serialize)]
pub struct PresetOption {
    pub name: &'static str,
    /// Point past which the run reports overrun, in seconds.
    pub duration_limit_seconds: u64,
    /// Window the run keeps observing for even when the measured phases finish early.
    pub minimum_duration_seconds: u64,
    /// Most the filesystem workloads will write.
    pub disk_limit_bytes: u64,
    /// Ceiling on the memory workload's buffer.
    pub memory_cap_bytes: u64,
    pub small_files: usize,
    pub sqlite_rows: usize,
}

/// Everything the benchmark form needs to draw itself.
///
/// Served rather than hardcoded in JavaScript so that a preset's limits are stated once, in
/// `bench::preset`, and the page cannot describe a `stress` run as writing two gigabytes on the day that
/// number changes.
#[derive(Debug, Clone, Serialize)]
pub struct Options {
    /// Whether this daemon will start a run at all.
    pub allowed: bool,
    /// Why not, when it will not.
    pub refusal: Option<&'static str>,
    pub presets: Vec<PresetOption>,
    pub default_preset: &'static str,
    /// Directory a run with no target named will measure.
    pub default_target_dir: String,
    /// Where reports will be written.
    pub reports_dir: String,
    /// Ceiling the dashboard imposes on live-LLM spend.
    pub max_cost_cap_usd: f64,
    /// Phases a run announces, so a gauge has a denominator before the first announcement arrives.
    pub phase_count: usize,
}

/// The registry's own inner state, behind one lock.
///
/// Both populated variants are boxed. `RunState::Finished` carries the request summary and up to forty lines
/// of captured stderr, which makes it by far the largest, and an enum sized to its biggest variant would pay
/// for that in the idle case that is true almost all of the time.
enum Inner {
    Idle,
    Running(Box<Active>),
    Finished(Box<RunState>),
}

/// A benchmark in flight.
struct Active {
    run_id: String,
    started_ms: i64,
    request: Summary,
    running: Running,
    cancelled: bool,
    report_path: PathBuf,
}

/// The daemon's single benchmark slot.
pub struct Registry {
    inner: Mutex<Inner>,
    /// This program's own executable, resolved once at construction.
    ///
    /// Resolved eagerly and deliberately: `current_exe` can fail, and finding that out when somebody presses
    /// the button — after the page has told them a run is starting — is worse than finding out at startup,
    /// where the daemon can log it and the page can grey the form out.
    program: PathBuf,
    data_dir: PathBuf,
    sink: Sink,
}

/// Written by hand, and deliberately without taking the lock.
///
/// [`Settings`] derives `Debug` and carries a registry, so this exists to keep that derive working. Locking
/// here would mean that formatting a `Settings` — which is the sort of thing a `dbg!` or a panic message
/// does — could block on whatever is holding the slot, or deadlock outright if the formatting happened
/// underneath it. What a reader of such a message needs is the two paths, and those are immutable.
///
/// [`Settings`]: crate::watch::serve::Settings
impl std::fmt::Debug for Registry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Registry")
            .field("program", &self.program)
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

impl Registry {
    /// Build a registry for a daemon whose data directory is `data_dir`.
    pub fn new(data_dir: &Path, sink: Sink) -> Result<Arc<Self>> {
        let program = std::env::current_exe().context("locate the running executable")?;
        Ok(Arc::new(Self {
            inner: Mutex::new(Inner::Idle),
            program,
            data_dir: data_dir.to_path_buf(),
            sink,
        }))
    }

    /// Where reports started from the dashboard are written.
    pub fn reports_dir(&self) -> PathBuf {
        self.data_dir.join(REPORTS_DIR)
    }

    /// The daemon's data directory, which is also what a request naming no target measures.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Everything the form needs, including the limits each preset implies.
    pub fn options(&self) -> Options {
        Options {
            allowed: true,
            refusal: None,
            presets: [Preset::Quick, Preset::Standard, Preset::Stress]
                .into_iter()
                .map(preset_option)
                .collect(),
            default_preset: Preset::Standard.name(),
            default_target_dir: self.data_dir.display().to_string(),
            reports_dir: self.reports_dir().display().to_string(),
            max_cost_cap_usd: MAX_COST_CAP_USD,
            phase_count: PHASE_COUNT,
        }
    }

    /// The options a daemon that will not start runs reports.
    ///
    /// Still a full payload, so the page renders its form and explains why it is disabled rather than
    /// failing to load and leaving the user to guess.
    pub fn refused_options(refusal: &'static str) -> Options {
        Options {
            allowed: false,
            refusal: Some(refusal),
            presets: [Preset::Quick, Preset::Standard, Preset::Stress]
                .into_iter()
                .map(preset_option)
                .collect(),
            default_preset: Preset::Standard.name(),
            default_target_dir: String::new(),
            reports_dir: String::new(),
            max_cost_cap_usd: MAX_COST_CAP_USD,
            phase_count: PHASE_COUNT,
        }
    }

    /// Start a benchmark, or say why not.
    ///
    /// Returns the new run's identifier. The identifier is this daemon's own, not the report's: a report's
    /// `run_id` does not exist until the run has finished writing one, and the page needs something to poll
    /// with from the moment it asks.
    pub fn start(&self, valid: &ValidRequest) -> Result<String> {
        let mut inner = self
            .inner
            .lock()
            .expect("the registry mutex is not poisoned");
        // Checked under the same lock the start happens under, so two requests arriving together cannot both
        // find the slot empty. The server is single-threaded today and this would hold anyway; relying on
        // that would be relying on something no comment at the call site records.
        if let Inner::Running(active) = &mut *inner {
            if active.running.exit_status().is_none() {
                bail!(
                    "a {} benchmark started {} is still running; wait for it or stop it first",
                    active.request.preset,
                    describe_age(crate::watch::store::now_ms() - active.started_ms)
                );
            }
            // It has finished but nothing has noticed yet, which the poll below would have settled. Settle it
            // here rather than refusing a request the machine is perfectly able to serve.
            let finished = std::mem::replace(&mut *inner, Inner::Idle);
            if let Inner::Running(active) = finished {
                *inner = Inner::Finished(Box::new(self.conclude(*active)));
            }
        }

        let run_id = uuid::Uuid::new_v4().to_string();
        let reports = self.reports_dir();
        std::fs::create_dir_all(&reports)
            .with_context(|| format!("create {}", reports.display()))?;
        let report_path = reports.join(format!("agentbench-{}.json", &run_id[..8]));
        let args = valid.to_args(&report_path);
        let running = Running::spawn(&self.program, &args)?;
        let summary = valid.summary();
        // Logged because a benchmark is the loudest thing this machine will do for the next few minutes, and
        // the operational log is where somebody looking at a cliff in the passive series goes to ask why.
        self.sink.log(
            Level::Info,
            "runs",
            format!(
                "started a {} benchmark from the dashboard; live LLM {}",
                summary.preset,
                if summary.live_llm {
                    "enabled"
                } else {
                    "disabled"
                }
            ),
        );
        *inner = Inner::Running(Box::new(Active {
            run_id: run_id.clone(),
            started_ms: crate::watch::store::now_ms(),
            request: summary,
            running,
            cancelled: false,
            report_path,
        }));
        Ok(run_id)
    }

    /// What the registry is doing now, settling a run that has finished since the last look.
    ///
    /// The transition from running to finished happens here rather than on a thread of its own. A child's
    /// exit is only interesting to whoever asks, the page asks every second while a run is in flight, and a
    /// thread whose whole job is to notice an exit slightly sooner would be a thread to supervise.
    pub fn snapshot(&self) -> RunState {
        let mut inner = self
            .inner
            .lock()
            .expect("the registry mutex is not poisoned");
        match &mut *inner {
            Inner::Idle => RunState::Idle,
            Inner::Finished(state) => (**state).clone(),
            Inner::Running(active) => {
                if active.running.exit_status().is_none() {
                    return RunState::Running {
                        run_id: active.run_id.clone(),
                        started_ms: active.started_ms,
                        request: active.request.clone(),
                        phase: active.running.phase().map(PhaseReport::from),
                    };
                }
                let finished = std::mem::replace(&mut *inner, Inner::Idle);
                let Inner::Running(active) = finished else {
                    unreachable!("the arm was just matched as running");
                };
                let state = self.conclude(*active);
                *inner = Inner::Finished(Box::new(state.clone()));
                state
            }
        }
    }

    /// Stop the run in flight, or say there was none.
    pub fn cancel(&self) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .expect("the registry mutex is not poisoned");
        let Inner::Running(active) = &mut *inner else {
            bail!("no benchmark is running");
        };
        if active.running.exit_status().is_some() {
            bail!("the benchmark has already finished");
        }
        active.cancelled = true;
        active.running.kill();
        self.sink.log(
            Level::Info,
            "runs",
            "the dashboard stopped the running benchmark".to_string(),
        );
        Ok(())
    }

    /// End a run in flight because the daemon is stopping.
    ///
    /// Called on the way out. A benchmark outliving the daemon that started it would keep loading the machine
    /// with nothing left to report its progress to, and would write a report into a directory whose owner has
    /// gone.
    pub fn shutdown(&self) {
        let mut inner = self
            .inner
            .lock()
            .expect("the registry mutex is not poisoned");
        if let Inner::Running(active) = &mut *inner
            && active.running.exit_status().is_none()
        {
            active.cancelled = true;
            active.running.kill();
            self.sink.log(
                Level::Warn,
                "runs",
                "the daemon is stopping, so the benchmark it was running was ended".to_string(),
            );
        }
    }

    /// Turn a finished child into the state the page will read.
    fn conclude(&self, active: Active) -> RunState {
        let Active {
            run_id,
            started_ms,
            request,
            running,
            cancelled,
            report_path,
        } = active;
        let (exit_code, stderr) = running.finish();
        // Success is the exit code *and* the file. A child that returned zero without leaving a report has
        // not produced anything the compare page can be pointed at, and reporting that as a success would
        // send the user looking for a file that is not there.
        let wrote_report = report_path.is_file();
        let markdown = report_path.with_extension("md");
        let ok = exit_code == Some(0) && wrote_report && !cancelled;
        if !ok {
            self.sink.log(
                if cancelled { Level::Info } else { Level::Warn },
                "runs",
                match (cancelled, exit_code) {
                    (true, _) => {
                        "the dashboard's benchmark was stopped before it finished".to_string()
                    }
                    (false, Some(code)) => {
                        format!("the dashboard's benchmark exited with status {code}")
                    }
                    (false, None) => {
                        "the dashboard's benchmark was ended by the operating system".to_string()
                    }
                },
            );
        }
        RunState::Finished {
            run_id,
            started_ms,
            ended_ms: crate::watch::store::now_ms(),
            request,
            ok,
            exit_code,
            report_path: wrote_report.then(|| report_path.display().to_string()),
            markdown_path: markdown.is_file().then(|| markdown.display().to_string()),
            cancelled,
            stderr,
        }
    }
}

/// One preset's published limits.
fn preset_option(preset: Preset) -> PresetOption {
    let limits = preset.limits();
    PresetOption {
        name: preset.name(),
        duration_limit_seconds: limits.duration_limit.as_secs(),
        minimum_duration_seconds: limits.minimum_duration.as_secs(),
        disk_limit_bytes: limits.disk_limit,
        memory_cap_bytes: limits.memory_cap,
        small_files: limits.small_files,
        sqlite_rows: limits.sqlite_rows,
    }
}

/// A rough age, for a refusal that has to name the run already in flight.
fn describe_age(age_ms: i64) -> String {
    let seconds = Duration::from_millis(age_ms.max(0) as u64).as_secs();
    match seconds {
        0..=90 => format!("{seconds}s ago"),
        _ => format!("{}m ago", seconds / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::Store;

    /// A registry wired to a real store's sink, since it logs.
    struct Fixture {
        temp: tempfile::TempDir,
        store: Store,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let store = Store::open(
                &temp.path().join("watch.db"),
                &crate::model::Inventory {
                    hostname_hash: "hash-runs".into(),
                    ..Default::default()
                },
            )
            .unwrap();
            Self { temp, store }
        }

        fn registry(&self) -> Arc<Registry> {
            Registry::new(self.temp.path(), self.store.sink()).expect("a registry")
        }
    }

    #[test]
    fn a_fresh_registry_is_idle_and_publishes_every_preset() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        assert!(matches!(registry.snapshot(), RunState::Idle));

        let options = registry.options();
        assert!(options.allowed);
        assert!(options.refusal.is_none());
        let names: Vec<&str> = options.presets.iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["quick", "standard", "stress"]);
        assert_eq!(options.phase_count, PHASE_COUNT);
        assert_eq!(options.max_cost_cap_usd, MAX_COST_CAP_USD);
        // The limits come from the presets themselves rather than from this file.
        let quick = &options.presets[0];
        assert_eq!(quick.duration_limit_seconds, 45);
        assert_eq!(quick.small_files, 500);
    }

    /// A daemon that will not start runs still describes the form, and says why it is disabled.
    #[test]
    fn refused_options_still_describe_the_form() {
        let options = Registry::refused_options("runs are disabled in watch.toml");
        assert!(!options.allowed);
        assert_eq!(options.refusal, Some("runs are disabled in watch.toml"));
        assert_eq!(options.presets.len(), 3);
    }

    #[test]
    fn cancelling_when_nothing_is_running_is_refused_rather_than_silent() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let error = registry.cancel().unwrap_err().to_string();
        assert!(error.contains("no benchmark is running"), "{error}");
    }

    /// Shutting down an idle registry is a no-op, and must not panic.
    #[test]
    fn shutting_down_an_idle_registry_does_nothing() {
        let fixture = Fixture::new();
        fixture.registry().shutdown();
    }

    #[test]
    fn an_age_is_described_in_whole_seconds_then_whole_minutes() {
        assert_eq!(describe_age(0), "0s ago");
        assert_eq!(describe_age(45_000), "45s ago");
        assert_eq!(describe_age(600_000), "10m ago");
        // A clock that went backwards must not produce a negative age.
        assert_eq!(describe_age(-5_000), "0s ago");
    }

    /// A run that exits without writing a report is a failure, however it exited.
    ///
    /// Driven through the real registry with a child that cannot possibly produce one: `--version` returns
    /// zero and writes nothing, which is exactly the shape of the bug this rule exists for.
    #[test]
    fn a_child_that_writes_no_report_is_not_reported_as_a_success() {
        let fixture = Fixture::new();
        let registry = fixture.registry();
        let request = BenchRequest::default()
            .validate(fixture.temp.path())
            .expect("valid");

        // Start through the registry, then replace the child's arguments by starting a second registry
        // pointed at a program that exits immediately. Done by hand rather than through `start`, because
        // `start` deliberately only ever runs `bench`.
        let running = Running::spawn(
            &std::env::current_exe().unwrap(),
            &["--version".to_string()],
        )
        .expect("spawn");
        let active = Active {
            run_id: "run-under-test".into(),
            started_ms: crate::watch::store::now_ms(),
            request: request.summary(),
            running,
            cancelled: false,
            report_path: fixture.temp.path().join("never-written.json"),
        };
        let state = registry.conclude(active);
        match state {
            RunState::Finished {
                ok, report_path, ..
            } => {
                assert!(!ok, "no report means the run did not succeed");
                assert!(report_path.is_none());
            }
            other => panic!("expected a finished run, got {other:?}"),
        }
    }
}
