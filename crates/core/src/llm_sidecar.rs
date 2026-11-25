use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};

use crate::config::{posix_single_quote, GpuTier, REMOTE_LLM_PORT};
use crate::locate::SSH_BIN;
use crate::runner::{CommandRunner, CommandSpec, LineCallback, StdinPolicy};
use crate::ssh_tunnel::build_mux_exec_spec;
use crate::CoreError;

pub const LLAMA_CPP_RELEASE_TAG: &str = "b12722";
pub const LLAMA_CPP_CUDA12_URL: &str = "https://github.com/ggml-org/llama.cpp/releases/download/b12722/llama-b12722-bin-ubuntu-cuda-12.8-x64.tar.gz";
pub const LLAMA_CPP_CUDA12_SHA256: &str =
    "98bd142911d8efa1da39819e74bc9443d99c8e0d7125fde435b75e26c253fee3";

pub const MIN_NVIDIA_DRIVER_MAJOR: u32 = 525;
pub const MIN_GLIBC_VERSION: (u32, u32) = (2, 31);
const COMPACT_VRAM_LIMIT_BYTES: u64 = 26 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SidecarModelPreset {
    pub id: String,
    pub name: String,
    pub description_zh: String,
    pub description_en: String,
    pub min_vram_gb: u32,
    pub total_bytes: u64,
    pub model_filename: String,
    pub model_url: String,
    pub model_size_bytes: u64,
    pub mmproj_filename: String,
    pub mmproj_url: String,
    pub mmproj_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VmSidecarProbe {
    pub driver_version: Option<String>,
    pub driver_major: Option<u32>,
    pub glibc_version: Option<(u32, u32)>,
    pub gpu_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum InterrogateStyle {
    FluxNatural,
    ComfyTags,
    WanVideoMotion,
}

pub fn builtin_sidecar_models() -> Vec<SidecarModelPreset> {
    vec![
        SidecarModelPreset {
            id: "qwen2.5-vl-3b-q4".to_string(),
            name: "Qwen2.5-VL 3B Instruct (Q4_K_M)".to_string(),
            description_zh: "轻量高速看图反推（约 2.8 GB，适合 T4/L4 与快速提示词提取）".to_string(),
            description_en: "Fast lightweight vision-to-prompt (~2.8 GB; ideal for T4/L4)".to_string(),
            min_vram_gb: 6,
            total_bytes: 1_929_901_056 + 844_757_728,
            model_filename: "Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf".to_string(),
            model_url: "https://huggingface.co/ggml-org/Qwen2.5-VL-3B-Instruct-GGUF/resolve/main/Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf".to_string(),
            model_size_bytes: 1_929_901_056,
            mmproj_filename: "mmproj-Qwen2.5-VL-3B-Instruct-Q8_0.gguf".to_string(),
            mmproj_url: "https://huggingface.co/ggml-org/Qwen2.5-VL-3B-Instruct-GGUF/resolve/main/mmproj-Qwen2.5-VL-3B-Instruct-Q8_0.gguf".to_string(),
            mmproj_size_bytes: 844_757_728,
        },
        SidecarModelPreset {
            id: "qwen2.5-vl-7b-q4".to_string(),
            name: "Qwen2.5-VL 7B Instruct (Q4_K_M)".to_string(),
            description_zh: "高精度多模态反推与本地网关（约 5.5 GB，适合 L4/G4/A100/H100）".to_string(),
            description_en: "High-accuracy vision interrogation & local gateway (~5.5 GB; L4/G4/A100/H100)".to_string(),
            min_vram_gb: 10,
            total_bytes: 4_683_072_032 + 853_119_712,
            model_filename: "Qwen2.5-VL-7B-Instruct-Q4_K_M.gguf".to_string(),
            model_url: "https://huggingface.co/ggml-org/Qwen2.5-VL-7B-Instruct-GGUF/resolve/main/Qwen2.5-VL-7B-Instruct-Q4_K_M.gguf".to_string(),
            model_size_bytes: 4_683_072_032,
            mmproj_filename: "mmproj-Qwen2.5-VL-7B-Instruct-Q8_0.gguf".to_string(),
            mmproj_url: "https://huggingface.co/ggml-org/Qwen2.5-VL-7B-Instruct-GGUF/resolve/main/mmproj-Qwen2.5-VL-7B-Instruct-Q8_0.gguf".to_string(),
            mmproj_size_bytes: 853_119_712,
        },
    ]
}

/// Returns `true` when GPU tier or total VRAM requires freeing ComfyUI models before loading LLM.
pub fn is_compact_vram_mode(gpu: GpuTier, vram_total_bytes: Option<u64>) -> bool {
    matches!(gpu, GpuTier::T4 | GpuTier::L4)
        || vram_total_bytes.is_some_and(|b| b <= COMPACT_VRAM_LIMIT_BYTES)
}

pub fn parse_vm_sidecar_probe(stdout: &str) -> VmSidecarProbe {
    let mut driver_version = None;
    let mut driver_major = None;
    let mut glibc_version = None;
    let mut gpu_name = None;

    let mut in_glibc = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "---GLIBC---" {
            in_glibc = true;
            continue;
        }
        if in_glibc {
            if glibc_version.is_none() {
                // e.g. "ldd (Ubuntu GLIBC 2.35-0ubuntu3.8) 2.35"
                if let Some(last) = trimmed.split_whitespace().last() {
                    let mut parts = last.split('.');
                    if let (Some(maj), Some(min)) = (
                        parts.next().and_then(|s| s.parse::<u32>().ok()),
                        parts.next().and_then(|s| s.parse::<u32>().ok()),
                    ) {
                        glibc_version = Some((maj, min));
                    }
                }
            }
        } else if driver_version.is_none() {
            let mut cols = trimmed.splitn(2, ',');
            if let Some(drv) = cols.next().map(str::trim) {
                if let Some(maj) = drv.split('.').next().and_then(|s| s.parse::<u32>().ok()) {
                    driver_version = Some(drv.to_string());
                    driver_major = Some(maj);
                }
            }
            if let Some(name) = cols.next().map(str::trim) {
                if !name.is_empty() {
                    gpu_name = Some(name.to_string());
                }
            }
        }
    }

    VmSidecarProbe {
        driver_version,
        driver_major,
        glibc_version,
        gpu_name,
    }
}

pub fn validate_vm_sidecar_probe(probe: &VmSidecarProbe) -> Result<(), CoreError> {
    let drv_maj = probe.driver_major.ok_or_else(|| CoreError::InvalidInput {
        detail:
            "未检测到 Colab VM NVIDIA 驱动版本（nvidia-smi 无响应），无法拉起 CUDA llama-server"
                .to_string(),
    })?;
    if drv_maj < MIN_NVIDIA_DRIVER_MAJOR {
        return Err(CoreError::InvalidInput {
            detail: format!(
                "Colab VM NVIDIA 驱动版本过低（当前 {}，llama.cpp {LLAMA_CPP_RELEASE_TAG} CUDA 12.8 需要 >= {MIN_NVIDIA_DRIVER_MAJOR}）",
                probe.driver_version.as_deref().unwrap_or("unknown")
            ),
        });
    }
    let glibc = probe.glibc_version.ok_or_else(|| CoreError::InvalidInput {
        detail: "未检测到 Colab VM glibc 版本，无法校验预编译 llama-server 兼容性".to_string(),
    })?;
    if glibc < MIN_GLIBC_VERSION {
        return Err(CoreError::InvalidInput {
            detail: format!(
                "Colab VM glibc 版本过低（当前 {}.{}，需要 >= {}.{}）",
                glibc.0, glibc.1, MIN_GLIBC_VERSION.0, MIN_GLIBC_VERSION.1
            ),
        });
    }
    Ok(())
}

pub async fn probe_vm_sidecar_env(
    ctl_sock: &Path,
    runner: &dyn CommandRunner,
) -> Result<VmSidecarProbe, CoreError> {
    let cmd = "nvidia-smi --query-gpu=driver_version,name --format=csv,noheader 2>/dev/null || true; echo '---GLIBC---'; ldd --version 2>&1 | head -n 1";
    let spec = build_mux_exec_spec(ctl_sock, cmd, StdinPolicy::Null, Duration::from_secs(15));
    let out = runner.run(spec, None).await?;
    if out.exit_code != Some(0) {
        return Err(CoreError::TunnelFailed {
            detail: format!("探测远端 CUDA/glibc 环境失败：{}", out.stderr.trim()),
        });
    }
    let probe = parse_vm_sidecar_probe(&out.stdout);
    validate_vm_sidecar_probe(&probe)?;
    Ok(probe)
}

/// Calls ComfyUI `POST /free` on the local loopback proxy to unload models in compact VRAM mode.
pub async fn free_comfy_vram(local_port: u16) -> Result<(), String> {
    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(("127.0.0.1", local_port)),
    )
    .await
    .map_err(|_| "connect timeout".to_string())?
    .map_err(|e| e.to_string())?;
    let body = r#"{"unload_models":true,"free_memory":true}"#;
    let req = format!(
        "POST /free HTTP/1.1\r\nHost: 127.0.0.1:{local_port}\r\nOrigin: http://127.0.0.1:{local_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut resp = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut resp)).await;
    Ok(())
}

/// Dynamically adds `-L <llm_sock>:127.0.0.1:8081` over the existing SSH ControlMaster.
pub async fn forward_llm_socket(
    ctl_sock: &Path,
    llm_sock: &Path,
    runner: &dyn CommandRunner,
) -> Result<(), CoreError> {
    let _ = std::fs::remove_file(llm_sock);
    let forward_spec = format!("{}:127.0.0.1:{REMOTE_LLM_PORT}", llm_sock.display());
    let spec = CommandSpec::new(
        SSH_BIN.to_string(),
        vec![
            "-F".to_string(),
            "/dev/null".to_string(),
            "-S".to_string(),
            ctl_sock.display().to_string(),
            "-O".to_string(),
            "forward".to_string(),
            "-L".to_string(),
            forward_spec,
            "placeholder".to_string(),
        ],
        Duration::from_secs(10),
    );
    let out = runner.run(spec, None).await?;
    if out.exit_code != Some(0) {
        return Err(CoreError::TunnelFailed {
            detail: format!("建立 llm.sock 端口转发失败：{}", out.stderr.trim()),
        });
    }
    Ok(())
}

/// Cancels the dynamic `-L <llm_sock>:127.0.0.1:8081` forward on the ControlMaster.
pub async fn cancel_llm_socket(ctl_sock: &Path, llm_sock: &Path, runner: &dyn CommandRunner) {
    let forward_spec = format!("{}:127.0.0.1:{REMOTE_LLM_PORT}", llm_sock.display());
    let spec = CommandSpec::new(
        SSH_BIN.to_string(),
        vec![
            "-F".to_string(),
            "/dev/null".to_string(),
            "-S".to_string(),
            ctl_sock.display().to_string(),
            "-O".to_string(),
            "cancel".to_string(),
            "-L".to_string(),
            forward_spec,
            "placeholder".to_string(),
        ],
        Duration::from_secs(5),
    );
    let _ = runner.run(spec, None).await;
    let _ = std::fs::remove_file(llm_sock);
}

pub fn build_sidecar_launch_script(preset: &SidecarModelPreset, has_hf_token: bool) -> String {
    let model_dir = format!("/content/llama-models/{}", preset.id);
    let model_path = format!("{model_dir}/{}", preset.model_filename);
    let mmproj_path = format!("{model_dir}/{}", preset.mmproj_filename);
    let q_bin_url = posix_single_quote(LLAMA_CPP_CUDA12_URL);
    let q_model_url = posix_single_quote(&preset.model_url);
    let q_mmproj_url = posix_single_quote(&preset.mmproj_url);
    let q_model_path = posix_single_quote(&model_path);
    let q_mmproj_path = posix_single_quote(&mmproj_path);
    let q_alias = posix_single_quote(&preset.id);
    let needed_bytes = preset.total_bytes.saturating_add(2 * 1024 * 1024 * 1024);

    let read_token_block = if has_hf_token {
        "IFS= read -r HF_TOKEN || true"
    } else {
        "HF_TOKEN=\"\""
    };

    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
rm -f -- "$0"
HF_TOKEN=""
{read_token_block}

CONTENT_FREE=$(df -PB1 /content | awk 'NR==2{{print $4}}')
if [ -n "${{CONTENT_FREE}}" ] && [ "${{CONTENT_FREE}}" -lt {needed_bytes} ]; then
  echo "LLAMA_INSUFFICIENT_DISK=${{CONTENT_FREE}} < {needed_bytes}" >&2
  exit 1
fi

mkdir -p /content/llama-bin {model_dir}
if [ ! -x /content/llama-bin/llama-server ]; then
  if ! command -v aria2c >/dev/null 2>&1; then
    apt-get update -qq && apt-get install -y -qq aria2 >/dev/null 2>&1 || true
  fi
  TMP_TGZ=$(mktemp /tmp/.llama_bin.XXXXXX.tar.gz)
  TMP_EXT=$(mktemp -d /tmp/.llama_ext.XXXXXX)
  trap 'rm -rf "${{TMP_TGZ:-}}" "${{TMP_EXT:-}}"' EXIT
  curl -fsSL {q_bin_url} -o "${{TMP_TGZ}}"
  ACTUAL_SHA=$(sha256sum "${{TMP_TGZ}}" | awk '{{print $1}}')
  if [ "${{ACTUAL_SHA}}" != "{LLAMA_CPP_CUDA12_SHA256}" ]; then
    echo "LLAMA_SHA256_MISMATCH=${{ACTUAL_SHA}}" >&2
    exit 1
  fi
  tar -xzf "${{TMP_TGZ}}" -C "${{TMP_EXT}}"
  BIN_SRC=$(find "${{TMP_EXT}}" -name llama-server -type f | head -n 1)
  if [ -z "${{BIN_SRC}}" ]; then
    echo "LLAMA_BIN_MISSING_IN_ARCHIVE" >&2
    exit 1
  fi
  BIN_DIR=$(dirname "${{BIN_SRC}}")
  cp -a "${{BIN_DIR}}/." /content/llama-bin/
  chmod +x /content/llama-bin/llama-server
  rm -rf "${{TMP_EXT}}" "${{TMP_TGZ}}"
fi

download_if_needed() {{
  local url="$1"
  local dest="$2"
  local expected="$3"
  if [ -f "${{dest}}" ]; then
    local cur
    cur=$(stat -c%s "${{dest}}" 2>/dev/null || echo 0)
    if [ "${{cur}}" = "${{expected}}" ]; then
      echo "SIDECAR_MODEL_CACHED=${{dest}}"
      return 0
    fi
  fi
  if command -v aria2c >/dev/null 2>&1; then
    if [ -n "${{HF_TOKEN:-}}" ]; then
      printf "%s\n  header=Authorization: Bearer %s\n" "${{url}}" "${{HF_TOKEN}}" | \
        aria2c --input-file=- --console-log-level=warn --summary-interval=5 --auto-file-renaming=false -c -x 8 -s 8 -k 1M -d "$(dirname "${{dest}}")" -o "$(basename "${{dest}}")"
    else
      printf "%s\n" "${{url}}" | \
        aria2c --input-file=- --console-log-level=warn --summary-interval=5 --auto-file-renaming=false -c -x 8 -s 8 -k 1M -d "$(dirname "${{dest}}")" -o "$(basename "${{dest}}")"
    fi
  else
    if [ -n "${{HF_TOKEN:-}}" ]; then
      printf 'header = "Authorization: Bearer %s"\n' "${{HF_TOKEN}}" | \
        curl -fsSL -C - -K - "${{url}}" -o "${{dest}}"
    else
      curl -fsSL -C - "${{url}}" -o "${{dest}}"
    fi
  fi
  local dl_sz
  dl_sz=$(stat -c%s "${{dest}}" 2>/dev/null || echo 0)
  if [ "${{expected}}" -gt 0 ] && [ "${{dl_sz}}" != "${{expected}}" ]; then
    echo "SIDECAR_SIZE_MISMATCH=${{dl_sz}} != ${{expected}} (${{dest}})" >&2
    exit 1
  fi
  local head_prefix
  head_prefix=$(head -c 64 "${{dest}}" 2>/dev/null | tr '[:upper:]' '[:lower:]' || true)
  if [[ "${{head_prefix}}" == *"<!doctype"* ]] || [[ "${{head_prefix}}" == *"<html"* ]]; then
    echo "SIDECAR_HTML_ERROR_PAGE=${{dest}}" >&2
    exit 1
  fi
}}

download_if_needed {q_model_url} {q_model_path} {model_size}
download_if_needed {q_mmproj_url} {q_mmproj_path} {mmproj_size}

pkill -f '[l]lama-bin/llama-server' || true
sleep 1
export LD_LIBRARY_PATH="/content/llama-bin:/usr/local/cuda/lib64:${{LD_LIBRARY_PATH:-}}"
nohup /content/llama-bin/llama-server \
  --host 127.0.0.1 \
  --port {REMOTE_LLM_PORT} \
  -m {q_model_path} \
  --mmproj {q_mmproj_path} \
  -ngl 99 \
  --ctx-size 8192 \
  --alias {q_alias} \
  >/tmp/llama-server.log 2>&1 </dev/null &

for i in $(seq 1 90); do
  if curl -fsS "http://127.0.0.1:{REMOTE_LLM_PORT}/health" >/dev/null 2>&1; then
    echo "LLAMA_SIDECAR_READY=1"
    exit 0
  fi
  sleep 1
done
tail -n 30 /tmp/llama-server.log >&2 || true
pkill -f '[l]lama-bin/llama-server' || true
exit 1
"#,
        model_size = preset.model_size_bytes,
        mmproj_size = preset.mmproj_size_bytes,
    )
}

