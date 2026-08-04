# Changelog

All notable changes to AgentBench are documented here. The project follows [Semantic Versioning](https://semver.org/).

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

[0.6.1]: https://github.com/jazzonaut/agentbench/releases/tag/v0.6.1

[0.6.0]: https://github.com/jazzonaut/agentbench/releases/tag/v0.6.0

[0.5.0]: https://github.com/jazzonaut/agentbench/releases/tag/v0.5.0
[0.4.0]: https://github.com/jazzonaut/agentbench/releases/tag/v0.4.0
[0.3.0]: https://github.com/jazzonaut/agentbench/releases/tag/v0.3.0
