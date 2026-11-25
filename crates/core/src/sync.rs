use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::locate::OPEN_BIN;
use crate::runner::{CommandRunner, CommandSpec};
use crate::webview_policy::sanitize_download_filename;
use crate::CoreError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PngComfyMetadata {
    pub positive_prompt: Option<String>,
    pub negative_prompt: Option<String>,
    pub seed: Option<String>,
    pub steps: Option<String>,
    pub cfg: Option<String>,
    pub sampler: Option<String>,
    pub model: Option<String>,
    pub workflow_json: Option<String>,
    pub prompt_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GalleryItem {
    pub path: String,
    pub filename: String,
    pub is_video: bool,
    pub size_bytes: u64,
    pub modified_unix: u64,
    pub metadata: Option<PngComfyMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryOutputRef {
    pub prompt_id: String,
    pub filename: String,
    pub subfolder: String,
    pub output_type: String,
    pub is_video: bool,
}

pub fn default_images_dir(home_dir: Option<&Path>) -> PathBuf {
    home_dir
        .map(|h| h.join("Pictures/ColabComfyUI"))
        .unwrap_or_else(|| PathBuf::from("/tmp/ColabComfyUI/Pictures"))
}

pub fn default_videos_dir(home_dir: Option<&Path>) -> PathBuf {
    home_dir
        .map(|h| h.join("Movies/ColabComfyUI"))
        .unwrap_or_else(|| PathBuf::from("/tmp/ColabComfyUI/Movies"))
}

/// Pure-Rust PNG `tEXt` / uncompressed `iTXt` chunk parser extracting ComfyUI `prompt` & `workflow`.
pub fn parse_png_comfy_metadata(bytes: &[u8]) -> Option<PngComfyMetadata> {
    const PNG_SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 8 || &bytes[..8] != PNG_SIG {
        return None;
    }

    let mut pos = 8usize;
    let mut prompt_json: Option<String> = None;
    let mut workflow_json: Option<String> = None;

    while pos + 12 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        let chunk_type = &bytes[pos + 4..pos + 8];
        let data_start = pos + 8;
        let Some(data_end) = data_start.checked_add(len) else {
            break;
        };
        if data_end + 4 > bytes.len() {
            break;
        }
        let data = &bytes[data_start..data_end];
        if chunk_type == b"tEXt" {
            if let Some(nul) = data.iter().position(|&b| b == 0) {
                let key = std::str::from_utf8(&data[..nul]).unwrap_or("");
                let val = String::from_utf8_lossy(&data[nul + 1..]).to_string();
                match key {
                    "prompt" => prompt_json = Some(val),
                    "workflow" => workflow_json = Some(val),
                    _ => {}
                }
            }
        } else if chunk_type == b"iTXt" {
            if let Some(nul1) = data.iter().position(|&b| b == 0) {
                let key = std::str::from_utf8(&data[..nul1]).unwrap_or("");
                // iTXt layout: key \0 compression_flag(1) compression_method(1) lang \0 translated \0 text
                if nul1 + 3 <= data.len() && data[nul1 + 1] == 0 {
                    let rest = &data[nul1 + 3..];
                    if let Some(nul2) = rest.iter().position(|&b| b == 0) {
                        let rest2 = &rest[nul2 + 1..];
                        if let Some(nul3) = rest2.iter().position(|&b| b == 0) {
                            let val = String::from_utf8_lossy(&rest2[nul3 + 1..]).to_string();
                            match key {
                                "prompt" => prompt_json = Some(val),
                                "workflow" => workflow_json = Some(val),
                                _ => {}
                            }
                        }
                    }
                }
            }
        } else if chunk_type == b"IEND" {
            break;
        }
        pos = data_end + 4;
    }

    if prompt_json.is_none() && workflow_json.is_none() {
        return None;
    }

    let mut meta = PngComfyMetadata {
        positive_prompt: None,
        negative_prompt: None,
        seed: None,
        steps: None,
        cfg: None,
        sampler: None,
        model: None,
        workflow_json,
        prompt_json: prompt_json.clone(),
    };

    if let Some(ref pjson) = prompt_json {
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(pjson)
        {
            populate_summary_from_prompt_graph(&map, &mut meta);
        }
    }
    Some(meta)
}

fn populate_summary_from_prompt_graph(
    map: &serde_json::Map<String, serde_json::Value>,
    meta: &mut PngComfyMetadata,
) {
    let mut texts_by_node: BTreeMap<String, String> = BTreeMap::new();
    for (node_id, node_val) in map {
        let class_type = node_val
            .get("class_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if let Some(inp) = node_val.get("inputs").and_then(|v| v.as_object()) {
            if class_type.contains("CLIPTextEncode") {
                if let Some(txt) = inp.get("text").and_then(|v| v.as_str()) {
                    texts_by_node.insert(node_id.clone(), txt.to_string());
                }
            }
            if class_type.contains("CheckpointLoader") || class_type.contains("UNETLoader") {
                if let Some(m) = inp
                    .get("ckpt_name")
                    .or_else(|| inp.get("unet_name"))
                    .and_then(|v| v.as_str())
                {
                    meta.model = Some(m.to_string());
                }
            }
        }
    }
    for node_val in map.values() {
        let class_type = node_val
            .get("class_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if class_type.contains("KSampler") {
            if let Some(inp) = node_val.get("inputs").and_then(|v| v.as_object()) {
                if let Some(s) = inp.get("seed").or_else(|| inp.get("noise_seed")) {
                    meta.seed = Some(s.to_string());
                }
                if let Some(st) = inp.get("steps") {
                    meta.steps = Some(st.to_string());
                }
                if let Some(c) = inp.get("cfg") {
                    meta.cfg = Some(c.to_string());
                }
                if let Some(sm) = inp.get("sampler_name").and_then(|v| v.as_str()) {
                    meta.sampler = Some(sm.to_string());
                }
                if let Some(pos_ref) = inp
                    .get("positive")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                {
                    if let Some(txt) = texts_by_node.get(pos_ref) {
                        meta.positive_prompt = Some(txt.clone());
                    }
                }
                if let Some(neg_ref) = inp
                    .get("negative")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                {
                    if let Some(txt) = texts_by_node.get(neg_ref) {
                        meta.negative_prompt = Some(txt.clone());
                    }
                }
            }
        }
    }
    if meta.positive_prompt.is_none() {
        for (_id, txt) in texts_by_node {
            if !txt.trim().is_empty() {
                meta.positive_prompt = Some(txt);
                break;
            }
        }
    }
}

pub fn list_gallery_items(images_dir: &Path, videos_dir: &Path) -> Vec<GalleryItem> {
    let mut items = Vec::new();
    for (dir, is_video) in [(images_dir, false), (videos_dir, true)] {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            let valid = if is_video {
                matches!(ext.as_str(), "mp4" | "webm" | "gif")
            } else {
                matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp")
            };
            if !valid {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let modified_unix = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let png_meta = if ext == "png" {
                fs::read(&path)
                    .ok()
                    .and_then(|b| parse_png_comfy_metadata(&b))
            } else {
                None
            };
            items.push(GalleryItem {
                path: path.display().to_string(),
                filename: entry.file_name().to_string_lossy().to_string(),
                is_video,
                size_bytes: meta.len(),
                modified_unix,
                metadata: png_meta,
            });
        }
    }
    items.sort_by_key(|b| std::cmp::Reverse(b.modified_unix));
    items.truncate(100);
    items
}

pub fn extract_history_outputs(history_json: &str) -> Vec<HistoryOutputRef> {
    let Ok(serde_json::Value::Object(root)) =
        serde_json::from_str::<serde_json::Value>(history_json)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (prompt_id, entry) in root {
        let Some(outputs) = entry.get("outputs").and_then(|v| v.as_object()) else {
            continue;
        };
        collect_outputs_map(&prompt_id, outputs, &mut out);
    }
    out
}

pub fn extract_ws_executed_outputs(ws_json: &str) -> Vec<HistoryOutputRef> {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(ws_json) else {
        return Vec::new();
    };
    if val.get("type").and_then(|v| v.as_str()) != Some("executed") {
        return Vec::new();
    }
    let Some(data) = val.get("data").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let prompt_id = data
        .get("prompt_id")
        .and_then(|v| v.as_str())
        .unwrap_or("ws");
    let Some(output) = data.get("output").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, is_vid) in [("images", false), ("gifs", true), ("videos", true)] {
        if let Some(arr) = output.get(key).and_then(|v| v.as_array()) {
            push_output_array(prompt_id, arr, is_vid, &mut out);
        }
    }
    out
}

