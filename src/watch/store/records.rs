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

/// Anything the writer can persist.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Sample(Sample),
    Event(Event),
}

impl Record {
    /// Samples may be dropped under sustained backpressure; events must not, because they are how a
    /// hidden daemon explains itself.
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
