//! Daemon configuration: a TOML file in the data directory, overridable per run.
//!
//! Loading is a single fallible step that produces a validated [`WatchConfig`]. Nothing downstream
//! reads raw strings or re-validates, so an invalid interval fails once, at startup, with a useful
//! message rather than at 3am inside a collector thread.

use crate::watch::platform;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    time::Duration,
};

/// Name of the configuration file inside the data directory.
pub const CONFIG_FILE: &str = "watch.toml";

/// Name of the SQLite database inside the data directory.
///
/// Public because a foreground run looks for an existing database to write a marker into, and two places
/// spelling the same file name is how one of them ends up writing to a file nothing reads.
pub const DATABASE_FILE: &str = "watch.db";

/// Default HTTP port for the dashboard.
pub const DEFAULT_PORT: u16 = 7878;

/// Ratio between the shipped active and idle sampling intervals (5s and 30s).
///
/// Used to scale the idle cadence when only the active one is overridden, so that asking for faster
/// sampling is not silently defeated by an unchanged idle interval.
pub const IDLE_INTERVAL_RATIO: u32 = 6;

/// Floor on the transcript poll interval.
///
/// A pass walks every directory under every root and reads the metadata of every transcript it finds,
/// whether or not anything has changed. On a heavy Claude Code user's machine — sessions plus their
/// subagents plus nested workflows — that is thousands of directory entries per pass, which at the
/// one-second floor this used to permit is exactly the filesystem churn the tool exists to attribute to
/// antivirus and filter drivers. Ten seconds is still far finer than anything on the page moves.
const SHORTEST_POLL: Duration = Duration::from_secs(10);

/// Floor on the passive sampling interval, active or idle.
///
/// A tick is cheap but not free: a narrowed CPU and memory refresh, plus a refresh of every watched pid.
/// At the millisecond intervals this used to accept, the sampler becomes a spin loop writing thousands of
/// rows a second into the table retention has to prune — the tool becoming the load it exists to find.
/// One second matches [`SHORTEST_PROBE`]: short enough for a test to drive the loop, long enough to be a
/// floor.
///
/// Public because the CLI overrides have to apply the same floor. A flag that could go below the file's
/// minimum would make the minimum decorative.
pub const SHORTEST_SAMPLE: Duration = Duration::from_secs(1);

/// Floor on the process-discovery interval.
///
/// Every pass enumerates the whole process table, which on Windows is the most expensive thing this tool
/// does per unit time, and the sampler is built to avoid doing it per tick. Five seconds is the shortest
/// interval at which that design still means anything.
///
/// The resolved value is additionally never shorter than the sampling interval. Discovery is decided
/// inside a tick, so a shorter interval cannot produce more passes than there are ticks anyway; clamping
/// makes the configured value say what the sampler will actually do.
const SHORTEST_DISCOVERY: Duration = Duration::from_secs(5);

/// Floor on the probe interval.
///
/// A probe is a second and a half of real load. Back-to-back probing would stop being background
/// collection and start being a benchmark that never ends, so the shortest interval accepted is one
/// second — short enough for a test to drive the loop, long enough that the floor is a floor.
///
/// Public because the CLI override has to apply the same floor. A flag that could go below the file's
/// minimum would make the minimum decorative.
pub const SHORTEST_PROBE: Duration = Duration::from_secs(1);