fn collect_outputs_map(
    prompt_id: &str,
    outputs: &serde_json::Map<String, serde_json::Value>,
    out: &mut Vec<HistoryOutputRef>,
) {
    for node_out in outputs.values() {
        let Some(obj) = node_out.as_object() else {
            continue;
        };
        for (key, is_vid) in [("images", false), ("gifs", true), ("videos", true)] {
            if let Some(arr) = obj.get(key).and_then(|v| v.as_array()) {
                push_output_array(prompt_id, arr, is_vid, out);
            }
        }
    }
}

fn push_output_array(
    prompt_id: &str,
    arr: &[serde_json::Value],
    default_is_video: bool,
    out: &mut Vec<HistoryOutputRef>,
) {
    for item in arr {
        let Some(filename) = item.get("filename").and_then(|v| v.as_str()) else {
            continue;
        };
        let output_type = item
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("output");
        if output_type != "output" {
            continue;
        }
        let subfolder = item
            .get("subfolder")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let lower = filename.to_ascii_lowercase();
        let is_video = default_is_video
            || lower.ends_with(".mp4")
            || lower.ends_with(".webm")
            || lower.ends_with(".gif");
        out.push(HistoryOutputRef {
            prompt_id: prompt_id.to_string(),
            filename: filename.to_string(),
            subfolder,
            output_type: output_type.to_string(),
            is_video,
        });
    }
}

