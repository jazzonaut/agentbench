use crate::{
    model::{LiveLlmRun, Metric, ProfileResult, SystemSample},
    profile::{self, CommandSpec, TimedChunk},
    system,
};
use anyhow::{Result, bail};
use std::{
    collections::HashMap,
    fs,
    net::{SocketAddr, TcpStream},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LlmRoute {
    Auto,
    Direct,
    Headroom,
    Both,
}

pub struct LiveOptions {
    pub route: LlmRoute,
    pub model: String,
    pub max_cost_usd: f64,
    pub headroom_port: u16,
    pub minimum_total_duration: Duration,
    pub maximum_total_duration: Duration,
}

pub struct LiveOutcome {
    pub runs: Vec<LiveLlmRun>,
    pub profiles: Vec<ProfileResult>,
    pub samples: Vec<SystemSample>,
    pub metrics: Vec<Metric>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Copy, Clone)]
enum Scenario {
    Latency,
    Throughput,
    FileSeek,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Self::Latency => "latency",
            Self::Throughput => "throughput",
            Self::FileSeek => "file_seek",
        }
    }
}

pub fn run_suite(
    options: &LiveOptions,
    working_directory: &Path,
    scratch_directory: &Path,
    overall_started: Instant,
    cancel: &Arc<AtomicBool>,
) -> Result<LiveOutcome> {
    if system::tool_version("claude", &["--version"]).is_none() {
        bail!(
            "live LLM benchmark requested, but `claude` was not found; install it or pass --no-live-llm"
        );
    }
    let headroom_up = headroom_proxy_available(options.headroom_port);
    let routes: Vec<&str> = match options.route {
        LlmRoute::Auto if headroom_up => vec!["direct", "headroom"],
        LlmRoute::Auto | LlmRoute::Direct => vec!["direct"],
        LlmRoute::Headroom => {
            if !headroom_up {
                bail!(
                    "Headroom route requested, but no proxy is listening on 127.0.0.1:{}",
                    options.headroom_port
                );
            }
            vec!["headroom"]
        }
        LlmRoute::Both => {
            if !headroom_up {
                bail!(
                    "both routes requested, but no Headroom proxy is listening on 127.0.0.1:{}",
                    options.headroom_port
                );
            }
            vec!["direct", "headroom"]
        }
    };

    let fixture = scratch_directory.join("llm-file-seek-fixture");
    let expected = create_file_fixture(&fixture)?;
    let mut warnings = Vec::new();
    if options.route == LlmRoute::Auto && !headroom_up {
        warnings.push(
            "Headroom proxy was not detected; auto routing ran direct Claude cases only".into(),
        );
    }
    let mut runs = Vec::new();
    let mut profiles = Vec::new();
    let mut samples = Vec::new();
    let mut repetitions: HashMap<(String, String), usize> = HashMap::new();
    let mut route_failures: HashMap<String, usize> = HashMap::new();
    let scenarios = [Scenario::Latency, Scenario::Throughput, Scenario::FileSeek];
    let live_started = Instant::now();
    let mut scenario_index = 0_usize;
    let mut total_cost = 0.0_f64;

    while overall_started.elapsed() < options.minimum_total_duration
        && overall_started.elapsed() < options.maximum_total_duration
        && total_cost < options.max_cost_usd
    {
        check_cancel(cancel)?;
        let scenario = scenarios[scenario_index % scenarios.len()];
        let route_order: Vec<&str> = if (scenario_index / scenarios.len()) & 1 == 0 {
            routes.clone()
        } else {
            routes.iter().rev().copied().collect()
        };
        for route in route_order {
            check_cancel(cancel)?;
            if route_failures.get(route).copied().unwrap_or(0) >= 2 {
                continue;
            }
            let remaining = options
                .maximum_total_duration
                .saturating_sub(overall_started.elapsed());
            if remaining < Duration::from_secs(10) || total_cost >= options.max_cost_usd {
                break;
            }
            let key = (route.to_string(), scenario.name().to_string());
            let repetition = repetitions.entry(key).or_default();
            *repetition += 1;
            eprintln!(
                "Live Claude: {route}/{} repetition {} (spent ${total_cost:.3} of ${:.2} cap)",
                scenario.name(),
                *repetition,
                options.max_cost_usd
            );
            let per_run_budget = (options.max_cost_usd - total_cost).max(0.01);
            let timeout = remaining.min(Duration::from_secs(90));
            let (mut run, profile, mut run_samples) = run_case(
                route,
                scenario,
                *repetition,
                &options.model,
                per_run_budget,
                options.headroom_port,
                working_directory,
                &fixture,
                &expected,
                timeout,
            )?;
            if let Some(cost) = run.total_cost_usd {
                total_cost += cost;
            }
            if !run.success {
                *route_failures.entry(route.into()).or_default() += 1;
                run.error = run.error.map(|value| system::redact_text(&value));
            }
            runs.push(run);
            profiles.push(profile);
            samples.append(&mut run_samples);
        }
        scenario_index += 1;
        if routes
            .iter()
            .all(|route| route_failures.get(*route).copied().unwrap_or(0) >= 2)
        {
            warnings
                .push("all configured live routes were disabled after repeated failures".into());
            break;
        }
    }
    if total_cost >= options.max_cost_usd {
        warnings.push(format!(
            "live LLM phase stopped at the configured ${:.2} cost cap",
            options.max_cost_usd
        ));
    }
    for (route, failures) in route_failures {
        if failures >= 2 {
            warnings.push(format!(
                "{route} live route was disabled after repeated failures"
            ));
        }
    }
    let mut metrics = summarize(&runs);
    metrics.push(Metric::scalar(
        "llm.total_cost_usd",
        total_cost,
        "USD",
        true,
        "live_llm",
    ));
    metrics.push(Metric::scalar(
        "llm.phase_wall_seconds",
        live_started.elapsed().as_secs_f64(),
        "s",
        true,
        "live_llm",
    ));
    Ok(LiveOutcome {
        runs,
        profiles,
        samples,
        metrics,
        warnings,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_case(
    route: &str,
    scenario: Scenario,
    repetition: usize,
    model: &str,
    max_budget_usd: f64,
    headroom_port: u16,
    working_directory: &Path,
    fixture: &Path,
    expected: &[String],
    timeout: Duration,
) -> Result<(LiveLlmRun, ProfileResult, Vec<SystemSample>)> {
    let prompt = prompt_for(scenario, fixture);
    let mut args = vec![
        "-p".into(),
        prompt,
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--include-partial-messages".into(),
        "--no-session-persistence".into(),
        "--model".into(),
        model.into(),
        "--max-budget-usd".into(),
        format!("{max_budget_usd:.4}"),
    ];
    match scenario {
        Scenario::Latency | Scenario::Throughput => {
            args.extend(["--safe-mode".into(), "--tools".into(), "".into()])
        }
        Scenario::FileSeek => args.extend([
            "--tools".into(),
            "Read,Glob,Grep".into(),
            "--permission-mode".into(),
            "dontAsk".into(),
        ]),
    }
    let mut env = HashMap::new();
    let mut env_remove = Vec::new();
    if route == "headroom" {
        env.insert(
            "ANTHROPIC_BASE_URL".into(),
            format!("http://127.0.0.1:{headroom_port}"),
        );
        env.insert("ENABLE_TOOL_SEARCH".into(), "auto".into());
    } else {
        env_remove.push("ANTHROPIC_BASE_URL".into());
    }
    let label = format!("llm:{route}:{}#{repetition}", scenario.name());
    let spec = CommandSpec {
        label,
        program: "claude".into(),
        args,
        env,
        env_remove,
        working_directory: working_directory.into(),
        timeout: Some(timeout),
        capture_output: true,
        save_output: false,
    };
    let captured = profile::profile_command_capture(&spec)?;
    let run = parse_run(
        route,
        scenario,
        repetition,
        model,
        &captured.stdout_tail,
        &captured.stdout_chunks,
        &captured.profile,
        expected,
    );
    Ok((run, captured.profile, captured.samples))
}

#[allow(clippy::too_many_arguments)]
fn parse_run(
    route: &str,
    scenario: Scenario,
    repetition: usize,
    model: &str,
    stdout: &str,
    chunks: &[TimedChunk],
    profile: &ProfileResult,
    expected: &[String],
) -> LiveLlmRun {
    let values: Vec<serde_json::Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let result = values.iter().rev().find(|value| value["type"] == "result");
    let actual_model = values
        .iter()
        .find(|value| value["type"] == "system" && value["subtype"] == "init")
        .and_then(|value| value["model"].as_str())
        .unwrap_or(model);
    let output_chunks = values
        .iter()
        .filter(|value| {
            value["type"] == "stream_event"
                && value["event"]["type"] == "content_block_delta"
                && value["event"]["delta"]["type"] == "text_delta"
        })
        .count();
    let chunk_times: Vec<f64> = chunks
        .iter()
        .filter(|chunk| chunk.text.contains("content_block_delta"))
        .map(|chunk| chunk.elapsed_ms as f64)
        .collect();
    let gaps: Vec<f64> = chunk_times
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .filter(|gap| *gap >= 0.0)
        .collect();
    let (gap_p50, gap_p95) = percentiles(&gaps);
    let value = result.cloned().unwrap_or_default();
    let output_tokens = as_u64(&value["usage"]["output_tokens"]);
    let duration_api_ms = value["duration_api_ms"].as_u64();
    let ttft_stream_ms = value["ttft_stream_ms"].as_u64();
    let generation_ms = duration_api_ms
        .zip(ttft_stream_ms)
        .map(|(duration, ttft)| duration.saturating_sub(ttft));
    let tokens_per_second = generation_ms
        .filter(|duration| *duration > 0)
        .map(|duration| output_tokens as f64 / (duration as f64 / 1000.0));
    let answer = value["result"].as_str().unwrap_or_default();
    let answer_valid = match scenario {
        Scenario::Latency => Some(answer.trim().eq_ignore_ascii_case("PONG")),
        Scenario::Throughput => Some((250..=450).contains(&answer.split_whitespace().count())),
        Scenario::FileSeek => Some(expected.iter().all(|marker| answer.contains(marker))),
    };
    let success = profile.success && result.is_some() && value["is_error"].as_bool() != Some(true);
    LiveLlmRun {
        route: route.into(),
        scenario: scenario.name().into(),
        model: actual_model.into(),
        repetition,
        success,
        answer_valid,
        wall_ms: profile.wall_ms,
        time_to_request_ms: value["time_to_request_ms"].as_u64(),
        ttft_ms: value["ttft_ms"].as_u64(),
        ttft_stream_ms,
        duration_api_ms,
        input_tokens: as_u64(&value["usage"]["input_tokens"]),
        cache_creation_input_tokens: as_u64(&value["usage"]["cache_creation_input_tokens"]),
        cache_read_input_tokens: as_u64(&value["usage"]["cache_read_input_tokens"]),
        output_tokens,
        output_chunks,
        output_tokens_per_second: tokens_per_second,
        chunk_gap_p50_ms: gap_p50,
        chunk_gap_p95_ms: gap_p95,
        total_cost_usd: value["total_cost_usd"].as_f64(),
        error: (!success).then(|| {
            profile
                .error
                .clone()
                .unwrap_or_else(|| "Claude stream did not contain a successful result event".into())
        }),
    }
}

fn prompt_for(scenario: Scenario, fixture: &Path) -> String {
    match scenario {
        Scenario::Latency => "Reply with exactly the word PONG and nothing else.".into(),
        Scenario::Throughput => "Write exactly 300 words of plain prose explaining how operating-system file caches affect source-code search benchmarks. Do not use tools, headings, lists, or markdown.".into(),
        Scenario::FileSeek => format!("Use the available file search/read tools to inspect every relevant file beneath `{}`. Find all AB_NEEDLE markers and return only the sorted marker values, one per line. Do not guess.", fixture.display()),
    }
}

fn create_file_fixture(directory: &Path) -> Result<Vec<String>> {
    fs::create_dir_all(directory)?;
    let marker_indices = [17, 241, 499, 777, 1_103, 1_411, 1_733, 1_997];
    let mut expected = Vec::new();
    for index in 0..2_000_usize {
        let shard = directory.join(format!("shard-{:02}", index % 40));
        fs::create_dir_all(&shard)?;
        let mut content = format!(
            "fixture={index}\n{}\n",
            "ordinary searchable source text ".repeat(32)
        );
        if marker_indices.contains(&index) {
            let marker = format!("AB_VALUE_{index:04}");
            content.push_str(&format!("AB_NEEDLE={marker}\n"));
            expected.push(marker);
        }
        fs::write(shard.join(format!("module-{index:04}.txt")), content)?;
    }
    Ok(expected)
}

fn summarize(runs: &[LiveLlmRun]) -> Vec<Metric> {
    let mut groups: HashMap<(String, String), Vec<&LiveLlmRun>> = HashMap::new();
    for run in runs {
        groups
            .entry((run.route.clone(), run.scenario.clone()))
            .or_default()
            .push(run);
    }
    let mut metrics = Vec::new();
    for ((route, scenario), group) in groups {
        let prefix = format!("llm.{route}.{scenario}");
        let wall: Vec<f64> = group.iter().map(|run| run.wall_ms as f64).collect();
        let ttft: Vec<f64> = group
            .iter()
            .filter_map(|run| run.ttft_stream_ms.map(|v| v as f64))
            .collect();
        let speed: Vec<f64> = group
            .iter()
            .filter_map(|run| run.output_tokens_per_second)
            .collect();
        metrics.push(Metric::distribution(
            format!("{prefix}.wall_ms"),
            &wall,
            "ms",
            true,
            "live_llm",
        ));
        metrics.push(Metric::distribution(
            format!("{prefix}.ttft_stream_ms"),
            &ttft,
            "ms",
            true,
            "live_llm",
        ));
        if !speed.is_empty() {
            metrics.push(Metric::distribution(
                format!("{prefix}.output_tokens_s"),
                &speed,
                "tokens/s",
                false,
                "live_llm",
            ));
        }
    }
    metrics
}

fn headroom_proxy_available(port: u16) -> bool {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok()
        && system::tool_version("headroom", &["--version"]).is_some()
}

fn as_u64(value: &serde_json::Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|v| v as u64))
        .unwrap_or(0)
}

fn percentiles(values: &[f64]) -> (Option<f64>, Option<f64>) {
    if values.is_empty() {
        return (None, None);
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at = |p: f64| {
        sorted
            .get(((sorted.len() - 1) as f64 * p).round() as usize)
            .copied()
    };
    (at(0.50), at(0.95))
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("live LLM benchmark cancelled");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn parses_claude_result_and_stream_metrics() {
        let stdout = concat!(
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"PONG\"}}}\n",
            "{\"type\":\"result\",\"is_error\":false,\"duration_api_ms\":3000,\"ttft_ms\":1200,\"ttft_stream_ms\":1000,\"time_to_request_ms\":100,\"total_cost_usd\":0.01,\"usage\":{\"input_tokens\":10,\"output_tokens\":20},\"result\":\"PONG\"}\n"
        );
        let profile = ProfileResult {
            success: true,
            wall_ms: 3200,
            started_at: Utc::now(),
            ..Default::default()
        };
        let chunks = vec![TimedChunk {
            elapsed_ms: 1_100,
            text: "content_block_delta".into(),
        }];
        let run = parse_run(
            "direct",
            Scenario::Latency,
            1,
            "sonnet",
            stdout,
            &chunks,
            &profile,
            &[],
        );
        assert!(run.success);
        assert_eq!(run.ttft_stream_ms, Some(1000));
        assert_eq!(run.output_tokens_per_second, Some(10.0));
        assert_eq!(run.answer_valid, Some(true));
    }
}
