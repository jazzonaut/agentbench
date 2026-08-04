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
| tool latency | `assistant(tool_use).timestamp` → matching `user.toolUseResult.timestamp`, linked by the result block's `tool_use_id` and falling back to `sourceToolAssistantUUID` | permission waits inflate it; parallel calls in one assistant row share a start; 1 ms timestamp resolution against an 11 ms median |
| `first_response_ms` | user prompt row → first assistant row | contains the whole thinking block; a queued prompt waits before the request is sent |
| tokens, cache ratio | `usage`, **deduped by `requestId`** | none once deduped |

Read-only tool latency (`Read`, `Grep`, `Glob`, `Edit`) is the clean filesystem signal and is charted
by default. `Bash` latency is stored but not charted by default: it is dominated by permission waits
and legitimately long commands, so unfiltered it measures time away from the keyboard.

**Superseded after 0.5.0: one tool per series, and only `Read` is judged.** Pooling those four was
wrong and measurement says by how much. Over 15,035 real calls the medians are `Read` 11 ms, `Edit`
35 ms, `Grep` 72 ms, `Glob` 223 ms — an order of magnitude apart, mixed in proportions the model
chooses. Across 23 days with enough calls to have a daily median, the pooled figure correlated with the
*share of calls that were reads* at r = −0.86, against −0.39 for the `Read`-only median: three quarters
of the movement in the only judged session series was composition. The pooled figure ranged 18–34 ms
across those days and 3 August sat at 30 ms, near the month's worst, on the day whose `Read` median was
its best at 9.5 ms. `Read` is now judged; `tool_edit_ms` (`Edit`, `Write`) and `tool_search_ms` (`Grep`,
`Glob`) are charted. Filtering by project was considered as a further control and rejected for now:
`Read` medians across the eight busiest projects span 9–12 ms, so the repository matters far less to a
read than the choice of tool does.

Two confounds in tool latency were measured and deliberately left in place.

- **Parallel calls share a start.** When one assistant row requests several tools, each result is
  timestamped separately but all of them are timed from the row that asked, so siblings inflate each
  other. Real effect, real size: batched `Read` calls have a median of 16 ms against 10 ms for
  unbatched, +60%. They are 4% of reads, which moves a median by nothing measurable, and excluding them
  would need a schema column and a full re-import to backfill. Recorded rather than fixed.
- **The measurement sits close to its own resolution.** Transcript timestamps are milliseconds and the
  `Read` median is 11 ms, so a single quantum is 9% of the value. The MAD across daily `Read` medians is
  exactly 1.0 ms — one quantum — which means the band floor rather than the observed spread is what
  governs that verdict. Charting a series whose noise floor is a rounding step is honest only if the
  floor is disclosed, and it is.

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
8. **Background priority is one-way on Unix, and phase 3 depends on that.** Lowering a nice value
   needs no privileges; raising it back does. There is therefore no "restore normal priority"
   function, and a measured thread must be *started* at normal priority rather than restored to it —
   which is what decision 6 already requires, but for a reason weaker than the real one. A restore
   that silently failed would be the worst available outcome: every probe on that thread would read
   slow, consistently, and the dashboard would report a machine degrading while nothing had changed.
9. **A Windows-only developer cannot see a Unix failure locally, and CI is the only check.** Three
   defects sat in phase-0 and phase-1 code until the commits were first pushed: a `clippy` lint in
   `platform/unix.rs`, an unused `mut` on macOS in `system.rs`, and a test that deleted its own
   temporary directory and then reopened a database inside it — which passes on Windows, where an
   open file handle blocks the deletion, and fails on Unix, where it does not. Cross-checking is not
   an option either: `libsqlite3-sys` and `ring` compile C, so `cargo check --target` needs a cross
   compiler. Assume any Unix-specific code is unverified until CI says otherwise.

## Phases

| Phase | Content | Status |
|---|---|---|
| 0 | Prep refactor: split `bench.rs` into `bench/`, introduce `metrics/`. No behaviour change | **done** |
| 1 | Spine: config, store with migrations, sampler, `events`, `--status`, one live tile and one chart | **done** |
| 2 | Sessions: transcript parsing, derivation, watermarks, full backfill | **done** |
| 3 | Probes: micro workloads, covariates, run markers | **done** |
| 4 | Analysis: baseline, verdicts, annotations, rollup and retention | **done** |
| 5 | Ship: README/CHANGELOG polish, autostart docs, 0.4.0 | **done** |
| 6 | Conditions: what was different today, and making every collected series reachable | **done** |

### Phase 6: why

Phases 1–5 answer "is my machine slower today than yesterday" and cannot answer the second half of the
question this document opens with, *"and what changed?"* Two of the five judged subjects are filesystem
series, and `contended` was three CPU thresholds — so a probe that ran while an update or a backup wrote
gigabytes read slow at 15% CPU and entered the baseline as clean data. `cpu.single_mops_s` is judged with
nothing recorded that could say the part was throttled. Neither filesystem series had a free-space
covariate, so the slow monotonic drift the tool exists to detect was the one thing it could not explain.

The display half is the same problem seen from the other end: `/api/series` advertises twelve series
nobody can reach — six passive and six session, against the two that are charted. (This was written as
thirteen; the enums say seven and seven, of which `cpu_percent` and `tool_read_ms` were charted.) Collection
that cannot be read is cost without benefit.

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
- **The probe drops the sequential *read*, and keeps only the write.** The design said "micro-scale
  reuse of `bench` workloads" and budgeted 8 MiB of writes per probe. At 8 MiB the read pass comes
  straight out of the OS page cache and reports memory bandwidth — thousands of MiB/s — under
  `filesystem.sequential_read_mib_s`, a name that means disk throughput everywhere else in the tool.
  Sizing past the cache instead would mean gigabytes of writes a day for one number. Shared metric
  names are the point of reusing the workloads, so the honest move was to emit one fewer of them.
