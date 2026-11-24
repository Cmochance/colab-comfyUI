use std::path::{Path, PathBuf};

const FALLBACK_FILENAME: &str = "download.bin";
const MAX_FILENAME_BYTES: usize = 200;

/// Sanitizes a remote-controlled download filename into a safe single basename.
pub fn sanitize_download_filename(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();

    let cleaned: String = last
        .chars()
        .filter(|&c| !c.is_control() && c != '\0')
        .collect();

    let trimmed = cleaned.trim().trim_start_matches('.');
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return FALLBACK_FILENAME.to_string();
    }

    truncate_utf8_preserving_ext(trimmed, MAX_FILENAME_BYTES)
}

fn truncate_utf8_preserving_ext(name: &str, max_bytes: usize) -> String {
    if name.len() <= max_bytes {
        return name.to_string();
    }

    if let Some((stem, ext)) = name.rsplit_once('.') {
        if !stem.is_empty() && !ext.is_empty() && ext.len() + 1 < max_bytes {
            let budget = max_bytes - ext.len() - 1;
            let stem_cut = floor_char_boundary(stem, budget);
            if stem_cut > 0 {
                return format!("{}.{ext}", &stem[..stem_cut]);
            }
        }
    }

    let cut = floor_char_boundary(name, max_bytes);
    if cut == 0 {
        FALLBACK_FILENAME.to_string()
    } else {
        name[..cut].to_string()
    }
}

fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Resolves a safe destination path strictly inside `downloads_dir`.
///
/// Ignores any directory prefix on `raw_suggested_or_wry_dest` so path traversal
/// segments in WebKit's `suggested_filename` cannot escape `downloads_dir`.
pub fn resolve_safe_download_destination(
    downloads_dir: &Path,
    raw_suggested_or_wry_dest: &Path,
) -> PathBuf {
    let raw_leaf = raw_suggested_or_wry_dest
        .file_name()
        .and_then(|n| n.to_str())
        .or_else(|| raw_suggested_or_wry_dest.to_str())
        .unwrap_or(FALLBACK_FILENAME);
    let safe_name = sanitize_download_filename(raw_leaf);

    let candidate = downloads_dir.join(&safe_name);
    if !candidate.exists() {
        return candidate;
    }

    let (stem, ext) = match safe_name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !e.is_empty() => (s, format!(".{e}")),
        _ => (safe_name.as_str(), String::new()),
    };

    for idx in 1..10_000 {
        let next = downloads_dir.join(format!("{stem} ({idx}){ext}"));
        if !next.exists() {
            return next;
        }
    }
    downloads_dir.join(format!("{stem}-{}{ext}", std::process::id()))
}

/// Navigation whitelist for the isolated `canvas` WebView (`http://127.0.0.1:<local_port>` only).
pub fn is_allowed_canvas_navigation(url_str: &str, local_port: u16) -> bool {
    let prefix = format!("http://127.0.0.1:{local_port}");
    let Some(rest) = url_str.strip_prefix(&prefix) else {
        return false;
    };
    rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#')
}

/// External link whitelist before delegating `window.open` to the OS browser.
pub fn is_allowed_external_url(url_str: &str) -> bool {
    let trimmed = url_str.trim();
    if trimmed.is_empty()
        || trimmed
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
    {
        return false;
    }
    let after_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else {
        return false;
    };

    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let host = if authority.starts_with('[') {
        authority
            .strip_prefix('[')
            .and_then(|s| s.split(']').next())
            .unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    !host.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = PathBuf::from(format!(
                "/tmp/ccu-wp-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn download_filename_sanitization_blocks_traversal_and_control_chars() {
        assert_eq!(
            sanitize_download_filename("../../.ssh/authorized_keys"),
            "authorized_keys"
        );
        assert_eq!(
            sanitize_download_filename("..\\..\\evil\\ComfyUI_00001_.png"),
            "ComfyUI_00001_.png"
        );
        assert_eq!(sanitize_download_filename("..."), "download.bin");
        assert_eq!(sanitize_download_filename(".."), "download.bin");
        assert_eq!(sanitize_download_filename(""), "download.bin");
        assert_eq!(sanitize_download_filename("a\x00b\n.png"), "ab.png");

        let long_name = format!("{}.png", "x".repeat(250));
        let truncated = sanitize_download_filename(&long_name);
        assert!(truncated.len() <= MAX_FILENAME_BYTES);
        assert!(truncated.ends_with(".png"));
    }

    #[test]
    fn resolve_safe_download_destination_stays_inside_downloads_dir_and_dedups() {
        let tmp = TempDir::new();
        let unsafe_wry_path = PathBuf::from("/Users/victim/.ssh/../../.zshrc");
        let dest1 = resolve_safe_download_destination(&tmp.0, &unsafe_wry_path);
        assert_eq!(dest1, tmp.0.join("zshrc"));

        let png_dest = resolve_safe_download_destination(&tmp.0, Path::new("image.png"));
        assert_eq!(png_dest, tmp.0.join("image.png"));
        fs::write(&png_dest, b"png").unwrap();

        let png_dest2 = resolve_safe_download_destination(&tmp.0, Path::new("image.png"));
        assert_eq!(png_dest2, tmp.0.join("image (1).png"));
    }

    #[test]
    fn canvas_navigation_allows_only_exact_loopback_origin() {
        assert!(is_allowed_canvas_navigation(
            "http://127.0.0.1:18188",
            18188
        ));
        assert!(is_allowed_canvas_navigation(
            "http://127.0.0.1:18188/",
            18188
        ));
        assert!(is_allowed_canvas_navigation(
            "http://127.0.0.1:18188/?mode=1#hash",
            18188
        ));

        for denied in [
            "https://127.0.0.1:18188/",
            "http://localhost:18188/",
            "http://127.0.0.1:8188/",
            "http://127.0.0.1:18188.evil.com/",
            "http://user@127.0.0.1:18188/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,hi",
            "blob:http://127.0.0.1:18188/uuid",
        ] {
            assert!(
                !is_allowed_canvas_navigation(denied, 18188),
                "should deny {denied}"
            );
        }
    }

    #[test]
    fn external_url_allows_only_http_and_https() {
        assert!(is_allowed_external_url(
            "https://github.com/comfyanonymous/ComfyUI"
        ));
        assert!(is_allowed_external_url("http://example.com:8080/docs"));

        for denied in [
            "file:///Applications/Calculator.app",
            "javascript:alert(1)",
            "data:text/plain,hello",
            "ssh://root@example.com",
            "https://user:pass@evil.com/",
            "https://evil.com/\nfoo",
            "",
        ] {
            assert!(!is_allowed_external_url(denied), "should deny {denied}");
        }
    }
}
