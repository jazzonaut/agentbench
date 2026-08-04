# Measurement

What AgentBench measures, at what cost, and what each number is and is not worth. The
[README](../README.md) says what to type; this file says why the numbers can be trusted, and where they
cannot be.

Platform-specific capabilities and their gaps live in [platform-support.md](platform-support.md).
Architectural decisions and their rejected alternatives live in [`docs/adr/`](adr/).

## Contents

- [On demand: what a benchmark measures](#on-demand-what-a-benchmark-measures)
- [Preset limits, and where the files land](#preset-limits-and-where-the-files-land)
- [Continuously: the probe stream](#continuously-the-probe-stream)
- [Contention, and why probes are not skipped](#contention-and-why-probes-are-not-skipped)
- [Session metrics, and what each one is worth](#session-metrics-and-what-each-one-is-worth)
- [Today against the days before it](#today-against-the-days-before-it)
- [What changed, drawn on the charts](#what-changed-drawn-on-the-charts)
- [Four frames, and nothing collected that you cannot reach](#four-frames-and-nothing-collected-that-you-cannot-reach)
- [A note on what some metrics are not](#a-note-on-what-some-metrics-are-not)
- [Comparability across versions](#comparability-across-versions)

## On demand: what a benchmark measures

By `bench`, `profile` and `experiment`:

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

Findings carry evidence, confidence, limitations and safe follow-ups, and never claim proof they do not
have. "Possible antivirus contention" is a suspicion to be tested by comparing matched excluded and
non-excluded directories, not a diagnosis. Per-process CPU readings are stated on the scale they are
actually on - a percentage of one core, which runs to 100 x cores - because a threshold set as though it
were a percentage of the machine fires on a scanner that is doing nothing at all.

### Live Claude cases, and the one that reads your files

A benchmark includes a live Claude phase by default: interleaved direct and Headroom-proxied cases,
`sonnet`, capped at $5 of reported cost per run. `--llm-route auto` is the default - interleaved direct and
Headroom cases when port 8787 is listening, direct cases otherwise. Explicit `headroom` or `both` fail early
unless the proxy is already running; AgentBench never starts or reconfigures it.

The file-seek case is the one part of a benchmark that gives a model access to your files. It runs `claude`
with read-only tools (`Read`, `Glob`, `Grep`), no write or execute tools, permission prompts suppressed, and
its working directory set to `--target-dir`. The prompt points at a generated fixture, but the model is not
confined to it: anything readable beneath the target directory is within reach for that case. Run it against
a directory you are willing to show a model, or pass `--no-live-llm`.

If live calls finish or hit their cost cap before the preset minimum, the rest of the window runs a sustained
small-file seek/read workload while sampling continues, so the thermal, storage, memory and scanner
observation windows stay comparable between machines.

## Preset limits, and where the files land

| Preset | Target duration | Disk ceiling | Generated file | Memory ceiling | Small files | SQLite rows |
|---|---:|---:|---:|---:|---:|---:|
| `quick` | 45 s | 128 MiB | 64 MiB | 10% RAM, max 512 MiB | 500 | 2,000 |
| `standard` | 3-4 min | 2 GiB | 512 MiB | 25% RAM, max 2 GiB | 5,000 | 20,000 |
| `stress` | 15 min | 10 GiB | 2 GiB | 50% RAM, max 8 GiB | 20,000 | 100,000 |

The limits are one value in `src/bench/preset.rs`, which is also what `GET /api/bench/options` serves to the
dashboard's benchmark form - so a page cannot describe a run the benchmark will not perform.

AgentBench checks that at least twice the generated working set is free before it starts. `q`, Escape or
Ctrl+C cancels cooperatively and cleans up the temporary directory.

Those files are written inside `--target-dir` by default, because the disk numbers are meant to describe the
volume your code lives on. That also means up to two gigabytes landing inside a repository, where an IDE
indexer, a `tsc --watch` or a file-watching test runner will wake up and compete - noise the report then
attributes to the disk. `--scratch-dir` moves the workload files out of the watched tree; keep it on the same
volume, or the filesystem figures describe a different disk. The live file-seek fixture stays under
`--target-dir` regardless, since that is where the agent's working directory is.

## Continuously: the probe stream

`bench` answers "how fast is this machine right now". It cannot answer "is it slower than on Tuesday",
because that needs a record that already existed before you went looking. The daemon keeps one, and the part
of it that is a controlled measurement is the probe stream.

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

These are micro-scale reruns of the same workloads `bench` uses, under the same metric names, so a threshold
written once applies to both. Never a paid API call.

Two workloads are missing on purpose. The sequential *read* is not probed, because at 8 MiB it would be
served from the page cache and would report memory bandwidth under a name that means disk. Multi-core CPU is
not probed, because saturating every core four times an hour is not a background activity.

### What is fixed, and what you can change

`--sample-interval` and `--probe-interval` raise resolution while you chase a regression; `--no-probes` turns
the whole stream off and leaves passive and session collection running. Lowering `--sample-interval` lowers
the idle cadence with it, so a faster setting is not silently defeated when the machine goes quiet.

The outbound HTTPS request is the only part of the daemon that leaves your machine. No prompt, no
credentials, no cost, just a timed round trip - but 96 requests a day in a tool that otherwise uploads
nothing gets a switch of its own: `--no-probe-network`, or `probe_network = false` under `[collect]`.

Two things are fixed rather than configurable. The **scale** of each workload, because the interval is a
preference while a working set is the unit the measurement is expressed in, and changing it would make
March's numbers incomparable with April's with nothing in the data to say so. And the probe thread's
**normal priority**, because a throttled measurement measures the throttle.

By default probes write inside the data directory, into `probe-scratch/`. If the code you care about lives
on another volume, point `scratch_dir` under `[collect]` at that volume, or the probe measures the wrong disk
and does so silently. The scratch directory is emptied when the daemon starts and after every probe, so a
daemon killed mid-workload leaves nothing behind to skew the next run.

### Probe values and bench values are never averaged together

The same workload over 200 files and over 5,000 answers the same question two orders of magnitude apart, so
the two are stored side by side under different sources. The dashboard requests them as `probe:<metric>` and
`bench:<metric>`, with no unprefixed form, precisely so a chart cannot silently pick one.

## Contention, and why probes are not skipped

**Probes are not skipped when the machine is busy.** Waiting for an idle moment collects nothing on exactly
the days you care about. Instead each probe is stamped with what it was competing with - CPU, scanner CPU,
whether an agent was working, whole-machine disk write throughput, whether you are on battery - read once,
immediately before the measurement.

A machine writing more than 20 MiB/s counts as contended on its own, because two of the five judged series
are filesystem measurements and an update, a backup or a cloud sync writing gigabytes reads slow at 15% CPU:
those probes used to enter the baseline as clean data. For scale, an idle desktop with a browser and an
editor open wrote 17 KiB/s at the median, and an all-core build 44.9 MiB/s.

### What the covariate window can and cannot see

The tag claims only "what this measurement began in", and the limit is real and worth putting a number on:
the readings span roughly the 200 ms before the workloads start, so sustained background load is caught and
a burst is caught only if the window lands inside it. Measured both ways:

- A copy loop that wrote at 2 GB/s for about a second at a time and idled between read **0.0 MiB/s** on two
  consecutive probes.
- Four continuous writers were tagged on **three consecutive probes at 3.1-3.5 GiB/s**.

A scanner busy for ten minutes is the case this covariate is for; one busy for a second is not. The chance of
catching a burst is about its duration divided by the probe interval.

Reading again *after* the workloads was tried and abandoned, because the closing CPU delta spans the probe and
reports the probe's own footprint as contention; on an idle sixteen-core machine it tagged 17 of 24 runs as
contended.

The ranked consumers are the one exception to the window: the three largest consumers on the machine by name
come from the process walk the probe already does, and the OS reports each process's CPU as an average since
it was last seen, so they answer "what has been using this machine since the last probe" rather than "what is
using it now". The daemon's own process tree is excluded - an early version ranked `agentbench.exe` itself,
which was the previous probe's own workloads.

The dashboard's **uncontended probes only** filter is where the tag gets used, and `--status` reports how many
of your runs were clean, because a verdict computed from four points is worth knowing about.

## Session metrics, and what each one is worth

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
calls that happened to be reads* at r = -0.86, against -0.39 for the `Read`-only median. Three quarters of
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

## Today against the days before it

The middle of the dashboard, and the point of collecting any of this. `--status` prints the same thing.

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

### And what was different about today

Where the conditions themselves moved, the verdict gains a third line that says what changed:
`clean probes: clock 128% today against 136%`, beneath `worse -8.0%` and the counts it rests on. `--status`
prints the same line.

A covariate earns a clause only when today's median falls outside its own baseline band, computed exactly the
way the verdict's band is - so there is one sensitivity rule for the whole tool rather than a hand-picked
threshold per covariate, and conditions stay silent for the same first four days that verdicts do. The line
reads *clean probes* rather than *today* because every figure in it is a median over the uncontended runs,
which is the population the verdict used. That also caps what it can report: a disk figure drawn from
uncontended runs cannot exceed the threshold that defines contention, because every run that did was excluded
by definition.

## What changed, drawn on the charts

Every chart carries marks for the things that explain a step in a line, listed underneath so the frames stay
readable:

- **Tool version changes**, as a dashed rule at the first sighting of each version. This is what turns "it
  got slower on Tuesday" into "it got slower when it was upgraded on Tuesday". Versions come from your
  transcripts, so it works retroactively over your whole history.
- **Foreground runs**, as a shaded band, from the marker written around every `bench`, `profile` and
  `experiment`. A run that was interrupted, or is still going, is drawn open-ended rather than waiting for an
  end that is not coming. Without these, the cliff a three-minute benchmark puts in the passive series reads
  as a machine degrading.

## Four frames, and nothing collected that you cannot reach

The history strip is four stacked frames, each with a switch over what it plots, sharing one cursor so a dip
in one line can be read against the others at the same instant:

| Frame | What its switch offers | Choices |
|---|---|---:|
| **System** | CPU, memory, swap, processes, scanner CPU, and the agent tree's CPU, memory and write rate | 9 |
| **Agent** | `Read`, `Edit`, search and `Bash` latency, first response, output tokens, tokens/s, cache hits | 8 |
| **Probe** | The four judged workloads | 4 |
| **Conditions at each probe** | Clock, disk writes, free space, and the machine, scanner and agent CPU each probe began in | 6 |

Twenty-seven choices between them, and every one carries a caption saying what is part of the reading - per
core or per machine, what a gap means, what the workload was - and an info mark for why the number is the way
it is. The catalogue is a single file, `src/watch/assets/series.js`, so the prose can be read end to end
rather than found.

Two of the six conditions are charted but never quoted in a verdict: over uncontended runs the scanner and
agent CPU figures are capped by their own thresholds at a tenth and a fifth of one core, so a large relative
move there explains nothing about a throughput drop.

**Nothing is collected that the page cannot show.** Twelve series used to be collected and unreachable,
which is cost without benefit, so two tests now guard it: the build fails if a collected series has no button
anywhere, and it fails the other way if a button names a series the server would refuse. The second failure
is the sneaky one - its only symptom is one empty frame, which looks exactly like the first day of collection.

A reader who touches nothing sees the default selections, which are the same three lines the page opened with
before, plus the conditions the runs above them ran in.

## A note on what some metrics are not

Two figures are honest measurements of something other than what their names suggest, and both say so in
their own descriptions:

- `filesystem.sequential_read_mib_s` re-reads the file just written, which at any preset size is still in
  the page cache. It measures 4,820 MiB/s at the quick preset's 64 MiB and 9,447 MiB/s at the standard
  preset's 512 MiB, against 1,463 MiB/s written to the same file moments earlier. It reports the cached read
  path, not the device.
- `memory.read_gib_s` touches one byte per cache line and divides by the whole buffer, so it is the rate at
  which memory can be *reached*. It is charted beside the write figure and never divided by it, and on a
  machine whose last-level cache holds the buffer it describes the cache.

## Comparability across versions

Four metrics changed meaning and their history should not be read across the change:

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

Report schema version 1 is the public Serde types in `src/model.rs`. The dashboard's SQLite schema is
versioned separately through `PRAGMA user_version` and migrated forward automatically; a database written by
a newer build is refused rather than downgraded, so history that cannot be regenerated is never silently
rewritten. A database written by 0.6.x is refused for the same reason and says so: the schema was collapsed to
a single migration for 0.7.0, because no release had carried the intermediate ones. Move `watch.db` aside - a
transcript backfill regenerates the session half of it, and the sample and probe half starts again.
