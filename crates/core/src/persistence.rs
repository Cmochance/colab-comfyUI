use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use crate::config::validate_session_name;
use crate::runner::{redact_sensitive_text, BoxFuture, CommandRunner, StdinPolicy};
use crate::ssh_tunnel::build_mux_exec_spec;
use crate::CoreError;

pub const MAX_DRIVE_MOUNT_ATTEMPTS: u8 = 2;
pub const DRIVE_AUTH_TIMEOUT: Duration = Duration::from_secs(600);
pub const DRIVE_FAILED_GRACE_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_DRIVE_EXPORT_TIMEOUT_SECS: u64 = 90;
const DRIVE_CHECK_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DriveAuthStatus {
    WaitingUser,
    Continuing,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveAuthPrompt {
    pub auth_url: Option<String>,
    pub started_at_unix: u64,
    pub deadline_unix: u64,
    pub grace_deadline_unix: Option<u64>,
    pub attempt: u8,
    pub max_attempts: u8,
    pub status: DriveAuthStatus,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveExportReport {
    pub ok: bool,
    pub flush_ok: bool,
    pub unmounted: bool,
    pub workflow_updated: bool,
    pub user_files: usize,
    pub output_files: usize,
    pub input_files: usize,
    pub custom_nodes_count: usize,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountProbe {
    MountedHealthy,
    MountedUnhealthy,
    NotMounted,
}

/// Line emitted by `colab drivemount`, carrying both redacted log text and
/// any unredacted authorization URL extracted prior to log redaction (P1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrivemountLine {
    pub redacted: String,
    pub auth_url: Option<String>,
    pub is_auth_prompt: bool,
}

/// Interactive child process handle for `colab drivemount` (detached via `setsid()`).
pub trait DrivemountProcess: Send {
    fn next_line<'a>(&'a mut self) -> BoxFuture<'a, Option<DrivemountLine>>;
    fn send_newline<'a>(&'a mut self) -> BoxFuture<'a, Result<(), CoreError>>;
    fn wait<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<i32>, CoreError>>;
    fn kill<'a>(&'a mut self) -> BoxFuture<'a, Result<(), CoreError>>;
}

pub trait DrivemountSpawner: Send + Sync {
    fn spawn(
        &self,
        program: &Path,
        args: &[String],
        envs: &[(String, String)],
    ) -> Result<Box<dyn DrivemountProcess>, CoreError>;
}

pub struct TokioDrivemountProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    lines_rx: mpsc::UnboundedReceiver<DrivemountLine>,
}

impl DrivemountProcess for TokioDrivemountProcess {
    fn next_line<'a>(&'a mut self) -> BoxFuture<'a, Option<DrivemountLine>> {
        Box::pin(async move { self.lines_rx.recv().await })
    }

    fn send_newline<'a>(&'a mut self) -> BoxFuture<'a, Result<(), CoreError>> {
        Box::pin(async move {
            if let Some(mut stdin) = self.stdin.take() {
                stdin
                    .write_all(b"\n")
                    .await
                    .map_err(|source| CoreError::Io {
                        context: "write newline to drivemount stdin".to_string(),
                        source,
                    })?;
                let _ = stdin.flush().await;
                let _ = stdin.shutdown().await;
            }
            Ok(())
        })
    }

    fn wait<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<i32>, CoreError>> {
        Box::pin(async move {
            let status = self.child.wait().await.map_err(|source| CoreError::Io {
                context: "wait drivemount".to_string(),
                source,
            })?;
            Ok(status.code())
        })
    }

    fn kill<'a>(&'a mut self) -> BoxFuture<'a, Result<(), CoreError>> {
        Box::pin(async move {
            let _ = self.child.kill().await;
            Ok(())
        })
    }
}

#[derive(Debug, Default)]
pub struct TokioDrivemountSpawner;

impl DrivemountSpawner for TokioDrivemountSpawner {
    fn spawn(
        &self,
        program: &Path,
        args: &[String],
        envs: &[(String, String)],
    ) -> Result<Box<dyn DrivemountProcess>, CoreError> {
        let mut cmd = Command::new(program);
        cmd.kill_on_drop(true);
        cmd.args(args);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Detach from any controlling TTY so `colab drivemount` falls back to stdin pipe
        // instead of hanging on `/dev/tty` (`automation.py`).
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = cmd.spawn().map_err(|source| CoreError::Io {
            context: format!("spawn {}", program.display()),
            source,
        })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (tx, rx) = mpsc::unbounded_channel();

        for stream_opt in [stdout.map(Left), stderr.map(Right)] {
            match stream_opt {
                Some(Left(out)) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(out).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let _ = tx.send(parse_drivemount_line(&line));
                        }
                    });
                }
                Some(Right(err)) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(err).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let _ = tx.send(parse_drivemount_line(&line));
                        }
                    });
                }
                None => {}
            }
        }

        Ok(Box::new(TokioDrivemountProcess {
            child,
            stdin,
            lines_rx: rx,
        }))
    }
}

