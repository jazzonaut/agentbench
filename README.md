# AgentBench

[![CI](https://github.com/jazzonaut/agentbench/actions/workflows/ci.yml/badge.svg)](https://github.com/jazzonaut/agentbench/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/jazzonaut/agentbench)](https://github.com/jazzonaut/agentbench/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

AgentBench is a local, cross-platform diagnostic CLI for answering a deceptively hard question: **why is a coding agent fast on one machine and slow on another?** It combines agent-shaped synthetic workloads, process-tree profiling, optional Headroom/RTK/Tokensave evidence, and offline report comparison.

It does not upload telemetry or change antivirus, proxy, power, or OS settings.

## Install

### Release binary

Download the archive for your platform from [GitHub Releases](https://github.com/jazzonaut/agentbench/releases/latest), verify it against `SHA256SUMS`, extract it, and place `agentbench` (or `agentbench.exe`) on your `PATH`.

Release assets are provided for:

- Windows x64: `x86_64-pc-windows-msvc`
- Linux x64: `x86_64-unknown-linux-gnu`
- macOS Intel: `x86_64-apple-darwin`
- macOS Apple Silicon: `aarch64-apple-darwin`

### Build from source

Rust 1.88 or newer is required.

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
silently defeated when the machine is quiet. `--no-sessions` stops it reading transcripts, and
`--sessions-root DIR` points it at transcripts somewhere other than `~/.claude/projects`.

### What it collects

- **Passive samples**: CPU, memory, swap, process count, security-scanner CPU, and CPU/RSS/process
  count attributed to your coding-agent process tree. Refreshes are narrowed to the counters actually
  used and to discovered process ids rather than walking the whole process table, the sampler thread
  runs at background CPU and I/O priority where the OS supports it, and the cadence backs off when the
  machine is idle.
- **Probes** *(from a later release)*: micro-scale reruns of the same workloads `bench` uses, at the
  same metric names, so thresholds and comparisons carry over. Never any paid API call.
- **Real session metrics**: tool latency, response intervals, and token accounting derived from your
  own Claude Code transcripts, so the charts reflect measured usage rather than a proxy for it.

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

There is no service installer, and AgentBench changes no OS configuration on its own. To start the
collector at login, register it with your platform's own scheduler.

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

Findings use documented thresholds and always include evidence, confidence, limitations, and safe follow-ups. “Possible antivirus contention” is deliberately not reported as proof: compare matched target directories and inspect scanner overlap first.

## Preset safety limits

| Preset | Target duration | Disk ceiling | Actual generated file | Memory ceiling |
|---|---:|---:|---:|---:|
| quick | 45 s | 128 MiB | 64 MiB | 10% RAM, max 512 MiB |
| standard | 3–4 min | 2 GiB | 512 MiB | 25% RAM, max 2 GiB |
| stress | 15 min | 10 GiB | 2 GiB | 50% RAM, max 8 GiB |

AgentBench verifies at least twice the generated-file working set is free. Press `q`, Escape, or Ctrl+C in the benchmark TUI for cooperative cancellation and temporary-directory cleanup.

If live calls finish or hit their cost cap before the preset minimum, the remaining standard duration runs a sustained small-file seek/read workload while resource sampling continues. This preserves comparable three-minute thermal, storage, memory, and background-scanner observation windows.

## Reports and privacy

Every run writes a versioned JSON report and adjacent Markdown summary. Reports include hashed host/path/config fingerprints so two machines can reveal mismatches without exporting their values. Raw config contents, environment values, source paths, prompts, and command arguments are excluded.

Schema version 1 is represented by the public Serde types in `src/model.rs`. `compare` refuses incompatible schema versions, run kinds, or benchmark presets rather than producing misleading deltas.

The background dashboard's SQLite schema is versioned separately via `PRAGMA user_version` and is
migrated forward automatically. A database written by a newer build is refused rather than
downgraded, so history that cannot be regenerated is never silently rewritten.

## Platform limitations

Portable counters are always collected when supported by the OS. Native collectors annotate their provenance and report missing capabilities instead of inventing zeros. Per-process network attribution is intentionally unavailable without kernel tracing. Thermal evidence varies considerably by OS; falling sustained throughput without a temperature/frequency signal is only a suspicion, never a thermal diagnosis.

`--elevated` never prompts for elevation and never changes the machine. Start AgentBench from an elevated terminal if deeper supported checks are desired.

## Design decisions

Architectural decisions and their rejected alternatives are recorded in [`docs/adr/`](docs/adr/).

## Project policy

- Changes: [CONTRIBUTING.md](CONTRIBUTING.md)
- Release history: [CHANGELOG.md](CHANGELOG.md)
- Private vulnerability reporting: [SECURITY.md](SECURITY.md)
