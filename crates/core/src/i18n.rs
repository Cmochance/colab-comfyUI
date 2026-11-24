use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Supported UI locales (Phase 1 populates `zh-Hans`; Phase 2 adds `en`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Locale {
    #[default]
    #[serde(rename = "zh-Hans")]
    ZhHans,
}

/// Stable localization message keys for console UI and error notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MsgKey {
    AppTitle,
    Subtitle,
    EnvColabFound,
    EnvColabMissing,
    EnvSshMissing,
    EnvSshKeygenMissing,
    AccountLabel,
    AccountNotSignedIn,
    BalanceLabel,
    RateLabel,
    ActiveAssignmentsLabel,
    GpuLabel,
    HighRamLabel,
    HighRamL4Disabled,
    BtnStart,
    BtnCancelStart,
    BtnOpenCanvas,
    BtnReconnect,
    BtnOpenColabUrl,
    BtnStop,
    BtnRefresh,
    BtnOpenColabPay,
    BtnOpenManageSessions,
    StepPreflight,
    StepCreatingSession,
    StepOpeningTunnel,
    StepBootstrapping,
    StepReady,
    ZeroBalanceTitle,
    ZeroBalanceBody,
    ZeroBalanceOpenPay,
    ZeroBalanceAllowTest,
    ZeroBalanceCancel,
    ExitTitle,
    ExitBody,
    ExitStopAndQuit,
    ExitLeaveRunning,
    ExitCancel,
    StartupPendingNotice,
    StartupActiveNotice,
    OrphanWarningTitle,
    OrphanWarningBody,
    AuthRequiredNotice,
    CorsHeaderBusyWarning,
    LogsTitle,
    LogsEmpty,
    StopVerifiedOk,
    StopWarningPrefix,
}

impl MsgKey {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AppTitle => "app_title",
            Self::Subtitle => "subtitle",
            Self::EnvColabFound => "env_colab_found",
            Self::EnvColabMissing => "env_colab_missing",
            Self::EnvSshMissing => "env_ssh_missing",
            Self::EnvSshKeygenMissing => "env_ssh_keygen_missing",
            Self::AccountLabel => "account_label",
            Self::AccountNotSignedIn => "account_not_signed_in",
            Self::BalanceLabel => "balance_label",
            Self::RateLabel => "rate_label",
            Self::ActiveAssignmentsLabel => "active_assignments_label",
            Self::GpuLabel => "gpu_label",
            Self::HighRamLabel => "high_ram_label",
            Self::HighRamL4Disabled => "high_ram_l4_disabled",
            Self::BtnStart => "btn_start",
            Self::BtnCancelStart => "btn_cancel_start",
            Self::BtnOpenCanvas => "btn_open_canvas",
            Self::BtnReconnect => "btn_reconnect",
            Self::BtnOpenColabUrl => "btn_open_colab_url",
            Self::BtnStop => "btn_stop",
            Self::BtnRefresh => "btn_refresh",
            Self::BtnOpenColabPay => "btn_open_colab_pay",
            Self::BtnOpenManageSessions => "btn_open_manage_sessions",
            Self::StepPreflight => "step_preflight",
            Self::StepCreatingSession => "step_creating_session",
            Self::StepOpeningTunnel => "step_opening_tunnel",
            Self::StepBootstrapping => "step_bootstrapping",
            Self::StepReady => "step_ready",
            Self::ZeroBalanceTitle => "zero_balance_title",
            Self::ZeroBalanceBody => "zero_balance_body",
            Self::ZeroBalanceOpenPay => "zero_balance_open_pay",
            Self::ZeroBalanceAllowTest => "zero_balance_allow_test",
            Self::ZeroBalanceCancel => "zero_balance_cancel",
            Self::ExitTitle => "exit_title",
            Self::ExitBody => "exit_body",
            Self::ExitStopAndQuit => "exit_stop_and_quit",
            Self::ExitLeaveRunning => "exit_leave_running",
            Self::ExitCancel => "exit_cancel",
            Self::StartupPendingNotice => "startup_pending_notice",
            Self::StartupActiveNotice => "startup_active_notice",
            Self::OrphanWarningTitle => "orphan_warning_title",
            Self::OrphanWarningBody => "orphan_warning_body",
            Self::AuthRequiredNotice => "auth_required_notice",
            Self::CorsHeaderBusyWarning => "cors_header_busy_warning",
            Self::LogsTitle => "logs_title",
            Self::LogsEmpty => "logs_empty",
            Self::StopVerifiedOk => "stop_verified_ok",
            Self::StopWarningPrefix => "stop_warning_prefix",
        }
    }

    pub const fn all() -> &'static [MsgKey] {
        &[
            Self::AppTitle,
            Self::Subtitle,
            Self::EnvColabFound,
            Self::EnvColabMissing,
            Self::EnvSshMissing,
            Self::EnvSshKeygenMissing,
            Self::AccountLabel,
            Self::AccountNotSignedIn,
            Self::BalanceLabel,
            Self::RateLabel,
            Self::ActiveAssignmentsLabel,
            Self::GpuLabel,
            Self::HighRamLabel,
            Self::HighRamL4Disabled,
            Self::BtnStart,
            Self::BtnCancelStart,
            Self::BtnOpenCanvas,
            Self::BtnReconnect,
            Self::BtnOpenColabUrl,
            Self::BtnStop,
            Self::BtnRefresh,
            Self::BtnOpenColabPay,
            Self::BtnOpenManageSessions,
            Self::StepPreflight,
            Self::StepCreatingSession,
            Self::StepOpeningTunnel,
            Self::StepBootstrapping,
            Self::StepReady,
            Self::ZeroBalanceTitle,
            Self::ZeroBalanceBody,
            Self::ZeroBalanceOpenPay,
            Self::ZeroBalanceAllowTest,
            Self::ZeroBalanceCancel,
            Self::ExitTitle,
            Self::ExitBody,
            Self::ExitStopAndQuit,
            Self::ExitLeaveRunning,
            Self::ExitCancel,
            Self::StartupPendingNotice,
            Self::StartupActiveNotice,
            Self::OrphanWarningTitle,
            Self::OrphanWarningBody,
            Self::AuthRequiredNotice,
            Self::CorsHeaderBusyWarning,
            Self::LogsTitle,
            Self::LogsEmpty,
            Self::StopVerifiedOk,
            Self::StopWarningPrefix,
        ]
    }
}

