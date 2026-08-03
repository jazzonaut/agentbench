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

/// Default HTTP port for the dashboard.
pub const DEFAULT_PORT: u16 = 7878;

/// Ratio between the shipped active and idle sampling intervals (5s and 30s).
///
/// Used to scale the idle cadence when only the active one is overridden, so that asking for faster
/// sampling is not silently defeated by an unchanged idle interval.
pub const IDLE_INTERVAL_RATIO: u32 = 6;

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
    /// Interval between probe runs. Stored now; used from phase 3.
    pub probe_interval: Duration,
    /// Directory probes write to. Must sit on the volume whose performance matters.
    pub scratch_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SessionsConfig {
    /// Directories scanned for Claude Code transcripts. Used from phase 2.
    pub roots: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct RetentionConfig {
    /// Days of raw samples kept before rolling up to one-minute aggregates.
    pub samples_raw_days: u32,
}

#[derive(Debug, Clone)]
pub struct AnalysisConfig {
    /// Trailing window used as the comparison baseline. Used from phase 4.
    pub baseline_window_days: u32,
}

impl WatchConfig {
    /// Path of the SQLite database.
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("watch.db")
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
    let seconds = match unit.trim() {
        "ms" => return Ok(Duration::from_millis(amount)),
        "s" | "" => amount,
        "m" => amount * 60,
        "h" => amount * 3_600,
        "d" => amount * 86_400,
        other => bail!("interval {trimmed:?} has unknown unit {other:?}; use ms, s, m, h, or d"),
    };
    Ok(Duration::from_secs(seconds))
}

/// Commented defaults written on first run so the file documents itself.
const DEFAULT_CONFIG_TOML: &str = r#"# AgentBench dashboard configuration.
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
probe_interval = "15m"
# Probes must run on the volume whose performance matters. Defaults to the data directory.
# scratch_dir = "D:/Stuff/.agentbench-scratch"

[sessions]
# Claude Code transcript directories. "~" is expanded.
roots = ["~/.claude/projects"]

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
    probe_interval: Option<String>,
    scratch_dir: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileSessions {
    roots: Option<Vec<String>>,
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
        let sample_interval = interval(self.collect.sample_interval, "5s")?;
        let sample_interval_idle = interval(self.collect.sample_interval_idle, "30s")?;
        if sample_interval.is_zero() || sample_interval_idle.is_zero() {
            bail!("collect sample intervals must be greater than zero");
        }
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
                discovery_interval: interval(self.collect.discovery_interval, "60s")?,
                agent_process_names: self
                    .collect
                    .agent_process_names
                    .unwrap_or_else(|| vec!["claude".into(), "node".into()]),
                scanner_process_names: self
                    .collect
                    .scanner_process_names
                    .unwrap_or_else(default_scanner_names),
                probe_interval: interval(self.collect.probe_interval, "15m")?,
                scratch_dir: self.collect.scratch_dir.as_deref().map(expand_home),
            },
            sessions: SessionsConfig {
                roots: self
                    .sessions
                    .roots
                    .unwrap_or_else(|| vec!["~/.claude/projects".into()])
                    .iter()
                    .map(|root| expand_home(root))
                    .collect(),
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
