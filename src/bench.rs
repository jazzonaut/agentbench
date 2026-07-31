use crate::{
    SCHEMA_VERSION, diagnosis, integrations, live_llm,
    model::{Metric, Report, RunConfig, RunKind, SystemSample},
    system,
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, params};
use std::{
    fs::{self, File},
    hint::black_box,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::Builder;
use uuid::Uuid;

#[derive(Debug, Copy, Clone)]
pub enum Preset {
    Quick,
    Standard,
    Stress,
}

#[derive(Debug, Clone)]
pub struct BenchOptions {
    pub offline: bool,
    pub elevated: bool,
    pub live_llm: bool,
    pub llm_route: live_llm::LlmRoute,
    pub llm_model: String,
    pub llm_cost_cap_usd: f64,
    pub headroom_port: u16,
}

impl BenchOptions {
    pub fn for_preset(preset: Preset) -> Self {
        Self {
            offline: false,
            elevated: false,
            live_llm: !matches!(preset, Preset::Quick),
            llm_route: live_llm::LlmRoute::Auto,
            llm_model: "sonnet".into(),
            llm_cost_cap_usd: 5.0,
            headroom_port: 8787,
        }
    }
}

#[derive(Debug, Clone)]
struct Limits {
    name: &'static str,
    duration_limit: Duration,
    disk_limit: u64,
    disk_working_set: u64,
    memory_fraction: f64,
    memory_cap: u64,
    cpu_seconds: u64,
    small_files: usize,
    sqlite_rows: usize,
    network_samples: usize,
    minimum_duration: Duration,
}

impl Preset {
    fn limits(self) -> Limits {
        match self {
            Self::Quick => Limits {
                name: "quick",
                duration_limit: Duration::from_secs(45),
                disk_limit: 128 << 20,
                disk_working_set: 64 << 20,
                memory_fraction: 0.10,
                memory_cap: 512 << 20,
                cpu_seconds: 2,
                small_files: 500,
                sqlite_rows: 2_000,
                network_samples: 2,
                minimum_duration: Duration::ZERO,
            },
            Self::Standard => Limits {
                name: "standard",
                duration_limit: Duration::from_secs(240),
                disk_limit: 2 << 30,
                disk_working_set: 512 << 20,
                memory_fraction: 0.25,
                memory_cap: 2 << 30,
                cpu_seconds: 5,
                small_files: 5_000,
                sqlite_rows: 20_000,
                network_samples: 4,
                minimum_duration: Duration::from_secs(180),
            },
            Self::Stress => Limits {
                name: "stress",
                duration_limit: Duration::from_secs(900),
                disk_limit: 10 << 30,
                disk_working_set: 2 << 30,
                memory_fraction: 0.50,
                memory_cap: 8 << 30,
                cpu_seconds: 30,
                small_files: 20_000,
                sqlite_rows: 100_000,
                network_samples: 8,
                minimum_duration: Duration::from_secs(600),
            },
        }
    }
}

pub fn run(preset: Preset, target: &Path, options: BenchOptions) -> Result<Report> {
    let cancel = Arc::new(AtomicBool::new(false));
    let handler_cancel = cancel.clone();
    let _ = ctrlc::set_handler(move || handler_cancel.store(true, Ordering::Relaxed));
    run_with_cancel(preset, target, options, cancel)
}

pub fn run_with_cancel(
    preset: Preset,
    target: &Path,
    options: BenchOptions,
    cancel: Arc<AtomicBool>,
) -> Result<Report> {
    let limits = preset.limits();
    let started = Instant::now();
    let mut inventory = system::inventory(options.elevated);
    let memory_size = ((inventory.memory_bytes as f64 * limits.memory_fraction) as u64)
        .min(limits.memory_cap)
        .max(16 << 20);
    let available = available_space_for(target).unwrap_or(u64::MAX);
    if available < limits.disk_working_set.saturating_mul(2) {
        bail!(
            "insufficient free space: need at least {:.1} GiB free beneath {}",
            limits.disk_working_set.saturating_mul(2) as f64 / 1_073_741_824.0,
            target.display()
        );
    }

    let stop = Arc::new(AtomicBool::new(false));
    let shared_samples = Arc::new(Mutex::new(Vec::<SystemSample>::new()));
    let mut sampler = SamplerGuard::spawn(stop.clone(), shared_samples.clone(), started);
    let temp = Builder::new()
        .prefix(".agentbench-tmp-")
        .tempdir_in(target)
        .context("create benchmark temporary directory")?;
    let mut metrics = Vec::new();
    let mut warnings = Vec::new();

    println!("[1/8] CPU benchmark");
    metrics.extend(cpu_benchmark(limits.cpu_seconds, &cancel)?);
    check_cancel(&cancel)?;
    println!(
        "[2/8] Memory benchmark ({:.0} MiB)",
        memory_size as f64 / 1_048_576.0
    );
    metrics.extend(memory_benchmark(memory_size as usize, &cancel)?);
    check_cancel(&cancel)?;
    println!(
        "[3/8] Filesystem benchmark ({:.0} MiB, {} small files)",
        limits.disk_working_set as f64 / 1_048_576.0,
        limits.small_files
    );
    metrics.extend(filesystem_benchmark(
        temp.path(),
        limits.disk_working_set,
        limits.small_files,
        &cancel,
    )?);
    check_cancel(&cancel)?;
    println!("[4/8] SQLite benchmark ({} rows)", limits.sqlite_rows);
    metrics.extend(sqlite_benchmark(temp.path(), limits.sqlite_rows, &cancel)?);
    check_cancel(&cancel)?;
    println!("[5/8] Process launch benchmark");
    metrics.extend(process_benchmark()?);
    check_cancel(&cancel)?;
    println!("[6/8] Loopback/network benchmark");
    metrics.extend(loopback_benchmark()?);
    check_cancel(&cancel)?;
    if !options.offline {
        match internet_benchmark(limits.network_samples, &cancel) {
            Ok(found) => metrics.extend(found),
            Err(error) => warnings.push(format!("internet benchmark skipped after error: {error}")),
        }
    }
    let mut profiles = Vec::new();
    let mut llm_runs = Vec::new();
    if options.live_llm {
        println!("[7/8] Live Claude benchmark (paid API/subscription traffic)");
        let minimum = if limits.minimum_duration.is_zero() {
            Duration::from_secs(30)
        } else {
            limits.minimum_duration
        };
        let live = live_llm::run_suite(
            &live_llm::LiveOptions {
                route: options.llm_route,
                model: options.llm_model.clone(),
                max_cost_usd: options.llm_cost_cap_usd,
                headroom_port: options.headroom_port,
                minimum_total_duration: minimum,
                maximum_total_duration: limits
                    .duration_limit
                    .saturating_sub(Duration::from_secs(10)),
            },
            target,
            temp.path(),
            started,
            &cancel,
        )?;
        metrics.extend(live.metrics);
        profiles.extend(live.profiles);
        llm_runs.extend(live.runs);
        warnings.extend(live.warnings);
    } else {
        println!("[7/8] Live Claude benchmark skipped (--no-live-llm)");
    }
    println!("[8/8] Agent integrations");
    let (integrations, mut unavailable) = integrations::collect(target, options.elevated);
    for integration in &integrations {
        if let Some(version) = &integration.version {
            inventory
                .tool_versions
                .insert(integration.name.clone(), version.clone());
        }
        if let Some(elapsed) = integration.elapsed_ms {
            metrics.push(Metric::scalar(
                format!("tool.{}_startup_ms", integration.name),
                elapsed as f64,
                "ms",
                true,
                "integrations",
            ));
        }
    }

    if started.elapsed() < limits.minimum_duration {
        let soak = sustained_seek_soak(
            temp.path(),
            limits.minimum_duration.saturating_sub(started.elapsed()),
            &cancel,
        )?;
        metrics.push(soak);
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

struct SamplerGuard {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SamplerGuard {
    fn spawn(
        stop: Arc<AtomicBool>,
        samples: Arc<Mutex<Vec<SystemSample>>>,
        started: Instant,
    ) -> Self {
        let worker_stop = stop.clone();
        let handle = thread::spawn(move || {
            let mut sys = sysinfo::System::new_all();
            while !worker_stop.load(Ordering::Relaxed) {
                if let Ok(mut output) = samples.lock() {
                    output.push(system::sample(&mut sys, started));
                }
                thread::sleep(Duration::from_millis(500));
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SamplerGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

fn cpu_benchmark(seconds: u64, cancel: &Arc<AtomicBool>) -> Result<Vec<Metric>> {
    let run = |duration: Duration, cancel: Arc<AtomicBool>| {
        let started = Instant::now();
        let mut iterations = 0_u64;
        let mut state = 0x9e3779b97f4a7c15_u64;
        while started.elapsed() < duration && !cancel.load(Ordering::Relaxed) {
            for _ in 0..10_000 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                iterations += 1;
            }
        }
        black_box(state);
        iterations as f64 / started.elapsed().as_secs_f64() / 1_000_000.0
    };
    let duration = Duration::from_secs(seconds.max(1));
    let single = run(duration, cancel.clone());
    check_cancel(cancel)?;
    let workers = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let started = Instant::now();
    let handles: Vec<_> = (0..workers)
        .map(|_| {
            let cancel = cancel.clone();
            thread::spawn(move || run(duration, cancel))
        })
        .collect();
    let total = handles.into_iter().filter_map(|h| h.join().ok()).sum();
    check_cancel(cancel)?;
    Ok(vec![
        Metric::scalar("cpu.single_mops_s", single, "Mops/s", false, "cpu"),
        Metric::scalar("cpu.multi_mops_s", total, "Mops/s", false, "cpu"),
        Metric::scalar(
            "cpu.multi_elapsed_ms",
            started.elapsed().as_secs_f64() * 1000.0,
            "ms",
            true,
            "cpu",
        ),
    ])
}

fn memory_benchmark(size: usize, cancel: &Arc<AtomicBool>) -> Result<Vec<Metric>> {
    let mut buffer = Vec::<u8>::new();
    buffer
        .try_reserve_exact(size)
        .context("reserve memory benchmark buffer")?;
    buffer.resize(size, 0);
    let started = Instant::now();
    for (index, byte) in buffer.iter_mut().enumerate() {
        if index % (16 << 20) == 0 {
            check_cancel(cancel)?;
        }
        *byte = (index as u8).wrapping_mul(31);
    }
    let write = size as f64 / started.elapsed().as_secs_f64() / 1_073_741_824.0;
    let started = Instant::now();
    let checksum: u64 = buffer.iter().step_by(64).map(|v| *v as u64).sum();
    black_box(checksum);
    let read = size as f64 / started.elapsed().as_secs_f64() / 1_073_741_824.0;
    Ok(vec![
        Metric::scalar("memory.write_gib_s", write, "GiB/s", false, "memory"),
        Metric::scalar("memory.read_gib_s", read, "GiB/s", false, "memory"),
    ])
}

fn filesystem_benchmark(
    dir: &Path,
    bytes: u64,
    small_files: usize,
    cancel: &Arc<AtomicBool>,
) -> Result<Vec<Metric>> {
    let data = dir.join("sequential.bin");
    let block = vec![0xA5_u8; 1 << 20];
    let started = Instant::now();
    let mut file = File::create(&data)?;
    for _ in 0..(bytes / block.len() as u64) {
        check_cancel(cancel)?;
        file.write_all(&block)?;
    }
    file.sync_all()?;
    let write_seconds = started.elapsed().as_secs_f64();
    drop(file);
    let started = Instant::now();
    let mut file = File::open(&data)?;
    let mut read_block = vec![0_u8; 1 << 20];
    let mut read_bytes = 0_u64;
    loop {
        let count = file.read(&mut read_block)?;
        if count == 0 {
            break;
        }
        read_bytes += count as u64;
        black_box(read_block[0]);
    }
    let read_seconds = started.elapsed().as_secs_f64();
    drop(file);
    fs::remove_file(&data)?;

    let small_dir = dir.join("small-files");
    fs::create_dir(&small_dir)?;
    let started = Instant::now();
    for index in 0..small_files {
        if index % 100 == 0 {
            check_cancel(cancel)?;
        }
        fs::write(
            small_dir.join(format!("f-{index:08}.dat")),
            format!("agentbench-{index}"),
        )?;
    }
    for index in 0..small_files {
        if index % 100 == 0 {
            check_cancel(cancel)?;
        }
        let path = small_dir.join(format!("f-{index:08}.dat"));
        black_box(fs::metadata(path)?.len());
    }
    for index in 0..small_files {
        if index % 100 == 0 {
            check_cancel(cancel)?;
        }
        let from = small_dir.join(format!("f-{index:08}.dat"));
        let to = small_dir.join(format!("r-{index:08}.dat"));
        fs::rename(from, to)?;
    }
    for index in 0..small_files {
        if index % 100 == 0 {
            check_cancel(cancel)?;
        }
        fs::remove_file(small_dir.join(format!("r-{index:08}.dat")))?;
    }
    let small_seconds = started.elapsed().as_secs_f64();
    fs::remove_dir(&small_dir)?;
    let operations = small_files as f64 * 4.0;
    Ok(vec![
        Metric::scalar(
            "filesystem.sequential_write_mib_s",
            bytes as f64 / write_seconds / 1_048_576.0,
            "MiB/s",
            false,
            "filesystem",
        ),
        Metric::scalar(
            "filesystem.sequential_read_mib_s",
            read_bytes as f64 / read_seconds / 1_048_576.0,
            "MiB/s",
            false,
            "filesystem",
        ),
        Metric::scalar(
            "filesystem.small_file_ops_s",
            operations / small_seconds,
            "ops/s",
            false,
            "filesystem",
        ),
        Metric::scalar(
            "filesystem.small_file_total_ms",
            small_seconds * 1000.0,
            "ms",
            true,
            "filesystem",
        ),
    ])
}

fn sqlite_benchmark(dir: &Path, rows: usize, cancel: &Arc<AtomicBool>) -> Result<Vec<Metric>> {
    let path = dir.join("sqlite-bench.db");
    let mut conn = Connection::open(&path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE nodes(id INTEGER PRIMARY KEY, name TEXT NOT NULL, payload BLOB NOT NULL); CREATE INDEX idx_nodes_name ON nodes(name);")?;
    let started = Instant::now();
    {
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare("INSERT INTO nodes(name,payload) VALUES (?1,?2)")?;
            let payload = vec![7_u8; 256];
            for index in 0..rows {
                if index % 1000 == 0 {
                    check_cancel(cancel)?;
                }
                insert.execute(params![format!("node-{index:08}"), &payload])?;
            }
        }
        tx.commit()?;
    }
    let insert_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut latencies = Vec::new();
    let mut statement = conn.prepare("SELECT length(payload) FROM nodes WHERE name=?1")?;
    for index in (0..rows).step_by((rows / 100).max(1)).take(100) {
        let started = Instant::now();
        let value: i64 = statement.query_row([format!("node-{index:08}")], |row| row.get(0))?;
        black_box(value);
        latencies.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    drop(statement);
    drop(conn);
    fs::remove_file(path).ok();
    Ok(vec![
        Metric::scalar(
            "sqlite.insert_rows_s",
            rows as f64 / (insert_ms / 1000.0),
            "rows/s",
            false,
            "sqlite",
        ),
        Metric::distribution("sqlite.lookup_ms", &latencies, "ms", true, "sqlite"),
    ])
}

fn process_benchmark() -> Result<Vec<Metric>> {
    let executable = std::env::current_exe()?;
    let mut times = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        let status = Command::new(&executable).arg("internal-noop").status()?;
        if !status.success() {
            bail!("internal process benchmark failed");
        }
        times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(vec![Metric::distribution(
        "process.spawn_ms",
        &times,
        "ms",
        true,
        "process",
    )])
}

fn loopback_benchmark() -> Result<Vec<Metric>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let bytes = 16 << 20;
    let server = thread::spawn(move || -> std::io::Result<()> {
        let (mut stream, _) = listener.accept()?;
        let mut buffer = vec![0_u8; 64 << 10];
        let mut total = 0;
        while total < bytes {
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total += count;
        }
        Ok(())
    });
    let started = Instant::now();
    let mut stream = TcpStream::connect(address)?;
    let connect_ms = started.elapsed().as_secs_f64() * 1000.0;
    let buffer = vec![42_u8; 64 << 10];
    let started = Instant::now();
    for _ in 0..(bytes / buffer.len()) {
        stream.write_all(&buffer)?;
    }
    stream.shutdown(std::net::Shutdown::Write)?;
    server
        .join()
        .map_err(|_| anyhow::anyhow!("loopback server panicked"))??;
    let mib_s = bytes as f64 / started.elapsed().as_secs_f64() / 1_048_576.0;
    Ok(vec![
        Metric::scalar(
            "network.loopback_connect_ms",
            connect_ms,
            "ms",
            true,
            "network",
        ),
        Metric::scalar("network.loopback_mib_s", mib_s, "MiB/s", false, "network"),
    ])
}

fn internet_benchmark(samples: usize, cancel: &Arc<AtomicBool>) -> Result<Vec<Metric>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("AgentBench/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut latencies = Vec::new();
    for _ in 0..samples {
        check_cancel(cancel)?;
        let started = Instant::now();
        let response = client.get("https://api.anthropic.com/").send()?;
        black_box(response.status());
        latencies.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(vec![Metric::distribution(
        "network.https_latency_ms",
        &latencies,
        "ms",
        true,
        "network",
    )])
}

fn available_space_for(path: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .iter()
        .filter(|disk| path.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| disk.available_space())
}

fn sustained_seek_soak(
    root: &Path,
    duration: Duration,
    cancel: &Arc<AtomicBool>,
) -> Result<Metric> {
    if duration.is_zero() {
        return Ok(Metric::scalar(
            "filesystem.sustained_seek_ops_s",
            0.0,
            "ops/s",
            false,
            "filesystem",
        ));
    }
    eprintln!(
        "Sustained file-seek/resource sampling for {:.0}s to complete the preset duration",
        duration.as_secs_f64()
    );
    let directory = root.join("sustained-seek");
    fs::create_dir_all(&directory)?;
    let payload = vec![0x5a_u8; 4096];
    let paths: Vec<_> = (0..512)
        .map(|index| directory.join(format!("seek-{index:04}.dat")))
        .collect();
    for path in &paths {
        fs::write(path, &payload)?;
    }
    let started = Instant::now();
    let mut operations = 0_u64;
    let mut index = 0_usize;
    while started.elapsed() < duration {
        if operations & 511 == 0 {
            check_cancel(cancel)?;
        }
        let path = &paths[(index.wrapping_mul(131)) % paths.len()];
        black_box(fs::metadata(path)?.len());
        let data = fs::read(path)?;
        black_box(data.first().copied());
        operations += 2;
        index = index.wrapping_add(1);
    }
    Ok(Metric::scalar(
        "filesystem.sustained_seek_ops_s",
        operations as f64 / started.elapsed().as_secs_f64(),
        "ops/s",
        false,
        "filesystem",
    ))
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("benchmark cancelled; temporary files were cleaned up");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_duration_is_three_to_four_minutes() {
        let limits = Preset::Standard.limits();
        assert_eq!(limits.minimum_duration, Duration::from_secs(180));
        assert_eq!(limits.duration_limit, Duration::from_secs(240));
    }
}
