//! Temporary verification harness. Deleted before commit.
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;
use sysinfo::{Pid, System};

fn burn(threads: usize) -> (Arc<AtomicBool>, Vec<std::thread::JoinHandle<u64>>) {
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    for _ in 0..threads {
        let stop = stop.clone();
        handles.push(std::thread::spawn(move || {
            let mut x = 1u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                for _ in 0..100_000 { x = x.wrapping_mul(6364136223846793005).wrapping_add(1); }
            }
            x
        }));
    }
    (stop, handles)
}

/// The production path: exactly what `system::sample` and `process_tree::usage` do.
#[test]
fn v1_production_path_process_cpu_scale() {
    let own = Pid::from_u32(std::process::id());
    let mut sys = System::new_all();
    let (stop, handles) = burn(4);
    agentbench::system::refresh_for_sample(&mut sys);
    std::thread::sleep(Duration::from_millis(1500));
    agentbench::system::refresh_for_sample(&mut sys);
    let direct = sys.process(own).map(|p| p.cpu_usage()).unwrap_or(-1.0);
    let tree = agentbench::process_tree::usage(&sys, &[own]);
    let global = sys.global_cpu_usage();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for h in handles { let _ = h.join(); }
    let cores = sys.cpus().len();
    println!("V1 4 busy threads on {cores} logical cores:");
    println!("V1   process.cpu_usage()      = {direct:.1}");
    println!("V1   process_tree::usage().cpu = {:.1}", tree.cpu_percent);
    println!("V1   global_cpu_usage()        = {global:.1}");
    println!("V1 per-core scale confirmed (expect ~400 for 4 threads): {}", direct > 150.0);
}

/// The same reading through the watch sampler's own code, which is what tags contention.
#[test]
fn v1b_watch_sampler_reading_under_load() {
    use agentbench::watch::collect::targets;
    let mut sys = System::new();
    let own_name = {
        let mut probe = System::new_all();
        probe.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        probe.process(Pid::from_u32(std::process::id()))
            .map(|p| p.name().to_string_lossy().into_owned()).unwrap()
    };
    let (stop, handles) = burn(4);
    let t = targets::discover(&mut sys, &[own_name.clone()], &[]);
    std::thread::sleep(Duration::from_millis(1500));
    targets::refresh_watched(&mut sys, &t);
    let usage = agentbench::process_tree::usage(&sys, &t.agents);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for h in handles { let _ = h.join(); }
    println!("V1b agent tree ({} pids named {own_name}) cpu = {:.1}", t.agents.len(), usage.cpu_percent);
    println!("V1b AGENT_WORKING_CORE_PERCENT is 20.0, so this reads as working: {}",
             usage.cpu_percent > 20.0);
}
