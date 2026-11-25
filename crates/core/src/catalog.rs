use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::posix_single_quote;
use crate::runner::{CommandRunner, LineCallback, StdinPolicy};
use crate::ssh_tunnel::build_mux_exec_spec;
use crate::CoreError;

pub const TWO_GIB: u64 = 2 * 1024 * 1024 * 1024;
pub const MODEL_DOWNLOAD_OK_SENTINEL: &str = "MODEL_DOWNLOAD_OK=1";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(3600);

pub const ALLOWED_MODEL_SUBDIRS: &[&str] = &[
    "checkpoints",
    "loras",
    "diffusion_models",
    "text_encoders",
    "vae",
    "controlnet",
    "upscale_models",
    "clip_vision",
];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DownloadTargetStorage {
    Drive,
    VmContent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogModelFile {
    pub url: String,
    pub subdir: String,
    pub filename: String,
    pub size_bytes: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogPreset {
    pub id: String,
    pub name: String,
    pub category: String,
    pub description_zh: String,
    pub description_en: String,
    pub min_vram_gb: u32,
    pub total_bytes: u64,
    pub files: Vec<CatalogModelFile>,
    pub workflow_filename: Option<String>,
    pub workflow_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadProgress {
    pub preset_id: String,
    pub current_file: String,
    pub file_index: usize,
    pub total_files: usize,
    pub percent: u8,
    pub speed: String,
    pub status: String,
}

/// Built-in verified presets (each URL and byte size verified via HEAD against huggingface.co).
/// Explicitly excludes unverified models such as `MiniMax-H3`.
pub fn builtin_catalog() -> Vec<CatalogPreset> {
    let flux_dev_bytes = 17_246_524_772;
    let flux_schnell_bytes = 17_236_328_572;
    let wan_diff_bytes = 9_999_658_848;
    let wan_txt_bytes = 6_735_906_897;
    let wan_vae_bytes = 1_409_400_960;

    vec![
        CatalogPreset {
            id: "flux1-dev-fp8".to_string(),
            name: "FLUX.1-dev FP8".to_string(),
            category: "image".to_string(),
            description_zh: "高精度文生图/图生图旗舰权重（Comfy-Org FP8 单文件）".to_string(),
            description_en: "High-precision FLUX.1-dev FP8 checkpoint (Comfy-Org single file)".to_string(),
            min_vram_gb: 24,
            total_bytes: flux_dev_bytes,
            files: vec![CatalogModelFile {
                url: "https://huggingface.co/Comfy-Org/Flux-1-Dev/resolve/main/flux1-dev-fp8.safetensors".to_string(),
                subdir: "checkpoints".to_string(),
                filename: "flux1-dev-fp8.safetensors".to_string(),
                size_bytes: flux_dev_bytes,
                sha256: None,
            }],
            workflow_filename: Some("FLUX.1-dev-FP8.json".to_string()),
            workflow_json: Some(flux_workflow_json("flux1-dev-fp8.safetensors", 20, "ComfyUI_FLUX_Dev")),
        },
        CatalogPreset {
            id: "flux1-schnell-fp8".to_string(),
            name: "FLUX.1-schnell FP8".to_string(),
            category: "image".to_string(),
            description_zh: "4 步极速出图权重（Apache 2.0 协议，T4/L4 均可流畅运行）".to_string(),
            description_en: "4-step fast image generation FP8 checkpoint (Apache 2.0)".to_string(),
            min_vram_gb: 16,
            total_bytes: flux_schnell_bytes,
            files: vec![CatalogModelFile {
                url: "https://huggingface.co/Comfy-Org/Flux-1-Schnell/resolve/main/flux1-schnell-fp8.safetensors".to_string(),
                subdir: "checkpoints".to_string(),
                filename: "flux1-schnell-fp8.safetensors".to_string(),
                size_bytes: flux_schnell_bytes,
                sha256: None,
            }],
            workflow_filename: Some("FLUX.1-schnell-FP8.json".to_string()),
            workflow_json: Some(flux_workflow_json("flux1-schnell-fp8.safetensors", 4, "ComfyUI_FLUX_Schnell")),
        },
        CatalogPreset {
            id: "wan22-ti2v-5b".to_string(),
            name: "Wan 2.2 TI2V 5B".to_string(),
            category: "video".to_string(),
            description_zh: "文生视频/图生视频三件套（扩散模型 + UMT5 文本编码器 + Wan2.2 VAE）".to_string(),
            description_en: "Wan 2.2 TI2V 5B text/image-to-video bundle (diffusion + UMT5 + VAE)".to_string(),
            min_vram_gb: 24,
            total_bytes: wan_diff_bytes + wan_txt_bytes + wan_vae_bytes,
            files: vec![
                CatalogModelFile {
                    url: "https://huggingface.co/Comfy-Org/Wan-2.2/resolve/main/split_files/diffusion_models/wan2.2_ti2v_5B_fp16.safetensors".to_string(),
                    subdir: "diffusion_models".to_string(),
                    filename: "wan2.2_ti2v_5B_fp16.safetensors".to_string(),
                    size_bytes: wan_diff_bytes,
                    sha256: None,
                },
                CatalogModelFile {
                    url: "https://huggingface.co/Comfy-Org/Wan-2.2/resolve/main/split_files/text_encoders/umt5_xxl_fp8_e4m3fn_scaled.safetensors".to_string(),
                    subdir: "text_encoders".to_string(),
                    filename: "umt5_xxl_fp8_e4m3fn_scaled.safetensors".to_string(),
                    size_bytes: wan_txt_bytes,
                    sha256: None,
                },
                CatalogModelFile {
                    url: "https://huggingface.co/Comfy-Org/Wan-2.2/resolve/main/split_files/vae/wan2.2_vae.safetensors".to_string(),
                    subdir: "vae".to_string(),
                    filename: "wan2.2_vae.safetensors".to_string(),
                    size_bytes: wan_vae_bytes,
                    sha256: None,
                },
            ],
            workflow_filename: Some("Wan2.2-TI2V-5B.json".to_string()),
            workflow_json: Some(wan_ti2v_workflow_json()),
        },
    ]
}

fn is_safe_model_filename(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Resolves a custom Hugging Face direct URL or `<owner>/<repo>:<file_path>` into a validated `CatalogModelFile`.
pub fn resolve_custom_hf_file(
    input: &str,
    subdir: &str,
    filename_override: Option<&str>,
) -> Result<CatalogModelFile, CoreError> {
    if !ALLOWED_MODEL_SUBDIRS.contains(&subdir) {
        return Err(CoreError::InvalidInput {
            detail: format!("不支持的模型子目录：{subdir}"),
        });
    }
    let trimmed = input.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|c| c.is_ascii_whitespace() || c.is_control() || c == '|')
    {
        return Err(CoreError::InvalidInput {
            detail: "Hugging Face 链接或仓库路径不能为空，且不能包含空白或 `|`".to_string(),
        });
    }

    let url = if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        let rest = trimmed
            .strip_prefix("https://huggingface.co/")
            .or_else(|| trimmed.strip_prefix("https://hf-mirror.com/"))
            .ok_or_else(|| CoreError::InvalidInput {
                detail: "仅允许 https://huggingface.co/ 或 https://hf-mirror.com/ 域名".to_string(),
            })?;
        let authority_prefix = if trimmed.starts_with("https://hf-mirror.com/") {
            "https://hf-mirror.com/"
        } else {
            "https://huggingface.co/"
        };
        if rest.is_empty() || rest.contains('@') || rest.contains("..") {
            return Err(CoreError::InvalidInput {
                detail: "Hugging Face 链接包含非法字符（如 @ 或 ..）".to_string(),
            });
        }
        // Normalize browser `/blob/<rev>/` URLs to `/resolve/<rev>/` so aria2c downloads raw weights instead of HTML.
        let normalized_rest = if let Some(idx) = rest.find("/blob/") {
            format!("{}/resolve/{}", &rest[..idx], &rest[idx + 6..])
        } else {
            rest.to_string()
        };
        format!("{authority_prefix}{normalized_rest}")
    } else if let Some((repo, file_path)) = trimmed.split_once(':') {
        let repo = repo.trim().trim_matches('/');
        let file_path = file_path.trim().trim_start_matches('/');
        if repo.split('/').count() != 2
            || file_path.is_empty()
            || file_path.contains("..")
            || file_path.contains('@')
        {
            return Err(CoreError::InvalidInput {
                detail: "Repo 格式应为 <owner>/<repo>:<path/to/file.safetensors>".to_string(),
            });
        }
        format!("https://huggingface.co/{repo}/resolve/main/{file_path}")
    } else {
        return Err(CoreError::InvalidInput {
            detail: "请输入完整 https://huggingface.co/... 链接或 <owner>/<repo>:<文件路径>"
                .to_string(),
        });
    };

    let raw_name = filename_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            url.split('?')
                .next()
                .and_then(|base| base.rsplit('/').next())
        })
        .unwrap_or("model.safetensors");
    let filename = raw_name.to_string();
    if !is_safe_model_filename(&filename) {
        return Err(CoreError::InvalidInput {
            detail: format!("非法文件名：{filename}"),
        });
    }

    Ok(CatalogModelFile {
        url,
        subdir: subdir.to_string(),
        filename,
        size_bytes: 0,
        sha256: None,
    })
}

