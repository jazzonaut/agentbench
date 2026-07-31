use crate::{
    SCHEMA_VERSION, diagnosis, integrations,
    model::{Report, RunConfig, RunKind},
    profile::{self, CommandSpec},
    system,
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use rand::{SeedableRng, rngs::StdRng, seq::SliceRandom};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentConfig {
    pub seed: Option<u64>,
    #[serde(default = "default_repetitions")]
    pub repetitions: usize,
    #[serde(default)]
    pub warmups: usize,
    #[serde(default)]
    pub save_command_output: bool,
    pub case: Vec<ExperimentCase>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentCase {
    pub name: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub working_directory: Option<PathBuf>,
    pub timeout_seconds: Option<u64>,
}

fn default_repetitions() -> usize {
    3
}

pub fn run(path: &Path, elevated: bool) -> Result<Report> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("read experiment config {}", path.display()))?;
    let config: ExperimentConfig = toml::from_str(&source).context("parse experiment TOML")?;
    if config.case.len() < 2 {
        bail!("an experiment requires at least two [[case]] entries");
    }
    if config.repetitions == 0 || config.repetitions > 100 {
        bail!("repetitions must be between 1 and 100");
    }
    if config.warmups > 10 {
        bail!("warmups must be 10 or fewer");
    }
    let seed = config.seed.unwrap_or_else(rand::random);
    eprintln!(
        "Experiment commands may access networks or paid APIs. Running {} cases × {} measured repetitions (seed {seed}).",
        config.case.len(),
        config.repetitions
    );
    let default_cwd = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;

    for case in &config.case {
        for index in 0..config.warmups {
            eprintln!("Warmup {}: {}", index + 1, case.name);
            let spec = to_spec(case, &default_cwd, config.save_command_output);
            let _ = profile::profile_command(&spec)?;
        }
    }
    let mut schedule = Vec::new();
    for repetition in 0..config.repetitions {
        for case_index in 0..config.case.len() {
            schedule.push((repetition, case_index));
        }
    }
    schedule.shuffle(&mut StdRng::seed_from_u64(seed));

    let mut profiles = Vec::new();
    let mut samples = Vec::new();
    for (repetition, case_index) in schedule {
        let case = &config.case[case_index];
        eprintln!("Measured run {}: {}", repetition + 1, case.name);
        let mut spec = to_spec(case, &default_cwd, config.save_command_output);
        spec.label = format!("{}#{}", case.name, repetition + 1);
        let (result, mut run_samples) = profile::profile_command(&spec)?;
        profiles.push(result);
        samples.append(&mut run_samples);
    }
    let mut inventory = system::inventory(elevated);
    let (integrations, unavailable) = integrations::collect(&default_cwd, elevated);
    for integration in &integrations {
        if let Some(version) = &integration.version {
            inventory
                .tool_versions
                .insert(integration.name.clone(), version.clone());
        }
    }
    let mut findings = diagnosis::analyze(&[], &samples, &profiles);
    findings.extend(diagnosis::analyze_integrations(&integrations));
    Ok(Report {
        schema_version: SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").into(),
        run_id: Uuid::new_v4().to_string(),
        created_at: Utc::now(),
        kind: RunKind::Experiment,
        inventory,
        config: RunConfig {
            preset: None,
            target_hash: Some(system::hash_private(
                default_cwd.to_string_lossy().as_bytes(),
            )),
            offline: false,
            elevated_requested: elevated,
            duration_limit_seconds: None,
            disk_limit_bytes: None,
            memory_limit_bytes: None,
            experiment_hash: Some(system::hash_private(source)),
            live_llm: false,
            llm_route: None,
            llm_model: None,
            llm_cost_cap_usd: None,
        },
        metrics: vec![],
        samples,
        profiles,
        llm_runs: vec![],
        integrations,
        findings,
        warnings: vec![
            "Command arguments and environment values are represented only by hashes in the report"
                .into(),
        ],
        unavailable,
    })
}

fn to_spec(case: &ExperimentCase, default_cwd: &Path, save_output: bool) -> CommandSpec {
    CommandSpec {
        label: case.name.clone(),
        program: case.program.clone(),
        args: case.args.clone(),
        env: case.env.clone(),
        env_remove: vec![],
        working_directory: case
            .working_directory
            .clone()
            .unwrap_or_else(|| default_cwd.to_path_buf()),
        timeout: case.timeout_seconds.map(Duration::from_secs),
        capture_output: false,
        save_output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_uses_argument_arrays_and_defaults() {
        let config: ExperimentConfig = toml::from_str(
            r#"
            [[case]]
            name = "direct"
            program = "claude"
            args = ["-p", "hello"]

            [[case]]
            name = "proxy"
            program = "claude"
            args = ["-p", "hello"]
        "#,
        )
        .unwrap();
        assert_eq!(config.repetitions, 3);
        assert_eq!(config.case[0].args, ["-p", "hello"]);
    }
}
