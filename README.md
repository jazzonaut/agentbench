# AgentBench

[![CI](https://github.com/jazzonaut/agentbench/actions/workflows/ci.yml/badge.svg)](https://github.com/jazzonaut/agentbench/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/jazzonaut/agentbench)](https://github.com/jazzonaut/agentbench/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Your coding agent feels slow. AgentBench works out whether it is the machine, and whether it changed.**

It is a local tool for developer machines running Claude Code, and it answers three questions that need
three different kinds of evidence:

- **How fast is this machine right now?** Agent-shaped workloads - small files, SQLite, process launch,
  loopback and disk - run on demand and produce a report you can compare against another machine.
- **Is it slower than it was last week, and what changed?** A background collector keeps a record: passive
  system samples, a controlled micro-workload every fifteen minutes, and real timings read out of your own
  Claude Code transcripts. It reports today against the days before it, and says when it does not know.
- **What is it doing right now?** A live terminal view of a process tree, for watching an agent work.

Nothing is uploaded. The collector makes one outbound request, a timed HTTPS round trip carrying no prompt
and no credentials, and that request has its own switch. Antivirus, proxy, power and OS settings are never
touched. Startup, `PATH` and the install directory change only when you ask for them.

## Start here: the control centre

Run it with no arguments.

```text
agentbench
```

That opens one screen with the answer to "is this working?" at the top and everything you might want to do
underneath it. It exists because the alternative was a command line with several dozen flags and a TOML
file, and because the first question anyone has - *am I actually collecting anything?* - deserves a better
answer than reading a log.

The status band is the same report `agentbench dashboard --status` prints, from the same code, so the screen
and the command cannot disagree about a verdict.

Below it, in sections:

| Section | Rows |
|---|---|
| **Startup** | Run at login, start in the tray, delay after login |
| **Install** | Install a durable copy, add the install directory to `PATH` |
| **Collection** | Sampling cadence, idle cadence, controlled probes on or off, the probe's network request, probe cadence |
| **Sessions** | Read Claude Code transcripts, or do not |
| **History** | How long raw samples are kept, how many days a baseline spans |
| **Server** | Serve the dashboard, and on which loopback port |
| **Actions** | Start collecting, open the dashboard, run a benchmark, run one elevated, compare two reports, erase collected data |

`↑` `↓` move, `space` toggles, `enter` edits a value or runs an action, `r` re-reads everything, `q` quits.

Three things about how it behaves are deliberate:

- **Changes apply as you make them.** No save key, because every answer to "what happens if you quit
  without saving" is a bad one. Each row reports what it did, including when a value was clamped to the
  configuration's floor and to what.
- **Rows are disabled with a reason, never hidden.** A startup section that vanished on an unsupported
  platform would read as a missing feature rather than an unsupported one. If autostart cannot be
  registered because you are running out of `target/`, the row says so.
- **The one destructive row asks twice.** "Erase collected data" arms on the first `enter` and acts on the
  second within five seconds, refuses outright while a daemon has the database open, and tells you how much
  it is about to remove first.

Settings are written to `watch.toml` with `toml_edit`, so the comments documenting that file survive being
edited from the screen.

### Everything on that screen is also a command

The screen is a front end, not a separate feature. Nothing is only reachable through it:

| Row | Command |
|---|---|
| Start collecting | `agentbench dashboard` |
| Run a benchmark | `agentbench bench --preset standard` |
| Compare two reports | `agentbench compare old.json new.json --output diff.md` |
| Erase collected data | delete `watch.db` in the data directory |
| Any setting | a flag on `agentbench dashboard`, or a key in `watch.toml` |

Two rows have no command-line form, because both are about installing the tool rather than using it:
"Install a durable copy" and "On PATH". Everything else on the screen is a shortcut for something you can
also type, and typing is still the right answer for scripts, for CI, and for the flags no screen should
carry: `--target-dir`, `--output`, live-LLM routes and cost caps, `profile` and `experiment`.

## Install

### Release binary

Download the archive for your platform from
[GitHub Releases](https://github.com/jazzonaut/agentbench/releases/latest), verify it against `SHA256SUMS`,
extract it, and put `agentbench` (or `agentbench.exe`) on your `PATH`. Or extract it anywhere, run
`agentbench`, and use the Install section of the control centre to do both for you.

Assets are published for Windows x64, Linux x64, macOS Intel and macOS Apple Silicon. Every tagged release
carries packaged binaries, SHA-256 checksums, generated notes and GitHub build-provenance attestations.

### Build from source

Rust 1.95 or newer.

```text
git clone https://github.com/jazzonaut/agentbench.git
cd agentbench
cargo install --path .
agentbench
```

## Comparing two machines

Run the same preset from the repository that feels slow, on both machines, then compare the reports.

```text
agentbench bench --preset standard --target-dir . --output machine-a.json
agentbench bench --preset standard --target-dir . --output machine-b.json
agentbench compare machine-a.json machine-b.json --output comparison.md
```

`compare` refuses mismatched schema versions, run kinds and presets rather than producing deltas that look
meaningful and are not. The comparison names the environment differences it found - OS, CPU, core count,
memory, tool versions, config fingerprints - before it shows a single metric, because that is usually where
the answer is. From the control centre, "Compare two reports" does the same thing for the two newest reports
in the working directory and opens the result.

If the daemon is running, the dashboard's **Compare reports** page does the same comparison as a page: pick
the two `.json` reports and it renders the environment differences and the metric deltas without producing a
file to open. The deltas are computed by the same code the command line uses, so the two cannot disagree; the
files are read in the browser and posted to your own daemon, and nothing leaves the machine.

A benchmark includes a live Claude phase by default: interleaved direct and Headroom-proxied cases, `sonnet`,
capped at $5 of reported cost per run.

```text
agentbench bench --preset standard --llm-route direct --llm-model opus --llm-cost-cap-usd 10
agentbench bench --preset standard --llm-route both --headroom-port 8787
agentbench bench --preset standard --no-live-llm
```

`--llm-route auto` is the default: interleaved direct and Headroom cases when port 8787 is listening,
direct cases otherwise. Explicit `headroom` or `both` fail early unless the proxy is already running.
AgentBench never starts or reconfigures it.

`quick` is a sub-minute smoke test. `stress` is explicit, bounded, and can sustain high CPU use and write
up to 2 GiB. `--offline` skips the public HTTPS timing test and does not disable live Claude calls you asked
for.

### Watching and profiling

```text
agentbench top --name claude
agentbench profile --label direct --timeout-seconds 300 --output direct.json -- claude -p "a reproducible task"
```

`top` is the live process-tree view. `profile` runs a non-interactive command and records its wall time,
first output, CPU, peak RSS, disk bytes, child count and exit status. Arguments, working directories,
environment values, prompts and output are redacted from reports; `--save-command-output` puts a bounded
output tail in the local JSON and should not be used with sensitive material.

For a controlled direct-versus-proxied experiment, copy
[`examples/headroom-experiment.toml`](examples/headroom-experiment.toml), replace the placeholder commands,
and run `agentbench experiment my-experiment.toml --output experiment.json`. Cases are interleaved with a
recorded random seed; warmups are not recorded.

## Continuous background collection

`bench` answers "how fast is this machine right now". It cannot answer "is it slower than on Tuesday",
because that needs a record that already existed before you went looking. The daemon keeps one:

```text
agentbench dashboard          # collect and serve a loopback dashboard
agentbench dashboard --status # check on it without starting anything
```

Turn on "Run at login" in the control centre and you can forget about it. On a first run your existing
transcripts are imported, so the charts open with months of real history instead of being empty.

Useful flags: `--port`, `--data-dir`, `--no-serve` to collect without opening a socket, `--sample-interval`
and `--probe-interval` to raise resolution while chasing a regression, `--no-probes`, `--no-probe-network`,
`--no-sessions`, `--sessions-root DIR`. Lowering `--sample-interval` lowers the idle cadence with it, so a
faster setting is not silently defeated when the machine goes quiet.

### What it collects

- **Passive samples.** CPU, memory, swap, process count, security-scanner CPU, and CPU, RSS and process
  count attributed to your agent's process tree. Disk write rates for the agent's tree and for the scanners
  are recorded beside them - attributable, unlike the whole-machine figure below, and blind to anything owned
  by SYSTEM, which the platform limitations explain. Refreshes are narrowed to the counters actually read and
  to discovered process ids rather than walking the whole process table; the sampler runs at background CPU
  and I/O priority where the OS supports it; the cadence backs off when the machine is idle.
- **Capability probes.** Micro-scale reruns of the same workloads `bench` uses, under the same metric names,
  so a threshold written once applies to both. Never a paid API call.
- **The conditions each probe ran in.** The CPU clock as a percentage of its nominal speed, whole-machine disk
  write throughput, free space on the volume being probed, the CPU figures behind the contention tag, and the
  three largest consumers on the machine by name. This is the difference between "small-file operations dropped
  at 14:00" and "...and `MsMpEng` was at 180% of a core".
- **Real session metrics.** Tool latency, response intervals and token accounting derived from your own
  Claude Code transcripts, so the charts show measured usage rather than a proxy for it. Each turn also
  records how long its response took to arrive and whether a subagent produced it.
- **Run markers.** The start and end of every `bench`, `profile` and `experiment`, so the cliff a
  three-minute benchmark puts in the passive series is labelled rather than read as a machine degrading.

### What probing costs, and why it is not gated

Passive samples say what the machine was doing. They cannot separate *the disk got slower* from *the disk
got busier*; that needs an identical workload on a schedule, which by definition consumes something. Every
fifteen minutes:

| Workload | Scale | Metric it feeds |
|---|---|---|
| Single-thread CPU | one core for 200 ms, after a 25 ms warm-up that is not timed | `cpu.single_mops_s` |
| Memory | a 64 MiB buffer | `memory.write_gib_s`, `memory.read_gib_s` |
| Sequential write | 8 MiB, flushed | `filesystem.sequential_write_mib_s` |
| Small files | 200 created, stat-ed, renamed, deleted | `filesystem.small_file_ops_s` |
| SQLite | 2,000 rows inserted, 100 indexed lookups | `sqlite.insert_rows_s`, `sqlite.lookup_ms` |
| Process launch | five child processes | `process.spawn_ms` |
| Loopback TCP | 1 MiB | `network.loopback_connect_ms`, `network.loopback_mib_s` |
| HTTPS | one round trip to `api.anthropic.com` | `network.https_latency_ms` |

That is about a second and a half of work per run, roughly 0.17% of the machine, and around 768 MiB of
writes and 19,000 file creates a day. The small-file rate is the one to watch: it is what moves when a
security scanner or a filesystem filter driver gets into the path.

Two workloads are missing on purpose. The sequential *read* is not probed, because at 8 MiB it would be
served from the page cache and would report memory bandwidth under a name that means disk. Multi-core CPU is
not probed, because saturating every core four times an hour is not a background activity.

**Probes are not skipped when the machine is busy.** Waiting for an idle moment collects nothing on exactly
the days you care about. Instead each probe is stamped with what it was competing with - CPU, scanner CPU,
whether an agent was working, **whole-machine disk write throughput**, whether you are on battery - read once,
immediately before the measurement. A machine writing more than 20 MiB/s counts as contended on its own,
because two of the five judged series are filesystem measurements and an update, a backup or a cloud sync
writing gigabytes reads slow at 15% CPU: those probes used to enter the baseline as clean data. For scale, an
idle desktop with a browser and an editor open wrote 17 KiB/s at the median, and an all-core build 44.9 MiB/s.

The tag claims only "what this measurement began in", and the limit is real and worth putting a number on: the
readings span roughly the 200 ms before the workloads start, so sustained background load is caught and a
burst is caught only if the window lands inside it. Measured both ways: a copy loop that wrote at 2 GB/s for
about a second at a time and idled between read 0.0 MiB/s on two consecutive probes, while four continuous
writers were tagged on three consecutive probes at 3.1-3.5 GiB/s. A scanner busy for ten minutes is the case
this covariate is for; one busy for a second is not. Reading again afterwards was
tried and abandoned, because the closing CPU delta spans the probe and reports the probe's own footprint as
contention; on an idle sixteen-core machine it tagged 17 of 24 runs as contended. The ranked consumers are the
one exception to the window: they come from the process walk the probe already does, and the OS reports each
process's CPU as an average since it was last seen, so they answer "what has been using this machine since the
last probe" rather than "what is using it now".

The dashboard's **uncontended probes only** filter is where the tag gets used, and `--status` reports how many
of your runs were clean, because a verdict computed from four points is worth knowing about.

Probe values and `bench` values are stored side by side under different sources and are **never averaged
together**: the same workload over 200 files and over 5,000 answers the same question two orders of
magnitude apart. The dashboard requests them as `probe:<metric>` and `bench:<metric>`, with no unprefixed
form, precisely so a chart cannot silently pick one.

The outbound HTTPS request is the only part of the daemon that leaves your machine. No prompt, no
credentials, no cost, just a timed round trip - but 96 requests a day in a tool that otherwise uploads
nothing gets a switch of its own: `--no-probe-network`, or `probe_network = false` under `[collect]`.
`--no-probes` turns the whole stream off and leaves passive and session collection running.

Two things are fixed rather than configurable. The **scale** of each workload, because the interval is a
preference while a working set is the unit the measurement is expressed in, and changing it would make
March's numbers incomparable with April's with nothing in the data to say so. And the probe thread's
**normal priority**, because a throttled measurement measures the throttle.

By default probes write inside the data directory. If the code you care about lives on another volume, point
`scratch_dir` under `[collect]` at that volume, or the probe measures the wrong disk and does so silently.

### Session metrics, and what each one is worth

Transcripts record no durations, so every session metric is an interval between two rows. They differ
sharply in how directly they measure the machine, and are charted accordingly:

| Series | What it is | What confounds it |
|---|---|---|
| `tool_read_ms` | Median `Read` latency | Little. This is the clean filesystem signal, and the only session series that carries a verdict |
| `tool_edit_ms` | Median `Edit` and `Write` latency | An `Edit` also pays to match the text it replaces, which is a property of the edit |
| `tool_search_ms` | Median `Grep` and `Glob` latency | Scales with the size of the tree searched, so it moves when the agent changes project. The closest thing here to a directory-walk measurement |
| `tool_bash_ms` | Median `Bash` latency | Dominated by how long the command legitimately took, and by waiting for permission |
| `first_response_ms` | Prompt to the first assistant message | **Not** a time to first token: it contains the whole thinking block, and a prompt typed while the agent was working waits in a queue first. Medians of seven to eight seconds are normal and say nothing about your network |
| `output_tokens` | Tokens produced per bucket | Measures how much you asked for, not how fast anything was |
| `output_tokens_per_s` | Tokens produced divided by the span of the response that produced them | Covers multi-row responses only. About 37% of turns arrive in a single row, have no measurable span, and are excluded rather than counted as instantaneous |
| `cache_hit_ratio` | Prompt tokens served from cache | None, once deduplicated by request |

**One tool per series, because the mix is not a property of your machine.** These used to be pooled into a
single "file-tool latency" series, and measurement is what ended that: over 15,035 real calls the medians
are `Read` 11 ms, `Edit` 35 ms, `Grep` 72 ms and `Glob` 223 ms, an order of magnitude apart and mixed in
whatever proportion the model chose. Across 23 days the pooled daily median correlated with *the share of
calls that happened to be reads* at r = −0.86, against −0.39 for the `Read`-only median. Three quarters of
the movement in the one judged series was composition. One day sat near the month's worst pooled figure on
the day its `Read` median was the month's best.

Failed, refused and interrupted calls are recorded but excluded from every latency series: each returned
early or spent its time waiting for a person, so including them would make a bad afternoon look like a fast
machine. They are 3.3% of calls on real data.

Reading is incremental and free of load. Transcripts are already on disk, each is read from where the last
pass stopped, an unchanged one is not opened at all, and subagent transcripts count too. Every turn and tool
call records whether a subagent produced it, which matters more than it sounds: 38% of the transcripts on the
development machine are subagent work that was previously blended into the parent project's numbers with
nothing able to separate the two. No series filters on the flag yet, so `tool_read_ms` is still a blend of
both.

### Today against the days before it

The middle of the page, and the point of collecting any of this. `--status` prints the same thing.

The comparison is deliberately coarse. Each of the previous seven local days is reduced to **one value**,
the median of that day's uncontended measurements, and the band is the median and median absolute deviation
across those seven numbers. The unit is the day because the question is about days: a band built from six
hundred individual probes measures the ordinary spread *within* a day, which is wide enough that a genuinely
slow week would sit comfortably inside it and be called normal.

Five series are judged:

| Series | Why it earns a verdict |
|---|---|
| `probe:filesystem.small_file_ops_s` | Moves when a scanner or filter driver gets into the path |
| `probe:filesystem.sequential_write_mib_s` | Disk throughput at a fixed 8 MiB working set |
| `probe:sqlite.lookup_ms` | Indexed read latency, microseconds on a healthy machine |
| `probe:cpu.single_mops_s` | Thermal throttling and power-plan changes show here first |
| `tool_read_ms` | What your agent actually experienced, from your own transcripts |

Everything else is charted and none of it is judged, which is a decision rather than an omission. A verdict
on `first_response_ms` would report the model's mood as a property of your machine.

Three things every verdict discloses, because a number without them is not a finding:

- **The count behind it.** Days, and measurements across those days. A day contributing fewer than three
  measurements is dropped, since a median of two numbers is one of them, and fewer than four contributing
  days produces **no verdict** rather than a confident one. Days are weighted equally: a quiet day counts as
  much as a busy one, so a quiet week produces a wider band and a real regression has to be larger before it
  is called one. The counts travel with the band so you can see which you are looking at.
- **When the band is a convention rather than a measurement.** Seven near-identical days have a deviation of
  zero, and a zero-width band would declare every later day better or worse. The band has a floor of 5% of
  the baseline median and says when the floor is what it used.
- **What powered the numbers.** Battery runs are counted, not excluded, because a laptop that lives
  unplugged still has a trend worth watching. But if today ran mostly on battery and the baseline mostly on
  mains, the verdict says so in words.

And where the conditions themselves moved, the verdict gains a third line that says what changed:
`clean probes: clock 128% today against 136%`, beneath `worse -8.0%` and the counts it rests on. `--status`
prints the same line. A covariate earns a clause only when today's median falls outside its own baseline band,
computed exactly the way the verdict's band is - so there is one
sensitivity rule for the whole tool rather than a hand-picked threshold per covariate, and conditions stay
silent for the same first four days that verdicts do. The line reads *clean probes* rather than *today*
because every figure in it is a median over the uncontended runs, which is the population the verdict used.
That also caps what it can report: a disk figure drawn from uncontended runs cannot exceed the threshold that
defines contention, because every run that did was excluded by definition.

### What changed, drawn on the charts

Every chart carries marks for the things that explain a step in a line, listed underneath so the frames stay
readable:

- **Tool version changes**, as a dashed rule at the first sighting of each version. This is what turns "it
  got slower on Tuesday" into "it got slower when it was upgraded on Tuesday". Versions come from your
  transcripts, so it works retroactively over your whole history.
- **Foreground runs**, as a shaded band. A run that was interrupted, or is still going, is drawn
  open-ended rather than waiting for an end that is not coming.

### Four frames, and nothing collected that you cannot reach

The history strip is four stacked frames, each with a switch over what it plots, sharing one cursor so a dip
in one line can be read against the others at the same instant:

| Frame | What its switch offers |
|---|---|
| **System** | CPU, memory, swap, processes, scanner CPU, and the agent tree's CPU, memory and write rate |
| **Agent** | `Read`, `Edit`, search and `Bash` latency, first response, output tokens, tokens/s, cache hits |
| **Probe** | The four judged workloads |
| **Conditions at each probe** | Clock, disk writes, free space, and the machine, scanner and agent CPU each probe began in |

Twenty-seven choices between them, and every one carries a caption saying what is part of the reading - per
core or per machine, what a gap means, what the workload was - and an info mark for why the number is the way
it is. The catalogue is a single file, `src/watch/assets/series.js`, so the prose can be read end to end
rather than found.

**Nothing is collected that the page cannot show.** Twelve series used to be collected and unreachable,
which is cost without benefit, so two tests now guard it: the build fails if a collected series has no button
anywhere, and it fails the other way if a button names a series the server would refuse. The second failure
is the sneaky one - its only symptom is one empty frame, which looks exactly like the first day of collection.

A reader who touches nothing sees the default selections, which are the same three lines the page opened with
before, plus the conditions the runs above them ran in.

### Three pages: the machine, a benchmark, a comparison

The dashboard is three pages behind one nav, all of them served by the same daemon on the same loopback port.

| Page | What it is for |
|---|---|
| `/` | Everything above: the live tiles, today's activity, the verdicts and the four history frames |
| `/bench` | Start a benchmark with the options you would otherwise pass as twelve flags, and watch its phases |
| `/compare` | Upload two reports and read the deltas as a page rather than as a generated `.md` file |

The benchmark page builds its form from the presets themselves, so the sentence under the selector - *up to
45s · 128 MiB written · 500 small files* - comes from the same limits the run will obey rather than from a
number typed into a template. The run happens in a **separate process**, which is not an implementation
detail: the daemon samples at background priority and holds the database writer, so a benchmark measured
inside it would report a slower machine than the identical run from a terminal. The page reads that process's
`[n/8]` phase lines and draws a gauge from them, and stopping the run reaches the process itself.

Two things it deliberately will not do. It offers no elevated run - a consent prompt raised by a web page is
one nobody can connect to something they did just now, so that stays in the control centre. And live Claude
cases are **off by default here**, unlike on the command line, where `standard` and `stress` enable them:
a form whose default spends money on submission is the wrong default. When you do ask for them, the cost cap
is bounded at $20 from the page; the command line, where the intent is unambiguous, has no ceiling.

```toml
[server]
# Keep the dashboard for reading only. `agentbench bench` is unaffected either way.
allow_runs = false
```

Requests that *start* something are held to more than the loopback binding below. A page on any origin can
send this daemon a correctly addressed `POST`, so a write must additionally arrive same-origin, from one of
this socket's own names, as `application/json` - the last of which is what makes a browser preflight it, and
what a cross-site HTML form therefore cannot produce. Reads are unchanged.

### Storage, privacy and lifecycle

State lives in one directory: `%LOCALAPPDATA%\agentbench\` on Windows, `$XDG_DATA_HOME/agentbench/` on
Linux, `~/Library/Application Support/agentbench/` on macOS. Override it with `--data-dir` or
`AGENTBENCH_DATA_DIR`. It holds `watch.db`, a self-documenting `watch.toml` written on first run, and a lock
file that stops two daemons double-counting the same machine.

The HTTP server binds `127.0.0.1` only and refuses anything else. That restriction is load-bearing:
**unlike every exported report, the local database stores real project paths and git branch names**, because
a dashboard that cannot tell you which project was slow is not much use.

Transcripts are read, never copied. What is stored is timings, token counts, model names, and the project
path and branch a session ran in. No prompts, no code, no command output, nothing from any tool's result.
`--no-sessions`, or `enabled = false` under `[sessions]`, turns the stream off entirely.

The database does not grow without limit. Passive samples arrive every few seconds, so after
`samples_raw_days` (14 by default) each whole minute is summarised into one row and the raw samples are
deleted; `0` keeps none of them, summarising every minute as soon as it has finished. Charts cross that
boundary without being asked to, and the response says which part of the line is
summarised and whether each point is that minute's mean or its peak: a swap chart keeps peaks, because a
thirty-second burst is the event, while memory in use keeps its average. Nothing else is pruned - probe
runs, session metrics and run markers arrive slowly and are the whole point of keeping a record, so a year
of them is a few tens of megabytes.

Erasing collected data, from the control centre or by deleting `watch.db`, removes the write-ahead log and
shared-memory index with it. It also removes the import watermarks, which is deliberate: the next daemon
re-reads every transcript from the beginning, so **session history comes back** while probe and sample
history is genuinely gone.

Probes write into `probe-scratch/` inside the data directory, or inside `scratch_dir` if you set one. It is
emptied when the daemon starts and after every probe, so a daemon killed mid-workload leaves nothing behind
to skew the next run.

### Starting at login

**On Windows, run `agentbench` and turn on "Run at login".** The control centre registers an unelevated
`ONLOGON` scheduled task, installs a copy of the executable somewhere `cargo clean` will not delete, and can
put that directory on your `PATH`. It never asks for administrator rights: the collector does not need them,
and Windows will not show a consent prompt at logon anyway. "Start in tray" uses `agentbench-tray.exe`
instead, which runs with no console window and a notification-area icon whose menu opens the dashboard, opens
the settings screen, or stops collecting through the same cooperative shutdown Ctrl+C uses.

The two-minute default delay is not padding: probes that fire during the login storm are recorded as
contended and drop out of the baseline, so a daemon that started immediately would collect samples it could
not later compare.

To register it by hand, or on another platform:

```text
schtasks /create /tn AgentBenchDashboard /sc onlogon /rl limited /tr "C:\path\to\agentbench.exe dashboard"
```

Linux, as `~/.config/systemd/user/agentbench-dashboard.service`:

```text
[Unit]
Description=AgentBench background collector

[Service]
ExecStart=%h/.cargo/bin/agentbench dashboard
Restart=on-failure

[Install]
WantedBy=default.target
```

then `systemctl --user daemon-reload && systemctl --user enable --now agentbench-dashboard`.

macOS, as `~/Library/LaunchAgents/dev.agentbench.dashboard.plist` with a `ProgramArguments` array of your
`agentbench` path plus `dashboard`, then `launchctl load` it.

Remove the collector by deleting the task, unit or plist. Nothing is left behind except the data directory.

The dashboard embeds [uPlot](https://github.com/leeoniya/uPlot) (MIT), served at `/assets/uplot.LICENSE`.
No asset is fetched from the network, so the page works fully offline.

## Reference

### What is measured

On demand, by `bench`, `profile` and `experiment`:

- Single- and multi-core integer throughput, with sustained system sampling throughout.
- Bounded memory write throughput, and the rate at which a buffer can be reached one byte per cache line.
- Sequential file I/O and small-file create/stat/rename/delete throughput on the selected volume.
- Generated SQLite insert and indexed-query performance, plus read-only Tokensave database health if found.
- Process launch, loopback TCP, and optional HTTPS latency to `api.anthropic.com`.
- Profiled process-tree wall time, first output, CPU, peak RSS, disk bytes, child count and exit status.
- Live Claude end-to-end latency, request preparation, time to first token, streamed time to first token,
  API duration, output tokens per second, stream-chunk cadence, input/cache/output tokens, reported cost,
  answer correctness and full process-tree resources.
- Three rotating live scenarios: minimal latency, sustained 300-word generation, and a tool-driven search
  through 2,000 generated files containing hidden markers.
- Installed Claude Code, Headroom, RTK and Tokensave versions; Headroom `doctor` and `perf` JSON if present.
- Normal-user OS metrics, plus capability-gated diagnostics when `--elevated` is requested from an already
  elevated shell.

Continuously, while the daemon runs: passive counters every few seconds, a micro-workload every fifteen
minutes under the same metric names, derived session metrics from transcripts, and a marker around every
foreground run.

Findings carry evidence, confidence, limitations and safe follow-ups, and never claim proof they do not
have. "Possible antivirus contention" is a suspicion to be tested by comparing matched excluded and
non-excluded directories, not a diagnosis. Per-process CPU readings are stated on the scale they are
actually on - a percentage of one core, which runs to 100 × cores - because a threshold set as though it
were a percentage of the machine fires on a scanner that is doing nothing at all.

### A note on what some metrics are not

Two figures are honest measurements of something other than what their names suggest, and both say so in
their own descriptions:

- `filesystem.sequential_read_mib_s` re-reads the file just written, which at any preset size is still in
  the page cache. It measures 4,820 MiB/s at the quick preset's 64 MiB and 9,447 MiB/s at the standard
  preset's 512 MiB, against 1,463 MiB/s written to the same file moments earlier. It reports the cached read
  path, not the device.
- `memory.read_gib_s` touches one byte per cache line and divides by the whole buffer, so it is the rate at
  which memory can be *reached*. It is charted beside the write figure and never divided by it, and on a
  machine whose last-level cache holds the buffer it describes the cache.

### Preset safety limits

| Preset | Target duration | Disk ceiling | Generated file | Memory ceiling |
|---|---:|---:|---:|---:|
| quick | 45 s | 128 MiB | 64 MiB | 10% RAM, max 512 MiB |
| standard | 3-4 min | 2 GiB | 512 MiB | 25% RAM, max 2 GiB |
| stress | 15 min | 10 GiB | 2 GiB | 50% RAM, max 8 GiB |

AgentBench checks that at least twice the generated working set is free before it starts. `q`, Escape or
Ctrl+C cancels cooperatively and cleans up the temporary directory.

Those files are written inside `--target-dir` by default, because the disk numbers are meant to describe the
volume your code lives on. That also means up to two gigabytes landing inside a repository, where an IDE
indexer, a `tsc --watch` or a file-watching test runner will wake up and compete - noise the report then
attributes to the disk. `--scratch-dir` moves the workload files out of the watched tree; keep it on the same
volume, or the filesystem figures describe a different disk. The live file-seek fixture stays under
`--target-dir` regardless, since that is where the agent's working directory is.

If live calls finish or hit their cost cap before the preset minimum, the rest of the window runs a sustained
small-file seek/read workload while sampling continues, so the thermal, storage, memory and scanner
observation windows stay comparable between machines.

### Reports and privacy

Every run writes a versioned JSON report and an adjacent Markdown summary, with hashed host, path and config
fingerprints so two machines can reveal a mismatch without exporting its value. Raw config contents,
environment values, source paths, prompts and command arguments are excluded.

The live-LLM file-seek case is the one part of a benchmark that gives a model access to your files. It runs
`claude` with read-only tools (`Read`, `Glob`, `Grep`), no write or execute tools, permission prompts
suppressed, and its working directory set to `--target-dir`. The prompt points at a generated fixture, but
the model is not confined to it: anything readable beneath the target directory is within reach for that
case. Run it against a directory you are willing to show a model, or pass `--no-live-llm`.

Report schema version 1 is the public Serde types in `src/model.rs`. The dashboard's SQLite schema is
versioned separately through `PRAGMA user_version` and migrated forward automatically; a database written by
a newer build is refused rather than downgraded, so history that cannot be regenerated is never silently
rewritten. A database written by 0.6.x is refused for the same reason and says so: the schema was collapsed to
a single migration for 0.7.0, because no release had carried the intermediate ones. Move `watch.db` aside - a
transcript backfill regenerates the session half of it, and the sample and probe half starts again.

### Platform limitations

Portable counters are collected wherever the OS supports them. Native collectors annotate their provenance
and report a missing capability instead of inventing a zero. Per-process network attribution is
intentionally unavailable without kernel tracing. Thermal evidence varies a great deal by OS: falling
sustained throughput without a temperature or frequency signal is a suspicion, never a thermal diagnosis.

`--elevated` never prompts for elevation and never changes the machine. Start AgentBench from an elevated
terminal if you want the deeper supported checks.

The collector degrades rather than guesses. Its sampler and transcript threads drop to background CPU and
I/O priority only where the OS supports that *per thread*: `THREAD_MODE_BACKGROUND_BEGIN` on Windows,
`setpriority(PRIO_PROCESS, …)` on Linux, `setpriority(PRIO_DARWIN_THREAD, …)` on macOS. Any other Unix
reports the capability as unavailable and runs those threads at normal priority, saying so in the daemon
log: the only call available there is process-wide, and a process-wide throttle applied by the sampler would
reach the probe thread, which must not be throttled. On Unix a lowered priority is never restored, by
design - lowering a nice value needs no privileges and raising it back does - so the probe thread is
*started* at normal priority rather than restored to it. A restore that silently failed would make every
probe on that thread read slow and report a machine degrading while nothing had changed.

Power source is read natively on Windows, Linux and macOS and recorded as *unknown* everywhere else, never
as "on mains", because a laptop on battery runs measurably slower for a reason that is not degradation.

**Disk I/O cannot be attributed to another user's processes without privileges this daemon does not take.** An
unelevated reader sees a SYSTEM-owned process's CPU and exactly zero of its bytes: over 36 seconds of
measurement, Defender, Windows Update, the search indexer, `System` and `Registry` all reported 0 bytes read
and 0 written while `svchost` reported 1.5% CPU, and 346 processes had an unreadable executable path. So the
per-process write rates in the passive stream cover *your* processes only, and the figure that catches the
backups, updates and scans is the whole-machine one, which is unattributed by construction. The two answer
different questions and neither substitutes for the other. `scanner_write_bytes_s` is therefore a flat zero on
a Windows machine whose scanner is Defender - configuration, not a broken counter - and the chart says so
where the line is, because a flat line otherwise reads as a quiet scanner.

**The clock is recorded as a percentage of nominal speed, not in MHz.** The MHz figure available to an ordinary
process on Windows is a value the registry holds from boot: `sysinfo` and WMI both reported exactly 3801 MHz
across thirty-six readings spanning 8% to 98% CPU, on a part whose nominal speed is 3801 MHz. What is live is
a performance counter that is natively a ratio - above 100% while the part boosts, below while it throttles -
and that is what gets stored. Charting the MHz number would have put a permanently flat line under a judged
CPU series, which is worse than having no covariate at all. On Linux the same ratio comes from
`scaling_cur_freq` against `cpuinfo_max_freq`, absent where cpufreq is not built in; on macOS and any other
platform the covariate is absent rather than guessed, like every other capability here.

**A probe's conditions describe roughly the 200 ms before it, not its whole duration.** The clock and disk
readings span the priming window between the two readings the probe needs anyway, which keeps them out of the
measurement they describe. Sustained background load is what that window sees; the chance of catching a burst
is about its duration divided by the probe interval, so a writer that runs for one second in ten is mostly
invisible to it while a scanner busy for ten minutes is not. Widening it by reading again after the workloads
was tried and is worse: the closing reading includes the probe's own footprint and tags an idle machine as
contended.

Per-process CPU needs three refreshes before it is a measurement rather than a zero, which is one more than
the platform documents. Every reader warms up first; the first probe of a daemon session takes an extra
priming reading for exactly this reason, and a benchmark's sampler discards two.

The dashboard is loopback-only and answers only to its own address. A request whose `Host` header names
anything but `127.0.0.1`, `[::1]` or `localhost` on the bound port is refused with 421, because binding to
loopback stops a network peer and not a browser: any page you visit can point a name it controls at
127.0.0.1 and would otherwise read every endpoint same-origin, project paths and branch names included.

One dashboard limitation is known and unresolved, and it affects no judged series: if you change
`--probe-interval` partway through a history, a chart spanning the change renders the minority-cadence
stretch as a blank frame instead of a line. The line-break threshold is the series' own median spacing,
against which every point at the other cadence looks like an outage. Making the threshold depend on the
requested range instead was tried, and it drew a confident straight line across a real ninety-second gap in
collection, which is the worse failure. The fix is per-neighbourhood gap detection.

### Comparability across versions

Three metrics changed meaning and their history should not be read across the change:

- `probe:memory.write_gib_s` reported around 0.07 GiB/s before v0.5.0 on hardware that should manage orders
  of magnitude more. A per-byte cancellation check inside the write loop was blocking vectorisation, and
  0.07 GiB/s is the order of magnitude of a *debug* build, which nothing warned about at the time. Both are
  fixed. **Figures from before v0.5.0 are not comparable with figures after it.**
- `cpu.single_mops_s` now discards a 25 ms warm-up, because an idle processor takes tens of milliseconds to
  raise its clock and how long depends on the power plan and how idle it had been. **Not comparable across
  the change.**
- `tool_read_ms` is the latency of `Read` alone, where it previously pooled `Read`, `Grep`, `Glob` and
  `Edit`. **Not comparable across the change**, and the old series was measuring the model's choice of tool
  as much as the machine.
- `contended`, and therefore every verdict's *uncontended* subset, counts a busy disk from v0.7.0. Probes that
  ran while something wrote more than 20 MiB/s used to be filed as clean data. The change makes the subset
  smaller and its members more alike, which is the point, and the covariate the tag is derived from is stored
  beside it so a revised threshold can be applied to history rather than only to what comes after it.

## Design decisions

Architectural decisions and their rejected alternatives are recorded in [`docs/adr/`](docs/adr/).

## Project policy

- Changes: [CONTRIBUTING.md](CONTRIBUTING.md)
- Release history: [CHANGELOG.md](CHANGELOG.md)
- Private vulnerability reporting: [SECURITY.md](SECURITY.md)
