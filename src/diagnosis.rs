use crate::{
    metrics::catalog,
    model::{
        Finding, IntegrationResult, LiveLlmRun, Metric, ProfileResult, Severity, SystemSample,
    },
};

/// Scanner CPU above which a slow filesystem result is worth attributing to it, **per core**.
///
/// `SystemSample::scanner_cpu_percent` is a sum of per-process readings, so it runs to 100 × cores and
/// 10.0 means a tenth of one core - see [`crate::process_tree::TreeUsage::cpu_percent`]. This was 2.0
/// and was read at the call site as "a couple of percent of total CPU"; on a 16-core machine it is one
/// eightieth of the machine, which a scanner sitting idle clears, so the higher-confidence branch was
/// effectively unconditional wherever a scanner was installed at all. The same mistake was found and
/// fixed on the dashboard's side of the tool, where the constant is
/// `watch::collect::probes::covariates::BUSY_SCANNER_CORE_PERCENT`; this is the same threshold for the
/// same reading, kept at the same value deliberately.
const SCANNER_BUSY_CORE_PERCENT: f32 = 10.0;

pub fn analyze(
    metrics: &[Metric],
    samples: &[SystemSample],
    profiles: &[ProfileResult],
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let metric = |name: &str| metrics.iter().find(|m| m.name == name);

    if let Some(m) = metric(catalog::FS_SEQUENTIAL_WRITE_MIB_S.name).filter(|m| m.value < 100.0) {
        findings.push(finding("disk", Severity::Warning, 0.75, "Low sequential filesystem write throughput",
            vec![format!("Measured {:.1} MiB/s; the diagnostic threshold is 100 MiB/s", m.value)],
            vec!["Filesystem cache, encryption, virtual disks, and concurrent workloads can affect this test"],
            vec!["Repeat against the actual repository volume", "Check drive health, free space, and active disk users"]));
    }
    if let Some(m) = metric(catalog::FS_SMALL_FILE_OPS_S.name).filter(|m| m.value < 1_000.0) {
        let scanner = samples
            .iter()
            .filter_map(|s| s.scanner_cpu_percent)
            .fold(0.0_f32, f32::max);
        let (confidence, mut evidence) = if scanner > SCANNER_BUSY_CORE_PERCENT {
            (
                0.85,
                vec![format!(
                    "A security-scanner process reached {scanner:.0}% of one core during the run \
                     (the threshold is {SCANNER_BUSY_CORE_PERCENT:.0}%)"
                )],
            )
        } else {
            (0.65, Vec::new())
        };
        evidence.insert(
            0,
            format!(
                "Measured {:.0} small-file operations/s; threshold is 1,000",
                m.value
            ),
        );
        findings.push(finding(
            "security_or_disk",
            Severity::Warning,
            confidence,
            "Small-file workload is slow",
            evidence,
            vec!["This does not prove antivirus causation"],
            vec![
                "Compare runs in an approved excluded and non-excluded test directory",
                "Inspect real-time scanner and indexing activity before changing exclusions",
            ],
        ));
    }
    let peak_swap = samples.iter().map(|s| s.used_swap_bytes).max().unwrap_or(0);
    if peak_swap > 512 * 1024 * 1024 {
        findings.push(finding(
            "memory",
            Severity::Warning,
            0.70,
            "Substantial swap usage observed",
            vec![format!(
                "Peak used swap was {:.1} GiB",
                peak_swap as f64 / 1_073_741_824.0
            )],
            vec!["Allocated swap alone does not prove active paging"],
            vec![
                "Close memory-heavy applications and repeat",
                "Use elevated/native counters to check hard page faults",
            ],
        ));
    }
    // Judged on the median rather than the p95. Every preset takes eight samples or fewer, and at that
    // size a p95 is the single worst request and nothing else (see `model::percentile_of_sorted`), so
    // the rule fired on one outlier while claiming to describe a tail. The worst request is still worth
    // showing, as evidence, under its own name.
    if let Some(m) = metric(catalog::NETWORK_HTTPS_LATENCY_MS.name)
        .filter(|m| m.p50.unwrap_or(m.value) > 1_000.0)
    {
        let mut evidence = vec![format!(
            "Median HTTPS latency was {:.0} ms over {} request(s); the threshold is 1,000 ms",
            m.p50.unwrap_or(m.value),
            m.samples
        )];
        if let Some(worst) = m.max {
            evidence.push(format!("The slowest request took {worst:.0} ms"));
        }
        findings.push(finding(
            "network",
            Severity::Warning,
            0.65,
            "High HTTPS latency",
            evidence,
            vec!["The public endpoint and current internet conditions contribute to this result"],
            vec![
                "Compare DNS/TLS timing on both machines at the same time",
                "Check VPN, proxy, DNS, and TLS-inspection differences",
            ],
        ));
    }
    if profiles.len() >= 2 {
        let direct = profiles
            .iter()
            .filter(|p| p.label.to_ascii_lowercase().contains("direct"))
            .map(|p| p.wall_ms as f64)
            .collect::<Vec<_>>();
        let proxied = profiles
            .iter()
            .filter(|p| {
                p.label.to_ascii_lowercase().contains("proxy")
                    || p.label.to_ascii_lowercase().contains("headroom")
            })
            .map(|p| p.wall_ms as f64)
            .collect::<Vec<_>>();
        if !direct.is_empty() && !proxied.is_empty() {
            let d = direct.iter().sum::<f64>() / direct.len() as f64;
            let p = proxied.iter().sum::<f64>() / proxied.len() as f64;
            let delta = (p - d) / d.max(1.0) * 100.0;
            if delta > 20.0 {
                findings.push(finding(
                    "proxy",
                    Severity::Warning,
                    0.85,
                    "Proxied command cases are materially slower",
                    vec![format!(
                        "Mean proxied wall time was {delta:.1}% above direct cases"
                    )],
                    vec!["Cases must perform equivalent work and remote model latency can vary"],
                    vec![
                        "Inspect Headroom perf transform/routing timings",
                        "Increase repetitions and keep interleaved ordering",
                    ],
                ));
            }
        }
    }
    findings
}

