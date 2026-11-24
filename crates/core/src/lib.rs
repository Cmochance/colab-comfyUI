pub mod bootstrap;
pub mod colab_cli;
pub mod config;
pub mod i18n;
pub mod local_proxy;
pub mod locate;
pub mod runner;
pub mod ssh_tunnel;
pub mod webview_policy;

pub use locate::find_colab;

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;

use crate::bootstrap::{run_remote_bootstrap, BootstrapOutcome};
use crate::colab_cli::{SessionEntry, SessionStatusLookup, StopReport, UsageInfo};
use crate::config::{
    active_session_path, append_app_log_0600, config_path, ctl_socket_path,
    default_session_name_for_email, ensure_data_dirs, load_json_opt, pending_session_path,
    remove_if_exists, save_json_0600, validate_session_name, ActiveSession, AppConfig, GpuTier,
    PendingSession, StateLock, LOCAL_PROXY_PORT, REMOTE_COMFY_PORT,
};
use crate::i18n::message_table;
use crate::local_proxy::{start_local_proxy, LocalProxyHandle};
use crate::locate::{check_env, resolve_proxy_envs, EnvReport};
use crate::runner::{
    redact_sensitive_text, CommandRunner, LineCallback, StdinPolicy, TokioCommandRunner,
};
use crate::ssh_tunnel::{
    build_mux_exec_spec, start_master_and_verify, stop_master_tunnel, TokioTunnelSpawner,
    TunnelProcess, TunnelSpawner,
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
    CapacityOrSessionLimit { detail: String },

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
    pub orphan_count: usize,
    pub startup_notice: Option<StartupNotice>,
    pub warning_banner: Option<String>,
    pub last_error: Option<UiError>,
    pub last_stop_report: Option<StopReport>,
    pub tunnel_connected: bool,
    pub canvas_url: Option<String>,
    pub logs: Vec<String>,
    pub i18n: BTreeMap<String, String>,
}

#[derive(Default)]
struct RuntimeHandles {
    proxy: Option<LocalProxyHandle>,
    tunnel_proc: Option<Box<dyn TunnelProcess>>,
    ctl_sock: Option<PathBuf>,
    comfy_sock: Option<PathBuf>,
    supervisor_task: Option<JoinHandle<()>>,
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
    orphan_count: usize,
    startup_notice: Option<StartupNotice>,
    warning_banner: Option<String>,
    last_error: Option<UiError>,
    last_stop_report: Option<StopReport>,
    tunnel_connected: bool,
    logs: VecDeque<String>,
    has_managed_session: Arc<AtomicBool>,
}

impl InnerState {
    fn sync_managed_flag(&self) {
        self.has_managed_session.store(
            self.active_session.is_some() || self.pending_session.is_some(),
            Ordering::SeqCst,
        );
    }
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
    state: Arc<Mutex<InnerState>>,
    handles: Arc<Mutex<RuntimeHandles>>,
    start_in_flight: Arc<AtomicBool>,
    cancel_requested: Arc<AtomicBool>,
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
        let pending_session: Option<PendingSession> =
            load_json_opt(&pending_session_path(&data_dir))
                .ok()
                .flatten();
        let active_session: Option<ActiveSession> = load_json_opt(&active_session_path(&data_dir))
            .ok()
            .flatten();
        let has_managed_session = Arc::new(AtomicBool::new(
            active_session.is_some() || pending_session.is_some(),
        ));
        let env = check_env(path_env.as_deref(), home_dir.as_deref());
        let (snapshot_tx, _) = watch::channel(0u64);

