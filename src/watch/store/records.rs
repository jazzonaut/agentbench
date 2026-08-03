//! What collectors send to the writer.
//!
//! Collectors never touch a connection. They construct a [`Record`] and hand it over a channel, which
//! keeps writing single-threaded, makes batching natural, and means no collector can block another on
//! a lock.

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

/// Anything the writer can persist.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Sample(Sample),
    Event(Event),
    Turn(Turn),
    ToolCall(ToolCall),
    ToolVersion(ToolVersion),
    Watermark(Watermark),
}

impl Record {
    /// Samples may be dropped under sustained backpressure; nothing else may.
    ///
    /// A dropped sample leaves a visible gap in a series that is about to be re-measured five seconds
    /// later. A dropped session row is lost for good: its watermark advances past it and no later pass
    /// will look at that byte range again.
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
