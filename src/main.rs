use agentbench::{bench, compare, experiment, profile, report, ui};
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
    /// Show a live system/process dashboard. Press q to quit.
    Dashboard {
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