pub fn analyze_integrations(integrations: &[IntegrationResult]) -> Vec<Finding> {
    let mut findings = Vec::new();
    if let Some(perf) = integrations
        .iter()
        .find(|item| item.name == "headroom_perf")
    {
        let optimization = &perf.data["overhead"]["optimization_ms"];
        let p95 = optimization["p95_ms"].as_f64();
        let average = optimization["average_ms"].as_f64();
        let slow_pct = optimization["slow_request_pct"].as_f64();
        if p95.is_some_and(|value| value > 500.0) {
            let mut evidence = vec![format!(
                "Headroom optimization p95 was {:.0} ms",
                p95.unwrap()
            )];
            if let Some(value) = average {
                evidence.push(format!("Average optimization overhead was {value:.0} ms"));
            }
            if let Some(value) = slow_pct {
                evidence.push(format!(
                    "{value:.1}% of requests crossed Headroom's slow threshold"
                ));
            }
            findings.push(finding(
                "proxy",
                Severity::Warning,
                0.90,
                "Headroom reports material optimization overhead",
                evidence,
                vec!["This measures proxy processing, not total upstream model latency"],
                vec![
                    "Compare the same command direct and proxied",
                    "Inspect Headroom stage breakdown and recent version/configuration changes",
                ],
            ));
        }
    }
    if let Some(doctor) = integrations
        .iter()
        .find(|item| item.name == "headroom_doctor")
        && let Some(checks) = doctor.data["checks"].as_array()
    {
        let warnings: Vec<String> = checks
            .iter()
            .filter(|check| matches!(check["status"].as_str(), Some("warn" | "fail")))
            .filter_map(|check| {
                Some(format!(
                    "{}: {}",
                    check["name"].as_str()?,
                    check["summary"].as_str()?
                ))
            })
            .collect();
        if !warnings.is_empty() {
            findings.push(finding(
                "configuration",
                Severity::Warning,
                0.95,
                "Headroom doctor found routing or configuration warnings",
                warnings,
                vec!["Some warnings may be intentional for clients not using Headroom"],
                vec!["Run `headroom doctor` directly and resolve warnings relevant to the tested client"],
            ));
        }
    }
    findings
}