enum EitherStream<A, B> {
    Left(A),
    Right(B),
}
use EitherStream::{Left, Right};

pub fn parse_drivemount_line(raw_line: &str) -> DrivemountLine {
    DrivemountLine {
        auth_url: extract_drive_auth_url(raw_line),
        is_auth_prompt: is_drive_auth_prompt_line(raw_line),
        redacted: redact_sensitive_text(raw_line),
    }
}

pub fn drivemount_args(session_name: &str) -> Result<Vec<String>, CoreError> {
    validate_session_name(session_name)?;
    Ok(vec![
        "drivemount".to_string(),
        "-s".to_string(),
        session_name.to_string(),
    ])
}

/// Checks whether `/content/drive/MyDrive` is mounted over the active `ControlMaster` socket.
/// Never runs `shutil.rmtree` if `/content/drive` is present in `/proc/mounts` (H1);
/// when unmounted, only removes an empty `/content/drive` or renames a non-empty stale dir.
pub async fn check_drive_mounted_mux(
    ctl_sock: &Path,
    runner: &dyn CommandRunner,
) -> Result<MountProbe, CoreError> {
    let script = concat!(
        "python3 - <<'PY'\n",
        "import os, time\n",
        "in_mounts = any(' /content/drive ' in l or ' /content/drive\\t' in l for l in open('/proc/mounts'))\n",
        "if in_mounts:\n",
        "    if os.path.isdir('/content/drive/MyDrive'):\n",
        "        print('DRIVE_PROBE=MOUNTED_HEALTHY')\n",
        "    else:\n",
        "        print('DRIVE_PROBE=MOUNTED_UNHEALTHY')\n",
        "else:\n",
        "    if os.path.exists('/content/drive'):\n",
        "        try:\n",
        "            os.rmdir('/content/drive')\n",
        "        except OSError:\n",
        "            stale = f'/content/drive.stale.{int(time.time())}'\n",
        "            os.rename('/content/drive', stale)\n",
        "            print(f'STALE_DRIVE_MOVED={stale}')\n",
        "    print('DRIVE_PROBE=NOT_MOUNTED')\n",
        "PY\n"
    );
    let spec = build_mux_exec_spec(
        ctl_sock,
        "bash -s",
        StdinPolicy::Bytes(script.as_bytes().to_vec()),
        DRIVE_CHECK_TIMEOUT,
    );
    let out = runner.run(spec, None).await?;
    Ok(parse_mount_probe(&out.stdout_raw))
}

pub fn parse_mount_probe(stdout: &str) -> MountProbe {
    for line in stdout.lines() {
        let t = line.trim();
        if t == "DRIVE_PROBE=MOUNTED_HEALTHY" || t == "DRIVE_MOUNTED=1" {
            return MountProbe::MountedHealthy;
        }
        if t == "DRIVE_PROBE=MOUNTED_UNHEALTHY" {
            return MountProbe::MountedUnhealthy;
        }
    }
    MountProbe::NotMounted
}

pub fn extract_drive_auth_url(line: &str) -> Option<String> {
    for token in line.split_whitespace() {
        let cleaned =
            token.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ')' | '(' | '<' | '>'));
        if cleaned.starts_with("https://accounts.google.com/")
            || cleaned.starts_with("https://colab.research.google.com/")
        {
            return Some(cleaned.to_string());
        }
    }
    None
}

pub fn is_drive_auth_prompt_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("google drive authorization needed")
        || lower.contains("unauthorized_redirect_uri")
        || lower.contains("press enter")
        || lower.contains("accounts.google.com/o/oauth2")
}