- **Covariates are read once, before the workloads, and a two-reading version was built and reverted.**
  The idea was sound on paper: read again afterwards, so that the closing CPU delta spans the measured
  window and catches a machine clobbered half a second in. Run against a real daemon on an idle
  sixteen-core machine, it tagged **17 of 24 runs contended**, because the delta it was computed from
  contained the probe's own work. The same objection defeats every repair: excluding our own process tree
  still leaves the scanner activity the probe's 200 file creates provoked, which is part of what the
  probe measures rather than something it competed with. So the covariates are the opening reading and
  nothing else. The cost is real and is documented in the type: something starting mid-probe is missed,
  and the tag claims only "what this measurement began in".
- **The contention thresholds are on two different scales, and getting that wrong was the second defect
  this phase.** `global_cpu_usage()` is 0–100 across all cores; `process_tree::usage` sums
  `Process::cpu_usage()`, which `sysinfo` reports as a percentage of *one* core, so a tree runs to
  100 × cores. The first version used 40 for the machine, 2 for scanners and 1 for agents as though all
  three were whole-machine figures. In practice 1% of one core is 0.06% of a sixteen-core machine, so
  every idling `node` process — and the default `agent_process_names` matches all of them — counted as
  contention. Real data showed the agent flag set on 6 of 8 runs on a machine doing nothing. The
  thresholds are now named for their scale (`BUSY_MACHINE_PERCENT`, `BUSY_SCANNER_CORE_PERCENT`,
  `AGENT_WORKING_CORE_PERCENT`) and set to 40, 10 and 20. The machine threshold still has to sit above
  the probe's own footprint, since one core of eight already reads 12%.
- **Both defects were found by running the daemon and reading `--status`, not by the tests.** Every unit
  test passed throughout: they asserted the classification rule against invented numbers, and the rule was
  fine. What was wrong was the numbers the rule was fed, which only a real machine produces. The lesson is
  narrow and worth keeping: a threshold is not verified by a test that supplies its own inputs.
- **Probe series are validated against the metric catalogue rather than a closed enum.** The passive and
  session series each have a hand-written statement per series, so an enum is the natural guard. Probes
  have one statement with the metric name as a bound parameter, and 18 catalogued metrics × 2 sources
  would be 36 enum variants restating a list that already exists. The catalogue *is* the closed set. The
  source prefix (`probe:` / `bench:`) is mandatory rather than defaulting to probes: a chart that
  silently picked one scale is exactly the failure the `source` column exists to prevent.
- **A probe records `p50` for a distribution, where a report records the mean.** `Metric::value` is the
  mean, which is right in a report that prints p50, p95 and max beside it. `probe_metrics` has one
  value column, and a single slow SQLite lookup out of a hundred must not become the fifteen-minute
  reading. This follows `Metric::distribution`'s own percentile convention, so a p50 on a probe chart
  and a p50 in a report mean the same thing.
- **Run markers carry their own id, not the report's.** The design implied one identifier. The opening
  write happens before any measurement, and the report's `run_id` does not exist until the run is over —
  a primary key cannot be assigned retroactively. `report_path`, written at the closing end, is what
  links a marker to the JSON it produced.
- **Markers are written by `profile` and `experiment` too, not only `bench`.** Both load the machine for
  minutes and put the same cliff in the passive series. Neither contributes metrics: a profile measures
  somebody else's command and an experiment runs whatever its TOML said, so their numbers describe that
  work rather than this machine's capability and have no business in a capability trend.
- **A foreground run writes to the *default* data directory, never to a `--data-dir` the daemon was
  started with.** A `bench` process has no way to discover where an unrelated process chose to keep its
  database. It also never creates one, and never reports a failure to write: collecting is something a
  user starts, and a message about a metrics database they may not know exists would be noise on the
  output of a benchmark that succeeded.
- **The marker writer refuses to migrate.** It may be an older binary than the one that created the
  database, and upgrading a schema out from under a running daemon is how history that cannot be
  regenerated gets corrupted. It checks `user_version`, writes in one short transaction under WAL and a
  five-second busy timeout, and closes.
- **A scratch directory that cannot be created is retried, not fatal.** The first implementation logged
  the failure once and returned. That was actively worse than looping: the supervisor treats an early
  return as a crash and restarts the worker after five seconds, so a permanently unwritable volume would
  have logged the same failure seventeen thousand times a day and buried every other event. The prober
  now prepares lazily inside its loop and keeps the handle once it succeeds, which also means a
  reconnected removable volume resumes probing without a restart.
- **Probe *scale* is deliberately not configurable, only the interval.** An interval is a preference. A
  working set is the unit the measurement is expressed in, and letting it change would silently make
  March's numbers incomparable to April's with nothing in the data to say so. The interval does get a
  one-second floor, in the file and on the CLI flag alike, since back-to-back probing stops being
  background collection.
- **`platform::on_battery()` is a new capability and does not share code with `system::power_source`.**
  They answer different questions under different budgets: the report's version names the source in prose
  once per run and is free to spend a child process on it, whereas this one is asked immediately before a
  measurement. Windows uses `GetSystemPowerStatus`, Linux reads sysfs, macOS spends a short `pmset`
  because IOKit is the only alternative and it would mean a new dependency, and everything else reports
  that it cannot tell. `None` is stored as SQL NULL and read back as unknown — never as "on mains",
  because a laptop on battery runs a third slower for a reason that has nothing to do with degradation.
- **`system::machine_id()` was extracted from `inventory`.** The marker writer needs the machine's
  identity and nothing else, and building a whole `Inventory` for it enumerates every disk and spawns a
  child process. The derivation now lives in one place, which it has to: that value is the primary key of
  the `machines` table, and two spellings of it would silently split one machine's history in two.