pub async fn start_remote_llama_server(
    ctl_sock: &Path,
    preset: &SidecarModelPreset,
    hf_token: Option<&str>,
    runner: &dyn CommandRunner,
    on_line: Option<LineCallback>,
) -> Result<(), CoreError> {
    let clean_token = hf_token.map(str::trim).filter(|t| !t.is_empty());
    let script = build_sidecar_launch_script(preset, clean_token.is_some());
    let encoded = encode_base64(script.as_bytes());
    let remote_cmd = format!(
        "tmp_sh=\"$(mktemp /tmp/.llama_start.XXXXXX.sh)\" && printf '%s' '{encoded}' | base64 -d > \"${{tmp_sh}}\" && chmod 700 \"${{tmp_sh}}\" && bash \"${{tmp_sh}}\"; rc=$?; rm -f \"${{tmp_sh}}\"; exit $rc"
    );
    let stdin = match clean_token {
        Some(tok) => {
            let mut b = tok.as_bytes().to_vec();
            b.push(b'\n');
            StdinPolicy::Bytes(b)
        }
        None => StdinPolicy::Null,
    };
    let spec = build_mux_exec_spec(ctl_sock, &remote_cmd, stdin, Duration::from_secs(1800));
    let out = runner.run(spec, on_line).await?;
    if out.exit_code != Some(0) || !out.stdout.contains("LLAMA_SIDECAR_READY=1") {
        return Err(CoreError::CommandFailed {
            command: "start remote llama-server".to_string(),
            exit_code: out.exit_code,
            stderr: out.stderr,
        });
    }
    Ok(())
}

