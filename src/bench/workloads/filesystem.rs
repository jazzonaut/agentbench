//! Sequential and small-file throughput on the selected target volume.
//!
//! The two halves are independently callable because they measure different things and are useful at
//! very different scales: sequential throughput is close to a hardware property, whereas small-file
//! operation rate is the metric that moves when a security scanner or filesystem filter driver gets
//! involved.

use crate::{bench::cancel::check_cancel, metrics::catalog, model::Metric};
use anyhow::Result;
use std::{
    fs::{self, File},
    hint::black_box,
    io::{Read, Write},
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

/// Both halves, in the order the benchmark reports them.
pub fn run(
    dir: &Path,
    bytes: u64,
    small_files: usize,
    cancel: &Arc<AtomicBool>,
) -> Result<Vec<Metric>> {
    let mut metrics = sequential_io(dir, bytes, cancel)?;
    metrics.extend(small_file_ops(dir, small_files, cancel)?);
    Ok(metrics)
}

/// Write then re-read one large file, flushing before the write is timed as complete.
///
/// Note for callers choosing `bytes`: a small file is served entirely from the OS page cache on the
/// read pass, which makes the read figure a memory-bandwidth measurement rather than a disk one.
pub fn sequential_io(dir: &Path, bytes: u64, cancel: &Arc<AtomicBool>) -> Result<Vec<Metric>> {
    let data = dir.join("sequential.bin");
    let block = vec![0xA5_u8; 1 << 20];

    // The written total is counted rather than assumed. `bytes` is divided into whole blocks, so a
    // caller passing something that is not a multiple of the block size writes less than it asked for,
    // and dividing the requested figure by the elapsed time would report throughput the volume never
    // delivered. Every caller today passes a multiple of 1 MiB, which is exactly the kind of thing that
    // stops being true silently.
    let started = Instant::now();
    let mut file = File::create(&data)?;
    let mut written = 0_u64;
    for _ in 0..(bytes / block.len() as u64) {
        check_cancel(cancel)?;
        file.write_all(&block)?;
        written += block.len() as u64;
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

    Ok(vec![
        catalog::FS_SEQUENTIAL_WRITE_MIB_S.scalar(written as f64 / write_seconds / 1_048_576.0),
        catalog::FS_SEQUENTIAL_READ_MIB_S.scalar(read_bytes as f64 / read_seconds / 1_048_576.0),
    ])
}

/// Create, stat, rename, and delete `count` small files, timing all four passes together.
///
/// The working directory is created non-recursively so that a leftover directory from an interrupted
/// run fails loudly rather than silently polluting the rename pass with stale entries.
pub fn small_file_ops(dir: &Path, count: usize, cancel: &Arc<AtomicBool>) -> Result<Vec<Metric>> {
    let small_dir = dir.join("small-files");
    fs::create_dir(&small_dir)?;
    let started = Instant::now();

    for index in 0..count {
        if index % 100 == 0 {
            check_cancel(cancel)?;
        }
        fs::write(
            small_dir.join(format!("f-{index:08}.dat")),
            format!("agentbench-{index}"),
        )?;
    }
    for index in 0..count {
        if index % 100 == 0 {
            check_cancel(cancel)?;
        }
        let path = small_dir.join(format!("f-{index:08}.dat"));
        black_box(fs::metadata(path)?.len());
    }
    for index in 0..count {
        if index % 100 == 0 {
            check_cancel(cancel)?;
        }
        let from = small_dir.join(format!("f-{index:08}.dat"));
        let to = small_dir.join(format!("r-{index:08}.dat"));
        fs::rename(from, to)?;
    }
    for index in 0..count {
        if index % 100 == 0 {
            check_cancel(cancel)?;
        }
        fs::remove_file(small_dir.join(format!("r-{index:08}.dat")))?;
    }

    let seconds = started.elapsed().as_secs_f64();
    fs::remove_dir(&small_dir)?;
    let operations = count as f64 * 4.0;

    Ok(vec![
        catalog::FS_SMALL_FILE_OPS_S.scalar(operations / seconds),
        catalog::FS_SMALL_FILE_TOTAL_MS.scalar(seconds * 1000.0),
    ])
}
