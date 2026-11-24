use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::runner::{CommandRunner, CommandSpec};

pub const COLAB_BIN: &str = "colab";
pub const SSH_BIN: &str = "/usr/bin/ssh";
pub const SSH_KEYGEN_BIN: &str = "/usr/bin/ssh-keygen";
pub const LSOF_BIN: &str = "/usr/sbin/lsof";
pub const OPEN_BIN: &str = "/usr/bin/open";
pub const SCUTIL_BIN: &str = "/usr/sbin/scutil";

/// Environment readiness report for external binaries required by Phase 1.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvReport {
    pub colab_path: Option<String>,
    pub ssh_available: bool,
    pub ssh_keygen_available: bool,
}

/// Returns fallback search directories for `colab` when `PATH` is minimal
/// (for example `/usr/bin:/bin:/usr/sbin:/sbin` when opened from Finder).
pub fn fallback_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    home.map(|home| home.join(".local/bin"))
        .into_iter()
        .chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from))
        .collect()
}

/// Finds `colab` using an injected `PATH` and `home` directory without reading `HOME`.
pub fn find_colab(path_env: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    find_colab_in(path_env, &fallback_dirs(home))
}

/// Finds `colab` in `path` entries first, then in `fallback` directories.
pub fn find_colab_in(path: Option<&OsStr>, fallback: &[PathBuf]) -> Option<PathBuf> {
    path.into_iter()
        .flat_map(env::split_paths)
        .chain(fallback.iter().cloned())
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(COLAB_BIN))
        .find(|candidate| is_executable(candidate))
}

/// Checks whether `colab`, `/usr/bin/ssh`, and `/usr/bin/ssh-keygen` are executable.
pub fn check_env(path_env: Option<&OsStr>, home: Option<&Path>) -> EnvReport {
    let colab_path = find_colab(path_env, home).map(|p| p.display().to_string());
    EnvReport {
        colab_path,
        ssh_available: is_executable(Path::new(SSH_BIN)),
        ssh_keygen_available: is_executable(Path::new(SSH_KEYGEN_BIN)),
    }
}

/// Parses `scutil --proxy` output into uppercase/lowercase HTTP/HTTPS proxy env vars
/// so Python `websocket-client` (`colab ssh --proxy-mode`) honors macOS system proxies
/// even when launched from Finder.
pub fn parse_scutil_proxy_envs(scutil_stdout: &str) -> Vec<(String, String)> {
    let mut http_enable = false;
    let mut http_host: Option<String> = None;
    let mut http_port: Option<u16> = None;
    let mut https_enable = false;
    let mut https_host: Option<String> = None;
    let mut https_port: Option<u16> = None;

    for line in scutil_stdout.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim();
        let val = v.trim();
        match key {
            "HTTPEnable" => http_enable = val == "1",
            "HTTPProxy" if !val.is_empty() => http_host = Some(val.to_string()),
            "HTTPPort" => http_port = val.parse::<u16>().ok(),
            "HTTPSEnable" => https_enable = val == "1",
            "HTTPSProxy" if !val.is_empty() => https_host = Some(val.to_string()),
            "HTTPSPort" => https_port = val.parse::<u16>().ok(),
            _ => {}
        }
    }

    let mut envs = Vec::new();
    if http_enable {
        if let (Some(h), Some(p)) = (http_host, http_port) {
            let url = format!("http://{h}:{p}");
            envs.push(("HTTP_PROXY".to_string(), url.clone()));
            envs.push(("http_proxy".to_string(), url));
        }
    }
    if https_enable {
        if let (Some(h), Some(p)) = (https_host, https_port) {
            let url = format!("http://{h}:{p}");
            envs.push(("HTTPS_PROXY".to_string(), url.clone()));
            envs.push(("https_proxy".to_string(), url));
        }
    }
    if !envs.is_empty() {
        envs.push(("NO_PROXY".to_string(), "127.0.0.1,localhost".to_string()));
        envs.push(("no_proxy".to_string(), "127.0.0.1,localhost".to_string()));
    }
    envs
}

/// Resolves proxy env vars from injected process env hints first, falling back to
/// macOS `/usr/sbin/scutil --proxy` when launched from Finder with an empty proxy env.
pub async fn resolve_proxy_envs(
    runner: &dyn CommandRunner,
    env_https: Option<&OsStr>,
    env_http: Option<&OsStr>,
) -> Vec<(String, String)> {
    let mut envs = Vec::new();
    if let Some(val) = env_https.and_then(|v| v.to_str()).filter(|s| !s.is_empty()) {
        envs.push(("HTTPS_PROXY".to_string(), val.to_string()));
        envs.push(("https_proxy".to_string(), val.to_string()));
    }
    if let Some(val) = env_http.and_then(|v| v.to_str()).filter(|s| !s.is_empty()) {
        envs.push(("HTTP_PROXY".to_string(), val.to_string()));
        envs.push(("http_proxy".to_string(), val.to_string()));
    }
    if !envs.is_empty() {
        envs.push(("NO_PROXY".to_string(), "127.0.0.1,localhost".to_string()));
        envs.push(("no_proxy".to_string(), "127.0.0.1,localhost".to_string()));
        return envs;
    }

    if !is_executable(Path::new(SCUTIL_BIN)) {
        return envs;
    }
    let spec = CommandSpec::new(
        SCUTIL_BIN,
        vec!["--proxy".to_string()],
        Duration::from_secs(3),
    );
    if let Ok(out) = runner.run(spec, None).await {
        if out.exit_code == Some(0) {
            return parse_scutil_proxy_envs(&out.stdout);
        }
    }
    envs
}

