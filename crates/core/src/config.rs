use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::i18n::Locale;
use crate::CoreError;

pub const LOCAL_PROXY_PORT: u16 = 18188;
pub const REMOTE_COMFY_PORT: u16 = 8188;
pub const MAX_UNIX_SOCKET_PATH_LEN: usize = 80;

const CONFIG_FILE: &str = "config.json";
const PENDING_SESSION_FILE: &str = "pending-session.json";
const ACTIVE_SESSION_FILE: &str = "active-session.json";
const STATE_LOCK_FILE: &str = "state.lock";
const SYNC_CURSOR_FILE: &str = "sync-cursor.json";
const APP_LOG_FILE: &str = "app.log";
const APP_LOG_BACKUP_FILE: &str = "app.log.1";
pub const MAX_APP_LOG_BYTES: u64 = 512 * 1024;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Supported Colab GPU tiers in Phase 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GpuTier {
    T4,
    #[default]
    L4,
    G4,
    A100,
    H100,
}

impl GpuTier {
    pub const fn as_cli_arg(self) -> &'static str {
        match self {
            Self::T4 => "T4",
            Self::L4 => "L4",
            Self::G4 => "G4",
            Self::A100 => "A100",
            Self::H100 => "H100",
        }
    }

    /// `google-colab-cli 0.8.0` ignores `--high-mem` on `L4` (`HIGH_MEM_ONLY_ACCELERATORS`).
    pub const fn supports_high_mem(self) -> bool {
        !matches!(self, Self::L4)
    }
}

const fn default_true() -> bool {
    true
}

const fn default_drive_export_timeout_secs() -> u64 {
    90
}

const fn default_watchdog_idle_minutes() -> Option<u32> {
    Some(30)
}

/// Non-sensitive local user configuration persisted in `<data_dir>/config.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    #[serde(default)]
    pub locale: Locale,
    #[serde(default)]
    pub default_gpu: GpuTier,
    #[serde(default)]
    pub high_mem: bool,
    #[serde(default = "default_true")]
    pub use_drive: bool,
    #[serde(default = "default_true")]
    pub export_outputs_on_stop: bool,
    #[serde(default)]
    pub privacy_mask: bool,
    #[serde(default = "default_drive_export_timeout_secs")]
    pub drive_export_timeout_secs: u64,
    #[serde(default = "default_watchdog_idle_minutes")]
    pub watchdog_idle_minutes: Option<u32>,
    #[serde(default = "default_true")]
    pub auto_sync_outputs: bool,
    #[serde(default)]
    pub sync_images_dir: Option<String>,
    #[serde(default)]
    pub sync_videos_dir: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            locale: Locale::ZhHans,
            default_gpu: GpuTier::L4,
            high_mem: false,
            use_drive: true,
            export_outputs_on_stop: true,
            privacy_mask: false,
            drive_export_timeout_secs: default_drive_export_timeout_secs(),
            watchdog_idle_minutes: default_watchdog_idle_minutes(),
            auto_sync_outputs: true,
            sync_images_dir: None,
            sync_videos_dir: None,
        }
    }
}

/// Stage-1 ledger written before invoking `colab new`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingSession {
    pub session_name: String,
    pub owner_email: String,
    pub gpu: GpuTier,
    pub high_mem: bool,
    pub cancel_requested: bool,
    #[serde(default)]
    pub uncertain_orphan: bool,
    #[serde(default)]
    pub drive_enabled: bool,
    pub created_at_unix: u64,
}

/// Stage-2 ledger written after `colab new` + `colab status` succeed.
/// Never stores raw Colab tokens or raw endpoint URLs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveSession {
    pub session_name: String,
    pub owner_email: String,
    pub endpoint_sha256: String,
    pub gpu: GpuTier,
    pub high_mem: bool,
    pub hardware_summary: String,
    pub local_port: u16,
    #[serde(default)]
    pub drive_enabled: bool,
    pub created_at_unix: u64,
}

pub const MAX_SYNC_CURSOR_ENTRIES: usize = 5000;

/// Persistent deduplication cursor for auto-synced ComfyUI outputs (`<data_dir>/sync-cursor.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SyncCursor {
    #[serde(default)]
    pub synced_keys: Vec<String>,
}

impl SyncCursor {
    pub fn contains(&self, prompt_id: &str, subfolder: &str, filename: &str) -> bool {
        let key = format!("{prompt_id}:{subfolder}:{filename}");
        self.synced_keys.iter().any(|k| k == &key)
    }

