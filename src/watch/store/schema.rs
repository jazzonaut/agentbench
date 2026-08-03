//! Table definitions.
//!
//! Shape follows each stream. `samples` is wide because its columns are stable and its row rate is
//! high; `probe_metrics` is long because the probe metric set will keep evolving and a migration per
//! metric would be intolerable. Every fact table carries `machine_id` from the outset: adding it
//! later would mean rewriting every table and every query.

/// Statements creating the schema at its current version.
pub const CREATE_V1: &str = r#"
CREATE TABLE machines (
    id             TEXT PRIMARY KEY,
    hostname_hash  TEXT NOT NULL,
    os             TEXT NOT NULL,
    os_version     TEXT NOT NULL,
    architecture   TEXT NOT NULL,
    cpu            TEXT NOT NULL,
    logical_cores  INTEGER NOT NULL,
    memory_bytes   INTEGER NOT NULL,
    first_seen     INTEGER NOT NULL,
    last_seen      INTEGER NOT NULL
);

-- Absolute wall-clock milliseconds since the Unix epoch. Deliberately not a run-relative offset:
-- a daemon outlives any single run, and suspend/resume makes monotonic offsets meaningless.
CREATE TABLE samples (
    machine_id      TEXT NOT NULL REFERENCES machines(id),
    ts              INTEGER NOT NULL,
    cpu_percent     REAL    NOT NULL,
    used_memory     INTEGER NOT NULL,
    total_memory    INTEGER NOT NULL,
    used_swap       INTEGER NOT NULL,
    process_count   INTEGER NOT NULL,
    scanner_cpu     REAL,
    agent_cpu       REAL,
    agent_rss       INTEGER,
    agent_processes INTEGER,
    PRIMARY KEY (machine_id, ts)
) WITHOUT ROWID;

CREATE TABLE samples_1m (
    machine_id       TEXT NOT NULL REFERENCES machines(id),
    bucket           INTEGER NOT NULL,
    samples          INTEGER NOT NULL,
    cpu_avg          REAL    NOT NULL,
    cpu_max          REAL    NOT NULL,
    used_memory_avg  INTEGER NOT NULL,
    used_swap_max    INTEGER NOT NULL,
    scanner_cpu_max  REAL,
    agent_cpu_max    REAL,
    PRIMARY KEY (machine_id, bucket)
) WITHOUT ROWID;

CREATE TABLE probe_runs (
    id            INTEGER PRIMARY KEY,
    machine_id    TEXT NOT NULL REFERENCES machines(id),
    ts            INTEGER NOT NULL,
    contended     INTEGER NOT NULL,
    cpu_at        REAL,
    scanner_at    REAL,
    agent_active  INTEGER NOT NULL,
    on_battery    INTEGER
);
CREATE INDEX idx_probe_runs_ts ON probe_runs(machine_id, ts);

CREATE TABLE probe_metrics (
    run_id          INTEGER NOT NULL REFERENCES probe_runs(id) ON DELETE CASCADE,
    name            TEXT    NOT NULL,
    value           REAL    NOT NULL,
    unit            TEXT    NOT NULL,
    lower_is_better INTEGER NOT NULL,
    source          TEXT    NOT NULL,
    PRIMARY KEY (run_id, name, source)
) WITHOUT ROWID;
CREATE INDEX idx_probe_metrics_name ON probe_metrics(name, source, run_id);

CREATE TABLE session_turns (
    uuid          TEXT PRIMARY KEY,
    machine_id    TEXT NOT NULL REFERENCES machines(id),
    ts            INTEGER NOT NULL,
    project       TEXT,
    branch        TEXT,
    model         TEXT,
    effort        TEXT,
    service_tier  TEXT,
    -- Renamed to first_response_ms by v2; see ALTER_V2.
    ttft_ms       INTEGER,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read    INTEGER NOT NULL DEFAULT 0,
    cache_create  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_session_turns_ts ON session_turns(machine_id, ts);

CREATE TABLE session_tools (
    uuid       TEXT PRIMARY KEY,
    machine_id TEXT NOT NULL REFERENCES machines(id),
    ts         INTEGER NOT NULL,
    project    TEXT,
    tool       TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    ok         INTEGER NOT NULL
);
CREATE INDEX idx_session_tools_tool_ts ON session_tools(machine_id, tool, ts);

CREATE TABLE run_markers (
    run_id      TEXT PRIMARY KEY,
    machine_id  TEXT NOT NULL REFERENCES machines(id),
    kind        TEXT NOT NULL,
    preset      TEXT,
    started     INTEGER NOT NULL,
    ended       INTEGER,
    report_path TEXT
);
CREATE INDEX idx_run_markers_started ON run_markers(machine_id, started);

CREATE TABLE tool_versions (
    machine_id TEXT NOT NULL REFERENCES machines(id),
    ts         INTEGER NOT NULL,
    tool       TEXT NOT NULL,
    version    TEXT NOT NULL,
    PRIMARY KEY (machine_id, tool, ts)
) WITHOUT ROWID;

CREATE TABLE import_watermark (
    path      TEXT PRIMARY KEY,
    size      INTEGER NOT NULL,
    mtime     INTEGER NOT NULL,
    rows_ok   INTEGER NOT NULL,
    rows_error INTEGER NOT NULL,
    updated   INTEGER NOT NULL
);

-- Bounded operational log. A daemon hidden by a scheduler has nowhere else to put diagnostics.
CREATE TABLE events (
    id     INTEGER PRIMARY KEY,
    ts     INTEGER NOT NULL,
    level  TEXT NOT NULL,
    source TEXT NOT NULL,
    message TEXT NOT NULL
);
CREATE INDEX idx_events_ts ON events(ts);
"#;

/// Session-stream corrections, applied when the transcript importer arrived.
///
/// Two things the v1 shape got wrong, both learned from real transcripts:
///
/// `ttft_ms` promised a time to first *token*. What a transcript can actually yield is the interval
/// to the first assistant *message*, which for a thinking model contains the entire thinking block —
/// a median of about fifteen seconds against sub-second network latency. Keeping the old name would
/// have every chart and tooltip explaining that the number does not mean what it says.
///
/// A turn is identified by its API request, not by the row that happened to be read first. One
/// request emits several assistant rows, so the unique index is what makes an interrupted import
/// idempotent: resuming mid-file cannot invent a second turn for a request already recorded.
pub const ALTER_V2: &str = r#"
ALTER TABLE session_turns RENAME COLUMN ttft_ms TO first_response_ms;
ALTER TABLE session_turns ADD COLUMN session_id TEXT;
ALTER TABLE session_turns ADD COLUMN request_id TEXT;
CREATE UNIQUE INDEX idx_session_turns_request ON session_turns(machine_id, request_id);
"#;