#[cfg(unix)]
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = PathBuf::from("/tmp").join(format!(
                "ccu-loc-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir.canonicalize().unwrap())
        }

        fn bin_dir(&self, name: &str, mode: Option<u32>) -> PathBuf {
            let dir = self.0.join(name);
            fs::create_dir_all(&dir).unwrap();
            if let Some(mode) = mode {
                let file = dir.join(COLAB_BIN);
                fs::write(&file, "#!/bin/sh\n").unwrap();
                fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
            }
            dir
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn path_wins_over_the_fallback_dirs() {
        let tmp = TempDir::new();
        let on_path = tmp.bin_dir("on-path", Some(0o755));
        let fallback = tmp.bin_dir("fallback", Some(0o755));
        let path = env::join_paths([tmp.bin_dir("empty", None), on_path.clone()]).unwrap();

        let found = find_colab_in(Some(&path), &[fallback]);

        assert_eq!(found, Some(on_path.join(COLAB_BIN)));
    }

    #[test]
    fn the_system_path_of_an_opened_app_falls_back_to_the_install_dirs() {
        let tmp = TempDir::new();
        let fallback = tmp.bin_dir("fallback", Some(0o755));

        let found = find_colab_in(
            Some(OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin")),
            &[tmp.bin_dir("empty", None), fallback.clone()],
        );

        assert_eq!(found, Some(fallback.join(COLAB_BIN)));
    }

    #[test]
    fn a_file_that_cannot_be_executed_is_skipped() {
        let tmp = TempDir::new();
        let not_executable = tmp.bin_dir("plain", Some(0o644));

        assert_eq!(find_colab_in(None, &[not_executable]), None);
    }

    #[test]
    fn relative_path_entries_are_ignored() {
        let tmp = TempDir::new();
        let fallback = tmp.bin_dir("fallback", Some(0o755));

        let found = find_colab_in(Some(OsStr::new(".::bin")), std::slice::from_ref(&fallback));

        assert_eq!(found, Some(fallback.join(COLAB_BIN)));
    }

    #[test]
    fn the_home_dir_comes_first_in_the_fallback_dirs() {
        let dirs = fallback_dirs(Some(Path::new("/Users/someone")));

        assert_eq!(
            dirs,
            [
                "/Users/someone/.local/bin",
                "/opt/homebrew/bin",
                "/usr/local/bin"
            ]
            .map(PathBuf::from)
        );
        assert_eq!(fallback_dirs(None).len(), 2);
    }

    #[test]
    fn check_env_detects_system_ssh_and_injected_home_colab() {
        let tmp = TempDir::new();
        let local_bin = tmp.0.join(".local/bin");
        fs::create_dir_all(&local_bin).unwrap();
        let colab = local_bin.join(COLAB_BIN);
        fs::write(&colab, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&colab, fs::Permissions::from_mode(0o755)).unwrap();

        let report = check_env(Some(OsStr::new("/usr/bin:/bin")), Some(&tmp.0));
        assert_eq!(report.colab_path, Some(colab.display().to_string()));
        assert!(report.ssh_available);
        assert!(report.ssh_keygen_available);
    }

    #[test]
    fn parse_scutil_proxy_extracts_http_and_https_endpoints() {
        let sample = r#"<dictionary> {
  HTTPEnable : 1
  HTTPPort : 7897
  HTTPProxy : 127.0.0.1
  HTTPSEnable : 1
  HTTPSPort : 7897
  HTTPSProxy : 127.0.0.1
  SOCKSEnable : 0
}"#;
        let envs = parse_scutil_proxy_envs(sample);
        assert!(envs.contains(&(
            "HTTPS_PROXY".to_string(),
            "http://127.0.0.1:7897".to_string()
        )));
        assert!(envs.contains(&(
            "https_proxy".to_string(),
            "http://127.0.0.1:7897".to_string()
        )));
        assert!(envs.contains(&(
            "HTTP_PROXY".to_string(),
            "http://127.0.0.1:7897".to_string()
        )));
        assert!(envs.contains(&("NO_PROXY".to_string(), "127.0.0.1,localhost".to_string())));
    }
}