/// Looks up localized text for `key` under `locale`.
pub fn t(locale: Locale, key: MsgKey) -> &'static str {
    match locale {
        Locale::ZhHans => match key {
            MsgKey::AppTitle => "Colab ComfyUI",
            MsgKey::Subtitle => "在 Google Colab GPU 上安全运行 ComfyUI",
            MsgKey::EnvColabFound => "colab CLI 已就绪",
            MsgKey::EnvColabMissing => "未找到 colab 命令，请先安装 google-colab-cli",
            MsgKey::EnvSshMissing => "未找到 /usr/bin/ssh",
            MsgKey::EnvSshKeygenMissing => "未找到 /usr/bin/ssh-keygen",
            MsgKey::AccountLabel => "当前账号",
            MsgKey::AccountNotSignedIn => "未登录或凭证已失效",
            MsgKey::BalanceLabel => "计算单元余额",
            MsgKey::RateLabel => "当前消耗速率",
            MsgKey::ActiveAssignmentsLabel => "账号活跃实例",
            MsgKey::GpuLabel => "GPU 规格",
            MsgKey::HighRamLabel => "High-RAM 高内存",
            MsgKey::HighRamL4Disabled => "L4 仅提供单档内存，不支持 High-RAM",
            MsgKey::BtnStart => "一键启动 ComfyUI",
            MsgKey::BtnCancelStart => "取消开机（等待分配后停机）",
            MsgKey::BtnOpenCanvas => "打开 ComfyUI 画布",
            MsgKey::BtnReconnect => "重连本地隧道",
            MsgKey::BtnOpenColabUrl => "在浏览器打开 Colab 页",
            MsgKey::BtnStop => "停机并核验停止计费",
            MsgKey::BtnRefresh => "刷新状态",
            MsgKey::BtnOpenColabPay => "打开 colab pay 充值",
            MsgKey::BtnOpenManageSessions => "打开 Colab 管理会话",
            MsgKey::StepPreflight => "1. 账号与余额检查",
            MsgKey::StepCreatingSession => "2. 申请 GPU 会话",
            MsgKey::StepOpeningTunnel => "3. 建立 SSH 隧道与本地代理",
            MsgKey::StepBootstrapping => "4. 远端启动 ComfyUI",
            MsgKey::StepReady => "5. 画布已就绪",
            MsgKey::ZeroBalanceTitle => "计算单元余额为 0.00 CU",
            MsgKey::ZeroBalanceBody => "当前账号处于 0 CU 免费层或余额耗尽，GPU 配额可能受限或随时中断。建议先充值，或仅在二次确认后继续短时测试。",
            MsgKey::ZeroBalanceOpenPay => "前往 colab pay",
            MsgKey::ZeroBalanceAllowTest => "已知风险，继续短测",
            MsgKey::ZeroBalanceCancel => "取消",
            MsgKey::ExitTitle => "云端 Colab 会话仍在计费",
            MsgKey::ExitBody => "关闭窗口或退出 App 不会停止 Colab GPU 会话，计算单元会继续扣费。请选择：",
            MsgKey::ExitStopAndQuit => "立即停机并退出",
            MsgKey::ExitLeaveRunning => "保留云端运行仅退出（继续计费）",
            MsgKey::ExitCancel => "取消",
            MsgKey::StartupPendingNotice => "检测到上次开机尚未完成即退出，已完成启动对账，请确认是否继续或停机。",
            MsgKey::StartupActiveNotice => "上次退出时会话仍在云端运行并继续计费，可直接重连画布或停机。",
            MsgKey::OrphanWarningTitle => "检测到未命名孤儿实例 [?]",
            MsgKey::OrphanWarningBody => "colab CLI 无法停掉未命名的孤儿实例，请点击打开 Colab 网页（Runtime → Manage sessions）手动终止。",
            MsgKey::AuthRequiredNotice => "colab 登录凭证失效或未授权，请在终端运行 `colab whoami` 或重新登录后再试。",
            MsgKey::CorsHeaderBusyWarning => "检测到旧 ComfyUI 进程带 --enable-cors-header 且队列非空，请等待当前任务完成后重启以恢复同源保护。",
            MsgKey::LogsTitle => "运行日志（已脱敏）",
            MsgKey::LogsEmpty => "暂无日志",
            MsgKey::StopVerifiedOk => "会话已停机并核验停止计费。",
            MsgKey::StopWarningPrefix => "停机核验告警：",
        },
    }
}

/// Exports the full key-value lookup table for the Svelte frontend.
pub fn message_table(locale: Locale) -> BTreeMap<&'static str, &'static str> {
    MsgKey::all()
        .iter()
        .map(|&k| (k.as_str(), t(locale, k)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zh_hans_table_covers_all_keys_with_non_empty_text() {
        let table = message_table(Locale::ZhHans);
        assert_eq!(table.len(), MsgKey::all().len());
        for (k, v) in table {
            assert!(!v.trim().is_empty(), "empty translation for {k}");
        }
    }
}
