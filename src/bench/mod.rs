//! Benchmark orchestration: sequences the workload phases and assembles the report.
//!
//! This module decides *order and sizing*; it contains no measurement logic. Each phase lives in
//! [`workloads`], each of which is parameterised rather than preset-aware so that other callers can
//! reuse the same measurement at a different scale.

pub mod options;
pub mod preset;
pub mod progress;
pub mod workloads;

mod cancel;
mod preflight;
mod sampler;

pub use options::BenchOptions;
pub use preset::Preset;
pub use progress::{PHASE_COUNT, Phase, Progress};

use crate::{
    SCHEMA_VERSION, diagnosis, integrations, live_llm,
    metrics::families,
    model::{Report, RunConfig, RunKind, SystemSample},
    system,
};
use anyhow::{Context, Result};
use cancel::check_cancel;
use chrono::Utc;
use sampler::SamplerGuard;
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tempfile::Builder;
use uuid::Uuid;

/// Fallback minimum live-LLM window for presets that declare no minimum duration.
const DEFAULT_LIVE_MINIMUM: Duration = Duration::from_secs(30);

/// Headroom left before the preset duration limit when budgeting the live-LLM phase.
const LIVE_PHASE_MARGIN: Duration = Duration::from_secs(10);

/// Child processes launched by the process phase.
///
/// Not a preset knob: ten launches take well under a second on any machine, so scaling it per preset
/// would change the numbers a preset reports without buying any more confidence in them. The workload
/// takes it as a parameter for the background prober, which wants the same metric far more cheaply.
const PROCESS_LAUNCHES: usize = 10;

/// Bytes pushed through the loopback socket. Fixed for the same reason as [`PROCESS_LAUNCHES`].
const LOOPBACK_BYTES: usize = 16 << 20;

/// Run a benchmark, installing a Ctrl+C handler for cooperative cancellation.
///
/// Announces phases on stdout, which is the right destination for the batch form: no TTY, redirected
/// output, or `--no-tui`.
pub fn run(preset: Preset, target: &Path, options: BenchOptions) -> Result<Report> {
    let cancel = Arc::new(AtomicBool::new(false));
    let handler_cancel = cancel.clone();
    let _ = ctrlc::set_handler(move || handler_cancel.store(true, Ordering::Relaxed));
    run_with_cancel(preset, target, options, cancel, &Progress::Stdout)
}

