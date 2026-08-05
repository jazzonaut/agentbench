# Changelog

All notable changes to AgentBench are documented here. The project follows [Semantic Versioning](https://semver.org/).

## [0.8.1] - 2026-08-05

Two child processes the windowless tray build had nowhere to put. The first was also the more expensive: a
metric it reported was 96% console allocation.

### Fixed

- **The tray build no longer flashes console windows.** Every probe launched five `internal-noop` children
  for `process.spawn_ms`, and a console-subsystem child with no console to inherit gets a fresh one — so the
  windowless daemon put five console windows on screen four times an hour. They are now spawned
  `DETACHED_PROCESS`, which is also the honest choice for the measurement: `CREATE_NO_WINDOW` only hides the
  window, leaving the console and its `conhost` host process in a number that is supposed to be the cost of
  starting one process. It was never only cosmetic — median per launch, measured at a one-second probe
  cadence: **184.8 ms from the tray build against 7.9 ms from a terminal, now 8.5 ms and 7.9 ms**. Console
  allocation was 96% of the figure, so **expect a large one-off downward step in `process.spawn_ms` collected
  by the tray build**, and treat earlier values from it as a `conhost` startup time rather than a baseline.
  Values from a terminal do not move. Probes also finish sooner: the same 45-second window fitted 19 probes
  before and 28 after.
- **No PowerShell window at login either.** `system::inventory()` named the power source with a
  `Get-CimInstance Win32_Battery` query — several hundred milliseconds of WMI, in a visible window, for a
  fact `GetSystemPowerStatus` answers in one call. It now delegates to the same `platform::on_battery()` the
  probe covariates use, as Linux and macOS already did. The value recorded on Windows changes from
  `ac_or_desktop` / `battery_status_2` to `ac` / `battery`, matching every other platform; it appears in the
  JSON report's inventory and nothing computes on it.

## [0.8.0] - 2026-08-04

The dashboard stops being read-only. Two pages join the machine view: one that starts a benchmark from a form
instead of twelve flags, and one that compares two reports as a page instead of generating a file to open.

### Added

- **A benchmark page at `/bench`.** Preset, target and scratch directories, the network-probe switch and the
  live-Claude options, submitted to the daemon and run as a **separate process** — the daemon samples at
  background priority and holds the database writer, so a benchmark measured inside it would report a slower
  machine than the identical run from a terminal. The page reads that process's `[n/8]` phase lines and draws
  a gauge from them, names the report it produced, and can stop a run in flight. One at a time: a second
  request is refused with `409`, because two benchmarks on one machine measure each other.
- **The form is built from the presets themselves.** *up to 45s · 128 MiB written · 500 small files* comes
  from `bench::preset::Limits` by way of `GET /api/bench/options`, so the page cannot describe a run the
  benchmark will not perform.
- **A comparison page at `/compare`.** Pick two `.json` reports; the browser reads them and posts them to
  your own daemon, which returns the same deltas `agentbench compare` computes. The environment differences
  come first, then the metric table with each row's verdict as a word — colour is a second encoding, never
  the only one. Pairs that cannot mean anything are still refused outright, with the same sentence the
  command line prints.
- **`server.allow_runs` in `watch.toml`**, default `true`. Set it to `false` to keep the dashboard for
  reading and lose no charts. `agentbench bench` is unaffected either way.
- **An application icon**, in the three places there was not one: both Windows executables now carry it as a
  resource, so Explorer, the taskbar and Alt+Tab show the mark rather than the generic one; the
  notification-area icon is it at the size the shell asks for rather than the stock application icon; and
  the dashboard serves it at `/favicon.ico`, which is the one part of this that is not Windows-only. One
  file, `branding/agentbench.ico`, feeds all three. Embedding the Windows resource needs `rc.exe` from the
  Windows SDK, and a build without it **warns and carries on** rather than failing — the executables and the
  tray then fall back to the stock icon, so `cargo install agentbench` never breaks over artwork.

### Changed

- **`compare` now computes a `Comparison` value that markdown and the page both render.** The CLI keeps its
  behaviour and its `--output diff.md`; what changed is that the arithmetic and the direction rules live in
  one place, so a page displaying the same numbers cannot reach a different verdict than the file beside it.
- **The process-launch workload sends its child's output to the null device.** Inherited handles made the
  cost of a spawn depend on what the *parent's* stdout happened to be, so the same benchmark run from a
  terminal, run with its output piped to a file, and run from a logon task with no console measured three
  slightly different things under one metric name; the null device is the same on every path. Expect a small
  one-off step in `process.spawn_ms` history where the change lands. It also quiets the test suite, where this
  program's own executable is the test harness and `internal-noop` reads as a filter matching nothing — 45
  copies of "running 0 tests … filtered out" interleaved with the real results.
- **A `405` names what the requested path accepts** rather than advertising `GET, HEAD` for every path alike,
  now that some paths answer `POST`. An unknown path is a `404` whatever the method: there is no allowed set
  to report for a path that does not exist.
- **The machine page opens on a day of history rather than two.** The wider ranges keep their place in the
  list; what changed is which one is selected before anyone clicks. Two days compressed a working day's
  detail into half the frame and spent the other half on history nobody had asked for.
- **Daemon events are collapsed to the ten most recent, with the rest behind a disclosure.** The page asks
  `/api/status` for a hundred and shows ten, so expanding costs no request — that endpoint runs six
  aggregates over the fact tables to answer, and a click is not a reason to pay for them twice. Fifty rows of
  routine startup chatter was a screenful of scrolling to reach nothing.
- **The verdict tiles fill the width of the row they wrap onto.** There are five verdicts and five is prime,
  so no column count divides them and every layout has a short last row; a grid left that row as a hole,
  four tiles across and the fifth alone beside three columns of empty page. A flex line grows its tiles
  instead, and the basis chooses the split: three and two at full width, both rows flush.

### Security

- **Requests that start work must prove they came from this dashboard.** Loopback binding and the `Host`
  check refuse a request addressed to somebody else's name; neither refuses a correctly addressed `POST` from
  a page the user happens to have open. A write additionally requires `Sec-Fetch-Site: same-origin`, an
  `Origin` naming one of this socket's own names, and `application/json` — the last being what forces a
  browser preflight and what a cross-site HTML form therefore cannot send. Reads are unchanged.
- **The benchmark page cannot ask for an elevated run**, and live-Claude cases are off by default on it even
  for presets that enable them on the command line, with the cost cap bounded at $20. Options arrive as a
  closed set of values placed into a fixed argument template, passed to the operating system as a vector, so
  nothing a browser sends is parsed by a shell.

### Fixed

- **The last turn of every session is imported.** A transcript is read while it is still live, and the
  deriver deliberately withholds its final response then — a truncated span reads as a *faster* machine. The
  position recorded afterwards claimed a complete read, and since neither size nor mtime ever moves again once
  a session ends, the withheld turn was never asked for a second time. Measured on one real corpus: 436 turns
  missing across 441 transcripts, one per file, taking their tokens, model, service tier and first-response
  time with them. Losing them needed no unusual timing — the poll that reads a session's final rows is by
  definition the one running within a poll interval of them being written. The change detector now carries
  whether the read left anything a later read could add, so an incomplete one is revisited until the file
  settles and then never again. A long-lived daemon was the worst affected, because a restart re-read the
  short watermark and healed it by accident. The importer's tests ran with a clock set to 2027 against
  fixtures written seconds earlier, which made every one of them settled before it had been read once and left
  the live path unexercised; one of them now derives its clock from the fixture's own modification time.
- **A panic in a request handler costs the request, not the daemon.** `Server::serve` had no panic boundary,
  unlike the worker bodies in `Supervisor::spawn`, and it runs on the main thread — so an unwind left
  `watch::run_with` through the back door. The workers were detached rather than stopped, each still holding a
  `Sink` clone, and the writer thread they fed can only end once every sender is gone; the unwind therefore
  blocked for ever inside `Store::drop` while holding the instance lock. The result was a process that
  answered nothing, exited never, let no replacement daemon start, and reported `Collecting: yes` throughout.
  Both halves are closed: the per-request work is wrapped and answers `500` with the fault in the event log,
  and `Supervisor` now stops and joins its workers however it goes out of scope, so *any* unexpected exit ends
  the process instead of wedging it.
- **Query parameters no longer reach unchecked arithmetic.** `GET /api/series?to=-9223372036854775808` made
  the default window's subtraction overflow: a panic under `overflow-checks`, which is every debug build, and
  the trigger for the wedge above. It needed no privilege and no write gate — a `GET` any page can issue as an
  `<img src=…>`, since CORS stops an attacker reading the response and not the server processing the request.
  The range and the bucket width both saturate now.
- **A benchmark started from the dashboard is marked in the database that daemon is writing.** `bench`
  resolves the per-user data directory to find a dashboard to annotate, which is right for a run someone
  types and wrong for one the daemon spawned itself: a daemon on `--data-dir` recorded no marker for its own
  run, while the marker and its `bench:` metric rows landed in a different, possibly unrelated database. Both
  halves were contrary to the design — the cliff a benchmark leaves in *this* daemon's passive series went
  unannotated for a later baseline to average in, and rows taken under one configuration accumulated in
  another daemon's history with nothing to distinguish them. `bench` takes a `--data-dir` that affects nothing
  but the marker, and the dashboard passes its own.
- **`limit` is honoured by every series family, and truncation is reported by all of them.** The probe,
  conditions and session series read the parameter and ignored it, and both run-shaped families hardcoded
  `truncated: false` while their SQL capped the rows — so a partial series described itself as whole. Worse,
  the cap was `ORDER BY ts LIMIT n`, which keeps the *oldest* n: a range wider than the budget silently
  dropped the recent end, the exact opposite of the passive series' documented policy of spending the budget
  newest-first. All four families now agree, which also matters between the two frames that share a cursor.
- **A daemon that cannot start a benchmark says so with a `500`.** Every failure from `Registry::start` was
  reported as `409 Conflict` with "a benchmark is still running" as its rationale, including a reports
  directory that could not be created and a child that would not launch — so the page displayed "the machine
  is busy" for a full disk.
- **A request body with no `Content-Length` is read rather than discarded.** `tiny_http` reports no length
  for a chunked request and decodes the chunks through the same reader; the absence was read as zero, and a
  valid chunked `POST /api/compare` was answered `unreadable report: EOF while parsing value` — an error
  blaming the document for a body the server had thrown away. Browsers' `fetch` sends a length for a string
  body, so the dashboard's own pages were unaffected; `fetch` with a stream body, and most other clients, are
  not.
- **A finished run's `ended_ms` is when its exit was seen, not when somebody asked.** The stamp was taken
  where the run is concluded, which is the same instant only while the page is polling. Nothing polls a page
  that has been closed, and the next `start` request then concluded the previous run at whatever time it
  happened to arrive — reporting a two-minute benchmark as having taken an hour. The exit is stamped when it
  is first observed, which the one-second poll keeps within a second of the truth. The exact end remains in
  the run marker `bench` writes for itself.
- **A standing probe failure no longer silences a different one.** One counter served every kind of problem
  and spoke on every hundredth, so an unwritable scratch volume failing every fifteen minutes could suppress
  the *first* report of an unrelated fault for up to 99 occurrences. The burst policy is per message now,
  with a bound on how many distinct messages are remembered, because a failure's text can carry a byte count.
- **An empty machine identity is refused rather than registered.** `machines.id` is the hashed hostname; every
  measurement in the file is attributed to it and every query filters on it, so an empty one is not a machine
  with a missing name but one key that every row belongs to. Only reachable by constructing an `Inventory` by
  hand, and load-bearing enough to be worth a guard.
- **A response's span is recorded only when something proves the response ended.** `generation_ms` is the
  interval from a request's first row to its last, and "the transcript has not changed for a while" does not
  establish the last one: an mtime moves only when a row is appended, so a response that pauses for longer
  than the settle window looked finished while it was still generating. The turn is still kept — its tokens
  and its first-response time are known — but the span is left absent, because a truncated span is *shorter*
  than the truth and the token rate derived from it is therefore higher. The artefact read as a machine
  responding faster rather than as an error, which is the one shape of wrong number 0.7.0 set out not to
  produce. There was no second chance to correct it either: the duplicate turn a later pass builds is
  discarded by the unique index on the request id, so the first figure written was final. For the same
  reason an unclassified transcript row no longer ends a response — a prompt and a tool result prove one
  finished, an uninterpreted row can be interleaved with it.
- **Whole-machine disk throughput no longer counts stacked devices twice on Linux.** Partitions were
  excluded, correctly, but `/sys/block/dm-0` and `/sys/block/md0` exist as surely as `/sys/block/sda` does,
  so a write through LVM, LUKS, RAID or devicemapper was counted once for the mapping and again for the disk
  beneath it. That is the Ubuntu installer's default layout and how most container hosts are built. Because
  the figure is compared against a threshold, the doubling meant 20 MiB/s of contention firing at 10 MiB/s of
  real traffic and quietly shrinking the comparable subset. The kernel's own `slaves` directory is now the
  test, so no list of device-name prefixes has to be kept up to date here.
- **A performance counter that reports failure in its status field is no longer read as a value.**
  `PdhGetFormattedCounterValue` can return success while putting the real answer in `CStatus`, in which case
  the formatted number means nothing — and a garbage *zero* would have passed the plausibility check, which
  for a disk rate is a claim that the machine was quiet.
- **A partially available set of counters says so.** The clock and the disk counter are independently
  optional, so a machine with the disk object disabled records its clock perfectly well; the startup log
  told it that neither was being recorded. That line exists to distinguish an absent covariate from a broken
  collector, so overstating the gap defeated its only purpose.

## [0.7.0] - 2026-08-04

The "and what changed?" half of the dashboard's question. A verdict that says *worse* can now also say what
was different about the day it is describing, and every series the daemon collects is reachable from the page
— enforced by a test that fails the build when one is not.

### Added

- **Probes record the conditions they ran in.** Four covariates on every run: the CPU clock as a percentage
  of nominal, whole-machine disk write throughput, free space on the scratch volume, and the raw agent CPU
  figure that `agent_active` was derived from. Between them they are the difference between a verdict saying
  `single-core CPU worse by 22%` and one that can also say the part was running at two thirds of its usual
  clock.
- **A busy disk now counts as contention.** `contended` was three CPU thresholds, so a probe that ran while
  an update, a backup or a cloud sync wrote gigabytes read slow at 15% CPU and went into the baseline as
  clean data — and two of the five judged series are filesystem measurements. Validated against a live
  daemon under a sustained writer: all five probes were tagged, at 769–1066 MiB/s with CPU at 16–22%.
- **Every probe records the three largest consumers on the machine.** Names and CPU, from the process walk
  the prober already does, which is the difference between "small-file operations dropped at 14:00" and
  "…and `MsMpEng` was at 180% of a core". The daemon's own process tree is excluded: an early version ranked
  `agentbench.exe` itself, which was the previous probe's own workloads.
- **The passive stream records the agent's and the scanners' disk write rates.** Attributable, unlike the
  whole-machine figure above, and blind in a way that is documented rather than hidden: an unelevated
  process reads exactly zero bytes for every SYSTEM-owned writer while still reading its CPU.
- **Sessions record how long a response took to arrive** (`generation_ms`, the span from a request's first
  row to its last) **and whether a subagent produced it** (`sidechain`). The first exists to be divided into
  the token count, which cancels the thing that makes a raw wait unjudgeable — a model choosing to think
  for longer. The second was already being parsed and discarded, and matters more than expected: 38% of the
  transcripts on the development machine are subagent transcripts whose work was blended into the parent
  project's numbers.
- **A verdict now says what was different about today.** A tile reading `worse -8.0%` gains a third line —
  `clean probes: clock 128% today against 136%` — in its own slot beside the existing evidence and power
  caveat, because an explanation and a qualification are different claims. `--status` prints the same line. A covariate is reported only when today's median falls outside its *own* baseline
  band, computed the same way the verdict's band is — one sensitivity rule for the whole tool rather than a
  hand-picked threshold per covariate. It reads `clean probes:` and not `today` because every figure in it is
  a median over the uncontended subset, which is the population the verdict itself used.
- **A fourth chart frame: the conditions each probe ran in.** Clock, whole-machine disk writes, free space and
  the three CPU figures behind the contention tag, sharing a cursor with the probe frame above it so a dip in
  one can be read against the other. Two of the six are charted but never quoted in a verdict: over
  uncontended runs the scanner and agent figures are capped by their own thresholds at a tenth and a fifth of
  one core, so a large relative move there explains nothing about a throughput drop.
- **Every series the daemon collects now has a button on the page**, where twelve of them were collected and
  unreachable. The System and Agent frames gained switches like the probe frame's — nine choices and eight —
  and each of the twenty-seven choices across the four frames carries a caption and a note saying what stops
  the number being misread: per-core versus whole-machine scales, absent-is-not-zero, and what the covariate
  window can and cannot see. Two guard tests keep it that way: one fails the build when a collected series has
  no button, the other when a button names a series the server would refuse.
- **Response speed, as tokens per second of generation** (`output_tokens_per_s`), which divides out the thing
  that makes a raw wait unjudgeable. It covers multi-row responses only: about 37% of turns are a single row
  and have no measurable span, and those are excluded rather than counted as instantaneous.

### Changed

- **The clock is recorded as a ratio, not in MHz, because the MHz figure available to an ordinary process on
  Windows is a static registry value.** `sysinfo` and WMI both reported exactly 3801 MHz across thirty-six
  readings spanning 8% to 98% CPU. The live reading comes from a performance counter instead and moved from
  136% of nominal at idle to 129% with every core loaded. This needs one additional feature on a dependency
  the project already had, and no new crate.
- **The live probe tile states what a run was competing with instead of inferring it from three fields.** The
  thresholds that define contention now live in one module that the writer, the analysis and the tile all read,
  so the tile gained the disk arm it had no way to know about: a probe tagged while gigabytes were being written
  at 13% CPU used to be described as "the machine was busy". The alternative was hardcoding 20 MiB/s in the
  page — a second authority for a number this project's ADR says will be revised.
- **A chart's axis and tooltip follow the unit of the series being displayed, which the server reports.** Three
  of the four frames change metric at runtime, so a panel that kept the formatter it was built with was a
  wrong number waiting for its first switch — byte rates would have rendered as `1,048,576 B/s`. Bytes and byte
  rates now scale like the latency formatter already did, staying precise at the quiet end where 977 KiB/s and
  1.1 MiB/s have to share an axis.
- **Card titles name the frame's subject — "System", "Agent" — rather than one measurement.** "System CPU" was
  a title that named one of nine choices and was wrong for the other eight. What is being measured is named by
  the pressed button, the caption beneath it and the tooltip, all of which change with the selection. The page
  is one card taller as a result; the default reading is unchanged, so a reader who touches nothing sees the
  three frames they saw before plus the conditions the runs above them ran in.
- **The schema was reset to a single migration.** Migrations v2–v5 corrected the development line and were
  never carried by a release, so they described an upgrade from a database that exists nowhere. **A database
  written by 0.6.x cannot be read by this build and has to be moved aside**, which the error message says.
  Transcript history is unaffected — a backfill regenerates it — but sample and probe history is lost, so
  probe verdicts read `insufficient` until four days have accumulated again.

### Fixed

- **A write rate is absent, never zero, on the tick that rediscovered the processes it measures.** The first
  I/O delta `sysinfo` reports for a newly seen process is its whole lifetime's traffic; unguarded, one tick
  of a full process table reported 12.2 GiB written "in one second". Absent rather than zero because zero is
  a claim the disk was quiet, which is exactly what a busy machine would then look like.

## [0.6.1] - 2026-08-04

### Changed

- **An agent process name has to name a process, not appear inside one.** `agent_process_names` was matched
  as a case-insensitive substring, and an agent match is expanded to its whole descendant tree whose CPU is
  then summed against a threshold meaning "an agent is working" — so a name matching more broadly than
  intended added not a process but everything that process had ever started. Names now match a whole process
  name, with or without its extension, so `"claude"` still finds `claude.exe` and no longer finds
  `claude-monitor.exe`. Scanner names are still fragments, deliberately: that list is written as fragments
  (`msmpeng` for `MsMpEng.exe`), and a scanner is recorded as one process rather than a tree.
- **The daemon says what it thinks the coding agent is.** One line in its own event log at startup, naming
  how many processes the configured agent names matched and how many scanner processes were found. This was
  invisible, and it is the setting most likely to be wrong: a name matching most of a developer's machine
  tags every probe as contended, which empties the comparable subset and leaves every verdict reading
  `insufficient` indefinitely, with nothing anywhere to say why.
- **The day's totals moved off the dashboard's five-second poll to a new `/api/today`.** `/api/live` is
  meant to be one row per stream, which is what makes it affordable twelve times a minute, but it also
  aggregated the whole day: two counts, a median and a cache ratio, recomputed on every tick while the
  server answers one request at a time on its own thread. None of those numbers can move faster than the
  importer that feeds them, which polls every thirty seconds at best. They are now on the minute cadence
  beside status and verdicts, and the page ticks "since the last agent activity" against its own clock from
  the timestamp it already holds, so that tile still reads at five-second resolution for no query at all.
- **The dashboard answers `GET` and `HEAD`, and refuses everything else with a `405`.** Dispatch was on the
  path alone, so `POST /api/series` was answered as though it were a `GET`. Harmless while every handler is
  a read through `Reader` — and a hole in the router rather than in the new handler on the day one is not,
  which is the harder place to notice it. One match arm now, while it costs one match arm.
- **`samples_raw_days = 0` means the same thing wherever it is written.** Three paths disagreed: the loader
  took it at its word, the control centre refused to accept the value and then raised it to a day on save
  anyway. Keeping no raw samples is a coherent request — every minute is summarised as soon as it has
  finished, which is exactly what the rollup already did — so it is now accepted by all three. The baseline
  window keeps its floor of one day, because zero there asks for a comparison against nothing.

### Fixed

- **The tray daemon collects `process.spawn_ms` again, and no longer flickers five icons per probe.** The
  process workload launched `current_exe()` with the hidden `internal-noop` subcommand, which is right for
  `agentbench.exe` and wrong for `agentbench-tray.exe`: that binary started a daemon whatever its arguments
  said, so each child loaded the configuration, failed to take the instance lock its own parent was holding,
  added a notification-area icon on the way out and exited non-zero. For anyone who had turned on "Start in
  tray" the `process` phase therefore failed on *every* probe — a metric the README documents, permanently
  absent from history — while five short-lived tray icons appeared four times an hour, and the recurring log
  entry was burst-limited to every hundredth occurrence so it read as a rare event rather than a permanent
  one. The workload now resolves the console build beside it, the same rename the logon task uses, and the
  tray build answers `internal-noop` by exiting successfully.
- **A `watch.toml` that only slows the sampler down no longer refuses to start.** Setting
  `sample_interval = "60s"` and nothing else left the shipped 30s idle default *faster* than the active
  interval, and the loader rejected that pair — `collect.sample_interval_idle (30s) must not be shorter than
  collect.sample_interval (60s)` — naming a key the user had never written, for the single most obvious edit
  there is to make to that file. Both the command line and the control centre clamped the same pair silently;
  only the file rejected it. There is now one rule, applied by all three: the idle cadence is raised to the
  active one, and the startup event reports the pair that was used.
- **`sample_interval_idle` from the file is honoured by `agentbench dashboard`.** It applied the rule that
  scales an idle cadence down to the shipped 1:6 ratio unconditionally — including when no interval flag had
  been passed at all — so a file asking for `5s` active and `300s` idle was sampled every 30s when idle. The
  tray build has no command line and honoured the file, so one configuration produced two behaviours
  depending on which executable the logon task pointed at. The scaling now applies only where a flag actually
  overrode the active interval and said nothing about the idle one, which is the case it exists for:
  `--sample-interval 1s` still pulls an untouched idle cadence down to 6s.
- **One abandoned tool call no longer counts a fresh import error every poll, for ever.** A pass caps how
  much of a transcript's tail it will re-read to recover measurements still waiting for their other half, and
  it spent that budget by clamping the resume offset to `end - 1 MiB` — an arbitrary byte position, where
  every offset the importer records has to be the start of a line. So an unanswered `tool_use` followed by
  more than a megabyte of further conversation — an interrupted call, an Escape, a session that crashed —
  left the watermark inside a line, and every subsequent pass seeked there, read a fragment, failed to parse
  it and counted an import error: ~2,880 fabricated errors and ~2.8 GiB of re-reads a day against a number
  the dashboard and `--status` both show prominently. The budget is now spent deciding which rows still count
  as open, so the recorded offset is a line boundary by construction.
- **A healthy daemon on a quiet machine is no longer reported as stalled.** Collection was judged stale
  after a fixed two minutes, while the idle sampling cadence is the user's to choose and legitimately reaches
  minutes — an active interval of 60s scales to six. At that setting `/api/status` returned
  `collecting: false` and the page drew `stalled · last sample 4m ago` beside a warning dot on a daemon that
  was working perfectly. The bound now follows the configured idle cadence — two intervals, never less than
  two minutes — and `dashboard --status` reaches the same verdict from the same rows.
- **`tool_versions` no longer grows a redundant row per import pass.** The row was keyed on the instant it
  was recorded, and the importer's deriver state is per-pass, so every poll that read new bytes wrote another
  row for a version that had not changed: roughly one row per poll while a session is live, ~2,880 a day, in
  a table nothing prunes and that `/api/annotations` grouped over in full every sixty seconds. The row is now
  keyed on the version and keeps the earliest sighting, which is the only one anything ever asked for.
  Migration v5 collapses the history already on disk; nothing on the page changes, which is exactly why this
  would have gone unnoticed until the query got slow.
- **A failed live-Claude case no longer discards a completed benchmark.** Every other phase degrades a
  failure to a warning; this one propagated, so a failed `claude` spawn or an unreadable fixture threw away
  the minutes of CPU, memory, filesystem, SQLite, process and network measurement already taken and wrote no
  report at all — for the one phase that depends on an external program behaving. It is now a warning like
  the rest. Cancellation stays an exception: it is a request to stop rather than a phase that failed, so it
  still unwinds and still cleans up the temporary files on the way out.
- **A baseline's measurement count no longer includes days the band excluded.** A day dropped for a
  non-finite value still contributed its measurements to the "N in the baseline" figure and to the thin-day
  caveat's arithmetic, so the disclosure that exists to say how much evidence is behind a verdict slightly
  overstated it.
- **An agent that restarts between two discovery passes is no longer invisible for a minute.** The sampler
  refreshes only the pids it discovered, on a fast cadence, and re-enumerates the whole process table on a
  slow one; the refresh has always reported how many watched processes are still alive, and both callers
  discarded it. When every watched process has gone there is nothing left for the next ticks to measure, so
  that case now rediscovers immediately — one process-table walk, in exactly the situation where the
  alternative is measuring an empty set.
- **The dashboard no longer renders a blank page against a cached copy of its own script.** Assets were
  served as `Cache-Control: public, max-age=604800, immutable` at a URL carrying no version, while
  `index.html` was sent `no-store`. So a browser that had opened the dashboard within the previous week
  paired a freshly fetched document with a week-old `app.js` and ran each against the other's markup. In
  0.6.0 that was fatal rather than cosmetic: the probe panel's element id changed, the stale script looked
  up the old one, and `Cannot read properties of null (reading 'closest')` was thrown while the module was
  still evaluating — which stops the rest of the file and leaves the whole page empty. Assets now carry an
  entity tag derived from their bytes and are sent `no-cache`, so the browser keeps its copy and finds out
  in one loopback round trip whether it is still the right one.
- **A panel the markup does not define no longer takes the rest of the dashboard with it.** A missing chart
  container is reported to the console and its chart skipped, and the probe metric switch is left undrawn
  rather than throwing from inside its own renderer. A test asserts that every element id the scripts look
  up exists in the markup they are served beside, which is the drift that caused this.
- **The startup rows on the control centre show what they just did, and "Start in tray" is no longer
  discarded.** Toggling "Run at login" registered or removed the task and then went on displaying the
  reading taken when the screen opened, so the row still said "off" until the screen was reloaded with `r`.
  The row reading stale was the smaller half of it: "Start in tray" and "Delay after login" only re-register
  a task that exists, they asked that same stale reading whether one did, and so a tray choice made straight
  after switching autostart on was written nowhere and gone by the next time the screen opened — while the
  message line said the login task would start in the tray. Every change to the task now re-reads it, and
  the delay and tray choice follow that reading, so a registration that was refused leaves the rows
  describing the task that is still there rather than the change that did not happen. Both rows also say
  which of the two things happened: applied to the task, or recorded for when autostart is switched on.
- **An append that does not move a transcript's timestamp is imported.** The modification time was the only
  change detector, and it is not a reliable one: file times are coarse on some filesystems, and on Windows
  the directory entry a scan reads is updated lazily for a file another process still holds open — which is
  every live transcript. Rows appended inside the recorded tick were invisible until some later append moved
  the timestamp. The length is compared as well now, which the scan already had in hand.
- **A machine row follows the machine it describes.** The upsert refreshed `last_seen`, `os_version`, `cpu`,
  the core count and the memory size, but not `os` or `architecture` — so a reinstalled machine kept
  describing the system it was first seen running, under the id every measurement in the file is attributed
  to. `first_seen` remains the one column that does not move.
- **A slow query no longer makes the dashboard queue more work behind it.** Three unguarded `setInterval`
  loaders against a server that answers one request at a time: a slow query did not delay one poll, it
  queued every poll behind it, so the moment the machine was busiest was the moment the page asked it for
  the most. A tick that arrives while the previous one is still in flight is now dropped — every payload is
  a snapshot rather than a delta, and the next tick is already scheduled. Clicking a range or the contention
  filter still redraws immediately.
- **The shipped `watch.toml` no longer carries a developer's directory as its example.** The commented-out
  `scratch_dir` line was written on first run with the path it happened to be authored on.

## [0.6.0] - 2026-08-04

### Added

- **The dashboard's probe frame switches between the four measurements a verdict is computed for.** One
  frame with a switch in its head rather than four stacked charts: small-file operations, sequential write,
  SQLite lookup and single-core CPU share nothing but a workload — ops/s, MiB/s, ms and Mops/s — so stacking
  them would put four y-axes on a page whose charts stack precisely because they can be read down one shared
  cursor. The set is exactly the judged one, so every verdict tile has a line to check it against, and a
  test fails if the two ever drift apart. Axis and tooltip both come from the unit the server reports for
  the series, so a fifth metric is one entry in a list rather than a formatter.
- **Every tile on the dashboard says what it measures.** A mark in the corner, and a note behind it naming
  what the number is and what would make it easy to misread — that the agent's CPU is counted per core
  where the machine's is not, that the process count is a minute older than the tiles beside it, that an
  absent value is not a zero, that a contended probe is recorded and charted but never judged. Verdict
  tiles explain the rule that produced the word: which runs were eligible, which direction is good, and
  what "normal" means. A button rather than a `title` attribute, because a native tooltip cannot be
  reached by keyboard, does not exist on a touch screen, and advertises nothing; it opens on hover, on
  click and on focus, and closes on Escape.

### Fixed

- **"Run at login" registers the task without administrator rights, and can switch itself back off.** It
  never could: the task was registered with `schtasks /Create /SC ONLOGON`, and `schtasks` has no way to
  scope a logon trigger to a user, so it wrote one that fires at *any* user's logon — an operation only an
  administrator may perform. Unelevated the row failed with `Access is denied`. From an elevated session it
  appeared to work, and left behind a task whose security descriptor grants Administrators full control and
  the user only read access: the row then read "on", and switching it off failed with `schtasks could not
  remove the logon task: ERROR: Access is denied`, permanently. The definition is now written as XML with
  the trigger scoped to the account that registered it, which needs no elevation and produces a task that
  account can remove. Registering while elevated is refused with the reason rather than producing another
  one of these, and a leftover task from an older version is removed after a single elevation prompt.
- **A logon task is no longer killed after three days, or when a laptop is unplugged.** `schtasks` defaults
  a task to a three-day execution limit, to refusing to start on battery, and to stopping when the power is
  disconnected — three ways for a daemon whose entire purpose is a long unbroken baseline to stop
  collecting without saying so. All three are now set explicitly.
- **"Start in tray" points the task at the windowless build.** It set a flag and left the task pointing at
  the console executable while dropping the subcommand that executable needs, so the registered task
  launched `agentbench.exe` with no arguments at every login. It also never read back — the setting is
  recovered from the name of the program the task starts, so a task recording "tray" reported it as off on
  the next visit. Installing now copies both builds, so the one the task names is the one that is there.
- **Installed paths no longer carry a `\\?\` prefix.** The running executable is canonicalised to compare
  it against the install directory, which on Windows returns the extended-length form. That form was then
  written verbatim into the logon task and printed on the control centre.

### Changed

- **The live tiles are updated in place rather than rebuilt on every poll.** A section is only rebuilt
  when the set of tiles in it changes. Rebuilding five times a minute was invisible while a tile was
  nothing but text, and became a bug the moment one carried a note a reader could open: the poll would
  close it under them and take the keyboard focus with it.

- **The dashboard's "uncontended probes only" filter starts off.** It started on, which is right for
  comparability and wrong as a default. A machine with a coding agent on it produces mostly contended
  probes, so what a new user actually saw was an empty frame — on a machine that had been probing all day —
  under a message explaining that the first probe takes fifteen minutes. An empty chart reads as a broken
  tool rather than as a filter doing its job, and the message compounded it by naming a cause that had
  already passed. Every run is drawn now, the live tile still says whether the last one was contended, and
  verdicts are unaffected: they use uncontended runs only, with no override.

## [0.5.0] - 2026-08-03

### Added

- **The control centre can start collection, compare reports, and erase what has been collected.** The
  three things the screen previously sent you back to the command line for. Starting the daemon passes the
  data directory explicitly, so a screen opened with `--data-dir` cannot start a daemon that collects
  somewhere else. Comparing takes the two newest reports in the working directory, older as the baseline,
  writes the comparison beside them and opens it — the guards that refuse to compare two different presets
  are the same ones `agentbench compare` uses, because they moved into a shared function rather than being
  reimplemented. Erasing asks for a second Enter within five seconds, refuses while a daemon holds the
  database, and reports the size it is about to remove before it removes it.
- **Two new charted session series, `tool_edit_ms` and `tool_search_ms`.** `Edit` and `Write` in the
  first; `Grep` and `Glob` in the second. Both were previously pooled into `tool_read_ms` and are now
  visible on their own. `tool_search_ms` is the closest thing the tool has to a directory-walk
  measurement, which is what moves when a filter driver or a cloud-sync placeholder provider gets into
  the path of an enumeration.

- **`agentbench` with no arguments opens a control centre.** One screen showing whether collection is
  working and letting every setting that was previously a flag or a hand-edited TOML key be changed in
  place: startup, install location, `PATH`, sampling and probe intervals, transcripts, retention, the
  baseline window, and the dashboard's port. Changes apply as they are made rather than behind a save key,
  and each one reports what it did — including when a value was clamped to the configuration's floor, and
  to what. Settings are written with `toml_edit`, so the comments that document `watch.toml` survive being
  edited.
- **Collection can start at login on Windows.** A toggle registers an unelevated `ONLOGON` scheduled task,
  which needs no administrator rights and therefore raises no consent prompt — Windows refuses to show one
  at logon in any case. The default two-minute delay is deliberate: probes that fire during the login storm
  are recorded as contended and drop out of the baseline, so a daemon that started immediately would
  collect samples it could not later compare. The task is the only record of the setting; it is read back
  rather than mirrored, so removing it in Task Scheduler is reflected honestly.
- **A tray icon, in a new `agentbench-tray` executable.** The Windows subsystem is fixed at link time, so
  this is a second binary rather than a flag: giving the main one no console would take out `top`, the
  control centre and every line of report output. It runs the collector with no console window and a
  notification-area icon whose menu opens the dashboard, opens the settings screen, or stops collecting —
  the last through the same cooperative shutdown a Ctrl+C uses, so the database closes cleanly rather than
  the process being torn down mid-write.
- **An install action and a `PATH` toggle.** Installing copies the executable to
  `%LOCALAPPDATA%\Programs\AgentBench`, and both `PATH` and the logon task point at that copy. Running from
  a Cargo build directory disables both rows and says why: `cargo clean` deletes that path, after which
  `agentbench` becomes "command not found" and the logon task starts nothing, neither with any error the
  user would see. `PATH` is edited through `HKCU\Environment` with the value's registry type preserved, and
  a `WM_SETTINGCHANGE` broadcast so a new terminal picks it up.
- **A benchmark can be started with administrator rights from the control centre**, which is where the one
  consent prompt in the design lives. The collector itself stays unelevated: it gathers identical data
  either way, and elevating it would turn any fault in its loopback HTTP server into an
  elevation-of-privilege one.

### Changed — the terminal interface

- **Every terminal screen is rebuilt on `ratatui`**, sharing one theme and one set of widgets. Colour now
  has a job — status colours mean state and nothing else, series colours can never be mistaken for a
  verdict, and text keeps text ink instead of whole lines being tinted. Panels adapt to the terminal's size
  rather than assuming there are enough rows, and the live process view keeps a minute of history, so a
  spike is still visible a moment after it happens.
- **`bench` no longer prints its phase lines into a screen that is overwriting them.** Progress reached
  stdout while the terminal UI held the alternate buffer, which is what the old note about timings being
  "emitted after the dashboard closes" was apologising for. Phases now go to a sink: a gauge under the
  terminal UI, and the same `[n/8] label` lines on stdout for a redirected or `--no-tui` run, byte for
  byte as before.
- The Windows release archive now contains `agentbench-tray.exe` alongside `agentbench.exe`.
- `cargo clippy` runs on every platform in CI, not only Linux. The lint job's `-D warnings` had never seen
  a `#[cfg(windows)]` module, which is where nearly all of this release's new code lives.

### Changed — measurement accuracy

- **`tool_read_ms` is the latency of `Read` alone**, where it used to pool `Read`, `Grep`, `Glob` and
  `Edit`. It is the only session series that carries a verdict, and pooling put a composition confound
  inside it: measured over 15,035 real tool calls, the four medians are 11, 72, 223 and 35 ms, and the
  pooled daily median correlated with *the share of calls that happened to be reads* at r = −0.86. Three
  quarters of the movement in the judged series was the model's choice of tool rather than the machine.
  One day in the sample sat near the month's worst pooled figure while its `Read` median was the month's
  best. **Values before this release are not comparable with values after it.**
- **A slow small-file result no longer blames an idle security scanner.** The threshold was 2.0 against a
  reading that is a percentage of *one core*, so on a sixteen-core machine it fired at one eightieth of
  the machine — which anything installed clears. It is now 10.0, the same value the dashboard's own
  contention tag uses for the same reading, and the evidence line states the scale it is on. The scale is
  documented once, on `process_tree::TreeUsage::cpu_percent`, rather than at each reader.
- **The benchmark's sampler warms up before it records, and walks the process table four times less
  often.** Per-process CPU needs three refreshes before it is a measurement rather than a `0.0`, so the
  first sample of every run previously reported no scanner activity whatever was happening — and because
  the scanner evidence takes the maximum over a run, a missing real reading mattered while a phantom quiet
  one did not. The whole-machine figure was inflated on that sample too: 9.0% against 4.4% once settled.
  The walk itself costs about 10 ms of every 500 ms interval on Windows, spent observing during the two
  measurements most sensitive to someone else using the machine, so it now runs every two seconds while
  the cheap counters are still read every 500 ms.
- **`network.https_latency_ms` establishes the connection before it starts timing**, when more than one
  sample is asked for. The first request through a fresh client pays DNS, TCP and TLS; the rest reuse the
  pooled connection, and at every preset's sample count the reported p95 *was* that first request, since
  `round((n - 1) × 0.95)` reaches the last index for any n up to 11. The diagnostic threshold now reads
  the median and reports the slowest request as evidence under its own name. A single-sample probe is
  left alone: warming up would double the daemon's outbound requests, and one cold request every fifteen
  minutes is consistent with the one before it.
- **`cpu.single_mops_s` discards a 25 ms warm-up.** An idle processor takes tens of milliseconds to raise
  its clock, and how long depends on the power plan and how idle it had been — a systematic bias, and a
  larger one for the prober's 200 ms reading than for a benchmark's seconds, which is backwards.
  **Values before this release are not comparable with values after it.**
- **`filesystem.small_file_total_ms` is informational.** It is `seconds × 1000` where
  `filesystem.small_file_ops_s` is `4 × count / seconds`: with the count fixed by the preset, each is the
  reciprocal of the other, and treating them as two comparable metrics counted one measurement twice in
  the comparison table.
- **Two metric descriptions now say what the number is.** `filesystem.sequential_read_mib_s` reports the
  cached read path, not the device: it measures 4,820 MiB/s at the quick preset's 64 MiB and 9,447 MiB/s
  at the standard preset's 512 MiB, against 1,463 MiB/s written to the same file moments earlier. And
  `memory.read_gib_s` is a rate at which memory can be *reached*, sampling one byte per cache line, so it
  is not comparable with the write figure beside it.
- **The first probe of a daemon session takes an extra priming reading.** Its covariates were previously
  a scanner at exactly 0.0% and an agent reported inactive whatever either was doing, so a probe that
  competed with a working agent could be recorded as comparable. Measured on one machine: the first probe
  said no agent was active where the second and third, with the same thirty-seven-process agent tree in
  front of them, said one was.
- **The terminal screens have a colour for their own structure.** The rewrite above gave colour a job but
  gave chrome none, so every panel title, heading and key hint was grey and the screens read as greyscale whatever the
  data was doing. Cyan is now reserved for structure — titles, section headings, bracketed key
  hints, the control centre's focus marker — and reserved in both directions: it left the series palette, so
  the process-tree CPU plot keeps its blue while the system CPU plot and the benchmark's phase gauge move
  from cyan to magenta. A hue can still only mean one thing, and the control centre's status band now tints
  the one word in its title that is a state rather than a label: green while collecting, dim when not.

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
- `bench --scratch-dir` moves the filesystem workloads out of the target directory. The default still
  writes inside it, since the disk numbers are meant to describe that volume, but up to two gigabytes
  landing inside a repository wakes IDE indexers and file-watching test runners, and the report attributes
  that to the disk. The live file-seek fixture stays under the target directory, where the agent's working
  directory is.
- Elevation is read from the process itself — `geteuid` on Unix, the process token on Windows — rather than
  by spawning `id -u` or `net session` on every `inventory()`.
- The dashboard's process-count tile says in its tooltip that the number comes from the sampler's discovery
  pass and can be up to a minute old.
- The README's privacy section states what the live file-seek case gives a model access to: read-only tools,
  no prompts, and anything readable beneath `--target-dir` for the duration of that case.
- CI runs doctests. `cargo test --all-targets` silently skips them.

### Fixed

- **A panel title is no longer dimmed by its own border.** Titles are drawn over the border area, so the
  deliberately recessive border style reached them and every bordered heading in the tool came out bold
  *and* dim at once — which is part of why the screens read as greyscale. The heading style now says it is
  not dim, and a test renders a panel and checks the pixels, since neither end of that interaction says
  anything about the other.
- `filesystem.sequential_write_mib_s` divides the bytes actually written by the elapsed time rather than
  the bytes requested. Every caller today passes a multiple of the 1 MiB block size, so the two agreed;
  the point is that they stop agreeing silently.

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
- A failing HTTP accept no longer stops the daemon. `serve` returned on the first error and `watch::run`
  fell straight through to shutdown, so a transient socket failure ended collection and logged it as
  "stopping HTTP server". Accepts are now retried, five consecutive failures give the page up explicitly,
  and collection continues either way.
- The probe scratch directory is emptied when the daemon starts, which is what the README already claimed.
  It was emptied when the first probe fell due — fifteen minutes later by default — and never at all when
  probes were disabled, so a daemon killed mid-workload could keep its leftovers indefinitely.
- Ctrl+C is no longer delayed by up to five seconds when it arrives during a worker's restart backoff.
- `compare` no longer panics on a report whose `run_id` is shorter than eight characters, or whose eighth
  character is multi-byte.
- An interval like `"999999999999999999999d"` is refused instead of wrapping silently into a short one.
- `collect.sample_interval`, `collect.sample_interval_idle` and `collect.discovery_interval` have floors,
  as `probe_interval` and `poll_interval` already did. A millisecond sampling cadence turned the sampler
  into a spin loop, and discovery enumerates the whole process table; the CLI overrides apply the same
  floors.
- A series that exactly fills its point budget is no longer reported as truncated, so a chart stops
  claiming it is missing history it has every point of.
- A baseline band whose floor equals its measured spread reports itself as floored rather than measured.
- The per-pass transcript dedupe set is bounded like every other map beside it.

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

[0.8.1]: https://github.com/jazzonaut/agentbench/releases/tag/v0.8.1

[0.8.0]: https://github.com/jazzonaut/agentbench/releases/tag/v0.8.0

[0.7.0]: https://github.com/jazzonaut/agentbench/releases/tag/v0.7.0

[0.6.1]: https://github.com/jazzonaut/agentbench/releases/tag/v0.6.1

[0.6.0]: https://github.com/jazzonaut/agentbench/releases/tag/v0.6.0

[0.5.0]: https://github.com/jazzonaut/agentbench/releases/tag/v0.5.0
[0.4.0]: https://github.com/jazzonaut/agentbench/releases/tag/v0.4.0
[0.3.0]: https://github.com/jazzonaut/agentbench/releases/tag/v0.3.0
