use std::path::Path;
use std::time::Duration;

use crate::runner::{CommandOutput, CommandRunner, LineCallback, StdinPolicy};
use crate::ssh_tunnel::build_mux_exec_spec;
use crate::CoreError;

pub const COMFY_READY_SENTINEL: &str = "COMFY_READY=1";
pub const CORS_HEADER_BUSY_SENTINEL: &str = "CORS_HEADER_ACTIVE_QUEUE_BUSY=1";
pub const LAYOUT_CHANGED_BUSY_SENTINEL: &str = "PERSISTENCE_LAYOUT_CHANGED_BUSY=1";
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapOutcome {
    pub cors_header_busy_warning: bool,
    pub layout_changed_busy_warning: bool,
}

/// Generates the Phase 2 remote ComfyUI setup & Drive persistence bash script.
///
/// Security & stability invariants:
/// - Freezes Colab's preinstalled core packages (`torch`, `torchvision`, `torchaudio`,
///   `numpy`, `transformers`, `pillow`, `safetensors`) into `/tmp/colab_constraints.txt`
///   and passes `-c /tmp/colab_constraints.txt` to every `pip` / `uv pip` install.
/// - Never launches `main.py` with `--enable-cors-header`.
/// - When `use_drive` is true and `/content/drive` is mounted, verifies file count & total bytes
///   before removing local directories when linking to Drive, and supports `<url> <commit_sha>`
///   in `custom_nodes_repos.txt`.
/// - When `use_drive` is false (or unmounted), cleans dangling symlinks and `extra_model_paths.yaml`
///   and restarts an idle ComfyUI instance if persistence layout changed.
pub fn build_remote_setup_script(comfy_port: u16, use_drive: bool) -> String {
    let use_drive_flag = if use_drive { "true" } else { "false" };
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

COMFY_PORT="{comfy_port}"
USE_DRIVE="{use_drive_flag}"
COMFY_DIR="/content/ComfyUI"
DRIVE_MOUNT_PATH="/content/drive/MyDrive"
DRIVE_ROOT="${{DRIVE_MOUNT_PATH}}/ComfyUI"
MANAGER_DIR="${{COMFY_DIR}}/custom_nodes/ComfyUI-Manager"

DRIVE_MOUNTED=false
if [ "${{USE_DRIVE}}" = true ] && [ -d "${{DRIVE_MOUNT_PATH}}" ] && grep -qs ' /content/drive' /proc/mounts; then
  DRIVE_MOUNTED=true
fi

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

# Freeze Colab preinstalled core packages to prevent custom nodes from downgrading CUDA torch/numpy
python3 - <<'PY' > /tmp/colab_constraints.txt
import importlib.metadata as meta

pkgs = ("torch", "torchvision", "torchaudio", "numpy", "transformers", "pillow", "safetensors")
for p in pkgs:
    try:
        v = meta.version(p)
        print(f"{{p}}=={{v}}")
    except meta.PackageNotFoundError:
        pass
PY

pip_install_req() {{
  local req_file="$1"
  if [ ! -s "${{req_file}}" ]; then
    return 0
  fi
  local filtered="/tmp/req_filtered_$$.txt"
  grep -Evi '^(torch|torchvision|torchaudio)($|[=<>!~ ;[])' "${{req_file}}" > "${{filtered}}" || true
  if [ ! -s "${{filtered}}" ]; then
    rm -f "${{filtered}}"
    return 0
  fi
  if command -v uv >/dev/null 2>&1; then
    uv pip install --system -q -c /tmp/colab_constraints.txt -r "${{filtered}}"
  else
    python3 -m pip install -q -c /tmp/colab_constraints.txt -r "${{filtered}}"
  fi
  rm -f "${{filtered}}"
}}

layout_changed=0

link_dir_to_drive() {{
  local local_dir="$1"
  local drive_dir="$2"
  mkdir -p "${{drive_dir}}"
  if [ -L "${{local_dir}}" ]; then
    local target
    target="$(readlink -f "${{local_dir}}" 2>/dev/null || true)"
    local drive_real
    drive_real="$(readlink -f "${{drive_dir}}" 2>/dev/null || echo "${{drive_dir}}")"
    if [ "${{target}}" = "${{drive_real}}" ]; then
      return 0
    fi
    rm -f "${{local_dir}}"
    layout_changed=1
  elif [ -d "${{local_dir}}" ]; then
    python3 - "${{local_dir}}" "${{drive_dir}}" <<'PY'
import os, shutil, sys

src_root, dst_root = sys.argv[1], sys.argv[2]
src_count = 0
src_bytes = 0
for dirpath, _, filenames in os.walk(src_root):
    rel = os.path.relpath(dirpath, src_root)
    dst_dir = dst_root if rel == "." else os.path.join(dst_root, rel)
    os.makedirs(dst_dir, exist_ok=True)
    for name in filenames:
        src_file = os.path.join(dirpath, name)
        dst_file = os.path.join(dst_dir, name)
        src_count += 1
        src_bytes += os.path.getsize(src_file)
        if not os.path.exists(dst_file):
            shutil.copy2(src_file, dst_file)

# Verify every source file exists on destination with matching size before removing source
verified_count = 0
verified_bytes = 0
for dirpath, _, filenames in os.walk(src_root):
    rel = os.path.relpath(dirpath, src_root)
    dst_dir = dst_root if rel == "." else os.path.join(dst_root, rel)
    for name in filenames:
        dst_file = os.path.join(dst_dir, name)
        if not os.path.isfile(dst_file):
            raise SystemExit(f"Drive copy verification failed: missing {{dst_file}}")
        verified_count += 1
        verified_bytes += os.path.getsize(dst_file)

if verified_count != src_count or verified_bytes < src_bytes:
    raise SystemExit(f"Drive copy verification mismatch: count={{verified_count}}/{{src_count}} bytes={{verified_bytes}}/{{src_bytes}}")
shutil.rmtree(src_root)
PY
    layout_changed=1
  fi
  mkdir -p "$(dirname "${{local_dir}}")"
  ln -s "${{drive_dir}}" "${{local_dir}}"
}}

if [ "${{DRIVE_MOUNTED}}" = true ]; then
  echo "=== [bootstrap] Configuring Google Drive persistence (${{DRIVE_ROOT}}) ==="
  mkdir -p \
    "${{DRIVE_ROOT}}/user_default/workflows" \
    "${{DRIVE_ROOT}}/output" \
    "${{DRIVE_ROOT}}/input" \
    "${{DRIVE_ROOT}}/models/checkpoints" \
    "${{DRIVE_ROOT}}/models/loras" \
    "${{DRIVE_ROOT}}/models/diffusion_models" \
    "${{DRIVE_ROOT}}/models/text_encoders" \
    "${{DRIVE_ROOT}}/models/vae" \
    "${{DRIVE_ROOT}}/models/controlnet" \
    "${{DRIVE_ROOT}}/models/upscale_models" \
    "${{DRIVE_ROOT}}/models/clip_vision"

  link_dir_to_drive "${{COMFY_DIR}}/user/default" "${{DRIVE_ROOT}}/user_default"
  link_dir_to_drive "${{COMFY_DIR}}/output" "${{DRIVE_ROOT}}/output"
  link_dir_to_drive "${{COMFY_DIR}}/input" "${{DRIVE_ROOT}}/input"

  cat > /tmp/extra_model_paths.yaml.new <<YAML
colab_drive:
  base_path: ${{DRIVE_ROOT}}/models
  checkpoints: checkpoints/
  loras: loras/
  diffusion_models: diffusion_models/
  text_encoders: text_encoders/
  vae: vae/
  controlnet: controlnet/
  upscale_models: upscale_models/
  clip_vision: clip_vision/
YAML
  if [ ! -f "${{COMFY_DIR}}/extra_model_paths.yaml" ] || ! cmp -s /tmp/extra_model_paths.yaml.new "${{COMFY_DIR}}/extra_model_paths.yaml"; then
    mv /tmp/extra_model_paths.yaml.new "${{COMFY_DIR}}/extra_model_paths.yaml"
    layout_changed=1
  else
    rm -f /tmp/extra_model_paths.yaml.new
  fi

  if [ -s "${{DRIVE_ROOT}}/custom_nodes_repos.txt" ]; then
    echo "=== [bootstrap] Restoring custom nodes from Drive ==="
    while IFS= read -r line || [ -n "${{line}}" ]; do
      line="$(echo "${{line}}" | xargs)"
      [ -z "${{line}}" ] && continue
      [[ "${{line}}" == \#* ]] && continue
      read -r repo_url commit_sha _ <<<"${{line}}"
      [ -z "${{repo_url}}" ] && continue
      repo_name="$(basename "${{repo_url}}" .git)"
      node_dir="${{COMFY_DIR}}/custom_nodes/${{repo_name}}"
      if [ ! -d "${{node_dir}}/.git" ]; then
        echo "[custom_nodes] Cloning ${{repo_name}} (${{repo_url}})"
        git clone --depth 1 "${{repo_url}}" "${{node_dir}}"
        if [ -n "${{commit_sha:-}}" ]; then
          git -C "${{node_dir}}" fetch --depth 1 origin "${{commit_sha}}" >/dev/null 2>&1 || true
          git -C "${{node_dir}}" checkout "${{commit_sha}}" >/dev/null 2>&1 || true
        fi
      fi
      if [ -f "${{node_dir}}/requirements.txt" ]; then
        pip_install_req "${{node_dir}}/requirements.txt"
      fi
    done < "${{DRIVE_ROOT}}/custom_nodes_repos.txt"
  fi
else
  echo "=== [bootstrap] --no-drive mode: cleaning Drive symlinks & extra_model_paths.yaml ==="
  for d in "${{COMFY_DIR}}/user/default" "${{COMFY_DIR}}/output" "${{COMFY_DIR}}/input"; do
    if [ -L "${{d}}" ]; then
      rm -f "${{d}}"
      mkdir -p "${{d}}"
      layout_changed=1
    elif [ ! -d "${{d}}" ]; then
      mkdir -p "${{d}}"
    fi
  done
  if [ -f "${{COMFY_DIR}}/extra_model_paths.yaml" ]; then
    rm -f "${{COMFY_DIR}}/extra_model_paths.yaml"
    layout_changed=1
  fi
fi

need_start=true
takeover_state="NONE"
if curl -sf "http://127.0.0.1:${{COMFY_PORT}}/system_stats" >/dev/null 2>&1; then
  echo "=== [bootstrap] Existing ComfyUI detected; auditing launch flags ==="
  takeover_state="$(python3 - "${{COMFY_PORT}}" "${{layout_changed}}" <<'PY'
import glob, json, os, sys, urllib.request

port = int(sys.argv[1])
layout_changed = int(sys.argv[2]) == 1
port_hex = f":{{port:04X}}"
listen_inodes = set()
for proc_net in ("/proc/net/tcp", "/proc/net/tcp6"):
    try:
        with open(proc_net, "r", encoding="utf-8", errors="replace") as f:
            next(f, None)
            for line in f:
                cols = line.split()
                if len(cols) >= 10 and cols[3] == "0A" and cols[1].upper().endswith(port_hex):
                    listen_inodes.add(f"socket:[{{cols[9]}}]")
    except OSError:
        continue

unsafe_pids = []
for pid_dir in glob.glob("/proc/[0-9]*"):
    try:
        int(os.path.basename(pid_dir))
    except ValueError:
        continue
    if listen_inodes:
        owns_port = False
        for fd_path in glob.glob(f"{{pid_dir}}/fd/*"):
            try:
                if os.readlink(fd_path) in listen_inodes:
                    owns_port = True
                    break
            except OSError:
                continue
        if not owns_port:
            continue
    try:
        with open(f"{{pid_dir}}/cmdline", "rb") as f:
            raw = f.read().decode("utf-8", errors="replace")
    except OSError:
        continue
    args = [a for a in raw.split("\x00") if a]
    if any("main.py" in a for a in args) and "--enable-cors-header" in args:
        unsafe_pids.append(pid_dir)

if not unsafe_pids and not layout_changed:
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
    if unsafe_pids:
        print("UNSAFE_BUSY")
    else:
        print("LAYOUT_BUSY")
    sys.exit(0)

print("NEEDS_RESTART")
PY
)"
  if [ "${{takeover_state}}" = "SAFE_RUNNING" ]; then
    need_start=false
  elif [ "${{takeover_state}}" = "UNSAFE_BUSY" ]; then
    echo "{CORS_HEADER_BUSY_SENTINEL}"
    need_start=false
  elif [ "${{takeover_state}}" = "LAYOUT_BUSY" ]; then
    echo "{LAYOUT_CHANGED_BUSY_SENTINEL}"
    need_start=false
  elif [ "${{takeover_state}}" = "NEEDS_RESTART" ]; then
    need_start=true
  fi
fi

if [ "${{need_start}}" = true ]; then
  echo "=== [bootstrap] Installing Python dependencies ==="
  cp "${{COMFY_DIR}}/requirements.txt" /tmp/comfy_reqs.txt
  pip_install_req /tmp/comfy_reqs.txt
  if [ -f "${{MANAGER_DIR}}/requirements.txt" ]; then
    pip_install_req "${{MANAGER_DIR}}/requirements.txt"
  fi

  launch_new=true
  if [ "${{takeover_state}}" = "NEEDS_RESTART" ] && curl -sf "http://127.0.0.1:${{COMFY_PORT}}/system_stats" >/dev/null 2>&1; then
    recheck_state="$(python3 - "${{COMFY_PORT}}" "${{layout_changed}}" <<'PY'
import glob, json, os, signal, sys, urllib.request

port = int(sys.argv[1])
layout_changed = int(sys.argv[2]) == 1
port_hex = f":{{port:04X}}"
listen_inodes = set()
for proc_net in ("/proc/net/tcp", "/proc/net/tcp6"):
    try:
        with open(proc_net, "r", encoding="utf-8", errors="replace") as f:
            next(f, None)
            for line in f:
                cols = line.split()
                if len(cols) >= 10 and cols[3] == "0A" and cols[1].upper().endswith(port_hex):
                    listen_inodes.add(f"socket:[{{cols[9]}}]")
    except OSError:
        continue

all_comfy_pids = []
unsafe_pids = []
for pid_dir in glob.glob("/proc/[0-9]*"):
    try:
        pid = int(os.path.basename(pid_dir))
    except ValueError:
        continue
    if listen_inodes:
        owns_port = False
        for fd_path in glob.glob(f"{{pid_dir}}/fd/*"):
            try:
                if os.readlink(fd_path) in listen_inodes:
                    owns_port = True
                    break
            except OSError:
                continue
        if not owns_port:
            continue
    try:
        with open(f"{{pid_dir}}/cmdline", "rb") as f:
            raw = f.read().decode("utf-8", errors="replace")
    except OSError:
        continue
    args = [a for a in raw.split("\x00") if a]
    if any("main.py" in a for a in args):
        all_comfy_pids.append(pid)
        if "--enable-cors-header" in args:
            unsafe_pids.append(pid)

target_pids = unsafe_pids if unsafe_pids else (all_comfy_pids if layout_changed else [])
if not target_pids:
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
    print("BECAME_BUSY_UNSAFE" if unsafe_pids else "BECAME_BUSY_LAYOUT")
    sys.exit(0)

for pid in target_pids:
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError:
        pass
print("STOP_SENT")
PY
)"
    if [ "${{recheck_state}}" = "SAFE_RUNNING" ]; then
      launch_new=false
    elif [ "${{recheck_state}}" = "BECAME_BUSY_UNSAFE" ]; then
      echo "{CORS_HEADER_BUSY_SENTINEL}"
      launch_new=false
    elif [ "${{recheck_state}}" = "BECAME_BUSY_LAYOUT" ]; then
      echo "{LAYOUT_CHANGED_BUSY_SENTINEL}"
      launch_new=false
    elif [ "${{recheck_state}}" = "STOP_SENT" ]; then
      echo "=== [bootstrap] Restarting ComfyUI cleanly ==="
      stopped=false
      for _ in $(seq 1 20); do
        if ! curl -sf "http://127.0.0.1:${{COMFY_PORT}}/system_stats" >/dev/null 2>&1; then
          stopped=true
          break
        fi
        sleep 1
      done
      if [ "${{stopped}}" != true ]; then
        echo "[bootstrap] Error: existing ComfyUI did not stop within 20s" >&2
        exit 1
      fi
    fi
  fi

  if [ "${{launch_new}}" = true ]; then
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
fi

echo "{COMFY_READY_SENTINEL}"
"#
    )
}