pub async fn stop_remote_llama_server(
    ctl_sock: &Path,
    runner: &dyn CommandRunner,
) -> Result<(), CoreError> {
    let spec = build_mux_exec_spec(
        ctl_sock,
        "pkill -f '[l]lama-bin/llama-server' || true",
        StdinPolicy::Null,
        Duration::from_secs(15),
    );
    let _ = runner.run(spec, None).await?;
    Ok(())
}

pub fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn detect_image_mime(path: &Path) -> Result<&'static str, CoreError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => Ok("image/png"),
        "jpg" | "jpeg" => Ok("image/jpeg"),
        "webp" => Ok("image/webp"),
        _ => Err(CoreError::InvalidInput {
            detail: "仅支持 PNG / JPG / WebP 图片进行视觉反推".to_string(),
        }),
    }
}

pub fn build_interrogate_payload(
    model_alias: &str,
    data_uri: &str,
    style: InterrogateStyle,
) -> serde_json::Value {
    let instruction = match style {
        InterrogateStyle::FluxNatural => {
            "Describe this image as a concise, high-detail FLUX image generation prompt in English (subject, composition, lighting, camera/lens, atmosphere, color palette). Output only the prompt text."
        }
        InterrogateStyle::ComfyTags => {
            "Extract descriptive ComfyUI / Stable Diffusion comma-separated prompt tags in English (subject, style, lighting, quality, details). Output only the comma-separated tags."
        }
        InterrogateStyle::WanVideoMotion => {
            "Describe this frame as an image-to-video motion prompt for Wan 2.2 in English (camera movement, subtle subject motion, lighting dynamics, mood). Output only the prompt text."
        }
    };

    serde_json::json!({
        "model": model_alias,
        "temperature": 0.3,
        "max_tokens": 512,
        "messages": [
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": instruction },
                    { "type": "image_url", "image_url": { "url": data_uri } }
                ]
            }
        ]
    })
}

