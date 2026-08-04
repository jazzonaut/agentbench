# Platform support

What each operating system will and will not tell an ordinary process, how AgentBench reports the gaps, and
how to start the collector at login on each platform.

The measurement methodology this file qualifies is in [measurement.md](measurement.md).

## Contents

- [The principle: absent, never zero](#the-principle-absent-never-zero)
- [Disk I/O cannot be attributed to another user's processes](#disk-io-cannot-be-attributed-to-another-users-processes)
- [The clock is a percentage, not MHz](#the-clock-is-a-percentage-not-mhz)
- [Background thread priority](#background-thread-priority)
- [Power source](#power-source)
- [Per-process CPU needs three refreshes](#per-process-cpu-needs-three-refreshes)
- [Elevation](#elevation)
- [The dashboard is loopback-only, and checks the `Host` header](#the-dashboard-is-loopback-only-and-checks-the-host-header)
- [A known dashboard limitation](#a-known-dashboard-limitation)
- [Starting at login](#starting-at-login)

## The principle: absent, never zero

Portable counters are collected wherever the OS supports them. Native collectors annotate their provenance
and report a missing capability instead of inventing a zero. Per-process network attribution is
intentionally unavailable without kernel tracing. Thermal evidence varies a great deal by OS: falling
sustained throughput without a temperature or frequency signal is a suspicion, never a thermal diagnosis.

The charts carry that distinction where the line is, because a flat line at zero otherwise reads as a
measurement of nothing happening rather than as the absence of a measurement.

## Disk I/O cannot be attributed to another user's processes

Not without privileges this daemon does not take. An unelevated reader sees a SYSTEM-owned process's CPU and
exactly zero of its bytes: over 36 seconds of measurement, Defender, Windows Update, the search indexer,
`System` and `Registry` all reported 0 bytes read and 0 written while `svchost` reported 1.5% CPU, and 346
processes had an unreadable executable path.

So the per-process write rates in the passive stream cover *your* processes only, and the figure that catches
the backups, updates and scans is the whole-machine one, which is unattributed by construction. The two answer
different questions and neither substitutes for the other. `scanner_write_bytes_s` is therefore a flat zero on
a Windows machine whose scanner is Defender - configuration, not a broken counter - and the chart says so
where the line is.

The whole-machine figure comes from PDH on Windows, which is also what makes the busy-disk contention rule in
[measurement.md](measurement.md#contention-and-why-probes-are-not-skipped) possible for an unelevated process.

## The clock is a percentage, not MHz

The MHz figure available to an ordinary process on Windows is a value the registry holds from boot: `sysinfo`
and WMI both reported exactly 3801 MHz across thirty-six readings spanning 8% to 98% CPU, on a part whose
nominal speed is 3801 MHz. What is live is a performance counter that is natively a ratio - above 100% while
the part boosts, below while it throttles - and that is what gets stored. Charting the MHz number would have
put a permanently flat line under a judged CPU series, which is worse than having no covariate at all.

On Linux the same ratio comes from `scaling_cur_freq` against `cpuinfo_max_freq`, absent where cpufreq is not
built in. On macOS and any other platform the covariate is absent rather than guessed, like every other
capability here.

## Background thread priority

The collector degrades rather than guesses. Its sampler and transcript threads drop to background CPU and
I/O priority only where the OS supports that *per thread*:

| Platform | Call |
|---|---|
| Windows | `THREAD_MODE_BACKGROUND_BEGIN` |
| Linux | `setpriority(PRIO_PROCESS, …)` |
| macOS | `setpriority(PRIO_DARWIN_THREAD, …)` |

Any other Unix reports the capability as unavailable and runs those threads at normal priority, saying so in
the daemon log: the only call available there is process-wide, and a process-wide throttle applied by the
sampler would reach the probe thread, which must not be throttled.

On Unix a lowered priority is never restored, by design - lowering a nice value needs no privileges and
raising it back does - so the probe thread is *started* at normal priority rather than restored to it. A
restore that silently failed would make every probe on that thread read slow and report a machine degrading
while nothing had changed.

## Power source

Read natively on Windows, Linux and macOS, and recorded as *unknown* everywhere else, never as "on mains",
because a laptop on battery runs measurably slower for a reason that is not degradation. Battery runs enter
the baseline; what a verdict does with them is described in
[measurement.md](measurement.md#today-against-the-days-before-it).

## Per-process CPU needs three refreshes

Three, before it is a measurement rather than a zero, which is one more than the platform documents. Every
reader warms up first; the first probe of a daemon session takes an extra priming reading for exactly this
reason, and a benchmark's sampler discards two.

Refreshes are narrowed to the counters actually read and to discovered process ids rather than walking the
whole process table.

## Elevation

`--elevated` never prompts for elevation and never changes the machine. Start AgentBench from an elevated
terminal if you want the deeper supported checks.

The dashboard's benchmark page offers no elevated run at all: a consent prompt raised by a web page is one
nobody can connect to something they did just now, so elevation stays in the control centre, which is the one
place in the design a UAC prompt appears.

Nothing about antivirus, proxy, power or OS settings is ever touched. Startup, `PATH` and the install
directory change only when you ask for them.

## The dashboard is loopback-only, and checks the `Host` header

The HTTP server binds `127.0.0.1` only and refuses anything else. A request whose `Host` header names
anything but `127.0.0.1`, `[::1]` or `localhost` on the bound port is refused with **421**, because binding to
loopback stops a network peer and not a browser: any page you visit can point a name it controls at
127.0.0.1 and would otherwise read every endpoint same-origin, project paths and branch names included.

Requests that *start* something are held to more than that. A page on any origin can send this daemon a
correctly addressed `POST`, so a write must additionally arrive:

- with `Sec-Fetch-Site: same-origin`,
- with an `Origin` naming one of this socket's own names,
- as `application/json` - which is what makes a browser preflight it, and what a cross-site HTML form
  therefore cannot produce.

Reads are unchanged. Benchmark options arrive as a closed set of values placed into a fixed argument
template, passed to the operating system as a vector, so nothing a browser sends is parsed by a shell.

## A known dashboard limitation

One is known and unresolved, and it affects no judged series: if you change `--probe-interval` partway
through a history, a chart spanning the change renders the minority-cadence stretch as a blank frame instead
of a line. The line-break threshold is the series' own median spacing, against which every point at the other
cadence looks like an outage. Making the threshold depend on the requested range instead was tried, and it
drew a confident straight line across a real ninety-second gap in collection, which is the worse failure. The
fix is per-neighbourhood gap detection.

## Starting at login

**On Windows, run `agentbench` and turn on "Run at login".** The control centre registers an unelevated
`ONLOGON` scheduled task, installs a copy of the executable somewhere `cargo clean` will not delete, and can
put that directory on your `PATH`. It never asks for administrator rights: the collector does not need them,
and Windows will not show a consent prompt at logon anyway.

"Start in tray" uses `agentbench-tray.exe` instead, which runs with no console window and a notification-area
icon whose menu opens the dashboard, opens the settings screen, or stops collecting through the same
cooperative shutdown Ctrl+C uses.

The two-minute default delay is not padding: probes that fire during the login storm are recorded as
contended and drop out of the baseline, so a daemon that started immediately would collect samples it could
not later compare.

### By hand, on Windows

```text
schtasks /create /tn AgentBenchDashboard /sc onlogon /rl limited /tr "C:\path\to\agentbench.exe dashboard"
```

### Linux

As `~/.config/systemd/user/agentbench-dashboard.service`:

```ini
[Unit]
Description=AgentBench background collector

[Service]
ExecStart=%h/.cargo/bin/agentbench dashboard
Restart=on-failure

[Install]
WantedBy=default.target
```

Then:

```text
systemctl --user daemon-reload && systemctl --user enable --now agentbench-dashboard
```

### macOS

As `~/Library/LaunchAgents/dev.agentbench.dashboard.plist`, with a `ProgramArguments` array of your
`agentbench` path plus `dashboard`, then `launchctl load` it.

### Removing it

Delete the task, unit or plist. Nothing is left behind except the data directory.
