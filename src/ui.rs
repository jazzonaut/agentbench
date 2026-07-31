use anyhow::{Result, bail};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use sysinfo::{Pid, ProcessesToUpdate, System};

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
        Ok(Self)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

pub fn run_task<T, F>(title: &str, task: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(Arc<AtomicBool>) -> Result<T> + Send + 'static,
{
    let (tx, rx) = mpsc::sync_channel(1);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    thread::spawn(move || {
        let _ = tx.send(task(worker_cancel));
    });
    let _guard = TerminalGuard::enter()?;
    let started = Instant::now();
    let mut system = System::new_all();
    loop {
        if let Ok(result) = rx.try_recv() {
            return result;
        }
        system.refresh_all();
        draw_header(title)?;
        let total = system.total_memory().max(1);
        println_line(
            format!("Elapsed       {:>8.1} s", started.elapsed().as_secs_f64()),
            Color::White,
        )?;
        println_line(
            format!("System CPU    {:>8.1} %", system.global_cpu_usage()),
            Color::Cyan,
        )?;
        println_line(
            format!(
                "Memory        {:>8.1} / {:.1} GiB",
                system.used_memory() as f64 / 1_073_741_824.0,
                total as f64 / 1_073_741_824.0
            ),
            Color::Magenta,
        )?;
        println_line(
            format!(
                "Swap          {:>8.1} GiB",
                system.used_swap() as f64 / 1_073_741_824.0
            ),
            Color::Yellow,
        )?;
        println_line(
            format!("Processes     {:>8}", system.processes().len()),
            Color::White,
        )?;
        println_line(
            "Benchmark phases and detailed timings are emitted after the dashboard closes.".into(),
            Color::DarkGrey,
        )?;
        let footer = if cancel.load(Ordering::Relaxed) {
            "Cancellation requested; cleaning up temporary data…"
        } else {
            "[q/Ctrl+C] cancel safely"
        };
        println_line(footer.into(), Color::DarkGrey)?;
        io::stdout().flush()?;
        if event::poll(Duration::from_millis(250))?
            && matches!(event::read()?, Event::Key(key) if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('c')))
        {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

pub fn dashboard(pid: Option<u32>, name: Option<&str>, interval_ms: u64) -> Result<()> {
    if interval_ms < 100 {
        bail!("--interval-ms must be at least 100");
    }
    let _guard = TerminalGuard::enter()?;
    let started = Instant::now();
    let mut system = System::new_all();
    loop {
        system.refresh_processes(ProcessesToUpdate::All, true);
        system.refresh_cpu_all();
        system.refresh_memory();
        let root = select_process(&system, pid, name);
        let processes = root
            .map(|root| tree_pids(&system, root))
            .unwrap_or_default();
        let mut cpu = 0.0_f32;
        let mut rss = 0_u64;
        let mut read = 0_u64;
        let mut written = 0_u64;
        for process_pid in &processes {
            if let Some(process) = system.process(*process_pid) {
                cpu += process.cpu_usage();
                rss += process.memory();
                let disk = process.disk_usage();
                read += disk.total_read_bytes;
                written += disk.total_written_bytes;
            }
        }
        draw_header("AgentBench live dashboard")?;
        println_line(
            format!("Elapsed       {:>8.1} s", started.elapsed().as_secs_f64()),
            Color::White,
        )?;
        println_line(
            format!("System CPU    {:>8.1} %", system.global_cpu_usage()),
            Color::Cyan,
        )?;
        println_line(
            format!(
                "System memory {:>8.1} / {:.1} GiB",
                system.used_memory() as f64 / 1_073_741_824.0,
                system.total_memory() as f64 / 1_073_741_824.0
            ),
            Color::Magenta,
        )?;
        println_line(
            format!(
                "Used swap     {:>8.1} GiB",
                system.used_swap() as f64 / 1_073_741_824.0
            ),
            Color::Yellow,
        )?;
        println_line(String::new(), Color::White)?;
        if let Some(root) = root {
            let process_name = system
                .process(root)
                .map(|p| p.name().to_string_lossy().into_owned())
                .unwrap_or_else(|| "exited".into());
            println_line(
                format!("Observed root {} ({})", root.as_u32(), process_name),
                Color::Green,
            )?;
            println_line(format!("Tree CPU      {:>8.1} %", cpu), Color::Cyan)?;
            println_line(
                format!("Tree RSS      {:>8.1} MiB", rss as f64 / 1_048_576.0),
                Color::Magenta,
            )?;
            println_line(
                format!("Tree processes{:>8}", processes.len()),
                Color::White,
            )?;
            println_line(
                format!("Total reads   {:>8.1} MiB", read as f64 / 1_048_576.0),
                Color::Blue,
            )?;
            println_line(
                format!("Total writes  {:>8.1} MiB", written as f64 / 1_048_576.0),
                Color::Blue,
            )?;
        } else {
            println_line(
                "No matching process. Showing system metrics until one appears.".into(),
                Color::Yellow,
            )?;
        }
        println_line(String::new(), Color::White)?;
        println_line("[q] quit".into(), Color::DarkGrey)?;
        io::stdout().flush()?;
        if event::poll(Duration::from_millis(interval_ms))?
            && matches!(event::read()?, Event::Key(key) if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc))
        {
            break;
        }
    }
    Ok(())
}

fn draw_header(title: &str) -> Result<()> {
    execute!(
        io::stdout(),
        cursor::MoveTo(0, 0),
        terminal::Clear(ClearType::All),
        SetForegroundColor(Color::Green),
        Print(format!("{title}\n{}\n", "═".repeat(title.len()))),
        ResetColor
    )?;
    Ok(())
}

fn println_line(value: String, color: Color) -> Result<()> {
    execute!(
        io::stdout(),
        SetForegroundColor(color),
        Print(value),
        Print("\n"),
        ResetColor
    )?;
    Ok(())
}

fn select_process(system: &System, pid: Option<u32>, name: Option<&str>) -> Option<Pid> {
    if let Some(pid) = pid {
        let pid = Pid::from_u32(pid);
        return system.process(pid).map(|_| pid);
    }
    let needle = name.unwrap_or("claude").to_ascii_lowercase();
    system
        .processes()
        .iter()
        .filter(|(_, p)| {
            p.name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains(&needle)
        })
        .max_by_key(|(_, p)| p.run_time())
        .map(|(pid, _)| *pid)
}

fn tree_pids(system: &System, root: Pid) -> Vec<Pid> {
    let mut result = vec![root];
    loop {
        let before = result.len();
        for (pid, process) in system.processes() {
            if process
                .parent()
                .is_some_and(|parent| result.contains(&parent))
                && !result.contains(pid)
            {
                result.push(*pid);
            }
        }
        if result.len() == before {
            break;
        }
    }
    result
}
