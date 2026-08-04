//! Table definitions.
//!
//! Shape follows each stream. `samples` is wide because its columns are stable and its row rate is
//! high; `probe_metrics` is long because the probe metric set will keep evolving and a migration per
//! metric would be intolerable. Every fact table carries `machine_id` from the outset: adding it
//! later would mean rewriting every table and every query.
//!
//! **Migrations v1–v5 were collapsed into this one statement before 0.7.0.** They corrected the session
//! tables' names and keys, widened `samples_1m`, added the index retention scans on, and re-keyed
//! `tool_versions`; each is recorded in ADR 0001's deviations, and none can ever run again because no
//! release carried them. Keeping five `ALTER` blocks and their upgrade tests would have meant 270 lines
//! describing a path from a database that exists nowhere. The reasoning was moved, not deleted — the
//! rule stays that an entry here is immutable once a release has shipped it.

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
--
-- The two write-rate columns are nullable for a reason that is not "the value might be missing": a
-- newly discovered process's first I/O delta is its whole lifetime's traffic, so the tick after every
-- discovery pass has no rate to report and must say so rather than record a zero.
CREATE TABLE samples (
    machine_id            TEXT NOT NULL REFERENCES machines(id),
    ts                    INTEGER NOT NULL,
    cpu_percent           REAL    NOT NULL,
    used_memory           INTEGER NOT NULL,
    total_memory          INTEGER NOT NULL,
    used_swap             INTEGER NOT NULL,
    process_count         INTEGER NOT NULL,
    scanner_cpu           REAL,
    agent_cpu             REAL,
    agent_rss             INTEGER,
    agent_processes       INTEGER,
    agent_write_bytes_s   REAL,
    scanner_write_bytes_s REAL,
    PRIMARY KEY (machine_id, ts)
) WITHOUT ROWID;

-- `samples` is `WITHOUT ROWID` on `(machine_id, ts)`, which answers every read the dashboard makes --
-- they all name a machine. Retention deliberately does not: it prunes the whole file rather than this
-- daemon's own rows, so that a database carried over from an old machine is not kept for ever by a
-- daemon that will never recognise it. That makes its three statements -- `min(ts)` before the cutoff,
-- the rollup aggregate, and the delete -- filter on `ts` alone, which without this index is a full scan
-- each, and a first pass over a fortnight of backlog runs fourteen chunks of three of them.
--
-- One index on the highest-rate table is the cost. It is the right trade precisely because that table is
-- pruned to a fortnight: the index never grows past a fortnight either.
CREATE INDEX idx_samples_ts ON samples(ts);

-- Every series the dashboard advertises has a column here, which is not a tidiness point: a series
-- missing one keeps its history to the retention boundary and then stops dead, with nothing on the page
-- to explain why the chart beside it did not.
--
-- The reducer differs per series and is a decision rather than a convention. Memory in use keeps its
-- average, because the average is what the machine was living with; swap, scanner CPU and the write
-- rates keep their peak, because a thirty-second burst of any of those *is* the event and its mean over
-- a minute hides it.
CREATE TABLE samples_1m (
    machine_id                TEXT NOT NULL REFERENCES machines(id),
    bucket                    INTEGER NOT NULL,
    samples                   INTEGER NOT NULL,
    cpu_avg                   REAL    NOT NULL,
    cpu_max                   REAL    NOT NULL,
    used_memory_avg           INTEGER NOT NULL,
    used_swap_max             INTEGER NOT NULL,
    scanner_cpu_max           REAL,
    agent_cpu_max             REAL,
    process_count_avg         INTEGER,
    agent_rss_max             INTEGER,
    agent_write_bytes_s_max   REAL,
    scanner_write_bytes_s_max REAL,
    PRIMARY KEY (machine_id, bucket)
) WITHOUT ROWID;