/// Extracts the workflow JSON with the highest `create_time` from ComfyUI `/history`.
pub fn extract_latest_workflow_from_history(json_str: &str) -> Option<(i64, serde_json::Value)> {
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let obj = v.as_object()?;
    let mut best: Option<(i64, serde_json::Value)> = None;
    for item in obj.values() {
        let Some(prompt_arr) = item.get("prompt").and_then(|p| p.as_array()) else {
            continue;
        };
        let Some(extra) = prompt_arr.get(3).and_then(|e| e.as_object()) else {
            continue;
        };
        let ts = extra
            .get("create_time")
            .and_then(|t| t.as_i64())
            .unwrap_or(0);
        let Some(wf) = extra
            .get("extra_pnginfo")
            .and_then(|p| p.get("workflow"))
            .filter(|w| w.is_object())
            .cloned()
        else {
            continue;
        };
        match &best {
            Some((best_ts, _)) if ts < *best_ts => {}
            _ => best = Some((ts, wf)),
        }
    }
    best
}

pub fn build_remote_export_script(include_outputs: bool, unmount: bool) -> String {
    let export_outputs_py = if include_outputs { "True" } else { "False" };
    let unmount_py = if unmount { "True" } else { "False" };
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

python3 - <<'PY'
import json
import os
import shutil
import subprocess
import urllib.parse
import urllib.request

EXPORT_OUTPUTS = {export_outputs_py}
UNMOUNT_DRIVE = {unmount_py}
COMFY_DIR = "/content/ComfyUI"
DRIVE_ROOT = "/content/drive/MyDrive/ComfyUI"
USER_DEFAULT_DIR = os.path.join(COMFY_DIR, "user", "default")
WORKFLOW_DIR = os.path.join(USER_DEFAULT_DIR, "workflows")
LATEST_WF_PATH = os.path.join(WORKFLOW_DIR, "latest_workflow.json")

mounted = os.path.isdir("/content/drive/MyDrive") and any(" /content/drive" in l for l in open("/proc/mounts"))
if not mounted:
    print("DRIVE_NOT_MOUNTED=1")
    raise SystemExit(1)

os.makedirs(WORKFLOW_DIR, exist_ok=True)
for sub in (
    "user_default/workflows",
    "output",
    "input",
    "models/checkpoints",
    "models/loras",
    "models/diffusion_models",
    "models/text_encoders",
    "models/vae",
    "models/controlnet",
    "models/upscale_models",
    "models/clip_vision",
):
    os.makedirs(os.path.join(DRIVE_ROOT, sub), exist_ok=True)

workflow_updated = False
try:
    with urllib.request.urlopen("http://127.0.0.1:8188/history", timeout=10) as resp:
        hist = json.loads(resp.read().decode("utf-8"))
    best_ts = -1
    best_wf = None
    for item in hist.values():
        prompt_arr = item.get("prompt") or []
        extra = prompt_arr[3] if len(prompt_arr) > 3 and isinstance(prompt_arr[3], dict) else {{}}
        ts = extra.get("create_time", 0)
        wf = (extra.get("extra_pnginfo") or {{}}).get("workflow")
        if isinstance(wf, dict) and ts >= best_ts:
            best_ts = ts
            best_wf = wf
    if best_wf is not None:
        with open(LATEST_WF_PATH, "w", encoding="utf-8") as f:
            json.dump(best_wf, f, indent=2, ensure_ascii=False)
        workflow_updated = True
except Exception as exc:
    print(f"[export] Warning: /history read skipped: {{exc}}")

def copy_tree(src_root, dst_root, skip_names=None):
    skip_names = skip_names or set()
    copied = 0
    if not os.path.isdir(src_root):
        return copied
    if os.path.realpath(src_root) == os.path.realpath(dst_root):
        return copied
    for dirpath, dirnames, filenames in os.walk(src_root):
        dirnames[:] = [d for d in dirnames if d not in skip_names]
        rel = os.path.relpath(dirpath, src_root)
        dst_dir = dst_root if rel == "." else os.path.join(dst_root, rel)
        for name in filenames:
            if name in skip_names:
                continue
            src_file = os.path.join(dirpath, name)
            dst_file = os.path.join(dst_dir, name)
            if os.path.realpath(src_file) == os.path.realpath(dst_file):
                continue
            os.makedirs(dst_dir, exist_ok=True)
            shutil.copy2(src_file, dst_file)
            copied += 1
    return copied

def strip_url_credentials(raw_url):
    try:
        parsed = urllib.parse.urlsplit(raw_url)
        if parsed.username is not None or parsed.password is not None:
            host = parsed.hostname or ""
            if parsed.port is not None:
                host = f"{{host}}:{{parsed.port}}"
            return urllib.parse.urlunsplit((parsed.scheme, host, parsed.path, parsed.query, parsed.fragment))
    except Exception:
        pass
    return raw_url

user_count = copy_tree(USER_DEFAULT_DIR, os.path.join(DRIVE_ROOT, "user_default"))
output_count = 0
if EXPORT_OUTPUTS:
    output_count = copy_tree(
        os.path.join(COMFY_DIR, "output"),
        os.path.join(DRIVE_ROOT, "output"),
        skip_names={{"_output_images_will_be_put_here"}},
    )
input_count = copy_tree(
    os.path.join(COMFY_DIR, "input"),
    os.path.join(DRIVE_ROOT, "input"),
    skip_names={{"example.png", "vintage_yellow_taxi.png"}},
)

repos = []
custom_nodes_dir = os.path.join(COMFY_DIR, "custom_nodes")
if os.path.isdir(custom_nodes_dir):
    for entry in sorted(os.listdir(custom_nodes_dir)):
        node_path = os.path.join(custom_nodes_dir, entry)
        if os.path.isdir(os.path.join(node_path, ".git")):
            url_res = subprocess.run(["git", "-C", node_path, "remote", "get-url", "origin"], capture_output=True, text=True)
            sha_res = subprocess.run(["git", "-C", node_path, "rev-parse", "HEAD"], capture_output=True, text=True)
            if url_res.returncode == 0 and url_res.stdout.strip():
                url = strip_url_credentials(url_res.stdout.strip())
                sha = sha_res.stdout.strip() if sha_res.returncode == 0 else ""
                repos.append(f"{{url}} {{sha}}".strip())

repos_file = os.path.join(DRIVE_ROOT, "custom_nodes_repos.txt")
with open(repos_file, "w", encoding="utf-8") as f:
    for r in repos:
        f.write(r + "\n")

flush_ok = False
unmounted = False
if UNMOUNT_DRIVE:
    try:
        from google.colab import drive
        drive.flush_and_unmount()
        flush_ok = True
        unmounted = True
    except Exception as exc:
        print(f"[export] Warning: flush_and_unmount failed: {{exc}}")
else:
    try:
        os.sync()
        flush_ok = True
    except Exception as exc:
        print(f"[export] Warning: os.sync failed: {{exc}}")

print(f"EXPORT_WORKFLOW_UPDATED={{1 if workflow_updated else 0}}")
print(f"EXPORT_USER_FILES={{user_count}}")
print(f"EXPORT_OUTPUT_FILES={{output_count}}")
print(f"EXPORT_INPUT_FILES={{input_count}}")
print(f"EXPORT_CUSTOM_NODES={{len(repos)}}")
print(f"FLUSH_OK={{1 if flush_ok else 0}}")
print(f"EXPORT_UNMOUNTED={{1 if unmounted else 0}}")
print("EXPORT_OK=1")
PY
"#
    )
}

