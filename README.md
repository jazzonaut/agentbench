# AgentBench

[![CI](https://github.com/jazzonaut/agentbench/actions/workflows/ci.yml/badge.svg)](https://github.com/jazzonaut/agentbench/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/jazzonaut/agentbench)](https://github.com/jazzonaut/agentbench/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

AgentBench is a local, cross-platform diagnostic CLI for answering a deceptively hard question: **why is a coding agent fast on one machine and slow on another?** It combines agent-shaped synthetic workloads, process-tree profiling, optional Headroom/RTK/Tokensave evidence, and offline report comparison.

`agentbench dashboard` answers the companion question — **is this machine slower today than it was last week, and what changed?** It collects passive system samples, a scheduled micro-workload, and real timings from your own Claude Code transcripts into a local database, then serves a loopback web dashboard with day-over-day verdicts. Your existing transcripts are imported on first run, so it starts with months of history rather than waiting to accrue it.

Run `agentbench` with no arguments for the control centre: one screen showing whether collection is working
and letting you change any of it without remembering a flag — start at login, run in the tray, install and
add to `PATH`, sampling and probe cadence, retention, and the dashboard's port.

It does not upload telemetry or change antivirus, proxy, power, or OS settings. The collector makes exactly one outbound request, a timed HTTPS round trip carrying no prompt and no credentials, and that request has its own switch. Startup, `PATH` and the install directory are only touched when you ask for them on that screen.

## Install

### Release binary

Download the archive for your platform from [GitHub Releases](https://github.com/jazzonaut/agentbench/releases/latest), verify it against `SHA256SUMS`, extract it, and place `agentbench` (or `agentbench.exe`) on your `PATH`.

Release assets are provided for:

- Windows x64: `x86_64-pc-windows-msvc`
- Linux x64: `x86_64-unknown-linux-gnu`
- macOS Intel: `x86_64-apple-darwin`
- macOS Apple Silicon: `aarch64-apple-darwin`

### Build from source

Rust 1.95 or newer is required.

```text
git clone https://github.com/jazzonaut/agentbench.git
cd agentbench
cargo install --path .
agentbench --help
```

Every tagged release includes packaged binaries, SHA-256 checksums, generated release notes, and GitHub build-provenance attestations.

## Recommended diagnosis workflow

Run the same standard workload from the repository that feels slow on both machines. Standard runs for at least three minutes and, by default, exercises both direct Claude and Headroom when a local Headroom proxy is detected:

```text
agentbench bench --preset standard --target-dir . --output machine-a.json
agentbench bench --preset standard --target-dir . --output machine-b.json
agentbench compare machine-a.json machine-b.json --output comparison.md
```

The default live model is `sonnet` and the total reported Claude cost is capped at `$5` per benchmark. Change the route, model, or cap explicitly when needed:

```text
agentbench bench --preset standard --llm-route direct --llm-model opus --llm-cost-cap-usd 10 --output direct.json
agentbench bench --preset standard --llm-route both --headroom-port 8787 --output paired.json
agentbench bench --preset standard --no-live-llm --output system-only.json
```

`--llm-route auto` is the default: it runs interleaved direct and Headroom cases when port 8787 is listening and otherwise runs direct cases. Explicit `headroom` or `both` routes fail early unless the proxy is already running. AgentBench never starts or reconfigures the proxy.

Run `quick` for a sub-minute system smoke test; add `--live-llm` to include a roughly 30-second paid Claude phase. `stress` is explicit and bounded, but can sustain high CPU use and create up to 2 GiB of temporary benchmark data. Use `--offline` to skip the standalone public HTTPS timing test; it does not disable explicitly enabled live Claude calls.

To watch Claude from a second terminal:

```text
agentbench top --name claude
agentbench top --pid 12345
```

> **Renamed in 0.4.0.** The live terminal view moved from `agentbench dashboard` to `agentbench top`,
> and `agentbench dashboard` now runs the background collector described below. The old
> `dashboard --pid/--name` form still works for one release and prints a notice.

To profile a non-interactive command and its descendants:

```text
agentbench profile --label claude-direct --timeout-seconds 300 --output direct.json -- claude -p "your reproducible task"
```

Arguments, working directories, environment values, prompts, and output are redacted from reports. `--save-command-output` explicitly places a bounded stdout/stderr tail in the local JSON report; do not use it with sensitive material.

## Continuous background collection

`bench` answers "how fast is this machine right now". It cannot answer "is it slower than it was on
Tuesday, and what changed", because that needs a record that already exists before you start looking.
`agentbench dashboard` keeps one:

```text
agentbench dashboard
```

It prints a loopback URL, collects continuously, and serves live tiles plus historical charts. Stop it
with Ctrl+C. To check on it without starting anything:

```text
agentbench dashboard --status
```

Useful flags: `--port`, `--data-dir`, `--no-serve` to collect without opening a socket, and
`--sample-interval` / `--probe-interval` to raise resolution while investigating a regression.
Lowering `--sample-interval` also lowers the idle cadence proportionally, so a faster setting is not
silently defeated when the machine is quiet. `--no-probes` stops the controlled workload,
`--no-probe-network` keeps it but drops its one outbound request, `--no-sessions` stops it reading
transcripts, and `--sessions-root DIR` points it at transcripts somewhere other than
`~/.claude/projects`.

### What it collects

- **Passive samples**: CPU, memory, swap, process count, security-scanner CPU, and CPU/RSS/process
  count attributed to your coding-agent process tree. Refreshes are narrowed to the counters actually
  used and to discovered process ids rather than walking the whole process table, the sampler thread
  runs at background CPU and I/O priority where the OS supports it, and the cadence backs off when the
  machine is idle.
- **Capability probes**: micro-scale reruns of the same workloads `bench` uses, under the same metric
  names, so thresholds and comparisons carry over. Never any paid API call. See below for the cost.
- **Real session metrics**: tool latency, response intervals, and token accounting derived from your
  own Claude Code transcripts, so the charts reflect measured usage rather than a proxy for it.
- **Run markers**: whenever you run `bench`, `profile` or `experiment`, the start and end are recorded,
  so the cliff a three-minute benchmark puts in the passive series is labelled rather than mistaken for
  a machine getting slower. Silent and optional: nothing creates a database, so if you have never
  started the dashboard, nothing happens.

### What probing costs, and why it is not gated

Passive samples explain what the machine was doing. They cannot distinguish *the disk got slower* from
*the disk got busier* — for that you need an identical workload run on a schedule, which by definition
consumes something. Every 15 minutes, one probe runs:

| Workload | Scale | Metric it feeds |
|---|---|---|
| Single-thread CPU | one core for 200 ms | `cpu.single_mops_s` |
| Memory | a 64 MiB buffer | `memory.write_gib_s`, `memory.read_gib_s` |
| Sequential write | 8 MiB, flushed | `filesystem.sequential_write_mib_s` |
| Small files | 200 files created, stat-ed, renamed, deleted | `filesystem.small_file_ops_s` |
| SQLite | 2,000 rows inserted, 100 indexed lookups | `sqlite.insert_rows_s`, `sqlite.lookup_ms` |
| Process launch | five child processes | `process.spawn_ms` |
| Loopback TCP | 1 MiB | `network.loopback_connect_ms`, `network.loopback_mib_s` |
| HTTPS | one round trip to `api.anthropic.com` | `network.https_latency_ms` |

That is roughly a second and a half of work per run — about 0.17% of the machine — and around
768 MiB of writes and 19,000 file creates a day. The small-file number is the one to watch: it is what
moves when a security scanner or filesystem filter driver gets into the path.

**Probes are not skipped when the machine is busy.** Waiting for an idle moment would collect nothing on
exactly the days you care about. Instead every probe is stamped with what it was competing with — CPU,
scanner CPU, whether an agent was working, whether you are on battery — read once, immediately before the
measurement. That reading claims only "what this measurement began in", and the limit is real: something
that starts half a second into a probe is missed. Reading again afterwards was tried and abandoned, because
the closing CPU delta spans the probe and so reports the probe's own footprint as contention — on an idle
sixteen-core machine it tagged 17 of 24 runs as contended. The dashboard's **uncontended probes only**
filter is where the tag gets used, and `--status` reports how many of your runs were clean, because a
verdict computed from four points is worth knowing about.

Probe values and `bench` values are stored side by side under different sources and are **never
averaged together**. The same workload over 200 files and over 5,000 answers the same question at
scales two orders of magnitude apart. The dashboard requests them as `probe:<metric>` and
`bench:<metric>`; there is no unprefixed form, precisely so that a chart cannot silently pick one.

The probe's outbound HTTPS request is the only part of the daemon that leaves your machine. It sends no
prompt and no credentials, costs nothing, and only times the round trip — but 96 requests a day in a
tool that otherwise uploads nothing gets its own switch: `--no-probe-network`, or `probe_network =
false` under `[collect]` in `watch.toml`. `--no-probes` / `probes_enabled = false` turns the whole
stream off and leaves passive and session collection running.

Two things about probing are deliberately fixed rather than configurable. The **scale** of each
workload is not a setting: the interval is a preference, but a working set is the unit the measurement
is expressed in, and changing it would make March's numbers incomparable to April's with nothing in the
data to say so. And the probe thread runs at **normal** priority, unlike the sampler — a throttled
measurement measures the throttle.

By default probes write inside the data directory. If the code you care about lives on a different
volume, point `scratch_dir` under `[collect]` at that volume, or the probe measures the wrong disk and
will do so silently.

### Session metrics, and what each one is worth

Transcripts record no durations, so every session metric is an interval between two rows. They differ
sharply in how directly they measure the machine, and the dashboard charts them accordingly:

| Series | What it is | What confounds it |
|---|---|---|
| `tool_read_ms` | Median latency of `Read`, `Grep`, `Glob` and `Edit` | Little. This is the clean filesystem signal, and it is the one charted by default |
| `tool_bash_ms` | Median `Bash` latency | Dominated by how long the command legitimately took, and by waiting for permission |
| `first_response_ms` | Prompt to the first assistant message | **Not** a time to first token: it contains the whole thinking block, and a prompt typed while the agent was working waits in a queue first. Medians of seven to eight seconds are normal and say nothing about your network |
| `output_tokens` | Tokens produced per bucket | Measures how much you asked for, not how fast anything was |
| `cache_hit_ratio` | Prompt tokens served from cache | None, once deduplicated |

Failed, refused and interrupted tool calls are recorded but excluded from the latency series: each one
returned early or spent its time waiting for a person, so including them would make a bad afternoon
look like a fast machine.

Reading is incremental and free of load: transcripts are already on disk, each is read from where the
last pass stopped, and an unchanged one is not opened at all. Subagent transcripts count too. On a
first run the whole history is imported — a few hundred megabytes takes well under a second — so the
charts have weeks of real data the moment you start, rather than being empty until data accrues.

### Today vs baseline

The page's middle section, and the point of collecting any of this: today's numbers against the days
before them, with a word for each. `--status` prints the same thing.

The comparison is deliberately coarse-grained. Each of the previous seven local days is reduced to **one
value** — the median of that day's uncontended measurements — and the band is the median and median
absolute deviation across those seven numbers. The unit is the day because the question is about days: a
band computed from six hundred individual probes measures the ordinary spread *within* a day, which is wide
enough that a genuinely slow week would sit comfortably inside it and be reported as normal.

Five series are judged:

| Series | Why it earns a verdict |
|---|---|
| `probe:filesystem.small_file_ops_s` | The one that moves when a scanner or filter driver gets into the path |
| `probe:filesystem.sequential_write_mib_s` | Disk throughput, at a fixed 8 MiB working set |
| `probe:sqlite.lookup_ms` | Indexed read latency — microseconds on a healthy machine |
| `probe:cpu.single_mops_s` | Thermal throttling and power-plan changes show up here first |
| `tool_read_ms` | What your agent actually experienced, from your own transcripts |

Everything else is charted and none of it is judged, which is a decision rather than an omission.
`first_response_ms` mixes queue wait, thinking time and network latency, so a verdict on it would report
the model's mood as a property of your machine. `tool_bash_ms` is mostly how long commands legitimately
took. Token counts and cache ratios describe what you asked the agent to do.

Three things every verdict discloses, because a number without them is not a finding:

- **The count behind it.** Days, and measurements across those days. A day contributing fewer than three
  measurements is dropped entirely — a median of two numbers is one of them — and fewer than four
  contributing days produces **no verdict** rather than a confident one. On a busy week the comparable
  subset can be small, and that is exactly when you would want to know.
- **When the band is a convention rather than a measurement.** Seven near-identical days have a median
  absolute deviation of zero, and a band of zero width would declare every later day either better or
  worse. The band therefore has a floor of 5% of the baseline median, and says when the floor is what it
  used. Measured against real probe values on an idle machine, per-probe spread is 1–10% and the
  day-to-day spread of a daily median is well under 1%, so 5% sits above the noise without hiding
  anything a machine would actually do.
- **What powered the numbers.** Battery runs are counted, not excluded — a laptop that lives unplugged
  still has a capability trend worth watching. But if today ran mostly on battery and the baseline mostly
  on mains, the verdict says so in words, because a laptop unplugged this morning reads as degraded for a
  reason that has nothing to do with the machine.

### What changed, drawn on the charts

Every chart carries marks for the things that explain a step in a line, listed underneath so the frames
stay readable:

- **Tool version changes**, as a dashed rule at the first sighting of each version. This is what turns "it
  got slower on Tuesday" into "it got slower when it was upgraded on Tuesday". Versions come from your
  transcripts, so this works retroactively over your whole history.
- **Foreground runs**, as a shaded band spanning the run. A three-minute `bench` is a cliff in the passive
  series and this is what labels it. A run that was interrupted, or is still going, is drawn open-ended
  rather than waiting for an end that is not coming.

### Storage, privacy, and lifecycle

State lives in one directory: `%LOCALAPPDATA%\agentbench\` on Windows,
`$XDG_DATA_HOME/agentbench/` on Linux, `~/Library/Application Support/agentbench/` on macOS. Override
it with `--data-dir` or `AGENTBENCH_DATA_DIR`. It holds `watch.db`, a self-documenting `watch.toml`
written on first run, and a lock file that prevents two daemons from double-counting the same machine.

The HTTP server binds `127.0.0.1` only and refuses to bind anything else. That restriction is
load-bearing: **unlike every exported report, the local database stores real project paths and git
branch names**, because a dashboard that cannot tell you which project was slow is not much use.
Nothing is uploaded, and report and comparison output continue to hash paths as before.

Transcripts are read, never copied. What is stored from them is timings, token counts, model names,
and the project path and branch a session ran in — no prompts, no code, no command output, and nothing
from any tool's result. If you would rather it did not read them at all, `--no-sessions` or
`enabled = false` under `[sessions]` in `watch.toml` turns the whole stream off.

The database does not grow without limit. Passive samples arrive every few seconds, so after
`samples_raw_days` (14 by default, under `[retention]`) each whole minute of them is summarised into one
row and the raw samples are deleted. Charts cross that boundary without being asked to: a range reaching
back past it continues out of the summary, and the response says which part of the line is summarised and
whether each point is that minute's mean or its peak — a swap chart keeps peaks, because a thirty-second
burst is the event, while memory in use keeps its average. Two of the seven passive series had no column in
the summary table before this existed; both were added rather than letting those charts stop dead at the
boundary.

Nothing else is pruned. Probe runs, session metrics and run markers arrive slowly and are the whole point
of keeping a record, so a year of them is a few tens of megabytes and worth every byte. Retention is
considered hourly and does nothing whenever there is nothing old enough, which is most of the time.

Probes write into `probe-scratch/` inside the data directory, or inside `scratch_dir` if you set one.
It is emptied when the daemon starts and after every probe, so a daemon killed mid-workload does not
leave a directory behind that would skew the next run.

**On Windows, run `agentbench` with no arguments and turn on "Run at login"** — the control centre
registers the task below for you, installs a copy of the executable somewhere `cargo clean` will not delete
it, and can put that directory on your `PATH`. It never asks for administrator rights: the collector does
not need them, and Windows will not show a consent prompt at logon anyway. Turning on "Start in tray" uses
`agentbench-tray.exe` instead, which runs with no console window and a notification-area icon.

AgentBench still changes no OS configuration unless asked. To register the task by hand instead, or on
another platform, use your own scheduler.

Windows, user-scoped and without administrator rights — substitute the real path to `agentbench.exe`:

```text
schtasks /create /tn AgentBenchDashboard /sc onlogon /rl limited /tr "C:\path\to\agentbench.exe dashboard"
```

Linux, as a systemd user unit — write `~/.config/systemd/user/agentbench-dashboard.service`:

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

macOS, as a launch agent — write `~/Library/LaunchAgents/dev.agentbench.dashboard.plist` with a
`ProgramArguments` array of your `agentbench` path plus `dashboard`, then
`launchctl load ~/Library/LaunchAgents/dev.agentbench.dashboard.plist`.

Remove the collector by deleting the scheduled task, unit, or plist; nothing else is left behind
except the data directory.

The dashboard embeds [uPlot](https://github.com/leeoniya/uPlot) (MIT); its licence is served at
`/assets/uplot.LICENSE`. No asset is fetched from the network, so the page works fully offline.

## Direct-versus-Headroom experiments

Copy [`examples/headroom-experiment.toml`](examples/headroom-experiment.toml), replace the placeholder commands with equivalent non-interactive direct and proxied commands, then run:

```text
agentbench experiment my-experiment.toml --output experiment.json
```

Cases are interleaved with a recorded random seed to reduce cache, load, and time-order bias. Warmups are not recorded. Experiment commands can call paid APIs; AgentBench never starts them without this explicit config and invocation.

## What is measured

- Single- and multi-core integer throughput and sustained system samples.
- Bounded memory write/read throughput and swap pressure.
- Sequential file I/O and small-file create/stat/rename/delete throughput on the selected volume.
- Generated SQLite insert and indexed-query performance, plus read-only Tokensave database health when found.
- Process launch, loopback TCP, and optional HTTPS latency to `api.anthropic.com`.
- Profiled process-tree wall time, first output, CPU estimate, peak RSS, disk bytes, child count, and exit status.
- Live Claude end-to-end latency, request preparation, time-to-first-token, streaming time-to-first-token, API duration, output tokens/second, stream-chunk cadence, input/cache/output tokens, reported cost, correctness, and full process-tree resources.
- Three rotating live scenarios: minimal latency, sustained 300-word generation, and a tool-driven search through 2,000 generated files containing hidden markers.
- Installed Claude Code, Headroom, RTK, and Tokensave versions; Headroom `doctor` and `perf` JSON when available.
- Normal-user OS metrics plus optional capability-gated diagnostics when `--elevated` is requested from an already elevated shell.

Continuously, while `agentbench dashboard` is running:

- Passive CPU, memory, swap, process count, security-scanner CPU, and CPU/RSS/process count attributed to the coding-agent process tree, every few seconds.
- A micro-workload every 15 minutes feeding the same metric names as the benchmarks above, each run stamped with the contention and power source it began in.
- Tool latency, prompt-to-first-response intervals, token counts and cache hit ratios derived from local Claude Code transcripts.
- The start and end of every `bench`, `profile` and `experiment` run, so a foreground load is labelled in the passive series rather than read as a machine getting slower.

Findings use documented thresholds and always include evidence, confidence, limitations, and safe follow-ups. “Possible antivirus contention” is deliberately not reported as proof: compare matched target directories and inspect scanner overlap first.

## Preset safety limits

| Preset | Target duration | Disk ceiling | Actual generated file | Memory ceiling |
|---|---:|---:|---:|---:|
| quick | 45 s | 128 MiB | 64 MiB | 10% RAM, max 512 MiB |
| standard | 3–4 min | 2 GiB | 512 MiB | 25% RAM, max 2 GiB |
| stress | 15 min | 10 GiB | 2 GiB | 50% RAM, max 8 GiB |

AgentBench verifies at least twice the generated-file working set is free. Press `q`, Escape, or Ctrl+C in the benchmark TUI for cooperative cancellation and temporary-directory cleanup.

Those files are written inside `--target-dir` by default, because the disk numbers are meant to describe the
volume your code lives on. That also means up to two gigabytes land inside a repository, where an IDE
indexer, a `tsc --watch` or a file-watching test runner will wake up and compete — noise the report then
attributes to the disk. `--scratch-dir` moves the workload files out of the watched tree; keep it on the same
volume, or the filesystem figures describe a different disk. The live file-seek fixture stays under
`--target-dir` regardless, since that is where the agent's working directory is.

If live calls finish or hit their cost cap before the preset minimum, the remaining standard duration runs a sustained small-file seek/read workload while resource sampling continues. This preserves comparable three-minute thermal, storage, memory, and background-scanner observation windows.

## Reports and privacy

Every run writes a versioned JSON report and adjacent Markdown summary. Reports include hashed host/path/config fingerprints so two machines can reveal mismatches without exporting their values. Raw config contents, environment values, source paths, prompts, and command arguments are excluded.

The live-LLM file-seek case is the one part of a benchmark that gives a model access to your files. It runs
`claude` with read-only tools (`Read`, `Glob`, `Grep`), no write or execute tools, permission prompts
suppressed, and its working directory set to `--target-dir`. The prompt points at a generated fixture, but
the model is not confined to it: anything readable beneath the target directory is within reach for the
duration of that case. Run it against a directory you are willing to show a model, or pass `--no-live-llm`.

Schema version 1 is represented by the public Serde types in `src/model.rs`. `compare` refuses incompatible schema versions, run kinds, or benchmark presets rather than producing misleading deltas.

The background dashboard's SQLite schema is versioned separately via `PRAGMA user_version` and is
migrated forward automatically. A database written by a newer build is refused rather than
downgraded, so history that cannot be regenerated is never silently rewritten.

## Platform limitations

Portable counters are always collected when supported by the OS. Native collectors annotate their provenance and report missing capabilities instead of inventing zeros. Per-process network attribution is intentionally unavailable without kernel tracing. Thermal evidence varies considerably by OS; falling sustained throughput without a temperature/frequency signal is only a suspicion, never a thermal diagnosis.

`--elevated` never prompts for elevation and never changes the machine. Start AgentBench from an elevated terminal if deeper supported checks are desired.

The background collector degrades rather than guesses. Its sampler and transcript threads drop to
background CPU and I/O priority only where the OS supports it *per thread* — `THREAD_MODE_BACKGROUND_BEGIN`
on Windows, `setpriority(PRIO_PROCESS, …)` on Linux, `setpriority(PRIO_DARWIN_THREAD, …)` on macOS. Any
other Unix reports the capability as unavailable and runs those threads at normal priority instead, saying
so in the daemon log: the only call available there is process-wide, and a process-wide throttle applied by
the sampler would reach the probe thread, which must not be throttled. Power source is read natively on
Windows, Linux and macOS and recorded as *unknown* everywhere else — never as "on mains", because a laptop
on battery runs measurably slower for a reason that is not degradation. On Unix that lowered priority is
never restored, by design: lowering a nice value needs no privileges and raising it back does, so the
probe thread is *started* at normal priority rather than restored to it. A restore that silently failed
would make every probe on that thread read slow and report a machine degrading while nothing had changed.

The dashboard is loopback-only, and answers only to its own address. A request whose `Host` header names
anything other than `127.0.0.1`, `[::1]` or `localhost` on the bound port is refused with 421, because
binding to loopback stops a network peer but not a browser: any page you visit can point a name it
controls at 127.0.0.1 and would otherwise read every endpoint same-origin, including real project paths
and branch names.

One limitation of the dashboard is known and unresolved. It does not affect a judged series:

- If you change `--probe-interval` partway through a history, a chart spanning the change renders the
  minority-cadence stretch as a blank frame instead of a line. The line-break threshold is the series'
  own median spacing, against which every point at the other cadence looks like an outage. Making the
  threshold depend on the requested range instead was tried, and it drew a confident straight line across
  a real ninety-second gap in collection — the worse of the two failures. The fix is per-neighbourhood
  gap detection.

`probe:memory.write_gib_s` used to report around 0.07 GiB/s on hardware that should manage orders of
magnitude more. Neither hypothesis recorded here was right: the working set was not the problem, and the
figure was not what the workload had always reported. A per-byte cancellation check inside the write loop
was blocking vectorisation, and 0.07 GiB/s is the order of magnitude of a *debug* build — which nothing
warned about at the time. Both are fixed, and the metric now reports the machine. **Figures from before
v0.5.0 are not comparable with figures after it.**

## Design decisions

Architectural decisions and their rejected alternatives are recorded in [`docs/adr/`](docs/adr/).

## Project policy

- Changes: [CONTRIBUTING.md](CONTRIBUTING.md)
- Release history: [CHANGELOG.md](CHANGELOG.md)
- Private vulnerability reporting: [SECURITY.md](SECURITY.md)
