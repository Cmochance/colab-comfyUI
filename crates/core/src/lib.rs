pub mod bootstrap;
pub mod colab_cli;
pub mod config;
pub mod i18n;
pub mod local_proxy;
pub mod locate;
pub mod persistence;
pub mod runner;
pub mod ssh_tunnel;
pub mod telemetry;
pub mod webview_policy;

pub use locate::find_colab;

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::Poll;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{watch, Mutex, Notify};
use tokio::task::JoinHandle;

use crate::bootstrap::{run_remote_bootstrap, BootstrapOutcome};
use crate::colab_cli::{SessionEntry, SessionStatusLookup, StopReport, UsageInfo};
use crate::config::{
    active_session_path, append_app_log_0600, config_path, ctl_socket_path,
    default_session_name_for_email, ensure_data_dirs, load_json_opt, pending_session_path,
    remove_if_exists, save_json_0600, validate_session_name, ActiveSession, AppConfig, GpuTier,
    PendingSession, StateLock, LOCAL_PROXY_PORT, REMOTE_COMFY_PORT,
};
use crate::i18n::{message_table, Locale};
use crate::local_proxy::{start_local_proxy, LocalProxyHandle};
use crate::locate::{check_env, resolve_proxy_envs, EnvReport};
use crate::persistence::{
    check_drive_mounted_mux, drivemount_args, run_remote_export_and_flush, DriveAuthPrompt,
    DriveAuthStatus, DriveExportReport, DrivemountLine, DrivemountSpawner, MountProbe,
    TokioDrivemountSpawner, DRIVE_AUTH_TIMEOUT, DRIVE_FAILED_GRACE_TIMEOUT,
    MAX_DRIVE_MOUNT_ATTEMPTS,
};
use crate::runner::{
    redact_sensitive_text, CommandRunner, LineCallback, StdinPolicy, TokioCommandRunner,
};
use crate::ssh_tunnel::{
    build_mux_exec_spec, start_master_and_verify, stop_master_tunnel, TokioTunnelSpawner,
    TunnelProcess, TunnelSpawner,
};
use crate::telemetry::{
    estimated_remaining_minutes, fetch_local_comfy_json, fetch_remote_probe, parse_comfy_queue,
    parse_comfy_system_stats, push_telemetry_sample, TelemetrySample, TelemetrySnapshot,
    TELEMETRY_LOCAL_INTERVAL,
};

const MAX_LOG_LINES: usize = 300;
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_BACKOFF_SECS: [u64; 5] = [2, 4, 8, 16, 30];

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("需要登录 colab：{detail}")]
    AuthRequired { detail: String },

    #[error("账号暂无 {gpu} 配额或未开通对应订阅：{detail}")]
    NoAcceleratorQuota { gpu: String, detail: String },

    #[error("Colab 暂无法分配所选 GPU（可能当前规格容量不足、网关超时或已达并发上限，建议改选 T4 等其他 GPU 重试）：{detail}")]
    CapacityOrSessionLimit { detail: String, uncertain: bool },

    #[error("本地会话账本损坏或无法读取，已保留现场并禁止静默清除：{detail}")]
    SessionLedgerCorrupted { detail: String },

    #[error("云端会话 '{session_name}' 已不存在")]
    SessionMissing { session_name: String },

    #[error("当前 colab 账号已变更（开机账号 {expected_email}，当前 {actual_email}）")]
    AccountMismatch {
        expected_email: String,
        actual_email: String,
    },

    #[error("本地会话账本不一致：{detail}")]
    SessionLedgerMismatch { detail: String },

    #[error("当前账号计算单元余额为 {balance:.2} CU，需要二次确认后才能继续测试")]
    ZeroBalanceBlocked { balance: f64 },

    #[error("未能核对 colab usage 余额：{detail}")]
    UsageUnverified { detail: String },

    #[error("未能核对 colab 会话状态：{detail}")]
    SessionsUnverified { detail: String },

    #[error("会话名 '{name}' 不合法（只允许 1..=32 位字母、数字、下划线或短横线）")]
    InvalidSessionName { name: String },

    #[error("Unix Socket 路径过长（{len} >= {max} 字节）：{path}")]
    SocketPathTooLong {
        path: String,
        len: usize,
        max: usize,
    },

    #[error("本地端口 127.0.0.1:{port} 已被占用（进程 {process_name:?}, PID {pid:?}）")]
    PortInUse {
        port: u16,
        pid: Option<u32>,
        process_name: Option<String>,
    },

    #[error("等待本地状态锁超时（{timeout_ms} ms）")]
    LockTimeout { timeout_ms: u64 },

    #[error("命令 {command} 超时（{timeout_secs}s）")]
    CommandTimeout { command: String, timeout_secs: u64 },

    #[error("命令 {command} 执行失败（退出码 {exit_code:?}）：{stderr}")]
    CommandFailed {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },

    #[error("缺少必要命令：{name}")]
    MissingBinary { name: String },

    #[error("远端 ComfyUI 部署或启动失败：{detail}")]
    BootstrapFailed { detail: String },

    #[error("SSH 隧道连接失败：{detail}")]
    TunnelFailed { detail: String },

    #[error("开机取消：{detail}")]
    StartCancelledAfterAllocation {
        session_name: String,
        stop_verified: bool,
        detail: String,
    },

    #[error("已有开机流程正在进行中")]
    StartInProgress,

    #[error("I/O 错误（{context}）：{source}")]
    Io {
        context: String,
        source: std::io::Error,
    },

    #[error("序列化错误（{context}）：{detail}")]
    Serialization { context: String, detail: String },
}