pub fn parse_chat_completion_text(json_bytes: &[u8]) -> Result<String, CoreError> {
    let val: serde_json::Value =
        serde_json::from_slice(json_bytes).map_err(|e| CoreError::InvalidInput {
            detail: format!("解析视觉模型响应 JSON 失败：{e}"),
        })?;
    let content = val
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::InvalidInput {
            detail: "视觉模型未返回有效提示词内容".to_string(),
        })?;
    Ok(content.to_string())
}

pub async fn interrogate_image_over_socket(
    llm_sock: &Path,
    image_path: &Path,
    model_alias: &str,
    style: InterrogateStyle,
) -> Result<String, CoreError> {
    const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
    let meta = tokio::fs::metadata(image_path)
        .await
        .map_err(|source| CoreError::Io {
            context: format!("stat image {}", image_path.display()),
            source,
        })?;
    if !meta.is_file() {
        return Err(CoreError::InvalidInput {
            detail: "反推目标不是有效图片文件".to_string(),
        });
    }
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(CoreError::InvalidInput {
            detail: "图片体积超过 16 MiB 上限，请先压缩后再反推".to_string(),
        });
    }
    let mime = detect_image_mime(image_path)?;
    let bytes = tokio::fs::read(image_path)
        .await
        .map_err(|source| CoreError::Io {
            context: format!("read image {}", image_path.display()),
            source,
        })?;
    let data_uri = format!("data:{mime};base64,{}", encode_base64(&bytes));
    let payload = build_interrogate_payload(model_alias, &data_uri, style);
    let body = serde_json::to_vec(&payload).map_err(|e| CoreError::InvalidInput {
        detail: format!("序列化反推请求失败：{e}"),
    })?;

    let mut stream = tokio::time::timeout(Duration::from_secs(10), UnixStream::connect(llm_sock))
        .await
        .map_err(|_| CoreError::TunnelFailed {
            detail: "连接 llm.sock 超时".to_string(),
        })?
        .map_err(|e| CoreError::TunnelFailed {
            detail: format!("连接 llm.sock 失败：{e}"),
        })?;

    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{REMOTE_LLM_PORT}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| CoreError::TunnelFailed {
            detail: e.to_string(),
        })?;
    stream
        .write_all(&body)
        .await
        .map_err(|e| CoreError::TunnelFailed {
            detail: e.to_string(),
        })?;

    let mut resp = Vec::new();
    tokio::time::timeout(Duration::from_secs(90), stream.read_to_end(&mut resp))
        .await
        .map_err(|_| CoreError::TunnelFailed {
            detail: "视觉反推请求超时".to_string(),
        })?
        .map_err(|e| CoreError::TunnelFailed {
            detail: e.to_string(),
        })?;

    let sep = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| CoreError::TunnelFailed {
            detail: "无效的 llama-server HTTP 响应".to_string(),
        })?;
    let header = String::from_utf8_lossy(&resp[..sep]);
    if !header.starts_with("HTTP/1.1 200") && !header.starts_with("HTTP/1.0 200") {
        return Err(CoreError::TunnelFailed {
            detail: format!(
                "llama-server 返回错误：{}",
                header.lines().next().unwrap_or("")
            ),
        });
    }
    parse_chat_completion_text(&resp[sep + 4..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[test]
    fn parses_and_validates_vm_cuda_driver_and_glibc_probe() {
        let ok_out = "550.90.07, NVIDIA L4\n---GLIBC---\nldd (Ubuntu GLIBC 2.35-0ubuntu3.8) 2.35\n";
        let probe = parse_vm_sidecar_probe(ok_out);
        assert_eq!(probe.driver_major, Some(550));
        assert_eq!(probe.glibc_version, Some((2, 35)));
        assert_eq!(probe.gpu_name.as_deref(), Some("NVIDIA L4"));
        assert!(validate_vm_sidecar_probe(&probe).is_ok());

        let old_drv = "515.65.01, Tesla T4\n---GLIBC---\nldd (Ubuntu GLIBC 2.35) 2.35\n";
        assert!(validate_vm_sidecar_probe(&parse_vm_sidecar_probe(old_drv)).is_err());

        let old_glibc = "550.90.07, NVIDIA L4\n---GLIBC---\nldd (GNU libc) 2.27\n";
        assert!(validate_vm_sidecar_probe(&parse_vm_sidecar_probe(old_glibc)).is_err());
    }

    #[test]
    fn compact_vram_mode_distinguishes_l4_t4_from_g4_h100() {
        assert!(is_compact_vram_mode(GpuTier::L4, None));
        assert!(is_compact_vram_mode(GpuTier::T4, None));
        assert!(!is_compact_vram_mode(
            GpuTier::G4,
            Some(96 * 1024 * 1024 * 1024)
        ));
        assert!(!is_compact_vram_mode(
            GpuTier::H100,
            Some(80 * 1024 * 1024 * 1024)
        ));
    }

    #[test]
    fn sidecar_launch_script_pins_release_tag_sha256_and_port_8081() {
        let preset = &builtin_sidecar_models()[0];
        let script = build_sidecar_launch_script(preset, true);
        assert!(script.contains(LLAMA_CPP_RELEASE_TAG));
        assert!(script.contains(LLAMA_CPP_CUDA12_SHA256));
        assert!(script.contains("--port 8081"));
        assert!(script.contains("--mmproj"));
        assert!(script.contains("--input-file=-"));
        assert!(script.contains("-K -"));
        assert!(script.contains("[l]lama-bin/llama-server"));
        assert!(script.contains("SIDECAR_HTML_ERROR_PAGE"));
        assert!(script.contains("rm -f -- \"$0\""));
    }

    #[test]
    fn start_remote_llama_server_passes_only_token_in_stdin_and_base64_script_in_cmd() {
        use crate::runner::CommandOutput;
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::Mutex;

        struct CaptureRunner {
            seen_args: Mutex<Vec<Vec<String>>>,
            seen_stdin: Mutex<Vec<Vec<u8>>>,
        }
        impl CommandRunner for CaptureRunner {
            fn run<'a>(
                &'a self,
                spec: CommandSpec,
                _on_line: Option<LineCallback>,
            ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, CoreError>> + Send + 'a>>
            {
                self.seen_args.lock().unwrap().push(spec.args.clone());
                if let StdinPolicy::Bytes(b) = &spec.stdin {
                    self.seen_stdin.lock().unwrap().push(b.clone());
                }
                Box::pin(async move {
                    Ok(CommandOutput::from_raw(
                        Some(0),
                        "LLAMA_SIDECAR_READY=1\n".to_string(),
                        String::new(),
                    ))
                })
            }
        }

        let runner = CaptureRunner {
            seen_args: Mutex::new(Vec::new()),
            seen_stdin: Mutex::new(Vec::new()),
        };
        let preset = &builtin_sidecar_models()[0];
        let tok = "hf_secret_sidecar_token_123456";
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(start_remote_llama_server(
            Path::new("/tmp/ctl"),
            preset,
            Some(tok),
            &runner,
            None,
        ))
        .unwrap();

        let args = runner.seen_args.lock().unwrap();
        assert!(!args[0].iter().any(|a| a.contains(tok)));
        assert!(args[0].last().unwrap().contains("base64 -d"));
        let stdin = runner.seen_stdin.lock().unwrap();
        assert_eq!(String::from_utf8_lossy(&stdin[0]), format!("{tok}\n"));
    }

    #[test]
    fn interrogate_over_mock_unix_socket_parses_prompt() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = std::env::temp_dir().join(format!("ccu_sidecar_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            let sock_path = dir.join("llm.sock");
            let _ = std::fs::remove_file(&sock_path);
            let img_path = dir.join("sample.png");
            std::fs::write(&img_path, b"\x89PNG\r\n\x1a\n").unwrap();

            let listener = UnixListener::bind(&sock_path).unwrap();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await.unwrap();
                let body = r#"{"choices":[{"message":{"content":"cinematic golden hour mountain landscape"}}]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(resp.as_bytes()).await.unwrap();
            });

            let prompt = interrogate_image_over_socket(
                &sock_path,
                &img_path,
                "qwen2.5-vl-3b-q4",
                InterrogateStyle::FluxNatural,
            )
            .await
            .unwrap();
            assert_eq!(prompt, "cinematic golden hour mountain landscape");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