pub async fn fetch_local_comfy_bytes(
    local_port: u16,
    path_and_query: &str,
) -> Result<Vec<u8>, String> {
    let mut stream = tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect(("127.0.0.1", local_port)),
    )
    .await
    .map_err(|_| "connect timeout".to_string())?
    .map_err(|e| e.to_string())?;
    let req = format!(
        "GET {path_and_query} HTTP/1.1\r\nHost: 127.0.0.1:{local_port}\r\nOrigin: http://127.0.0.1:{local_port}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), stream.read_to_end(&mut buf))
        .await
        .map_err(|_| "read timeout".to_string())?
        .map_err(|e| e.to_string())?;
    let sep = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "invalid HTTP response".to_string())?;
    let header = String::from_utf8_lossy(&buf[..sep]);
    if !header.starts_with("HTTP/1.1 200") && !header.starts_with("HTTP/1.0 200") {
        return Err(format!(
            "HTTP error: {}",
            header.lines().next().unwrap_or("")
        ));
    }
    Ok(buf[sep + 4..].to_vec())
}

pub async fn download_output_to_local_dir(
    local_port: u16,
    item: &HistoryOutputRef,
    images_dir: &Path,
    videos_dir: &Path,
) -> Result<PathBuf, CoreError> {
    let safe_name = sanitize_download_filename(&item.filename);
    let target_dir = if item.is_video {
        videos_dir
    } else {
        images_dir
    };
    tokio::fs::create_dir_all(target_dir)
        .await
        .map_err(|source| CoreError::Io {
            context: format!("create sync dir {}", target_dir.display()),
            source,
        })?;
    let query = format!(
        "/view?filename={}&subfolder={}&type={}",
        percent_encode(&item.filename),
        percent_encode(&item.subfolder),
        percent_encode(&item.output_type)
    );
    let bytes = fetch_local_comfy_bytes(local_port, &query)
        .await
        .map_err(|detail| CoreError::TunnelFailed { detail })?;
    let dest = deduplicate_dest_path(target_dir, &safe_name);
    tokio::fs::write(&dest, &bytes)
        .await
        .map_err(|source| CoreError::Io {
            context: format!("write synced output {}", dest.display()),
            source,
        })?;
    Ok(dest)
}