- **The baseline's unit is the day, not the measurement.** Decision 12 said "trailing 7-day median/MAD
  band" without saying what the band is over, and the two readings behave completely differently. Pooling
  the window's individual runs — about 670 at the default cadence — produces a band that measures the
  *within-day* spread, which on real data is 1–10% per probe; a genuinely slow week sits inside it and
  reports as normal. Reducing each day to one value and taking median/MAD across those seven numbers
  measures day-to-day variance, which is what a day-over-day verdict is asking about. The cost is that
  seven numbers make a coarse MAD, which is what forces the floor below.
- **The band has a relative floor, and that floor is load-bearing rather than cosmetic.** A MAD over seven
  daily values is frequently *exactly zero* — measured on this machine, `sqlite.lookup_ms` had a
  day-to-day MAD of 0.0 and the others were 0.03–1.0% of their medians. A band of zero width declares every
  subsequent day either better or worse than history, which is a verdict generator rather than a verdict.
  The floor is 5% of the baseline median, and `width_is_floor` is reported so a reader is told when the band
  is a convention instead of a measurement. Validated by seeding a real database with real collected values
  as previous days: today read `normal` on all four probe series with deltas of 0.0–0.7%, and the floor was
  what defined the band in every one of them. The honest caveat is that seeded days drawn from one
  afternoon understate true day-to-day variance, so in real use the measured spread will bind more often
  than the floor — which is the intended regime, with the floor protecting the degenerate case.
- **Battery runs are reported, not excluded.** The covariate exists to be filtered at analysis time, and
  three filters were available: exclude known-battery runs, match today's dominant power source, or count
  everything and disclose the mix. Excluding was rejected because a laptop that lives unplugged would never
  reach a verdict at all — the feature would be dead exactly where capability drift matters most. The
  decision was to count everything and, when today's majority power source differs from the baseline's, say
  so in words beside the verdict. The accepted cost is explicit: a laptop unplugged this morning reads as
  degraded, and the caveat rather than the filter is what tells the reader why.
- **A verdict declines rather than guesses.** A day contributing fewer than three measurements is dropped
  entirely, because a median of two numbers is one of them, and fewer than four contributing days yields
  `Insufficient` — a distinct outcome from `Normal`, since "behaving as usual" and "nobody knows" warrant
  different responses. Every verdict carries the day count *and* the measurement count behind it, because
  seven days of two probes each is not a week of evidence and the day count alone is flattering.
- **`first_response_ms` and `tool_bash_ms` are charted and deliberately not judged.** Both were candidates
  for the curated set and both would have been false-alarm generators: the first contains the whole thinking
  block, so a verdict on it reports the model's mood as a property of the machine, and the second is
  dominated by how long commands legitimately took and by waits for a human to grant permission. Token
  counts and cache ratios describe what was asked of the agent. This is asserted in a test so the exclusion
  cannot be quietly undone.
- **Annotations needed no table.** Decision 12 promised version annotations and the design implied storing
  them; both sources — `tool_versions` from phase 2 and `run_markers` from phase 3 — were already being
  collected for exactly this purpose, so annotations are a query over facts rather than a fourth stream. A
  version's mark is the *first* sighting of it, since the importer records the running version on every
  transcript row it reads and a mark per sighting would paint the axis solid. Run marks are returned for runs
  *overlapping* the range rather than starting inside it: a stress run that began before the window is the
  explanation for everything in it, and a mark drawn only at its start would be off-screen when it matters.
- **Only the first sighting is stored, not only displayed** (corrected in 0.6.1, migration v5). Taking the
  first sighting at query time was correct and left the write side recording one row per *sighting*, keyed on
  `(machine_id, tool, ts)`. The deriver's state is per-pass, so it emits a version record for the first row
  of every pass that read new bytes: roughly one row per poll while a session is live, ~2,880 a day, in a
  table nothing prunes and that the annotations query grouped over in full every sixty seconds. The key is
  now `(machine_id, tool, version)` with `ts` kept at the minimum, so the write is idempotent in intent as
  well as effect and a version's first sighting is a lookup. Nothing visible changed, which is exactly why
  this would have gone unnoticed until the query got slow.
- **Retention runs on the writer thread, as an instruction rather than a row.** The single-writer rule is
  what makes this database intelligible, and a bulk summarise-and-delete issued from a second connection is
  precisely the race it exists to prevent. `Record::Maintenance` therefore travels the same channel as
  every sample, and the writer commits the batch first and then does the housekeeping in transactions of its
  own — a fortnight of backlog must not be welded to whatever samples happened to be queued beside it. A
  failure is logged and swallowed: retention is the least important thing the daemon does, and propagating
  the error would take the writer thread down and stop every stream.
- **`samples_1m` was two columns short, and nothing had noticed because nothing wrote to it.** The v1 table
  summarised five of the seven passive series the dashboard advertises. Left alone, a thirty-day chart of
  system CPU would have kept its history while the same chart of process count stopped dead at the retention
  boundary with nothing on the page to explain the difference. Migration v3 adds `process_count_avg` and
  `agent_rss_max`; the table has never held a row on any machine, so widening it needed no backfill.
- **A summarised minute is never rewritten.** The cutoff is aligned down to a minute boundary so a
  summarised bucket is always whole, and a late sample landing in a minute already rolled up is pruned
  without being merged. Merging would mean averaging an average, which is a number that is neither the mean
  of the minute nor anything else. It takes a clock going backwards to reach this case, and losing one
  sample is a better outcome than a corrupted summary.
- **A chart that reads from two tables has to say so.** A rolled-up point is a summary of sixty seconds, and
  which summary depends on the series: memory in use keeps its average because the average is what the
  machine was living with, while swap and scanner CPU keep their peak because a thirty-second burst *is* the
  event. The response therefore reports `resolution` (`raw` / `rollup` / `mixed`) and the reducer, and the
  page notes it under the chart. Mixed is the normal case for any range spanning the boundary, not an error.
