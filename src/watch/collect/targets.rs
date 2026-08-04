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
        for root in named(system, name) {
            agents.extend(process_tree::descendants(system, root));
        }
    }
    let scanners = scanner_names
        .iter()
        .flat_map(|name| containing(system, name))
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

/// Every process whose name *is* `needle`, case-insensitively, with or without a file extension.
///
/// Exact rather than substring, which [`containing`] is, and the asymmetry is deliberate. An agent match is
/// expanded to its whole descendant tree and that tree's CPU is *summed*, so a name matching more broadly
/// than the user meant does not merely add a process: it adds everything that process ever started, and
/// enough idle helpers at one percent of a core each clear the "an agent is working" threshold between
/// them. Every probe is then tagged contended, the comparable subset empties, and every verdict reads
/// `insufficient` indefinitely. `claude` matching `claude-monitor.exe` is the shape of that fault.
///
/// The extension is optional so that a configured `"claude"` matches `claude.exe` on Windows and `claude`
/// everywhere else, and so writing the full file name is still an accepted way to name a process.
///
/// Unlike [`process_tree::select`] this returns all matches rather than the longest-running one:
/// several agent sessions can legitimately run at once and all of them count.
fn named(system: &System, needle: &str) -> Vec<Pid> {
    let needle = needle.to_ascii_lowercase();
    system
        .processes()
        .iter()
        .filter(|(_, process)| {
            let name = process.name().to_string_lossy().to_ascii_lowercase();
            let stem = name
                .rsplit_once('.')
                .map_or(name.as_str(), |(stem, _)| stem);
            name == needle || stem == needle
        })
        .map(|(pid, _)| *pid)
        .collect()
}

/// Every process whose name contains `needle`, case-insensitively.
///
/// Substring, and deliberately: the scanner list is fragments of real names — `msmpeng` for `MsMpEng.exe`,
/// `sophos` for `SophosFS.exe` — which no exact match would find. A scanner is also recorded as the single
/// matched process rather than expanded to a tree, so a loose match costs one process's CPU rather than a
/// subtree's, and the threshold it is read against is per-process.
///
/// Returns all matches, for the same reason [`named`] does: a machine can be running several.
fn containing(system: &System, needle: &str) -> Vec<Pid> {
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

    /// An agent name has to name a process, not appear inside one.
    ///
    /// The whole cost of a loose agent match is downstream: the match is expanded to a descendant tree and
    /// the tree's CPU is summed against a threshold meaning "an agent is working".
    #[test]
    fn an_agent_name_matches_a_whole_process_name_and_not_a_fragment_of_one() {
        let mut system = System::new();
        let own = Pid::from_u32(std::process::id());
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[own]),
            false,
            process_refresh_kind(),
        );
        let full = system
            .process(own)
            .map(|process| process.name().to_string_lossy().into_owned())
            .expect("own process is visible");
        let stem = full
            .rsplit_once('.')
            .map_or(full.as_str(), |(stem, _)| stem);

        // Both spellings of this process's own name are accepted.
        for spelling in [full.as_str(), stem, &full.to_ascii_uppercase()] {
            assert_eq!(
                named(&system, spelling),
                vec![own],
                "{spelling:?} should name this process"
            );
        }
        // A fragment of it is not, however suggestive.
        let fragment = &stem[..stem.len().saturating_sub(1)];
        assert!(
            named(&system, fragment).is_empty(),
            "{fragment:?} is a fragment, not a name"
        );
        // A scanner fragment still matches, because that list is written as fragments.
        assert_eq!(containing(&system, fragment), vec![own]);
    }
}