fn deduplicate_dest_path(dir: &Path, filename: &str) -> PathBuf {
    let first = dir.join(filename);
    if !first.exists() {
        return first;
    }
    let p = Path::new(filename);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    for i in 1..1000 {
        let name = if ext.is_empty() {
            format!("{stem}_{i}")
        } else {
            format!("{stem}_{i}.{ext}")
        };
        let cand = dir.join(name);
        if !cand.exists() {
            return cand;
        }
    }
    dir.join(format!("{stem}_latest.{ext}"))
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Connects to ComfyUI `/ws` over `127.0.0.1:<local_port>` using a minimal RFC6455 client
/// and yields decoded text frames until the socket closes.
pub async fn read_ws_executed_once<F, Fut>(local_port: u16, mut on_outputs: F) -> Result<(), String>
where
    F: FnMut(Vec<HistoryOutputRef>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let stream = TcpStream::connect(("127.0.0.1", local_port))
        .await
        .map_err(|e| e.to_string())?;
    let (reader, mut writer) = stream.into_split();
    let req = format!(
        "GET /ws?clientId=colab-comfyui-sync HTTP/1.1\r\n\
         Host: 127.0.0.1:{local_port}\r\n\
         Origin: http://127.0.0.1:{local_port}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    writer
        .write_all(req.as_bytes())
        .await
        .map_err(|e| e.to_string())?;

    let mut br = BufReader::new(reader);
    let mut status_line = String::new();
    br.read_line(&mut status_line)
        .await
        .map_err(|e| e.to_string())?;
    if !status_line.contains("101") {
        return Err(format!("ws handshake failed: {}", status_line.trim()));
    }
    loop {
        let mut h = String::new();
        let n = br.read_line(&mut h).await.map_err(|e| e.to_string())?;
        if n == 0 || h == "\r\n" {
            break;
        }
    }

    loop {
        let mut hdr = [0u8; 2];
        if br.read_exact(&mut hdr).await.is_err() {
            break;
        }
        let opcode = hdr[0] & 0x0f;
        let masked = (hdr[1] & 0x80) != 0;
        let mut len = (hdr[1] & 0x7f) as u64;
        if len == 126 {
            let mut ext = [0u8; 2];
            if br.read_exact(&mut ext).await.is_err() {
                break;
            }
            len = u16::from_be_bytes(ext) as u64;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            if br.read_exact(&mut ext).await.is_err() {
                break;
            }
            len = u64::from_be_bytes(ext);
        }
        if len > 16 * 1024 * 1024 {
            break;
        }
        let mut mask_key = [0u8; 4];
        if masked && br.read_exact(&mut mask_key).await.is_err() {
            break;
        }
        let mut payload = vec![0u8; len as usize];
        if br.read_exact(&mut payload).await.is_err() {
            break;
        }
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask_key[i % 4];
            }
        }
        if opcode == 0x8 {
            break;
        } else if opcode == 0x9 {
            // Ping -> send masked Pong
            let _ = writer.write_all(&[0x8A, 0x80, 0, 0, 0, 0]).await;
        } else if opcode == 0x1 {
            if let Ok(text) = std::str::from_utf8(&payload) {
                let outs = extract_ws_executed_outputs(text);
                if !outs.is_empty() {
                    on_outputs(outs).await;
                }
            }
        }
    }
    Ok(())
}

