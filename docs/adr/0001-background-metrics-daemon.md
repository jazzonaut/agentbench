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
| tool latency | `assistant(tool_use).timestamp` → matching `user.toolUseResult.timestamp`, linked by the result block's `tool_use_id` and falling back to `sourceToolAssistantUUID` | permission waits inflate it |
| `first_response_ms` | user prompt row → first assistant row | contains the whole thinking block; a queued prompt waits before the request is sent |
| tokens, cache ratio | `usage`, **deduped by `requestId`** | none once deduped |

Read-only tool latency (`Read`, `Grep`, `Glob`, `Edit`) is the clean filesystem signal and is charted
by default. `Bash` latency is stored but not charted by default: it is dominated by permission waits
and legitimately long commands, so unfiltered it measures time away from the keyboard.

One API request emits several assistant rows sharing a `requestId`, each repeating the *cumulative*
`usage`. Summing naively multiplies token counts. Dedupe by `requestId` before aggregating. Measured
on 411 real transcripts: 1,844 of 2,926 requests emit more than one row.

Failed, refused and interrupted calls are recorded with `ok = 0` and excluded from every latency
series. Each returned early or spent its time waiting for a person, so including them would make the
machine look faster the more went wrong. On real data they are 3.3% of calls.

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
| 2 | Sessions: transcript parsing, derivation, watermarks, full backfill | **done** |
| 3 | Probes: micro workloads, covariates, run markers | next |
| 4 | Analysis: baseline, verdicts, annotations, rollup and retention | |
| 5 | Ship: README/CHANGELOG polish, autostart docs | mostly done in 1 |

### Deviations from the plan as executed

- **The CLI rename landed in phase 1, not phase 5.** Phase 1 needed a command name, and inventing a
  temporary one to rename later was pure churn. `dashboard` is the daemon, the TUI is `top`, and the
  deprecation shim is in place. Phase 5 is reduced to documentation polish.
- **The `process_tree` consolidation landed in the phase 0 refactor commit** rather than with the
  feature, because it is behaviour-preserving cleanup of existing code and keeping it there made the
  phase 1 diff purely additive.
- **"TTFT proxy" was renamed `first_response_ms`, by schema migration v2.** The design assumed the
  confound was local CLI overhead. On real transcripts the interval has a median of about 15 seconds,
  because an assistant row is written only once the whole first message exists and for a thinking model
  that includes the entire thinking block. A column called `ttft_ms` holding 15,000 would have had
  every chart, tooltip and future reader explaining that the number does not mean what it says. The
  measurement is kept — it is a real end-to-end interval a person waits through — under a name that
  claims no more than it delivers.
- **`session_turns` gained a unique index on `(machine_id, request_id)`, also in v2.** Identifying a
  turn by the row that happened to be read first is only correct if reading always starts at the top of
  a request. An import that resumed between two rows of one request would otherwise record a second
  turn carrying the same cumulative usage and inflate every token total downstream. The index turns the
  dedupe rule from a convention the importer has to remember into something the database enforces. It
  also, unplanned, handles resumed and forked sessions: 612 requests and 751 tool results appear in more
  than one transcript file, because resuming a session copies its earlier rows into the new file.
- **The watermark is "the earliest byte still needed", not "the last byte read".** A measurement spans
  two rows, and the pair straddles the end of a pass whenever a tool call is in flight. Stopping at the
  last byte read would silently lose those; re-reading a fixed stretch before it would re-read whole
  files to catch them. The deriver instead reports the offset of the oldest row it is still waiting on,
  which is usually a few hundred bytes back and often nothing at all. A resumed pass therefore reads
  exactly the new bytes plus the open rows, and parser state never has to survive between passes — so a
  long-running daemon and a freshly started one derive identically.
- **Tool-version capture moved into phase 2** from the phase-4 annotation work. The version appears
  only in transcripts, so recovering it later would mean a second full pass over every file. Collecting
  it while the bytes are already being read costs about fifteen lines and yielded 20 versions with
  first-seen dates on the first run.
- **The writer now drains its queue before committing** rather than committing every dozen records. A
  backfill submits tens of thousands of rows at once; a transaction per dozen made that thousands of
  commits. Batch size now follows the producers: one sample commits immediately, a backfill commits
  thousands at a time. This also removed the linger timer, since every drain ends in a commit and no
  partial batch is ever left waiting.
- **Transcript discovery had to go deeper than the documented layout.** Subagent transcripts live under
  `<session>/subagents/`, and those spawned inside a workflow under
  `<session>/subagents/workflows/<workflow>/`. A four-level walk found 344 of 411 files and silently
  dropped a fifth of the evidence; the cap is now eight levels, bounded by the file count rather than
  by depth alone.
- **Sampler priming was added and was not in the original design.** `sysinfo` derives CPU percentages
  from the delta between two refreshes, so the first reading of every session reported exactly 100%.
  Left in, each daemon restart would have planted a phantom spike that phase 4's baselines would have
  averaged in. The sampler now takes and discards a throwaway reading, then waits at least
  `sysinfo::MINIMUM_CPU_UPDATE_INTERVAL` before recording.

### ratatui and the MSRV

**Decided: adopt ratatui 0.30. MSRV is 1.88, and is now verified.**

ratatui 0.29 requires crossterm 0.28.1 against this project's 0.29, which would link two crossterm
versions into one binary with two independent owners of terminal raw mode. ratatui 0.30 matches
crossterm 0.29 but declares `rust-version = 1.86.0`, so adopting it requires the bump. The bump landed
first, on its own, so that the version constraint is recorded independently of the rewrite.

Keep the dependency narrow when adopting it: `default-features = false` with only the features
actually needed, since the defaults pull `all-widgets`, `macros`, `layout-cache`, and — on non-Windows
targets — the termion and termwiz backends, none of which this project uses.

This was originally recorded as an MSRV of 1.86, with the note that CI had no verification job and that
`rust-version` was therefore documentation rather than something enforced. Adding the job proved the
point immediately: the crate does not compile on 1.86 at all, and never did. It uses `let` chains,
which stabilised for edition 2024 in **1.88**, in code that predates the daemon work
(`diagnosis.rs`, `supervisor.rs`). Both 1.85 and 1.86 were claims nobody had tested.

The declared version is now 1.88, and the `msrv` CI job reads it out of `Cargo.toml` rather than
repeating it, so the manifest and the check cannot disagree. The job checks the library and binary
only: an MSRV is a promise to whoever builds the crate, and they do not build its tests, so including
test targets would only invite a failure the day a dev-dependency raises its own minimum.

The general lesson is worth keeping: a version constraint that nothing executes is a guess with a
number in it.

### Open questions carried into phase 3

- **Whether `agentbench top` or the `bench` progress display is rewritten first.** The progress display
  is currently a static text list where gauges and a sparkline would help most; `top` already works.
- **Whether `first_response_ms` deserves to be split.** It currently mixes queue wait, thinking time and
  real latency. The transcript may carry enough to separate them — `queue-operation` rows exist, and a
  thinking block's size is visible — but until something is measured that separation is speculation.
- **Whether subagent activity should be distinguishable from the session that spawned it.** Both are
  real work on this machine and both are imported, but a heavy workflow's tool calls currently blend
  into the parent project's numbers, and neither table records which is which.