/// Validated daemon configuration.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    pub data_dir: PathBuf,
    pub server: ServerConfig,
    pub collect: CollectConfig,
    pub sessions: SessionsConfig,
    pub retention: RetentionConfig,
    pub analysis: AnalysisConfig,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub enabled: bool,
    pub bind: IpAddr,
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct CollectConfig {
    /// Sampling interval while the machine is active.
    pub sample_interval: Duration,
    /// Sampling interval once the machine looks idle, to avoid waking a sleeping CPU needlessly.
    pub sample_interval_idle: Duration,
    /// Global CPU percentage below which the machine is considered idle for cadence purposes.
    pub idle_cpu_percent: f32,
    /// How often to re-enumerate the full process table to discover interesting pids.
    pub discovery_interval: Duration,
    /// Substrings identifying coding-agent processes to attribute resource use to.
    pub agent_process_names: Vec<String>,
    /// Substrings identifying security scanners whose activity is worth correlating against.
    pub scanner_process_names: Vec<String>,
    /// Whether the controlled micro-workload runs at all.
    ///
    /// The one collector that costs the machine anything, so it gets a single switch rather than a
    /// search through the code. Turning it off leaves the passive and session streams intact and gives
    /// up the ability to tell "the disk got slower" from "the disk got busier".
    pub probes_enabled: bool,
    /// Whether a probe includes one outbound HTTPS timing request.
    ///
    /// Separate from [`probes_enabled`] because it is the only part of the daemon that leaves the
    /// machine. The request carries no prompt and no credentials and costs nothing, but a tool that
    /// otherwise uploads nothing owes the user a switch for 96 outbound requests a day.
    ///
    /// [`probes_enabled`]: CollectConfig::probes_enabled
    pub probe_network: bool,
    /// Interval between probe runs.
    pub probe_interval: Duration,
    /// Directory probes write to. Must sit on the volume whose performance matters.
    pub scratch_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SessionsConfig {
    /// Whether transcripts are read at all.
    ///
    /// Turning this off costs the daemon its only source of real agent timings, but the transcripts
    /// are the one input that is not the daemon's own to begin with, so refusing to read them has to
    /// be a single switch rather than a search through the code.
    pub enabled: bool,
    /// Directories scanned for Claude Code transcripts.
    pub roots: Vec<PathBuf>,
    /// How often to look for transcripts that have changed.
    pub poll_interval: Duration,
}

#[derive(Debug, Clone)]
pub struct RetentionConfig {
    /// Days of raw samples kept before being summarised into one-minute aggregates and pruned.
    ///
    /// Applies to the passive stream alone. Probe runs, session metrics and run markers arrive slowly and are
    /// the whole point of keeping a record, so nothing prunes them.
    pub samples_raw_days: u32,
}

#[derive(Debug, Clone)]
pub struct AnalysisConfig {
    /// Trailing window of whole local days today is compared against.
    ///
    /// Days rather than hours, because the comparison is between days: each contributes one value and the
    /// band is the spread across those values.
    pub baseline_window_days: u32,
}

impl WatchConfig {
    /// Path of the SQLite database.
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join(DATABASE_FILE)
    }

    /// Path of the single-instance lock file.
    pub fn lock_path(&self) -> PathBuf {
        self.data_dir.join("watch.lock")
    }

    /// Load configuration, writing a commented default file on first run.
    ///
    /// `data_dir` defaults to the per-user data directory when not given.
    pub fn load(data_dir: Option<PathBuf>) -> Result<Self> {
        let data_dir = match data_dir {
            Some(dir) => {
                fs::create_dir_all(&dir)
                    .with_context(|| format!("create data directory {}", dir.display()))?;
                dir
            }
            None => platform::data_dir()?,
        };
        let path = data_dir.join(CONFIG_FILE);
        let file = if path.exists() {
            let text =
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            toml::from_str::<FileConfig>(&text)
                .with_context(|| format!("parse {}", path.display()))?
        } else {
            fs::write(&path, DEFAULT_CONFIG_TOML)
                .with_context(|| format!("write default config to {}", path.display()))?;
            FileConfig::default()
        };
        file.resolve(data_dir)
    }

    /// Reject a bind address that is not loopback.
    ///
    /// The dashboard stores real project paths and branch names, unlike every exported artefact, so
    /// exposing it beyond this machine would leak more than a report ever does.
    pub fn ensure_loopback(&self) -> Result<()> {
        if !self.server.bind.is_loopback() {
            bail!(
                "refusing to bind {}: the dashboard serves unhashed local paths and is loopback-only",
                self.server.bind
            );
        }
        Ok(())
    }
}