-- What the machine was doing when a probe began. Every covariate is nullable except the two booleans,
-- because each is a capability some platform declines to provide and a guess would be worse than a gap.
--
-- `agent_at` is stored beside `agent_active` although one is derived from the other. `agent_active` is a
-- threshold applied at write time, and the threshold is the part of this design most likely to change;
-- without the raw figure every row collected under the old constant would be impossible to reclassify.
CREATE TABLE probe_runs (
    id                 INTEGER PRIMARY KEY,
    machine_id         TEXT NOT NULL REFERENCES machines(id),
    ts                 INTEGER NOT NULL,
    contended          INTEGER NOT NULL,
    cpu_at             REAL,
    scanner_at         REAL,
    agent_at           REAL,
    agent_active       INTEGER NOT NULL,
    on_battery         INTEGER,
    clock_percent      REAL,
    disk_write_bytes_s REAL,
    scratch_free_bytes INTEGER
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

-- The largest consumers when a probe began: what the covariates cannot say. A handful of rows per run,
-- cascading with it, and read only when a reader asks what was competing.
CREATE TABLE probe_processes (
    run_id      INTEGER NOT NULL REFERENCES probe_runs(id) ON DELETE CASCADE,
    rank        INTEGER NOT NULL,
    name        TEXT    NOT NULL,
    cpu_percent REAL    NOT NULL,
    write_bytes INTEGER NOT NULL,
    PRIMARY KEY (run_id, rank)
) WITHOUT ROWID;

-- A turn is one API request, not one row. Several assistant rows share a `request_id` and each repeats
-- the same *cumulative* usage, so the unique index below is what makes an interrupted import
-- idempotent: resuming mid-file cannot invent a second turn for a request already recorded. Identifying
-- a turn by whichever row was read first is only correct if reading always starts at the top of a
-- request, which a resumed pass does not.
--
-- `first_response_ms` is named for what a transcript can actually yield: the interval to the first
-- assistant *message*, which for a thinking model contains the entire thinking block and has a median
-- around fifteen seconds. It was called `ttft_ms` once, and a column holding 15,000 under that name
-- would have had every chart and future reader explaining that the number does not mean what it says.
CREATE TABLE session_turns (
    uuid              TEXT PRIMARY KEY,
    machine_id        TEXT NOT NULL REFERENCES machines(id),
    ts                INTEGER NOT NULL,
    session_id        TEXT,
    request_id        TEXT,
    project           TEXT,
    branch            TEXT,
    model             TEXT,
    effort            TEXT,
    service_tier      TEXT,
    first_response_ms INTEGER,
    generation_ms     INTEGER,
    sidechain         INTEGER NOT NULL DEFAULT 0,
    input_tokens      INTEGER NOT NULL DEFAULT 0,
    output_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_read        INTEGER NOT NULL DEFAULT 0,
    cache_create      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_session_turns_ts ON session_turns(machine_id, ts);
CREATE UNIQUE INDEX idx_session_turns_request ON session_turns(machine_id, request_id);

CREATE TABLE session_tools (
    uuid        TEXT PRIMARY KEY,
    machine_id  TEXT NOT NULL REFERENCES machines(id),
    ts          INTEGER NOT NULL,
    project     TEXT,
    tool        TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    ok          INTEGER NOT NULL,
    sidechain   INTEGER NOT NULL DEFAULT 0
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

-- One row per version, not one per sighting. The importer sees the running version on nearly every
-- transcript row it reads and its deriver state is per-pass, so a key including `ts` made every poll
-- that read new bytes write another row recording a version that had not changed -- roughly one row per
-- poll while a session is live, and nothing prunes this table. Keying on the version makes the write
-- idempotent in intent as well as in effect, and makes "when was this first seen" a lookup rather than
-- an aggregate over the whole table.
CREATE TABLE tool_versions (
    machine_id TEXT NOT NULL REFERENCES machines(id),
    ts         INTEGER NOT NULL,
    tool       TEXT NOT NULL,
    version    TEXT NOT NULL,
    PRIMARY KEY (machine_id, tool, version)
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
