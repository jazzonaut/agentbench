use agentbench::{bench, compare, experiment, profile, report, status_report, ui, watch};
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
    /// Absent means the control centre: `agentbench` with no arguments opens a screen rather than printing
    /// help, because the help is twelve flags long and the screen is how you avoid needing to know them.
    ///
    /// The control centre uses the default data directory. Anyone wanting another one is already passing
    /// `dashboard --data-dir`, and a second copy of that flag up here would collide with it.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run synthetic, system-resource, and optional paid live-LLM benchmarks.
    Bench {
        #[arg(long, value_enum, default_value = "standard")]
        preset: PresetArg,
        #[arg(long, default_value = ".")]
        target_dir: PathBuf,
        /// Where the filesystem workloads write. Defaults to inside --target-dir.
        ///
        /// The default writes up to two gigabytes inside the repository being measured, which wakes IDE
        /// indexers and file-watching test runners — noise the report then attributes to the disk. Point
        /// this at a directory on the same volume to keep the measurement without the watchers. A
        /// different volume measures a different disk.
        #[arg(long, verbatim_doc_comment)]
        scratch_dir: Option<PathBuf>,
        /// Skip the standalone HTTPS probe (live Claude still uses the network).
        #[arg(long)]
        offline: bool,
        /// Force live Claude tests (quick skips them by default).
        #[arg(long, conflicts_with = "no_live_llm")]
        live_llm: bool,
        /// Skip paid/live Claude tests. Standard still runs for at least 3 minutes.
        #[arg(long)]
        no_live_llm: bool,
        /// Which live-Claude routes to exercise.
        ///
        /// `auto` runs BOTH direct and Headroom whenever a Headroom proxy is listening, so every
        /// scenario is paid for twice; it falls back to direct alone when none is. Pass `direct` to
        /// halve the spend, or `both` to require the proxy rather than depend on whether it happens to
        /// be up.
        #[arg(long, value_enum, default_value = "auto", verbatim_doc_comment)]
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
        /// Do not run the controlled micro-workload. Collection stays passive.
        #[arg(long)]
        no_probes: bool,
        /// Do not make the probe's outbound HTTPS timing request.
        #[arg(long)]
        no_probe_network: bool,
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

/// Say so when this binary cannot produce a number worth keeping.
///
/// A debug build reports memory write bandwidth around forty times low, and everything else in
/// proportion. Those figures do not stay in the terminal: `bench` writes them into the dashboard's
/// history as `source = bench` rows, and a debug-built daemon writes its probe series alongside a
/// release build's, where nothing downstream can tell them apart. So this is a line on stderr at
/// startup rather than a footnote in the README.
fn warn_if_measuring_with_a_debug_build() {
    if cfg!(debug_assertions) {
        eprintln!(
            "warning: this is a debug build. Its measurements are far slower than the machine and \
             must not be compared with, or stored beside, release-build history. Use --release."
        );
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // The control centre is included because it can launch a benchmark, so a debug build has to say so
    // before the user starts one from it rather than after the numbers are already in the database.
    if matches!(
        cli.command,
        None | Some(
            Command::Bench { .. }
                | Command::Profile { .. }
                | Command::Experiment { .. }
                | Command::Dashboard { status: false, .. }
        )
    ) {
        warn_if_measuring_with_a_debug_build();
    }
    let Some(command) = cli.command else {
        return ui::control::run(None);
    };
    match command {
        Command::Bench {
            preset,
            target_dir,
            scratch_dir,
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
            // Resolved here so a mistyped path fails before any load is generated, rather than after the
            // CPU and memory phases have already run.
            let scratch_dir = scratch_dir
                .map(|path| {
                    path.canonicalize().with_context(|| {
                        format!("scratch directory does not exist: {}", path.display())
                    })
                })
                .transpose()?;
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
            options.scratch_dir = scratch_dir;
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
            // No TTY means no screen: a redirected or piped run gets the `[n/8]` phase lines on stdout
            // instead, which is what a script reading this output expects.
            let use_tui = !no_tui && crossterm::tty::IsTty::is_tty(&std::io::stdout());
            // Marked before the load starts and again after the report is written, so an interrupted run
            // still explains the cliff it left in the dashboard's passive series. Silently a no-op when
            // no dashboard database exists, which is the common case.
            let marking = mark_run("benchmark", Some(preset.name()));
            let completed = if use_tui {
                let tui_options = options.clone();
                let run = move |cancel, progress| {
                    bench::run_with_cancel(preset, &target_dir, tui_options, cancel, &progress)
                };
                ui::run_task("AgentBench", run)?
            } else {
                bench::run(preset, &target_dir, options)?
            };
            let paths = report::write_report(&completed, output.as_deref())?;
            marking.finish(
                watch::store::now_ms(),
                paths.json.to_str(),
                &completed.metrics,
            );
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
            no_probes,
            no_probe_network,
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
                ui::top(pid, name.as_deref(), interval_ms.unwrap_or(500))?;
            } else {
                run_dashboard(DashboardArgs {
                    port,
                    data_dir,
                    no_serve,
                    sample_interval,
                    sample_interval_idle,
                    probe_interval,
                    no_probes,
                    no_probe_network,
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
        } => ui::top(pid, name.as_deref(), interval_ms)?,
        Command::Profile {
            label,
            timeout_seconds,
            save_command_output,
            elevated,
            output,
            command,
        } => {
            let marking = mark_run("profile", None);
            let completed = profile::run_report(
                &label,
                &command,
                timeout_seconds,
                save_command_output,
                elevated,
            )?;
            let paths = report::write_report(&completed, output.as_deref())?;
            // No metrics: a profile measures somebody else's command, so its numbers describe that
            // command rather than this machine and have no business in a capability trend.
            marking.finish(watch::store::now_ms(), paths.json.to_str(), &[]);
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
            let marking = mark_run("experiment", None);
            let completed = experiment::run(&config, elevated)?;
            let paths = report::write_report(&completed, output.as_deref())?;
            // Also no metrics, for the same reason: an experiment's cases are whatever the TOML said.
            marking.finish(watch::store::now_ms(), paths.json.to_str(), &[]);
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

/// Start marking a foreground run in the dashboard database, if one exists.
///
/// Every command that loads this machine calls this, because every one of them puts a cliff in the passive
/// series that a later baseline would otherwise average in as degradation. A machine with no dashboard
/// gets a no-op that says nothing and costs nothing.
fn mark_run(kind: &str, preset: Option<&str>) -> watch::marker::Marking {
    watch::marker::Marking::begin(
        None,
        &uuid::Uuid::new_v4().to_string(),
        kind,
        preset,
        watch::store::now_ms(),
    )
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
    no_probes: bool,
    no_probe_network: bool,
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
        no_probes,
        no_probe_network,
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
    if no_probes {
        config.collect.probes_enabled = false;
    }
    if no_probe_network {
        config.collect.probe_network = false;
    }
    if no_sessions {
        config.sessions.enabled = false;
    }
    if !sessions_root.is_empty() {
        // Replaces rather than extends: the point of naming roots is to import those and nothing else.
        config.sessions.roots = sessions_root;
    }
    // Both interval overrides go through the same clamp the configuration file and the control centre's
    // save path use. A flag that could go below the file's own minimum would make the minimum decorative,
    // and three copies of the rule is how one of them ends up disagreeing with the other two.
    if let Some(value) = sample_interval {
        config.collect.sample_interval =
            watch::config::parse_duration(&value).context("--sample-interval")?;
    }
    if let Some(value) = sample_interval_idle {
        config.collect.sample_interval_idle =
            watch::config::parse_duration(&value).context("--sample-interval-idle")?;
    }
    let (active, idle) = watch::config::clamp_sample_intervals(
        config.collect.sample_interval,
        config.collect.sample_interval_idle,
    );
    config.collect.sample_interval = active;
    config.collect.sample_interval_idle = idle;
    if let Some(value) = probe_interval {
        config.collect.probe_interval = watch::config::clamp_probe_interval(
            watch::config::parse_duration(&value).context("--probe-interval")?,
        );
    }

    if status {
        print_status(&config)?;
        return Ok(());
    }
    watch::run(config)
}

/// Human-readable rendering of the same payload `/api/status` returns.
///
/// One reader for both halves. Opening a second would re-derive the same machine id and re-open the
/// same file to answer a question the first one could already have answered. The wording and the
/// queries both live in [`status_report`], so this command and the control centre can never reach
/// different verdicts from the same rows.
fn print_status(config: &watch::WatchConfig) -> Result<()> {
    let reader = watch::open_for_reading(config)?;
    let report = status_report::Report::build(config, &reader)?;
    status_report::print(&report);
    Ok(())
}