- **Two display faults were found by running the daemon and reading the output, again.** `sqlite.lookup_ms`
  is genuinely four or five microseconds on a healthy machine, and both surfaces rendered it as "0 ms" —
  the web formatter was written for tool latencies in the tens of milliseconds, and `--status` printed one
  decimal place. A metric in the judged set that reads zero for ever is worse than one that is absent. Every
  unit test passed throughout, for the same reason as in phase 3: they asserted formatting against numbers
  they chose themselves.
- **A gap threshold floored at a fraction of the requested range was built and reverted, and the underlying
  limitation is still open.** The problem is real: a range whose cadence changed within it has a median
  spacing set by the dense half, against which every sparse point is an outage and becomes an island between
  two breaks — and an island has no line segment to belong to, so a chart holding seventy-two points can
  render as an empty frame. Flooring the threshold at a forty-eighth of the *requested* range fixed that and
  immediately drew a confident straight line across a ninety-second daemon restart, because the request was
  for forty-eight hours while the plot had auto-ranged to nine minutes. The server cannot know what the
  client will auto-range to, so the threshold has no business consulting the request, and interpolating
  through unobserved time is the worse of the two failures. Reverted to the cadence rule.
  Point markers were also left on uPlot's automatic rule rather than switched off, which is an improvement in
  its own right — four probes in an hour-wide view are now visible instead of being three faint segments —
  but it does **not** rescue the island case: twelve points ten minutes apart are about two pixels apart in a
  forty-eight-hour plot, too dense for uPlot to mark and with no segment to join. Verified by screenshot, not
  assumed. The mixed-cadence range therefore still renders its minority-cadence stretch as blank, and the
  real fix is per-neighbourhood gap detection rather than one threshold for the whole series.
- **The y-axis width is measured rather than fixed.** A hardcoded 52 pixels fits `30.0%` and clips
  `7,129 ops/s` to `000 ops/s`, which reads as a chart of small numbers rather than as a truncated label.
- **The percentile convention moved into `model`.** It was hand-rolled in `Metric::distribution` and again
  in the session series reducer, and a baseline needed a third copy. A p50 on a chart, a p50 in a printed
  report and a p50 behind a verdict have to be the same number, and a reader comparing two of them has no
  way to discover that they were not.
- **Local-day arithmetic has one home.** `clock::local_day_start_ms` was deleted and `analysis::day` owns
  it, because the live tiles count "today" and the baselines count the days before it — if those two
  disagreed about when today started, the page would show a figure the verdict beside it was not computed
  from. `Day` carries a start and an end rather than a start and a constant, since two days a year are 23 or
  25 hours long.
- **The retention startup delay was set to sixty seconds and then to ten.** Sixty was conservative without
  a reason that survived scrutiny: startup is a socket bind and a primed sampler, well under a second
  between them, and a longer delay only means a daemon someone runs for a few minutes at a time never gets
  round to it. Ten seconds also makes the wiring testable inside a normal integration test, which sixty did
  not.
- **Verdict and retention integration tests seed history rather than waiting for it.** A verdict needs days
  of data and retention needs samples older than both its window and the minute in progress; neither can be
  produced by waiting. Both tests write through the real `Store` before the daemon opens the directory, so
  the rows are exactly the shape the daemon produces — including the machine id, which has to match or the
  daemon reads none of them.

- **Phase 5's largest finding was that the manifest never moved.** The phase was scoped to prose, since the
  CLI rename landed in phase 1 and phases 1 and 4 each wrote their own README and CHANGELOG sections. What
  nothing had caught is that `Cargo.toml` still said `0.3.0` while the README already told readers the
  terminal view was "renamed in 0.4.0" and the CHANGELOG said the compatibility shim would be "removed in
  0.5.0". Both statements were true of the release being prepared and false of the crate as it stood, and
  `release.yml` would have rejected a `v0.4.0` tag for exactly this reason — it compares the tag against
  `cargo metadata` and greps the CHANGELOG for a matching heading. The version is now `0.4.0` and
  `[Unreleased]` is dated. The general shape is familiar from the MSRV: a version written in prose but not
  in the file that defines it is a claim nothing executes.
- **Two open questions were promoted out of this document into the README.** The mixed-cadence blank stretch
  and `memory.write_gib_s` were both recorded here as things to fix later, which is the right place for a
  plan and the wrong place for a caveat a user will hit. The blank frame is visible to anybody who changes
  `--probe-interval`, and a charted metric reading two orders of magnitude low invites the reader to trust
  it. The tool's stated posture is to report a missing capability rather than invent a number, and that
  posture has to extend to numbers it does produce but does not yet believe. Both remain open questions
  below; the README now says so in the platform limitations, where somebody reading a wrong-looking chart
  will actually look.
- **The daemon's platform caveats went to `## Platform limitations`, not into the collection section.** The
  one-way Unix priority rule, best-effort thread priority, and `on_battery()` returning unknown on
  unsupported platforms were each documented where they were implemented — inside the long prose on what the
  collector does and costs. That is the section somebody reads when deciding whether to run it, not the one
  they read when their platform behaves differently from the description. The README already had a section
  whose entire subject is "what this OS cannot tell you", and these belonged in it.

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

### Phase 6 deviations, and what measurement decided

- **The obvious API for the clock covariate is useless, and would have shipped a permanently flat line.**
  The design said "record the CPU frequency". `sysinfo::Cpu::frequency()` reported exactly **3801 MHz on
  all thirty-six readings**, spanning 8% to 98% whole-machine CPU, on a part whose nominal figure is
  3801 MHz; WMI's `Win32_Processor.CurrentClockSpeed` agrees with it and equals `MaxClockSpeed`. Both read
  a value the registry holds from boot. What *is* live is the PDH counter
  `\Processor Information(_Total)\% Processor Performance`, which read 135–137 idle and **128–131 under an
  all-core `cargo build`**. So the covariate is a ratio of nominal rather than a figure in MHz — above 100
  while boosting, below 100 while throttled — and it comes from PDH. Had this not been measured first, a
  judged CPU chart would have gained a covariate that never moved, which is worse than one that is absent:
  a flat line reads as a machine behaving.