/// Parse a human interval such as `500ms`, `30s`, `15m`, `4h`, or `14d`.
pub fn parse_duration(value: &str) -> Result<Duration> {
    let trimmed = value.trim();
    let (digits, unit) = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .map(|index| trimmed.split_at(index))
        .unwrap_or((trimmed, "s"));
    if digits.is_empty() {
        bail!("interval {trimmed:?} must start with a number, for example \"15m\"");
    }
    let amount: u64 = digits
        .parse()
        .with_context(|| format!("interval {trimmed:?} has an unreadable number"))?;
    // Checked rather than bare arithmetic: `"999999999999999999999d"` parses as a `u64` and used to wrap
    // silently in a release build, turning an absurd interval into a plausible one. A configuration error
    // has to fail as one.
    let seconds = match unit.trim() {
        "ms" => return Ok(Duration::from_millis(amount)),
        "s" | "" => Some(amount),
        "m" => amount.checked_mul(60),
        "h" => amount.checked_mul(3_600),
        "d" => amount.checked_mul(86_400),
        other => bail!("interval {trimmed:?} has unknown unit {other:?}; use ms, s, m, h, or d"),
    };
    let seconds = seconds.with_context(|| {
        format!("interval {trimmed:?} is longer than this program can represent")
    })?;
    Ok(Duration::from_secs(seconds))
}

/// Apply the floor and the ordering invariant to a pair of sampling intervals.
///
/// Shared by the CLI overrides and the control centre's save path, because both can otherwise defeat the
/// file's own minimum and make it decorative. The idle cadence is the subtle half: asking for faster
/// sampling has to actually produce it, and left alone a configured idle interval would keep a quiet
/// machine at its slow default so the override would appear to do nothing at all. Idle is scaled to the
/// shipped active-to-idle ratio and never allowed below the active value.
pub fn clamp_sample_intervals(active: Duration, idle: Duration) -> (Duration, Duration) {
    let active = active.max(SHORTEST_SAMPLE);
    let idle = idle
        .min(active * IDLE_INTERVAL_RATIO)
        .max(active)
        .max(SHORTEST_SAMPLE);
    (active, idle)
}

/// Apply the floor to a probe interval.
///
/// A probe is real load, and back-to-back probing stops being background collection.
pub fn clamp_probe_interval(interval: Duration) -> Duration {
    interval.max(SHORTEST_PROBE)
}

/// Render a duration in the units [`parse_duration`] accepts, choosing the coarsest exact one.
///
/// Written back into the file rather than a raw second count, so a value the control centre saved still
/// looks like the values around it and still reads as "15m" rather than "900s".
pub fn duration_text(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds == 0 {
        return format!("{}ms", duration.subsec_millis());
    }
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if seconds.is_multiple_of(size) {
            return format!("{}{unit}", seconds / size);
        }
    }
    format!("{seconds}s")
}

/// Commented defaults written on first run so the file documents itself.
pub(super) const DEFAULT_CONFIG_TOML: &str = r#"# AgentBench dashboard configuration.
# Every value here is optional; deleting one restores its default.
# Intervals accept ms, s, m, h, d — for example "500ms", "15m", "14d".

[server]
# The dashboard is loopback-only by design: it stores real project paths.
enabled = true
port = 7878

[collect]
sample_interval = "5s"
sample_interval_idle = "30s"
# Global CPU below this percentage switches to the idle cadence.
idle_cpu_percent = 10.0
# How often the full process table is re-enumerated to find agent processes.
discovery_interval = "60s"
agent_process_names = ["claude", "node"]
scanner_process_names = [
    "msmpeng", "windefend", "sophos", "crowdstrike",
    "sentinelone", "clamd", "eset", "avast", "avg",
]
# The controlled micro-workload: ~1.5s of real work per run, which is what makes a day-over-day
# number comparable. Without it the dashboard cannot tell a slower disk from a busier one.
probes_enabled = true
# Each probe makes one HTTPS request to api.anthropic.com to time the round trip. No prompt, no
# credentials, no cost — but it is the only part of the daemon that leaves this machine, so it has
# its own switch. Roughly 96 requests a day at the default interval.
probe_network = true
probe_interval = "15m"
# Probes must run on the volume whose performance matters. Defaults to the data directory.
# scratch_dir = "D:/Stuff/.agentbench-scratch"

