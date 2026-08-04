//! Process launch latency, measured against AgentBench's own hidden no-op subcommand.

use crate::{install, metrics::catalog, model::Metric};
use anyhow::{Result, bail};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

/// Launch and reap `launches` minimal child processes.
///
/// Spawning our own executable keeps the measurement comparable across machines: no dependency on
/// which shells or system utilities happen to be installed, and the image is already in cache.
///
/// The count is a parameter because process creation is one of the operations a security scanner
/// intercepts, which makes it worth measuring in the background as well as in a benchmark — and the
/// background wants far fewer launches for the same metric.
///
/// All three of the child's streams go to the null device, and that is part of the measurement rather than
/// tidiness. Inherited handles make the cost of a spawn depend on what the *parent's* stdout happens to be, so
/// the same benchmark run from a terminal, run with its output piped to a file, and run from a logon task with
/// no console at all measured three slightly different things under one metric name. The null device is the
/// same on every path.
///
/// It also stops the child talking. Under `cargo test` this program's own executable is the test harness, which
/// reads `internal-noop` as a filter matching nothing and says so — 45 copies of "running 0 tests … 558
/// filtered out" interleaved with the real results, which is where the summary went to hide.
pub fn run(launches: usize) -> Result<Vec<Metric>> {
    let current = std::env::current_exe()?;
    let executable = console_build(&current, |path| path.is_file());
    let mut times = Vec::new();
    for _ in 0..launches.max(1) {
        let started = Instant::now();
        let status = Command::new(&executable)
            .arg("internal-noop")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("internal process benchmark failed");
        }
        times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(vec![catalog::PROCESS_SPAWN_MS.distribution(&times)])
}

/// The build beside `program` that understands `internal-noop`, if `present` says it is there.
///
/// `current_exe` is the wrong answer under the windowless tray build. That binary starts a daemon
/// whatever its arguments say, so a child launched with `internal-noop` loads the configuration, fails
/// to take the instance lock its own parent is holding, adds a notification-area icon on the way out and
/// exits non-zero. The effect on the background prober was that the `process` phase failed on *every*
/// probe — `process.spawn_ms` permanently absent from a metric the README documents — while five
/// short-lived tray icons flickered in the notification area four times an hour.
///
/// The console executable is the one carrying the subcommand, and `install` puts both builds in the same
/// directory, so this is the same rename [`install::build_for`] performs for the logon task.
///
/// Falling back to `program` when the sibling is absent keeps every other caller behaving exactly as
/// before, the test harness included: its own executable has no `internal-noop` either, which is why the
/// probe's tests filter this one phase out.
fn console_build(program: &Path, present: impl FnOnce(&Path) -> bool) -> PathBuf {
    let console = install::build_for(program, false);
    if present(&console) {
        console
    } else {
        program.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The launched program must never be the build that ignores the subcommand.
    #[test]
    fn the_tray_build_launches_its_console_sibling_rather_than_itself() {
        let tray = Path::new(r"C:\Programs\AgentBench\agentbench-tray.exe");
        let resolved = console_build(tray, |_| true);
        assert_eq!(
            resolved,
            Path::new(r"C:\Programs\AgentBench\agentbench.exe")
        );
        assert!(!install::is_tray_build(&resolved));
    }

    /// Asking for the build already in hand has to be the identity, or the benchmark path changes.
    #[test]
    fn the_console_build_resolves_to_itself() {
        let console = Path::new(r"C:\Programs\AgentBench\agentbench.exe");
        assert_eq!(console_build(console, |_| true), console);
    }

    /// A lone executable — a `cargo build` of one target, or the test harness — is launched as it is.
    #[test]
    fn an_absent_sibling_leaves_the_running_executable_in_place() {
        let harness = Path::new("target/debug/deps/agentbench-1a2b3c");
        assert_eq!(console_build(harness, |_| false), harness);
    }
}
