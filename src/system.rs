use crate::model::{DiskInfo, Inventory, SystemSample};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, env, fs, path::Path, process::Command, time::Instant};
use sysinfo::{Disks, System};

pub fn hash_private(value: impl AsRef<[u8]>) -> String {
    let digest = Sha256::digest(value.as_ref());
    hex::encode(&digest[..8])
}

pub fn redact_text(value: &str) -> String {
    let mut output = value.to_string();
    if let Some(home) = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        let home = home.to_string_lossy();
        output = output.replace(home.as_ref(), "<home>");
        output = output.replace(&home.replace('\\', "/"), "<home>");
    }
    output
}

pub fn inventory(elevated_requested: bool) -> Inventory {
    let mut system = System::new_all();
    system.refresh_all();
    let disks = Disks::new_with_refreshed_list();
    let cpu = system
        .cpus()
        .first()
        .map(|cpu| cpu.brand().trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    let hostname = System::host_name().unwrap_or_else(|| "unknown".into());
    let mut result = Inventory {
        os: System::name().unwrap_or_else(|| env::consts::OS.into()),
        os_version: System::long_os_version()
            .or_else(System::os_version)
            .unwrap_or_else(|| "unknown".into()),
        architecture: env::consts::ARCH.into(),
        hostname_hash: hash_private(hostname),
        cpu,
        physical_cores: system.physical_core_count(),
        logical_cores: system.cpus().len(),
        memory_bytes: system.total_memory(),
        disks: disks
            .iter()
            .map(|disk| DiskInfo {
                name: hash_private(disk.name().to_string_lossy().as_bytes()),
                kind: format!("{:?}", disk.kind()),
                total_bytes: disk.total_space(),
                available_bytes: disk.available_space(),
                filesystem: disk.file_system().to_string_lossy().into_owned(),
                removable: disk.is_removable(),
            })
            .collect(),
        power_source: power_source(),
        elevated: is_elevated(),
        tool_versions: BTreeMap::new(),
        config_fingerprints: config_fingerprints(),
    };
    if elevated_requested && !result.elevated {
        result
            .config_fingerprints
            .insert("elevation".into(), "requested_but_unavailable".into());
    }
    result
}

pub fn sample(system: &mut System, started: Instant) -> SystemSample {
    system.refresh_all();
    let scanner_names = [
        "msmpeng",
        "windefend",
        "sophos",
        "crowdstrike",
        "sentinelone",
        "clamd",
        "eset",
        "avast",
        "avg",
    ];
    let scanner_cpu: f32 = system
        .processes()
        .values()
        .filter(|p| {
            let name = p.name().to_string_lossy().to_ascii_lowercase();
            scanner_names.iter().any(|scanner| name.contains(scanner))
        })
        .map(|p| p.cpu_usage())
        .sum();
    SystemSample {
        elapsed_ms: started.elapsed().as_millis() as u64,
        cpu_percent: system.global_cpu_usage(),
        used_memory_bytes: system.used_memory(),
        used_swap_bytes: system.used_swap(),
        process_count: system.processes().len(),
        scanner_cpu_percent: (scanner_cpu > 0.0).then_some(scanner_cpu),
    }
}

pub fn tool_version(program: &str, args: &[&str]) -> Option<(String, u64)> {
    let started = Instant::now();
    let output = Command::new(program).args(args).output().ok()?;
    let elapsed = started.elapsed().as_millis() as u64;
    if !output.status.success() {
        return None;
    }
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr)
    } else {
        String::from_utf8_lossy(&output.stdout)
    };
    Some((
        text.lines().next().unwrap_or_default().trim().to_string(),
        elapsed,
    ))
}

pub fn path_fingerprint(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(hash_private(format!("{}:{modified}", metadata.len())))
}

fn config_fingerprints() -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    let home = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(std::path::PathBuf::from);
    if let Some(home) = home {
        for (name, relative) in [
            ("claude_settings", ".claude/settings.json"),
            ("headroom_config", ".headroom/config.toml"),
            ("tokensave_db", ".tokensave/tokensave.db"),
        ] {
            if let Some(value) = path_fingerprint(&home.join(relative)) {
                result.insert(name.into(), value);
            }
        }
    }
    for name in [
        "ANTHROPIC_BASE_URL",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "HEADROOM_PORT",
        "CLAUDE_CONFIG_DIR",
    ] {
        if env::var_os(name).is_some() {
            result.insert(format!("env:{name}"), "present".into());
        }
    }
    result
}

#[cfg(windows)]
fn is_elevated() -> bool {
    Command::new("net")
        .arg("session")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn is_elevated() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

#[cfg(windows)]
fn power_source() -> Option<String> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue).BatteryStatus",
        ])
        .output()
        .ok()?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        Some("ac_or_desktop".into())
    } else {
        Some(format!("battery_status_{value}"))
    }
}

#[cfg(target_os = "linux")]
fn power_source() -> Option<String> {
    let base = Path::new("/sys/class/power_supply");
    let entries = fs::read_dir(base).ok()?;
    for entry in entries.flatten() {
        let online = entry.path().join("online");
        if fs::read_to_string(online).ok()?.trim() == "1" {
            return Some("ac".into());
        }
    }
    Some("battery_or_unknown".into())
}

#[cfg(target_os = "macos")]
fn power_source() -> Option<String> {
    let output = Command::new("pmset").args(["-g", "batt"]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    Some(
        if text.contains("ac power") {
            "ac"
        } else {
            "battery"
        }
        .into(),
    )
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn power_source() -> Option<String> {
    None
}

pub fn native_diagnostics(_elevated_requested: bool) -> (serde_json::Value, Vec<String>) {
    let mut unavailable = Vec::new();
    // Only Windows and Linux have diagnostics to insert here. macOS contributes to `unavailable`
    // instead, and so never mutates this map.
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(unused_mut))]
    let mut data = serde_json::Map::new();
    #[cfg(windows)]
    {
        if _elevated_requested && is_elevated() {
            let script = "Get-MpComputerStatus | Select-Object AntivirusEnabled,RealTimeProtectionEnabled,BehaviorMonitorEnabled,IoavProtectionEnabled | ConvertTo-Json -Compress";
            match Command::new("powershell")
                .args(["-NoProfile", "-Command", script])
                .output()
            {
                Ok(out) if out.status.success() => {
                    if let Ok(value) = serde_json::from_slice(&out.stdout) {
                        data.insert("windows_defender".into(), value);
                    }
                }
                _ => unavailable.push("Windows Defender status".into()),
            }
        } else if _elevated_requested {
            unavailable.push(
                "elevated Windows security diagnostics (start from an Administrator terminal)"
                    .into(),
            );
        } else {
            unavailable.push(
                "Windows Defender details (rerun with --elevated from an Administrator terminal)"
                    .into(),
            );
        }
    }
    #[cfg(target_os = "linux")]
    {
        for (name, path) in [
            ("cpu_pressure", "/proc/pressure/cpu"),
            ("io_pressure", "/proc/pressure/io"),
            ("memory_pressure", "/proc/pressure/memory"),
        ] {
            if let Ok(text) = fs::read_to_string(path) {
                data.insert(name.into(), serde_json::Value::String(text.trim().into()));
            } else {
                unavailable.push(name.replace('_', " "));
            }
        }
    }
    #[cfg(target_os = "macos")]
    unavailable.push("macOS thermal pressure detail (requires powermetrics privileges)".into());
    (serde_json::Value::Object(data), unavailable)
}