pub async fn reveal_in_finder(path: &Path, runner: &dyn CommandRunner) -> Result<(), CoreError> {
    let spec = CommandSpec::new(
        OPEN_BIN.to_string(),
        vec!["-R".to_string(), path.display().to_string()],
        Duration::from_secs(10),
    );
    let out = runner.run(spec, None).await?;
    if out.exit_code != Some(0) {
        return Err(CoreError::CommandFailed {
            command: "open -R".to_string(),
            exit_code: out.exit_code,
            stderr: out.stderr,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_png_chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + data.len());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(tag);
        out.extend_from_slice(data);
        out.extend_from_slice(&[0u8; 4]);
        out
    }

    #[test]
    fn parses_comfyui_png_text_and_itxt_metadata() {
        let prompt_json = r#"{
            "3": { "class_type": "KSampler", "inputs": { "seed": 123456, "steps": 20, "cfg": 7.5, "sampler_name": "euler", "positive": ["6", 0], "negative": ["7", 0] } },
            "4": { "class_type": "CheckpointLoaderSimple", "inputs": { "ckpt_name": "flux1-dev-fp8.safetensors" } },
            "6": { "class_type": "CLIPTextEncode", "inputs": { "text": "cinematic mountain sunset" } },
            "7": { "class_type": "CLIPTextEncode", "inputs": { "text": "blurry, lowres" } }
        }"#;
        let workflow_json = r#"{"last_node_id": 9}"#;

        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(make_png_chunk(b"IHDR", &[0u8; 13]));
        let mut text_prompt = b"prompt\0".to_vec();
        text_prompt.extend_from_slice(prompt_json.as_bytes());
        png.extend(make_png_chunk(b"tEXt", &text_prompt));
        let mut itxt_wf = b"workflow\0\0\0\0\0".to_vec();
        itxt_wf.extend_from_slice(workflow_json.as_bytes());
        png.extend(make_png_chunk(b"iTXt", &itxt_wf));
        png.extend(make_png_chunk(b"IEND", &[]));

        let meta = parse_png_comfy_metadata(&png).expect("should parse PNG metadata");
        assert_eq!(
            meta.positive_prompt.as_deref(),
            Some("cinematic mountain sunset")
        );
        assert_eq!(meta.negative_prompt.as_deref(), Some("blurry, lowres"));
        assert_eq!(meta.seed.as_deref(), Some("123456"));
        assert_eq!(meta.steps.as_deref(), Some("20"));
        assert_eq!(meta.cfg.as_deref(), Some("7.5"));
        assert_eq!(meta.sampler.as_deref(), Some("euler"));
        assert_eq!(meta.model.as_deref(), Some("flux1-dev-fp8.safetensors"));
        assert_eq!(meta.workflow_json.as_deref(), Some(workflow_json));
    }

    #[test]
    fn extracts_history_and_ws_outputs_and_cursor_dedups() {
        let hist = r#"{
            "p-1": {
                "outputs": {
                    "9": {
                        "images": [
                            { "filename": "ComfyUI_00001_.png", "subfolder": "", "type": "output" },
                            { "filename": "preview.png", "subfolder": "", "type": "temp" }
                        ]
                    }
                }
            }
        }"#;
        let outs = extract_history_outputs(hist);
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].prompt_id, "p-1");
        assert_eq!(outs[0].filename, "ComfyUI_00001_.png");
        assert!(!outs[0].is_video);

        let ws = r#"{"type":"executed","data":{"prompt_id":"p-2","output":{"videos":[{"filename":"wan_001.mp4","subfolder":"vid","type":"output"}]}}}"#;
        let ws_outs = extract_ws_executed_outputs(ws);
        assert_eq!(ws_outs.len(), 1);
        assert!(ws_outs[0].is_video);

        let mut cursor = crate::config::SyncCursor::default();
        assert!(!cursor.contains("p-1", "ComfyUI_00001_.png"));
        cursor.insert("p-1", "ComfyUI_00001_.png");
        assert!(cursor.contains("p-1", "ComfyUI_00001_.png"));
    }
}