- **An unelevated process sees SYSTEM-owned processes' CPU but not their I/O.** `svchost` reported 1.5%
  CPU and **exactly zero bytes**; so did Defender, the search indexer, `System` and `Registry`, and 346
  processes had an unreadable `exe()`. Per-process I/O therefore cannot see the backups, updates and
  scans that make a filesystem probe read slow — which is most of what the covariate was for. Whole-machine
  throughput comes from `\PhysicalDisk(_Total)\Disk Write Bytes/sec` instead, unattributed by construction,
  and the per-process figures are kept beside it as attribution for the user's own processes only. The two
  answer different questions and neither replaces the other. The daemon stays unelevated, so this is a
  permanent property rather than something to fix.
- **PDH costs one feature on an existing dependency and 302 µs a call.** `windows-sys` was already a direct
  dependency; `Win32_System_Performance` is one more feature and no new crate. `PdhAddEnglishCounterW`
  rather than `PdhAddCounterW`, because a counter path is localised and `\PhysicalDisk` does not exist by
  that name on a German install — a failure that would appear only on machines no test of ours runs on. The
  first collect of a rate counter returns no value, which is the same priming rule the sampler's CPU delta
  and the per-process I/O deltas already follow.
- **The extra process-table walk the plan budgeted turned out to be unnecessary.** A PDH collect needs no
  walk, so the machine-wide disk rate spans exactly the window the CPU covariate spans, and one disclosure
  covers both: the reading is the ~200 ms before the workloads, so it catches sustained background I/O and
  misses a burst that starts mid-probe.
- **`Current Disk Queue Length` was collected and dropped.** Flat 0.0 in every reading, including while
  45 MiB/s was being written, which is what that counter does on NVMe.
- **The disk threshold is 20 MiB/s, from measurement.** An idle desktop with a browser and an editor open
  wrote 17 KiB/s at the median and peaked at 1.3 MiB/s; an all-core build ran to 44.9 MiB/s. Validated
  against a live daemon under a sustained writer: **all five probes were tagged contended at 769–1066 MiB/s
  while whole-machine CPU sat at 16–22%**, every one of which the old rules would have filed as clean data.
  Like every threshold here it cannot be validated by a test that supplies its own inputs, and unlike the
  CPU ones it has no history behind it yet.
- **`Read` latency is *not* confounded by file size, and that is worth recording so nobody re-opens it.**
  The pooled-tools finding was that three quarters of the movement in the only judged session series was
  composition, at r = −0.86. The same objection applied on its face to file size within `Read`: a 40-line
  read and a 4,000-line read are not the same measurement and the mix is the model's choice. Measured over
  7,493 real calls, **Spearman r = −0.035**; across size quintiles whose medians span 634 B to 11,990 B, a
  nineteenfold range, the median latency reads **11, 11, 11, 10, 11 ms**; and the day-level composition test
  over 24 days gives **r = −0.125** against the −0.86 that condemned tool pooling. No column, no re-import.
  The quintile row is also the cleanest demonstration yet of something already claimed above: the
  measurement sits on its own 1 ms resolution floor, so a nineteenfold change in what is being read moves
  the median by at most one quantum.
- **The probe ranked itself as its own competition, and only real data showed it.** The second probe of a
  session named `agentbench.exe` at 14.3% of a core as the machine's second-largest consumer: the *previous*
  probe's workloads, averaged over the interval since. The busier the probe, the higher it would have
  ranked, putting a plausible and entirely wrong explanation beside every reading. The daemon's own process
  tree is now excluded; the agent's deliberately is not, since an agent working while a probe runs is
  exactly the competition worth naming. Every unit test passed before and after, for the third time in this
  document's history.
- **Ranked consumers span the interval since the previous probe, not the priming window.** The figures come
  from the walk in `Observer::prime`, and `sysinfo` reports a process's CPU as a delta since that process
  was last refreshed. Walking again inside `read` would make them an instant and would put the walk inside
  the window the CPU covariate is computed from, which is what the two-reading version was reverted for.
  The longer window is arguably the more useful one — what explains a slow probe is a scanner busy for ten
  minutes, not one scheduled during a particular 200 ms — but it answers "what has been using the machine"
  rather than "what is using it now", and nothing downstream may claim otherwise.
- **Consumers are ranked by absolute CPU, and this is the weakest part of phase 6.** On the development
  machine `RadeonSoftware.exe` and `NGenuity2Helper.exe` each burn about a whole core continuously —
  corroborated independently, since Task Manager's 6% of a sixteen-core machine is 96% of one core — so they
  hold two of the three slots on every probe for ever, and the one remaining slot is all that is left to
  name the process that was *unusual* today. A process that always burns a core explains nothing about why
  today differs from yesterday, which is the question this whole design exists to answer. Ranking by
  deviation from each process's own usual level would answer it and needs per-process history the schema
  does not have. Shipped absolute, recorded as an open question, and worth revisiting once a week of real
  consumer data exists to look at.
- **`agent_at` is stored beside `agent_active` although one is derived from the other.** `agent_active` is a
  threshold applied at write time, and the open question at the foot of this document says that threshold
  and its default process names are likely to change. Without the raw figure every row collected under the
  old constant would be impossible to reclassify: stranded, and silently mixed with later rows that meant
  something different. One column buys the ability to recompute a verdict from stored facts.
