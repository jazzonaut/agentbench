use crate::{
    SCHEMA_VERSION, diagnosis, integrations,
    model::{ProfileResult, Report, RunConfig, RunKind, SystemSample},
    process_tree, system,
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::{
    collections::{HashMap, VecDeque},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use sysinfo::{Pid, ProcessesToUpdate, System};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub env_remove: Vec<String>,
    pub working_directory: PathBuf,
    pub timeout: Option<Duration>,
    pub capture_output: bool,
    pub save_output: bool,
}

pub struct CommandCapture {
    pub profile: ProfileResult,
    pub samples: Vec<SystemSample>,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub stdout_chunks: Vec<TimedChunk>,
}

pub struct TimedChunk {
    pub elapsed_ms: u64,
    pub text: String,
}

pub fn run_report(
    label: &str,
    command: &[String],
    timeout_seconds: Option<u64>,
    save_output: bool,
    elevated: bool,
) -> Result<Report> {
    if command.is_empty() {
        bail!("a command is required");
    }
    let cwd = std::env::current_dir()?;
    let spec = CommandSpec {
        label: label.into(),
        program: command[0].clone(),
        args: command[1..].to_vec(),
        env: HashMap::new(),
        env_remove: vec![],
        working_directory: cwd.clone(),
        timeout: timeout_seconds.map(Duration::from_secs),
        capture_output: false,
        save_output,
    };
    let started = Instant::now();
    let (profile, samples) = profile_command(&spec)?;
    let mut inventory = system::inventory(elevated);
    let (integrations, unavailable) = integrations::collect(&cwd, elevated);
    for integration in &integrations {
        if let Some(version) = &integration.version {
            inventory
                .tool_versions
                .insert(integration.name.clone(), version.clone());
        }
    }
    let mut findings = diagnosis::analyze(&[], &samples, std::slice::from_ref(&profile));
    findings.extend(diagnosis::analyze_integrations(&integrations));
    Ok(Report {
        schema_version: SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").into(),
        run_id: Uuid::new_v4().to_string(),
        created_at: Utc::now(),
        kind: RunKind::Profile,
        inventory,
        config: RunConfig {
            preset: None,
            target_hash: Some(system::hash_private(cwd.to_string_lossy().as_bytes())),
            offline: false,
            elevated_requested: elevated,
            duration_limit_seconds: timeout_seconds,
            disk_limit_bytes: None,
            memory_limit_bytes: None,
            experiment_hash: None,
            live_llm: false,
            llm_route: None,
            llm_model: None,
            llm_cost_cap_usd: None,
        },
        metrics: vec![],
        samples,
        profiles: vec![profile],
        llm_runs: vec![],
        integrations,
        findings,
        warnings: vec![format!(
            "profile completed in {:.1}s",
            started.elapsed().as_secs_f64()
        )],
        unavailable,
    })
}

pub fn profile_command(spec: &CommandSpec) -> Result<(ProfileResult, Vec<SystemSample>)> {
    let captured = profile_command_capture(spec)?;
    Ok((captured.profile, captured.samples))
}

pub fn profile_command_capture(spec: &CommandSpec) -> Result<CommandCapture> {
    let start_time = Utc::now();
    let started = Instant::now();
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .envs(&spec.env)
        .current_dir(&spec.working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in &spec.env_remove {
        command.env_remove(key);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("launch {}", spec.program))?;
    let root_pid = child.id();
    let stdout = child.stdout.take().context("capture stdout")?;
    let stderr = child.stderr.take().context("capture stderr")?;
    let (first_tx, first_rx) = mpsc::channel::<u64>();
    let capture_output = spec.capture_output || spec.save_output;
    let stdout_reader = spawn_reader(stdout, started, first_tx.clone(), capture_output);
    let stderr_reader = spawn_reader(stderr, started, first_tx, capture_output);

    let mut system = System::new_all();
    let mut samples = Vec::new();
    let mut first_output_ms = None;
    let mut peak_rss = 0_u64;
    let mut read_bytes = 0_u64;
    let mut written_bytes = 0_u64;
    let mut max_processes = 1_usize;
    let mut integrated_cpu_ms = 0_f64;
    let mut timed_out = false;
    let mut previous = Instant::now();

    let exit_status = loop {
        if first_output_ms.is_none() {
            first_output_ms = first_rx.try_recv().ok();
        }
        system.refresh_processes(ProcessesToUpdate::All, true);
        let tree = process_tree::descendants(&system, Pid::from_u32(root_pid));
        max_processes = max_processes.max(tree.len());
        let usage = process_tree::usage(&system, &tree);
        let tree_cpu = usage.cpu_percent;
        let tree_rss = usage.rss_bytes;
        let tree_read = usage.read_bytes;
        let tree_write = usage.written_bytes;
        let elapsed = previous.elapsed();
        previous = Instant::now();
        integrated_cpu_ms += tree_cpu as f64 / 100.0 * elapsed.as_secs_f64() * 1000.0;
        peak_rss = peak_rss.max(tree_rss);
        read_bytes = read_bytes.max(tree_read);
        written_bytes = written_bytes.max(tree_write);
        let mut sample = system::sample(&mut system, started);
        sample.cpu_percent = tree_cpu;
        samples.push(sample);
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if spec
            .timeout
            .is_some_and(|timeout| started.elapsed() >= timeout)
        {
            timed_out = true;
            for pid in tree.iter().filter(|pid| pid.as_u32() != root_pid) {
                if let Some(process) = system.process(*pid) {
                    let _ = process.kill();
                }
            }
            child.kill().context("terminate timed-out process")?;
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(250));
    };

    let (stdout_count, stdout_tail, stdout_chunks) = stdout_reader.join().unwrap_or_default();
    let (stderr_count, stderr_tail, _) = stderr_reader.join().unwrap_or_default();
    if first_output_ms.is_none() {
        first_output_ms = first_rx.try_recv().ok();
    }
    let output_tail = if spec.save_output {
        Some(format!(
            "stdout:\n{}\nstderr:\n{}",
            stdout_tail, stderr_tail
        ))
    } else {
        None
    };
    let program = Path::new(&spec.program)
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new(&spec.program))
        .to_string_lossy()
        .into_owned();
    let args_hash = system::hash_private(spec.args.join("\0"));
    let working_directory_hash =
        system::hash_private(spec.working_directory.to_string_lossy().as_bytes());
    let profile = ProfileResult {
        label: spec.label.clone(),
        program,
        args_hash,
        working_directory_hash,
        started_at: start_time,
        wall_ms: started.elapsed().as_millis() as u64,
        first_output_ms,
        exit_code: exit_status.code(),
        success: exit_status.success(),
        timed_out,
        peak_rss_bytes: peak_rss,
        cpu_time_ms: integrated_cpu_ms as u64,
        read_bytes,
        written_bytes,
        max_processes,
        output_bytes: stdout_count + stderr_count,
        output_tail,
        error: (!exit_status.success()).then(|| {
            if timed_out {
                "command timed out".into()
            } else {
                format!("command exited with {:?}", exit_status.code())
            }
        }),
    };
    Ok(CommandCapture {
        profile,
        samples,
        stdout_tail,
        stderr_tail,
        stdout_chunks,
    })
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    started: Instant,
    first: mpsc::Sender<u64>,
    save: bool,
) -> thread::JoinHandle<(u64, String, Vec<TimedChunk>)> {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        let mut total = 0_u64;
        let mut announced = false;
        const CAPTURE_LIMIT: usize = 4 * 1024 * 1024;
        let mut tail = VecDeque::with_capacity(CAPTURE_LIMIT);
        let mut chunks = Vec::new();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if !announced {
                        let _ = first.send(started.elapsed().as_millis() as u64);
                        announced = true;
                    }
                    total += n as u64;
                    if save {
                        chunks.push(TimedChunk {
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            text: String::from_utf8_lossy(&buffer[..n]).into_owned(),
                        });
                        for byte in &buffer[..n] {
                            if tail.len() == CAPTURE_LIMIT {
                                tail.pop_front();
                            }
                            tail.push_back(*byte);
                        }
                    }
                }
            }
        }
        let bytes: Vec<u8> = tail.into_iter().collect();
        (total, String::from_utf8_lossy(&bytes).into_owned(), chunks)
    })
}
