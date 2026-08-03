//! Selecting a process and aggregating resource use across its descendants.
//!
//! A coding agent is never one process: a CLI spawns language servers, MCP servers, shells, and
//! compilers. Attributing resource use to "the agent" therefore means walking a subtree, which both
//! the profiler and the live views need to do identically.

use std::collections::HashSet;
use sysinfo::{Pid, System};

/// Aggregated resource use across a set of processes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TreeUsage {
    /// Summed CPU use **as a percentage of one core**, so the range is 0 to 100 × cores.
    ///
    /// This is `sysinfo`'s own scale for a process ("might be bigger than 100 if run on a multi-core
    /// machine"), and it is emphatically not the scale of [`System::global_cpu_usage`], which is 0 to
    /// 100 for the whole machine. The two get compared against each other by anything that reads a
    /// sample, so the difference is stated here rather than at each reader: measured on a 16-core
    /// Windows machine, four busy threads report 401 here and 55 there.
    ///
    /// A second hazard belongs with the first, because it looks like a value: `sysinfo` needs a
    /// process refreshed **three** times before this is a measurement. The first refresh arms the
    /// interval, the second saves the counters the delta is taken from, and only the third can
    /// subtract them - the documented "twice" is one short on Windows. Refreshes one and two report
    /// exactly `0.0`, which is indistinguishable from an idle process. Every caller that reads this
    /// therefore warms up first: `bench::sampler` discards two readings before it records one,
    /// `watch::collect::sampler::Sampler::prime` costs the daemon its first sample, and the prober takes
    /// one extra priming reading before the first probe of a session.
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub process_count: usize,
}

/// Find a process by pid, or else the longest-running process whose name contains `name_contains`.
///
/// Longest-running rather than first-found: transient helper processes share a name with the
/// long-lived session you actually mean.
pub fn select(system: &System, pid: Option<u32>, name_contains: &str) -> Option<Pid> {
    if let Some(pid) = pid {
        let pid = Pid::from_u32(pid);
        return system.process(pid).map(|_| pid);
    }
    let needle = name_contains.to_ascii_lowercase();
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
        .max_by_key(|(_, process)| process.run_time())
        .map(|(pid, _)| *pid)
}

/// Every process whose ancestry reaches `root`, including `root` itself.
///
/// Iterates to a fixed point because `sysinfo` yields processes in arbitrary order, so a child may be
/// visited before its parent has been recognised as part of the tree.
pub fn descendants(system: &System, root: Pid) -> HashSet<Pid> {
    let mut result = HashSet::from([root]);
    loop {
        let before = result.len();
        for (pid, process) in system.processes() {
            if process
                .parent()
                .is_some_and(|parent| result.contains(&parent))
            {
                result.insert(*pid);
            }
        }
        if result.len() == before {
            return result;
        }
    }
}

/// Sum CPU, resident memory, and cumulative disk bytes over `pids`.
///
/// Pids that have exited since collection are skipped rather than treated as zero, so
/// `process_count` reflects what was actually observable.
pub fn usage<'a>(system: &System, pids: impl IntoIterator<Item = &'a Pid>) -> TreeUsage {
    let mut usage = TreeUsage::default();
    for pid in pids {
        if let Some(process) = system.process(*pid) {
            usage.cpu_percent += process.cpu_usage();
            usage.rss_bytes += process.memory();
            let disk = process.disk_usage();
            usage.read_bytes += disk.total_read_bytes;
            usage.written_bytes += disk.total_written_bytes;
            usage.process_count += 1;
        }
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The current process is always its own descendant, and always measurable.
    #[test]
    fn descendants_always_include_the_root() {
        let mut system = System::new_all();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let root = Pid::from_u32(std::process::id());
        let tree = descendants(&system, root);
        assert!(tree.contains(&root));

        let usage = usage(&system, &tree);
        assert!(usage.process_count >= 1);
        assert!(usage.rss_bytes > 0);
    }

    #[test]
    fn select_prefers_an_explicit_pid_and_rejects_unknown_ones() {
        let mut system = System::new_all();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let own = std::process::id();
        assert_eq!(
            select(&system, Some(own), "no-such-name"),
            Some(Pid::from_u32(own))
        );
        assert_eq!(select(&system, Some(u32::MAX), "no-such-name"), None);
        assert_eq!(select(&system, None, "\u{0}no-such-process\u{0}"), None);
    }
}