- **`generation_ms` forced turns to be held open, which created and then closed a new failure mode.** The
  span of a response is first row to last, so it cannot be known when the first row is read; a turn is now
  held until something proves the request finished. That alone would have lost the last turn of every
  completed transcript, because a session ends on the assistant's final message and a file whose mtime never
  changes again is never re-read. The importer therefore takes a `settled` flag — a transcript untouched for
  more than two poll intervals has its last response closed, a live one does not. Flushing unconditionally
  was rejected as *worse than losing the turn*: a response can outlast a thirty-second poll, and a truncated
  span reads as a faster response than actually happened, which is a wrong number rather than a missing one.
- **Migrations v1–v5 were collapsed into one statement.** No release carried any of them, so they described
  an upgrade from a database that exists nowhere, and keeping them meant 270 lines of SQL and upgrade tests
  for paths that can never run again. The reasoning was moved here and into `schema.rs`, not deleted, and the
  rule stands: an entry is immutable once a release has shipped it. The refusal message for an unrecognised
  `user_version` now names the real remedy, because the likeliest cause is a database from the development
  line whose counter had reached 5, for which "upgrade AgentBench" would be advice that cannot work.
- **The accepted cost of that collapse is four days of verdicts.** Wiping the databases loses the sample and
  probe history, so every probe verdict reads `insufficient` until four days have accumulated, and the open
  question about whether the 5% band floor survives a real week restarts from zero. Sessions are unaffected:
  a full transcript backfill regenerates them, which is also how the new session columns reach history.

### Phase 6 analysis and API decisions

- **"Differs materially" reuses the verdict band rather than a threshold per covariate.** Each covariate
  gets its own `Baseline::from_days` over the same window the verdict used, and a clause appears in the
  conditions line only when today's median falls outside that covariate's own band. The alternative was a
  hand-picked sensitivity per covariate — some percentage of clock, some number of MiB/s — which is one more
  constant per covariate that nothing in this project could validate. Reusing the band gives one sensitivity rule for the
  whole tool, and `MIN_DAYS` and the 5% floor come with it free. The cost is symmetrical and worth stating:
  conditions lines stay silent for exactly the same first four days that verdicts do, so a fresh database
  explains nothing until it can also judge nothing.
- **`cond:*` honours the page's uncontended filter, and the page has to admit what that does.** The
  Conditions frame shares a cursor with the Probe frame above it, and correlating a covariate against a
  measurement requires both to be drawn from one population. The consequence is not obvious and would
  otherwise read as a defect: with the filter on, `cond:disk_write_bytes_s` cannot exceed the 20 MiB/s
  contention threshold, because every run that did was excluded by definition. The same holds for the
  conditions *line*, every figure in which is a median over the uncontended subset — which is why it reads
  `clean probes:` rather than `today`. Naming the population is the whole difference between a capped figure
  and a wrong one.
- **Every series reports its own unit, and the page hardcodes no formatter.** A closed vocabulary — `%`,
  `B`, `B/s`, `ms`, `ratio`, `tokens`, `tokens/s`, and `""` for a bare count — travels with each series from
  the server, and `unitFormatter` grew a case per unit. Three of the four frames change metric at runtime, so
  a panel that kept the formatter it was constructed with was a live fault waiting for its first switch:
  a byte rate rendered through the latency formatter is not a formatting blemish, it is a wrong number.
  Per-core versus whole-machine is deliberately *not* a unit but a note, because two series measured in
  percent do not become comparable by sharing an axis label.
- **The contention thresholds moved to `src/watch/contention.rs`, and `is_contended` became
  `cause(..).is_some()`.** Three layers need those numbers now — the writer that tags a run, the analysis
  that explains one, and the live tile that names what a probe was competing with — and the tile had been
  inferring the cause from the covariates by hand, which is why it had no disk arm at all and would have
  reported "the machine was busy" for a probe tagged by 3 GiB/s of writes at 13% CPU. Without the extraction
  the fix meant hardcoding 20 MiB/s in `app.js`: a second authority for a number this document says will be
  revised.
- **`CondSeries::EXPLANATORY` is four of the six charted covariates.** `scanner_at` and `agent_at` are
  charted but never appear in a conditions line, because that line is computed over uncontended runs where
  both are bounded above by their own contention thresholds, at a tenth and a fifth of one core. A move from
  a fiftieth of a core to a twelfth is a large *relative* change that explains nothing about an 8% throughput
  drop, and it would push the useful clauses off the tile. `cpu_at` survives the same objection because its
  bound is 40% of the whole machine, which on this machine is six cores of somebody else's work. Both
  excluded series stay on the chart, where the reader supplies the judgement the tile cannot.
- **The covariate window under-samples bursts, and the size of the effect is calculable.** Proving the disk
  arm end to end took four concurrent sustained writers: three consecutive probes read 3,095–3,487 MiB/s and
  came back `contended=true, cause=disk` at 12.9% whole-machine CPU. Two earlier attempts with a `dd` loop
  read 0.0 MiB/s and were **not** a defect — `typeperf` on the same counter showed 2.0 GB/s for about one
  second per pass and near-zero between, so the ~200 ms window kept landing in the gaps. The chance of
  catching a burst is roughly its duration divided by the probe interval; sustained load is what the covariate
  sees, which is what it is documented to claim and also the load that actually explains a slow day.
- **`scanner_write_bytes_s` ships knowing it is a flat zero on Windows.** Forty-nine consecutive live samples
  read exactly zero, reproducing A1's finding precisely: the scanner's CPU is visible to an unelevated reader
  and its I/O is not. This is the same trap that killed the MHz covariate — a flat line reads as a machine
  behaving — except the column already existed and the guard test below forces it onto the page. It is
  charted with a note saying that a flat line here is a reader without privileges, not a quiet scanner. The
  distinction that makes it shippable is that the zero is *configuration*-dependent rather than structural:
  a user-owned scanner would report bytes.
- **`output_tokens_per_s` excludes the turns that have no denominator.** `generation_ms` is `NULL` for a
  single-row request, which is about 37% of them, and a zero span is arithmetically worse than a missing one.
  Both are excluded rather than treated as instantaneous, and the note says the series covers multi-row
  responses only. A rate averaged over turns that had no measurable duration would read fastest exactly when
  the least was happening.