[sessions]
# Reading transcripts costs nothing and is where the real agent timings come from.
enabled = true
# Claude Code transcript directories. "~" is expanded.
roots = ["~/.claude/projects"]
# How often to check for transcripts that have changed.
poll_interval = "30s"

[retention]
# Raw samples are rolled up to one-minute aggregates after this many days.
samples_raw_days = 14

[analysis]
# Trailing window compared against today.
baseline_window_days = 7
"#;

/// On-disk shape. Every field optional so an old or partial file still loads.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    server: FileServer,
    #[serde(default)]
    collect: FileCollect,
    #[serde(default)]
    sessions: FileSessions,
    #[serde(default)]
    retention: FileRetention,
    #[serde(default)]
    analysis: FileAnalysis,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileServer {
    enabled: Option<bool>,
    port: Option<u16>,
    bind: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileCollect {
    sample_interval: Option<String>,
    sample_interval_idle: Option<String>,
    idle_cpu_percent: Option<f32>,
    discovery_interval: Option<String>,
    agent_process_names: Option<Vec<String>>,
    scanner_process_names: Option<Vec<String>>,
    probes_enabled: Option<bool>,
    probe_network: Option<bool>,
    probe_interval: Option<String>,
    scratch_dir: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileSessions {
    enabled: Option<bool>,
    roots: Option<Vec<String>>,
    poll_interval: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileRetention {
    samples_raw_days: Option<u32>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileAnalysis {
    baseline_window_days: Option<u32>,
}

impl FileConfig {
    fn resolve(self, data_dir: PathBuf) -> Result<WatchConfig> {
        let interval = |value: Option<String>, default: &str| -> Result<Duration> {
            parse_duration(value.as_deref().unwrap_or(default))
        };
        let bind = match self.server.bind.as_deref() {
            Some(text) => text
                .parse::<IpAddr>()
                .with_context(|| format!("server.bind {text:?} is not an IP address"))?,
            None => IpAddr::V4(Ipv4Addr::LOCALHOST),
        };
        // Clamped rather than rejected, like the probe and poll intervals: the value is a preference, not
        // a claim about the data, so collecting slightly less often than asked beats refusing to start.
        // The floor also subsumes the zero this used to reject separately.
        let sample_interval = interval(self.collect.sample_interval, "5s")?.max(SHORTEST_SAMPLE);
        let sample_interval_idle =
            interval(self.collect.sample_interval_idle, "30s")?.max(SHORTEST_SAMPLE);
        if sample_interval_idle < sample_interval {
            bail!(
                "collect.sample_interval_idle ({sample_interval_idle:?}) must not be shorter than \
                 collect.sample_interval ({sample_interval:?})"
            );
        }
        let config = WatchConfig {
            server: ServerConfig {
                enabled: self.server.enabled.unwrap_or(true),
                bind,
                port: self.server.port.unwrap_or(DEFAULT_PORT),
            },
            collect: CollectConfig {
                sample_interval,
                sample_interval_idle,
                idle_cpu_percent: self.collect.idle_cpu_percent.unwrap_or(10.0),
                discovery_interval: interval(self.collect.discovery_interval, "60s")?
                    .max(SHORTEST_DISCOVERY)
                    .max(sample_interval),
                agent_process_names: self
                    .collect
                    .agent_process_names
                    .unwrap_or_else(|| vec!["claude".into(), "node".into()]),
                scanner_process_names: self
                    .collect
                    .scanner_process_names
                    .unwrap_or_else(default_scanner_names),
                probes_enabled: self.collect.probes_enabled.unwrap_or(true),
                probe_network: self.collect.probe_network.unwrap_or(true),
                probe_interval: interval(self.collect.probe_interval, "15m")?.max(SHORTEST_PROBE),
                scratch_dir: self.collect.scratch_dir.as_deref().map(expand_home),
            },
            sessions: SessionsConfig {
                enabled: self.sessions.enabled.unwrap_or(true),
                roots: self
                    .sessions
                    .roots
                    .unwrap_or_else(|| vec!["~/.claude/projects".into()])
                    .iter()
                    .map(|root| expand_home(root))
                    .collect(),
                // A shorter poll would find a live transcript's newest rows sooner, but nothing on
                // the page changes fast enough to notice, and each pass stats every transcript.
                poll_interval: interval(self.sessions.poll_interval, "30s")?.max(SHORTEST_POLL),
            },
            retention: RetentionConfig {
                samples_raw_days: self.retention.samples_raw_days.unwrap_or(14),
            },
            analysis: AnalysisConfig {
                baseline_window_days: self.analysis.baseline_window_days.unwrap_or(7).max(1),
            },
            data_dir,
        };
        Ok(config)
    }
}

/// Scanner name fragments matched case-insensitively against process names.
fn default_scanner_names() -> Vec<String> {
    [
        "msmpeng",
        "windefend",
        "sophos",
        "crowdstrike",
        "sentinelone",
        "clamd",
        "eset",
        "avast",
        "avg",
    ]
    .iter()
    .map(|name| (*name).to_string())
    .collect()
}

/// Expand a leading `~` using the platform home directory.
fn expand_home(value: &str) -> PathBuf {
    let Some(rest) = value.strip_prefix('~') else {
        return PathBuf::from(value);
    };
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    match home {
        Some(home) => home.join(Path::new(rest.trim_start_matches(['/', '\\']))),
        None => PathBuf::from(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_accept_every_documented_unit() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("4h").unwrap(), Duration::from_secs(14_400));
        assert_eq!(
            parse_duration("14d").unwrap(),
            Duration::from_secs(1_209_600)
        );
        assert_eq!(parse_duration(" 7 ").unwrap(), Duration::from_secs(7));
    }

    #[test]
    fn durations_reject_nonsense() {
        for bad in ["", "m", "abc", "10y", "-5s"] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    /// The unit multiply used to wrap, so a number too large to mean anything became a small interval.
    #[test]
    fn a_duration_too_large_to_represent_is_refused_rather_than_wrapped() {
        let error = parse_duration("184467440737095517d")
            .unwrap_err()
            .to_string();
        assert!(error.contains("longer than"), "{error}");
        // The largest whole day count that still fits, to show the boundary is not off by one.
        assert!(parse_duration("213503982334d").is_ok());
    }

    /// A millisecond cadence is a spin loop, and zero is not a cadence at all.
    #[test]
    fn absurdly_short_sampling_intervals_are_clamped_to_the_floor() {
        let file: FileConfig =
            toml::from_str("[collect]\nsample_interval = \"1ms\"\nsample_interval_idle = \"0s\"\n")
                .unwrap();
        let config = file.resolve(PathBuf::from("/tmp/agentbench")).unwrap();
        assert_eq!(config.collect.sample_interval, SHORTEST_SAMPLE);
        assert_eq!(config.collect.sample_interval_idle, SHORTEST_SAMPLE);
    }

    /// Discovery walks the whole process table, which is the one thing the sampler is built around not
    /// doing per tick.
    #[test]
    fn discovery_is_clamped_to_its_floor_and_never_runs_faster_than_sampling() {
        let file: FileConfig =
            toml::from_str("[collect]\ndiscovery_interval = \"100ms\"\n").unwrap();
        let config = file.resolve(PathBuf::from("/tmp/agentbench")).unwrap();
        assert_eq!(config.collect.discovery_interval, SHORTEST_DISCOVERY);

        let slow: FileConfig =
            toml::from_str("[collect]\nsample_interval = \"30s\"\ndiscovery_interval = \"10s\"\n")
                .unwrap();
        let config = slow.resolve(PathBuf::from("/tmp/agentbench")).unwrap();
        assert_eq!(
            config.collect.discovery_interval,
            Duration::from_secs(30),
            "rediscovering between two ticks buys nothing"
        );
    }

    #[test]
    fn the_default_config_text_parses_into_the_documented_defaults() {
        let file: FileConfig = toml::from_str(DEFAULT_CONFIG_TOML).expect("shipped defaults parse");
        let config = file.resolve(PathBuf::from("/tmp/agentbench")).unwrap();
        assert_eq!(config.server.port, DEFAULT_PORT);
        assert!(config.server.enabled);
        assert!(config.server.bind.is_loopback());
        assert_eq!(config.collect.sample_interval, Duration::from_secs(5));
        assert_eq!(config.collect.sample_interval_idle, Duration::from_secs(30));
        assert_eq!(config.collect.probe_interval, Duration::from_secs(900));
        assert!(config.collect.probes_enabled);
        assert!(config.collect.probe_network);
        assert_eq!(config.retention.samples_raw_days, 14);
        assert_eq!(config.analysis.baseline_window_days, 7);
        assert!(
            config
                .collect
                .scanner_process_names
                .contains(&"msmpeng".to_string())
        );
        config.ensure_loopback().unwrap();
    }

    #[test]
    fn an_empty_file_still_yields_defaults() {
        let file: FileConfig = toml::from_str("").unwrap();
        let config = file.resolve(PathBuf::from("/tmp/agentbench")).unwrap();
        assert_eq!(config.server.port, DEFAULT_PORT);
        assert_eq!(config.sessions.roots.len(), 1);
    }

    #[test]
    fn a_non_loopback_bind_is_refused() {
        let file: FileConfig = toml::from_str("[server]\nbind = \"0.0.0.0\"\n").unwrap();
        let config = file.resolve(PathBuf::from("/tmp/agentbench")).unwrap();
        let error = config.ensure_loopback().unwrap_err().to_string();
        assert!(error.contains("loopback-only"), "{error}");
    }

    #[test]
    fn an_idle_cadence_faster_than_the_active_one_is_refused() {
        let file: FileConfig =
            toml::from_str("[collect]\nsample_interval = \"30s\"\nsample_interval_idle = \"5s\"\n")
                .unwrap();
        let error = file
            .resolve(PathBuf::from("/tmp/agentbench"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("must not be shorter"), "{error}");
    }

    /// A probe is real load, so an interval short enough to make probing continuous is clamped rather
    /// than honoured. It is clamped rather than rejected because the value is a preference, not a claim
    /// about the data, and refusing to start over one would be worse than collecting slightly less often
    /// than asked.
    #[test]
    fn an_absurdly_short_probe_interval_is_clamped_to_the_floor() {
        let file: FileConfig = toml::from_str("[collect]\nprobe_interval = \"10ms\"\n").unwrap();
        let config = file.resolve(PathBuf::from("/tmp/agentbench")).unwrap();
        assert_eq!(config.collect.probe_interval, SHORTEST_PROBE);
    }

    /// The outbound request is switchable independently of probing itself.
    #[test]
    fn the_network_probe_can_be_switched_off_without_switching_off_probing() {
        let file: FileConfig = toml::from_str("[collect]\nprobe_network = false\n").unwrap();
        let config = file.resolve(PathBuf::from("/tmp/agentbench")).unwrap();
        assert!(config.collect.probes_enabled);
        assert!(!config.collect.probe_network);
    }

    #[test]
    fn unknown_keys_are_reported_rather_than_ignored() {
        let error = toml::from_str::<FileConfig>("[server]\nprot = 1234\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn loading_twice_writes_defaults_once_and_reads_them_back() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let first = WatchConfig::load(Some(dir.clone())).unwrap();
        assert!(dir.join(CONFIG_FILE).is_file());
        let second = WatchConfig::load(Some(dir)).unwrap();
        assert_eq!(first.server.port, second.server.port);
        assert_eq!(
            first.collect.sample_interval,
            second.collect.sample_interval
        );
    }
}
