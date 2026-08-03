# Changelog

All notable changes to AgentBench are documented here. The project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `agentbench dashboard` background collector: continuous passive sampling of CPU, memory, swap,
  process count, security-scanner CPU, and coding-agent process-tree attribution, stored in a
  versioned local SQLite database.
- Loopback-only web dashboard with live tiles and an interactive history chart, all assets embedded so
  it works with no network access.
- Real session metrics imported from local Claude Code transcripts: tool latency, prompt-to-first-
  response intervals, token counts and cache hit ratios, with the whole existing history backfilled on
  first run so the charts start full rather than empty. Nothing is read but timings, token counts,
  model names, project paths and branches; prompts, code and tool output are skipped.
- Read-only file-tool latency (`Read`, `Grep`, `Glob`, `Edit`) charted alongside system CPU, with one
  cursor shared across both charts so a slow afternoon can be read down a single vertical line.
- A "Today" summary on the dashboard: requests, tool calls, sessions, projects, output tokens, cache
  hit rate and median file-tool latency, counted since local midnight.
- Background capability probes: a controlled micro-workload every 15 minutes — single-thread CPU,
  memory bandwidth, an 8 MiB sequential write, 200 small-file operations, 2,000 SQLite rows, five
  process launches, loopback TCP, and one HTTPS round trip — costing about 0.17% of the machine. They
  reuse the `bench` workload functions at micro scale and emit the same metric names, so a threshold
  written once applies to both. Probe and benchmark values are stored side by side under different
  sources and are never averaged together: the same workload over 200 files and over 5,000 answers the
  same question at scales two orders of magnitude apart.
- Every probe is stamped with what the machine was competing with — CPU, security-scanner CPU, whether
  a coding agent was working, and whether the machine is on battery — read both immediately before and
  immediately after the measurement, so a probe clobbered halfway through is not filed as a clean one.
  Probing is never gated on an idle machine; contention is recorded at collection time and excluded at
  analysis time, which is what the dashboard's "uncontended probes only" filter does.
- A probe chart on the dashboard and a tile reporting when the last probe ran and whether it was
  contended, sharing the cursor with the system and tool-latency charts.
- Run markers: `bench`, `profile` and `experiment` record when they started and finished in the
  dashboard database, so the cliff a three-minute benchmark puts in the passive series is explained
  rather than mistaken for a machine getting slower. A benchmark also contributes its metrics, under
  the same names as the probes and a `bench` source. Entirely silent and entirely optional — nothing
  creates a database, so a machine that has never started the dashboard is unaffected.
- `agentbench dashboard --status` for checking collection health, row counts, probe runs and how many
  of them were uncontended, marked runs, imported transcripts, and recent daemon events without
  starting anything.
- `watch.toml` configuration, written with commented defaults on first run, overridable per run by
  `--port`, `--data-dir`, `--no-serve`, `--sample-interval`, `--sample-interval-idle`,
  `--probe-interval`, `--no-probes`, `--no-probe-network`, `--no-sessions`, and `--sessions-root`.
- The probe's one outbound request — an HTTPS round trip to `api.anthropic.com`, no prompt, no
  credentials, no cost — has its own switch, `probe_network` / `--no-probe-network`. It is the only
  part of the daemon that leaves the machine, and 96 requests a day in a tool that otherwise uploads
  nothing is worth being able to turn off on its own.
- Single-instance locking so two collectors cannot double-count the same machine.
- `docs/adr/` recording architectural decisions and their rejected alternatives.

### Changed

- **Breaking:** the minimum supported Rust version is now 1.88, raised from 1.85. Two reasons. `let`
  chains, which this crate uses, stabilised for edition 2024 in 1.88 — so 1.85 through 1.87 never
  actually compiled it, and the previously declared 1.86 was a claim no build had ever verified.
  Separately, ratatui 0.30 requires 1.86: it is the first ratatui release built against crossterm 0.29,
  and earlier versions require crossterm 0.28 and would link a second copy of crossterm alongside this
  project's, with two independent owners of terminal raw mode. CI now checks the declared version on
  every push, reading it from `Cargo.toml` so the two cannot drift.
- **Breaking:** `agentbench dashboard` now starts the background collector and web dashboard. The live
  terminal view moved to `agentbench top`. Passing the old `--pid`, `--name`, or `--interval-ms` flags
  to `dashboard` still works for this release and prints a notice pointing at `top`; the shim is
  removed in 0.5.0.
- Dependencies updated: rusqlite 0.32 to 0.40, sysinfo 0.33 to 0.38, toml 0.8 to 1.1, rand 0.9 to
  0.10, sha2 0.10 to 0.11. Two needed source changes: rusqlite no longer accepts `u64` for a column
  SQLite stores as a signed 64-bit integer, and `System::physical_core_count` became an associated
  function. Hashed machine identity is unchanged, so existing databases and previously exported
  reports remain comparable.
- `bench` internals split into `bench/` with one module per workload, so a workload can be reused
  independently of a preset. No change to emitted metrics.
- The CPU, process-launch and loopback workloads take their scale as a parameter, so the background
  prober can ask the same questions far more cheaply — one core for 200 ms instead of every core for
  five seconds, five child processes instead of ten, 1 MiB through the socket instead of 16. The
  benchmark's own numbers are unchanged.
- Metric names, units, directions, and descriptions consolidated into a single `metrics` catalog,
  replacing string literals duplicated across benchmarking, comparison, and diagnosis.
- Process-tree selection and resource aggregation consolidated into one `process_tree` module,
  replacing separate implementations in the profiler and the terminal view.

### Fixed

- The first CPU reading of a collection session no longer records a spurious 100%. `sysinfo` needs two
  refreshes to compute a delta, so the sampler now primes and discards a throwaway reading.
- Lowering `--sample-interval` now lowers the idle sampling cadence proportionally, instead of leaving
  a quiet machine at its slow default and appearing to ignore the override.

## [0.3.0] - 2026-07-31

### Added

- Cross-platform CPU, memory, filesystem, SQLite, process, loopback, and HTTPS benchmarks.
- Paid live-Claude latency, streaming-throughput, and tool-driven file-search scenarios.
- Automatic paired direct and Headroom routes with cost caps and correctness checks.
- Live terminal dashboard and process-tree profiling.
- Privacy-safe JSON and Markdown reports with offline machine comparison.
- Evidence-ranked diagnoses for system, network, security-scanner, and proxy bottlenecks.
- Tag-driven Windows, Linux, macOS Intel, and macOS Apple Silicon GitHub releases.

[0.3.0]: https://github.com/jazzonaut/agentbench/releases/tag/v0.3.0
