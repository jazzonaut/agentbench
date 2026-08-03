//! Deciding which processes are worth measuring, and refreshing only those.
//!
//! A full process-table walk is the expensive part of sampling. Discovery runs on a slow cadence to
//! find agent and scanner pids; the fast sampling cadence then refreshes only that set.

use crate::process_tree;
use std::collections::HashSet;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// Processes currently considered interesting.
#[derive(Debug, Default, Clone)]
pub struct Targets {
    /// Agent process trees, flattened.
    pub agents: HashSet<Pid>,
    /// Security-scanner processes.
    pub scanners: HashSet<Pid>,
}

impl Targets {
    /// Every pid worth refreshing on the fast cadence.
    pub fn watched(&self) -> Vec<Pid> {
        self.agents.union(&self.scanners).copied().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty() && self.scanners.is_empty()
    }
}

/// What a process refresh needs to yield for attribution.
pub fn process_refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_disk_usage()
}

/// Re-enumerate the whole process table and rediscover interesting pids.
///
/// Expensive, so called on the slow discovery cadence only.
pub fn discover(system: &mut System, agent_names: &[String], scanner_names: &[String]) -> Targets {
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh_kind());

    let mut agents = HashSet::new();
    for name in agent_names {
        for root in matching_roots(system, name) {
            agents.extend(process_tree::descendants(system, root));
        }
    }
    let scanners = scanner_names
        .iter()
        .flat_map(|name| matching_roots(system, name))
        .collect();

    Targets { agents, scanners }
}

/// Refresh only the known pids.
///
/// Returns the pids that are still alive, so a caller can notice when its target set has decayed and
/// rediscovery is worthwhile before the next scheduled one.
pub fn refresh_watched(system: &mut System, targets: &Targets) -> usize {
    let watched = targets.watched();
    if watched.is_empty() {
        return 0;
    }
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&watched),
        false,
        process_refresh_kind(),
    );
    watched
        .iter()
        .filter(|pid| system.process(**pid).is_some())
        .count()
}

/// Every process whose name contains `needle`, case-insensitively.
///
/// Unlike [`process_tree::select`] this returns all matches rather than the longest-running one:
/// several agent sessions can legitimately run at once and all of them count.
fn matching_roots(system: &System, needle: &str) -> Vec<Pid> {
    let needle = needle.to_ascii_lowercase();
    system
        .processes()
        .iter()
        .filter(|(_, process)| {
            process
                .name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains(&needle)
        })
        .map(|(pid, _)| *pid)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_finds_this_process_by_name_and_refreshes_it() {
        let mut system = System::new();
        let own = Pid::from_u32(std::process::id());
        let name = {
            let mut probe = System::new();
            probe.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[own]),
                false,
                process_refresh_kind(),
            );
            probe
                .process(own)
                .map(|p| p.name().to_string_lossy().into_owned())
                .expect("own process is visible")
        };
        let targets = discover(&mut system, &[name], &[]);
        assert!(
            targets.agents.contains(&own),
            "own pid should be discovered as an agent match"
        );
        assert!(!targets.is_empty());
        assert!(refresh_watched(&mut system, &targets) >= 1);
    }

    #[test]
    fn no_matches_yields_empty_targets_and_no_refresh_work() {
        let mut system = System::new();
        let targets = discover(&mut system, &["\u{0}nothing\u{0}".into()], &[]);
        assert!(targets.is_empty());
        assert_eq!(refresh_watched(&mut system, &targets), 0);
    }
}
