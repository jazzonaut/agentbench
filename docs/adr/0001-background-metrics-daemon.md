# ADR 0001: Background metrics daemon with an HTTP dashboard

- Status: accepted
- Date: 2026-08-03
- Supersedes: none

## Context

AgentBench answers "why is a coding agent fast on one machine and slow on another?" through
foreground, load-generating runs: `bench` spins CPU, writes hundreds of MiB, exercises memory and
SQLite, and optionally makes paid Claude calls. Every run is a deliberate, supervised event that
produces one JSON report.

That shape cannot answer a different and equally practical question: *"is my machine slower today
than it was yesterday, and what changed?"* Answering it needs a continuous record rather than
occasional snapshots, and it needs that record to exist before the day you start investigating.

Two constraints shape the whole design and pull against each other:

1. Collection must not meaningfully degrade the machine it observes.
2. A comparable day-over-day number requires an identical controlled workload, which by definition
   consumes resources.

A third constraint is inherited: the tool must stay local. It uploads nothing, opens no inbound
network surface today, and hashes host and path identifiers in every exported artefact.

## Decision

Add an `agentbench dashboard` daemon that collects three complementary streams into a local SQLite
database and serves a loopback web dashboard with live tiles, historical charts, and day-over-day
verdicts.

### Streams

| Stream | Source | Cost | Answers |
|---|---|---|---|
| `samples` | `sysinfo` counters, selectively refreshed | negligible | what is competing for the machine right now |
| `probes` | micro-scale reuse of `bench` workloads | ~0.17% duty cycle | is the machine's capability drifting |
| `sessions` | Claude Code transcript JSONL under `~/.claude/projects` | zero | what real agent performance actually was |

The session stream is the highest-value component and is available retroactively: hundreds of
existing transcripts mean the dashboard ships with months of real history rather than being empty
until data accrues.

### Numbered decisions

| # | Decision | Rationale |
|---|---|---|
| 1 | Collect all three streams | Passive data explains *why*; probes detect capability drift; sessions measure the real thing rather than a proxy |
| 2 | Full transcript backfill, curated metric set | Instant history; only low-confound metrics are charted by default |
| 3 | One process, four threads (sampler, prober, tailer, http) | No IPC, one lifecycle, matches the existing std-threads idiom |
| 4 | `tiny_http` | Blocking, ~4 transitive deps, no async runtime in a codebase that has none |
| 5 | Loopback-only, no auth, plaintext paths in the local DB | Anything reaching loopback can already read the file; exports stay hashed |
| 6 | Selective refresh, per-thread background priority, prober at normal priority | Politeness must not corrupt the measurement |
| 7 | Micro-scale probes, never a paid call | Sessions already supply real LLM timings free |
| 8 | Ungated probing with covariate tagging | Uniform collection; contention is filtered at analysis time |
| 9 | 15-minute cadence, configurable | 96 points/day is ample; a third of the cost of 5 minutes |
| 10 | Wide `samples`, long `probe_metrics`, derived session tables | Shape follows each stream's stability and row rate |
| 11 | Vendored uPlot, JSON polling | Offline-capable; cursor sync is the key correlation feature |
| 12 | Trailing 7-day median/MAD band, plus version annotations | Robust to outliers; annotations turn "it dropped" into "it dropped when X changed" |
| 13 | `dashboard` becomes the web daemon; the TUI becomes `top` | The web UI is the natural owner of the name; pre-1.0 permits it |
| 14 | Foreground process, documented autostart, `--status`, instance lock | Preserves the no-OS-changes promise |
| 15 | Run markers always; bench metrics as a separate `source` | Explains cliffs in the passive series without ever averaging incomparable measurements |
| 16 | Nested `src/watch/` layered by responsibility | ~3,000 lines cannot live in five flat files |
| 17 | Single-writer channel, `Store`/`Reader` split, internal `Req`/`Resp`, injected `Clock`, thread supervision, `events` table | Compile-time guarantees and deterministic, sleep-free tests |
| 18 | Prep commit: `bench/` split and shared `metrics/` module | The feature forces these files open anyway |
| 19 | `watch.toml` with CLI overrides | A scheduled task cannot carry ad-hoc flags |
| 20 | Single page: Now / Today-vs-baseline / cursor-synced history | Correlation by reading down one vertical line |
| 21 | `machine_id` from day one, single-machine UI | Cheap now, expensive to retrofit across every table and query |
| 22 | Six phases, tracer bullet first, sessions before probes | Proves the spine early; real backfilled data makes later phases easier to build against |

### Derived session metrics

Transcripts carry no duration or TTFT fields. The following are derived from row timestamps:

| Metric | Derivation | Confound |
|---|---|---|
| tool latency | `assistant(tool_use).timestamp` → matching `user.toolUseResult.timestamp` via `sourceToolAssistantUUID` | permission waits inflate it |
| TTFT proxy | user prompt row → first assistant row | includes local CLI overhead |
| tokens, cache ratio | `usage`, **deduped by `requestId`** | none once deduped |