    pub fn insert(&mut self, prompt_id: &str, subfolder: &str, filename: &str) {
        let key = format!("{prompt_id}:{subfolder}:{filename}");
        if self.synced_keys.iter().any(|k| k == &key) {
            return;
        }
        if self.synced_keys.len() >= MAX_SYNC_CURSOR_ENTRIES {
            let overflow = self.synced_keys.len() + 1 - MAX_SYNC_CURSOR_ENTRIES;
            self.synced_keys.drain(0..overflow);
        }
        self.synced_keys.push(key);
    }
}

pub fn sync_cursor_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SYNC_CURSOR_FILE)
}

/// Exclusive advisory lock on `<data_dir>/state.lock` opened with `O_CLOEXEC`.
pub struct StateLock {
    file: File,
}

impl StateLock {
    pub async fn acquire(data_dir: &Path, timeout: Duration) -> Result<Self, CoreError> {
        ensure_data_dirs(data_dir)?;
        let lock_path = data_dir.join(STATE_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(&lock_path)
            .map_err(|source| CoreError::Io {
                context: format!("open lock file {}", lock_path.display()),
                source,
            })?;
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            CoreError::Io {
                context: format!("chmod 0600 {}", lock_path.display()),
                source,
            }
        })?;

        let deadline = Instant::now() + timeout;
        loop {
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(Self { file });
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK)
                && err.raw_os_error() != Some(libc::EAGAIN)
            {
                return Err(CoreError::Io {
                    context: format!("flock {}", lock_path.display()),
                    source: err,
                });
            }
            if Instant::now() >= deadline {
                return Err(CoreError::LockTimeout {
                    timeout_ms: timeout.as_millis() as u64,
                });
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Ensures `<data_dir>`, `<data_dir>/ssh`, and `<data_dir>/run` exist with `0700`.
pub fn ensure_data_dirs(data_dir: &Path) -> Result<(), CoreError> {
    for dir in [
        data_dir.to_path_buf(),
        data_dir.join("ssh"),
        data_dir.join("run"),
    ] {
        fs::create_dir_all(&dir).map_err(|source| CoreError::Io {
            context: format!("create directory {}", dir.display()),
            source,
        })?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(|source| {
            CoreError::Io {
                context: format!("chmod 0700 {}", dir.display()),
                source,
            }
        })?;
    }
    Ok(())
}

/// Atomically writes `content` to `path` with mode `0600` (`temp + fsync + rename`).
pub fn atomic_write_0600(path: &Path, content: &[u8]) -> Result<(), CoreError> {
    let parent = path.parent().ok_or_else(|| CoreError::Io {
        context: format!("missing parent directory for {}", path.display()),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent directory"),
    })?;
    fs::create_dir_all(parent).map_err(|source| CoreError::Io {
        context: format!("create parent {}", parent.display()),
        source,
    })?;

    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));

    let write_res = (|| -> std::io::Result<()> {
        let mut tmp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(&tmp_path)?;
        tmp.write_all(content)?;
        tmp.sync_all()?;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp_path, path)?;
        if let Ok(dir_file) = File::open(parent) {
            let _ = dir_file.sync_all();
        }
        Ok(())
    })();

    if let Err(source) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(CoreError::Io {
            context: format!("atomic write {}", path.display()),
            source,
        });
    }
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Deterministic account-scoped session name: `comfy-<sha256(lowercase_email)[..8]>`.
pub fn default_session_name_for_email(email: &str) -> String {
    let normalized = email.trim().to_ascii_lowercase();
    let digest = sha256_hex(normalized.as_bytes());
    format!("comfy-{}", &digest[..8])
}

/// Validates session names against `^[A-Za-z0-9_-]{1,32}$` to prevent shell/path injection.
pub fn validate_session_name(name: &str) -> Result<(), CoreError> {
    let valid = !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(CoreError::InvalidSessionName {
            name: name.to_string(),
        })
    }
}

/// Quotes a string safely for POSIX `/bin/sh` (`ProxyCommand`).
pub fn posix_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn validate_socket_path(path: PathBuf) -> Result<PathBuf, CoreError> {
    let len = path.to_string_lossy().len();
    if len >= MAX_UNIX_SOCKET_PATH_LEN {
        return Err(CoreError::SocketPathTooLong {
            path: path.display().to_string(),
            len,
            max: MAX_UNIX_SOCKET_PATH_LEN,
        });
    }
    Ok(path)
}

