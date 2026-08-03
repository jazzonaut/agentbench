//! What collectors send to the writer.
//!
//! Collectors never touch a connection. They construct a [`Record`] and hand it over a channel, which
//! keeps writing single-threaded, makes batching natural, and means no collector can block another on
//! a lock.

use crate::model::Metric;
use serde::Serialize;

/// Severity of an operational event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// One passive observation of the machine, stamped with absolute wall-clock time.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sample {
    /// Milliseconds since the Unix epoch.
    pub ts: i64,
    pub cpu_percent: f32,
    pub used_memory: u64,
    pub total_memory: u64,
    pub used_swap: u64,
    pub process_count: u64,
    pub scanner_cpu: Option<f32>,
    pub agent_cpu: Option<f32>,
    pub agent_rss: Option<u64>,
    pub agent_processes: Option<u64>,
}

/// What the machine looked like when a measurement started.
///
/// Probing is not gated on an idle machine — every probe runs on schedule and carries this instead, so
/// that a busy day produces data rather than a hole. The filtering happens at analysis time, which is
/// only possible if every run says what it was competing with.
///
/// Every field describes the moment *before* the measurement began, and only that moment. Something that
/// starts half a second into a probe is missed, which is a real limitation and a deliberate one: any
/// reading taken during or after the measurement has a CPU delta spanning it, and so reports the probe's
/// own footprint — and the scanner activity the probe's own writes provoked — as contention.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Covariates {
    /// Whole-machine CPU use immediately before the measurement, on a 0–100 scale across every core.
    pub cpu_percent: Option<f32>,
    /// CPU use of security scanners, or absent when none were found.
    ///
    /// Per *core*, not per machine: `sysinfo` reports process CPU as a percentage of one core, so a tree
    /// of them runs to 100 × cores. Not comparable to [`cpu_percent`] without dividing by the core count.
    ///
    /// [`cpu_percent`]: Covariates::cpu_percent
    pub scanner_percent: Option<f32>,
    /// Whether a coding agent was doing work.
    pub agent_active: bool,
    /// Whether anything was already competing for the machine when this started.
    pub contended: bool,
    /// Absent where the platform will not say. Never guessed: a laptop on battery runs slower for a
    /// reason that has nothing to do with the machine degrading.
    pub on_battery: Option<bool>,
}

/// One measurement within a run: a metric name, its value, and where it came from.
///
/// `source` is what keeps two incomparable measurements out of one average. A probe's
/// `filesystem.small_file_ops_s` is 200 files; a benchmark's is 5,000. Same question, same name, same
/// threshold — wildly different absolute numbers, so they share a table and never a series.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProbeMetric {
    pub name: String,
    pub value: f64,
    pub unit: String,
    pub lower_is_better: bool,
    pub source: MetricSource,
}

impl ProbeMetric {
    /// Reduce a [`Metric`] to the one number a trend is drawn from.
    ///
    /// A distribution contributes its median rather than its mean. `Metric::value` is the mean, which is
    /// the right thing in a report that prints p50, p95 and max beside it; here there is one column, and
    /// a single slow SQLite lookup out of a hundred must not become the fifteen-minute reading. The
    /// median convention is `Metric::distribution`'s own, so a p50 on this chart and a p50 in a report
    /// mean the same thing.
    pub fn from_metric(metric: &Metric, source: MetricSource) -> Self {
        Self {
            name: metric.name.clone(),
            value: metric.p50.unwrap_or(metric.value),
            unit: metric.unit.clone(),
            lower_is_better: metric.lower_is_better,
            source,
        }
    }
}

/// Which kind of run produced a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricSource {
    /// A micro-scale background probe, four times an hour.
    Probe,
    /// A foreground `bench` run, at full scale.
    Bench,
}

impl MetricSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Bench => "bench",
        }
    }

    /// Parse the wire name used by the dashboard.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "probe" => Some(Self::Probe),
            "bench" => Some(Self::Bench),
            _ => None,
        }
    }
}

/// One probe run: the covariates, and every metric it measured.
///
/// The metrics travel with the run rather than as separate records because a metric row is meaningless
/// without the run row it hangs off, and the run's identity is a rowid the writer only learns on
/// insert. Keeping them together means the writer never has to correlate anything.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProbeRun {
    pub ts: i64,
    pub covariates: Covariates,
    pub metrics: Vec<ProbeMetric>,
}

/// A foreground run that loaded this machine.
///
/// Recorded whether or not anything is watching, and at both ends: a run that is interrupted still
/// explains the cliff it put in the passive series, which a marker written only on success would not.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunMarker {
    /// The marker's own identity, minted before the run starts.
    ///
    /// Not the report's `run_id`, which does not exist yet: the opening write happens before any
    /// measurement, and the primary key cannot be assigned retroactively. [`report_path`] is what links a
    /// marker to the JSON it produced.
    ///
    /// [`report_path`]: RunMarker::report_path
    pub run_id: String,
    pub kind: String,
    pub preset: Option<String>,
    pub started: i64,
    /// Absent while the run is still going.
    pub ended: Option<i64>,
    /// Stored unhashed, like every other path in this database.
    pub report_path: Option<String>,
}

/// An operational log line from the daemon itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub ts: i64,
    pub level: Level,
    pub source: String,
    pub message: String,
}

