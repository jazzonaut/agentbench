use agentbench::{bench, compare, experiment, profile, report, ui, watch};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "agentbench",
    version,
    about = "Diagnose slow coding-agent environments"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run synthetic, system-resource, and optional paid live-LLM benchmarks.
    Bench {
        #[arg(long, value_enum, default_value = "standard")]
        preset: PresetArg,
        #[arg(long, default_value = ".")]
        target_dir: PathBuf,
        /// Skip the standalone HTTPS probe (live Claude still uses the network).
        #[arg(long)]
        offline: bool,
        /// Force live Claude tests (quick skips them by default).
        #[arg(long, conflicts_with = "no_live_llm")]
        live_llm: bool,
        /// Skip paid/live Claude tests. Standard still runs for at least 3 minutes.
        #[arg(long)]
        no_live_llm: bool,
        #[arg(long, value_enum, default_value = "auto")]
        llm_route: LlmRouteArg,
        #[arg(long, default_value = "sonnet")]
        llm_model: String,
        #[arg(long, default_value_t = 5.0)]
        llm_cost_cap_usd: f64,
        #[arg(long, default_value_t = 8787, value_parser = clap::value_parser!(u16).range(1..))]
        headroom_port: u16,
        #[arg(long)]
        elevated: bool,
        #[arg(long)]
        no_tui: bool,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Collect metrics in the background and serve a local web dashboard.
    Dashboard {
        /// Port for the loopback HTTP server.
        #[arg(long)]
        port: Option<u16>,
        /// Directory holding the database and configuration. Defaults to the per-user data dir.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Collect without serving the web UI.
        #[arg(long)]
        no_serve: bool,
        /// Override the passive sampling interval, e.g. 5s.
        #[arg(long)]
        sample_interval: Option<String>,
        /// Override the idle sampling interval, e.g. 30s. Never shorter than --sample-interval.
        #[arg(long)]
        sample_interval_idle: Option<String>,
        /// Override the probe interval, e.g. 15m.
        #[arg(long)]
        probe_interval: Option<String>,
        /// Do not read Claude Code transcripts.
        #[arg(long)]
        no_sessions: bool,
        /// Read transcripts from this directory instead of the configured roots. Repeatable.
        #[arg(long, value_name = "DIR")]
        sessions_root: Vec<PathBuf>,
        /// Print collection status and recent daemon events, then exit.
        #[arg(long)]
        status: bool,
        /// Accepted for one release: the live TUI moved to `agentbench top`.
        #[arg(long, hide = true)]
        pid: Option<u32>,
        /// Accepted for one release: the live TUI moved to `agentbench top`.
        #[arg(long, hide = true)]
        name: Option<String>,
        /// Accepted for one release: the live TUI moved to `agentbench top`.
        #[arg(long, hide = true)]
        interval_ms: Option<u64>,
    },
    /// Show a live system/process TUI. Press q to quit.
    Top {
        #[arg(long)]
        pid: Option<u32>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value_t = 500)]
        interval_ms: u64,
    },
    /// Launch and profile a non-interactive command.
    Profile {
        #[arg(long, default_value = "profiled-command")]
        label: String,
        #[arg(long)]
        timeout_seconds: Option<u64>,
        #[arg(long)]
        save_command_output: bool,
        #[arg(long)]
        elevated: bool,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Run interleaved command cases from a TOML file.
    Experiment {
        config: PathBuf,
        #[arg(long)]
        elevated: bool,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Compare two JSON reports offline.
    Compare {
        baseline: PathBuf,
        candidate: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    #[command(hide = true)]
    InternalNoop,
}

#[derive(Copy, Clone, ValueEnum)]
enum PresetArg {
    Quick,
    Standard,
    Stress,
}

#[derive(Copy, Clone, ValueEnum)]
enum LlmRouteArg {
    Auto,
    Direct,
    Headroom,
    Both,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Bench {
            preset,
            target_dir,
            offline,
            live_llm,
            no_live_llm,
            llm_route,
            llm_model,
            llm_cost_cap_usd,
            headroom_port,
            elevated,
            no_tui,
            output,
        } => {
            let target_dir = target_dir.canonicalize().with_context(|| {
                format!("target directory does not exist: {}", target_dir.display())
            })?;
            let preset = match preset {
                PresetArg::Quick => bench::Preset::Quick,
                PresetArg::Standard => bench::Preset::Standard,
                PresetArg::Stress => bench::Preset::Stress,
            };
            if llm_cost_cap_usd <= 0.0 || !llm_cost_cap_usd.is_finite() {
                anyhow::bail!("--llm-cost-cap-usd must be a positive finite number");
            }
            let mut options = bench::BenchOptions::for_preset(preset);
            options.offline = offline;
            options.elevated = elevated;
            options.live_llm = if no_live_llm {
                false
            } else if live_llm {
                true
            } else {
                options.live_llm
            };
            options.llm_route = match llm_route {
                LlmRouteArg::Auto => agentbench::live_llm::LlmRoute::Auto,
                LlmRouteArg::Direct => agentbench::live_llm::LlmRoute::Direct,
                LlmRouteArg::Headroom => agentbench::live_llm::LlmRoute::Headroom,
                LlmRouteArg::Both => agentbench::live_llm::LlmRoute::Both,
            };
            options.llm_model = llm_model;
            options.llm_cost_cap_usd = llm_cost_cap_usd;
            options.headroom_port = headroom_port;
            let use_tui = !no_tui && crossterm::tty::IsTty::is_tty(&std::io::stdout());
            let completed = if use_tui {
                let tui_options = options.clone();
                let run =
                    move |cancel| bench::run_with_cancel(preset, &target_dir, tui_options, cancel);
                ui::run_task("AgentBench", run)?
            } else {
                bench::run(preset, &target_dir, options)?
            };
            let paths = report::write_report(&completed, output.as_deref())?;
            println!(
                "JSON: {}\nSummary: {}",
                paths.json.display(),
                paths.markdown.display()
            );
        }
        Command::Dashboard {
            port,
            data_dir,
            no_serve,
            sample_interval,
            sample_interval_idle,
            probe_interval,
            no_sessions,
            sessions_root,
            status,
            pid,
            name,
            interval_ms,
        } => {
            // The TUI used to live here. Its distinctive flags identify an old invocation, so
            // forward it once with a notice rather than starting a web server someone did not ask for.
            if pid.is_some() || name.is_some() || interval_ms.is_some() {
                eprintln!(
                    "note: the live TUI moved to `agentbench top`; `agentbench dashboard` now runs \
                     the background collector and web dashboard. Forwarding to `top` for this run."
                );
                ui::dashboard(pid, name.as_deref(), interval_ms.unwrap_or(500))?;
            } else {
                run_dashboard(DashboardArgs {
                    port,
                    data_dir,
                    no_serve,
                    sample_interval,
                    sample_interval_idle,
                    probe_interval,
                    no_sessions,
                    sessions_root,
                    status,
                })?;
            }
        }
        Command::Top {
            pid,
            name,
            interval_ms,
        } => ui::dashboard(pid, name.as_deref(), interval_ms)?,
        Command::Profile {
            label,
            timeout_seconds,
            save_command_output,
            elevated,
            output,
            command,
        } => {
            let completed = profile::run_report(
                &label,
                &command,
                timeout_seconds,
                save_command_output,
                elevated,
            )?;
            let paths = report::write_report(&completed, output.as_deref())?;
            println!(
                "JSON: {}\nSummary: {}",
                paths.json.display(),
                paths.markdown.display()
            );
        }
        Command::Experiment {
            config,
            elevated,
            output,
        } => {
            let completed = experiment::run(&config, elevated)?;
            let paths = report::write_report(&completed, output.as_deref())?;
            println!(
                "JSON: {}\nSummary: {}",
                paths.json.display(),
                paths.markdown.display()
            );
        }
        Command::Compare {
            baseline,
            candidate,
            output,
        } => compare::run(&baseline, &candidate, output.as_deref())?,
        Command::InternalNoop => {}
    }
    Ok(())
}

/// Everything `dashboard` accepts that is not the deprecated TUI form.
///
/// Grouped rather than passed positionally: nine parameters of which four are `Option<String>` is a
/// call nobody can read, and a mis-ordered pair of them would compile.
struct DashboardArgs {
    port: Option<u16>,
    data_dir: Option<PathBuf>,
    no_serve: bool,
    sample_interval: Option<String>,
    sample_interval_idle: Option<String>,
    probe_interval: Option<String>,
    no_sessions: bool,
    sessions_root: Vec<PathBuf>,
    status: bool,
}

/// Load configuration, apply CLI overrides, then either report status or run the daemon.
fn run_dashboard(args: DashboardArgs) -> Result<()> {
    let DashboardArgs {
        port,
        data_dir,
        no_serve,
        sample_interval,
        sample_interval_idle,
        probe_interval,
        no_sessions,
        sessions_root,
        status,
    } = args;
    let mut config = watch::WatchConfig::load(data_dir)?;
    if let Some(port) = port {
        config.server.port = port;
    }
    if no_serve {
        config.server.enabled = false;
    }
    if no_sessions {
        config.sessions.enabled = false;
    }
    if !sessions_root.is_empty() {
        // Replaces rather than extends: the point of naming roots is to import those and nothing else.
        config.sessions.roots = sessions_root;
    }
    if let Some(value) = sample_interval {
        let active = watch::config::parse_duration(&value).context("--sample-interval")?;
        config.collect.sample_interval = active;
        // Asking for a faster cadence must actually produce one. Left alone, the configured idle
        // interval would keep a quiet machine at its slow default and the override would appear to do
        // nothing at all. Clamp idle to the shipped active:idle ratio, never below the active value.
        config.collect.sample_interval_idle = config
            .collect
            .sample_interval_idle
            .min(active * watch::config::IDLE_INTERVAL_RATIO)
            .max(active);
    }
    if let Some(value) = sample_interval_idle {
        let idle = watch::config::parse_duration(&value).context("--sample-interval-idle")?;
        config.collect.sample_interval_idle = idle.max(config.collect.sample_interval);
    }
    if let Some(value) = probe_interval {
        config.collect.probe_interval =
            watch::config::parse_duration(&value).context("--probe-interval")?;
    }

    if status {
        print_status(&config)?;
        return Ok(());
    }
    watch::run(config)
}

/// Human-readable rendering of the same payload `/api/status` returns.
fn print_status(config: &watch::WatchConfig) -> Result<()> {
    let status = watch::status(config, 10)?;
    let age = status
        .sample_age_ms
        .map(|ms| format!("{:.0}s ago", ms as f64 / 1000.0))
        .unwrap_or_else(|| "never".into());
    println!("Data directory: {}", config.data_dir.display());
    println!(
        "Collecting:     {}",
        if status.collecting { "yes" } else { "no" }
    );
    println!(
        "Daemon running: {}",
        if watch::is_running(config) {
            "yes"
        } else {
            "no"
        }
    );
    println!("Last sample:    {age}");
    println!(
        "Rows:           {} samples, {} probe runs, {} session turns, {} tool calls",
        status.health.samples,
        status.health.probe_runs,
        status.health.session_turns,
        status.health.session_tools
    );
    println!("Transcripts:    {} imported", status.health.imported_files);
    println!("Import errors:  {}", status.health.import_errors);
    println!("Schema version: {}", status.health.schema_version);
    if !status.events.is_empty() {
        println!("\nRecent events:");
        for event in &status.events {
            println!("  [{}] {}: {}", event.level, event.source, event.message);
        }
    }
    Ok(())
}
