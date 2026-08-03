//! Every metric AgentBench emits under a fixed, fully known name.
//!
//! Adding a metric means adding a `const` here and appending it to [`ALL`]. Emit it through
//! [`MetricSpec::scalar`] or [`MetricSpec::distribution`] so the name, unit, direction, and phase
//! cannot drift apart.
//!
//! [`MetricSpec::scalar`]: super::MetricSpec::scalar
//! [`MetricSpec::distribution`]: super::MetricSpec::distribution

use super::MetricSpec;

macro_rules! spec {
    (
        $ident:ident,
        name: $name:literal,
        unit: $unit:literal,
        lower_is_better: $lower:literal,
        phase: $phase:literal,
        informational: $informational:literal,
        description: $description:literal,
    ) => {
        pub const $ident: MetricSpec = MetricSpec {
            name: $name,
            unit: $unit,
            lower_is_better: $lower,
            phase: $phase,
            description: $description,
            informational: $informational,
        };
    };
}

// ---------------------------------------------------------------------------- cpu

spec! {
    CPU_SINGLE_MOPS_S,
    name: "cpu.single_mops_s",
    unit: "Mops/s",
    lower_is_better: false,
    phase: "cpu",
    informational: false,
    description: "Single-thread integer work completed per second; reflects one-core execution speed.",
}

spec! {
    CPU_MULTI_MOPS_S,
    name: "cpu.multi_mops_s",
    unit: "Mops/s",
    lower_is_better: false,
    phase: "cpu",
    informational: false,
    description: "Integer work completed across all logical processors; reflects sustained parallel CPU capacity.",
}

spec! {
    CPU_MULTI_ELAPSED_MS,
    name: "cpu.multi_elapsed_ms",
    unit: "ms",
    lower_is_better: true,
    phase: "cpu",
    informational: true,
    description: "Observed wall time of the fixed-duration multi-core phase; mainly indicates scheduling or shutdown overrun.",
}

// ------------------------------------------------------------------------- memory

spec! {
    MEMORY_WRITE_GIB_S,
    name: "memory.write_gib_s",
    unit: "GiB/s",
    lower_is_better: false,
    phase: "memory",
    informational: false,
    description: "Sequential speed while filling the benchmark memory buffer; affected by CPU, RAM, and power limits.",
}

spec! {
    MEMORY_READ_GIB_S,
    name: "memory.read_gib_s",
    unit: "GiB/s",
    lower_is_better: false,
    phase: "memory",
    informational: false,
    description: "Speed while sampling the benchmark memory buffer; affected by cache hierarchy and memory bandwidth.",
}

// --------------------------------------------------------------------- filesystem

spec! {
    FS_SEQUENTIAL_WRITE_MIB_S,
    name: "filesystem.sequential_write_mib_s",
    unit: "MiB/s",
    lower_is_better: false,
    phase: "filesystem",
    informational: false,
    description: "Large-file write throughput on the selected target volume, including the final flush.",
}

spec! {
    FS_SEQUENTIAL_READ_MIB_S,
    name: "filesystem.sequential_read_mib_s",
    unit: "MiB/s",
    lower_is_better: false,
    phase: "filesystem",
    informational: false,
    description: "Large-file read throughput on the selected target volume; OS filesystem cache may contribute.",
}

spec! {
    FS_SMALL_FILE_OPS_S,
    name: "filesystem.small_file_ops_s",
    unit: "ops/s",
    lower_is_better: false,
    phase: "filesystem",
    informational: false,
    description: "Combined create, metadata-stat, rename, and delete operations per second across many small files.",
}

spec! {
    FS_SMALL_FILE_TOTAL_MS,
    name: "filesystem.small_file_total_ms",
    unit: "ms",
    lower_is_better: true,
    phase: "filesystem",
    informational: false,
    description: "Total wall time for the complete small-file create/stat/rename/delete workload.",
}

spec! {
    FS_SUSTAINED_SEEK_OPS_S,
    name: "filesystem.sustained_seek_ops_s",
    unit: "ops/s",
    lower_is_better: false,
    phase: "filesystem",
    informational: false,
    description: "Repeated small-file metadata and read operations during the preset duration-filling phase.",
}

// ------------------------------------------------------------------------- sqlite

spec! {
    SQLITE_INSERT_ROWS_S,
    name: "sqlite.insert_rows_s",
    unit: "rows/s",
    lower_is_better: false,
    phase: "sqlite",
    informational: false,
    description: "Rows inserted per second into the generated indexed SQLite database in one transaction.",
}

spec! {
    SQLITE_LOOKUP_MS,
    name: "sqlite.lookup_ms",
    unit: "ms",
    lower_is_better: true,
    phase: "sqlite",
    informational: false,
    description: "Latency of indexed point lookups in the generated SQLite database.",
}

// ------------------------------------------------------------------------ process

spec! {
    PROCESS_SPAWN_MS,
    name: "process.spawn_ms",
    unit: "ms",
    lower_is_better: true,
    phase: "process",
    informational: false,
    description: "Time to launch and complete a minimal child AgentBench process.",
}

// ------------------------------------------------------------------------ network

spec! {
    NETWORK_LOOPBACK_CONNECT_MS,
    name: "network.loopback_connect_ms",
    unit: "ms",
    lower_is_better: true,
    phase: "network",
    informational: false,
    description: "TCP connection setup latency through the local operating-system network stack.",
}

spec! {
    NETWORK_LOOPBACK_MIB_S,
    name: "network.loopback_mib_s",
    unit: "MiB/s",
    lower_is_better: false,
    phase: "network",
    informational: false,
    description: "TCP throughput over localhost; exercises CPU, memory copies, and the OS network stack without internet variability.",
}

spec! {
    NETWORK_HTTPS_LATENCY_MS,
    name: "network.https_latency_ms",
    unit: "ms",
    lower_is_better: true,
    phase: "network",
    informational: false,
    description: "End-to-end HTTPS request latency to the public Anthropic endpoint, including network and TLS effects.",
}

// ----------------------------------------------------------------------- live llm

spec! {
    LLM_TOTAL_COST_USD,
    name: "llm.total_cost_usd",
    unit: "USD",
    lower_is_better: true,
    phase: "live_llm",
    informational: true,
    description: "Total provider-reported cost of all live cases completed in this run; depends on run count and is informational.",
}

spec! {
    LLM_PHASE_WALL_SECONDS,
    name: "llm.phase_wall_seconds",
    unit: "s",
    lower_is_better: true,
    phase: "live_llm",
    informational: true,
    description: "Wall time spent in the live-LLM phase; depends on how many cases fit in the preset budget and is informational.",
}

/// Every catalogued spec, in emission order per phase.
pub const ALL: &[MetricSpec] = &[
    CPU_SINGLE_MOPS_S,
    CPU_MULTI_MOPS_S,
    CPU_MULTI_ELAPSED_MS,
    MEMORY_WRITE_GIB_S,
    MEMORY_READ_GIB_S,
    FS_SEQUENTIAL_WRITE_MIB_S,
    FS_SEQUENTIAL_READ_MIB_S,
    FS_SMALL_FILE_OPS_S,
    FS_SMALL_FILE_TOTAL_MS,
    FS_SUSTAINED_SEEK_OPS_S,
    SQLITE_INSERT_ROWS_S,
    SQLITE_LOOKUP_MS,
    PROCESS_SPAWN_MS,
    NETWORK_LOOPBACK_CONNECT_MS,
    NETWORK_LOOPBACK_MIB_S,
    NETWORK_HTTPS_LATENCY_MS,
    LLM_TOTAL_COST_USD,
    LLM_PHASE_WALL_SECONDS,
];