/// Validates dual-disk quota (`/content` + Drive when `storage == Drive`) with 2 GiB safety headroom.
pub fn check_disk_quota(
    required_bytes: u64,
    storage: DownloadTargetStorage,
    content_free_bytes: Option<u64>,
    drive_free_bytes: Option<u64>,
) -> Result<(), CoreError> {
    if required_bytes == 0 {
        return Ok(());
    }
    let needed = required_bytes.saturating_add(TWO_GIB);
    if let Some(content_free) = content_free_bytes {
        if content_free < needed {
            return Err(CoreError::InvalidInput {
                detail: format!(
                    "/content 剩余空间不足：需要 {:.1} GB（含 2GB 余量），当前剩余 {:.1} GB",
                    needed as f64 / 1e9,
                    content_free as f64 / 1e9
                ),
            });
        }
    }
    if storage == DownloadTargetStorage::Drive {
        if let Some(drive_free) = drive_free_bytes {
            if drive_free < needed {
                return Err(CoreError::InvalidInput {
                    detail: format!(
                        "Google Drive 剩余空间不足：需要 {:.1} GB（含 2GB 余量），当前剩余 {:.1} GB，请改选「仅 VM /content」或清理 Drive",
                        needed as f64 / 1e9,
                        drive_free as f64 / 1e9
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Builds a remote bash script that reads an optional `HF_TOKEN` from stdin line 1,
/// downloads into a unique `mktemp -d` under `/content` via `aria2c --input-file=-`
/// (`--auto-file-renaming=false`, zero token in argv), verifies size/non-HTML/sha256,
/// checks Drive free space before atomic copy, and cleans up on exit.
pub fn build_remote_download_script(
    files: &[CatalogModelFile],
    storage: DownloadTargetStorage,
    has_hf_token: bool,
) -> String {
    let to_drive = if storage == DownloadTargetStorage::Drive {
        "true"
    } else {
        "false"
    };
    let mut file_entries = String::new();
    for f in files {
        let sha = f.sha256.as_deref().unwrap_or("-");
        file_entries.push_str(&format!(
            "{}|{}|{}|{}|{}\n",
            f.url, f.subdir, f.filename, f.size_bytes, sha
        ));
    }

    let read_token_block = if has_hf_token {
        "IFS= read -r HF_TOKEN || true\n"
    } else {
        "HF_TOKEN=\"\"\n"
    };

    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
rm -f -- "$0" 2>/dev/null || true

{read_token_block}
TO_DRIVE="{to_drive}"
COMFY_MODELS="/content/ComfyUI/models"
DRIVE_MODELS="/content/drive/MyDrive/ComfyUI/models"
TMP_DIR="$(mktemp -d /content/.tmp_comfy_dl.XXXXXX)"

cleanup() {{
  unset HF_TOKEN || true
  rm -rf "${{TMP_DIR}}"
}}
trap cleanup EXIT

while IFS='|' read -r url subdir filename expected_bytes expected_sha; do
  [ -z "${{url}}" ] && continue
  if [ "${{TO_DRIVE}}" = true ]; then
    if [ ! -d "/content/drive/MyDrive" ] || ! grep -qs ' /content/drive' /proc/mounts; then
      echo "[catalog] Error: Google Drive is not mounted" >&2
      exit 1
    fi
    final_dir="${{DRIVE_MODELS}}/${{subdir}}"
  else
    final_dir="${{COMFY_MODELS}}/${{subdir}}"
  fi
  mkdir -p "${{final_dir}}"
  final_path="${{final_dir}}/${{filename}}"
  alt_comfy_path="${{COMFY_MODELS}}/${{subdir}}/${{filename}}"

  for cand in "${{final_path}}" "${{alt_comfy_path}}"; do
    if [ -f "${{cand}}" ]; then
      actual_sz="$(stat -c%s "${{cand}}" 2>/dev/null || wc -c < "${{cand}}")"
      if [ "${{expected_bytes}}" -eq 0 ] && [ "${{actual_sz}}" -gt 0 ]; then
        echo "MODEL_SKIPPED=${{filename}}"
        continue 2
      elif [ "${{expected_bytes}}" -gt 0 ] && [ "${{actual_sz}}" -ge "${{expected_bytes}}" ]; then
        echo "MODEL_SKIPPED=${{filename}}"
        continue 2
      fi
    fi
  done

  echo "MODEL_FILE_START=${{filename}}"
  tmp_file="${{TMP_DIR}}/${{filename}}"
  if [ -n "${{HF_TOKEN:-}}" ]; then
    printf "%s\n  header=Authorization: Bearer %s\n" "${{url}}" "${{HF_TOKEN}}" | \
      aria2c --input-file=- -d "${{TMP_DIR}}" -o "${{filename}}" --auto-file-renaming=false -x 16 -s 16 -k 1M -c --summary-interval=2
  else
    printf "%s\n" "${{url}}" | \
      aria2c --input-file=- -d "${{TMP_DIR}}" -o "${{filename}}" --auto-file-renaming=false -x 16 -s 16 -k 1M -c --summary-interval=2
  fi

  dl_sz="$(stat -c%s "${{tmp_file}}" 2>/dev/null || wc -c < "${{tmp_file}}")"
  if [ "${{expected_bytes}}" -gt 0 ] && [ "${{dl_sz}}" -lt "${{expected_bytes}}" ]; then
    echo "[catalog] Error: downloaded size ${{dl_sz}} < expected ${{expected_bytes}} for ${{filename}}" >&2
    exit 1
  fi
  # Guard against saving an HTML login/error page as model weights
  head_prefix="$(head -c 64 "${{tmp_file}}" 2>/dev/null | tr '[:upper:]' '[:lower:]' || true)"
  if [[ "${{head_prefix}}" == *"<!doctype"* ]] || [[ "${{head_prefix}}" == *"<html"* ]]; then
    echo "[catalog] Error: downloaded HTML page instead of model binary for ${{filename}}" >&2
    exit 1
  fi
  if [ "${{expected_sha}}" != "-" ]; then
    actual_sha="$(sha256sum "${{tmp_file}}" | awk '{{print $1}}')"
    if [ "${{actual_sha}}" != "${{expected_sha}}" ]; then
      echo "[catalog] Error: sha256 mismatch for ${{filename}}" >&2
      exit 1
    fi
  fi

  if [ "${{TO_DRIVE}}" = true ]; then
    drive_free="$(df -PB1 /content/drive/MyDrive | awk 'NR==2{{print $4}}')"
    needed=$(( dl_sz + {TWO_GIB} ))
    if [ -n "${{drive_free}}" ] && [ "${{drive_free}}" -lt "${{needed}}" ]; then
      echo "[catalog] Error: insufficient Google Drive free space (${{drive_free}} < ${{needed}})" >&2
      exit 1
    fi
    cp -f "${{tmp_file}}" "${{final_path}}.tmp"
    mv -f "${{final_path}}.tmp" "${{final_path}}"
    rm -f "${{tmp_file}}"
  else
    mv -f "${{tmp_file}}" "${{final_path}}"
  fi
  echo "MODEL_FILE_DONE=${{filename}}"
done <<'EOF_FILES'
{file_entries}EOF_FILES

echo "{MODEL_DOWNLOAD_OK_SENTINEL}"
"#
    )
}

/// Parses `aria2c` summary line e.g. `[#2089b0 1.2GiB/16GiB(7%) CN:16 DL:45MiB ETA:5m]`
pub fn parse_aria2c_progress_line(line: &str) -> Option<(u8, String)> {
    let pct_end = line.find("%)")?;
    let pct_start = line[..pct_end].rfind('(')? + 1;
    let pct = line[pct_start..pct_end].trim().parse::<u8>().ok()?;
    let speed = if let Some(dl_idx) = line.find("DL:") {
        let rest = &line[dl_idx + 3..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == ']')
            .unwrap_or(rest.len());
        format!("{}/s", &rest[..end])
    } else {
        String::new()
    };
    Some((pct.min(100), speed))
}

pub async fn run_remote_model_download(
    ctl_sock: &Path,
    files: &[CatalogModelFile],
    storage: DownloadTargetStorage,
    hf_token: Option<&str>,
    runner: &dyn CommandRunner,
    on_line: Option<LineCallback>,
) -> Result<(), CoreError> {
    let has_token = hf_token.is_some_and(|t| !t.trim().is_empty());
    let script = build_remote_download_script(files, storage, has_token);
    if let Some(tok) = hf_token.filter(|t| !t.trim().is_empty()) {
        let encoded = base64_encode(script.as_bytes());
        let cmd = format!(
            "tmp_sh=\"$(mktemp /tmp/.colab_dl.XXXXXX.sh)\" && printf '%s' '{encoded}' | base64 -d > \"${{tmp_sh}}\" && chmod 700 \"${{tmp_sh}}\" && bash \"${{tmp_sh}}\"; rc=$?; rm -f \"${{tmp_sh}}\"; exit $rc"
        );
        let mut stdin_bytes = tok.trim().as_bytes().to_vec();
        stdin_bytes.push(b'\n');
        let spec = build_mux_exec_spec(
            ctl_sock,
            &cmd,
            StdinPolicy::Bytes(stdin_bytes),
            DOWNLOAD_TIMEOUT,
        );
        let out = runner.run(spec, on_line).await?;
        if out.exit_code == Some(0) && out.stdout_raw.contains(MODEL_DOWNLOAD_OK_SENTINEL) {
            return Ok(());
        }
        return Err(CoreError::BootstrapFailed {
            detail: if out.stderr.is_empty() {
                format!("模型下载未完成（缺少 {MODEL_DOWNLOAD_OK_SENTINEL}）")
            } else {
                out.stderr
            },
        });
    }

    let spec = build_mux_exec_spec(
        ctl_sock,
        "bash -s",
        StdinPolicy::Bytes(script.into_bytes()),
        DOWNLOAD_TIMEOUT,
    );
    let out = runner.run(spec, on_line).await?;
    if out.exit_code == Some(0) && out.stdout_raw.contains(MODEL_DOWNLOAD_OK_SENTINEL) {
        return Ok(());
    }
    Err(CoreError::BootstrapFailed {
        detail: if out.stderr.is_empty() {
            format!("模型下载未完成（缺少 {MODEL_DOWNLOAD_OK_SENTINEL}）")
        } else {
            out.stderr
        },
    })
}

pub async fn upload_workflow_to_remote(
    ctl_sock: &Path,
    filename: &str,
    workflow_json: &str,
    runner: &dyn CommandRunner,
) -> Result<(), CoreError> {
    if !is_safe_model_filename(filename) {
        return Err(CoreError::InvalidInput {
            detail: format!("非法工作流文件名：{filename}"),
        });
    }
    let encoded = base64_encode(workflow_json.as_bytes());
    let dest = posix_single_quote(&format!(
        "/content/ComfyUI/user/default/workflows/{filename}"
    ));
    let cmd = format!(
        "mkdir -p /content/ComfyUI/user/default/workflows && printf '%s' '{encoded}' | base64 -d > {dest}"
    );
    let spec = build_mux_exec_spec(ctl_sock, &cmd, StdinPolicy::Null, Duration::from_secs(20));
    let out = runner.run(spec, None).await?;
    if out.exit_code != Some(0) {
        return Err(CoreError::CommandFailed {
            command: "upload workflow".to_string(),
            exit_code: out.exit_code,
            stderr: out.stderr,
        });
    }
    Ok(())
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn flux_workflow_json(ckpt_name: &str, steps: u32, save_prefix: &str) -> String {
    serde_json::json!({
        "revision": 0,
        "last_node_id": 8,
        "last_link_id": 10,
        "nodes": [
            {
                "id": 1, "type": "CheckpointLoaderSimple", "pos": [50, 200], "size": [320, 98], "flags": {}, "order": 0, "mode": 0,
                "inputs": [],
                "outputs": [
                    {"name": "MODEL", "type": "MODEL", "slot_index": 0, "links": [1]},
                    {"name": "CLIP", "type": "CLIP", "slot_index": 1, "links": [2]},
                    {"name": "VAE", "type": "VAE", "slot_index": 2, "links": [9]}
                ],
                "widgets_values": [ckpt_name]
            },
            {
                "id": 2, "type": "CLIPTextEncode", "pos": [420, 120], "size": [400, 160], "flags": {}, "order": 1, "mode": 0,
                "inputs": [{"name": "clip", "type": "CLIP", "link": 2}],
                "outputs": [{"name": "CONDITIONING", "type": "CONDITIONING", "slot_index": 0, "links": [3, 4]}],
                "widgets_values": ["a cinematic mountain landscape at sunrise, soft golden lighting, crisp natural detail"]
            },
            {
                "id": 3, "type": "FluxGuidance", "pos": [860, 100], "size": [220, 58], "flags": {}, "order": 2, "mode": 0,
                "inputs": [{"name": "conditioning", "type": "CONDITIONING", "link": 3}],
                "outputs": [{"name": "CONDITIONING", "type": "CONDITIONING", "slot_index": 0, "links": [5]}],
                "widgets_values": [3.5]
            },
            {
                "id": 4, "type": "ConditioningZeroOut", "pos": [860, 200], "size": [220, 30], "flags": {}, "order": 3, "mode": 0,
                "inputs": [{"name": "conditioning", "type": "CONDITIONING", "link": 4}],
                "outputs": [{"name": "CONDITIONING", "type": "CONDITIONING", "slot_index": 0, "links": [6]}]
            },
            {
                "id": 5, "type": "EmptySD3LatentImage", "pos": [860, 290], "size": [260, 106], "flags": {}, "order": 4, "mode": 0,
                "inputs": [],
                "outputs": [{"name": "LATENT", "type": "LATENT", "slot_index": 0, "links": [7]}],
                "widgets_values": [1024, 1024, 1]
            },
            {
                "id": 6, "type": "KSampler", "pos": [1160, 120], "size": [315, 262], "flags": {}, "order": 5, "mode": 0,
                "inputs": [
                    {"name": "model", "type": "MODEL", "link": 1},
                    {"name": "positive", "type": "CONDITIONING", "link": 5},
                    {"name": "negative", "type": "CONDITIONING", "link": 6},
                    {"name": "latent_image", "type": "LATENT", "link": 7}
                ],
                "outputs": [{"name": "LATENT", "type": "LATENT", "slot_index": 0, "links": [8]}],
                "widgets_values": [42, "randomize", steps, 1.0, "euler", "simple", 1.0]
            },
            {
                "id": 7, "type": "VAEDecode", "pos": [1520, 120], "size": [210, 46], "flags": {}, "order": 6, "mode": 0,
                "inputs": [
                    {"name": "samples", "type": "LATENT", "link": 8},
                    {"name": "vae", "type": "VAE", "link": 9}
                ],
                "outputs": [{"name": "IMAGE", "type": "IMAGE", "slot_index": 0, "links": [10]}]
            },
            {
                "id": 8, "type": "SaveImage", "pos": [1770, 120], "size": [320, 270], "flags": {}, "order": 7, "mode": 0,
                "inputs": [{"name": "images", "type": "IMAGE", "link": 10}],
                "outputs": [],
                "widgets_values": [save_prefix]
            }
        ],
        "links": [
            [1, 1, 0, 6, 0, "MODEL"],
            [2, 1, 1, 2, 0, "CLIP"],
            [3, 2, 0, 3, 0, "CONDITIONING"],
            [4, 2, 0, 4, 0, "CONDITIONING"],
            [5, 3, 0, 6, 1, "CONDITIONING"],
            [6, 4, 0, 6, 2, "CONDITIONING"],
            [7, 5, 0, 6, 3, "LATENT"],
            [8, 6, 0, 7, 0, "LATENT"],
            [9, 1, 2, 7, 1, "VAE"],
            [10, 7, 0, 8, 0, "IMAGE"]
        ]
    })
    .to_string()
}

fn wan_ti2v_workflow_json() -> String {
    serde_json::json!({
        "revision": 0,
        "last_node_id": 10,
        "last_link_id": 10,
        "nodes": [
            {
                "id": 1, "type": "UNETLoader", "pos": [50, 80], "size": [320, 82], "flags": {}, "order": 0, "mode": 0,
                "inputs": [], "outputs": [{"name": "MODEL", "type": "MODEL", "slot_index": 0, "links": [1]}],
                "widgets_values": ["wan2.2_ti2v_5B_fp16.safetensors", "default"]
            },
            {
                "id": 2, "type": "CLIPLoader", "pos": [50, 200], "size": [320, 82], "flags": {}, "order": 1, "mode": 0,
                "inputs": [], "outputs": [{"name": "CLIP", "type": "CLIP", "slot_index": 0, "links": [2, 3]}],
                "widgets_values": ["umt5_xxl_fp8_e4m3fn_scaled.safetensors", "wan", "default"]
            },
            {
                "id": 3, "type": "VAELoader", "pos": [50, 320], "size": [320, 58], "flags": {}, "order": 2, "mode": 0,
                "inputs": [], "outputs": [{"name": "VAE", "type": "VAE", "slot_index": 0, "links": [8]}],
                "widgets_values": ["wan2.2_vae.safetensors"]
            },
            {
                "id": 4, "type": "CLIPTextEncode", "pos": [420, 120], "size": [380, 140], "flags": {}, "order": 3, "mode": 0,
                "inputs": [{"name": "clip", "type": "CLIP", "link": 2}],
                "outputs": [{"name": "CONDITIONING", "type": "CONDITIONING", "slot_index": 0, "links": [4]}],
                "widgets_values": ["a tranquil forest stream in morning mist, smooth camera pan, high detail"]
            },
            {
                "id": 5, "type": "CLIPTextEncode", "pos": [420, 290], "size": [380, 120], "flags": {}, "order": 4, "mode": 0,
                "inputs": [{"name": "clip", "type": "CLIP", "link": 3}],
                "outputs": [{"name": "CONDITIONING", "type": "CONDITIONING", "slot_index": 0, "links": [5]}],
                "widgets_values": ["blurry, static, low quality, artifacts"]
            },
            {
                "id": 6, "type": "EmptyHunyuanLatentVideo", "pos": [420, 440], "size": [320, 130], "flags": {}, "order": 5, "mode": 0,
                "inputs": [],
                "outputs": [{"name": "LATENT", "type": "LATENT", "slot_index": 0, "links": [6]}],
                "widgets_values": [832, 480, 25, 1]
            },
            {
                "id": 7, "type": "KSampler", "pos": [840, 140], "size": [315, 262], "flags": {}, "order": 6, "mode": 0,
                "inputs": [
                    {"name": "model", "type": "MODEL", "link": 1},
                    {"name": "positive", "type": "CONDITIONING", "link": 4},
                    {"name": "negative", "type": "CONDITIONING", "link": 5},
                    {"name": "latent_image", "type": "LATENT", "link": 6}
                ],
                "outputs": [{"name": "LATENT", "type": "LATENT", "slot_index": 0, "links": [7]}],
                "widgets_values": [42, "randomize", 20, 5.0, "uni_pc", "simple", 1.0]
            },
            {
                "id": 8, "type": "VAEDecode", "pos": [1200, 140], "size": [210, 46], "flags": {}, "order": 7, "mode": 0,
                "inputs": [
                    {"name": "samples", "type": "LATENT", "link": 7},
                    {"name": "vae", "type": "VAE", "link": 8}
                ],
                "outputs": [{"name": "IMAGE", "type": "IMAGE", "slot_index": 0, "links": [9]}]
            },
            {
                "id": 9, "type": "CreateVideo", "pos": [1450, 140], "size": [220, 78], "flags": {}, "order": 8, "mode": 0,
                "inputs": [{"name": "images", "type": "IMAGE", "link": 9}],
                "outputs": [{"name": "VIDEO", "type": "VIDEO", "slot_index": 0, "links": [10]}],
                "widgets_values": [16]
            },
            {
                "id": 10, "type": "SaveVideo", "pos": [1710, 140], "size": [280, 220], "flags": {}, "order": 9, "mode": 0,
                "inputs": [{"name": "video", "type": "VIDEO", "link": 10}],
                "outputs": [],
                "widgets_values": ["ComfyUI_Wan22", "auto", "auto"]
            }
        ],
        "links": [
            [1, 1, 0, 7, 0, "MODEL"],
            [2, 2, 0, 4, 0, "CLIP"],
            [3, 2, 0, 5, 0, "CLIP"],
            [4, 4, 0, 7, 1, "CONDITIONING"],
            [5, 5, 0, 7, 2, "CONDITIONING"],
            [6, 6, 0, 7, 3, "LATENT"],
            [7, 7, 0, 8, 0, "LATENT"],
            [8, 3, 0, 8, 1, "VAE"],
            [9, 8, 0, 9, 0, "IMAGE"],
            [10, 9, 0, 10, 0, "VIDEO"]
        ]
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_has_verified_sizes_valid_litegraph_workflows_and_excludes_minimax_h3() {
        let cat = builtin_catalog();
        assert!(cat.len() >= 3);
        assert!(!cat.iter().any(|p| p.id.to_lowercase().contains("minimax")));
        for preset in &cat {
            assert!(!preset.files.is_empty());
            let sum: u64 = preset.files.iter().map(|f| f.size_bytes).sum();
            assert_eq!(sum, preset.total_bytes);
            assert!(sum > 0);
            for f in &preset.files {
                assert!(f.url.starts_with("https://huggingface.co/"));
            }
            // H1: every preset has a complete LiteGraph workflow with non-empty links & closed node refs
            let wf_str = preset
                .workflow_json
                .as_ref()
                .expect("every preset should provide a workflow template");
            let wf: serde_json::Value = serde_json::from_str(wf_str).unwrap();
            let nodes = wf["nodes"].as_array().unwrap();
            let links = wf["links"].as_array().unwrap();
            assert!(!links.is_empty(), "workflow {} has empty links", preset.id);
            let node_ids: std::collections::HashSet<i64> =
                nodes.iter().filter_map(|n| n["id"].as_i64()).collect();
            for l in links {
                let arr = l.as_array().unwrap();
                assert!(node_ids.contains(&arr[1].as_i64().unwrap()));
                assert!(node_ids.contains(&arr[3].as_i64().unwrap()));
            }
            if preset.id.starts_with("flux") {
                assert!(nodes.iter().any(|n| n["type"] == "EmptySD3LatentImage"));
                assert!(nodes.iter().any(|n| n["type"] == "FluxGuidance"));
                assert!(!nodes.iter().any(|n| n["type"] == "EmptyLatentImage"));
            }
        }
    }

    #[test]
    fn custom_hf_import_validates_host_normalizes_blob_and_blocks_injection() {
        let f1 = resolve_custom_hf_file(
            "https://huggingface.co/org/repo/blob/main/model.safetensors",
            "loras",
            None,
        )
        .unwrap();
        assert_eq!(
            f1.url,
            "https://huggingface.co/org/repo/resolve/main/model.safetensors"
        );
        assert_eq!(f1.filename, "model.safetensors");

        let f2 =
            resolve_custom_hf_file("org/repo:weights/lora.safetensors", "loras", None).unwrap();
        assert_eq!(
            f2.url,
            "https://huggingface.co/org/repo/resolve/main/weights/lora.safetensors"
        );

        assert!(resolve_custom_hf_file("https://evil.com/a.safetensors", "loras", None).is_err());
        assert!(resolve_custom_hf_file(
            "https://huggingface.co/@evil.com/a.safetensors",
            "loras",
            None
        )
        .is_err());
        assert!(resolve_custom_hf_file(
            "https://huggingface.co/org/repo/resolve/main/a.safetensors|bad",
            "loras",
            None
        )
        .is_err());
        assert!(resolve_custom_hf_file("https://huggingface.co/a/b", "invalid_dir", None).is_err());
    }

    #[test]
    fn disk_quota_enforces_2gib_headroom_on_both_content_and_drive() {
        let ten_gb = 10 * 1024 * 1024 * 1024;
        let eleven_gb = 11 * 1024 * 1024 * 1024;
        let twenty_gb = 20 * 1024 * 1024 * 1024;
        assert!(check_disk_quota(
            ten_gb,
            DownloadTargetStorage::Drive,
            Some(twenty_gb),
            Some(eleven_gb)
        )
        .is_err());
        assert!(check_disk_quota(
            ten_gb,
            DownloadTargetStorage::VmContent,
            Some(twenty_gb),
            Some(eleven_gb)
        )
        .is_ok());
    }

    #[test]
    fn download_script_uses_mktemp_auto_file_renaming_false_and_drive_df_check() {
        let files = vec![CatalogModelFile {
            url: "https://huggingface.co/org/repo/resolve/main/a.safetensors".to_string(),
            subdir: "checkpoints".to_string(),
            filename: "a.safetensors".to_string(),
            size_bytes: 1024,
            sha256: None,
        }];
        let script = build_remote_download_script(&files, DownloadTargetStorage::Drive, true);
        assert!(script.contains("IFS= read -r HF_TOKEN"));
        assert!(script.contains("mktemp -d /content/.tmp_comfy_dl.XXXXXX"));
        assert!(script.contains("aria2c --input-file=-"));
        assert!(script.contains("--auto-file-renaming=false"));
        assert!(script.contains("df -PB1 /content/drive/MyDrive"));
        assert!(!script.contains("--header"));
    }

    #[test]
    fn parses_aria2c_progress_lines() {
        let line = "[#2089b0 1.2GiB/16GiB(7%) CN:16 DL:45MiB ETA:5m]";
        assert_eq!(
            parse_aria2c_progress_line(line),
            Some((7, "45MiB/s".to_string()))
        );
    }
}
