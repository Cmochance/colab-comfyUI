use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::runner::{CommandRunner, StdinPolicy};
use crate::ssh_tunnel::build_mux_exec_spec;
use crate::CoreError;

pub const MAX_TELEMETRY_SAMPLES: usize = 120;
pub const TELEMETRY_LOCAL_INTERVAL: Duration = Duration::from_secs(3);
pub const TELEMETRY_REMOTE_INTERVAL: Duration = Duration::from_secs(20);
pub const TELEMETRY_USAGE_INTERVAL: Duration = Duration::from_secs(60);

const LOCAL_HTTP_TIMEOUT: Duration = Duration::from_secs(2);
const REMOTE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelemetrySample {
    pub timestamp_unix: u64,
    pub gpu_util_pct: Option<f32>,
    pub vram_pct: Option<f32>,
    pub ram_pct: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TelemetrySnapshot {
    pub gpu_name: Option<String>,
    pub gpu_util_pct: Option<f32>,
    pub gpu_temp_c: Option<f32>,
    pub vram_used_bytes: Option<u64>,
    pub vram_total_bytes: Option<u64>,
    pub ram_used_bytes: Option<u64>,
    pub ram_total_bytes: Option<u64>,
    pub queue_running: u32,
    pub queue_pending: u32,
    pub content_free_bytes: Option<u64>,
    pub content_total_bytes: Option<u64>,
    pub drive_free_bytes: Option<u64>,
    pub drive_total_bytes: Option<u64>,
    pub estimated_remaining_minutes: Option<u64>,
    pub samples: Vec<TelemetrySample>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ComfySystemStats {
    pub gpu_name: Option<String>,
    pub vram_used_bytes: Option<u64>,
    pub vram_total_bytes: Option<u64>,
    pub ram_used_bytes: Option<u64>,
    pub ram_total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RemoteProbeMetrics {
    pub gpu_util_pct: Option<f32>,
    pub gpu_temp_c: Option<f32>,
    pub vram_used_bytes: Option<u64>,
    pub vram_total_bytes: Option<u64>,
    pub content_free_bytes: Option<u64>,
    pub content_total_bytes: Option<u64>,
    pub drive_free_bytes: Option<u64>,
    pub drive_total_bytes: Option<u64>,
}

pub fn push_telemetry_sample(samples: &mut Vec<TelemetrySample>, sample: TelemetrySample) {
    if samples.len() >= MAX_TELEMETRY_SAMPLES {
        let excess = samples.len() + 1 - MAX_TELEMETRY_SAMPLES;
        samples.drain(0..excess);
    }
    samples.push(sample);
}

/// Computes remaining minutes from CU balance and hourly usage rate.
/// Returns `None` when `rate_hourly <= 0.0` or inputs are non-finite.
pub fn estimated_remaining_minutes(balance: f64, rate_hourly: f64) -> Option<u64> {
    if !balance.is_finite() || !rate_hourly.is_finite() || rate_hourly <= 0.0 {
        return None;
    }
    if balance <= 0.0 {
        return Some(0);
    }
    let mins = ((balance / rate_hourly) * 60.0).floor();
    if !mins.is_finite() || mins < 0.0 {
        return Some(0);
    }
    Some(mins.min(u64::MAX as f64) as u64)
}

pub fn parse_comfy_system_stats(json_str: &str) -> Result<ComfySystemStats, CoreError> {
    let v: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| CoreError::Serialization {
            context: "parse /system_stats".to_string(),
            detail: e.to_string(),
        })?;
    let ram_total = v
        .get("system")
        .and_then(|s| s.get("ram_total"))
        .and_then(|n| n.as_u64());
    let ram_free = v
        .get("system")
        .and_then(|s| s.get("ram_free"))
        .and_then(|n| n.as_u64());
    let ram_used = match (ram_total, ram_free) {
        (Some(tot), Some(free)) => Some(tot.saturating_sub(free)),
        _ => None,
    };

    let first_dev = v
        .get("devices")
        .and_then(|d| d.as_array())
        .and_then(|arr| arr.first());
    let gpu_name = first_dev
        .and_then(|d| d.get("name"))
        .and_then(|s| s.as_str())
        .map(ToString::to_string);
    let vram_total = first_dev
        .and_then(|d| d.get("vram_total"))
        .and_then(|n| n.as_u64());
    let vram_free = first_dev
        .and_then(|d| d.get("vram_free"))
        .and_then(|n| n.as_u64());
    let vram_used = match (vram_total, vram_free) {
        (Some(tot), Some(free)) => Some(tot.saturating_sub(free)),
        _ => None,
    };

    Ok(ComfySystemStats {
        gpu_name,
        vram_used_bytes: vram_used,
        vram_total_bytes: vram_total,
        ram_used_bytes: ram_used,
        ram_total_bytes: ram_total,
    })
}

pub fn parse_comfy_queue(json_str: &str) -> Result<(u32, u32), CoreError> {
    let v: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| CoreError::Serialization {
            context: "parse /queue".to_string(),
            detail: e.to_string(),
        })?;
    let running = v
        .get("queue_running")
        .and_then(|a| a.as_array())
        .map(|a| a.len() as u32)
        .unwrap_or(0);
    let pending = v
        .get("queue_pending")
        .and_then(|a| a.as_array())
        .map(|a| a.len() as u32)
        .unwrap_or(0);
    Ok((running, pending))
}

pub fn parse_remote_probe(stdout: &str) -> RemoteProbeMetrics {
    let mut metrics = RemoteProbeMetrics::default();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("NVIDIA_SMI:") {
            let parts: Vec<&str> = rest.split(',').map(str::trim).collect();
            if parts.len() >= 2 {
                metrics.gpu_util_pct = parts[0].parse::<f32>().ok();
                metrics.gpu_temp_c = parts[1].parse::<f32>().ok();
            }
            if parts.len() >= 4 {
                if let (Ok(used_mib), Ok(tot_mib)) =
                    (parts[2].parse::<u64>(), parts[3].parse::<u64>())
                {
                    metrics.vram_used_bytes = Some(used_mib.saturating_mul(1024 * 1024));
                    metrics.vram_total_bytes = Some(tot_mib.saturating_mul(1024 * 1024));
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix("DF_CONTENT:") {
            let cols: Vec<&str> = rest.split_whitespace().collect();
            if cols.len() >= 3 {
                metrics.content_total_bytes = cols[0].parse::<u64>().ok();
                metrics.content_free_bytes = cols[2].parse::<u64>().ok();
            }
        } else if let Some(rest) = trimmed.strip_prefix("DF_DRIVE:") {
            let cols: Vec<&str> = rest.split_whitespace().collect();
            if cols.len() >= 3 {
                metrics.drive_total_bytes = cols[0].parse::<u64>().ok();
                metrics.drive_free_bytes = cols[2].parse::<u64>().ok();
            }
        }
    }
    metrics
}

/// Fetches a JSON body over the local loopback proxy (`127.0.0.1:<port>`) without
/// spawning any SSH process or external HTTP client crate.
pub async fn fetch_local_comfy_json(local_port: u16, path: &str) -> Result<String, CoreError> {
    let addr = format!("127.0.0.1:{local_port}");
    let fut = async {
        let mut stream = TcpStream::connect(&addr)
            .await
            .map_err(|source| CoreError::Io {
                context: format!("connect telemetry {addr}"),
                source,
            })?;
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{local_port}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
        );
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|source| CoreError::Io {
                context: format!("write telemetry {path}"),
                source,
            })?;
        let mut buf = Vec::new();
        stream
            .read_to_end(&mut buf)
            .await
            .map_err(|source| CoreError::Io {
                context: format!("read telemetry {path}"),
                source,
            })?;
        let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            return Err(CoreError::Serialization {
                context: format!("http response {path}"),
                detail: "missing header terminator".to_string(),
            });
        };
        let header = String::from_utf8_lossy(&buf[..pos]);
        let status_ok = header
            .lines()
            .next()
            .is_some_and(|line| line.contains(" 200 "));
        if !status_ok {
            return Err(CoreError::CommandFailed {
                command: format!("GET {path}"),
                exit_code: None,
                stderr: header.lines().next().unwrap_or("http error").to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&buf[pos + 4..]).to_string())
    };

    tokio::time::timeout(LOCAL_HTTP_TIMEOUT, fut)
        .await
        .map_err(|_| CoreError::CommandTimeout {
            command: format!("GET {path}"),
            timeout_secs: LOCAL_HTTP_TIMEOUT.as_secs(),
        })?
}

/// Runs a single lightweight remote probe over the existing `ControlMaster` socket (`ctl_sock`).
/// Never opens a new SSH connection.
pub async fn fetch_remote_probe(
    ctl_sock: &Path,
    runner: &dyn CommandRunner,
) -> Result<RemoteProbeMetrics, CoreError> {
    let remote_cmd = concat!(
        "if command -v nvidia-smi >/dev/null 2>&1; then ",
        "nvidia-smi --query-gpu=utilization.gpu,temperature.gpu,memory.used,memory.total --format=csv,noheader,nounits 2>/dev/null | head -n 1 | sed 's/^/NVIDIA_SMI:/'; ",
        "fi; ",
        "df -B1 /content 2>/dev/null | tail -n 1 | awk '{print \"DF_CONTENT:\" $2, $3, $4}'; ",
        "if [ -d /content/drive/MyDrive ] && grep -qs ' /content/drive' /proc/mounts; then ",
        "df -B1 /content/drive 2>/dev/null | tail -n 1 | awk '{print \"DF_DRIVE:\" $2, $3, $4}'; ",
        "fi"
    );
    let spec = build_mux_exec_spec(
        ctl_sock,
        remote_cmd,
        StdinPolicy::Null,
        REMOTE_PROBE_TIMEOUT,
    );
    let out = runner.run(spec, None).await?;
    Ok(parse_remote_probe(&out.stdout_raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comfy_system_stats_and_queue() {
        let stats_json = r#"{
            "system": {"os": "posix", "ram_total": 34359738368, "ram_free": 17179869184},
            "devices": [{"name": "cuda:0 NVIDIA L4", "type": "cuda", "vram_total": 24000000000, "vram_free": 12000000000}]
        }"#;
        let stats = parse_comfy_system_stats(stats_json).unwrap();
        assert_eq!(stats.gpu_name.as_deref(), Some("cuda:0 NVIDIA L4"));
        assert_eq!(stats.ram_total_bytes, Some(34359738368));
        assert_eq!(stats.ram_used_bytes, Some(17179869184));
        assert_eq!(stats.vram_total_bytes, Some(24000000000));
        assert_eq!(stats.vram_used_bytes, Some(12000000000));

        let queue_json = r#"{"queue_running": [["a"]], "queue_pending": [["b"], ["c"]]}"#;
        assert_eq!(parse_comfy_queue(queue_json).unwrap(), (1, 2));
    }

    #[test]
    fn parses_remote_probe_output() {
        let sample = "\
NVIDIA_SMI:42, 61, 8192, 23034
DF_CONTENT:107374182400 21474836480 85899345920
DF_DRIVE:16106127360 5368709120 10737418240
";
        let m = parse_remote_probe(sample);
        assert_eq!(m.gpu_util_pct, Some(42.0));
        assert_eq!(m.gpu_temp_c, Some(61.0));
        assert_eq!(m.vram_used_bytes, Some(8192 * 1024 * 1024));
        assert_eq!(m.vram_total_bytes, Some(23034 * 1024 * 1024));
        assert_eq!(m.content_total_bytes, Some(107374182400));
        assert_eq!(m.content_free_bytes, Some(85899345920));
        assert_eq!(m.drive_total_bytes, Some(16106127360));
        assert_eq!(m.drive_free_bytes, Some(10737418240));
    }

    #[test]
    fn estimated_remaining_minutes_handles_edge_cases() {
        assert_eq!(estimated_remaining_minutes(10.0, 0.0), None);
        assert_eq!(estimated_remaining_minutes(10.0, -1.0), None);
        assert_eq!(estimated_remaining_minutes(0.0, 2.0), Some(0));
        assert_eq!(estimated_remaining_minutes(-0.5, 2.0), Some(0));
        assert_eq!(estimated_remaining_minutes(3.0, 1.5), Some(120));
        assert_eq!(estimated_remaining_minutes(f64::NAN, 1.5), None);
    }

    #[test]
    fn sliding_window_caps_at_120_samples() {
        let mut samples = Vec::new();
        for i in 0..150u64 {
            push_telemetry_sample(
                &mut samples,
                TelemetrySample {
                    timestamp_unix: i,
                    gpu_util_pct: Some(50.0),
                    vram_pct: Some(40.0),
                    ram_pct: Some(30.0),
                },
            );
        }
        assert_eq!(samples.len(), MAX_TELEMETRY_SAMPLES);
        assert_eq!(samples.first().unwrap().timestamp_unix, 30);
        assert_eq!(samples.last().unwrap().timestamp_unix, 149);
    }
}