Read-only tool latency (`Read`, `Grep`, `Glob`, `Edit`) is the clean filesystem signal and is charted
by default. `Bash` latency is stored but not charted by default: it is dominated by permission waits
and legitimately long commands, so unfiltered it measures time away from the keyboard.

One API request emits several assistant rows sharing a `requestId`, each repeating the *cumulative*
`usage`. Summing naively multiplies token counts. Dedupe by `requestId` before aggregating.

## Consequences

### Accepted costs

- A listening socket exists where previously there were none. Mitigated by hard loopback binding.
- The local database stores real project paths and branch names, unlike every exported artefact.
  This split is deliberate and must be documented, and any future export must hash them.
- Probing without an idle gate induces ~768 MiB/day of writes and ~19k file creates/day, and some
  probes will land mid-agent-turn.
- Tagging contention rather than gating on it relocates data sparsity from collection time to
  analysis time. On busy days the uncontended subset shrinks, so verdicts must report the sample
  count behind each baseline and decline to compute a band from too few points.
- `dashboard` changes meaning. A hidden alias covers the 0.4.x line, then is removed.

### Rejected alternatives

- **Passive observation only.** Cannot distinguish "the disk got slower" from "the disk got busier".
- **`axum` + `tokio`.** Roughly 30 transitive crates and a second concurrency model, for a handful
  of local JSON endpoints.
- **A charting CDN.** Breaks offline operation and contradicts the tool's posture.
- **Whole-process background priority.** Would make probe values incomparable to `bench` output and
  exaggerate degradation under load.
- **Running `quick` on a schedule instead of micro-probes.** 45 s of real load per hour is not
  background collection.
- **A paid LLM canary probe.** Pays money for a worse version of what the session stream provides.
- **A true system service.** Requires administrator rights to install and has no business running
  when the user whose transcripts and interactive session it measures is not logged in.

### Implementation hazards

1. Day bucketing must use local time; UTC buckets produce wrong days. Two days a year are 23/25
   hours under DST.
2. `SystemSample.elapsed_ms` is relative to a run start and is unusable for a daemon. Store absolute
   wall-clock timestamps, and do not interpolate charts across gaps larger than ~2× the sample
   interval or a night of sleep draws a straight line through nothing.
3. The scratch directory must sit on the volume whose performance matters, or probes measure the
   wrong disk.
4. `events` cannot record a failure to open the database that holds it; a stderr fallback is required
   before the store is available.
5. `MetricName` must accommodate genuinely dynamic names (`llm.{route}.{scenario}.*`,
   `tool.{name}_startup_ms`), so a closed enum is insufficient.
6. The instance lock uses `libc`/`windows-sys` rather than `File::try_lock`, which stabilised after
   the declared MSRV.
7. The probe issues ~96 outbound HTTPS requests/day to `api.anthropic.com`. Not telemetry, but it
   warrants explicit documentation and a configuration switch.

## Phases

| Phase | Content | Status |
|---|---|---|
| 0 | Prep refactor: split `bench.rs` into `bench/`, introduce `metrics/`. No behaviour change | **done** |
| 1 | Spine: config, store with migrations, sampler, `events`, `--status`, one live tile and one chart | **done** |
| 2 | Sessions: transcript parsing, derivation, watermarks, full backfill | next |
| 3 | Probes: micro workloads, covariates, run markers | |
| 4 | Analysis: baseline, verdicts, annotations, rollup and retention | |
| 5 | Ship: README/CHANGELOG polish, autostart docs | mostly done in 1 |

### Deviations from the plan as executed

- **The CLI rename landed in phase 1, not phase 5.** Phase 1 needed a command name, and inventing a
  temporary one to rename later was pure churn. `dashboard` is the daemon, the TUI is `top`, and the
  deprecation shim is in place. Phase 5 is reduced to documentation polish.
- **The `process_tree` consolidation landed in the phase 0 refactor commit** rather than with the
  feature, because it is behaviour-preserving cleanup of existing code and keeping it there made the
  phase 1 diff purely additive.
- **Sampler priming was added and was not in the original design.** `sysinfo` derives CPU percentages
  from the delta between two refreshes, so the first reading of every session reported exactly 100%.
  Left in, each daemon restart would have planted a phantom spike that phase 4's baselines would have
  averaged in. The sampler now takes and discards a throwaway reading, then waits at least
  `sysinfo::MINIMUM_CPU_UPDATE_INTERVAL` before recording.

### Open questions carried into phase 2

- **ratatui for the terminal views.** Desirable, but version-constrained: ratatui 0.29 requires
  crossterm 0.28.1 against this project's 0.29, which would link two crossterm versions each managing
  raw mode; ratatui 0.30 matches crossterm 0.29 but declares `rust-version = 1.86.0` against this
  project's declared MSRV of 1.85. Adopting it therefore means an MSRV bump. Undecided.
- **Whether `agentbench top` or the `bench` progress display benefits more** from a widget library.
  The progress display is currently a static text list where gauges and sparklines would help most.
