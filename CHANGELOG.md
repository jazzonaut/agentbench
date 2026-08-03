# Changelog

All notable changes to AgentBench are documented here. The project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed — measurement values move

- **`memory.write_gib_s` now reports roughly an order of magnitude more, on both the probe and the
  benchmark.** The cancellation check sat inside the per-byte write loop, which blocked vectorisation,
  so the figure was the branch rather than the machine: 2.4 GiB/s in the old shape against about 28 in
  the new one on the same hardware, writing byte-for-byte identical output. **Values recorded before
  this release are not comparable with values after it.** This also resolves the README's open item —
  the 0.07 GiB/s figure it recorded was a debug build, and neither hypothesis it offered was right.
- A debug build now warns on stderr before it measures anything. Nothing previously stopped
  `cargo run -- bench` writing figures forty times low into the dashboard's history beside a release
  build's.

### Fixed

- A single refused row no longer ends all collection. The writer logged nothing and exited on any
  insert failure, after which every collector's `send` silently returned `false`, the page kept serving,
  and the only report was at process exit. Refused records are now counted, dropped and explained in the
  operational log; only a transaction that cannot be opened or committed stops the thread, and when it
  does it says so in the log and in `/api/status`.
- A collector that panics is caught, logged with its message, and restarted on the same backoff as one
  that returns early. Previously the thread died and nothing said so until shutdown, potentially days
  later, while the dashboard looked healthy.
- `dashboard --status` and its verdicts no longer open the database read-write. Both went through
  `Store::open`, which runs migrations — so running a newer binary's `--status` while an older daemon
  was collecting upgraded the schema underneath it. They now open a read-only connection, report an
  out-of-range schema instead of changing it, and share one connection instead of building two.
- On macOS, background priority is applied per thread with `PRIO_DARWIN_THREAD`. `PRIO_PROCESS` is
  process-wide there, so the sampler was dragging the probe thread down with it and the probe was
  measuring its own throttle. Other Unixes now report the capability as unavailable rather than do the
  same.
- The dashboard refuses requests whose `Host` is not its own loopback address, closing DNS rebinding —
  a page on any origin could otherwise read every endpoint, including real project paths and branch
  names. `X-Frame-Options` and a content security policy are sent alongside the existing `nosniff`.
- `system::power_source` returned nothing on most Linux laptops: a `?` inside the directory loop
  returned from the whole function as soon as a battery was visited before a mains supply, and
  directory order is arbitrary. It now derives from the single reading in `watch::platform`.
- Retention no longer full-scans `samples` three times per chunk. Its statements filter on `ts` alone,
  which the `(machine_id, ts)` primary key cannot serve; migration v4 adds the index.
- Transcript import positions are deleted when the transcript is gone. `import_watermark` had no delete
  path at all, and every row in it is loaded into memory at startup.

### Changed

- The dashboard polls `/api/status` and `/api/verdicts` once a minute rather than every five seconds.
  Between them they cost more than the collectors they report on — six `count(*)` aggregates and a
  re-derived trailing window — so an open page was biasing the series it was drawing. Live tiles are
  unchanged at five seconds.
- `profile` walks the process table once per tick rather than twice, and asks for only the fields it
  reads.
- `profile` no longer retains every chunk of a child's output in memory. It kept an owned `String` per
  8 KiB read, uncapped, for stdout and stderr, where the one consumer wanted a single substring test.
- Transcript discovery reuses the metadata a directory entry already carries, which on Windows removes
  a file open per transcript per pass, and the poll interval floor is 10 seconds rather than 1.
- A verdict computed from a thin partial day says how thin it is: "today rests on N measurements against
  a baseline of about M a day".
- `--llm-route auto` documents that it runs both routes and pays for every scenario twice, and a run
  that reported no cost is now named in the warnings rather than silently omitted from the cap's
  arithmetic.
- Directories under the sessions roots that cannot be listed are reported once, and again when the
  count changes, instead of being counted and never mentioned.

## [0.4.0] - 2026-08-03

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
  a coding agent was working, and whether the machine is on battery — read once, immediately before the
  measurement, so the tag claims only what that measurement began in. Probing is never gated on an idle
  machine; contention is recorded at collection time and excluded at analysis time, which is what the
  dashboard's "uncontended probes only" filter does.
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
- A "Today vs baseline" section on the dashboard, and the same verdicts in `--status`. Each of the
  previous seven local days is reduced to one value — the median of that day's uncontended measurements —
  and today is compared against the median and median absolute deviation of those daily values. Five
  series are judged: small-file operations, sequential write, SQLite lookup latency, single-core CPU, and
  the file-tool latency your agent actually experienced. The confounded series stay charted and unjudged.
- Every verdict reports the evidence behind it: how many days contributed, how many measurements those
  days held, and how many of them ran on battery. A day with fewer than three comparable measurements is
  dropped, fewer than four contributing days produces no verdict rather than a confident one, and a band
  narrower than 5% of its own median is widened to that floor and says so — seven identical days would
  otherwise declare every later day a regression.
- Verdicts state when today's power source disagrees with the baseline's, rather than filtering battery
  runs out. A laptop that lives unplugged still has a capability trend; one unplugged this morning reads
  as degraded for a reason that is not the machine.
- Chart annotations: a dashed rule at the first sighting of each tool version, and a shaded band across
  each `bench`, `profile` or `experiment` run, listed beneath the charts so the frames stay readable.
  Versions come from transcripts, so annotations cover the whole backfilled history. Served by
  `/api/annotations`.
- Sample retention: after `samples_raw_days` (14 by default) each whole minute of passive samples is
  summarised into one row and the raw samples are pruned. Charts cross the boundary transparently — a
  range reaching past it continues out of the summary, and the response reports which part is summarised
  and whether each point is that minute's mean or its peak. Probe runs, session metrics and run markers
  are never pruned.
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
- The percentile convention — `index = round((n - 1) × p)` on sorted values — now lives in one function
  in `model` rather than being hand-rolled in three places. A p50 on a chart, a p50 in a printed report
  and a p50 behind a verdict have to be the same number, and a reader comparing two of them has no way to
  discover that they were not.
- Process-tree selection and resource aggregation consolidated into one `process_tree` module,
  replacing separate implementations in the profiler and the terminal view.

### Fixed

- Sub-millisecond latencies are no longer displayed as "0 ms". A probe's SQLite lookup is four or five
  microseconds on a healthy machine, and the dashboard's latency formatter — written for tool calls in the
  tens of milliseconds — rounded it to zero on a tile the page now judges. Latencies below a millisecond
  are shown in microseconds, and `--status` picks its precision from the value rather than always printing
  one decimal place.
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

[0.4.0]: https://github.com/jazzonaut/agentbench/releases/tag/v0.4.0
[0.3.0]: https://github.com/jazzonaut/agentbench/releases/tag/v0.3.0