/// Run a benchmark against a caller-owned cancellation flag and progress sink.
///
/// The TUI uses this so that `q` and Escape share one cancellation path with Ctrl+C, and so that phase
/// announcements reach a gauge instead of being printed into an alternate screen buffer nobody reads.
pub fn run_with_cancel(
    preset: Preset,
    target: &Path,
    options: BenchOptions,
    cancel: Arc<AtomicBool>,
    progress: &Progress,
) -> Result<Report> {
    let limits = preset.limits();
    let started = Instant::now();
    let mut inventory = system::inventory(options.elevated);
    let memory_size = limits.memory_size(inventory.memory_bytes);
    // The volume that has to hold the working set is the one being written to, which is not necessarily the
    // target directory once `scratch_dir` is set.
    let scratch_parent = options.scratch_dir.as_deref().unwrap_or(target);
    preflight::ensure_free_space(scratch_parent, &limits)?;

    let stop = Arc::new(AtomicBool::new(false));
    let shared_samples = Arc::new(Mutex::new(Vec::<SystemSample>::new()));
    let mut sampler = SamplerGuard::spawn(stop.clone(), shared_samples.clone(), started);
    let temp = Builder::new()
        .prefix(".agentbench-tmp-")
        .tempdir_in(scratch_parent)
        .context("create benchmark temporary directory")?;
    let mut metrics = Vec::new();
    let mut warnings = Vec::new();

    progress.phase(1, "CPU benchmark");
    metrics.extend(workloads::cpu::run(limits.cpu_seconds, &cancel)?);
    check_cancel(&cancel)?;

    progress.phase(
        2,
        format!(
            "Memory benchmark ({:.0} MiB)",
            memory_size as f64 / 1_048_576.0
        ),
    );
    metrics.extend(workloads::memory::run(memory_size as usize, &cancel)?);
    check_cancel(&cancel)?;

    progress.phase(
        3,
        format!(
            "Filesystem benchmark ({:.0} MiB, {} small files)",
            limits.disk_working_set as f64 / 1_048_576.0,
            limits.small_files
        ),
    );
    metrics.extend(workloads::filesystem::run(
        temp.path(),
        limits.disk_working_set,
        limits.small_files,
        &cancel,
    )?);
    check_cancel(&cancel)?;

    progress.phase(4, format!("SQLite benchmark ({} rows)", limits.sqlite_rows));
    metrics.extend(workloads::sqlite::run(
        temp.path(),
        limits.sqlite_rows,
        &cancel,
    )?);
    check_cancel(&cancel)?;

    progress.phase(5, "Process launch benchmark");
    metrics.extend(workloads::process::run(PROCESS_LAUNCHES)?);
    check_cancel(&cancel)?;

    progress.phase(6, "Loopback/network benchmark");
    metrics.extend(workloads::network::loopback(LOOPBACK_BYTES)?);
    check_cancel(&cancel)?;

    if !options.offline {
        match workloads::network::https(limits.network_samples, &cancel) {
            Ok(found) => metrics.extend(found),
            Err(error) => warnings.push(format!("internet benchmark skipped after error: {error}")),
        }
    }

    let mut profiles = Vec::new();
    let mut llm_runs = Vec::new();
    if options.live_llm {
        progress.phase(7, "Live Claude benchmark (paid API/subscription traffic)");
        let minimum = if limits.minimum_duration.is_zero() {
            DEFAULT_LIVE_MINIMUM
        } else {
            limits.minimum_duration
        };
        // The live fixture stays under the target directory even when `scratch_dir` moves the heavy
        // filesystem work elsewhere. The FileSeek case runs `claude` with its cwd set to the target and
        // points it at this path, and a fixture outside that tree is one the agent may not be permitted to
        // read — a scratch location chosen to reduce filesystem noise must not quietly break a paid case.
        let fixture_home = Builder::new()
            .prefix(".agentbench-tmp-llm-")
            .tempdir_in(target)
            .context("create live-LLM fixture directory")?;
        let suite = live_llm::run_suite(
            &live_llm::LiveOptions {
                route: options.llm_route,
                model: options.llm_model.clone(),
                max_cost_usd: options.llm_cost_cap_usd,
                headroom_port: options.headroom_port,
                minimum_total_duration: minimum,
                maximum_total_duration: limits.duration_limit.saturating_sub(LIVE_PHASE_MARGIN),
            },
            target,
            fixture_home.path(),
            started,
            &cancel,
        );
        match suite {
            Ok(live) => {
                metrics.extend(live.metrics);
                profiles.extend(live.profiles);
                llm_runs.extend(live.runs);
                warnings.extend(live.warnings);
            }
            // Degraded to a warning, in the same shape as the HTTPS phase above. This phase used to
            // propagate, so a failed `claude` spawn or an unreadable fixture threw away the minutes of CPU,
            // memory, filesystem, SQLite, process and network measurement already taken and wrote no report
            // at all — for the one phase that depends on an external program behaving.
            //
            // Cancellation is the deliberate exception: it is a request to stop, not a phase that failed, so
            // it is re-propagated and the temporary directories are still dropped on the way out.
            Err(error) => {
                check_cancel(&cancel)?;
                warnings.push(format!(
                    "live Claude benchmark skipped after error: {error:#}"
                ));
            }
        }
    } else {
        progress.phase(7, "Live Claude benchmark skipped (--no-live-llm)");
    }

    progress.phase(8, "Agent integrations");
    let (integrations, mut unavailable) = integrations::collect(target, options.elevated);
    for integration in &integrations {
        if let Some(version) = &integration.version {
            inventory
                .tool_versions
                .insert(integration.name.clone(), version.clone());
        }
        if let Some(elapsed) = integration.elapsed_ms {
            metrics.push(families::TOOL_STARTUP_MS.scalar(&integration.name, elapsed as f64));
        }
    }

    if started.elapsed() < limits.minimum_duration {
        metrics.push(workloads::soak::run(
            temp.path(),
            limits.minimum_duration.saturating_sub(started.elapsed()),
            &cancel,
        )?);
    }

    sampler.stop();
    let samples = Arc::try_unwrap(shared_samples)
        .map(|m| m.into_inner().unwrap_or_default())
        .unwrap_or_else(|arc| arc.lock().map(|v| v.clone()).unwrap_or_default());
    if started.elapsed() > limits.duration_limit {
        warnings.push(format!(
            "preset target duration exceeded: {:.1}s > {}s",
            started.elapsed().as_secs_f64(),
            limits.duration_limit.as_secs()
        ));
    }
    unavailable
        .push("per-process network attribution is not portable without kernel tracing".into());

    let mut findings = diagnosis::analyze(&metrics, &samples, &profiles);
    findings.extend(diagnosis::analyze_live_llm(&llm_runs));
    findings.extend(diagnosis::analyze_integrations(&integrations));

    Ok(Report {
        schema_version: SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").into(),
        run_id: Uuid::new_v4().to_string(),
        created_at: Utc::now(),
        kind: RunKind::Benchmark,
        inventory,
        config: RunConfig {
            preset: Some(limits.name.into()),
            target_hash: Some(system::hash_private(target.to_string_lossy().as_bytes())),
            offline: options.offline,
            elevated_requested: options.elevated,
            duration_limit_seconds: Some(limits.duration_limit.as_secs()),
            disk_limit_bytes: Some(limits.disk_limit),
            memory_limit_bytes: Some(memory_size),
            experiment_hash: None,
            live_llm: options.live_llm,
            llm_route: Some(format!("{:?}", options.llm_route).to_ascii_lowercase()),
            llm_model: options.live_llm.then(|| options.llm_model.clone()),
            llm_cost_cap_usd: options.live_llm.then_some(options.llm_cost_cap_usd),
        },
        metrics,
        samples,
        profiles,
        llm_runs,
        integrations,
        findings,
        warnings,
        unavailable,
    })
}