### Phase 6 dashboard decisions

- **The series catalogue is its own asset, `src/watch/assets/series.js`.** Twenty-seven choices each carrying
  a caption and a note is about 370 lines of prose, and `app.js` would have gone past 1,100 lines holding it.
  The cost is a fifth embedded asset with its own ETag and a row in three tests that enumerate assets. The
  benefit beyond size is that the guard tests read *that* file, so what they check is a catalogue rather than
  a whole application, and a caption can be rewritten without touching rendering code.
- **The guard tests live in `serve::assets::tests`, not beside the enums or in `subjects.rs`.** That module
  already owns the markup↔script contract and is the only place that sees both the assets and the server's own
  `known_series()`, so one test covers all three closed enums and its converse covers the probe and `bench:`
  names too. `subjects.rs` keeps its narrower assertion about the judged four, which is a different claim:
  that every *judged* metric is offered, not that every *collected* one is reachable.
- **Both directions are asserted, because the two failures look nothing alike.** A collected series with no
  button is the failure this phase exists to prevent — cost without benefit, and invisible by construction.
  A button naming a series the server would reject is the cheaper fault, but its symptom is a single empty
  frame, which is indistinguishable from the first day of collection and would therefore survive review. The
  second test costs four lines. Both were proven by breaking them: `used_swap` mistyped as `used_swapp` fails
  in both directions with messages that name the fault, because a test that has never failed is a test that
  has never been checked.
- **Chart frames gained an info mark anchored inline at the end of the caption**, rather than reusing the
  tile pattern unchanged. A card's top-right corner is taken by the switch, and a note hung from the card
  would open below the plot — a frame's height away from the pointer that revealed it. A `.note-anchor` span
  sits after the caption text and the note hangs from that. The mark is created once and its wording
  rewritten on each selection, exactly as a verdict tile's is, so changing metric cannot close a note the
  reader has open.
- **The caption is written from the catalogue and never authored in the markup.** The markup holds an empty
  paragraph. A caption maintained in two places is precisely how a frame comes to describe the previous
  selection's scale, and three of the four frames now change selection at runtime.
- **Card titles became the frame's subject — "System", "Agent" — not the measurement.** "System CPU" was a
  title that named one of nine choices, and would have been wrong for eight of them. What is being measured
  is named by the pressed button and by the tooltip label, both of which change with the selection; the title
  names what the frame is *about*, which does not.
- **Page height grew by one card, against the plan's own mitigation.** The plan claimed height would not
  grow while its own table listed four frames where three had been, which was wrong when it was written.
  What the mitigation was actually protecting is intact and is the part that matters: the *default* reading
  is unchanged, so a reader who touches nothing sees today's three frames plus the conditions the runs
  plotted above them ran in.

### The dashboard gained pages that act, and what that cost

Decision 5 above reads "loopback-only, no auth, plaintext paths in the local DB", justified on the grounds
that *anything reaching loopback can already read the file*. That argument is sound and it covers reads
only. Adding a page that starts a benchmark broke its premise, so the premise is restated here rather than
left to be inferred from the code.

- **The `Host` check was not sufficient and was never meant to be.** `origin::is_own_host` refuses a request
  addressed to somebody else's name — the DNS-rebinding case. It cannot refuse one addressed correctly to
  `127.0.0.1:7878` by a page the user happens to have open, because that request carries exactly the `Host`
  this server expects. Harmless for a read; not harmless for a request that loads the machine for up to
  fifteen minutes and, with live cases enabled, spends the API credit of whoever the daemon runs as.
- **So a write must satisfy three conditions, none sufficient alone.** `Sec-Fetch-Site: same-origin` where
  the header is present, an `Origin` naming one of this socket's own names where that is present, and
  `Content-Type: application/json` unconditionally. The third is the load-bearing one: a cross-site HTML
  form can `POST` without a preflight, but only as `x-www-form-urlencoded`, `multipart/form-data` or
  `text/plain`, so requiring JSON makes every write preflighted and this server answers no preflight. The
  first two are tolerant of absence because `curl` and the test suite send no fetch metadata; the third is
  not, because every real client can set it.
- **The benchmark runs in a child process, not on a daemon thread.** This is a measurement decision. The
  sampler runs at background CPU and I/O priority and the daemon owns the single database writer; a workload
  measured inside that process reports a slower machine than the identical workload from a terminal, and the
  two numbers would be stored under one name. The same reasoning already forbids throttling the prober. The
  cost is a run supervisor, two reader threads per run, and a phase format that now has a parser as well as
  a printer — `Phase::parse` beside `Phase::line`, with a round-trip test, in the file whose documented
  hazard is those two drifting apart.
- **`install::run_detached` was not reused, despite existing for "launch a benchmark from a UI".** It takes
  one argument *string*, which is the wrong shape for values that arrived over a socket, and it is
  Windows-only. `std::process::Command::args` passes a vector to the operating system, so a target directory
  named `foo & shutdown /s` is a directory name rather than two commands, and there is no quoting to get
  wrong. `CREATE_NO_WINDOW` covers the one thing `run_detached` was giving us on Windows.
- **One run at a time, refused with `409` rather than queued.** Two benchmarks on one machine measure each
  other. A queue would start a stress run twenty minutes after somebody clicked, on a machine whose state
  nobody was watching by then.
- **No elevated run from the page, and live cases off by default on it.** Both are departures from what the
  command line and the control centre offer, and both are deliberate: a UAC prompt raised by a web page is
  one the user cannot connect to anything they did, and a form whose default spends money on submission has
  the wrong default. `BenchOptions::for_preset` enables live cases for `standard` and `stress`; the page
  overrides that to off and bounds the cost cap at $20, where the command line has no ceiling.