/// Returns `<data_dir>/ssh/ctl-<hash8>`, enforcing `< 80` bytes for macOS `sun_path`.
pub fn ctl_socket_path(data_dir: &Path, session_name: &str) -> Result<PathBuf, CoreError> {
    let digest = sha256_hex(session_name.as_bytes());
    validate_socket_path(data_dir.join("ssh").join(format!("ctl-{}", &digest[..8])))
}

/// Returns `<data_dir>/run/comfy.sock`, enforcing `< 80` bytes for macOS `sun_path`.
pub fn comfy_socket_path(data_dir: &Path) -> Result<PathBuf, CoreError> {
    validate_socket_path(data_dir.join("run").join("comfy.sock"))
}

pub fn ssh_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ssh").join("id_ed25519")
}

pub fn ssh_pub_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ssh").join("id_ed25519.pub")
}

pub fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CONFIG_FILE)
}

pub fn pending_session_path(data_dir: &Path) -> PathBuf {
    data_dir.join(PENDING_SESSION_FILE)
}

pub fn active_session_path(data_dir: &Path) -> PathBuf {
    data_dir.join(ACTIVE_SESSION_FILE)
}

pub fn app_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join(APP_LOG_FILE)
}

pub fn app_log_backup_path(data_dir: &Path) -> PathBuf {
    data_dir.join(APP_LOG_BACKUP_FILE)
}

pub fn load_json_opt<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, CoreError> {
    match fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|err| CoreError::Serialization {
                    context: format!("parse {}", path.display()),
                    detail: err.to_string(),
                })
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CoreError::Io {
            context: format!("read {}", path.display()),
            source,
        }),
    }
}

pub fn save_json_0600<T: Serialize>(path: &Path, value: &T) -> Result<(), CoreError> {
    let json = serde_json::to_vec_pretty(value).map_err(|err| CoreError::Serialization {
        context: format!("serialize {}", path.display()),
        detail: err.to_string(),
    })?;
    atomic_write_0600(path, &json)
}

pub fn remove_if_exists(path: &Path) -> Result<(), CoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CoreError::Io {
            context: format!("remove {}", path.display()),
            source,
        }),
    }
}

/// Appends a redacted log line to `<data_dir>/app.log` (`0600`), rotating to `app.log.1` at `MAX_APP_LOG_BYTES`.
pub fn append_app_log_0600(data_dir: &Path, message: &str) -> Result<(), CoreError> {
    append_app_log_with_limit(data_dir, message, MAX_APP_LOG_BYTES)
}

