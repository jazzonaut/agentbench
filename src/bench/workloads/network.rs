//! Loopback stack performance and, optionally, public HTTPS latency.

use crate::{bench::cancel::check_cancel, metrics::catalog, model::Metric};
use anyhow::Result;
use std::{
    hint::black_box,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

/// Endpoint used for the standalone HTTPS timing probe.
const HTTPS_PROBE_URL: &str = "https://api.anthropic.com/";

/// TCP connect latency and throughput over localhost, moving `bytes` through the socket.
///
/// Exercises the OS network stack, memory copies, and scheduling without any internet variability.
///
/// The volume is a parameter so the background prober can ask the same question far more cheaply.
/// Connect latency barely depends on it — that is the number a loopback filter driver moves — whereas
/// throughput is a memory-copy measurement that scales with the transfer and costs accordingly.
pub fn loopback(bytes: usize) -> Result<Vec<Metric>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let bytes = bytes.max(64 << 10);

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
        catalog::NETWORK_LOOPBACK_CONNECT_MS.scalar(connect_ms),
        catalog::NETWORK_LOOPBACK_MIB_S.scalar(mib_s),
    ])
}

/// End-to-end HTTPS request latency to the public Anthropic endpoint.
///
/// Skipped entirely under `--offline`. This sends no prompt and no credentials; it is a timing probe.
pub fn https(samples: usize, cancel: &Arc<AtomicBool>) -> Result<Vec<Metric>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("AgentBench/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut latencies = Vec::new();
    for _ in 0..samples {
        check_cancel(cancel)?;
        let started = Instant::now();
        let response = client.get(HTTPS_PROBE_URL).send()?;
        black_box(response.status());
        latencies.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(vec![
        catalog::NETWORK_HTTPS_LATENCY_MS.distribution(&latencies),
    ])
}