- **`server.allow_runs` exists so the premise can be restored.** A machine whose owner wants the dashboard
  for reading and nothing else sets it to `false` and keeps every chart. Default `true`, because a feature
  that shipped switched off would mostly generate questions about why the button does nothing.
- **The comparison was split into a value and two renderers.** `compare::compare_reports` produces a
  `Comparison`; markdown and the page are both views of it. What counts as a regression depends on the
  metric's direction and on whether it is informational at all, and a second implementation of that in
  JavaScript would eventually disagree with the `.md` file beside it about the same pair of files.
  `POST /api/compare` takes report *documents* and no paths — an endpoint taking paths would be a loopback
  service that reads any file on the machine and returns whichever parts of it parse as a report.

### Questions phase 6 answered

- **Whether subagent activity can be distinguished is now yes, and it was nearly free.** `Row::is_sidechain`
  was already being parsed and thrown away. It is populated: **164 of 431 transcripts** on the development
  machine are subagent transcripts, so 38% of the corpus was blended into the parent project's numbers with
  nothing in the database able to separate it. Both session tables now carry the flag. Subagent rows also
  carry `agentId` and a `slug`, so naming *which* agent is available later without a second pass.
- **Whether `Read` latency needs a file-size control is now no**, with the numbers above.

### Questions phase 4 answered

- **"What a verdict does when the uncontended subset is too small" is now a number.** Fewer than three
  measurements in a day drops the day; fewer than four contributing days declines the verdict. Both are
  reported rather than implied, and `--status` prints the counts beside every figure.
- **Whether the baseline should filter on power was decided as "no, disclose".** Recorded above with the
  reasoning and the accepted cost.

### Open questions carried past 0.4.0

- **Whether `agentbench top` or the `bench` progress display is rewritten first.** The progress display
  is currently a static text list where gauges and a sparkline would help most; `top` already works.
- **Whether `first_response_ms` deserves to be split.** It currently mixes queue wait, thinking time and
  real latency. The transcript may carry enough to separate them — `queue-operation` rows exist, and a
  thinking block's size is visible — but until something is measured that separation is speculation.
- ~~**Whether subagent activity should be distinguishable from the session that spawned it.**~~ Answered in
  phase 6: both session tables now record it. What remains is a *display* question — no series filters on it
  yet, and 38% of the corpus being subagent work means the judged `Read` series is still a blend until one
  does.
- **Whether ranked consumers should be ordered by absolute CPU or by deviation from their own norm.** Two
  processes on the development machine burn a core each around the clock and hold two of three slots on every
  probe, leaving one slot to name whatever was actually unusual. Absolute is what shipped; deviation is what
  the question the dashboard asks would want, and it needs per-process history no table holds. Revisit with a
  week of real consumer rows in hand.
- **How to break a line when the cadence changed inside the range.** One threshold for the whole series
  cannot serve two cadences, and the two wrong answers are both known: the median leaves the sparse stretch
  blank, and a range-relative floor interpolates across real outages. A per-neighbourhood rule — compare each
  spacing to its local neighbours rather than to the series median — would answer it. Worth doing when
  somebody actually changes `--probe-interval` mid-history and notices; today it needs that to happen
  deliberately.
- **Whether the 5% band floor survives a real week.** It was validated against real values standing in for
  previous days, which understates day-to-day variance. The question it cannot answer is what the *true*
  day-to-day MAD of a daily median is across reboots, thermal states and background load. A week of real
  collection settles it: if the measured spread routinely exceeds the floor, the floor is doing only its
  intended degenerate-case job; if it does not, 5% is the de facto sensitivity of the whole feature and
  deserves to be chosen deliberately rather than inherited.
- **Whether `memory.write_gib_s` is measuring what it claims.** A release-build probe reports about
  0.07 GiB/s on a machine that should manage two orders of magnitude more. It is not in the judged set so it
  did not block this phase, but the workload is shared with `bench`, which means either the probe's 64 MiB
  scale is too small to measure bandwidth or the benchmark has been reporting the same wrong number all
  along. Worth measuring before anything is changed, since a fix moves published report values.
- **Whether run markers should suppress probes while a foreground run is in flight.** Today a probe that
  lands during a `bench` run is collected and tagged contended by its own covariates, which is consistent
  and costs nothing. The alternative — the marker telling the prober to skip — trades a correctly-tagged
  data point for a hole, and would be the first piece of cross-process coordination in the design. Not
  worth doing on suspicion; worth revisiting if the tags turn out not to catch it.
- **Whether a probe should observe the machine *during* the measurement at all.** The opening reading
  cannot see something that starts mid-probe, and the two-reading attempt above shows the naive fix is
  worse than the gap. A per-process accounting that excluded the probe's own tree *and* attributed
  scanner CPU to the probe's own file operations would be the real answer, and it is a considerable
  amount of work for a case whose frequency nobody has measured yet.
- **Whether `agent_process_names` should default to matching every `node` process.** It is what makes the
  agent covariate fire on a developer machine most of the day, which is arguably correct and arguably
  useless. The alternative is matching the Claude Code process specifically and accepting that other
  agents go unattributed. Worth deciding with phase 4's baselines in hand, since the cost of the current
  default is exactly "how small does the comparable subset get".

  Two things narrowed the question in 0.6.1 without answering it. The names are now matched against a whole
  process name rather than as a substring of one, because a match is expanded to its descendant tree and the
  tree's CPU is *summed* against a threshold meaning "an agent is working" — so a loose match adds not a
  process but everything that process ever started. And the sampler now logs what the configured names
  actually matched, once, at startup, because the count was otherwise invisible: on the development machine
  the shipped `["claude", "node"]` matches **21 processes**, which is the measurement this question was
  waiting for. The threshold itself is untouched — it cannot be validated by a test that supplies its own
  inputs — but the number needed to decide is now in the operational log of every machine that runs this.
