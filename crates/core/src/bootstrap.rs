use std::path::Path;
use std::time::Duration;

use crate::runner::{CommandOutput, CommandRunner, LineCallback, StdinPolicy};
use crate::ssh_tunnel::build_mux_exec_spec;
use crate::CoreError;

pub const COMFY_READY_SENTINEL: &str = "COMFY_READY=1";
pub const CORS_HEADER_BUSY_SENTINEL: &str = "CORS_HEADER_ACTIVE_QUEUE_BUSY=1";
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapOutcome {
    pub cors_header_busy_warning: bool,
}

/// Generates the Phase 1 remote ComfyUI setup & launch bash script.
///
/// Security invariants:
/// - Preserves Colab's preinstalled CUDA PyTorch (`torch/torchvision/torchaudio`).
/// - Never launches `main.py` with `--enable-cors-header`.
/// - During hot-takeover, checks `/proc/[0-9]*/cmdline` for `--enable-cors-header`
///   and automatically restarts ComfyUI if `/queue` is idle.
pub fn build_remote_setup_script(comfy_port: u16) -> String {
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

COMFY_PORT="{comfy_port}"
COMFY_DIR="/content/ComfyUI"
MANAGER_DIR="${{COMFY_DIR}}/custom_nodes/ComfyUI-Manager"

echo "=== [bootstrap] Hardware & Disk ==="
nvidia-smi --query-gpu=name,memory.total --format=csv,noheader 2>/dev/null || echo "CPU"
df -h /content | tail -n 1

if ! command -v aria2c >/dev/null 2>&1; then
  echo "=== [bootstrap] Installing aria2 ==="
  apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq aria2 >/dev/null
fi

if [ ! -d "${{COMFY_DIR}}/.git" ]; then
  echo "=== [bootstrap] Cloning ComfyUI ==="
  git clone --depth 1 https://github.com/comfyanonymous/ComfyUI.git "${{COMFY_DIR}}"
else
  echo "=== [bootstrap] ComfyUI already present ==="
fi

if [ ! -d "${{MANAGER_DIR}}/.git" ]; then
  echo "=== [bootstrap] Cloning ComfyUI-Manager ==="
  mkdir -p "${{COMFY_DIR}}/custom_nodes"
  git clone --depth 1 https://github.com/Comfy-Org/ComfyUI-Manager.git "${{MANAGER_DIR}}"
else
  echo "=== [bootstrap] ComfyUI-Manager already present ==="
fi

pip_install_req() {{
  local req_file="$1"
  if [ ! -s "${{req_file}}" ]; then
    return 0
  fi
  if command -v uv >/dev/null 2>&1; then
    uv pip install --system -q -r "${{req_file}}"
  else
    python3 -m pip install -q -r "${{req_file}}"
  fi
}}

echo "=== [bootstrap] Installing Python dependencies ==="
if python3 -c "import torch; assert torch.cuda.is_available()" >/dev/null 2>&1; then
  grep -Evi '^(torch|torchvision|torchaudio)($|[=<>!~ ;[])' "${{COMFY_DIR}}/requirements.txt" > /tmp/comfy_reqs.txt || true
else
  cp "${{COMFY_DIR}}/requirements.txt" /tmp/comfy_reqs.txt
fi
pip_install_req /tmp/comfy_reqs.txt
if [ -f "${{MANAGER_DIR}}/requirements.txt" ]; then
  pip_install_req "${{MANAGER_DIR}}/requirements.txt"
fi

need_start=true
if curl -sf "http://127.0.0.1:${{COMFY_PORT}}/system_stats" >/dev/null 2>&1; then
  echo "=== [bootstrap] Existing ComfyUI detected; auditing launch flags ==="
  takeover_state="$(python3 - "${{COMFY_PORT}}" <<'PY'
import glob, json, os, signal, sys, urllib.request

port = sys.argv[1]
unsafe_pids = []
for cmd_path in glob.glob("/proc/[0-9]*/cmdline"):
    try:
        with open(cmd_path, "rb") as f:
            raw = f.read().decode("utf-8", errors="replace")
    except OSError:
        continue
    args = [a for a in raw.split("\x00") if a]
    if any("main.py" in a for a in args) and "--enable-cors-header" in args:
        pid = int(cmd_path.split("/")[2])
        unsafe_pids.append(pid)

if not unsafe_pids:
    print("SAFE_RUNNING")
    sys.exit(0)

queue_empty = False
try:
    with urllib.request.urlopen(f"http://127.0.0.1:{{port}}/queue", timeout=5) as resp:
        q = json.loads(resp.read().decode("utf-8"))
        queue_empty = not q.get("queue_running") and not q.get("queue_pending")
except Exception:
    queue_empty = False

if not queue_empty:
    print("UNSAFE_BUSY")
    sys.exit(0)

for pid in unsafe_pids:
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError:
        pass
print("RESTART_CLEAN")
PY
)"
  if [ "${{takeover_state}}" = "SAFE_RUNNING" ]; then
    need_start=false
  elif [ "${{takeover_state}}" = "UNSAFE_BUSY" ]; then
    echo "{CORS_HEADER_BUSY_SENTINEL}"
    need_start=false
  elif [ "${{takeover_state}}" = "RESTART_CLEAN" ]; then
    echo "=== [bootstrap] Restarting ComfyUI without --enable-cors-header ==="
    for _ in $(seq 1 20); do
      if ! curl -sf "http://127.0.0.1:${{COMFY_PORT}}/system_stats" >/dev/null 2>&1; then
        break
      fi
      sleep 1
    done
    need_start=true
  fi
