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
    /// Bytes read over the whole life of these processes.
    pub read_bytes: u64,
    /// Bytes written over the whole life of these processes.
    pub written_bytes: u64,
    /// Bytes written since the previous refresh of these processes.
    ///
    /// Carries the same hazard as [`cpu_percent`] and a worse first reading: on the refresh that first
    /// sees a process this is not a delta at all but its entire lifetime's traffic. Measured on the
    /// development machine, the first reading of a full process table reported 12.2 GiB written and
    /// 33.8 GiB read "in one second". A caller therefore has to discard the reading that follows a
    /// discovery, and must report *absent* rather than zero for it — zero is a claim that nothing was
    /// written, which is exactly what a busy machine looks like if you get this wrong.
    ///
    /// **Attributable but partly blind.** An unelevated process cannot open a SYSTEM-owned process, so
    /// these counters read exactly zero for Defender, the update stack and the search indexer while
    /// their CPU still reads correctly. Whole-machine throughput has to come from
    /// [`crate::watch::platform::CounterReading::disk_write_bytes_s`] instead; this figure answers
    /// "which of the user's processes", not "how busy was the disk".
    ///
    /// [`cpu_percent`]: TreeUsage::cpu_percent
    pub written_delta_bytes: u64,
    /// Bytes read since the previous refresh, with the same caveats as [`written_delta_bytes`].
    ///
    /// [`written_delta_bytes`]: TreeUsage::written_delta_bytes
    pub read_delta_bytes: u64,
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

/// Sum CPU, resident memory, and both the cumulative and per-refresh disk bytes over `pids`.
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
            usage.written_delta_bytes += disk.written_bytes;
            usage.read_delta_bytes += disk.read_bytes;
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
