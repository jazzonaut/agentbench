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
- `agentbench dashboard --status` for checking collection health, row counts, imported transcripts,
  and recent daemon events without starting anything.
- `watch.toml` configuration, written with commented defaults on first run, overridable per run by
  `--port`, `--data-dir`, `--no-serve`, `--sample-interval`, `--sample-interval-idle`,
  `--probe-interval`, `--no-sessions`, and `--sessions-root`.
- Single-instance locking so two collectors cannot double-count the same machine.
- `docs/adr/` recording architectural decisions and their rejected alternatives.

### Changed

- **Breaking:** the minimum supported Rust version is now 1.86, raised from 1.85. Required by ratatui
  0.30, which is the first ratatui release built against crossterm 0.29; earlier versions require
  crossterm 0.28 and would link a second copy of crossterm alongside this project's, with two
  independent owners of terminal raw mode.
- **Breaking:** `agentbench dashboard` now starts the background collector and web dashboard. The live
  terminal view moved to `agentbench top`. Passing the old `--pid`, `--name`, or `--interval-ms` flags
  to `dashboard` still works for this release and prints a notice pointing at `top`; the shim is
  removed in 0.5.0.
- `bench` internals split into `bench/` with one module per workload, so a workload can be reused
  independently of a preset. No change to emitted metrics.
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