        let inner = InnerState {
            config,
            phase: SessionPhase::Idle,
            step: None,
            env,
            account_email: None,
            auth_required: false,
            usage: None,
            active_session,
            pending_session,
            orphan_count: 0,
            startup_notice: None,
            warning_banner: None,
            last_error: None,
            last_stop_report: None,
            tunnel_connected: false,
            logs: VecDeque::with_capacity(MAX_LOG_LINES),
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
            state: Arc::new(Mutex::new(inner)),
            handles: Arc::new(Mutex::new(RuntimeHandles::default())),
            start_in_flight: Arc::new(AtomicBool::new(false)),
            cancel_requested: Arc::new(AtomicBool::new(false)),
            intentional_stop: Arc::new(AtomicBool::new(false)),
            has_managed_session,
            snapshot_tx,
        }
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
        Self::build_snapshot(&guard)
    }

    pub async fn has_running_or_pending_session(&self) -> bool {
        let guard = self.state.lock().await;
        guard.sync_managed_flag();
        guard.active_session.is_some() || guard.pending_session.is_some()
    }

    pub fn has_running_or_pending_session_sync(&self) -> bool {
        self.has_managed_session.load(Ordering::SeqCst)
    }

    fn build_snapshot(s: &InnerState) -> AppSnapshot {
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
            orphan_count: s.orphan_count,
            startup_notice: s.startup_notice.clone(),
            warning_banner: s.warning_banner.clone(),
            last_error: s.last_error.clone(),
            last_stop_report: s.last_stop_report.clone(),
            tunnel_connected: s.tunnel_connected,
            canvas_url,
            logs: s.logs.iter().cloned().collect(),
            i18n: table,
        }
    }

    fn notify_snapshot(&self) {
        let next = self.snapshot_tx.borrow().wrapping_add(1);
        let _ = self.snapshot_tx.send(next);
    }

    fn record_ui_error(data_dir: &Path, err: &UiError) {
        let _ = append_app_log_0600(data_dir, &format!("ERROR [{}] {}", err.code, err.message));
    }

    async fn push_log(&self, line: String) {
        let redacted = redact_sensitive_text(&line);
        let _ = append_app_log_0600(&self.data_dir, &redacted);
        let mut guard = self.state.lock().await;
        if guard.logs.len() >= MAX_LOG_LINES {
            guard.logs.pop_front();
        }
        guard.logs.push_back(redacted);
        drop(guard);
        self.notify_snapshot();
    }

    fn make_line_callback(&self) -> LineCallback {
        let state = self.state.clone();
        let tx = self.snapshot_tx.clone();
        let data_dir = self.data_dir.clone();
        Arc::new(move |line: String| {
            let state = state.clone();
            let tx = tx.clone();
            let data_dir = data_dir.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let redacted = redact_sensitive_text(&line);
                    let _ = append_app_log_0600(&data_dir, &redacted);
                    let mut guard = state.lock().await;
                    if guard.logs.len() >= MAX_LOG_LINES {
                        guard.logs.pop_front();
                    }
                    guard.logs.push_back(redacted);
                    drop(guard);
                    let next = tx.borrow().wrapping_add(1);
                    let _ = tx.send(next);
                });
            }
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
        let env = check_env(self.path_env.as_deref(), self.home_dir.as_deref());
        {
            let mut guard = self.state.lock().await;
            guard.env = env.clone();
        }

        let Ok(colab_bin) = self.require_colab_bin(&env) else {
            self.notify_snapshot();
            return self.snapshot().await;
        };

        let _ = self.refresh_account_and_sessions(&colab_bin).await;
        let proxy_envs = self.resolve_colab_envs().await;

        let pending: Option<PendingSession> = load_json_opt(&pending_session_path(&self.data_dir))
            .ok()
            .flatten();
        let active: Option<ActiveSession> = load_json_opt(&active_session_path(&self.data_dir))
            .ok()
            .flatten();

        if let Some(pend) = pending {
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
                        created_at_unix: pend.created_at_unix,
                    };
                    let _ = save_json_0600(&active_session_path(&self.data_dir), &promoted);
                    let _ = remove_if_exists(&pending_session_path(&self.data_dir));
                    let mut guard = self.state.lock().await;
                    guard.pending_session = None;
                    guard.sync_managed_flag();
                    guard.active_session = Some(promoted);
                    guard.sync_managed_flag();
                    guard.startup_notice = Some(StartupNotice {
                        kind: "pending_alive".to_string(),
                        message: format!(
                            "上次开机期间 App 退出，云端会话 {} 仍在计费中，可直接重连或停机。",
                            pend.session_name
                        ),
                        session_name: Some(pend.session_name),
                    });
                }
                Ok(SessionStatusLookup::Missing) => {
                    let _ = remove_if_exists(&pending_session_path(&self.data_dir));
                    let mut guard = self.state.lock().await;
                    guard.pending_session = None;
                    guard.sync_managed_flag();
                    guard.startup_notice = Some(StartupNotice {
                        kind: "pending_gone".to_string(),
                        message: format!(
                            "上次未完成的开机记录 {} 在云端已不存在，已清理本地残留记录。",
                            pend.session_name
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
        } else if let Some(act) = active {
            let current_email = { self.state.lock().await.account_email.clone() };
            if let Some(ref email) = current_email {
                if !email.eq_ignore_ascii_case(&act.owner_email) {
                    let mut guard = self.state.lock().await;
                    guard.startup_notice = Some(StartupNotice {
                        kind: "account_changed".to_string(),
                        message: format!(
                            "上次退出时账号 {} 仍有会话 {}，但当前 colab 已切换为 {}。",
                            act.owner_email, act.session_name, email
                        ),
                        session_name: Some(act.session_name),
                    });
                    self.notify_snapshot();
                    return self.snapshot().await;
                }
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
                    let _ = remove_if_exists(&active_session_path(&self.data_dir));
                    let mut guard = self.state.lock().await;
                    guard.active_session = None;
                    guard.sync_managed_flag();
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

    async fn refresh_account_and_sessions(&self, colab_bin: &Path) -> Result<(), CoreError> {
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
        guard.account_email = Some(email);
        match usage_res {
            Ok(u) => guard.usage = Some(u),
            Err(err) => {
                let ui_err = UiError::from(&err);
                Self::record_ui_error(&self.data_dir, &ui_err);
                guard.last_error = Some(ui_err);
            }
        }
        if let Ok(list) = sessions_res {
            guard.orphan_count = list.iter().filter(|s| s.is_orphan).count();
        }
        Ok(())
    }

    pub async fn cancel_start(&self) -> AppSnapshot {
        self.cancel_requested.store(true, Ordering::SeqCst);
        self.push_log(
            "已请求取消开机：若正在执行 colab new，将等待分配完成后自动停机核验。".to_string(),
        )
        .await;
        if let Ok(Some(mut pend)) =
            load_json_opt::<PendingSession>(&pending_session_path(&self.data_dir))
        {
            pend.cancel_requested = true;
            let _ = save_json_0600(&pending_session_path(&self.data_dir), &pend);
            let mut guard = self.state.lock().await;
            guard.pending_session = Some(pend);
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
        self.intentional_stop.store(false, Ordering::SeqCst);

        let res = self
            .start_session_inner(gpu, high_mem, allow_zero_balance)
            .await;
        self.start_in_flight.store(false, Ordering::SeqCst);

        if let Err(ref err) = res {
            let mut guard = self.state.lock().await;
            if !matches!(err, CoreError::ZeroBalanceBlocked { .. }) {
                let ui_err = UiError::from(err);
                Self::record_ui_error(&self.data_dir, &ui_err);
                guard.phase = SessionPhase::Error;
                guard.last_error = Some(ui_err);
            } else {
                guard.phase = SessionPhase::Idle;
                guard.step = None;
            }
            drop(guard);
            self.notify_snapshot();
        }
        res
    }

    async fn start_session_inner(
        &self,
        gpu: GpuTier,
        high_mem: bool,
        allow_zero_balance: bool,
    ) -> Result<AppSnapshot, CoreError> {
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

        {
            let mut guard = self.state.lock().await;
            guard.env = env;
            guard.phase = SessionPhase::Starting;
            guard.step = Some(StartStep::Preflight);
            guard.last_error = None;
            guard.warning_banner = None;
            guard.startup_notice = None;
            guard.config.default_gpu = gpu;
            guard.config.high_mem = high_mem && gpu.supports_high_mem();
            let _ = save_json_0600(&config_path(&self.data_dir), &guard.config);
        }
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

        // Phase 1 ledger + collision check inside state.lock
        let already_exists = {
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
                SessionStatusLookup::Present(entry) => Some(entry),
                SessionStatusLookup::Missing => {
                    let pend = PendingSession {
                        session_name: session_name.clone(),
                        owner_email: owner_email.clone(),
                        gpu,
                        high_mem: high_mem && gpu.supports_high_mem(),
                        cancel_requested: false,
                        created_at_unix: now_unix,
                    };
                    save_json_0600(&pending_session_path(&self.data_dir), &pend)?;
                    let mut guard = self.state.lock().await;
                    guard.pending_session = Some(pend);
                    guard.sync_managed_flag();
                    None
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

            // Never kill `colab new`; wait for it to return and check `cancel_requested`.
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
                let _ = remove_if_exists(&pending_session_path(&self.data_dir));
                let mut guard = self.state.lock().await;
                guard.pending_session = None;
                guard.sync_managed_flag();
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
            let entry = match status_lookup {
                SessionStatusLookup::Present(e) => e,
                SessionStatusLookup::Missing => {
                    let _ = remove_if_exists(&pending_session_path(&self.data_dir));
                    let mut guard = self.state.lock().await;
                    guard.pending_session = None;
                    guard.sync_managed_flag();
                    return Err(CoreError::SessionMissing { session_name });
                }
            };

            if self.cancel_requested.load(Ordering::SeqCst) {
                self.push_log("分配已返回，按取消请求立即执行停机与核验...".to_string())
                    .await;
                let report = colab_cli::stop_and_verify(
                    self.runner.as_ref(),
                    &colab_bin,
                    &session_name,
                    &owner_email,
                    Some(&entry.endpoint_sha256),
                    &proxy_envs,
                    cb.clone(),
                )
                .await?;
                let _ = remove_if_exists(&pending_session_path(&self.data_dir));
                let mut guard = self.state.lock().await;
                guard.pending_session = None;
                guard.sync_managed_flag();
                guard.last_stop_report = Some(report.clone());
                guard.phase = SessionPhase::Idle;
                guard.step = None;
                return Err(CoreError::StartCancelledAfterAllocation {
                    session_name,
                    stop_verified: report.billing_verified_stopped,
                    detail: report.summary,
                });
            }

            entry
        };

        let active = ActiveSession {
            session_name: session_name.clone(),
            owner_email,
            endpoint_sha256: entry.endpoint_sha256,
            gpu,
            high_mem: high_mem && gpu.supports_high_mem(),
            hardware_summary: entry.hardware,
            local_port: self.local_proxy_port,
            created_at_unix: now_unix,
        };
        {
            let _lock = StateLock::acquire(&self.data_dir, LOCK_TIMEOUT).await?;
            save_json_0600(&active_session_path(&self.data_dir), &active)?;
            remove_if_exists(&pending_session_path(&self.data_dir))?;
        }
        {
            let mut guard = self.state.lock().await;
            guard.pending_session = None;
            guard.active_session = Some(active.clone());
            guard.sync_managed_flag();
        }
        self.notify_snapshot();

        self.open_tunnel_and_bootstrap(&colab_bin, &active, true)
            .await?;
        let _ = self.refresh_account_and_sessions(&colab_bin).await;
        self.notify_snapshot();
        Ok(self.snapshot().await)
    }

    pub async fn reconnect_tunnel(&self) -> Result<AppSnapshot, CoreError> {
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

        self.open_tunnel_and_bootstrap(&colab_bin, &active, true)
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
    ) -> Result<(), CoreError> {
        self.teardown_local_handles().await;

        {
            let mut guard = self.state.lock().await;
            guard.step = Some(StartStep::OpeningTunnel);
        }
        self.notify_snapshot();

        let cb = Some(self.make_line_callback());
        let proxy_envs = self.resolve_colab_envs().await;
        let started = start_master_and_verify(
            &self.data_dir,
            colab_bin,
            active,
            self.runner.as_ref(),
            self.spawner.as_ref(),
            &proxy_envs,
            cb.clone(),
        )
        .await?;

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

        if run_bootstrap {
            {
                let mut guard = self.state.lock().await;
                guard.step = Some(StartStep::Bootstrapping);
            }
            self.notify_snapshot();
            self.push_log("正在通过复用 SSH 通道检查并启动远端 ComfyUI...".to_string())
                .await;

            match run_remote_bootstrap(
                &started.ctl_sock,
                REMOTE_COMFY_PORT,
                self.runner.as_ref(),
                cb,
            )
            .await
            {
                Ok(BootstrapOutcome {
                    cors_header_busy_warning,
                }) => {
                    if cors_header_busy_warning {
                        let mut guard = self.state.lock().await;
                        guard.warning_banner = Some(
                            "检测到旧 ComfyUI 进程带 --enable-cors-header 且队列非空，请在任务结束后重启以恢复同源保护。".to_string(),
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
        Ok(())
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
                let _ = snapshot_tx.send(snapshot_tx.borrow().wrapping_add(1));

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
                            let _ = snapshot_tx.send(snapshot_tx.borrow().wrapping_add(1));
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
                            let _ = snapshot_tx.send(snapshot_tx.borrow().wrapping_add(1));
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
                    let _ = snapshot_tx.send(snapshot_tx.borrow().wrapping_add(1));
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
        let (proxy, mut tunnel_proc, ctl_sock, comfy_sock, sup) = {
            let mut h = self.handles.lock().await;
            (
                h.proxy.take(),
                h.tunnel_proc.take(),
                h.ctl_sock.take(),
                h.comfy_sock.take(),
                h.supervisor_task.take(),
            )
        };
        if let Some(task) = sup {
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
        self.intentional_stop.store(true, Ordering::SeqCst);
        {
            let mut guard = self.state.lock().await;
            guard.phase = SessionPhase::Stopping;
        }
        self.notify_snapshot();
        self.teardown_local_handles().await;

        let env = check_env(self.path_env.as_deref(), self.home_dir.as_deref());
        let colab_bin = self.require_colab_bin(&env)?;

        let (session_name, owner_email, endpoint_sha256) = {
            let guard = self.state.lock().await;
            if let Some(ref act) = guard.active_session {
                (
                    act.session_name.clone(),
                    act.owner_email.clone(),
                    Some(act.endpoint_sha256.clone()),
                )
            } else if let Some(ref pend) = guard.pending_session {
                (pend.session_name.clone(), pend.owner_email.clone(), None)
            } else {
                return Err(CoreError::SessionLedgerMismatch {
                    detail: "当前没有可停机的会话".to_string(),
                });
            }
        };

        let cb = Some(self.make_line_callback());
        let proxy_envs = self.resolve_colab_envs().await;
        let report = colab_cli::stop_and_verify(
            self.runner.as_ref(),
            &colab_bin,
            &session_name,
            &owner_email,
            endpoint_sha256.as_deref(),
            &proxy_envs,
            cb,
        )
        .await?;

        if report.terminated_sentinel && report.endpoint_absent && report.owner_verified {
            let _lock = StateLock::acquire(&self.data_dir, LOCK_TIMEOUT).await?;
            let _ = remove_if_exists(&active_session_path(&self.data_dir));
            let _ = remove_if_exists(&pending_session_path(&self.data_dir));
        }

        {
            let mut guard = self.state.lock().await;
            if report.terminated_sentinel && report.endpoint_absent && report.owner_verified {
                guard.active_session = None;
                guard.pending_session = None;
                guard.sync_managed_flag();
                guard.phase = SessionPhase::Idle;
                guard.step = None;
            } else {
                guard.phase = SessionPhase::Error;
            }
            guard.usage = report.usage.clone();
            guard.last_stop_report = Some(report.clone());
        }
        let _ = self.refresh_account_and_sessions(&colab_bin).await;
        self.notify_snapshot();
        Ok(report)
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
}