pub fn parse_export_output(stdout: &str) -> DriveExportReport {
    let mut report = DriveExportReport {
        ok: false,
        flush_ok: false,
        unmounted: false,
        workflow_updated: false,
        user_files: 0,
        output_files: 0,
        input_files: 0,
        custom_nodes_count: 0,
        summary: String::new(),
    };
    for line in stdout.lines() {
        let t = line.trim();
        if t == "EXPORT_OK=1" {
            report.ok = true;
        } else if t == "FLUSH_OK=1" {
            report.flush_ok = true;
        } else if t == "EXPORT_UNMOUNTED=1" {
            report.unmounted = true;
        } else if t == "EXPORT_WORKFLOW_UPDATED=1" {
            report.workflow_updated = true;
        } else if let Some(v) = t.strip_prefix("EXPORT_USER_FILES=") {
            report.user_files = v.parse().unwrap_or(0);
        } else if let Some(v) = t.strip_prefix("EXPORT_OUTPUT_FILES=") {
            report.output_files = v.parse().unwrap_or(0);
        } else if let Some(v) = t.strip_prefix("EXPORT_INPUT_FILES=") {
            report.input_files = v.parse().unwrap_or(0);
        } else if let Some(v) = t.strip_prefix("EXPORT_CUSTOM_NODES=") {
            report.custom_nodes_count = v.parse().unwrap_or(0);
        }
    }
    report.summary = if report.ok && report.flush_ok {
        format!(
            "已导出到 Drive 并完成刷盘（工作流更新={}，nodes={}，user={}，output={}，input={}，卸载={}）。",
            report.workflow_updated,
            report.custom_nodes_count,
            report.user_files,
            report.output_files,
            report.input_files,
            report.unmounted
        )
    } else if report.ok {
        "已写入 Drive 目录，但刷盘未确认完成（Drive 同步未确认）。".to_string()
    } else {
        "Drive 导出未完成（Drive 同步未确认）。".to_string()
    };
    report
}