impl CoreError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::AuthRequired { .. } => "AUTH_REQUIRED",
            Self::NoAcceleratorQuota { .. } => "NO_ACCELERATOR_QUOTA",
            Self::CapacityOrSessionLimit { .. } => "CAPACITY_OR_SESSION_LIMIT",
            Self::SessionMissing { .. } => "SESSION_MISSING",
            Self::AccountMismatch { .. } => "ACCOUNT_MISMATCH",
            Self::SessionLedgerMismatch { .. } => "SESSION_LEDGER_MISMATCH",
            Self::SessionLedgerCorrupted { .. } => "SESSION_LEDGER_CORRUPTED",
            Self::ZeroBalanceBlocked { .. } => "ZERO_BALANCE_BLOCKED",
            Self::UsageUnverified { .. } => "USAGE_UNVERIFIED",
            Self::SessionsUnverified { .. } => "SESSIONS_UNVERIFIED",
            Self::InvalidSessionName { .. } => "INVALID_SESSION_NAME",
            Self::SocketPathTooLong { .. } => "SOCKET_PATH_TOO_LONG",
            Self::PortInUse { .. } => "PORT_IN_USE",
            Self::LockTimeout { .. } => "LOCK_TIMEOUT",
            Self::CommandTimeout { .. } => "COMMAND_TIMEOUT",
            Self::CommandFailed { .. } => "COMMAND_FAILED",
            Self::MissingBinary { .. } => "MISSING_BINARY",
            Self::BootstrapFailed { .. } => "BOOTSTRAP_FAILED",
            Self::TunnelFailed { .. } => "TUNNEL_FAILED",
            Self::StartCancelledAfterAllocation { .. } => "START_CANCELLED",
            Self::StartInProgress => "START_IN_PROGRESS",
            Self::Io { .. } => "IO_ERROR",
            Self::Serialization { .. } => "SERIALIZATION_ERROR",
        }
    }

    pub const fn is_uncertain_allocation_failure(&self) -> bool {
        match self {
            Self::CapacityOrSessionLimit { uncertain, .. } => *uncertain,
            Self::CommandTimeout { .. } | Self::CommandFailed { .. } | Self::Io { .. } => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiError {
    pub code: String,
    pub message: String,
}

impl From<&CoreError> for UiError {
    fn from(err: &CoreError) -> Self {
        Self {
            code: err.code().to_string(),
            message: err.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum SessionPhase {
    #[default]
    Idle,
    Starting,
    Ready,
    Reconnecting,
    Stopping,
    DisconnectedOrMissing,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum StartStep {
    Preflight,
    CreatingSession,
    OpeningTunnel,
    Bootstrapping,
    Ready,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartupNotice {
    pub kind: String,
    pub message: String,
    pub session_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppSnapshot {
    pub phase: SessionPhase,
    pub step: Option<StartStep>,
    pub env: EnvReport,
    pub config: AppConfig,
    pub account_email: Option<String>,
    pub auth_required: bool,
    pub usage: Option<UsageInfo>,
    pub active_session: Option<ActiveSession>,
    pub pending_session: Option<PendingSession>,
    pub ledger_corrupted: bool,
    pub orphan_count: usize,
    pub startup_notice: Option<StartupNotice>,
    pub warning_banner: Option<String>,
    pub last_error: Option<UiError>,
    pub last_stop_report: Option<StopReport>,
    pub tunnel_connected: bool,
    pub canvas_url: Option<String>,
    pub drive_auth: Option<DriveAuthPrompt>,
    pub telemetry: TelemetrySnapshot,
    pub last_export_report: Option<DriveExportReport>,
    pub logs: Vec<String>,
    pub i18n: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriveUserAction {
    Continue,
    Retry,
    Skip,
}

#[derive(Default)]
struct RuntimeHandles {
    proxy: Option<LocalProxyHandle>,
    tunnel_proc: Option<Box<dyn TunnelProcess>>,
    ctl_sock: Option<PathBuf>,
    comfy_sock: Option<PathBuf>,
    supervisor_task: Option<JoinHandle<()>>,
    telemetry_task: Option<JoinHandle<()>>,
}

struct InnerState {
    config: AppConfig,
    phase: SessionPhase,
    step: Option<StartStep>,
    env: EnvReport,
    account_email: Option<String>,
    auth_required: bool,
    usage: Option<UsageInfo>,
    active_session: Option<ActiveSession>,
    pending_session: Option<PendingSession>,
    ledger_corrupted: bool,
    orphan_count: usize,
    startup_notice: Option<StartupNotice>,
    warning_banner: Option<String>,
    last_error: Option<UiError>,
    last_stop_report: Option<StopReport>,
    tunnel_connected: bool,
    drive_auth: Option<DriveAuthPrompt>,
    telemetry: TelemetrySnapshot,
    last_export_report: Option<DriveExportReport>,
    has_managed_session: Arc<AtomicBool>,
}

impl InnerState {
    fn sync_managed_flag(&self) {
        self.has_managed_session.store(
            self.active_session.is_some()
                || self.pending_session.is_some()
                || self.ledger_corrupted,
            Ordering::SeqCst,
        );
    }
}

struct LoadedLedgers {
    pending: Option<PendingSession>,
    active: Option<ActiveSession>,
    pending_corrupted: bool,
    active_corrupted: bool,
    corruption_error: Option<UiError>,
}

enum VerifiedLedgerTarget {
    Active {
        session_name: String,
        owner_email: String,
    },
    Pending {
        session_name: String,
        owner_email: String,
    },
}

fn load_ledgers_checked(data_dir: &Path) -> LoadedLedgers {
    let mut errors = Vec::new();
    let mut pending_corrupted = false;
    let mut active_corrupted = false;
    let pending = match load_json_opt::<PendingSession>(&pending_session_path(data_dir)) {
        Ok(v) => v,
        Err(err) => {
            pending_corrupted = true;
            errors.push(format!("pending-session.json: {err}"));
            None
        }
    };
    let active = match load_json_opt::<ActiveSession>(&active_session_path(data_dir)) {
        Ok(v) => v,
        Err(err) => {
            active_corrupted = true;
            errors.push(format!("active-session.json: {err}"));
            None
        }
    };
    let corruption_error = if errors.is_empty() {
        None
    } else {
        Some(UiError::from(&CoreError::SessionLedgerCorrupted {
            detail: errors.join("; "),
        }))
    };
    LoadedLedgers {
        pending,
        active,
        pending_corrupted,
        active_corrupted,
        corruption_error,
    }
}

fn bump_snapshot(tx: &watch::Sender<u64>) {
    tx.send_modify(|v| *v = v.wrapping_add(1));
}

fn append_log_sync(
    data_dir: &Path,
    logs: &StdMutex<VecDeque<String>>,
    snapshot_tx: &watch::Sender<u64>,
    line: &str,
) {
    let redacted = redact_sensitive_text(line);
    let _ = append_app_log_0600(data_dir, &redacted);
    {
        let mut guard = logs.lock().unwrap();
        if guard.len() >= MAX_LOG_LINES {
            guard.pop_front();
        }
        guard.push_back(redacted);
    }
    bump_snapshot(snapshot_tx);
}

async fn race_with_cancel<F, T>(fut: F, mut cancel_rx: watch::Receiver<bool>) -> Option<T>
where
    F: Future<Output = T>,
{
    if *cancel_rx.borrow() {
        return None;
    }
    let mut fut = std::pin::pin!(fut);
    let mut cancel_fut = std::pin::pin!(cancel_rx.wait_for(|&c| c));
    std::future::poll_fn(move |cx| {
        if let Poll::Ready(res) = fut.as_mut().poll(cx) {
            return Poll::Ready(Some(res));
        }
        if cancel_fut.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

/// Core session controller coordinating ledger files, `colab` CLI, SSH ControlMaster,
/// local `Host`-validated reverse proxy, and remote ComfyUI bootstrap.
pub struct SessionManager {
    data_dir: PathBuf,
    home_dir: Option<PathBuf>,
    path_env: Option<OsString>,
    proxy_https_hint: Option<OsString>,
    proxy_http_hint: Option<OsString>,
    session_name_override: Option<String>,
    local_proxy_port: u16,
    runner: Arc<dyn CommandRunner>,
    spawner: Arc<dyn TunnelSpawner>,
    drivemount_spawner: Arc<dyn DrivemountSpawner>,
    drive_action_tx: watch::Sender<Option<DriveUserAction>>,
    state: Arc<Mutex<InnerState>>,
    logs: Arc<StdMutex<VecDeque<String>>>,
    handles: Arc<Mutex<RuntimeHandles>>,
    start_in_flight: Arc<AtomicBool>,
    start_done_notify: Arc<Notify>,
    cancel_requested: Arc<AtomicBool>,
    cancel_tx: watch::Sender<bool>,
    intentional_stop: Arc<AtomicBool>,
    has_managed_session: Arc<AtomicBool>,
    snapshot_tx: watch::Sender<u64>,
}

impl SessionManager {
    pub fn new(data_dir: PathBuf, home_dir: Option<PathBuf>, path_env: Option<OsString>) -> Self {
        Self::with_deps(
            data_dir,
            home_dir,
            path_env,
            LOCAL_PROXY_PORT,
            Arc::new(TokioCommandRunner),
            Arc::new(TokioTunnelSpawner),
        )
    }

    pub fn with_deps(
        data_dir: PathBuf,
        home_dir: Option<PathBuf>,
        path_env: Option<OsString>,
        local_proxy_port: u16,
        runner: Arc<dyn CommandRunner>,
        spawner: Arc<dyn TunnelSpawner>,
    ) -> Self {
        let _ = ensure_data_dirs(&data_dir);
        let config: AppConfig = load_json_opt(&config_path(&data_dir))
            .ok()
            .flatten()
            .unwrap_or_default();
        let ledgers = load_ledgers_checked(&data_dir);
        let ledger_corrupted = ledgers.corruption_error.is_some();
        if let Some(ref err) = ledgers.corruption_error {
            Self::record_ui_error(&data_dir, err);
        }
        let has_managed_session = Arc::new(AtomicBool::new(
            ledgers.active.is_some() || ledgers.pending.is_some() || ledger_corrupted,
        ));
        let env = check_env(path_env.as_deref(), home_dir.as_deref());
        let (snapshot_tx, _) = watch::channel(0u64);
        let (cancel_tx, _) = watch::channel(false);
        let (drive_action_tx, _) = watch::channel(None);

        let inner = InnerState {
            config,
            phase: if ledger_corrupted {
                SessionPhase::Error
            } else {
                SessionPhase::Idle
            },
            step: None,
            env,
            account_email: None,
            auth_required: false,
            usage: None,
            active_session: ledgers.active,
            pending_session: ledgers.pending,
            ledger_corrupted,
            orphan_count: 0,
            startup_notice: if ledger_corrupted {
                Some(StartupNotice {
                    kind: "ledger_corrupted".to_string(),
                    message: "本地会话账本文件读取或解析失败，已保留原文件并开启退出保护，请人工核对或打开 Colab 网页确认。".to_string(),
                    session_name: None,
                })
            } else {
                None
            },
            warning_banner: None,
            last_error: ledgers.corruption_error,
            last_stop_report: None,
            tunnel_connected: false,
            drive_auth: None,
            telemetry: TelemetrySnapshot::default(),
            last_export_report: None,
            has_managed_session: has_managed_session.clone(),
        };

        Self {
            data_dir,
            home_dir,
            path_env,
            proxy_https_hint: None,
            proxy_http_hint: None,
            session_name_override: None,
            local_proxy_port,
            runner,
            spawner,
            drivemount_spawner: Arc::new(TokioDrivemountSpawner),
            drive_action_tx,
            state: Arc::new(Mutex::new(inner)),
            logs: Arc::new(StdMutex::new(VecDeque::with_capacity(MAX_LOG_LINES))),
            handles: Arc::new(Mutex::new(RuntimeHandles::default())),
            start_in_flight: Arc::new(AtomicBool::new(false)),
            start_done_notify: Arc::new(Notify::new()),
            cancel_requested: Arc::new(AtomicBool::new(false)),
            cancel_tx,
            intentional_stop: Arc::new(AtomicBool::new(false)),
            has_managed_session,
            snapshot_tx,
        }
    }

    pub fn with_drivemount_spawner(mut self, spawner: Arc<dyn DrivemountSpawner>) -> Self {
        self.drivemount_spawner = spawner;
        self
    }

    pub fn with_locale_hint(self, locale: Locale) -> Self {
        if load_json_opt::<AppConfig>(&config_path(&self.data_dir))
            .ok()
            .flatten()
            .is_none()
        {
            if let Ok(mut guard) = self.state.try_lock() {
                guard.config.locale = locale;
            }
        }
        self
    }

    pub fn with_proxy_hints(
        mut self,
        https_proxy: Option<OsString>,
        http_proxy: Option<OsString>,
    ) -> Self {
        self.proxy_https_hint = https_proxy;
        self.proxy_http_hint = http_proxy;
        self
    }

    pub fn with_session_name_override(mut self, name: String) -> Result<Self, CoreError> {
        validate_session_name(&name)?;
        self.session_name_override = Some(name);
        Ok(self)
    }

    async fn resolve_colab_envs(&self) -> Vec<(String, String)> {
        resolve_proxy_envs(
            self.runner.as_ref(),
            self.proxy_https_hint.as_deref(),
            self.proxy_http_hint.as_deref(),
        )
        .await
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.snapshot_tx.subscribe()
    }

    pub async fn snapshot(&self) -> AppSnapshot {
        let guard = self.state.lock().await;
        let logs = self.logs.lock().unwrap().iter().cloned().collect();
        Self::build_snapshot(&guard, logs)
    }

    pub async fn has_running_or_pending_session(&self) -> bool {
        let guard = self.state.lock().await;
        guard.sync_managed_flag();
        guard.active_session.is_some() || guard.pending_session.is_some() || guard.ledger_corrupted
    }

    pub fn has_running_or_pending_session_sync(&self) -> bool {
        self.has_managed_session.load(Ordering::SeqCst)
    }

    fn build_snapshot(s: &InnerState, logs: Vec<String>) -> AppSnapshot {
        s.sync_managed_flag();
        let table = message_table(s.config.locale)
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let canvas_url = if s.tunnel_connected {
            s.active_session
                .as_ref()
                .map(|a| format!("http://127.0.0.1:{}", a.local_port))
        } else {
            None
        };
        AppSnapshot {
            phase: s.phase,
            step: s.step,
            env: s.env.clone(),
            config: s.config.clone(),
            account_email: s.account_email.clone(),
            auth_required: s.auth_required,
            usage: s.usage.clone(),
            active_session: s.active_session.clone(),
            pending_session: s.pending_session.clone(),
            ledger_corrupted: s.ledger_corrupted,
            orphan_count: s.orphan_count,
            startup_notice: s.startup_notice.clone(),
            warning_banner: s.warning_banner.clone(),
            last_error: s.last_error.clone(),
            last_stop_report: s.last_stop_report.clone(),
            tunnel_connected: s.tunnel_connected,
            canvas_url,
            drive_auth: s.drive_auth.clone(),
            telemetry: s.telemetry.clone(),
            last_export_report: s.last_export_report.clone(),
            logs,
            i18n: table,
        }
    }

    pub async fn update_preferences(
        &self,
        locale: Option<Locale>,
        use_drive: Option<bool>,
        export_outputs_on_stop: Option<bool>,
        privacy_mask: Option<bool>,
        drive_export_timeout_secs: Option<u64>,
    ) -> Result<AppSnapshot, CoreError> {
        {
            let mut guard = self.state.lock().await;
            if let Some(l) = locale {
                guard.config.locale = l;
            }
            if let Some(d) = use_drive {
                guard.config.use_drive = d;
            }
            if let Some(e) = export_outputs_on_stop {
                guard.config.export_outputs_on_stop = e;
            }
            if let Some(p) = privacy_mask {
                guard.config.privacy_mask = p;
            }
            if let Some(secs) = drive_export_timeout_secs {
                guard.config.drive_export_timeout_secs = secs.clamp(15, 1800);
            }
            save_json_0600(&config_path(&self.data_dir), &guard.config)?;
        }
        self.notify_snapshot();
        Ok(self.snapshot().await)
    }

    pub async fn confirm_drive_auth(&self) -> AppSnapshot {
        self.drive_action_tx
            .send_replace(Some(DriveUserAction::Continue));
        {
            let mut guard = self.state.lock().await;
            if let Some(ref mut prompt) = guard.drive_auth {
                prompt.status = DriveAuthStatus::Continuing;
            }
        }
        self.notify_snapshot();
        self.snapshot().await
    }

    pub async fn retry_drive_mount(&self) -> AppSnapshot {
        self.drive_action_tx
            .send_replace(Some(DriveUserAction::Retry));
        self.notify_snapshot();
        self.snapshot().await
    }

    pub async fn skip_drive_for_session(&self) -> AppSnapshot {
        self.drive_action_tx
            .send_replace(Some(DriveUserAction::Skip));
        {
            let mut guard = self.state.lock().await;
            guard.drive_auth = None;
        }
        self.notify_snapshot();
        self.snapshot().await
    }

    pub async fn export_to_drive_now(
        &self,
        include_outputs: bool,
    ) -> Result<DriveExportReport, CoreError> {
        let ctl_sock = {
            let h = self.handles.lock().await;
            h.ctl_sock.clone()
        }
        .ok_or_else(|| CoreError::TunnelFailed {
            detail: "当前没有已连接的 SSH 隧道，无法导出到 Drive".to_string(),
        })?;
        let timeout_secs = {
            let guard = self.state.lock().await;
            guard.config.drive_export_timeout_secs.clamp(15, 1800)
        };
        let report = run_remote_export_and_flush(
            &ctl_sock,
            include_outputs,
            false,
            Duration::from_secs(timeout_secs),
            self.runner.as_ref(),
        )
        .await?;
        self.push_log(report.summary.clone()).await;
        {
            let mut guard = self.state.lock().await;
            guard.last_export_report = Some(report.clone());
        }
        self.notify_snapshot();
        Ok(report)
    }

    fn notify_snapshot(&self) {
        bump_snapshot(&self.snapshot_tx);
    }

    fn record_ui_error(data_dir: &Path, err: &UiError) {
        let _ = append_app_log_0600(data_dir, &format!("ERROR [{}] {}", err.code, err.message));
    }

    async fn push_log(&self, line: String) {
        append_log_sync(&self.data_dir, &self.logs, &self.snapshot_tx, &line);
    }

    fn make_line_callback(&self) -> LineCallback {
        let logs = self.logs.clone();
        let tx = self.snapshot_tx.clone();
        let data_dir = self.data_dir.clone();
        Arc::new(move |line: String| {
            append_log_sync(&data_dir, &logs, &tx, &line);
        })
    }

    fn require_colab_bin(&self, env: &EnvReport) -> Result<PathBuf, CoreError> {
        env.colab_path
            .as_ref()
            .map(PathBuf::from)
            .ok_or_else(|| CoreError::MissingBinary {
                name: "colab".to_string(),
            })
    }

    /// Reconciles `pending-session.json` and `active-session.json` against Colab on startup.
    pub async fn reconcile_on_startup(&self) -> AppSnapshot {
        // Guard against webview reloads wiping an active in-flight `colab new` (P2).
        if self.start_in_flight.load(Ordering::SeqCst) {
            return self.snapshot().await;
        }
        {
            let guard = self.state.lock().await;
            if matches!(
                guard.phase,
                SessionPhase::Starting | SessionPhase::Stopping | SessionPhase::Reconnecting
            ) {
                return Self::build_snapshot(
                    &guard,
                    self.logs.lock().unwrap().iter().cloned().collect(),
                );
            }
        }

        let env = check_env(self.path_env.as_deref(), self.home_dir.as_deref());
        {
            let mut guard = self.state.lock().await;
            guard.env = env.clone();
        }

        let Ok(colab_bin) = self.require_colab_bin(&env) else {
            self.notify_snapshot();
            return self.snapshot().await;
        };

        let account_res = self.refresh_account_and_sessions(&colab_bin).await;
        let proxy_envs = self.resolve_colab_envs().await;

        let ledgers = load_ledgers_checked(&self.data_dir);
        if let Some(ref err) = ledgers.corruption_error {
            Self::record_ui_error(&self.data_dir, err);
            let mut guard = self.state.lock().await;
            guard.ledger_corrupted = true;
            guard.pending_session = ledgers.pending.clone();
            guard.active_session = ledgers.active.clone();
            guard.phase = SessionPhase::Error;
            guard.last_error = Some(err.clone());
            guard.startup_notice = Some(StartupNotice {
                kind: "ledger_corrupted".to_string(),
                message: "本地会话账本文件读取或解析失败，已保留原文件，请核对后手工处理。"
                    .to_string(),
                session_name: None,
            });
            guard.sync_managed_flag();
            drop(guard);
            self.notify_snapshot();
            return self.snapshot().await;
        } else {
            let mut guard = self.state.lock().await;
            let was_corrupted = guard.ledger_corrupted;
            guard.ledger_corrupted = false;
            guard.pending_session = ledgers.pending.clone();
            guard.active_session = ledgers.active.clone();
            if was_corrupted && ledgers.pending.is_none() && ledgers.active.is_none() {
                guard.phase = SessionPhase::Idle;
                guard.step = None;
                guard.last_error = None;
                guard.startup_notice = None;
            }
            guard.sync_managed_flag();
        }

        if let Some(pend) = ledgers.pending {
            let Ok((ref email, verified_orphan_count)) = account_res else {
                let mut guard = self.state.lock().await;
                guard.startup_notice = Some(StartupNotice {
                    kind: "reconcile_deferred".to_string(),
                    message: format!(
                        "发现未完成开机记录 {}，但未能核验当前 colab 账号，已保留记录，请恢复网络/登录后点「重新对账」。",
                        pend.session_name
                    ),
                    session_name: Some(pend.session_name),
                });
                drop(guard);
                self.notify_snapshot();
                return self.snapshot().await;
            };
            if !email.eq_ignore_ascii_case(&pend.owner_email) {
                let mut guard = self.state.lock().await;
                guard.phase = SessionPhase::DisconnectedOrMissing;
                guard.startup_notice = Some(StartupNotice {
                    kind: "account_changed".to_string(),
                    message: format!(
                        "上次开机账号 {} 留有待核对会话 {}，当前已切换为 {}，请切回原账号后处理。",
                        pend.owner_email, pend.session_name, email
                    ),
                    session_name: Some(pend.session_name),
                });
                drop(guard);
                self.notify_snapshot();
                return self.snapshot().await;
            }

            match colab_cli::status(
                self.runner.as_ref(),
                &colab_bin,
                &pend.session_name,
                &proxy_envs,
                None,
            )
            .await
            {
                Ok(SessionStatusLookup::Present(entry)) => {
                    let promoted = ActiveSession {
                        session_name: pend.session_name.clone(),
                        owner_email: pend.owner_email.clone(),
                        endpoint_sha256: entry.endpoint_sha256,
                        gpu: pend.gpu,
                        high_mem: pend.high_mem,
                        hardware_summary: entry.hardware,
                        local_port: self.local_proxy_port,
                        drive_enabled: pend.drive_enabled,
                        created_at_unix: pend.created_at_unix,
                    };
                    let promotion_saved =
                        match StateLock::acquire(&self.data_dir, LOCK_TIMEOUT).await {
                            Ok(_lock) => {
                                if let Err(err) =
                                    save_json_0600(&active_session_path(&self.data_dir), &promoted)
                                {
                                    let ui_err = UiError::from(&err);
                                    Self::record_ui_error(&self.data_dir, &ui_err);
                                    let mut guard = self.state.lock().await;
                                    guard.last_error = Some(ui_err);
                                    false
                                } else {
                                    let _ = remove_if_exists(&pending_session_path(&self.data_dir));
                                    true
                                }
                            }
                            Err(err) => {
                                let ui_err = UiError::from(&err);
                                Self::record_ui_error(&self.data_dir, &ui_err);
                                let mut guard = self.state.lock().await;
                                guard.last_error = Some(ui_err);
                                false
                            }
                        };
                    if promotion_saved {
                        let mut guard = self.state.lock().await;
                        guard.pending_session = None;
                        guard.active_session = Some(promoted);
                        guard.sync_managed_flag();
                        guard.startup_notice = Some(StartupNotice {
                            kind: "pending_alive".to_string(),
                            message: if pend.cancel_requested {
                                format!(
                                    "上次开机期间已请求取消，云端会话 {} 仍在计费中，请立即点击「停机并核验停止计费」。",
                                    pend.session_name
                                )
                            } else {
                                format!(
                                    "上次开机期间 App 退出，云端会话 {} 仍在计费中，可直接重连或停机。",
                                    pend.session_name
                                )
                            },
                            session_name: Some(pend.session_name),
                        });
                    }
                }
                Ok(SessionStatusLookup::Missing) => {
                    if pend.uncertain_orphan {
                        match verified_orphan_count {
                            Some(0) => {
                                let _ = self
                                    .clear_verified_ledger(VerifiedLedgerTarget::Pending {
                                        session_name: pend.session_name.clone(),
                                        owner_email: pend.owner_email.clone(),
                                    })
                                    .await;
                                let mut guard = self.state.lock().await;
                                guard.warning_banner = None;
                                guard.startup_notice = Some(StartupNotice {
                                    kind: "pending_gone".to_string(),
                                    message: format!(
                                        "上次未完成的开机记录 {} 在云端已不存在且已核验无孤儿实例，已清理本地残留记录。",
                                        pend.session_name
                                    ),
                                    session_name: None,
                                });
                            }
                            Some(orphan_count) => {
                                let mut guard = self.state.lock().await;
                                guard.warning_banner = Some(format!(
                                    "上次申请会话 {} 异常返回，且账号下存在 {} 个孤儿实例 [?]，已保留本地记录以防误关，请先前往 Colab 网页确认。",
                                    pend.session_name, orphan_count
                                ));
                            }
                            None => {
                                let mut guard = self.state.lock().await;
                                guard.startup_notice = Some(StartupNotice {
                                    kind: "reconcile_deferred".to_string(),
                                    message: format!(
                                        "上次申请会话 {} 异常返回且本次未能拉取实例列表核验是否存在孤儿实例 [?]，已保留本地记录与退出保护，请点击「重新对账」。",
                                        pend.session_name
                                    ),
                                    session_name: Some(pend.session_name),
                                });
                            }
                        }
                    } else {
                        let _ = self
                            .clear_verified_ledger(VerifiedLedgerTarget::Pending {
                                session_name: pend.session_name.clone(),
                                owner_email: pend.owner_email.clone(),
                            })
                            .await;
                        let mut guard = self.state.lock().await;
                        guard.startup_notice = Some(StartupNotice {
                            kind: "pending_gone".to_string(),
                            message: format!(
                                "上次未完成的开机记录 {} 在云端已不存在，已清理本地残留记录。",
                                pend.session_name
                            ),
                            session_name: None,
                        });
                    }
                }
                Err(err) => {
                    let ui_err = UiError::from(&err);
                    Self::record_ui_error(&self.data_dir, &ui_err);
                    let mut guard = self.state.lock().await;
                    guard.last_error = Some(ui_err);
                }
            }
        } else if let Some(act) = ledgers.active {
            let Ok((ref email, _)) = account_res else {
                let mut guard = self.state.lock().await;
                guard.startup_notice = Some(StartupNotice {
                    kind: "reconcile_deferred".to_string(),
                    message: format!(
                        "发现活跃会话记录 {}，但未能核验当前 colab 账号，已保留记录。",
                        act.session_name
                    ),
                    session_name: Some(act.session_name),
                });
                drop(guard);
                self.notify_snapshot();
                return self.snapshot().await;
            };
            if !email.eq_ignore_ascii_case(&act.owner_email) {
                let mut guard = self.state.lock().await;
                guard.phase = SessionPhase::DisconnectedOrMissing;
                guard.startup_notice = Some(StartupNotice {
                    kind: "account_changed".to_string(),
                    message: format!(
                        "上次退出时账号 {} 仍有会话 {}，但当前 colab 已切换为 {}。",
                        act.owner_email, act.session_name, email
                    ),
                    session_name: Some(act.session_name),
                });
                drop(guard);
                self.notify_snapshot();
                return self.snapshot().await;
            }

            match colab_cli::status(
                self.runner.as_ref(),
                &colab_bin,
                &act.session_name,
                &proxy_envs,
                None,
            )
            .await
            {
                Ok(SessionStatusLookup::Present(_)) => {
                    let mut guard = self.state.lock().await;
                    guard.startup_notice = Some(StartupNotice {
                        kind: "active_alive".to_string(),
                        message: format!(
                            "上次退出时会话 {} 仍在云端运行并继续计费，可点击「重连本地隧道」或「停机」。",
                            act.session_name
                        ),
                        session_name: Some(act.session_name),
                    });
                }
                Ok(SessionStatusLookup::Missing) => {
                    let _ = self
                        .clear_verified_ledger(VerifiedLedgerTarget::Active {
                            session_name: act.session_name.clone(),
                            owner_email: act.owner_email.clone(),
                        })
                        .await;
                    let mut guard = self.state.lock().await;
                    guard.phase = SessionPhase::DisconnectedOrMissing;
                    guard.startup_notice = Some(StartupNotice {
                        kind: "active_gone".to_string(),
                        message: format!(
                            "上次记录的会话 {} 已不在云端运行（可能已被 Colab 回收），已清理本地记录。",
                            act.session_name
                        ),
                        session_name: None,
                    });
                }
                Err(err) => {
                    let ui_err = UiError::from(&err);
                    Self::record_ui_error(&self.data_dir, &ui_err);
                    let mut guard = self.state.lock().await;
                    guard.last_error = Some(ui_err);
                }
            }
        }

        self.notify_snapshot();
        self.snapshot().await
    }

    pub async fn refresh_status(&self) -> AppSnapshot {
        let env = check_env(self.path_env.as_deref(), self.home_dir.as_deref());
        {
            let mut guard = self.state.lock().await;
            guard.env = env.clone();
        }
        if let Ok(colab_bin) = self.require_colab_bin(&env) {
            let _ = self.refresh_account_and_sessions(&colab_bin).await;
        }
        self.notify_snapshot();
        self.snapshot().await
    }

    async fn refresh_account_and_sessions(
        &self,
        colab_bin: &Path,
    ) -> Result<(String, Option<usize>), CoreError> {
        let proxy_envs = self.resolve_colab_envs().await;
        let email =
            match colab_cli::whoami(self.runner.as_ref(), colab_bin, &proxy_envs, None).await {
                Ok(email) => {
                    let mut guard = self.state.lock().await;
                    guard.account_email = Some(email.clone());
                    guard.auth_required = false;
                    email
                }
                Err(CoreError::AuthRequired { detail }) => {
                    let ui_err = UiError::from(&CoreError::AuthRequired {
                        detail: detail.clone(),
                    });
                    Self::record_ui_error(&self.data_dir, &ui_err);
                    let mut guard = self.state.lock().await;
                    guard.account_email = None;
                    guard.auth_required = true;
                    guard.last_error = Some(ui_err);
                    return Err(CoreError::AuthRequired { detail });
                }
                Err(err) => {
                    let ui_err = UiError::from(&err);
                    Self::record_ui_error(&self.data_dir, &ui_err);
                    let mut guard = self.state.lock().await;
                    guard.last_error = Some(ui_err);
                    return Err(err);
                }
            };

        let usage_res = colab_cli::usage(self.runner.as_ref(), colab_bin, &proxy_envs, None).await;
        let sessions_res =
            colab_cli::sessions(self.runner.as_ref(), colab_bin, &proxy_envs, None).await;

        let mut guard = self.state.lock().await;
        guard.account_email = Some(email.clone());
        match usage_res {
            Ok(u) => {
                guard.telemetry.estimated_remaining_minutes =
                    estimated_remaining_minutes(u.current_balance, u.usage_rate_hourly);
                guard.usage = Some(u);
            }
            Err(err) => {
                let ui_err = UiError::from(&err);
                Self::record_ui_error(&self.data_dir, &ui_err);
                guard.last_error = Some(ui_err);
            }
        }
        let verified_orphan_count = match sessions_res {
            Ok(list) => {
                let count = list.iter().filter(|s| s.is_orphan).count();
                guard.orphan_count = count;
                Some(count)
            }
            Err(err) => {
                let ui_err = UiError::from(&err);
                Self::record_ui_error(&self.data_dir, &ui_err);
                guard.last_error = Some(ui_err);
                None
            }
        };
        Ok((email, verified_orphan_count))
    }

    async fn clear_verified_ledger(&self, target: VerifiedLedgerTarget) -> Result<(), CoreError> {
        let _lock = StateLock::acquire(&self.data_dir, LOCK_TIMEOUT).await?;
        let current = load_ledgers_checked(&self.data_dir);
        match &target {
            VerifiedLedgerTarget::Active {
                session_name,
                owner_email,
            } => {
                if !current.active_corrupted {
                    let matches_active = current.active.as_ref().is_none_or(|a| {
                        a.session_name == *session_name
                            && a.owner_email.eq_ignore_ascii_case(owner_email)
                    });
                    if matches_active {
                        remove_if_exists(&active_session_path(&self.data_dir))?;
                    }
                }
                if !current.pending_corrupted {
                    if let Some(ref pend) = current.pending {
                        if !pend.uncertain_orphan
                            && pend.session_name == *session_name
                            && pend.owner_email.eq_ignore_ascii_case(owner_email)
                        {
                            remove_if_exists(&pending_session_path(&self.data_dir))?;
                        }
                    }
                }
            }
            VerifiedLedgerTarget::Pending {
                session_name,
                owner_email,
            } => {
                if !current.pending_corrupted {
                    let matches_pending = current.pending.as_ref().is_none_or(|p| {
                        p.session_name == *session_name
                            && p.owner_email.eq_ignore_ascii_case(owner_email)
                    });
                    if matches_pending {
                        remove_if_exists(&pending_session_path(&self.data_dir))?;
                    }
                }
            }
        }

        let after = load_ledgers_checked(&self.data_dir);
        let mut guard = self.state.lock().await;
        guard.active_session = after.active;
        guard.pending_session = after.pending;
        guard.ledger_corrupted = after.corruption_error.is_some();
        if let Some(err) = after.corruption_error {
            guard.phase = SessionPhase::Error;
            guard.step = None;
            guard.last_error = Some(err);
        } else if guard.active_session.is_none() && guard.pending_session.is_none() {
            guard.phase = SessionPhase::Idle;
            guard.step = None;
        }
        guard.sync_managed_flag();
        Ok(())
    }

    pub async fn cancel_start(&self) -> AppSnapshot {
        if !self.start_in_flight.load(Ordering::SeqCst) {
            return self.snapshot().await;
        }
        self.cancel_requested.store(true, Ordering::SeqCst);
        self.cancel_tx.send_replace(true);
        self.push_log(
            "已请求取消开机：若正在等待 GPU 分配将待其返回后停机；若已在建隧道或部署则立即中断并停机核验。"
                .to_string(),
        )
        .await;
        if let Ok(_lock) = StateLock::acquire(&self.data_dir, LOCK_TIMEOUT).await {
            if !load_ledgers_checked(&self.data_dir).pending_corrupted {
                if let Ok(Some(mut pend)) =
                    load_json_opt::<PendingSession>(&pending_session_path(&self.data_dir))
                {
                    pend.cancel_requested = true;
                    let _ = save_json_0600(&pending_session_path(&self.data_dir), &pend);
                    let mut guard = self.state.lock().await;
                    guard.pending_session = Some(pend);
                    guard.sync_managed_flag();
                }
            }
        }
        self.notify_snapshot();
        self.snapshot().await
    }

    pub async fn start_session(
        &self,
        gpu: GpuTier,
        high_mem: bool,
        allow_zero_balance: bool,
    ) -> Result<AppSnapshot, CoreError> {
        if self
            .start_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(CoreError::StartInProgress);
        }
        self.cancel_requested.store(false, Ordering::SeqCst);
        self.cancel_tx.send_replace(false);
        self.intentional_stop.store(false, Ordering::SeqCst);

        let res = self
            .start_session_inner(gpu, high_mem, allow_zero_balance)
            .await;
        self.start_in_flight.store(false, Ordering::SeqCst);
        self.start_done_notify.notify_waiters();

        if let Err(ref err) = res {
            let mut guard = self.state.lock().await;
            match err {
                CoreError::ZeroBalanceBlocked { .. } => {
                    guard.phase = SessionPhase::Idle;
                    guard.step = None;
                }
                CoreError::StartCancelledAfterAllocation { .. }
                    if guard.active_session.is_none()
                        && guard.pending_session.is_none()
                        && !guard.ledger_corrupted =>
                {
                    let ui_err = UiError::from(err);
                    Self::record_ui_error(&self.data_dir, &ui_err);
                    guard.phase = SessionPhase::Idle;
                    guard.step = None;
                    guard.last_error = Some(ui_err);
                }
                _ => {
                    let ui_err = UiError::from(err);
                    Self::record_ui_error(&self.data_dir, &ui_err);
                    guard.phase = SessionPhase::Error;
                    guard.last_error = Some(ui_err);
                }
            }
            drop(guard);
            self.notify_snapshot();
        }
        res
    }

    #[allow(clippy::too_many_arguments)]
    async fn cancel_allocated_session(
        &self,
        colab_bin: &Path,
        session_name: &str,
        owner_email: &str,
        endpoint_sha256: &str,
        proxy_envs: &[(String, String)],
        cb: Option<LineCallback>,
        prior_ledger_err: Option<CoreError>,
        target: VerifiedLedgerTarget,
    ) -> Result<AppSnapshot, CoreError> {
        if let Some(ref err) = prior_ledger_err {
            let ui_err = UiError::from(err);
            Self::record_ui_error(&self.data_dir, &ui_err);
            self.push_log(format!(
                "本地活跃账本写入失败（已继续执行云端停机核验）：{err}"
            ))
            .await;
        }
        self.push_log("收到取消开机请求，立即执行停机与计费核验...".to_string())
            .await;
        let report = colab_cli::stop_and_verify(
            self.runner.as_ref(),
            colab_bin,
            session_name,
            owner_email,
            Some(endpoint_sha256),
            proxy_envs,
            cb,
        )
        .await?;

        let mut clear_err_note: Option<String> = None;
        if report.safe_to_clear_ledger {
            if let Err(err) = self.clear_verified_ledger(target).await {
                let ui_err = UiError::from(&err);
                Self::record_ui_error(&self.data_dir, &ui_err);
                clear_err_note = Some(err.to_string());
            }
        }

        {
            let mut guard = self.state.lock().await;
            if report.safe_to_clear_ledger
                && clear_err_note.is_none()
                && guard.active_session.is_none()
                && guard.pending_session.is_none()
                && !guard.ledger_corrupted
            {
                guard.phase = SessionPhase::Idle;
                guard.step = None;
            } else {
                guard.phase = SessionPhase::Error;
            }
            guard.sync_managed_flag();
            guard.usage = report.usage.clone();
            guard.last_stop_report = Some(report.clone());
        }

        let mut detail = report.summary;
        if let Some(err) = prior_ledger_err {
            detail = format!("{detail}（账本晋升写入失败：{err}）");
        }
        if let Some(err) = clear_err_note {
            detail = format!("{detail}（清理本地账本失败：{err}）");
        }

        Err(CoreError::StartCancelledAfterAllocation {
            session_name: session_name.to_string(),
            stop_verified: report.billing_verified_stopped,
            detail,
        })
    }

    async fn start_session_inner(
        &self,
        gpu: GpuTier,
        high_mem: bool,
        allow_zero_balance: bool,
    ) -> Result<AppSnapshot, CoreError> {
        let ledgers = load_ledgers_checked(&self.data_dir);
        if let Some(err) = ledgers.corruption_error {
            return Err(CoreError::SessionLedgerCorrupted {
                detail: err.message,
            });
        }
        let env = check_env(self.path_env.as_deref(), self.home_dir.as_deref());
        let colab_bin = self.require_colab_bin(&env)?;
        if !env.ssh_available {
            return Err(CoreError::MissingBinary {
                name: crate::locate::SSH_BIN.to_string(),
            });
        }
        if !env.ssh_keygen_available {
            return Err(CoreError::MissingBinary {
                name: crate::locate::SSH_KEYGEN_BIN.to_string(),
            });
        }

        let use_drive = {
            let mut guard = self.state.lock().await;
            guard.env = env;
            guard.phase = SessionPhase::Starting;
            guard.step = Some(StartStep::Preflight);
            guard.last_error = None;
            guard.warning_banner = None;
            guard.startup_notice = None;
            guard.drive_auth = None;
            guard.telemetry = TelemetrySnapshot::default();
            guard.config.default_gpu = gpu;
            guard.config.high_mem = high_mem && gpu.supports_high_mem();
            let _ = save_json_0600(&config_path(&self.data_dir), &guard.config);
            guard.config.use_drive
        };
        self.notify_snapshot();

        let cb = Some(self.make_line_callback());
        let proxy_envs = self.resolve_colab_envs().await;
        let owner_email =
            colab_cli::whoami(self.runner.as_ref(), &colab_bin, &proxy_envs, cb.clone()).await?;
        let usage =
            colab_cli::usage(self.runner.as_ref(), &colab_bin, &proxy_envs, cb.clone()).await?;

        {
            let mut guard = self.state.lock().await;
            guard.account_email = Some(owner_email.clone());
            guard.auth_required = false;
            guard.telemetry.estimated_remaining_minutes =
                estimated_remaining_minutes(usage.current_balance, usage.usage_rate_hourly);
            guard.usage = Some(usage.clone());
        }
        self.notify_snapshot();

        if usage.current_balance <= 0.0 && !allow_zero_balance {
            return Err(CoreError::ZeroBalanceBlocked {
                balance: usage.current_balance,
            });
        }

        let session_name = self
            .session_name_override
            .clone()
            .unwrap_or_else(|| default_session_name_for_email(&owner_email));
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Phase 1 ledger + collision check + baseline orphan snapshot inside state.lock
        let (already_exists, pre_orphan_hashes): (Option<SessionEntry>, Option<HashSet<String>>) = {
            let _lock = StateLock::acquire(&self.data_dir, LOCK_TIMEOUT).await?;
            let lookup = colab_cli::status(
                self.runner.as_ref(),
                &colab_bin,
                &session_name,
                &proxy_envs,
                cb.clone(),
            )
            .await?;
            match lookup {
                SessionStatusLookup::Present(entry) => (Some(entry), None),
                SessionStatusLookup::Missing => {
                    if self.cancel_requested.load(Ordering::SeqCst) {
                        return Err(CoreError::StartCancelledAfterAllocation {
                            session_name,
                            stop_verified: true,
                            detail: "尚未申请云端实例即已取消。".to_string(),
                        });
                    }
                    let baseline =
                        colab_cli::sessions(self.runner.as_ref(), &colab_bin, &proxy_envs, None)
                            .await
                            .ok()
                            .map(|list| {
                                list.into_iter()
                                    .filter(|s| s.is_orphan)
                                    .map(|s| s.endpoint_sha256)
                                    .collect::<HashSet<String>>()
                            });
                    if self.cancel_requested.load(Ordering::SeqCst) {
                        return Err(CoreError::StartCancelledAfterAllocation {
                            session_name,
                            stop_verified: true,
                            detail: "尚未申请云端实例即已取消。".to_string(),
                        });
                    }
                    let pend = PendingSession {
                        session_name: session_name.clone(),
                        owner_email: owner_email.clone(),
                        gpu,
                        high_mem: high_mem && gpu.supports_high_mem(),
                        cancel_requested: false,
                        uncertain_orphan: false,
                        drive_enabled: use_drive,
                        created_at_unix: now_unix,
                    };
                    save_json_0600(&pending_session_path(&self.data_dir), &pend)?;
                    let mut guard = self.state.lock().await;
                    guard.pending_session = Some(pend);
                    guard.sync_managed_flag();
                    (None, baseline)
                }
            }
        };

        let entry: SessionEntry = if let Some(existing) = already_exists {
            self.push_log(format!(
                "复用云端已存在会话：{session_name} ({})",
                existing.hardware
            ))
            .await;
            existing
        } else {
            if self.cancel_requested.load(Ordering::SeqCst) {
                let _ = self
                    .clear_verified_ledger(VerifiedLedgerTarget::Pending {
                        session_name: session_name.clone(),
                        owner_email: owner_email.clone(),
                    })
                    .await;
                return Err(CoreError::StartCancelledAfterAllocation {
                    session_name,
                    stop_verified: true,
                    detail: "尚未申请云端实例即已取消。".to_string(),
                });
            }
            {
                let mut guard = self.state.lock().await;
                guard.step = Some(StartStep::CreatingSession);
            }
            self.notify_snapshot();
            self.push_log(format!(
                "正在申请 Colab 会话 {session_name} (GPU={})...",
                gpu.as_cli_arg()
            ))
            .await;

            // Never kill `colab new`; wait for it to return and reconcile on uncertain failures.
            let new_res = colab_cli::new_session(
                self.runner.as_ref(),
                &colab_bin,
                &session_name,
                gpu,
                high_mem,
                &proxy_envs,
                cb.clone(),
            )
            .await;

            if let Err(err) = new_res {
                if !err.is_uncertain_allocation_failure() {
                    if let Ok(_lock) = StateLock::acquire(&self.data_dir, LOCK_TIMEOUT).await {
                        let _ = remove_if_exists(&pending_session_path(&self.data_dir));
                    }
                    let mut guard = self.state.lock().await;
                    guard.pending_session = None;
                    guard.sync_managed_flag();
                    return Err(err);
                }

                // Uncertain failure (timeout / 5xx): reconcile status & check for newly created [?] orphans (H4).
                let post_status = colab_cli::status(
                    self.runner.as_ref(),
                    &colab_bin,
                    &session_name,
                    &proxy_envs,
                    cb.clone(),
                )
                .await;
                if let Ok(SessionStatusLookup::Present(alive_entry)) = post_status {
                    let promoted = ActiveSession {
                        session_name: session_name.clone(),
                        owner_email: owner_email.clone(),
                        endpoint_sha256: alive_entry.endpoint_sha256.clone(),
                        gpu,
                        high_mem: high_mem && gpu.supports_high_mem(),
                        hardware_summary: alive_entry.hardware.clone(),
                        local_port: self.local_proxy_port,
                        drive_enabled: use_drive,
                        created_at_unix: now_unix,
                    };
                    let promotion_err: Option<CoreError> =
                        match StateLock::acquire(&self.data_dir, LOCK_TIMEOUT).await {
                            Ok(_lock) => {
                                match save_json_0600(
                                    &active_session_path(&self.data_dir),
                                    &promoted,
                                ) {
                                    Ok(()) => {
                                        let _ =
                                            remove_if_exists(&pending_session_path(&self.data_dir));
                                        None
                                    }
                                    Err(err) => Some(err),
                                }
                            }
                            Err(err) => Some(err),
                        };
                    {
                        let mut guard = self.state.lock().await;
                        if promotion_err.is_none() {
                            guard.pending_session = None;
                        }
                        guard.active_session = Some(promoted);
                        guard.sync_managed_flag();
                    }
                    if self.cancel_requested.load(Ordering::SeqCst) {
                        return self
                            .cancel_allocated_session(
                                &colab_bin,
                                &session_name,
                                &owner_email,
                                &alive_entry.endpoint_sha256,
                                &proxy_envs,
                                cb,
                                promotion_err,
                                VerifiedLedgerTarget::Active {
                                    session_name: session_name.clone(),
                                    owner_email: owner_email.clone(),
                                },
                            )
                            .await;
                    }
                    return Err(CoreError::CapacityOrSessionLimit {
                        detail: format!(
                            "colab new 报错但云端会话 {} 已存活，已转入活跃记录，请直接重连或停机（原错误：{err}）",
                            session_name
                        ),
                        uncertain: true,
                    });
                }

                let post_sessions =
                    colab_cli::sessions(self.runner.as_ref(), &colab_bin, &proxy_envs, cb.clone())
                        .await;
                let (new_orphan_detected, orphan_total) = match &post_sessions {
                    Ok(list) => {
                        let current_orphans: Vec<&SessionEntry> =
                            list.iter().filter(|s| s.is_orphan).collect();
                        let new_detected = match &pre_orphan_hashes {
                            Some(pre) => current_orphans
                                .iter()
                                .any(|s| !pre.contains(&s.endpoint_sha256)),
                            None => !current_orphans.is_empty(),
                        };
                        (new_detected, current_orphans.len())
                    }
                    Err(_) => (true, 0),
                };

                let status_missing = matches!(post_status, Ok(SessionStatusLookup::Missing));
                if status_missing && post_sessions.is_ok() && !new_orphan_detected {
                    if let Ok(_lock) = StateLock::acquire(&self.data_dir, LOCK_TIMEOUT).await {
                        let _ = remove_if_exists(&pending_session_path(&self.data_dir));
                    }
                    let mut guard = self.state.lock().await;
                    guard.pending_session = None;
                    guard.sync_managed_flag();
                } else {
                    if let Ok(_lock) = StateLock::acquire(&self.data_dir, LOCK_TIMEOUT).await {
                        if let Ok(Some(mut pend)) =
                            load_json_opt::<PendingSession>(&pending_session_path(&self.data_dir))
                        {
                            pend.uncertain_orphan = true;
                            let _ = save_json_0600(&pending_session_path(&self.data_dir), &pend);
                            let mut guard = self.state.lock().await;
                            guard.pending_session = Some(pend);
                            guard.sync_managed_flag();
                        }
                    }
                    let mut guard = self.state.lock().await;
                    if orphan_total > 0 {
                        guard.orphan_count = orphan_total;
                    }
                    guard.warning_banner = Some(
                        "colab new 异常/超时后未能确认资源已释放（可能已产生 [?] 孤儿计费实例），已保留开机记录；请点击「打开 Colab 管理会话」核对并手动终止后再点「重新对账」。"
                            .to_string(),
                    );
                }
                return Err(err);
            }

            let status_lookup = colab_cli::status(
                self.runner.as_ref(),
                &colab_bin,
                &session_name,
                &proxy_envs,
                cb.clone(),
            )
            .await?;
            match status_lookup {
                SessionStatusLookup::Present(e) => e,
                SessionStatusLookup::Missing => {
                    if let Ok(_lock) = StateLock::acquire(&self.data_dir, LOCK_TIMEOUT).await {
                        let _ = remove_if_exists(&pending_session_path(&self.data_dir));
                    }
                    let mut guard = self.state.lock().await;
                    guard.pending_session = None;
                    guard.sync_managed_flag();
                    return Err(CoreError::SessionMissing { session_name });
                }
            }
        };

        // Promote pending -> active BEFORE checking cancel so H3 always keeps a managed ledger if stop fails,
        // without letting a disk-write error skip stopping the just-allocated GPU (D3).
        let active = ActiveSession {
            session_name: session_name.clone(),
            owner_email: owner_email.clone(),
            endpoint_sha256: entry.endpoint_sha256.clone(),
            gpu,
            high_mem: high_mem && gpu.supports_high_mem(),
            hardware_summary: entry.hardware,
            local_port: self.local_proxy_port,
            drive_enabled: use_drive,
            created_at_unix: now_unix,
        };
        let promotion_err: Option<CoreError> =
            match StateLock::acquire(&self.data_dir, LOCK_TIMEOUT).await {
                Ok(_lock) => match save_json_0600(&active_session_path(&self.data_dir), &active) {
                    Ok(()) => {
                        let _ = remove_if_exists(&pending_session_path(&self.data_dir));
                        None
                    }
                    Err(err) => Some(err),
                },
                Err(err) => Some(err),
            };
        {
            let mut guard = self.state.lock().await;
            if promotion_err.is_none() {
                guard.pending_session = None;
            }
            guard.active_session = Some(active.clone());
            guard.sync_managed_flag();
        }
        self.notify_snapshot();

        if self.cancel_requested.load(Ordering::SeqCst) {
            return self
                .cancel_allocated_session(
                    &colab_bin,
                    &session_name,
                    &owner_email,
                    &entry.endpoint_sha256,
                    &proxy_envs,
                    cb,
                    promotion_err,
                    VerifiedLedgerTarget::Active {
                        session_name: session_name.clone(),
                        owner_email: owner_email.clone(),
                    },
                )
                .await;
        }
        if let Some(err) = promotion_err {
            return Err(err);
        }

        let cancel_rx = self.cancel_tx.subscribe();
        match self
            .open_tunnel_and_bootstrap(&colab_bin, &active, true, Some(cancel_rx))
            .await
        {
            Ok(()) => {}
            Err(CoreError::StartCancelledAfterAllocation { .. }) => {
                return self
                    .cancel_allocated_session(
                        &colab_bin,
                        &session_name,
                        &owner_email,
                        &entry.endpoint_sha256,
                        &proxy_envs,
                        cb,
                        None,
                        VerifiedLedgerTarget::Active {
                            session_name: session_name.clone(),
                            owner_email: owner_email.clone(),
                        },
                    )
                    .await;
            }
            Err(err) => return Err(err),
        }

        let _ = self.refresh_account_and_sessions(&colab_bin).await;
        self.notify_snapshot();
        Ok(self.snapshot().await)
    }

    pub async fn reconnect_tunnel(&self) -> Result<AppSnapshot, CoreError> {
        let res = self.reconnect_tunnel_inner().await;
        if let Err(ref err) = res {
            let ui_err = UiError::from(err);
            Self::record_ui_error(&self.data_dir, &ui_err);
            let mut guard = self.state.lock().await;
            guard.tunnel_connected = false;
            guard.step = None;
            guard.phase = match err {
                CoreError::SessionMissing { .. }
                | CoreError::AccountMismatch { .. }
                | CoreError::SessionLedgerMismatch { .. }
                | CoreError::AuthRequired { .. } => SessionPhase::DisconnectedOrMissing,
                _ => SessionPhase::Error,
            };
            guard.last_error = Some(ui_err);
            drop(guard);
            self.notify_snapshot();
        }
        res
    }

    async fn reconnect_tunnel_inner(&self) -> Result<AppSnapshot, CoreError> {
        let env = check_env(self.path_env.as_deref(), self.home_dir.as_deref());
        let colab_bin = self.require_colab_bin(&env)?;
        let active = {
            let guard = self.state.lock().await;
            guard.active_session.clone()
        }
        .ok_or_else(|| CoreError::SessionLedgerMismatch {
            detail: "没有可重连的本地会话记录".to_string(),
        })?;

        self.intentional_stop.store(false, Ordering::SeqCst);
        {
            let mut guard = self.state.lock().await;
            guard.phase = SessionPhase::Reconnecting;
            guard.step = Some(StartStep::OpeningTunnel);
            guard.last_error = None;
        }
        self.notify_snapshot();

        self.open_tunnel_and_bootstrap(&colab_bin, &active, true, None)
            .await?;
        let _ = self.refresh_account_and_sessions(&colab_bin).await;
        self.notify_snapshot();
        Ok(self.snapshot().await)
    }

    async fn open_tunnel_and_bootstrap(
        &self,
        colab_bin: &Path,
        active: &ActiveSession,
        run_bootstrap: bool,
        cancel_rx: Option<watch::Receiver<bool>>,
    ) -> Result<(), CoreError> {
        self.teardown_local_handles().await;

        {
            let mut guard = self.state.lock().await;
            guard.step = Some(StartStep::OpeningTunnel);
        }
        self.notify_snapshot();

        let cb = Some(self.make_line_callback());
        let proxy_envs = self.resolve_colab_envs().await;
        let master_fut = start_master_and_verify(
            &self.data_dir,
            colab_bin,
            active,
            self.runner.as_ref(),
            self.spawner.as_ref(),
            &proxy_envs,
            cb.clone(),
        );
        let started = match cancel_rx.clone() {
            Some(rx) => match race_with_cancel(master_fut, rx).await {
                Some(res) => res?,
                None => {
                    if let Ok(ctl) = ctl_socket_path(&self.data_dir, &active.session_name) {
                        let _ = remove_if_exists(&ctl);
                    }
                    if let Ok(sock) = crate::config::comfy_socket_path(&self.data_dir) {
                        let _ = remove_if_exists(&sock);
                    }
                    return Err(CoreError::StartCancelledAfterAllocation {
                        session_name: active.session_name.clone(),
                        stop_verified: false,
                        detail: "已在建立 SSH 隧道阶段请求取消".to_string(),
                    });
                }
            },
            None => master_fut.await?,
        };

        if let Some(warn) = started.session_count_warning {
            let mut guard = self.state.lock().await;
            guard.warning_banner = Some(warn);
        }

        let proxy = match start_local_proxy(
            active.local_port,
            started.comfy_sock.clone(),
            self.runner.as_ref(),
        )
        .await
        {
            Ok(p) => p,
            Err(err) => {
                let mut proc_box = started.process;
                stop_master_tunnel(
                    &started.ctl_sock,
                    &started.comfy_sock,
                    Some(&mut proc_box),
                    self.runner.as_ref(),
                )
                .await;
                return Err(err);
            }
        };

        let mut effective_use_drive = active.drive_enabled;
        if run_bootstrap && active.drive_enabled {
            match self
                .ensure_drive_mounted_with_prompt(
                    colab_bin,
                    &started.ctl_sock,
                    &active.session_name,
                    &proxy_envs,
                    cancel_rx.clone(),
                )
                .await
            {
                Ok(mounted) => {
                    effective_use_drive = mounted;
                    if !mounted {
                        let mut updated = active.clone();
                        updated.drive_enabled = false;
                        match StateLock::acquire(&self.data_dir, LOCK_TIMEOUT).await {
                            Ok(_lock) => {
                                if let Err(err) =
                                    save_json_0600(&active_session_path(&self.data_dir), &updated)
                                {
                                    self.push_log(format!(
                                        "告警：更新本地账本 drive_enabled=false 失败（{err}）"
                                    ))
                                    .await;
                                }
                            }
                            Err(err) => {
                                self.push_log(format!(
                                    "告警：获取本地账本锁失败，未能写入 drive_enabled=false（{err}）"
                                ))
                                .await;
                            }
                        }
                        let mut guard = self.state.lock().await;
                        guard.active_session = Some(updated);
                        guard.warning_banner = Some(
                            "已跳过 Google Drive 挂载，本次会话以无持久化（--no-drive）模式继续。"
                                .to_string(),
                        );
                    }
                }
                Err(err) => {
                    proxy.shutdown().await;
                    let mut proc_box = started.process;
                    stop_master_tunnel(
                        &started.ctl_sock,
                        &started.comfy_sock,
                        Some(&mut proc_box),
                        self.runner.as_ref(),
                    )
                    .await;
                    return Err(err);
                }
            }
        }

        if run_bootstrap {
            {
                let mut guard = self.state.lock().await;
                guard.step = Some(StartStep::Bootstrapping);
            }
            self.notify_snapshot();
            self.push_log("正在通过复用 SSH 通道检查并启动远端 ComfyUI...".to_string())
                .await;

            let bootstrap_fut = run_remote_bootstrap(
                &started.ctl_sock,
                REMOTE_COMFY_PORT,
                effective_use_drive,
                self.runner.as_ref(),
                cb,
            );
            let boot_res = match cancel_rx {
                Some(rx) => match race_with_cancel(bootstrap_fut, rx).await {
                    Some(r) => r,
                    None => Err(CoreError::StartCancelledAfterAllocation {
                        session_name: active.session_name.clone(),
                        stop_verified: false,
                        detail: "已在远端部署 ComfyUI 阶段请求取消".to_string(),
                    }),
                },
                None => bootstrap_fut.await,
            };

            match boot_res {
                Ok(BootstrapOutcome {
                    cors_header_busy_warning,
                    layout_changed_busy_warning,
                    failed_custom_nodes,
                    constraint_fallback_warning,
                }) => {
                    let mut warnings = Vec::new();
                    if cors_header_busy_warning {
                        warnings.push("检测到旧 ComfyUI 进程带 --enable-cors-header 且队列非空，请在任务结束后重启以恢复同源保护。".to_string());
                    }
                    if layout_changed_busy_warning {
                        warnings.push("Drive 持久化目录或 extra_model_paths.yaml 已变更，但当前 ComfyUI 队列非空，请在任务结束后重启以生效。".to_string());
                    }
                    if !failed_custom_nodes.is_empty() {
                        warnings.push(format!(
                            "部分自定义节点恢复失败（已跳过）：{}。",
                            failed_custom_nodes.join(", ")
                        ));
                    }
                    if constraint_fallback_warning {
                        warnings.push("部分自定义节点依赖与 Colab 非核心约束冲突，已自动降级为仅锁定 PyTorch/CUDA 约束继续安装。".to_string());
                    }
                    if !warnings.is_empty() {
                        let joined = warnings.join(" ");
                        self.push_log(format!("告警：{joined}")).await;
                        let mut guard = self.state.lock().await;
                        if let Some(prev) = guard.warning_banner.take() {
                            guard.warning_banner = Some(format!("{prev} {joined}"));
                        } else {
                            guard.warning_banner = Some(joined);
                        }
                    }
                }
                Err(err) => {
                    proxy.shutdown().await;
                    let mut proc_box = started.process;
                    stop_master_tunnel(
                        &started.ctl_sock,
                        &started.comfy_sock,
                        Some(&mut proc_box),
                        self.runner.as_ref(),
                    )
                    .await;
                    return Err(err);
                }
            }
        }

        {
            let mut handles = self.handles.lock().await;
            handles.proxy = Some(proxy);
            handles.tunnel_proc = Some(started.process);
            handles.ctl_sock = Some(started.ctl_sock);
            handles.comfy_sock = Some(started.comfy_sock);
        }

        {
            let mut guard = self.state.lock().await;
            guard.phase = SessionPhase::Ready;
            guard.step = Some(StartStep::Ready);
            guard.tunnel_connected = true;
            guard.startup_notice = None;
        }
        self.notify_snapshot();
        self.spawn_tunnel_supervisor();
        self.spawn_telemetry_supervisor();
        Ok(())
    }

    async fn ensure_drive_mounted_with_prompt(
        &self,
        colab_bin: &Path,
        ctl_sock: &Path,
        session_name: &str,
        proxy_envs: &[(String, String)],
        cancel_rx: Option<watch::Receiver<bool>>,
    ) -> Result<bool, CoreError> {
        // P1-11: verify `colab whoami` before invoking `colab drivemount`.
        let _ = colab_cli::whoami(self.runner.as_ref(), colab_bin, proxy_envs, None).await?;
        match check_drive_mounted_mux(ctl_sock, self.runner.as_ref()).await? {
            MountProbe::MountedHealthy => {
                self.push_log("Google Drive 已挂载。".to_string()).await;
                return Ok(true);
            }
            MountProbe::MountedUnhealthy => {
                self.push_log(
                    "告警：/content/drive 已出现在挂载表中，但 MyDrive 不可访问。".to_string(),
                )
                .await;
                return Err(CoreError::BootstrapFailed {
                    detail: "Google Drive 挂载点已存在但状态异常，请先停机或检查 /content/drive"
                        .to_string(),
                });
            }
            MountProbe::NotMounted => {}
        }

        self.push_log("正在挂载 Google Drive（零 Token 上云）...".to_string())
            .await;
        let args = drivemount_args(session_name)?;
        let mut envs = proxy_envs.to_vec();
        envs.push(("PYTHONUNBUFFERED".to_string(), "1".to_string()));

        for attempt in 1..=MAX_DRIVE_MOUNT_ATTEMPTS {
            self.drive_action_tx.send_replace(None);
            let mut proc = self.drivemount_spawner.spawn(colab_bin, &args, &envs)?;
            let mut action_rx = self.drive_action_tx.subscribe();
            let mut cancel_sub = cancel_rx.clone();
            let started_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let deadline = tokio::time::Instant::now() + DRIVE_AUTH_TIMEOUT;
            let mut auth_url: Option<String> = None;
            let mut skipped = false;

            loop {
                let is_cancelled = cancel_sub.as_ref().is_some_and(|r| *r.borrow());
                if is_cancelled {
                    let _ = proc.kill().await;
                    let mut guard = self.state.lock().await;
                    guard.drive_auth = None;
                    return Err(CoreError::StartCancelledAfterAllocation {
                        session_name: session_name.to_string(),
                        stop_verified: false,
                        detail: "已在挂载 Google Drive 阶段请求取消".to_string(),
                    });
                }

                enum DriveLoopEvent {
                    Line(Option<DrivemountLine>),
                    Action,
                    Cancelled,
                    Timeout,
                }

                let ev = {
                    let mut line_fut = std::pin::pin!(proc.next_line());
                    let mut action_fut = std::pin::pin!(action_rx.changed());
                    let mut sleep_fut = std::pin::pin!(tokio::time::sleep_until(deadline));
                    let mut cancel_fut = std::pin::pin!(async {
                        if let Some(ref mut crx) = cancel_sub {
                            let _ = crx.wait_for(|&c| c).await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    });
                    std::future::poll_fn(|cx| {
                        if cancel_fut.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(DriveLoopEvent::Cancelled);
                        }
                        if let Poll::Ready(line_opt) = line_fut.as_mut().poll(cx) {
                            return Poll::Ready(DriveLoopEvent::Line(line_opt));
                        }
                        if action_fut.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(DriveLoopEvent::Action);
                        }
                        if sleep_fut.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(DriveLoopEvent::Timeout);
                        }
                        Poll::Pending
                    })
                    .await
                };

                match ev {
                    DriveLoopEvent::Line(Some(line)) => {
                        self.push_log(line.redacted.clone()).await;
                        if let Some(u) = line.auth_url {
                            auth_url = Some(u);
                        }
                        if auth_url.is_some() || line.is_auth_prompt {
                            let mut guard = self.state.lock().await;
                            guard.drive_auth = Some(DriveAuthPrompt {
                                auth_url: auth_url.clone(),
                                started_at_unix: started_unix,
                                deadline_unix: started_unix + DRIVE_AUTH_TIMEOUT.as_secs(),
                                grace_deadline_unix: None,
                                attempt,
                                max_attempts: MAX_DRIVE_MOUNT_ATTEMPTS,
                                status: DriveAuthStatus::WaitingUser,
                                message: None,
                            });
                            drop(guard);
                            self.notify_snapshot();
                        }
                    }
                    DriveLoopEvent::Line(None) => break,
                    DriveLoopEvent::Action => {
                        let act = *action_rx.borrow_and_update();
                        match act {
                            Some(DriveUserAction::Continue) => {
                                let _ = proc.send_newline().await;
                            }
                            Some(DriveUserAction::Skip) => {
                                let _ = proc.kill().await;
                                skipped = true;
                                break;
                            }
                            _ => {}
                        }
                    }
                    DriveLoopEvent::Cancelled => {
                        let _ = proc.kill().await;
                        let mut guard = self.state.lock().await;
                        guard.drive_auth = None;
                        return Err(CoreError::StartCancelledAfterAllocation {
                            session_name: session_name.to_string(),
                            stop_verified: false,
                            detail: "已在挂载 Google Drive 阶段请求取消".to_string(),
                        });
                    }
                    DriveLoopEvent::Timeout => {
                        let _ = proc.kill().await;
                        break;
                    }
                }
            }

            if skipped {
                let mut guard = self.state.lock().await;
                guard.drive_auth = None;
                self.notify_snapshot();
                return Ok(false);
            }

            let _ = proc.wait().await;
            if matches!(
                check_drive_mounted_mux(ctl_sock, self.runner.as_ref()).await,
                Ok(MountProbe::MountedHealthy)
            ) {
                let mut guard = self.state.lock().await;
                guard.drive_auth = None;
                drop(guard);
                self.notify_snapshot();
                self.push_log("Google Drive 挂载验证成功（DRIVE_MOUNTED=1）。".to_string())
                    .await;
                return Ok(true);
            }

            let grace_deadline_unix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                + DRIVE_FAILED_GRACE_TIMEOUT.as_secs();
            let msg = if attempt < MAX_DRIVE_MOUNT_ATTEMPTS {
                format!(
                    "Google Drive 第 {attempt}/{MAX_DRIVE_MOUNT_ATTEMPTS} 次挂载未成功，可点击「重试挂载」、「跳过 Drive」或「取消并停机」（120s 后将自动降级为无 Drive 模式）。"
                )
            } else {
                format!(
                    "Google Drive 已达最大重试次数（{MAX_DRIVE_MOUNT_ATTEMPTS}/{MAX_DRIVE_MOUNT_ATTEMPTS}），120s 后将自动降级为无 Drive 模式，也可立即跳过或取消并停机。"
                )
            };
            {
                let mut guard = self.state.lock().await;
                guard.drive_auth = Some(DriveAuthPrompt {
                    auth_url: auth_url.clone(),
                    started_at_unix: started_unix,
                    deadline_unix: started_unix + DRIVE_AUTH_TIMEOUT.as_secs(),
                    grace_deadline_unix: Some(grace_deadline_unix),
                    attempt,
                    max_attempts: MAX_DRIVE_MOUNT_ATTEMPTS,
                    status: DriveAuthStatus::Failed,
                    message: Some(msg),
                });
            }
            self.notify_snapshot();

            // Wait for user decision: Retry (if attempts left), Skip, Cancel, or 120s grace timeout (M2-A).
            self.drive_action_tx.send_replace(None);
            let mut wait_action = self.drive_action_tx.subscribe();
            let grace_deadline = tokio::time::Instant::now() + DRIVE_FAILED_GRACE_TIMEOUT;
            loop {
                enum DriveRetryEvent {
                    Action,
                    Cancelled,
                    GraceTimeout,
                }
                let ev = {
                    let mut action_fut = std::pin::pin!(wait_action.changed());
                    let mut grace_fut = std::pin::pin!(tokio::time::sleep_until(grace_deadline));
                    let mut cancel_fut = std::pin::pin!(async {
                        if let Some(ref mut crx) = cancel_sub {
                            let _ = crx.wait_for(|&c| c).await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    });
                    std::future::poll_fn(|cx| {
                        if cancel_fut.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(DriveRetryEvent::Cancelled);
                        }
                        if action_fut.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(DriveRetryEvent::Action);
                        }
                        if grace_fut.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(DriveRetryEvent::GraceTimeout);
                        }
                        Poll::Pending
                    })
                    .await
                };
                match ev {
                    DriveRetryEvent::Action => {
                        let act = *wait_action.borrow_and_update();
                        match act {
                            Some(DriveUserAction::Retry) if attempt < MAX_DRIVE_MOUNT_ATTEMPTS => {
                                break;
                            }
                            Some(DriveUserAction::Skip) => {
                                let mut guard = self.state.lock().await;
                                guard.drive_auth = None;
                                self.notify_snapshot();
                                return Ok(false);
                            }
                            _ => {}
                        }
                    }
                    DriveRetryEvent::GraceTimeout => {
                        self.push_log(
                            "Google Drive 挂载宽限期已到（120s），自动降级为无 Drive 模式继续启动。"
                                .to_string(),
                        )
                        .await;
                        let mut guard = self.state.lock().await;
                        guard.drive_auth = None;
                        self.notify_snapshot();
                        return Ok(false);
                    }
                    DriveRetryEvent::Cancelled => {
                        let mut guard = self.state.lock().await;
                        guard.drive_auth = None;
                        return Err(CoreError::StartCancelledAfterAllocation {
                            session_name: session_name.to_string(),
                            stop_verified: false,
                            detail: "已在挂载 Google Drive 失败后请求取消".to_string(),
                        });
                    }
                }
            }
        }

        Err(CoreError::BootstrapFailed {
            detail: "Google Drive 挂载失败".to_string(),
        })
    }

    fn spawn_telemetry_supervisor(&self) {
        let local_port = self.local_proxy_port;
        let home_dir = self.home_dir.clone();
        let path_env = self.path_env.clone();
        let proxy_https_hint = self.proxy_https_hint.clone();
        let proxy_http_hint = self.proxy_http_hint.clone();
        let runner = self.runner.clone();
        let state = self.state.clone();
        let handles = self.handles.clone();
        let intentional_stop = self.intentional_stop.clone();
        let snapshot_tx = self.snapshot_tx.clone();

        let task = tokio::spawn(async move {
            let mut tick: u64 = 0;
            let mut interval = tokio::time::interval(TELEMETRY_LOCAL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                if intentional_stop.load(Ordering::SeqCst) {
                    break;
                }
                let (tunnel_connected, ctl_sock) = {
                    let guard = state.lock().await;
                    let tc = guard.tunnel_connected;
                    drop(guard);
                    let ctl = handles.lock().await.ctl_sock.clone();
                    (tc, ctl)
                };
                if !tunnel_connected {
                    tick = tick.wrapping_add(1);
                    continue;
                }

                // Tier 1 (every 3s): local ComfyUI HTTP /system_stats + /queue over loopback proxy
                if local_port != 0 {
                    let stats_res = fetch_local_comfy_json(local_port, "/system_stats").await;
                    let queue_res = fetch_local_comfy_json(local_port, "/queue").await;
                    let mut guard = state.lock().await;
                    if let Ok(stats_body) = stats_res {
                        if let Ok(st) = parse_comfy_system_stats(&stats_body) {
                            if st.gpu_name.is_some() {
                                guard.telemetry.gpu_name = st.gpu_name;
                            }
                            if st.vram_used_bytes.is_some() {
                                guard.telemetry.vram_used_bytes = st.vram_used_bytes;
                            }
                            if st.vram_total_bytes.is_some() {
                                guard.telemetry.vram_total_bytes = st.vram_total_bytes;
                            }
                            guard.telemetry.ram_used_bytes = st.ram_used_bytes;
                            guard.telemetry.ram_total_bytes = st.ram_total_bytes;
                        }
                    }
                    if let Ok(queue_body) = queue_res {
                        if let Ok((running, pending)) = parse_comfy_queue(&queue_body) {
                            guard.telemetry.queue_running = running;
                            guard.telemetry.queue_pending = pending;
                        }
                    }
                }

                // Tier 2 (every ~21s): single probe over existing ControlMaster socket (zero extra SSH)
                if tick.is_multiple_of(7) {
                    if let Some(ref ctl) = ctl_sock {
                        if let Ok(remote) = fetch_remote_probe(ctl, runner.as_ref()).await {
                            let mut guard = state.lock().await;
                            guard.telemetry.gpu_util_pct = remote.gpu_util_pct;
                            guard.telemetry.gpu_temp_c = remote.gpu_temp_c;
                            if guard.telemetry.vram_used_bytes.is_none() {
                                guard.telemetry.vram_used_bytes = remote.vram_used_bytes;
                            }
                            if guard.telemetry.vram_total_bytes.is_none() {
                                guard.telemetry.vram_total_bytes = remote.vram_total_bytes;
                            }
                            guard.telemetry.content_free_bytes = remote.content_free_bytes;
                            guard.telemetry.content_total_bytes = remote.content_total_bytes;
                            guard.telemetry.drive_free_bytes = remote.drive_free_bytes;
                            guard.telemetry.drive_total_bytes = remote.drive_total_bytes;
                        }
                    }
                }

                // Tier 3 (every 60s): `colab usage` balance & remaining time estimate
                if tick > 0 && tick.is_multiple_of(20) {
                    let env = check_env(path_env.as_deref(), home_dir.as_deref());
                    if let Some(colab_bin) = env.colab_path.map(PathBuf::from) {
                        let proxy_envs = resolve_proxy_envs(
                            runner.as_ref(),
                            proxy_https_hint.as_deref(),
                            proxy_http_hint.as_deref(),
                        )
                        .await;
                        if let Ok(u) =
                            colab_cli::usage(runner.as_ref(), &colab_bin, &proxy_envs, None).await
                        {
                            let mut guard = state.lock().await;
                            guard.telemetry.estimated_remaining_minutes =
                                estimated_remaining_minutes(u.current_balance, u.usage_rate_hourly);
                            guard.usage = Some(u);
                        }
                    }
                }

                {
                    let now_unix = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let mut guard = state.lock().await;
                    let vram_pct = match (
                        guard.telemetry.vram_used_bytes,
                        guard.telemetry.vram_total_bytes,
                    ) {
                        (Some(u), Some(t)) if t > 0 => Some((u as f32 / t as f32) * 100.0),
                        _ => None,
                    };
                    let ram_pct = match (
                        guard.telemetry.ram_used_bytes,
                        guard.telemetry.ram_total_bytes,
                    ) {
                        (Some(u), Some(t)) if t > 0 => Some((u as f32 / t as f32) * 100.0),
                        _ => None,
                    };
                    let sample = TelemetrySample {
                        timestamp_unix: now_unix,
                        gpu_util_pct: guard.telemetry.gpu_util_pct,
                        vram_pct,
                        ram_pct,
                    };
                    push_telemetry_sample(&mut guard.telemetry.samples, sample);
                }
                bump_snapshot(&snapshot_tx);
                tick = tick.wrapping_add(1);
            }
        });

        let handles_ref = self.handles.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let mut h = handles_ref.lock().await;
                if let Some(old) = h.telemetry_task.replace(task) {
                    old.abort();
                }
            });
        }
    }

    fn spawn_tunnel_supervisor(&self) {
        let data_dir = self.data_dir.clone();
        let home_dir = self.home_dir.clone();
        let path_env = self.path_env.clone();
        let proxy_https_hint = self.proxy_https_hint.clone();
        let proxy_http_hint = self.proxy_http_hint.clone();
        let runner = self.runner.clone();
        let spawner = self.spawner.clone();
        let state = self.state.clone();
        let handles = self.handles.clone();
        let intentional_stop = self.intentional_stop.clone();
        let snapshot_tx = self.snapshot_tx.clone();

        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if intentional_stop.load(Ordering::SeqCst) {
                    break;
                }
                let exited = {
                    let mut h = handles.lock().await;
                    match h.tunnel_proc.as_mut() {
                        Some(proc) => proc.try_wait().ok().flatten().is_some(),
                        None => break,
                    }
                };
                if !exited {
                    continue;
                }

                {
                    let mut guard = state.lock().await;
                    guard.tunnel_connected = false;
                    guard.phase = SessionPhase::Reconnecting;
                }
                bump_snapshot(&snapshot_tx);

                let env = check_env(path_env.as_deref(), home_dir.as_deref());
                let Some(colab_bin) = env.colab_path.map(PathBuf::from) else {
                    break;
                };
                let active = { state.lock().await.active_session.clone() };
                let Some(active) = active else {
                    break;
                };
                let proxy_envs = resolve_proxy_envs(
                    runner.as_ref(),
                    proxy_https_hint.as_deref(),
                    proxy_http_hint.as_deref(),
                )
                .await;

                let mut reconnected = false;
                for delay_secs in RECONNECT_BACKOFF_SECS {
                    tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                    if intentional_stop.load(Ordering::SeqCst) {
                        return;
                    }
                    match start_master_and_verify(
                        &data_dir,
                        &colab_bin,
                        &active,
                        runner.as_ref(),
                        spawner.as_ref(),
                        &proxy_envs,
                        None,
                    )
                    .await
                    {
                        Ok(started) => {
                            let mut h = handles.lock().await;
                            h.tunnel_proc = Some(started.process);
                            h.ctl_sock = Some(started.ctl_sock);
                            h.comfy_sock = Some(started.comfy_sock);
                            let mut guard = state.lock().await;
                            guard.tunnel_connected = true;
                            guard.phase = SessionPhase::Ready;
                            if let Some(w) = started.session_count_warning {
                                guard.warning_banner = Some(w);
                            }
                            bump_snapshot(&snapshot_tx);
                            reconnected = true;
                            break;
                        }
                        Err(
                            err @ (CoreError::AccountMismatch { .. }
                            | CoreError::SessionMissing { .. }
                            | CoreError::SessionLedgerMismatch { .. }
                            | CoreError::AuthRequired { .. }),
                        ) => {
                            let ui_err = UiError::from(&err);
                            Self::record_ui_error(&data_dir, &ui_err);
                            let mut guard = state.lock().await;
                            guard.phase = SessionPhase::DisconnectedOrMissing;
                            guard.tunnel_connected = false;
                            guard.last_error = Some(ui_err);
                            bump_snapshot(&snapshot_tx);
                            return;
                        }
                        Err(_) => continue,
                    }
                }

                if !reconnected {
                    let ui_err = UiError {
                        code: "TUNNEL_RECONNECT_EXHAUSTED".to_string(),
                        message: "SSH 隧道多次重连失败，请检查网络或手动重连。".to_string(),
                    };
                    Self::record_ui_error(&data_dir, &ui_err);
                    let mut guard = state.lock().await;
                    guard.phase = SessionPhase::Error;
                    guard.tunnel_connected = false;
                    guard.last_error = Some(ui_err);
                    bump_snapshot(&snapshot_tx);
                    break;
                }
            }
        });

        let handles_ref = self.handles.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let mut h = handles_ref.lock().await;
                if let Some(old) = h.supervisor_task.replace(task) {
                    old.abort();
                }
            });
        }
    }

    async fn teardown_local_handles(&self) {
        let (proxy, mut tunnel_proc, ctl_sock, comfy_sock, sup, telem) = {
            let mut h = self.handles.lock().await;
            (
                h.proxy.take(),
                h.tunnel_proc.take(),
                h.ctl_sock.take(),
                h.comfy_sock.take(),
                h.supervisor_task.take(),
                h.telemetry_task.take(),
            )
        };
        if let Some(task) = sup {
            task.abort();
        }
        if let Some(task) = telem {
            task.abort();
        }
        if let Some(p) = proxy {
            p.shutdown().await;
        }
        if let (Some(ctl), Some(comfy)) = (ctl_sock, comfy_sock) {
            stop_master_tunnel(&ctl, &comfy, tunnel_proc.as_mut(), self.runner.as_ref()).await;
        } else if let Some(mut proc) = tunnel_proc {
            let _ = proc.kill().await;
        }
        let mut guard = self.state.lock().await;
        guard.tunnel_connected = false;
    }

    /// Stops the active (or pending) Colab session and runs triple verification.
    pub async fn stop_session(&self) -> Result<StopReport, CoreError> {
        if self.start_in_flight.load(Ordering::SeqCst) {
            let notified = self.start_done_notify.notified();
            if self.start_in_flight.load(Ordering::SeqCst) {
                let _ = self.cancel_start().await;
                notified.await;
                let guard = self.state.lock().await;
                if guard.active_session.is_none() && guard.pending_session.is_none() {
                    if let Some(report) = guard.last_stop_report.clone() {
                        return Ok(report);
                    }
                }
            }
        }

        let res = self.stop_session_inner().await;
        if let Err(ref err) = res {
            let ui_err = UiError::from(err);
            Self::record_ui_error(&self.data_dir, &ui_err);
            let mut guard = self.state.lock().await;
            guard.step = None;
            guard.last_error = Some(ui_err);
            guard.phase = if guard.active_session.is_some()
                || guard.pending_session.is_some()
                || guard.ledger_corrupted
            {
                SessionPhase::Error
            } else {
                SessionPhase::Idle
            };
            drop(guard);
            self.notify_snapshot();
        }
        res
    }

    async fn stop_session_inner(&self) -> Result<StopReport, CoreError> {
        let env = check_env(self.path_env.as_deref(), self.home_dir.as_deref());
        let colab_bin = self.require_colab_bin(&env)?;

        let (
            session_name,
            owner_email,
            endpoint_sha256,
            drive_enabled,
            export_outputs,
            export_timeout_secs,
            target,
        ) = {
            let guard = self.state.lock().await;
            let export_outputs = guard.config.export_outputs_on_stop;
            let export_timeout_secs = guard.config.drive_export_timeout_secs.clamp(15, 1800);
            if let Some(ref act) = guard.active_session {
                (
                    act.session_name.clone(),
                    act.owner_email.clone(),
                    Some(act.endpoint_sha256.clone()),
                    act.drive_enabled,
                    export_outputs,
                    export_timeout_secs,
                    VerifiedLedgerTarget::Active {
                        session_name: act.session_name.clone(),
                        owner_email: act.owner_email.clone(),
                    },
                )
            } else if let Some(ref pend) = guard.pending_session {
                (
                    pend.session_name.clone(),
                    pend.owner_email.clone(),
                    None,
                    false,
                    export_outputs,
                    export_timeout_secs,
                    VerifiedLedgerTarget::Pending {
                        session_name: pend.session_name.clone(),
                        owner_email: pend.owner_email.clone(),
                    },
                )
            } else {
                return Err(CoreError::SessionLedgerMismatch {
                    detail: "当前没有可停机的会话".to_string(),
                });
            }
        };

        self.intentional_stop.store(true, Ordering::SeqCst);
        {
            let mut guard = self.state.lock().await;
            guard.phase = SessionPhase::Stopping;
            guard.last_error = None;
        }
        self.notify_snapshot();

        // Non-blocking pre-stop Drive export & flush_and_unmount() (P1-10, H3, L7, M3-A):
        // Never blocks `colab stop` if export/flush fails or times out.
        let mut drive_sync_confirmed: Option<bool> = None;
        if drive_enabled {
            let proxy_envs = resolve_proxy_envs(
                self.runner.as_ref(),
                self.proxy_http_hint.as_deref(),
                self.proxy_https_hint.as_deref(),
            )
            .await;
            let account_matches = match colab_cli::whoami(
                self.runner.as_ref(),
                &colab_bin,
                &proxy_envs,
                None,
            )
            .await
            {
                Ok(current_email) if current_email != owner_email => {
                    self.push_log(format!(
                        "告警：当前登录账号（{current_email}）与会话所属账号（{owner_email}）不一致，跳过停机前 Drive 导出，继续执行停机。"
                    ))
                    .await;
                    false
                }
                Ok(_) => true,
                Err(_) => true,
            };

            if !account_matches {
                drive_sync_confirmed = Some(false);
            } else {
                let ctl_sock = { self.handles.lock().await.ctl_sock.clone() };
                if let Some(ctl) = ctl_sock {
                    self.push_log(
                        "停机前正在导出工作流到 Drive 并执行 flush_and_unmount()...".to_string(),
                    )
                    .await;
                    let timeout_dur = Duration::from_secs(export_timeout_secs);
                    match tokio::time::timeout(
                        timeout_dur,
                        run_remote_export_and_flush(
                            &ctl,
                            export_outputs,
                            true,
                            timeout_dur,
                            self.runner.as_ref(),
                        ),
                    )
                    .await
                    {
                        Ok(Ok(exp)) if exp.ok && exp.flush_ok => {
                            drive_sync_confirmed = Some(true);
                            self.push_log(exp.summary.clone()).await;
                            let mut guard = self.state.lock().await;
                            guard.last_export_report = Some(exp);
                        }
                        Ok(Ok(exp)) => {
                            drive_sync_confirmed = Some(false);
                            self.push_log(format!("告警：{}（继续停机）", exp.summary))
                                .await;
                            let mut guard = self.state.lock().await;
                            guard.last_export_report = Some(exp);
                        }
                        Ok(Err(err)) => {
                            drive_sync_confirmed = Some(false);
                            self.push_log(format!(
                                "告警：停机前 Drive 导出失败（{err}），Drive 同步未确认，继续执行停机。"
                            ))
                            .await;
                        }
                        Err(_) => {
                            drive_sync_confirmed = Some(false);
                            self.push_log(format!(
                                "告警：停机前 Drive 导出/刷盘超时（{export_timeout_secs}s），Drive 同步未确认，继续执行停机以防烧算力。"
                            ))
                            .await;
                        }
                    }
                } else {
                    drive_sync_confirmed = Some(false);
                    self.push_log(
                        "告警：当前无活跃 SSH 隧道，已跳过停机前 Drive 导出与刷盘（Drive 同步未确认），继续执行停机。"
                            .to_string(),
                    )
                    .await;
                }
            }
        }

        self.teardown_local_handles().await;

        let cb = Some(self.make_line_callback());
        let proxy_envs = self.resolve_colab_envs().await;
        let mut report = colab_cli::stop_and_verify(
            self.runner.as_ref(),
            &colab_bin,
            &session_name,
            &owner_email,
            endpoint_sha256.as_deref(),
            &proxy_envs,
            cb,
        )
        .await?;
        report.drive_sync_confirmed = drive_sync_confirmed;
        if drive_sync_confirmed == Some(false) {
            report
                .warnings
                .push("Drive 同步未确认（停机前导出或 flush_and_unmount 未完成）。".to_string());
        }

        if report.safe_to_clear_ledger {
            self.clear_verified_ledger(target).await?;
            let mut guard = self.state.lock().await;
            guard.usage = report.usage.clone();
            guard.last_stop_report = Some(report.clone());
        } else {
            let mut guard = self.state.lock().await;
            guard.phase = SessionPhase::Error;
            guard.sync_managed_flag();
            guard.usage = report.usage.clone();
            guard.last_stop_report = Some(report.clone());
        }
        let _ = self.refresh_account_and_sessions(&colab_bin).await;
        self.notify_snapshot();
        Ok(report)
    }

    /// Stops active/pending sessions and rechecks all exit guards before allowing the desktop app to quit.
    pub async fn stop_session_for_exit(&self) -> Result<(), UiError> {
        let has_stoppable = self.start_in_flight.load(Ordering::SeqCst) || {
            let guard = self.state.lock().await;
            guard.active_session.is_some() || guard.pending_session.is_some()
        };
        if has_stoppable {
            let report = self.stop_session().await.map_err(|e| UiError::from(&e))?;
            if !report.safe_to_clear_ledger {
                return Err(UiError {
                    code: "STOP_UNVERIFIED".to_string(),
                    message: report.summary,
                });
            }
        }

        let disk = load_ledgers_checked(&self.data_dir);
        if let Some(err) = disk.corruption_error {
            Self::record_ui_error(&self.data_dir, &err);
            {
                let mut guard = self.state.lock().await;
                guard.ledger_corrupted = true;
                guard.active_session = disk.active;
                guard.pending_session = disk.pending;
                guard.phase = SessionPhase::Error;
                guard.step = None;
                guard.last_error = Some(err.clone());
                guard.sync_managed_flag();
            }
            self.notify_snapshot();
            return Err(err);
        }

        if disk.active.is_some()
            || disk.pending.is_some()
            || self.has_running_or_pending_session().await
        {
            let err = UiError {
                code: "STOP_UNVERIFIED".to_string(),
                message: "本地仍存在未解除的会话保护记录（如孤儿待核对或跨状态残留），已阻止按名称盲停与自动退出；请先核对状态或显式选择「保留云端运行，仅退出 App」。".to_string(),
            };
            Self::record_ui_error(&self.data_dir, &err);
            {
                let mut guard = self.state.lock().await;
                guard.active_session = disk.active;
                guard.pending_session = disk.pending;
                guard.phase = SessionPhase::Error;
                guard.step = None;
                guard.last_error = Some(err.clone());
                guard.sync_managed_flag();
            }
            self.notify_snapshot();
            return Err(err);
        }
        Ok(())
    }

    /// Closes local proxy + SSH ControlMaster on exit when the user explicitly
    /// chooses to keep the remote Colab GPU running.
    pub async fn disconnect_local_only_for_exit(&self) {
        self.intentional_stop.store(true, Ordering::SeqCst);
        self.teardown_local_handles().await;
    }

    pub async fn open_session_in_browser(&self) -> Result<(), CoreError> {
        let env = check_env(self.path_env.as_deref(), self.home_dir.as_deref());
        let colab_bin = self.require_colab_bin(&env)?;
        let session_name = {
            let guard = self.state.lock().await;
            guard
                .active_session
                .as_ref()
                .map(|a| a.session_name.clone())
        }
        .ok_or_else(|| CoreError::SessionLedgerMismatch {
            detail: "没有活跃会话".to_string(),
        })?;
        let proxy_envs = self.resolve_colab_envs().await;
        colab_cli::open_url(
            self.runner.as_ref(),
            &colab_bin,
            &session_name,
            &proxy_envs,
            None,
        )
        .await
    }

    pub async fn open_colab_pay(&self) -> Result<(), CoreError> {
        let env = check_env(self.path_env.as_deref(), self.home_dir.as_deref());
        let colab_bin = self.require_colab_bin(&env)?;
        let proxy_envs = self.resolve_colab_envs().await;
        colab_cli::open_pay(self.runner.as_ref(), &colab_bin, &proxy_envs, None).await
    }

    /// Runs a command remotely over the active ControlMaster socket (used by live tests / probes).
    pub async fn exec_over_mux(
        &self,
        remote_cmd: &str,
        stdin: StdinPolicy,
        timeout: Duration,
    ) -> Result<(Option<i32>, String, String), CoreError> {
        let ctl_sock =
            {
                let guard = self.state.lock().await;
                let active = guard.active_session.as_ref().ok_or_else(|| {
                    CoreError::SessionLedgerMismatch {
                        detail: "没有活跃会话".to_string(),
                    }
                })?;
                ctl_socket_path(&self.data_dir, &active.session_name)?
            };
        let out = self
            .runner
            .run(
                build_mux_exec_spec(&ctl_sock, remote_cmd, stdin, timeout),
                None,
            )
            .await?;
        Ok((out.exit_code, out.stdout, out.stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::sha256_hex;
    use crate::runner::{BoxFuture, CommandOutput, CommandSpec};
    use std::collections::VecDeque;
    use std::fs;
    use std::future::Future;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicUsize;

    struct ShortTempDir(PathBuf);

    impl ShortTempDir {
        fn new() -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let dir = PathBuf::from(format!(
                "/tmp/ccu-mgr-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for ShortTempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct MockProc;
    impl TunnelProcess for MockProc {
        fn try_wait(&mut self) -> Result<Option<i32>, CoreError> {
            Ok(None)
        }
        fn kill<'a>(&'a mut self) -> BoxFuture<'a, Result<(), CoreError>> {
            Box::pin(async { Ok(()) })
        }
    }

    struct MockSpawner;
    impl TunnelSpawner for MockSpawner {
        fn spawn(
            &self,
            _program: &Path,
            _args: &[String],
            _envs: &[(String, String)],
        ) -> Result<Box<dyn TunnelProcess>, CoreError> {
            Ok(Box::new(MockProc))
        }
    }

    struct QueuedRunner {
        queue: std::sync::Mutex<VecDeque<CommandOutput>>,
        calls: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl QueuedRunner {
        fn new(items: Vec<CommandOutput>) -> Self {
            Self {
                queue: std::sync::Mutex::new(items.into()),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for QueuedRunner {
        fn run<'a>(
            &'a self,
            spec: CommandSpec,
            _on_line: Option<LineCallback>,
        ) -> BoxFuture<'a, Result<CommandOutput, CoreError>> {
            Box::pin(async move {
                if spec.program == Path::new(crate::locate::SCUTIL_BIN) {
                    return Ok(CommandOutput::from_raw(
                        Some(0),
                        String::new(),
                        String::new(),
                    ));
                }
                self.calls.lock().unwrap().push(spec.args);
                Ok(self
                    .queue
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("missing queued output"))
            })
        }
    }

    fn block_on<F: Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    fn install_fake_colab(home: &Path) {
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let colab = bin.join("colab");
        fs::write(&colab, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&colab, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn start_blocks_zero_balance_unless_confirmed_and_recovers_pending_on_startup() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);

        // Zero balance blocks start before colab new
        let runner_zero = Arc::new(QueuedRunner::new(vec![
            CommandOutput::from_raw(
                Some(0),
                "Email: alice@example.com\n".to_string(),
                String::new(),
            ),
            CommandOutput::from_raw(
                Some(0),
                "Current balance: 0.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0"
                    .to_string(),
                String::new(),
            ),
        ]));
        let mgr = SessionManager::with_deps(
            data.clone(),
            Some(home.clone()),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            runner_zero,
            Arc::new(MockSpawner),
        );
        let err = block_on(mgr.start_session(GpuTier::L4, false, false)).unwrap_err();
        assert!(matches!(err, CoreError::ZeroBalanceBlocked { .. }));
        assert!(!pending_session_path(&data).exists());

        // Startup reconciliation promotes alive pending-session.json -> active-session.json
        let sname = default_session_name_for_email("alice@example.com");
        save_json_0600(
            &pending_session_path(&data),
            &PendingSession {
                session_name: sname.clone(),
                owner_email: "alice@example.com".to_string(),
                gpu: GpuTier::T4,
                high_mem: false,
                cancel_requested: false,
                uncertain_orphan: false,
                drive_enabled: false,
                created_at_unix: 100,
            },
        )
        .unwrap();

        let runner_startup = Arc::new(QueuedRunner::new(vec![
            CommandOutput::from_raw(
                Some(0),
                "Email: alice@example.com\n".to_string(),
                String::new(),
            ),
            CommandOutput::from_raw(
                Some(0),
                "Current balance: 10.00 compute units\nUsage rate: 1.50/hr\nActive assignments: 1"
                    .to_string(),
                String::new(),
            ),
            CommandOutput::from_raw(
                Some(0),
                format!(
                    "[{sname}] m-s-pend1 | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU"
                ),
                String::new(),
            ),
            CommandOutput::from_raw(
                Some(0),
                format!(
                    "[{sname}] m-s-pend1 | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU"
                ),
                String::new(),
            ),
        ]));
        let mgr2 = SessionManager::with_deps(
            data.clone(),
            Some(home),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            runner_startup,
            Arc::new(MockSpawner),
        );
        let snap = block_on(mgr2.reconcile_on_startup());
        assert!(snap.pending_session.is_none());
        let active = snap.active_session.expect("pending promoted to active");
        assert_eq!(active.endpoint_sha256, sha256_hex(b"m-s-pend1"));
        assert!(snap.startup_notice.is_some());
    }

    type DynResponder = Box<
        dyn Fn(&CommandSpec) -> BoxFuture<'static, Result<CommandOutput, CoreError>> + Send + Sync,
    >;

    struct SmartRunner {
        responder: DynResponder,
    }

    impl CommandRunner for SmartRunner {
        fn run<'a>(
            &'a self,
            spec: CommandSpec,
            _on_line: Option<LineCallback>,
        ) -> BoxFuture<'a, Result<CommandOutput, CoreError>> {
            (self.responder)(&spec)
        }
    }

    struct FlakyProc {
        fail_first_wait: bool,
        calls: Arc<AtomicUsize>,
    }

    impl TunnelProcess for FlakyProc {
        fn try_wait(&mut self) -> Result<Option<i32>, CoreError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_first_wait && n == 1 {
                return Ok(Some(255));
            }
            Ok(None)
        }

        fn kill<'a>(&'a mut self) -> BoxFuture<'a, Result<(), CoreError>> {
            Box::pin(async { Ok(()) })
        }
    }

    struct FlakySpawner {
        spawns: Arc<AtomicUsize>,
    }

    impl TunnelSpawner for FlakySpawner {
        fn spawn(
            &self,
            _program: &Path,
            _args: &[String],
            _envs: &[(String, String)],
        ) -> Result<Box<dyn TunnelProcess>, CoreError> {
            let idx = self.spawns.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FlakyProc {
                fail_first_wait: idx == 0,
                calls: Arc::new(AtomicUsize::new(0)),
            }))
        }
    }

    #[test]
    fn supervisor_reconnects_without_deadlock_when_subscriber_active_and_allows_stop() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);
        let sname = default_session_name_for_email("alice@example.com");

        let runner = Arc::new(SmartRunner {
            responder: Box::new(move |spec| {
                let sname = sname.clone();
                let prog = spec.program.clone();
                let args = spec.args.clone();
                Box::pin(async move {
                    if prog == Path::new(crate::locate::SCUTIL_BIN)
                        || prog == Path::new(crate::locate::SSH_KEYGEN_BIN)
                    {
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    if prog == Path::new(crate::locate::SSH_BIN) {
                        if args.iter().any(|a| a == "bash -s") {
                            return Ok(CommandOutput::from_raw(
                                Some(0),
                                format!("{}\n", crate::bootstrap::COMFY_READY_SENTINEL),
                                String::new(),
                            ));
                        }
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    let sub = args
                        .iter()
                        .find(|a| !a.starts_with('-'))
                        .map(String::as_str);
                    let out = match sub {
                        Some("whoami") => "Email: alice@example.com\n".to_string(),
                        Some("usage") => "Current balance: 10.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0".to_string(),
                        Some("status") => format!("[{sname}] m-s-mine | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU"),
                        Some("sessions") => format!("[{sname}] m-s-mine | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU"),
                        Some("stop") => "[colab] Session terminated.".to_string(),
                        _ => String::new(),
                    };
                    Ok(CommandOutput::from_raw(Some(0), out, String::new()))
                })
            }),
        });
        let spawner = Arc::new(FlakySpawner {
            spawns: Arc::new(AtomicUsize::new(0)),
        });

        let mgr = Arc::new(SessionManager::with_deps(
            data.clone(),
            Some(home),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            runner,
            spawner.clone(),
        ));
        fs::write(crate::config::ssh_key_path(&data), "priv").unwrap();
        fs::write(crate::config::ssh_pub_key_path(&data), "pub").unwrap();

        block_on(async {
            let _ = mgr
                .update_preferences(None, Some(false), None, None, None)
                .await;
            let mut rx = mgr.subscribe();
            let snap = mgr.start_session(GpuTier::T4, false, false).await.unwrap();
            assert_eq!(snap.phase, SessionPhase::Ready);

            // Wait for supervisor to detect exited ssh -> Reconnecting -> Ready without deadlocking.
            let observed_reconnect = tokio::time::timeout(Duration::from_secs(8), async {
                let mut saw_reconnecting = false;
                loop {
                    rx.changed().await.unwrap();
                    let s = mgr.snapshot().await;
                    if s.phase == SessionPhase::Reconnecting {
                        saw_reconnecting = true;
                    }
                    if saw_reconnecting
                        && s.phase == SessionPhase::Ready
                        && spawner.spawns.load(Ordering::SeqCst) >= 2
                    {
                        return true;
                    }
                }
            })
            .await
            .expect("supervisor must reconnect without watch::Sender deadlock");
            assert!(observed_reconnect);

            let report = tokio::time::timeout(Duration::from_secs(3), mgr.stop_session())
                .await
                .expect("stop_session must not hang when subscriber is active")
                .unwrap();
            assert!(report.terminated_sentinel);
            assert!(report.owner_verified);
        });
    }

    #[test]
    fn cancel_during_bootstrap_stops_session_and_retains_ledger_when_stop_unverified() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);
        let sname = default_session_name_for_email("alice@example.com");
        let stop_should_succeed = Arc::new(AtomicBool::new(false));
        let stop_flag = stop_should_succeed.clone();

        let runner = Arc::new(SmartRunner {
            responder: Box::new(move |spec| {
                let sname = sname.clone();
                let stop_flag = stop_flag.clone();
                let prog = spec.program.clone();
                let args = spec.args.clone();
                Box::pin(async move {
                    if prog == Path::new(crate::locate::SCUTIL_BIN)
                        || prog == Path::new(crate::locate::SSH_KEYGEN_BIN)
                    {
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    if prog == Path::new(crate::locate::SSH_BIN) {
                        if args.iter().any(|a| a == "bash -s") {
                            tokio::time::sleep(Duration::from_secs(10)).await;
                            return Ok(CommandOutput::from_raw(
                                Some(0),
                                String::new(),
                                String::new(),
                            ));
                        }
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    let sub = args
                        .iter()
                        .find(|a| !a.starts_with('-'))
                        .map(String::as_str);
                    let out = match sub {
                        Some("whoami") => "Email: alice@example.com\n".to_string(),
                        Some("usage") => "Current balance: 10.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0".to_string(),
                        Some("status") => format!("[{sname}] m-s-boot1 | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU"),
                        Some("sessions") => {
                            if stop_flag.load(Ordering::SeqCst) {
                                "[colab] No active sessions found on server.".to_string()
                            } else {
                                format!("[{sname}] m-s-boot1 | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU")
                            }
                        }
                        Some("stop") => {
                            if stop_flag.load(Ordering::SeqCst) {
                                "[colab] Session terminated.".to_string()
                            } else {
                                "transient stop failure".to_string()
                            }
                        }
                        _ => String::new(),
                    };
                    Ok(CommandOutput::from_raw(Some(0), out, String::new()))
                })
            }),
        });

        let mgr = Arc::new(SessionManager::with_deps(
            data.clone(),
            Some(home),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            runner,
            Arc::new(MockSpawner),
        ));
        fs::write(crate::config::ssh_key_path(&data), "priv").unwrap();
        fs::write(crate::config::ssh_pub_key_path(&data), "pub").unwrap();

        block_on(async {
            let _ = mgr
                .update_preferences(None, Some(false), None, None, None)
                .await;
            let mgr_start = mgr.clone();
            let handle =
                tokio::spawn(
                    async move { mgr_start.start_session(GpuTier::T4, false, false).await },
                );

            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if mgr.snapshot().await.step == Some(StartStep::Bootstrapping) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();

            assert!(active_session_path(&data).exists());
            let _ = mgr.cancel_start().await;

            let err = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(matches!(
                err,
                CoreError::StartCancelledAfterAllocation {
                    stop_verified: false,
                    ..
                }
            ));
            // H3: unverified stop must RETAIN active-session.json and stay in Error!
            assert!(active_session_path(&data).exists());
            assert_eq!(mgr.snapshot().await.phase, SessionPhase::Error);
            assert!(mgr.has_running_or_pending_session_sync());

            // Retrying stop when server confirms cleared clears the ledger cleanly.
            stop_should_succeed.store(true, Ordering::SeqCst);
            let report = mgr.stop_session().await.unwrap();
            assert!(report.safe_to_clear_ledger);
            assert!(!active_session_path(&data).exists());
            assert_eq!(mgr.snapshot().await.phase, SessionPhase::Idle);
        });
    }

    #[test]
    fn uncertain_new_timeout_with_orphan_retains_pending_ledger_and_sets_warning() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);
        let sessions_calls = Arc::new(AtomicUsize::new(0));
        let sc = sessions_calls.clone();
        let sname = default_session_name_for_email("alice@example.com");

        let runner = Arc::new(SmartRunner {
            responder: Box::new(move |spec| {
                let sc = sc.clone();
                let sname = sname.clone();
                let prog = spec.program.clone();
                let args = spec.args.clone();
                Box::pin(async move {
                    if prog == Path::new(crate::locate::SCUTIL_BIN) {
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    let sub = args
                        .iter()
                        .find(|a| !a.starts_with('-'))
                        .map(String::as_str);
                    match sub {
                        Some("whoami") => Ok(CommandOutput::from_raw(
                            Some(0),
                            "Email: alice@example.com\n".to_string(),
                            String::new(),
                        )),
                        Some("usage") => Ok(CommandOutput::from_raw(
                            Some(0),
                            "Current balance: 10.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0".to_string(),
                            String::new(),
                        )),
                        Some("status") => Ok(CommandOutput::from_raw(
                            Some(0),
                            format!("[colab] Session '{sname}' not found."),
                            String::new(),
                        )),
                        Some("sessions") => {
                            let n = sc.fetch_add(1, Ordering::SeqCst);
                            let stdout = if n == 0 {
                                "[colab] No active sessions found on server.".to_string()
                            } else {
                                "[?] m-s-ghost-new | Hardware: NVIDIA L4 | Shape: Standard | Variant: GPU".to_string()
                            };
                            Ok(CommandOutput::from_raw(Some(0), stdout, String::new()))
                        }
                        Some("new") => Ok(CommandOutput::from_raw(
                            Some(1),
                            String::new(),
                            "requests.exceptions.ReadTimeout: Read timed out.".to_string(),
                        )),
                        _ => Ok(CommandOutput::from_raw(Some(0), String::new(), String::new())),
                    }
                })
            }),
        });

        let mgr = SessionManager::with_deps(
            data.clone(),
            Some(home),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            runner,
            Arc::new(MockSpawner),
        );

        let err = block_on(mgr.start_session(GpuTier::L4, false, false)).unwrap_err();
        assert!(matches!(
            err,
            CoreError::CapacityOrSessionLimit {
                uncertain: true,
                ..
            }
        ));
        assert!(pending_session_path(&data).exists());
        let pend: PendingSession = load_json_opt(&pending_session_path(&data))
            .unwrap()
            .unwrap();
        assert!(pend.uncertain_orphan);
        let snap = block_on(mgr.snapshot());
        assert_eq!(snap.orphan_count, 1);
        assert!(snap.warning_banner.is_some());
    }

    #[test]
    fn reconcile_pending_checks_account_mismatch_preserves_corrupted_ledger_and_guards_inflight() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);

        // M3: pending ledger owned by alice@example.com, current account is bob@example.com
        let sname = default_session_name_for_email("alice@example.com");
        save_json_0600(
            &pending_session_path(&data),
            &PendingSession {
                session_name: sname.clone(),
                owner_email: "alice@example.com".to_string(),
                gpu: GpuTier::T4,
                high_mem: false,
                cancel_requested: false,
                uncertain_orphan: false,
                drive_enabled: false,
                created_at_unix: 100,
            },
        )
        .unwrap();

        let status_calls = Arc::new(AtomicUsize::new(0));
        let st_c = status_calls.clone();
        let runner_mismatch =
            Arc::new(QueuedRunner::new(vec![
            CommandOutput::from_raw(Some(0), "Email: bob@example.com\n".to_string(), String::new()),
            CommandOutput::from_raw(
                Some(0),
                "Current balance: 10.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0"
                    .to_string(),
                String::new(),
            ),
            CommandOutput::from_raw(
                Some(0),
                "[colab] No active sessions found on server.".to_string(),
                String::new(),
            ),
        ]));
        let mgr_m3 = SessionManager::with_deps(
            data.clone(),
            Some(home.clone()),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            runner_mismatch,
            Arc::new(MockSpawner),
        );
        // P2: if start_in_flight is set, reconcile must bail immediately without touching files
        mgr_m3.start_in_flight.store(true, Ordering::SeqCst);
        let _ = block_on(mgr_m3.reconcile_on_startup());
        assert!(pending_session_path(&data).exists());
        mgr_m3.start_in_flight.store(false, Ordering::SeqCst);

        let snap_m3 = block_on(mgr_m3.reconcile_on_startup());
        assert_eq!(st_c.load(Ordering::SeqCst), 0);
        assert!(pending_session_path(&data).exists());
        assert_eq!(snap_m3.phase, SessionPhase::DisconnectedOrMissing);
        assert_eq!(
            snap_m3.startup_notice.as_ref().map(|n| n.kind.as_str()),
            Some("account_changed")
        );

        // M4: corrupted active-session.json must stay on disk and enable exit guard
        let _ = remove_if_exists(&pending_session_path(&data));
        fs::write(active_session_path(&data), b"{corrupted json").unwrap();
        let mgr_m4 = SessionManager::with_deps(
            data.clone(),
            Some(home),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            Arc::new(QueuedRunner::new(vec![])),
            Arc::new(MockSpawner),
        );
        let snap_m4 = block_on(mgr_m4.snapshot());
        assert!(snap_m4.ledger_corrupted);
        assert!(mgr_m4.has_running_or_pending_session_sync());
        assert_eq!(snap_m4.phase, SessionPhase::Error);
        assert!(active_session_path(&data).exists());
    }

    #[test]
    fn reconnect_and_stop_errors_reset_intermediate_phases() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);

        // M1 stop: missing ledger returns error and stays Idle (not Stopping)
        let mgr = SessionManager::with_deps(
            data.clone(),
            Some(home.clone()),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            Arc::new(QueuedRunner::new(vec![])),
            Arc::new(MockSpawner),
        );
        let stop_err = block_on(mgr.stop_session()).unwrap_err();
        assert!(matches!(stop_err, CoreError::SessionLedgerMismatch { .. }));
        assert_eq!(block_on(mgr.snapshot()).phase, SessionPhase::Idle);

        // M1 reconnect: SessionMissing resets phase from Reconnecting -> DisconnectedOrMissing
        let active = ActiveSession {
            session_name: "comfy-1234abcd".to_string(),
            owner_email: "alice@example.com".to_string(),
            endpoint_sha256: sha256_hex(b"m-s-gone"),
            gpu: GpuTier::T4,
            high_mem: false,
            hardware_summary: "NVIDIA T4".to_string(),
            local_port: LOCAL_PROXY_PORT,
            drive_enabled: false,
            created_at_unix: 100,
        };
        save_json_0600(&active_session_path(&data), &active).unwrap();
        let mgr_re = SessionManager::with_deps(
            data,
            Some(home),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            Arc::new(QueuedRunner::new(vec![
                CommandOutput::from_raw(
                    Some(0),
                    "Email: alice@example.com\n".to_string(),
                    String::new(),
                ),
                CommandOutput::from_raw(
                    Some(0),
                    "[colab] Session 'comfy-1234abcd' not found.".to_string(),
                    String::new(),
                ),
            ])),
            Arc::new(MockSpawner),
        );
        let re_err = block_on(mgr_re.reconnect_tunnel()).unwrap_err();
        assert!(matches!(re_err, CoreError::SessionMissing { .. }));
        let snap = block_on(mgr_re.snapshot());
        assert_eq!(snap.phase, SessionPhase::DisconnectedOrMissing);
        assert!(snap.step.is_none());
        assert!(snap.last_error.is_some());
    }

    #[test]
    fn cancel_tunnel_then_second_start_succeeds_without_stale_cancel() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);
        let sname = default_session_name_for_email("alice@example.com");
        let second_attempt = Arc::new(AtomicBool::new(false));
        let second_flag = second_attempt.clone();
        let boot_entered = Arc::new(tokio::sync::Notify::new());
        let boot_entered_srv = boot_entered.clone();
        let stop_calls = Arc::new(AtomicUsize::new(0));
        let stop_calls_srv = stop_calls.clone();

        let runner = Arc::new(SmartRunner {
            responder: Box::new(move |spec| {
                let sname = sname.clone();
                let second_flag = second_flag.clone();
                let boot_entered_srv = boot_entered_srv.clone();
                let stop_calls_srv = stop_calls_srv.clone();
                let prog = spec.program.clone();
                let args = spec.args.clone();
                Box::pin(async move {
                    if prog == Path::new(crate::locate::SCUTIL_BIN)
                        || prog == Path::new(crate::locate::SSH_KEYGEN_BIN)
                    {
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    if prog == Path::new(crate::locate::SSH_BIN) {
                        if args.iter().any(|a| a == "bash -s") {
                            if !second_flag.load(Ordering::SeqCst) {
                                boot_entered_srv.notify_one();
                                tokio::time::sleep(Duration::from_secs(10)).await;
                            }
                            return Ok(CommandOutput::from_raw(
                                Some(0),
                                format!("{}\n", crate::bootstrap::COMFY_READY_SENTINEL),
                                String::new(),
                            ));
                        }
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    let sub = args
                        .iter()
                        .find(|a| !a.starts_with('-'))
                        .map(String::as_str);
                    let out = match sub {
                        Some("whoami") => "Email: alice@example.com\n".to_string(),
                        Some("usage") => "Current balance: 10.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0".to_string(),
                        Some("status") => format!("[{sname}] m-s-d1 | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU"),
                        Some("sessions") => {
                            if stop_calls_srv.load(Ordering::SeqCst) > 0 && !second_flag.load(Ordering::SeqCst) {
                                "[colab] No active sessions found on server.".to_string()
                            } else {
                                format!("[{sname}] m-s-d1 | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU")
                            }
                        }
                        Some("stop") => {
                            stop_calls_srv.fetch_add(1, Ordering::SeqCst);
                            "[colab] Session terminated.".to_string()
                        }
                        _ => String::new(),
                    };
                    Ok(CommandOutput::from_raw(Some(0), out, String::new()))
                })
            }),
        });

        let mgr = Arc::new(SessionManager::with_deps(
            data.clone(),
            Some(home),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            runner,
            Arc::new(MockSpawner),
        ));
        fs::write(crate::config::ssh_key_path(&data), "priv").unwrap();
        fs::write(crate::config::ssh_pub_key_path(&data), "pub").unwrap();

        block_on(async {
            let _ = mgr
                .update_preferences(None, Some(false), None, None, None)
                .await;
            let mgr_start = mgr.clone();
            let handle =
                tokio::spawn(
                    async move { mgr_start.start_session(GpuTier::T4, false, false).await },
                );
            tokio::time::timeout(Duration::from_secs(5), boot_entered.notified())
                .await
                .unwrap();
            let _ = mgr.cancel_start().await;
            let err = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(matches!(
                err,
                CoreError::StartCancelledAfterAllocation {
                    stop_verified: true,
                    ..
                }
            ));
            assert_eq!(mgr.snapshot().await.phase, SessionPhase::Idle);

            // Second start without clicking cancel must clear stale watch cancel state via send_replace.
            second_attempt.store(true, Ordering::SeqCst);
            let snap2 = tokio::time::timeout(
                Duration::from_secs(5),
                mgr.start_session(GpuTier::T4, false, false),
            )
            .await
            .unwrap()
            .expect("second start must not see stale cancel=true");
            assert_eq!(snap2.phase, SessionPhase::Ready);
            assert!(snap2.tunnel_connected);
        });
    }

    #[test]
    fn reconcile_preserves_uncertain_orphan_pending_when_sessions_query_fails() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);
        let sname = default_session_name_for_email("alice@example.com");
        save_json_0600(
            &pending_session_path(&data),
            &PendingSession {
                session_name: sname.clone(),
                owner_email: "alice@example.com".to_string(),
                gpu: GpuTier::L4,
                high_mem: false,
                cancel_requested: false,
                uncertain_orphan: true,
                drive_enabled: false,
                created_at_unix: 100,
            },
        )
        .unwrap();

        let sessions_fail = Arc::new(AtomicBool::new(true));
        let sf = sessions_fail.clone();
        let runner = Arc::new(SmartRunner {
            responder: Box::new(move |spec| {
                let sname = sname.clone();
                let sf = sf.clone();
                let prog = spec.program.clone();
                let args = spec.args.clone();
                Box::pin(async move {
                    if prog == Path::new(crate::locate::SCUTIL_BIN) {
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    let sub = args
                        .iter()
                        .find(|a| !a.starts_with('-'))
                        .map(String::as_str);
                    match sub {
                        Some("whoami") => Ok(CommandOutput::from_raw(
                            Some(0),
                            "Email: alice@example.com\n".to_string(),
                            String::new(),
                        )),
                        Some("usage") => Ok(CommandOutput::from_raw(
                            Some(0),
                            "Current balance: 10.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0".to_string(),
                            String::new(),
                        )),
                        Some("sessions") => {
                            if sf.load(Ordering::SeqCst) {
                                Ok(CommandOutput::from_raw(
                                    Some(1),
                                    String::new(),
                                    "network unreachable".to_string(),
                                ))
                            } else {
                                Ok(CommandOutput::from_raw(
                                    Some(0),
                                    "[colab] No active sessions found on server.".to_string(),
                                    String::new(),
                                ))
                            }
                        }
                        Some("status") => Ok(CommandOutput::from_raw(
                            Some(0),
                            format!("[colab] Session '{sname}' not found."),
                            String::new(),
                        )),
                        _ => Ok(CommandOutput::from_raw(Some(0), String::new(), String::new())),
                    }
                })
            }),
        });

        let mgr = SessionManager::with_deps(
            data.clone(),
            Some(home),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            runner,
            Arc::new(MockSpawner),
        );

        let snap1 = block_on(mgr.reconcile_on_startup());
        assert!(pending_session_path(&data).exists());
        assert!(snap1.pending_session.is_some());
        assert!(mgr.has_running_or_pending_session_sync());
        assert_eq!(
            snap1.startup_notice.as_ref().map(|n| n.kind.as_str()),
            Some("reconcile_deferred")
        );

        // Once sessions succeeds cleanly with 0 orphans, pending ledger can be cleared.
        sessions_fail.store(false, Ordering::SeqCst);
        let snap2 = block_on(mgr.reconcile_on_startup());
        assert!(!pending_session_path(&data).exists());
        assert!(snap2.pending_session.is_none());
        assert!(!mgr.has_running_or_pending_session_sync());
        assert_eq!(
            snap2.startup_notice.as_ref().map(|n| n.kind.as_str()),
            Some("pending_gone")
        );
    }

    #[test]
    fn allocated_session_still_stops_on_cancel_when_active_promotion_fails() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);
        let sname = default_session_name_for_email("alice@example.com");
        let new_entered = Arc::new(tokio::sync::Notify::new());
        let new_entered_srv = new_entered.clone();
        let allow_new_finish = Arc::new(tokio::sync::Notify::new());
        let allow_new_finish_srv = allow_new_finish.clone();
        let stop_called = Arc::new(AtomicBool::new(false));
        let stop_called_srv = stop_called.clone();
        let status_calls = Arc::new(AtomicUsize::new(0));
        let status_calls_srv = status_calls.clone();

        let runner = Arc::new(SmartRunner {
            responder: Box::new(move |spec| {
                let sname = sname.clone();
                let new_entered_srv = new_entered_srv.clone();
                let allow_new_finish_srv = allow_new_finish_srv.clone();
                let stop_called_srv = stop_called_srv.clone();
                let status_calls_srv = status_calls_srv.clone();
                let prog = spec.program.clone();
                let args = spec.args.clone();
                Box::pin(async move {
                    if prog == Path::new(crate::locate::SCUTIL_BIN)
                        || prog == Path::new(crate::locate::SSH_KEYGEN_BIN)
                    {
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    let sub = args
                        .iter()
                        .find(|a| !a.starts_with('-'))
                        .map(String::as_str);
                    let out = match sub {
                        Some("whoami") => "Email: alice@example.com\n".to_string(),
                        Some("usage") => "Current balance: 10.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0".to_string(),
                        Some("status") => {
                            let idx = status_calls_srv.fetch_add(1, Ordering::SeqCst);
                            if idx == 0 {
                                format!("[colab] Session '{sname}' not found.")
                            } else {
                                format!("[{sname}] m-s-d3 | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU")
                            }
                        }
                        Some("sessions") => {
                            "[colab] No active sessions found on server.".to_string()
                        }
                        Some("new") => {
                            new_entered_srv.notify_one();
                            allow_new_finish_srv.notified().await;
                            format!("[{sname}] m-s-d3 | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU")
                        }
                        Some("stop") => {
                            stop_called_srv.store(true, Ordering::SeqCst);
                            "[colab] Session terminated.".to_string()
                        }
                        _ => String::new(),
                    };
                    Ok(CommandOutput::from_raw(Some(0), out, String::new()))
                })
            }),
        });

        let mgr = Arc::new(SessionManager::with_deps(
            data.clone(),
            Some(home),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            runner,
            Arc::new(MockSpawner),
        ));

        block_on(async {
            let mgr_start = mgr.clone();
            let handle =
                tokio::spawn(
                    async move { mgr_start.start_session(GpuTier::T4, false, false).await },
                );
            tokio::time::timeout(Duration::from_secs(5), new_entered.notified())
                .await
                .unwrap();

            // Sabotage active-session.json path as a directory so save_json_0600 fails on promote.
            fs::create_dir_all(active_session_path(&data)).unwrap();
            let _ = mgr.cancel_start().await;
            allow_new_finish.notify_one();

            let err = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(stop_called.load(Ordering::SeqCst));
            assert!(matches!(
                err,
                CoreError::StartCancelledAfterAllocation {
                    stop_verified: true,
                    ..
                }
            ));
            assert!(!pending_session_path(&data).exists());
            let _ = fs::remove_dir_all(active_session_path(&data));
        });
    }

    #[test]
    fn stop_verified_active_preserves_corrupted_pending_ledger() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);
        let sname = default_session_name_for_email("alice@example.com");

        save_json_0600(
            &active_session_path(&data),
            &ActiveSession {
                session_name: sname.clone(),
                owner_email: "alice@example.com".to_string(),
                endpoint_sha256: sha256_hex(b"m-s-d5"),
                gpu: GpuTier::T4,
                high_mem: false,
                hardware_summary: "NVIDIA T4".to_string(),
                local_port: LOCAL_PROXY_PORT,
                drive_enabled: false,
                created_at_unix: 100,
            },
        )
        .unwrap();
        fs::write(pending_session_path(&data), b"{corrupted pending json").unwrap();

        let runner = Arc::new(SmartRunner {
            responder: Box::new(move |spec| {
                let sname = sname.clone();
                let prog = spec.program.clone();
                let args = spec.args.clone();
                Box::pin(async move {
                    if prog == Path::new(crate::locate::SCUTIL_BIN) {
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    let sub = args
                        .iter()
                        .find(|a| !a.starts_with('-'))
                        .map(String::as_str);
                    let out = match sub {
                        Some("whoami") => "Email: alice@example.com\n".to_string(),
                        Some("usage") => "Current balance: 10.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0".to_string(),
                        Some("status") => format!("[colab] Session '{sname}' not found."),
                        Some("sessions") => "[colab] No active sessions found on server.".to_string(),
                        _ => String::new(),
                    };
                    Ok(CommandOutput::from_raw(Some(0), out, String::new()))
                })
            }),
        });

        let mgr = SessionManager::with_deps(
            data.clone(),
            Some(home),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            runner,
            Arc::new(MockSpawner),
        );

        let snap_boot = block_on(mgr.reconcile_on_startup());
        assert!(snap_boot.ledger_corrupted);
        assert!(snap_boot.active_session.is_some());
        assert_eq!(snap_boot.phase, SessionPhase::Error);

        // Stop and Exit verifies & removes only active-session.json; corrupted pending-session.json stays
        // untouched and blocks quitting until resolved.
        let exit_err = block_on(mgr.stop_session_for_exit()).unwrap_err();
        assert_eq!(exit_err.code, "SESSION_LEDGER_CORRUPTED");
        assert!(!active_session_path(&data).exists());
        assert!(pending_session_path(&data).exists());

        let snap_after = block_on(mgr.snapshot());
        assert!(snap_after.active_session.is_none());
        assert!(snap_after.ledger_corrupted);
        assert_eq!(snap_after.phase, SessionPhase::Error);
        assert!(snap_after
            .last_stop_report
            .as_ref()
            .is_some_and(|r| r.safe_to_clear_ledger));
        assert!(mgr.has_running_or_pending_session_sync());

        // Removing the corrupted file and reconciling restores Idle cleanly and allows exit.
        let _ = remove_if_exists(&pending_session_path(&data));
        let snap_clean = block_on(mgr.reconcile_on_startup());
        assert!(!snap_clean.ledger_corrupted);
        assert_eq!(snap_clean.phase, SessionPhase::Idle);
        assert!(!mgr.has_running_or_pending_session_sync());
        block_on(mgr.stop_session_for_exit()).unwrap();
    }

    #[test]
    fn cancel_during_baseline_sessions_skips_colab_new() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);
        let sname = default_session_name_for_email("alice@example.com");
        let sessions_entered = Arc::new(tokio::sync::Notify::new());
        let sessions_entered_srv = sessions_entered.clone();
        let allow_sessions_finish = Arc::new(tokio::sync::Notify::new());
        let allow_sessions_finish_srv = allow_sessions_finish.clone();
        let new_called = Arc::new(AtomicBool::new(false));
        let new_called_srv = new_called.clone();

        let runner = Arc::new(SmartRunner {
            responder: Box::new(move |spec| {
                let sname = sname.clone();
                let sessions_entered_srv = sessions_entered_srv.clone();
                let allow_sessions_finish_srv = allow_sessions_finish_srv.clone();
                let new_called_srv = new_called_srv.clone();
                let prog = spec.program.clone();
                let args = spec.args.clone();
                Box::pin(async move {
                    if prog == Path::new(crate::locate::SCUTIL_BIN)
                        || prog == Path::new(crate::locate::SSH_KEYGEN_BIN)
                    {
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    let sub = args
                        .iter()
                        .find(|a| !a.starts_with('-'))
                        .map(String::as_str);
                    let out = match sub {
                        Some("whoami") => "Email: alice@example.com\n".to_string(),
                        Some("usage") => "Current balance: 10.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0".to_string(),
                        Some("status") => format!("[colab] Session '{sname}' not found."),
                        Some("sessions") => {
                            sessions_entered_srv.notify_one();
                            allow_sessions_finish_srv.notified().await;
                            "[colab] No active sessions found on server.".to_string()
                        }
                        Some("new") => {
                            new_called_srv.store(true, Ordering::SeqCst);
                            String::new()
                        }
                        _ => String::new(),
                    };
                    Ok(CommandOutput::from_raw(Some(0), out, String::new()))
                })
            }),
        });

        let mgr = Arc::new(SessionManager::with_deps(
            data.clone(),
            Some(home),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            runner,
            Arc::new(MockSpawner),
        ));

        block_on(async {
            let mgr_start = mgr.clone();
            let handle =
                tokio::spawn(
                    async move { mgr_start.start_session(GpuTier::T4, false, false).await },
                );
            tokio::time::timeout(Duration::from_secs(5), sessions_entered.notified())
                .await
                .unwrap();

            let _ = mgr.cancel_start().await;
            allow_sessions_finish.notify_one();

            let err = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(!new_called.load(Ordering::SeqCst));
            assert!(!pending_session_path(&data).exists());
            assert!(matches!(
                err,
                CoreError::StartCancelledAfterAllocation {
                    stop_verified: true,
                    ..
                }
            ));
            assert_eq!(mgr.snapshot().await.phase, SessionPhase::Idle);
        });
    }

    #[test]
    fn stop_for_exit_spares_replacement_runtime_and_blocks_quit_when_uncertain_pending_remains() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);
        let sname = default_session_name_for_email("alice@example.com");

        save_json_0600(
            &active_session_path(&data),
            &ActiveSession {
                session_name: sname.clone(),
                owner_email: "alice@example.com".to_string(),
                endpoint_sha256: sha256_hex(b"m-s-old"),
                gpu: GpuTier::T4,
                high_mem: false,
                hardware_summary: "NVIDIA T4".to_string(),
                local_port: LOCAL_PROXY_PORT,
                drive_enabled: false,
                created_at_unix: 100,
            },
        )
        .unwrap();
        save_json_0600(
            &pending_session_path(&data),
            &PendingSession {
                session_name: sname.clone(),
                owner_email: "alice@example.com".to_string(),
                gpu: GpuTier::T4,
                high_mem: false,
                cancel_requested: false,
                uncertain_orphan: true,
                drive_enabled: false,
                created_at_unix: 90,
            },
        )
        .unwrap();

        let stop_calls = Arc::new(AtomicUsize::new(0));
        let stop_calls_srv = stop_calls.clone();
        let runner = Arc::new(SmartRunner {
            responder: Box::new(move |spec| {
                let sname = sname.clone();
                let stop_calls_srv = stop_calls_srv.clone();
                let prog = spec.program.clone();
                let args = spec.args.clone();
                Box::pin(async move {
                    if prog == Path::new(crate::locate::SCUTIL_BIN) {
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    let sub = args
                        .iter()
                        .find(|a| !a.starts_with('-'))
                        .map(String::as_str);
                    let out = match sub {
                        Some("whoami") => "Email: alice@example.com\n".to_string(),
                        Some("usage") => "Current balance: 10.00 compute units\nUsage rate: 1.50/hr\nActive assignments: 1".to_string(),
                        Some("status") => format!("[{sname}] m-s-replacement | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU"),
                        Some("sessions") => format!("[{sname}] m-s-replacement | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU"),
                        Some("stop") => {
                            stop_calls_srv.fetch_add(1, Ordering::SeqCst);
                            "[colab] Session terminated.".to_string()
                        }
                        _ => String::new(),
                    };
                    Ok(CommandOutput::from_raw(Some(0), out, String::new()))
                })
            }),
        });

        let mgr = SessionManager::with_deps(
            data.clone(),
            Some(home),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            runner,
            Arc::new(MockSpawner),
        );

        let err = block_on(mgr.stop_session_for_exit()).unwrap_err();
        assert_eq!(err.code, "STOP_UNVERIFIED");
        assert_eq!(
            stop_calls.load(Ordering::SeqCst),
            0,
            "must never invoke name-only colab stop against an external replacement runtime"
        );
        assert!(!active_session_path(&data).exists());
        assert!(pending_session_path(&data).exists());
        assert!(mgr.has_running_or_pending_session_sync());
        assert_eq!(block_on(mgr.snapshot()).phase, SessionPhase::Error);
    }

    struct MockDrivemountProc {
        rx: tokio::sync::mpsc::UnboundedReceiver<String>,
        tx_newline: Option<tokio::sync::oneshot::Sender<()>>,
        exit_code: Option<i32>,
    }

    impl crate::persistence::DrivemountProcess for MockDrivemountProc {
        fn next_line<'a>(&'a mut self) -> BoxFuture<'a, Option<DrivemountLine>> {
            Box::pin(async move {
                self.rx
                    .recv()
                    .await
                    .map(|raw| crate::persistence::parse_drivemount_line(&raw))
            })
        }
        fn send_newline<'a>(&'a mut self) -> BoxFuture<'a, Result<(), CoreError>> {
            Box::pin(async move {
                if let Some(tx) = self.tx_newline.take() {
                    let _ = tx.send(());
                }
                Ok(())
            })
        }
        fn wait<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<i32>, CoreError>> {
            Box::pin(async move { Ok(self.exit_code) })
        }
        fn kill<'a>(&'a mut self) -> BoxFuture<'a, Result<(), CoreError>> {
            Box::pin(async move { Ok(()) })
        }
    }

    struct MockDrivemountSpawner {
        succeed_on_continue: bool,
        drive_mounted_flag: Arc<AtomicBool>,
        spawns: Arc<AtomicUsize>,
    }

    impl crate::persistence::DrivemountSpawner for MockDrivemountSpawner {
        fn spawn(
            &self,
            _program: &Path,
            _args: &[String],
            _envs: &[(String, String)],
        ) -> Result<Box<dyn crate::persistence::DrivemountProcess>, CoreError> {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let (nl_tx, nl_rx) = tokio::sync::oneshot::channel();
            let mounted = self.drive_mounted_flag.clone();
            let ok = self.succeed_on_continue;
            tokio::spawn(async move {
                let _ = tx.send(
                    "REQUIRED: Google Drive Authorization needed. Visit https://accounts.google.com/o/oauth2/auth?mock=1 and press Enter."
                        .to_string(),
                );
                if ok {
                    let _ = nl_rx.await;
                    mounted.store(true, Ordering::SeqCst);
                    let _ = tx.send("Mounted at /content/drive".to_string());
                } else {
                    let _ = tx.send("unauthorized_redirect_uri: mount failed".to_string());
                }
            });
            Ok(Box::new(MockDrivemountProc {
                rx,
                tx_newline: Some(nl_tx),
                exit_code: Some(if self.succeed_on_continue { 0 } else { 1 }),
            }))
        }
    }

    #[test]
    fn drivemount_auth_prompt_confirm_and_nonblocking_stop_export() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);
        let sname = default_session_name_for_email("alice@example.com");
        let drive_mounted = Arc::new(AtomicBool::new(false));
        let dm_flag = drive_mounted.clone();
        let export_should_fail = Arc::new(AtomicBool::new(true));
        let exp_fail = export_should_fail.clone();

        let runner = Arc::new(SmartRunner {
            responder: Box::new(move |spec| {
                let sname = sname.clone();
                let dm_flag = dm_flag.clone();
                let exp_fail = exp_fail.clone();
                let prog = spec.program.clone();
                let args = spec.args.clone();
                let stdin_bytes = match &spec.stdin {
                    StdinPolicy::Bytes(b) => Some(b.clone()),
                    StdinPolicy::Null => None,
                };
                Box::pin(async move {
                    if prog == Path::new(crate::locate::SCUTIL_BIN)
                        || prog == Path::new(crate::locate::SSH_KEYGEN_BIN)
                    {
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    if prog == Path::new(crate::locate::SSH_BIN) {
                        if args.iter().any(|a| a == "bash -s") {
                            if let Some(ref b) = stdin_bytes {
                                let script = String::from_utf8_lossy(b);
                                if script.contains("DRIVE_PROBE=") {
                                    let out = if dm_flag.load(Ordering::SeqCst) {
                                        "DRIVE_PROBE=MOUNTED_HEALTHY\n"
                                    } else {
                                        "DRIVE_PROBE=NOT_MOUNTED\n"
                                    };
                                    return Ok(CommandOutput::from_raw(
                                        Some(0),
                                        out.to_string(),
                                        String::new(),
                                    ));
                                }
                                if script.contains("EXPORT_OK=1") {
                                    if exp_fail.load(Ordering::SeqCst) {
                                        return Ok(CommandOutput::from_raw(
                                            Some(1),
                                            String::new(),
                                            "simulated drive export failure".to_string(),
                                        ));
                                    }
                                    return Ok(CommandOutput::from_raw(
                                        Some(0),
                                        "FLUSH_OK=1\nEXPORT_OK=1\n".to_string(),
                                        String::new(),
                                    ));
                                }
                            }
                            return Ok(CommandOutput::from_raw(
                                Some(0),
                                format!("{}\n", crate::bootstrap::COMFY_READY_SENTINEL),
                                String::new(),
                            ));
                        }
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    let sub = args
                        .iter()
                        .find(|a| !a.starts_with('-'))
                        .map(String::as_str);
                    let out = match sub {
                        Some("whoami") => "Email: alice@example.com\n".to_string(),
                        Some("usage") => "Current balance: 10.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0".to_string(),
                        Some("status") => format!("[{sname}] m-s-drv | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU"),
                        Some("sessions") => "[colab] No active sessions found on server.".to_string(),
                        Some("stop") => "[colab] Session terminated.".to_string(),
                        _ => String::new(),
                    };
                    Ok(CommandOutput::from_raw(Some(0), out, String::new()))
                })
            }),
        });

        let dm_spawner = Arc::new(MockDrivemountSpawner {
            succeed_on_continue: true,
            drive_mounted_flag: drive_mounted.clone(),
            spawns: Arc::new(AtomicUsize::new(0)),
        });

        let mgr = Arc::new(
            SessionManager::with_deps(
                data.clone(),
                Some(home),
                Some(OsString::from("/usr/bin:/bin")),
                0,
                runner,
                Arc::new(MockSpawner),
            )
            .with_drivemount_spawner(dm_spawner.clone()),
        );
        fs::write(crate::config::ssh_key_path(&data), "priv").unwrap();
        fs::write(crate::config::ssh_pub_key_path(&data), "pub").unwrap();

        block_on(async {
            let mgr_start = mgr.clone();
            let handle =
                tokio::spawn(
                    async move { mgr_start.start_session(GpuTier::T4, false, false).await },
                );

            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(ref da) = mgr.snapshot().await.drive_auth {
                        if da.auth_url.is_some()
                            && da.status == crate::persistence::DriveAuthStatus::WaitingUser
                        {
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("drive_auth prompt should appear");

            let _ = mgr.confirm_drive_auth().await;
            let snap = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(snap.phase, SessionPhase::Ready);
            assert!(snap.active_session.as_ref().unwrap().drive_enabled);
            assert!(snap.drive_auth.is_none());
            assert_eq!(dm_spawner.spawns.load(Ordering::SeqCst), 1);

            // Stop when Drive export fails must NOT block colab stop and marks drive_sync_confirmed = Some(false).
            let report = mgr.stop_session().await.unwrap();
            assert!(report.terminated_sentinel);
            assert_eq!(report.drive_sync_confirmed, Some(false));
            assert!(report
                .warnings
                .iter()
                .any(|w| w.contains("Drive 同步未确认")));
        });
    }

    #[test]
    fn drivemount_failure_allows_retry_up_to_2_then_skip_to_no_drive() {
        let tmp = ShortTempDir::new();
        let home = tmp.0.join("h");
        let data = tmp.0.join("d");
        install_fake_colab(&home);
        let sname = default_session_name_for_email("alice@example.com");
        let drive_mounted = Arc::new(AtomicBool::new(false));
        let spawns = Arc::new(AtomicUsize::new(0));

        let runner = Arc::new(SmartRunner {
            responder: Box::new(move |spec| {
                let sname = sname.clone();
                let prog = spec.program.clone();
                let args = spec.args.clone();
                let stdin_bytes = match &spec.stdin {
                    StdinPolicy::Bytes(b) => Some(b.clone()),
                    StdinPolicy::Null => None,
                };
                Box::pin(async move {
                    if prog == Path::new(crate::locate::SCUTIL_BIN)
                        || prog == Path::new(crate::locate::SSH_KEYGEN_BIN)
                    {
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    if prog == Path::new(crate::locate::SSH_BIN) {
                        if args.iter().any(|a| a == "bash -s") {
                            if let Some(ref b) = stdin_bytes {
                                let script = String::from_utf8_lossy(b);
                                if script.contains("DRIVE_PROBE=") {
                                    return Ok(CommandOutput::from_raw(
                                        Some(0),
                                        "DRIVE_PROBE=NOT_MOUNTED\n".to_string(),
                                        String::new(),
                                    ));
                                }
                            }
                            return Ok(CommandOutput::from_raw(
                                Some(0),
                                format!("{}\n", crate::bootstrap::COMFY_READY_SENTINEL),
                                String::new(),
                            ));
                        }
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    let sub = args
                        .iter()
                        .find(|a| !a.starts_with('-'))
                        .map(String::as_str);
                    let out = match sub {
                        Some("whoami") => "Email: alice@example.com\n".to_string(),
                        Some("usage") => "Current balance: 10.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0".to_string(),
                        Some("status") => format!("[{sname}] m-s-drv2 | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU"),
                        Some("sessions") => "[colab] No active sessions found on server.".to_string(),
                        _ => String::new(),
                    };
                    Ok(CommandOutput::from_raw(Some(0), out, String::new()))
                })
            }),
        });

        let dm_spawner = Arc::new(MockDrivemountSpawner {
            succeed_on_continue: false,
            drive_mounted_flag: drive_mounted,
            spawns: spawns.clone(),
        });

        let mgr = Arc::new(
            SessionManager::with_deps(
                data.clone(),
                Some(home),
                Some(OsString::from("/usr/bin:/bin")),
                0,
                runner,
                Arc::new(MockSpawner),
            )
            .with_drivemount_spawner(dm_spawner),
        );
        fs::write(crate::config::ssh_key_path(&data), "priv").unwrap();
        fs::write(crate::config::ssh_pub_key_path(&data), "pub").unwrap();

        block_on(async {
            let mgr_start = mgr.clone();
            let handle =
                tokio::spawn(
                    async move { mgr_start.start_session(GpuTier::T4, false, false).await },
                );

            // Wait for attempt 1 failure -> click retry -> wait for attempt 2 failure -> click skip.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(ref da) = mgr.snapshot().await.drive_auth {
                        if da.status == crate::persistence::DriveAuthStatus::Failed
                            && da.attempt == 1
                        {
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            let _ = mgr.retry_drive_mount().await;

            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(ref da) = mgr.snapshot().await.drive_auth {
                        if da.status == crate::persistence::DriveAuthStatus::Failed
                            && da.attempt == 2
                        {
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            let _ = mgr.skip_drive_for_session().await;

            let snap = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(snap.phase, SessionPhase::Ready);
            assert!(!snap.active_session.as_ref().unwrap().drive_enabled);
            assert_eq!(spawns.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn manual_export_does_not_unmount_and_stop_without_tunnel_warns() {
        let tmp = ShortTempDir::new();
        let data = tmp.0.join("d");
        let home = tmp.0.join("h");
        install_fake_colab(&home);

        let sname = default_session_name_for_email("alice@example.com");
        let script_seen_unmount = Arc::new(StdMutex::new(Vec::<bool>::new()));
        let seen_clone = script_seen_unmount.clone();
        let drive_mounted = Arc::new(AtomicBool::new(true));

        let runner: Arc<dyn CommandRunner> = Arc::new(SmartRunner {
            responder: Box::new(move |spec| {
                let sname = sname.clone();
                let seen_clone = seen_clone.clone();
                let dm_flag = drive_mounted.clone();
                let prog = spec.program.clone();
                let args = spec.args.clone();
                let stdin_bytes = match &spec.stdin {
                    StdinPolicy::Bytes(b) => Some(b.clone()),
                    StdinPolicy::Null => None,
                };
                Box::pin(async move {
                    if prog == Path::new(crate::locate::SCUTIL_BIN)
                        || prog == Path::new(crate::locate::SSH_KEYGEN_BIN)
                    {
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    if prog == Path::new(crate::locate::SSH_BIN) {
                        if args.iter().any(|a| a == "bash -s") {
                            if let Some(ref b) = stdin_bytes {
                                let script = String::from_utf8_lossy(b);
                                if script.contains("DRIVE_PROBE=") {
                                    let out = if dm_flag.load(Ordering::SeqCst) {
                                        "DRIVE_PROBE=MOUNTED_HEALTHY\n"
                                    } else {
                                        "DRIVE_PROBE=NOT_MOUNTED\n"
                                    };
                                    return Ok(CommandOutput::from_raw(
                                        Some(0),
                                        out.to_string(),
                                        String::new(),
                                    ));
                                }
                                if script.contains("EXPORT_OK=1") {
                                    seen_clone
                                        .lock()
                                        .unwrap()
                                        .push(script.contains("UNMOUNT_DRIVE = True"));
                                    return Ok(CommandOutput::from_raw(
                                        Some(0),
                                        "FLUSH_OK=1\nEXPORT_OK=1\n".to_string(),
                                        String::new(),
                                    ));
                                }
                            }
                            return Ok(CommandOutput::from_raw(
                                Some(0),
                                format!("{}\n", crate::bootstrap::COMFY_READY_SENTINEL),
                                String::new(),
                            ));
                        }
                        return Ok(CommandOutput::from_raw(
                            Some(0),
                            String::new(),
                            String::new(),
                        ));
                    }
                    let sub = args
                        .iter()
                        .find(|a| !a.starts_with('-'))
                        .map(String::as_str);
                    let out = match sub {
                        Some("whoami") => "Email: alice@example.com\n".to_string(),
                        Some("usage") => "Current balance: 10.00 compute units\nUsage rate: 0.00/hr\nActive assignments: 0".to_string(),
                        Some("status") => format!("[{sname}] m-s-drv2 | Hardware: NVIDIA T4 | Shape: Standard | Variant: GPU"),
                        Some("sessions") => "[colab] No active sessions found on server.".to_string(),
                        Some("stop") => "[colab] Session terminated.".to_string(),
                        _ => String::new(),
                    };
                    Ok(CommandOutput::from_raw(Some(0), out, String::new()))
                })
            }),
        });

        let mgr = Arc::new(SessionManager::with_deps(
            data.clone(),
            Some(home),
            Some(OsString::from("/usr/bin:/bin")),
            0,
            runner,
            Arc::new(MockSpawner),
        ));
        fs::write(crate::config::ssh_key_path(&data), "priv").unwrap();
        fs::write(crate::config::ssh_pub_key_path(&data), "pub").unwrap();

        block_on(async {
            let snap = mgr.start_session(GpuTier::T4, false, false).await.unwrap();
            assert_eq!(snap.phase, SessionPhase::Ready);

            // H2: manual export uses os.sync() and does NOT call flush_and_unmount()
            let rep = mgr.export_to_drive_now(false).await.unwrap();
            assert!(rep.ok && rep.flush_ok && !rep.unmounted);
            assert_eq!(*script_seen_unmount.lock().unwrap(), vec![false]);

            // H3: simulate disconnected tunnel on stop -> drive_sync_confirmed = Some(false)
            mgr.teardown_local_handles().await;
            let stop_rep = mgr.stop_session().await.unwrap();
            assert_eq!(stop_rep.drive_sync_confirmed, Some(false));
        });
    }
}