fn append_app_log_with_limit(
    data_dir: &Path,
    message: &str,
    max_bytes: u64,
) -> Result<(), CoreError> {
    ensure_data_dirs(data_dir)?;
    let redacted = crate::runner::redact_sensitive_text(message);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let entry = format!("[{ts}] {}\n", redacted.trim_end_matches('\n'));

    let log_path = app_log_path(data_dir);
    let backup_path = app_log_backup_path(data_dir);
    if let Ok(meta) = fs::metadata(&log_path) {
        if meta.len() > 0 && meta.len().saturating_add(entry.len() as u64) > max_bytes {
            let _ = fs::rename(&log_path, &backup_path);
            if backup_path.exists() {
                let _ = fs::set_permissions(&backup_path, fs::Permissions::from_mode(0o600));
            }
        }
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(&log_path)
        .map_err(|source| CoreError::Io {
            context: format!("open app log {}", log_path.display()),
            source,
        })?;
    fs::set_permissions(&log_path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        CoreError::Io {
            context: format!("chmod 0600 {}", log_path.display()),
            source,
        }
    })?;
    file.write_all(entry.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|source| CoreError::Io {
            context: format!("write app log {}", log_path.display()),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    struct ShortTempDir(PathBuf);

    impl ShortTempDir {
        fn new() -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = PathBuf::from(format!("/tmp/ccu-cfg-{}-{seq}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for ShortTempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn block_on<F: Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn ensure_dirs_and_atomic_write_set_0700_and_0600_permissions() {
        let tmp = ShortTempDir::new();
        ensure_data_dirs(&tmp.0).unwrap();

        for dir in [&tmp.0, &tmp.0.join("ssh"), &tmp.0.join("run")] {
            let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "dir {}", dir.display());
        }

        let target = active_session_path(&tmp.0);
        let session = ActiveSession {
            session_name: "comfy-12345678".to_string(),
            owner_email: "alice@example.com".to_string(),
            endpoint_sha256: "deadbeef".to_string(),
            gpu: GpuTier::L4,
            high_mem: false,
            hardware_summary: "GPU L4".to_string(),
            local_port: LOCAL_PROXY_PORT,
            drive_enabled: true,
            created_at_unix: 1_700_000_000,
        };
        save_json_0600(&target, &session).unwrap();
        let file_mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);

        let loaded: ActiveSession = load_json_opt(&target).unwrap().unwrap();
        assert_eq!(loaded, session);
    }

    #[test]
    fn state_lock_uses_cloexec_and_blocks_second_acquirer_until_dropped() {
        let tmp = ShortTempDir::new();
        block_on(async {
            let lock1 = StateLock::acquire(&tmp.0, Duration::from_millis(500))
                .await
                .unwrap();
            let fd_flags = unsafe { libc::fcntl(lock1.file.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(fd_flags & libc::FD_CLOEXEC, 0);

            let err = StateLock::acquire(&tmp.0, Duration::from_millis(80))
                .await
                .err()
                .expect("second lock must time out while first lock is held");
            assert!(matches!(err, CoreError::LockTimeout { .. }));

            drop(lock1);

            let lock2 = StateLock::acquire(&tmp.0, Duration::from_millis(500))
                .await
                .unwrap();
            drop(lock2);
        });
    }

    #[test]
    fn session_name_hashing_validation_and_posix_quoting() {
        let a = default_session_name_for_email("Alice@Example.com ");
        let b = default_session_name_for_email("alice@example.com");
        assert_eq!(a, b);
        assert!(a.starts_with("comfy-"));
        assert_eq!(a.len(), 14);
        assert!(validate_session_name(&a).is_ok());

        for bad in [
            "",
            "bad space",
            "bad;rm",
            "../etc",
            "a/b",
            "toolong12345678901234567890123456",
        ] {
            assert!(
                validate_session_name(bad).is_err(),
                "expected err for {bad}"
            );
        }

        assert_eq!(
            posix_single_quote("/tmp/my 'colab' bin"),
            "'/tmp/my '\\''colab'\\'' bin'"
        );
    }

    #[test]
    fn socket_paths_enforce_80_byte_limit() {
        let tmp = ShortTempDir::new();
        let ctl = ctl_socket_path(&tmp.0, "comfy-12345678").unwrap();
        let comfy = comfy_socket_path(&tmp.0).unwrap();
        assert!(ctl.to_string_lossy().len() < MAX_UNIX_SOCKET_PATH_LEN);
        assert!(comfy.to_string_lossy().len() < MAX_UNIX_SOCKET_PATH_LEN);

        let long_dir = PathBuf::from(format!("/tmp/{}", "a".repeat(75)));
        assert!(matches!(
            comfy_socket_path(&long_dir),
            Err(CoreError::SocketPathTooLong { .. })
        ));
    }

    #[test]
    fn l4_disables_high_mem() {
        assert!(!GpuTier::L4.supports_high_mem());
        assert!(GpuTier::T4.supports_high_mem());
        assert!(GpuTier::G4.supports_high_mem());
        assert!(GpuTier::A100.supports_high_mem());
        assert!(GpuTier::H100.supports_high_mem());
    }

    #[test]
    fn app_log_enforces_0600_redacts_secrets_and_rotates_at_512kib() {
        let tmp = ShortTempDir::new();
        append_app_log_0600(
            &tmp.0,
            "endpoint m-s-secret999 with Authorization: Bearer ya29.secret_tok",
        )
        .unwrap();

        let log_path = app_log_path(&tmp.0);
        let backup_path = app_log_backup_path(&tmp.0);
        assert_eq!(
            fs::metadata(&log_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let content = fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("[REDACTED_ENDPOINT]"));
        assert!(content.contains("Bearer [REDACTED]"));
        assert!(!content.contains("m-s-secret999"));
        assert!(!content.contains("ya29.secret_tok"));

        let chunk = "a".repeat(200 * 1024);
        for _ in 0..3 {
            append_app_log_0600(&tmp.0, &chunk).unwrap();
        }
        assert!(backup_path.exists(), "app.log.1 should exist after 512 KiB");
        assert_eq!(
            fs::metadata(&backup_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(fs::metadata(&log_path).unwrap().len() < MAX_APP_LOG_BYTES);
    }
}
