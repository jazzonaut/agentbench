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
- **Whether subagent activity should be distinguishable from the session that spawned it.** Both are
  real work on this machine and both are imported, but a heavy workflow's tool calls currently blend
  into the parent project's numbers, and neither table records which is which.
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