pub fn analyze_live_llm(runs: &[LiveLlmRun]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let failures: Vec<String> = runs
        .iter()
        .filter(|run| !run.success)
        .map(|run| {
            format!(
                "{}/{} repetition {} failed",
                run.route, run.scenario, run.repetition
            )
        })
        .collect();
    if !failures.is_empty() {
        findings.push(finding(
            "live_llm", Severity::Critical, 0.95, "One or more live Claude cases failed", failures,
            vec!["A failed route cannot be compared reliably"],
            vec!["Inspect the case exit status and verify Claude authentication and Headroom proxy health"],
        ));
    }
    let invalid: Vec<String> = runs
        .iter()
        .filter(|run| run.answer_valid == Some(false))
        .map(|run| {
            format!(
                "{}/{} repetition {} returned an unexpected answer",
                run.route, run.scenario, run.repetition
            )
        })
        .collect();
    if !invalid.is_empty() {
        findings.push(finding(
            "live_llm",
            Severity::Warning,
            0.85,
            "Live task correctness was inconsistent",
            invalid,
            vec!["Model output can vary even when the machine is healthy"],
            vec!["Inspect repeated failures before trusting the corresponding latency numbers"],
        ));
    }
    let slow_ttft: Vec<&LiveLlmRun> = runs
        .iter()
        .filter(|run| run.ttft_stream_ms.is_some_and(|value| value > 5_000))
        .collect();
    if slow_ttft.len() >= 2 {
        let worst = slow_ttft
            .iter()
            .filter_map(|run| run.ttft_stream_ms)
            .max()
            .unwrap_or(0);
        findings.push(finding(
            "network_or_provider",
            Severity::Warning,
            0.75,
            "Live Claude time-to-first-token is high",
            vec![format!(
                "{} runs exceeded 5 seconds; worst was {:.1} seconds",
                slow_ttft.len(),
                worst as f64 / 1000.0
            )],
            vec![
                "Provider load, model choice, prompt-cache state, and local routing all contribute",
            ],
            vec![
                "Compare direct cases across machines at the same time",
                "Check whether only Headroom-routed cases regress",
            ],
        ));
    }
    for scenario in ["latency", "throughput", "file_seek"] {
        let direct = mean_wall(runs, "direct", scenario);
        let headroom = mean_wall(runs, "headroom", scenario);
        if let (Some(direct), Some(headroom)) = (direct, headroom) {
            let delta = (headroom - direct) / direct.max(1.0) * 100.0;
            if delta > 20.0 {
                findings.push(finding(
                    "proxy", Severity::Warning, 0.90, &format!("Headroom is slower for the live {scenario} scenario"),
                    vec![format!("Mean Headroom wall time was {delta:.1}% above matched direct runs")],
                    vec!["Remote response variance remains unless enough repetitions complete"],
                    vec!["Compare Headroom optimization timing with the paired direct/Headroom delta"],
                ));
            }
        }
    }
    findings
}

fn mean_wall(runs: &[LiveLlmRun], route: &str, scenario: &str) -> Option<f64> {
    let values: Vec<u64> = runs
        .iter()
        .filter(|run| run.success && run.route == route && run.scenario == scenario)
        .map(|run| run.wall_ms)
        .collect();
    (!values.is_empty()).then(|| values.iter().sum::<u64>() as f64 / values.len() as f64)
}

fn finding(
    category: &str,
    severity: Severity,
    confidence: f32,
    title: &str,
    evidence: Vec<String>,
    limitations: Vec<&str>,
    recommendations: Vec<&str>,
) -> Finding {
    Finding {
        category: category.into(),
        severity,
        confidence,
        title: title.into(),
        evidence,
        limitations: limitations.into_iter().map(str::to_string).collect(),
        recommendations: recommendations.into_iter().map(str::to_string).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_small_files_with_scanner_overlap_has_high_confidence() {
        let metrics = vec![catalog::FS_SMALL_FILE_OPS_S.scalar(400.0)];
        let samples = vec![SystemSample {
            scanner_cpu_percent: Some(12.0),
            ..Default::default()
        }];
        let findings = analyze(&metrics, &samples, &[]);
        let finding = findings
            .iter()
            .find(|f| f.category == "security_or_disk")
            .unwrap();
        assert!(finding.confidence >= 0.8);
        assert!(
            finding
                .limitations
                .iter()
                .any(|v| v.contains("does not prove"))
        );
    }

    /// The scale trap: an idle scanner must not raise the confidence of a filesystem finding.
    ///
    /// Both numbers here are percentages of *one core*. At the previous threshold of 2.0 the second
    /// case reported 0.85 confidence and named a scanner in its evidence, on the strength of a process
    /// using a thirtieth of one core - which on a 16-core machine is two thousandths of the machine.
    #[test]
    fn an_installed_but_idle_scanner_does_not_raise_confidence() {
        let finding_for = |scanner: f32| {
            let metrics = vec![catalog::FS_SMALL_FILE_OPS_S.scalar(400.0)];
            let samples = vec![SystemSample {
                scanner_cpu_percent: Some(scanner),
                ..Default::default()
            }];
            analyze(&metrics, &samples, &[])
                .into_iter()
                .find(|f| f.category == "security_or_disk")
                .expect("a slow small-file result is always a finding")
        };

        let idle = finding_for(3.0);
        assert_eq!(idle.confidence, 0.65, "{:?}", idle.evidence);
        assert!(
            idle.evidence.iter().all(|line| !line.contains("scanner")),
            "an idle scanner must not be named as evidence: {:?}",
            idle.evidence
        );

        let busy = finding_for(60.0);
        assert_eq!(busy.confidence, 0.85);
        assert!(
            busy.evidence
                .iter()
                .any(|line| line.contains("of one core")),
            "the evidence must state the scale it is on: {:?}",
            busy.evidence
        );
    }

    #[test]
    fn healthy_synthetic_metrics_do_not_create_findings() {
        let metrics = vec![
            catalog::FS_SEQUENTIAL_WRITE_MIB_S.scalar(500.0),
            catalog::FS_SMALL_FILE_OPS_S.scalar(5_000.0),
            catalog::NETWORK_HTTPS_LATENCY_MS.distribution(&[20.0, 30.0]),
        ];
        assert!(analyze(&metrics, &[], &[]).is_empty());
    }
}
