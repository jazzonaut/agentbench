use crate::{model::IntegrationResult, system};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{path::Path, process::Command, time::Instant};

pub fn collect(target: &Path, elevated: bool) -> (Vec<IntegrationResult>, Vec<String>) {
    let mut results = Vec::new();
    for (name, program, version_args) in [
        ("claude", "claude", vec!["--version"]),
        ("headroom", "headroom", vec!["--version"]),
        ("rtk", "rtk", vec!["--version"]),
        ("tokensave", "tokensave", vec!["--version"]),
    ] {
        results.push(version_probe(name, program, &version_args));
    }

    if command_exists("headroom") {
        results.push(json_command(
            "headroom_doctor",
            "headroom",
            &["doctor", "--json"],
        ));
        results.push(json_command(
            "headroom_perf",
            "headroom",
            &["perf", "--hours", "168", "--format", "json"],
        ));
    }

    let mut candidates = vec![target.join(".tokensave").join("tokensave.db")];
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        candidates.push(Path::new(&home).join(".tokensave").join("tokensave.db"));
    }
    if let Some(db) = candidates.iter().find(|p| p.is_file()) {
        results.push(tokensave_db(db));
    }

    let (native, unavailable) = system::native_diagnostics(elevated);
    results.push(IntegrationResult {
        name: "native_diagnostics".into(),
        available: !native.as_object().map(|m| m.is_empty()).unwrap_or(true),
        version: None,
        elapsed_ms: None,
        status: "collected capability-gated OS diagnostics".into(),
        data: native,
    });
    (results, unavailable)
}

fn command_exists(program: &str) -> bool {
    Command::new(program).arg("--version").output().is_ok()
}

fn version_probe(name: &str, program: &str, args: &[&str]) -> IntegrationResult {
    match system::tool_version(program, args) {
        Some((version, elapsed)) => IntegrationResult {
            name: name.into(),
            available: true,
            version: Some(version),
            elapsed_ms: Some(elapsed),
            status: "available".into(),
            data: json!({}),
        },
        None => IntegrationResult {
            name: name.into(),
            available: false,
            version: None,
            elapsed_ms: None,
            status: "not found or version probe failed".into(),
            data: json!({}),
        },
    }
}

fn json_command(name: &str, program: &str, args: &[&str]) -> IntegrationResult {
    let started = Instant::now();
    match Command::new(program).args(args).output() {
        Ok(output) => {
            let data = extract_json(&output.stdout)
                .map(sanitize_json)
                .unwrap_or_else(|| json!({"parse_error": true}));
            IntegrationResult {
                name: name.into(),
                available: true,
                version: None,
                elapsed_ms: Some(started.elapsed().as_millis() as u64),
                status: format!(
                    "exit {}",
                    output
                        .status
                        .code()
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "signal".into())
                ),
                data,
            }
        }
        Err(error) => IntegrationResult {
            name: name.into(),
            available: false,
            version: None,
            elapsed_ms: None,
            status: error.to_string(),
            data: json!({}),
        },
    }
}

fn extract_json(bytes: &[u8]) -> Option<Value> {
    if let Ok(value) = serde_json::from_slice(bytes) {
        return Some(value);
    }
    let (start, end) = if let (Some(start), Some(end)) = (
        bytes.iter().position(|byte| *byte == b'{'),
        bytes.iter().rposition(|byte| *byte == b'}'),
    ) {
        (start, end)
    } else {
        (
            bytes.iter().position(|byte| *byte == b'[')?,
            bytes.iter().rposition(|byte| *byte == b']')?,
        )
    };
    serde_json::from_slice(&bytes[start..=end]).ok()
}

fn sanitize_json(value: Value) -> Value {
    match value {
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| {
                    let normalized = key.to_ascii_lowercase();
                    let secret = matches!(
                        normalized.as_str(),
                        "api_key"
                            | "access_token"
                            | "refresh_token"
                            | "authorization"
                            | "password"
                            | "secret"
                    );
                    let identifier = normalized == "request_id"
                        || normalized.ends_with("_path")
                        || normalized == "path";
                    let value = if secret {
                        Value::String("<redacted>".into())
                    } else if identifier {
                        Value::String(system::hash_private(value.to_string()))
                    } else {
                        sanitize_json(value)
                    };
                    (key, value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(sanitize_json).collect()),
        Value::String(value) => Value::String(system::redact_text(&value)),
        other => other,
    }
}

fn tokensave_db(path: &Path) -> IntegrationResult {
    let started = Instant::now();
    let mut data = serde_json::Map::new();
    data.insert(
        "path_hash".into(),
        Value::String(system::hash_private(path.to_string_lossy().as_bytes())),
    );
    data.insert(
        "size_bytes".into(),
        json!(path.metadata().map(|m| m.len()).unwrap_or(0)),
    );
    let result = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).and_then(|conn| {
        let quick: String = conn.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
        data.insert("quick_check".into(), Value::String(quick));
        let tables: Vec<String> = conn.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name IN ('nodes','edges','files') ORDER BY name")?.query_map([], |row| row.get(0))?.filter_map(Result::ok).collect();
        data.insert("tables".into(), json!(tables));
        for table in ["nodes", "edges", "files"] {
            if tables.iter().any(|t| t == table) {
                let query = format!("SELECT count(*) FROM {table}");
                let timed = Instant::now();
                let count: i64 = conn.query_row(&query, [], |row| row.get(0))?;
                data.insert(format!("{table}_count"), json!(count));
                data.insert(format!("{table}_count_ms"), json!(timed.elapsed().as_secs_f64() * 1000.0));
            }
        }
        Ok(())
    });
    IntegrationResult {
        name: "tokensave_db".into(),
        available: result.is_ok(),
        version: None,
        elapsed_ms: Some(started.elapsed().as_millis() as u64),
        status: result
            .map(|_| "read-only health check complete".into())
            .unwrap_or_else(|e| format!("read-only check failed: {e}")),
        data: Value::Object(data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_after_noisy_prefix() {
        let value = extract_json(b"\x1b[1;31mprovider notice\x1b[0m\n{\"ok\":true}\n").unwrap();
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn sanitizes_secrets_paths_and_request_ids() {
        let value = json!({"api_key": "secret", "request_id": "abc", "summary": format!("config at {}", std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default())});
        let clean = sanitize_json(value);
        assert_eq!(clean["api_key"], "<redacted>");
        assert_ne!(clean["request_id"], "abc");
        assert!(clean["summary"].as_str().unwrap().contains("<home>"));
    }
}