/// One Claude API request, as reconstructed from a transcript.
///
/// A turn is the request, not the row. One request emits several assistant rows that each repeat the
/// same *cumulative* usage, so the importer collapses them and `request_id` is what the database
/// enforces uniqueness on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Turn {
    /// Identity of the first assistant row seen for this request.
    pub uuid: String,
    pub request_id: String,
    pub session_id: String,
    pub ts: i64,
    /// Working directory, stored unhashed: local database, loopback-only server.
    pub project: Option<String>,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
    /// Interval from the user's prompt to the first assistant message of this request.
    ///
    /// Present only on the request that answers a prompt directly; a continuation driven by a tool
    /// result has no prompt to measure from.
    pub first_response_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read: i64,
    pub cache_create: i64,
}

/// One tool call, timed from the assistant row that requested it to the row carrying its result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolCall {
    /// Identity of the result row, which makes re-import idempotent.
    pub uuid: String,
    pub ts: i64,
    pub project: Option<String>,
    pub tool: String,
    pub duration_ms: i64,
    /// False for an error, an interruption, or a denied permission. Such a call returns early and
    /// would deflate a latency series, so charts exclude it.
    pub ok: bool,
}

/// A version of an external tool, observed at a point in time.
///
/// Recorded while transcripts are being read because that is the only place the version appears, and
/// re-reading every transcript later to recover it would cost a second full scan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolVersion {
    pub ts: i64,
    pub tool: String,
    pub version: String,
}

/// Housekeeping the writer performs, rather than a row it stores.
///
/// Retention travels the same channel as everything else for one reason: the writer is the only thing in
/// the process that may mutate the database, and a bulk summarise-and-delete issued from another connection
/// is precisely the race that rule exists to make impossible. Sending an instruction keeps the guarantee
/// intact without a second lock or a second connection.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Maintenance {
    /// Summarise and prune raw samples older than this instant.
    ///
    /// An absolute instant rather than an age, so the decision about what "old" means is made once by
    /// whoever holds the configuration, and the writer neither reads it nor has to agree about the clock.
    pub samples_before_ms: i64,
}

/// How far one transcript has been imported.
///
/// `size` is a byte offset, not merely a change detector: transcripts are append-only, so the next
/// pass seeks to it rather than re-reading megabytes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Watermark {
    pub path: String,
    pub size: i64,
    pub mtime: i64,
    pub rows_ok: i64,
    pub rows_error: i64,
}

/// Transcripts whose recorded position is no longer worth keeping.
///
/// `import_watermark` had no delete path at all, and every row in it is loaded into memory at every
/// startup. Claude Code writes one transcript per project per invocation and subagents write their
/// own, so on a long-lived machine the table grows without bound into tens of thousands of rows
/// describing files that were removed months ago.
///
/// Sent as a batch rather than one record per path: a housekeeping pass finds them together, and one
/// row of the queue should not be spent on each.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ForgetWatermarks {
    /// Paths recorded here that are no longer on disk.
    pub paths: Vec<String>,
}

/// Anything the writer can persist.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Sample(Sample),
    Event(Event),
    ProbeRun(ProbeRun),
    RunMarker(RunMarker),
    Turn(Turn),
    ToolCall(ToolCall),
    ToolVersion(ToolVersion),
    Watermark(Watermark),
    ForgetWatermarks(ForgetWatermarks),
    Maintenance(Maintenance),
}

impl Record {
    /// Samples may be dropped under sustained backpressure; nothing else may.
    ///
    /// A dropped sample leaves a visible gap in a series that is about to be re-measured five seconds
    /// later. A dropped session row is lost for good: its watermark advances past it and no later pass
    /// will look at that byte range again. A dropped probe run is worse still — the machine has already
    /// been loaded to produce it, and the next one is fifteen minutes away.
    ///
    /// A maintenance instruction would in fact be harmless to drop, since another follows in an hour. It
    /// is not droppable anyway, because "only a sample may be discarded" is a rule worth being able to
    /// state without exceptions, and waiting a moment once an hour costs nothing.
    pub fn is_droppable(&self) -> bool {
        matches!(self, Self::Sample(_))
    }
}

impl From<Sample> for Record {
    fn from(value: Sample) -> Self {
        Self::Sample(value)
    }
}

impl From<Event> for Record {
    fn from(value: Event) -> Self {
        Self::Event(value)
    }
}

impl From<ProbeRun> for Record {
    fn from(value: ProbeRun) -> Self {
        Self::ProbeRun(value)
    }
}

impl From<RunMarker> for Record {
    fn from(value: RunMarker) -> Self {
        Self::RunMarker(value)
    }
}

impl From<Turn> for Record {
    fn from(value: Turn) -> Self {
        Self::Turn(value)
    }
}

impl From<ToolCall> for Record {
    fn from(value: ToolCall) -> Self {
        Self::ToolCall(value)
    }
}

impl From<ToolVersion> for Record {
    fn from(value: ToolVersion) -> Self {
        Self::ToolVersion(value)
    }
}

impl From<Watermark> for Record {
    fn from(value: Watermark) -> Self {
        Self::Watermark(value)
    }
}

impl From<ForgetWatermarks> for Record {
    fn from(value: ForgetWatermarks) -> Self {
        Self::ForgetWatermarks(value)
    }
}

impl From<Maintenance> for Record {
    fn from(value: Maintenance) -> Self {
        Self::Maintenance(value)
    }
}