pub fn parse_bootstrap_output(out: &CommandOutput) -> Result<BootstrapOutcome, CoreError> {
    let mut ready = false;
    let mut cors_header_busy_warning = false;
    let mut layout_changed_busy_warning = false;
    for line in out.stdout_raw.lines() {
        let trimmed = line.trim();
        if trimmed == COMFY_READY_SENTINEL {
            ready = true;
        } else if trimmed == CORS_HEADER_BUSY_SENTINEL {
            cors_header_busy_warning = true;
        } else if trimmed == LAYOUT_CHANGED_BUSY_SENTINEL {
            layout_changed_busy_warning = true;
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
        layout_changed_busy_warning,
    })
}

pub async fn run_remote_bootstrap(
    ctl_sock: &Path,
    comfy_port: u16,
    use_drive: bool,
    runner: &dyn CommandRunner,
    on_line: Option<LineCallback>,
) -> Result<BootstrapOutcome, CoreError> {
    let script = build_remote_setup_script(comfy_port, use_drive);
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
        let script = build_remote_setup_script(8188, true);
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
        assert!(script.contains("/proc/net/tcp"));
        assert!(script.contains("/queue"));
        assert!(script.contains("/tmp/colab_constraints.txt"));
        assert!(script.contains("-c /tmp/colab_constraints.txt"));
        assert!(script.contains(COMFY_READY_SENTINEL));
        let start_if_pos = script
            .find("if [ \"${need_start}\" = true ]; then")
            .unwrap();
        let pip_pos = script.find("pip_install_req /tmp/comfy_reqs.txt").unwrap();
        assert!(
            pip_pos > start_if_pos,
            "pip_install_req must only run when starting ComfyUI"
        );
    }

    #[test]
    fn bootstrap_script_installs_deps_before_terminating_unsafe_comfy() {
        let script = build_remote_setup_script(8188, true);
        let pip_comfy_pos = script.find("pip_install_req /tmp/comfy_reqs.txt").unwrap();
        let pip_mgr_pos = script
            .find("pip_install_req \"${MANAGER_DIR}/requirements.txt\"")
            .unwrap();
        let kill_pos = script.find("os.kill(pid, signal.SIGTERM)").unwrap();
        assert!(
            pip_comfy_pos < kill_pos && pip_mgr_pos < kill_pos,
            "pip_install_req must complete before sending SIGTERM to existing ComfyUI"
        );
        assert_eq!(
            script.matches("os.kill(pid, signal.SIGTERM)").count(),
            1,
            "SIGTERM should only appear in the post-install recheck block"
        );
        assert!(script.contains("NEEDS_RESTART"));
        assert!(script.contains("BECAME_BUSY_UNSAFE"));
    }

    #[test]
    fn bootstrap_script_cleans_dangling_symlinks_and_yaml_in_no_drive_mode() {
        let script = build_remote_setup_script(8188, false);
        assert!(script.contains("USE_DRIVE=\"false\""));
        assert!(script.contains("extra_model_paths.yaml"));
        assert!(script.contains("if [ -L \"${d}\" ]; then"));
        assert!(script.contains("verified_count != src_count or verified_bytes < src_bytes"));
        assert!(script.contains("git -C \"${node_dir}\" checkout \"${commit_sha}\""));
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
                cors_header_busy_warning: false,
                layout_changed_busy_warning: false,
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
                cors_header_busy_warning: true,
                layout_changed_busy_warning: false,
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