fi

if [ "${{need_start}}" = true ]; then
  echo "=== [bootstrap] Starting ComfyUI on 127.0.0.1:${{COMFY_PORT}} ==="
  cd "${{COMFY_DIR}}"
  nohup python3 main.py --listen 127.0.0.1 --port "${{COMFY_PORT}}" >/content/comfyui.log 2>&1 </dev/null &
  disown || true

  ready=false
  for _ in $(seq 1 90); do
    if curl -sf "http://127.0.0.1:${{COMFY_PORT}}/system_stats" >/dev/null 2>&1; then
      ready=true
      break
    fi
    sleep 2
  done

  if [ "${{ready}}" != true ]; then
    echo "[bootstrap] Error: ComfyUI failed to become ready within 180s. Last 60 log lines:" >&2
    tail -n 60 /content/comfyui.log >&2 || true
    exit 1
  fi
fi

echo "{COMFY_READY_SENTINEL}"
"#
    )
}

pub fn parse_bootstrap_output(out: &CommandOutput) -> Result<BootstrapOutcome, CoreError> {
    let mut ready = false;
    let mut cors_header_busy_warning = false;
    for line in out.stdout_raw.lines() {
        let trimmed = line.trim();
        if trimmed == COMFY_READY_SENTINEL {
            ready = true;
        } else if trimmed == CORS_HEADER_BUSY_SENTINEL {
            cors_header_busy_warning = true;
        }
    }
    if !ready {
        return Err(CoreError::BootstrapFailed {
            detail: if out.stderr.is_empty() {
                format!("missing {COMFY_READY_SENTINEL} sentinel")
            } else {
                out.stderr.clone()
            },
        });
    }
    Ok(BootstrapOutcome {
        cors_header_busy_warning,
    })
}

pub async fn run_remote_bootstrap(
    ctl_sock: &Path,
    comfy_port: u16,
    runner: &dyn CommandRunner,
    on_line: Option<LineCallback>,
) -> Result<BootstrapOutcome, CoreError> {
    let script = build_remote_setup_script(comfy_port);
    let spec = build_mux_exec_spec(
        ctl_sock,
        "bash -s",
        StdinPolicy::Bytes(script.into_bytes()),
        BOOTSTRAP_TIMEOUT,
    );
    let out = runner.run(spec, on_line).await?;
    parse_bootstrap_output(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_script_never_launches_with_cors_header_and_audits_takeover() {
        let script = build_remote_setup_script(8188);
        assert!(script.contains("nohup python3 main.py --listen 127.0.0.1 --port \"${COMFY_PORT}\" >/content/comfyui.log"));
        for line in script.lines() {
            if line.contains("main.py --listen") {
                assert!(
                    !line.contains("--enable-cors-header"),
                    "launch line must never pass --enable-cors-header: {line}"
                );
            }
        }
        assert!(script.contains("--enable-cors-header"));
        assert!(script.contains("/queue"));
        assert!(script.contains(COMFY_READY_SENTINEL));
    }

    #[test]
    fn parse_bootstrap_requires_sentinel_even_on_exit_zero() {
        let ok = CommandOutput::from_raw(
            Some(0),
            format!("=== [bootstrap] ===\n{COMFY_READY_SENTINEL}\n"),
            String::new(),
        );
        assert_eq!(
            parse_bootstrap_output(&ok).unwrap(),
            BootstrapOutcome {
                cors_header_busy_warning: false
            }
        );

        let busy = CommandOutput::from_raw(
            Some(0),
            format!("{CORS_HEADER_BUSY_SENTINEL}\n{COMFY_READY_SENTINEL}\n"),
            String::new(),
        );
        assert_eq!(
            parse_bootstrap_output(&busy).unwrap(),
            BootstrapOutcome {
                cors_header_busy_warning: true
            }
        );

        let fake_zero = CommandOutput::from_raw(
            Some(0),
            "some random stdout without sentinel".to_string(),
            String::new(),
        );
        assert!(matches!(
            parse_bootstrap_output(&fake_zero),
            Err(CoreError::BootstrapFailed { .. })
        ));
    }
}