pub async fn run_remote_export_and_flush(
    ctl_sock: &Path,
    include_outputs: bool,
    unmount: bool,
    timeout: Duration,
    runner: &dyn CommandRunner,
) -> Result<DriveExportReport, CoreError> {
    let script = build_remote_export_script(include_outputs, unmount);
    let spec = build_mux_exec_spec(
        ctl_sock,
        "bash -s",
        StdinPolicy::Bytes(script.into_bytes()),
        timeout,
    );
    let out = runner.run(spec, None).await?;
    Ok(parse_export_output(&out.stdout_raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_latest_workflow_by_create_time() {
        let hist = r#"{
            "old": {
                "prompt": [1, "id1", {}, {"create_time": 100, "extra_pnginfo": {"workflow": {"version": 1}}}]
            },
            "new": {
                "prompt": [2, "id2", {}, {"create_time": 250, "extra_pnginfo": {"workflow": {"version": 2}}}]
            },
            "no_wf": {
                "prompt": [3, "id3", {}, {"create_time": 999}]
            }
        }"#;
        let (ts, wf) = extract_latest_workflow_from_history(hist).unwrap();
        assert_eq!(ts, 250);
        assert_eq!(wf["version"], 2);
    }

    #[test]
    fn parses_drive_auth_url_and_prompt_lines_before_redaction() {
        let line = "REQUIRED: Google Drive Authorization needed. Visit https://accounts.google.com/o/oauth2/auth?client_id=123 and press Enter.";
        let parsed = parse_drivemount_line(line);
        assert!(parsed.is_auth_prompt);
        assert_eq!(
            parsed.auth_url.as_deref(),
            Some("https://accounts.google.com/o/oauth2/auth?client_id=123")
        );
    }

    #[test]
    fn parses_mount_probe_three_states() {
        assert_eq!(
            parse_mount_probe("DRIVE_PROBE=MOUNTED_HEALTHY\n"),
            MountProbe::MountedHealthy
        );
        assert_eq!(
            parse_mount_probe("DRIVE_PROBE=MOUNTED_UNHEALTHY\n"),
            MountProbe::MountedUnhealthy
        );
        assert_eq!(
            parse_mount_probe("DRIVE_PROBE=NOT_MOUNTED\n"),
            MountProbe::NotMounted
        );
    }

    #[test]
    fn parses_export_and_flush_sentinels() {
        let ok_out = "EXPORT_WORKFLOW_UPDATED=1\nEXPORT_USER_FILES=3\nEXPORT_OUTPUT_FILES=2\nEXPORT_INPUT_FILES=1\nEXPORT_CUSTOM_NODES=4\nFLUSH_OK=1\nEXPORT_UNMOUNTED=1\nEXPORT_OK=1\n";
        let r1 = parse_export_output(ok_out);
        assert!(r1.ok && r1.flush_ok && r1.unmounted && r1.workflow_updated);
        assert_eq!(r1.custom_nodes_count, 4);

        let no_flush = "EXPORT_CUSTOM_NODES=2\nFLUSH_OK=0\nEXPORT_OK=1\n";
        let r2 = parse_export_output(no_flush);
        assert!(r2.ok && !r2.flush_ok);
        assert!(r2.summary.contains("Drive 同步未确认"));
    }

    #[test]
    fn manual_export_script_keeps_drive_mounted_and_strips_git_credentials() {
        let s_manual = build_remote_export_script(false, false);
        assert!(s_manual.contains("EXPORT_OUTPUTS = False"));
        assert!(s_manual.contains("UNMOUNT_DRIVE = False"));
        assert!(s_manual.contains("strip_url_credentials"));
        assert!(s_manual.contains("os.sync()"));

        let s_stop = build_remote_export_script(true, true);
        assert!(s_stop.contains("UNMOUNT_DRIVE = True"));
        assert!(s_stop.contains("drive.flush_and_unmount()"));
    }
}
