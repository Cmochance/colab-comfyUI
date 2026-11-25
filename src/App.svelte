<script lang="ts">
  import { getVersion } from '@tauri-apps/api/app';
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';

  type GpuTier = 'T4' | 'L4' | 'G4' | 'A100' | 'H100';
  type SessionPhase =
    | 'Idle'
    | 'Starting'
    | 'Ready'
    | 'Reconnecting'
    | 'Stopping'
    | 'DisconnectedOrMissing'
    | 'Error';
  type StartStep = 'Preflight' | 'CreatingSession' | 'OpeningTunnel' | 'Bootstrapping' | 'Ready';

  interface EnvReport {
    colab_path: string | null;
    ssh_available: boolean;
    ssh_keygen_available: boolean;
    agy_cs_path: string | null;
    agy_switcher_app_path: string | null;
  }

  interface UsageInfo {
    current_balance: number;
    usage_rate_hourly: number;
    active_assignments: number;
  }

  interface ActiveSession {
    session_name: string;
    owner_email: string;
    endpoint_sha256: string;
    gpu: GpuTier;
    high_mem: boolean;
    hardware_summary: string;
    local_port: number;
    drive_enabled: boolean;
    created_at_unix: number;
  }

  interface PendingSession {
    session_name: string;
    owner_email: string;
    gpu: GpuTier;
    high_mem: boolean;
    cancel_requested: boolean;
    uncertain_orphan: boolean;
    drive_enabled: boolean;
    created_at_unix: number;
  }

  interface StartupNotice {
    kind: string;
    message: string;
    session_name: string | null;
  }

  interface UiError {
    code: string;
    message: string;
  }

  interface StopReport {
    session_name: string;
    terminated_sentinel: boolean;
    already_gone: boolean;
    endpoint_absent: boolean;
    owner_verified: boolean;
    safe_to_clear_ledger: boolean;
    usage: UsageInfo | null;
    billing_verified_stopped: boolean;
    drive_sync_confirmed?: boolean | null;
    summary: string;
    warnings: string[];
  }

  interface DriveAuthPrompt {
    auth_url: string | null;
    started_at_unix: number;
    deadline_unix: number;
    grace_deadline_unix?: number | null;
    attempt: number;
    max_attempts: number;
    status: 'WaitingUser' | 'Continuing' | 'Failed';
    message: string | null;
  }

  interface DriveExportReport {
    ok: boolean;
    flush_ok: boolean;
    workflow_updated: boolean;
    user_files: number;
    output_files: number;
    input_files: number;
    custom_nodes_count: number;
    summary: string;
  }

  interface TelemetrySample {
    timestamp_unix: number;
    gpu_util_pct: number | null;
    vram_pct: number | null;
    ram_pct: number | null;
  }

  interface TelemetrySnapshot {
    gpu_name: string | null;
    gpu_util_pct: number | null;
    gpu_temp_c: number | null;
    vram_used_bytes: number | null;
    vram_total_bytes: number | null;
    ram_used_bytes: number | null;
    ram_total_bytes: number | null;
    queue_running: number;
    queue_pending: number;
    content_free_bytes: number | null;
    content_total_bytes: number | null;
    drive_free_bytes: number | null;
    drive_total_bytes: number | null;
    estimated_remaining_minutes: number | null;
    samples: TelemetrySample[];
  }

  interface AgyColabAccount {
    name: string;
    email: string;
    balance_cu: number | null;
    usage_rate_hourly: number | null;
    active_assignments: number | null;
    is_current: boolean;
  }

  interface WatchdogSnapshot {
    enabled: boolean;
    threshold_minutes: number | null;
    phase: 'Disabled' | 'Counting' | 'Warning' | 'Stopping';
    idle_secs: number;
    warning_remaining_secs: number | null;
    sleep_wake_notice: boolean;
  }

  interface AppSnapshot {
    phase: SessionPhase;
    step: StartStep | null;
    env: EnvReport;
    config: {
      locale: 'zh-Hans' | 'en';
      default_gpu: GpuTier;
      high_mem: boolean;
      use_drive: boolean;
      export_outputs_on_stop: boolean;
      drive_export_timeout_secs: number;
      watchdog_idle_minutes: number | null;
      privacy_mask: boolean;
    };
    account_email: string | null;
    auth_required: boolean;
    usage: UsageInfo | null;
    agy_accounts: AgyColabAccount[];
    account_drift_detected: boolean;
    low_balance_warning: boolean;
    active_session: ActiveSession | null;
    pending_session: PendingSession | null;
    ledger_corrupted: boolean;
    orphan_count: number;
    startup_notice: StartupNotice | null;
    warning_banner: string | null;
    last_error: UiError | null;
    last_stop_report: StopReport | null;
    tunnel_connected: boolean;
    canvas_url: string | null;
    drive_auth: DriveAuthPrompt | null;
    telemetry: TelemetrySnapshot;
    watchdog: WatchdogSnapshot;
    last_export_report: DriveExportReport | null;
    logs: string[];
    i18n: Record<string, string>;
  }

  const GPU_OPTIONS: { value: GpuTier; label: string; note: string }[] = [
    { value: 'L4', label: 'L4 (24GB)', note: '默认均衡' },
    { value: 'T4', label: 'T4 (16GB)', note: '省单元' },
    { value: 'G4', label: 'G4 (Blackwell)', note: '高显存新卡' },
    { value: 'A100', label: 'A100 (40/80GB)', note: '旗舰推理' },
    { value: 'H100', label: 'H100 (80GB)', note: '最高算力' }
  ];

  const STEPS: { key: StartStep; labelKey: string; fallback: string }[] = [
    { key: 'Preflight', labelKey: 'step_preflight', fallback: '1. 账号与余额检查' },
    { key: 'CreatingSession', labelKey: 'step_creating_session', fallback: '2. 申请 GPU 会话' },
    { key: 'OpeningTunnel', labelKey: 'step_opening_tunnel', fallback: '3. 建立 SSH 隧道与本地代理' },
    { key: 'Bootstrapping', labelKey: 'step_bootstrapping', fallback: '4. 远端启动 ComfyUI' },
    { key: 'Ready', labelKey: 'step_ready', fallback: '5. 画布已就绪' }
  ];

  let version = $state('');
  let snap = $state<AppSnapshot | null>(null);
  let selectedGpu = $state<GpuTier>('L4');
  let highMem = $state(false);
  let useDrive = $state(true);
  let exportOutputsOnStop = $state(true);
  let driveExportTimeoutSecs = $state(90);
  let privacyMask = $state(false);
  let configSynced = $state(false);
  let starting = $state(false);
  let stopping = $state(false);
  let reconnecting = $state(false);
  let refreshing = $state(false);
  let exporting = $state(false);
  let exitBusy = $state(false);
  let agyBusy = $state(false);
  let logsExpanded = $state(false);
  let showZeroCuModal = $state(false);
  let showExitModal = $state(false);
  let actionError = $state<UiError | null>(null);
  let elapsedSecs = $state(0);
  let driveRemainingSecs = $state(0);
  let driveGraceRemainingSecs = $state(0);
  let watchdogWarningSecs = $state(0);

  const busy = $derived(
    starting || stopping || reconnecting || refreshing || exporting || exitBusy || agyBusy
  );
  const t = (key: string, fallback: string) => snap?.i18n?.[key] ?? fallback;

  $effect(() => {
    const wd = snap?.watchdog;
    if (!snap?.active_session || wd?.phase !== 'Warning' || wd.warning_remaining_secs == null) {
      watchdogWarningSecs = 0;
      return;
    }
    watchdogWarningSecs = wd.warning_remaining_secs;
    const timer = setInterval(() => {
      watchdogWarningSecs = Math.max(0, watchdogWarningSecs - 1);
    }, 1000);
    return () => clearInterval(timer);
  });

  function formatGiB(bytes: number | null | undefined): string {
    if (bytes == null) return '--';
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
  }

  function formatDurationMinutes(mins: number | null | undefined): string {
    if (mins == null) return t('cu_unlimited_or_idle', '0.00/hr');
    const h = Math.floor(mins / 60);
    const m = mins % 60;
    return h > 0 ? `${h}h ${m}m` : `${m}m`;
  }

  function svgPolyline(samples: TelemetrySample[], pick: (s: TelemetrySample) => number | null): string {
    const W = 320;
    const H = 80;
    const n = samples.length;
    if (n === 0) return '';
    const pts: string[] = [];
    for (let i = 0; i < n; i++) {
      const val = pick(samples[i]);
      if (val == null || Number.isNaN(val)) continue;
      const x = n === 1 ? W : (i / (n - 1)) * W;
      const clamped = Math.max(0, Math.min(100, val));
      const y = H - (clamped / 100) * (H - 8) - 4;
      pts.push(`${x.toFixed(1)},${y.toFixed(1)}`);
    }
    return pts.join(' ');
  }

  const highMemSupported = $derived(selectedGpu !== 'L4');
  const isTimedPhase = $derived(snap?.phase === 'Starting' || snap?.phase === 'Reconnecting');
  const activeStepIndex = $derived.by(() => {
    if (!snap?.step) return -1;
    return STEPS.findIndex((s) => s.key === snap?.step);
  });

  $effect(() => {
    if (!highMemSupported && highMem) {
      highMem = false;
    }
  });

  $effect(() => {
    if (!isTimedPhase) {
      elapsedSecs = 0;
      return;
    }
    elapsedSecs = 0;
    const startedAt = Date.now();
    const timer = setInterval(() => {
      elapsedSecs = Math.floor((Date.now() - startedAt) / 1000);
    }, 1000);
    return () => clearInterval(timer);
  });

  $effect(() => {
    const da = snap?.drive_auth;
    if (!da) {
      driveRemainingSecs = 0;
      driveGraceRemainingSecs = 0;
      return;
    }
    const updateCountdown = () => {
      const nowSec = Math.floor(Date.now() / 1000);
      driveRemainingSecs = Math.max(0, da.deadline_unix - nowSec);
      driveGraceRemainingSecs = da.grace_deadline_unix
        ? Math.max(0, da.grace_deadline_unix - nowSec)
        : 0;
    };
    updateCountdown();
    const timer = setInterval(updateCountdown, 1000);
    return () => clearInterval(timer);
  });

  $effect(() => {
    let unlistenSnapshot: (() => void) | undefined;
    let unlistenExit: (() => void) | undefined;

    getVersion().then((v) => (version = v));
    invoke<AppSnapshot>('reconcile_startup').then((s) => {
      snap = s;
      if (!configSynced) {
        selectedGpu = s.config.default_gpu;
        highMem = s.config.default_gpu !== 'L4' && s.config.high_mem;
        useDrive = s.config.use_drive;
        exportOutputsOnStop = s.config.export_outputs_on_stop;
        driveExportTimeoutSecs = s.config.drive_export_timeout_secs ?? 90;
        privacyMask = s.config.privacy_mask;
        configSynced = true;
      }
    });

    listen<AppSnapshot>('snapshot-updated', (ev) => {
      snap = ev.payload;
    }).then((fn) => (unlistenSnapshot = fn));

    listen('exit-confirm-requested', () => {
      showExitModal = true;
    }).then((fn) => (unlistenExit = fn));

    return () => {
      unlistenSnapshot?.();
      unlistenExit?.();
    };
  });

  async function runRefresh() {
    refreshing = true;
    actionError = null;
    try {
      snap = await invoke<AppSnapshot>('refresh_status');
    } catch (err) {
      actionError = err as UiError;
    } finally {
      refreshing = false;
    }
  }

  async function runReconcile() {
    refreshing = true;
    actionError = null;
    try {
      snap = await invoke<AppSnapshot>('reconcile_startup');
    } catch (err) {
      actionError = err as UiError;
    } finally {
      refreshing = false;
    }
  }

  async function runStart(allowZeroBalance: boolean) {
    starting = true;
    actionError = null;
    showZeroCuModal = false;
    logsExpanded = true;
    try {
      snap = await invoke<AppSnapshot>('start_session', {
        gpu: selectedGpu,
        highMem: highMemSupported ? highMem : false,
        allowZeroBalance
      });
    } catch (err) {
      const uiErr = err as UiError;
      if (uiErr?.code === 'ZERO_BALANCE_BLOCKED') {
        showZeroCuModal = true;
      } else {
        actionError = uiErr;
        logsExpanded = true;
      }
    } finally {
      starting = false;
    }
  }

  async function runCancelStart() {
    actionError = null;
    snap = await invoke<AppSnapshot>('cancel_start');
  }

  async function runReconnect() {
    reconnecting = true;
    actionError = null;
    try {
      snap = await invoke<AppSnapshot>('reconnect_tunnel');
    } catch (err) {
      actionError = err as UiError;
      logsExpanded = true;
    } finally {
      reconnecting = false;
    }
  }

  async function runOpenCanvas() {
    actionError = null;
    try {
      await invoke('open_canvas_window');
    } catch (err) {
      actionError = err as UiError;
    }
  }

  async function runOpenColabUrl() {
    actionError = null;
    try {
      await invoke('open_colab_url');
    } catch (err) {
      actionError = err as UiError;
    }
  }

  async function runStop() {
    stopping = true;
    actionError = null;
    try {
      await invoke<StopReport>('stop_session');
      snap = await invoke<AppSnapshot>('get_snapshot');
    } catch (err) {
      actionError = err as UiError;
    } finally {
      stopping = false;
    }
  }

  async function runOpenPay() {
    actionError = null;
    try {
      await invoke('open_colab_pay');
    } catch (err) {
      actionError = err as UiError;
    }
  }

  async function runOpenManageSessions() {
    actionError = null;
    try {
      await invoke('open_colab_manage_sessions');
    } catch (err) {
      actionError = err as UiError;
    }
  }

  async function runStopAndExit() {
    exitBusy = true;
    actionError = null;
    try {
      await invoke('stop_and_exit');
    } catch (err) {
      showExitModal = false;
      actionError = err as UiError;
      exitBusy = false;
    }
  }

  async function runLeaveRunningAndExit() {
    await invoke('leave_running_and_exit');
  }

  async function savePreferences(patch: {
    locale?: 'zh-Hans' | 'en';
    useDrive?: boolean;
    exportOutputsOnStop?: boolean;
    privacyMask?: boolean;
    driveExportTimeoutSecs?: number;
  }) {
    try {
      snap = await invoke<AppSnapshot>('update_preferences', {
        locale: patch.locale ?? null,
        useDrive: patch.useDrive ?? null,
        exportOutputsOnStop: patch.exportOutputsOnStop ?? null,
        privacyMask: patch.privacyMask ?? null,
        driveExportTimeoutSecs: patch.driveExportTimeoutSecs ?? null
      });
    } catch (err) {
      actionError = err as UiError;
    }
  }

  async function runToggleLocale() {
    const next = snap?.config.locale === 'en' ? 'zh-Hans' : 'en';
    await savePreferences({ locale: next });
  }

  async function runTogglePrivacyMask() {
    privacyMask = !privacyMask;
    await savePreferences({ privacyMask });
  }

  async function runExportDriveNow() {
    exporting = true;
    actionError = null;
    try {
      await invoke<DriveExportReport>('export_to_drive_now', {
        includeOutputs: exportOutputsOnStop
      });
      snap = await invoke<AppSnapshot>('get_snapshot');
    } catch (err) {
      actionError = err as UiError;
    } finally {
      exporting = false;
    }
  }

  async function runReopenDriveAuthUrl(url: string | null) {
    if (!url) return;
    try {
      await invoke('open_external_url', { url });
    } catch (err) {
      actionError = err as UiError;
    }
  }

  async function runConfirmDriveAuth() {
    snap = await invoke<AppSnapshot>('confirm_drive_auth');
  }

  async function runRetryDriveMount() {
    snap = await invoke<AppSnapshot>('retry_drive_mount');
  }

  async function runSkipDriveForSession() {
    snap = await invoke<AppSnapshot>('skip_drive_for_session');
  }

  async function runSetWatchdogMinutes(val: string) {
    const minutes = val === 'off' ? null : Number(val);
    try {
      snap = await invoke<AppSnapshot>('set_watchdog_minutes', { minutes });
    } catch (err) {
      actionError = err as UiError;
    }
  }

  async function runExtendWatchdog() {
    snap = await invoke<AppSnapshot>('extend_watchdog', { extraMinutes: 30 });
  }

  async function runDismissSleepNotice() {
    snap = await invoke<AppSnapshot>('dismiss_watchdog_sleep_notice');
  }

  async function runRefreshAgyAccounts(refreshQuota: boolean) {
    agyBusy = true;
    try {
      snap = await invoke<AppSnapshot>('refresh_agy_accounts', { refreshQuota });
    } catch (err) {
      actionError = err as UiError;
    } finally {
      agyBusy = false;
    }
  }

  async function runSwitchAgyAccount(selectEl: HTMLSelectElement) {
    const name = selectEl.value;
    if (!name) return;
    agyBusy = true;
    try {
      actionError = null;
      snap = await invoke<AppSnapshot>('switch_agy_account', { name });
    } catch (err) {
      actionError = err as UiError;
      const cur = snap?.agy_accounts.find((a) => a.is_current)?.name ?? '';
      if (cur) selectEl.value = cur;
    } finally {
      agyBusy = false;
    }
  }

  async function runSwitchBackToOwner() {
    agyBusy = true;
    try {
      actionError = null;
      snap = await invoke<AppSnapshot>('switch_back_to_owner_account');
    } catch (err) {
      actionError = err as UiError;
    } finally {
      agyBusy = false;
    }
  }

  async function runOpenAgySwitcher() {
    try {
      const res = await invoke<string>('open_agy_switcher_or_guide');
      if (res === 'guide_terminal') {
        actionError = {
          code: 'AGY_SWITCHER_NOT_FOUND',
          message: t(
            'agy_switcher_not_found',
            '未检测到 Agy Switcher.app，请在终端执行 `colab whoami` 登录或安装 agy-cs。'
          )
        };
      }
    } catch (err) {
      actionError = err as UiError;
    }
  }
</script>

<div class="app-shell" class:masked={privacyMask}>
<main>
  <header class="topbar">
    <div>
      <h1>{t('app_title', 'Colab ComfyUI')}</h1>
      <p class="subtitle">
        {t('subtitle', '在 Google Colab GPU 上安全运行 ComfyUI')} · {version ? `v${version}` : ''}
      </p>
    </div>
    <div class="topbar-actions">
      <button class="secondary" onclick={runToggleLocale}>
        {snap?.config.locale === 'en' ? '中文' : 'EN'}
      </button>
      <button class="secondary" onclick={runTogglePrivacyMask}>
        {privacyMask ? '👁 ' : ''}{t('privacy_mask_label', '隐私打码')}
      </button>
      <span class="phase-badge phase-{snap?.phase?.toLowerCase() ?? 'idle'}">
        {snap?.phase ?? 'Idle'}
      </span>
      <button class="secondary" onclick={runRefresh} disabled={busy}>
        {t('btn_refresh', '刷新状态')}
      </button>
    </div>
  </header>

  {#if snap?.auth_required}
    <section class="banner warn">
      <strong>{t('account_not_signed_in', '未登录或凭证已失效')}</strong>
      <p>{t('auth_required_notice', '请在终端运行 colab whoami 完成授权后再刷新。')}</p>
    </section>
  {/if}

  {#if snap?.startup_notice}
    <section class="banner info">
      <strong>{t('startup_notice_title', '启动对账提示')}</strong>
      <p class="sensitive">{snap.startup_notice.message}</p>
    </section>
  {/if}

  {#if (snap?.orphan_count ?? 0) > 0}
    <section class="banner warn row-between">
      <div>
        <strong>{t('orphan_warning_title', '检测到未命名孤儿实例 [?]')} ({snap?.orphan_count})</strong>
        <p>{t('orphan_warning_body', 'colab CLI 无法停掉未命名的孤儿实例，请前往 Colab 网页手动终止。')}</p>
      </div>
      <button class="secondary" onclick={runOpenManageSessions}>
        {t('btn_open_manage_sessions', '打开 Colab 管理会话')}
      </button>
    </section>
  {/if}

  {#if snap?.warning_banner}
    <section class="banner warn">
      <p>{snap.warning_banner}</p>
    </section>
  {/if}

  {#if snap?.account_drift_detected}
    <section class="banner danger">
      <p>{t('agy_drift_banner', '检测到外部已切换账号，与当前托管会话所属账号不一致，请切回原号后再停机。')}</p>
      {#if snap?.env.agy_cs_path}
        <button class="secondary" onclick={runSwitchBackToOwner}>
          {t('agy_switch_back_btn', '一键切回原号')}
        </button>
      {/if}
    </section>
  {/if}

  {#if snap?.low_balance_warning}
    <section class="banner warn">
      <p>{t('low_balance_warning_banner', '按当前消耗速率估算剩余可用时间不足 15 分钟，请及时导出工作流与成品。')}</p>
    </section>
  {/if}

  {#if snap?.active_session && snap?.watchdog?.phase === 'Warning'}
    <section class="banner warn">
      <p>
        {t('watchdog_warning_banner', '闲置自动停机看门狗：将在 {secs}s 后自动导出并停机。').replace('{secs}', String(watchdogWarningSecs))}
      </p>
      <button class="primary" onclick={runExtendWatchdog}>
        {t('watchdog_extend_btn', '延长 30 分钟')}
      </button>
    </section>
  {:else if snap?.active_session && snap?.watchdog?.sleep_wake_notice}
    <section class="banner warn">
      <p>{t('watchdog_sleep_wake_banner', '检测到 Mac 睡眠唤醒，已重置闲置看门狗并等待重查队列。')}</p>
      <button class="secondary" onclick={runDismissSleepNotice}>OK</button>
    </section>
  {/if}

  {#if actionError || snap?.last_error}
    <section class="banner danger">
      <strong>{(actionError ?? snap?.last_error)?.code}</strong>
      <p>{(actionError ?? snap?.last_error)?.message}</p>
    </section>
  {/if}

  <section class="grid-two">
    <div class="card">
      <h2>{t('section_env_account', '环境与账号')}</h2>
      <dl class="kv">
        <dt>colab CLI</dt>
        <dd class="mono sensitive">{snap?.env.colab_path ?? t('env_colab_missing', '未找到 colab')}</dd>
        <dt>OpenSSH</dt>
        <dd>{snap?.env.ssh_available && snap?.env.ssh_keygen_available ? t('env_ssh_available', 'ssh / ssh-keygen 可用') : t('env_ssh_missing', '未找到 /usr/bin/ssh')}</dd>
        <dt>{t('account_label', '当前账号')}</dt>
        <dd class="sensitive">{snap?.account_email ?? t('account_not_signed_in', '未登录')}</dd>
        <dt>{t('balance_label', '计算单元余额')}</dt>
        <dd>
          {#if snap?.usage}
            {snap.usage.current_balance.toFixed(2)} CU
            （{snap.usage.usage_rate_hourly.toFixed(2)}/hr · {snap.usage.active_assignments}）
          {:else}
            --
          {/if}
        </dd>
        <dt>{t('cu_remaining_label', '按当前速率预计可用')}</dt>
        <dd>
          {#if snap?.usage}
            {formatDurationMinutes(snap?.telemetry?.estimated_remaining_minutes)}
          {:else}
            --
          {/if}
        </dd>
      </dl>
      {#if snap?.env.agy_cs_path && snap?.agy_accounts?.length}
        <div class="form-row">
          <label for="agy-account-select">{t('agy_account_switch_label', '切换 Colab 账号 (agy-cs)')}</label>
          <select
            id="agy-account-select"
            class="sensitive"
            disabled={busy || Boolean(snap?.active_session) || Boolean(snap?.pending_session) || Boolean(snap?.ledger_corrupted)}
            onchange={(e) => runSwitchAgyAccount(e.currentTarget as HTMLSelectElement)}
          >
            {#each snap.agy_accounts as acc}
              <option value={acc.name} selected={acc.is_current}>
                {acc.name} ({acc.email}) — {acc.balance_cu != null ? `${acc.balance_cu.toFixed(2)} CU` : '--'}
              </option>
            {/each}
          </select>
        </div>
        <div class="controls">
          <button class="secondary" disabled={busy} onclick={() => runRefreshAgyAccounts(true)}>
            {t('agy_refresh_quota_btn', '刷新全部额度')}
          </button>
          <button class="secondary" disabled={busy} onclick={runOpenAgySwitcher}>
            {t('agy_manage_accounts_btn', '新增 / 管理账号')}
          </button>
        </div>
      {/if}
    </div>

    <div class="card">
      <h2>{t('section_session_config', '会话配置')}</h2>
      <div class="form-row">
        <label for="gpu-select">{t('gpu_label', 'GPU 规格')}</label>
        <select
          id="gpu-select"
          bind:value={selectedGpu}
          disabled={busy || Boolean(snap?.active_session) || Boolean(snap?.pending_session) || Boolean(snap?.ledger_corrupted)}
        >
          {#each GPU_OPTIONS as opt}
            <option value={opt.value}>{opt.label} — {opt.note}</option>
          {/each}
        </select>
      </div>
      <div class="form-row checkbox-row">
        <label>
          <input
            type="checkbox"
            bind:checked={highMem}
            disabled={!highMemSupported || busy || Boolean(snap?.active_session) || Boolean(snap?.pending_session) || Boolean(snap?.ledger_corrupted)}
          />
          {t('high_ram_label', 'High-RAM 高内存')}
        </label>
        {#if !highMemSupported}
          <span class="hint">{t('high_ram_l4_disabled', 'L4 仅提供单档内存，不支持 High-RAM')}</span>
        {/if}
      </div>
      <div class="form-row checkbox-row">
        <label>
          <input
            type="checkbox"
            bind:checked={useDrive}
            onchange={() => savePreferences({ useDrive })}
            disabled={busy || Boolean(snap?.active_session) || Boolean(snap?.pending_session) || Boolean(snap?.ledger_corrupted)}
          />
          {t('use_drive_label', '使用 Google Drive 持久化（零 Token 上云）')}
        </label>
      </div>
      <div class="form-row checkbox-row">
        <label>
          <input
            type="checkbox"
            bind:checked={exportOutputsOnStop}
            onchange={() => savePreferences({ exportOutputsOnStop })}
            disabled={stopping}
          />
          {t('export_outputs_label', '停机时导出 output/ 成品到 Drive')}
        </label>
      </div>
      <div class="form-row">
        <label for="export-timeout-input">{t('export_timeout_label', '停机导出超时 (秒)')}</label>
        <input
          id="export-timeout-input"
          type="number"
          min="15"
          max="1800"
          step="15"
          bind:value={driveExportTimeoutSecs}
          onchange={() => savePreferences({ driveExportTimeoutSecs })}
          disabled={stopping}
        />
      </div>
      <div class="form-row">
        <label for="watchdog-select">
          {t('watchdog_label', '闲置自动停机看门狗')}
          {#if snap?.active_session && snap?.watchdog?.phase === 'Counting' && snap.watchdog.idle_secs > 0}
            <span class="hint">({Math.floor(snap.watchdog.idle_secs / 60)}m)</span>
          {/if}
        </label>
        <select
          id="watchdog-select"
          value={snap?.config.watchdog_idle_minutes == null ? 'off' : String(snap.config.watchdog_idle_minutes)}
          onchange={(e) => runSetWatchdogMinutes((e.currentTarget as HTMLSelectElement).value)}
        >
          <option value="15">{t('watchdog_minutes_option', '{mins} 分钟').replace('{mins}', '15')}</option>
          <option value="30">{t('watchdog_minutes_option', '{mins} 分钟').replace('{mins}', '30')}</option>
          <option value="60">{t('watchdog_minutes_option', '{mins} 分钟').replace('{mins}', '60')}</option>
          <option value="off">{t('watchdog_off_option', '关闭')}</option>
        </select>
      </div>
      {#if snap?.active_session}
        <div class="session-meta">
          <span>{t('session_label', '当前会话')}: <code class="sensitive">{snap.active_session.session_name}</code></span>
          <span class="sensitive">{t('hardware_label', '硬件')}: {snap.active_session.hardware_summary}</span>
          <span>{t('local_proxy_label', '本地代理')}: <code>127.0.0.1:{snap.active_session.local_port}</code></span>
          <span>Drive: {snap.active_session.drive_enabled ? 'ON' : 'OFF'}</span>
        </div>
      {:else if snap?.pending_session}
        <div class="session-meta">
          <span>{t('pending_session_label', '待完成会话')}: <code class="sensitive">{snap.pending_session.session_name}</code></span>
          <span>GPU: {snap.pending_session.gpu}</span>
        </div>
      {/if}
    </div>
  </section>

  <section class="card">
    <div class="controls">
      {#if !snap?.active_session && !snap?.pending_session && !snap?.ledger_corrupted}
        <button
          class="primary"
          onclick={() => runStart(false)}
          disabled={busy || !snap?.env.colab_path || !snap?.env.ssh_available}
        >
          {t('btn_start', '一键启动 ComfyUI')}
        </button>
      {/if}

      {#if snap?.phase === 'Starting'}
        <button class="danger" onclick={runCancelStart} disabled={stopping}>
          {snap?.active_session
            ? t('btn_cancel_and_stop', '取消并停机')
            : t('btn_cancel_start', '取消开机（等待分配后停机）')}
        </button>
      {/if}

      {#if snap?.active_session}
        <button class="primary" onclick={runOpenCanvas} disabled={!snap.tunnel_connected}>
          {t('btn_open_canvas', '打开 ComfyUI 画布')}
        </button>
        <button class="secondary" onclick={runReconnect} disabled={starting || reconnecting || stopping || refreshing}>
          {t('btn_reconnect', '重连本地隧道')}
        </button>
        <button class="secondary" onclick={runOpenColabUrl} disabled={stopping}>
          {t('btn_open_colab_url', '在浏览器打开 Colab 页')}
        </button>
        {#if snap.active_session.drive_enabled && snap.tunnel_connected}
          <button class="secondary" onclick={runExportDriveNow} disabled={exporting || stopping}>
            {t('btn_export_drive', '立即导出到 Drive')}
          </button>
        {/if}
        <button class="danger" onclick={runStop} disabled={stopping}>
          {t('btn_stop', '停机并核验停止计费')}
        </button>
      {/if}

      {#if (snap?.pending_session || snap?.ledger_corrupted) && !snap?.active_session && snap?.phase !== 'Starting'}
        <button class="secondary" onclick={runReconcile} disabled={refreshing || stopping}>
          重新对账云端状态
        </button>
        {#if snap?.pending_session}
          <button class="danger" onclick={runStop} disabled={stopping}>
            {t('btn_stop', '停机并核验停止计费')}
          </button>
        {/if}
        <button class="secondary" onclick={runOpenManageSessions}>
          {t('btn_open_manage_sessions', '打开 Colab 管理会话')}
        </button>
      {/if}
    </div>

    <ol class="steps">
      {#each STEPS as step, idx}
        <li
          class:done={activeStepIndex > idx || (snap?.phase === 'Ready' && idx === STEPS.length - 1)}
          class:active={activeStepIndex === idx && (snap?.phase === 'Starting' || snap?.phase === 'Reconnecting')}
          class:failed={activeStepIndex === idx && snap?.phase === 'Error'}
        >
          {t(step.labelKey, step.fallback)}
        </li>
      {/each}
    </ol>
    {#if isTimedPhase}
      <p class="elapsed-hint">
        {(snap?.phase === 'Reconnecting'
          ? t('reconnecting_elapsed_hint', '正在重连本地 SSH 隧道，已耗时 {secs}s…')
          : t(
              'starting_elapsed_hint',
              '正在等待云端响应，已耗时 {secs}s（Colab 容量紧张时会自动退避重试，可点取消安全停机）'
            )
        ).replace('{secs}', String(elapsedSecs))}
      </p>
    {/if}
  </section>

  <section class="card">
    <div class="row-between">
      <h2>{t('telemetry_title', '实时算力与资源监控')}</h2>
      {#if snap?.telemetry?.gpu_name}
        <span class="hint sensitive">{snap.telemetry.gpu_name}</span>
      {/if}
    </div>
    {#if snap?.tunnel_connected && snap?.telemetry}
      <div class="telemetry-grid">
        <div class="metric">
          <span class="metric-label">{t('gpu_util_label', 'GPU 利用率')}</span>
          <strong>
            {snap.telemetry.gpu_util_pct != null ? `${snap.telemetry.gpu_util_pct.toFixed(0)}%` : '--'}
            {snap.telemetry.gpu_temp_c != null ? ` (${snap.telemetry.gpu_temp_c.toFixed(0)}°C)` : ''}
          </strong>
        </div>
        <div class="metric">
          <span class="metric-label">{t('vram_label', '显存 (VRAM)')}</span>
          <strong>{formatGiB(snap.telemetry.vram_used_bytes)} / {formatGiB(snap.telemetry.vram_total_bytes)}</strong>
        </div>
        <div class="metric">
          <span class="metric-label">{t('ram_label', '系统内存 (RAM)')}</span>
          <strong>{formatGiB(snap.telemetry.ram_used_bytes)} / {formatGiB(snap.telemetry.ram_total_bytes)}</strong>
        </div>
        <div class="metric">
          <span class="metric-label">{t('queue_label', '任务队列')}</span>
          <strong>{snap.telemetry.queue_running} running / {snap.telemetry.queue_pending} pending</strong>
        </div>
        <div class="metric">
          <span class="metric-label">{t('disk_content_label', '/content 剩余')}</span>
          <strong>{formatGiB(snap.telemetry.content_free_bytes)}</strong>
        </div>
        <div class="metric">
          <span class="metric-label">{t('disk_drive_label', 'Drive 剩余')}</span>
          <strong>{formatGiB(snap.telemetry.drive_free_bytes)}</strong>
        </div>
      </div>
      {#if snap.telemetry.samples.length > 0}
        <div class="chart-wrap">
          <svg viewBox="0 0 320 80" class="telem-svg" preserveAspectRatio="none">
            <line x1="0" y1="40" x2="320" y2="40" stroke="currentColor" stroke-opacity="0.15" stroke-dasharray="3 3" />
            <polyline
              fill="none"
              stroke="#0a84ff"
              stroke-width="2"
              points={svgPolyline(snap.telemetry.samples, (s) => s.gpu_util_pct)}
            />
            <polyline
              fill="none"
              stroke="#30d158"
              stroke-width="2"
              points={svgPolyline(snap.telemetry.samples, (s) => s.vram_pct)}
            />
            <polyline
              fill="none"
              stroke="#ff9f0a"
              stroke-width="1.5"
              points={svgPolyline(snap.telemetry.samples, (s) => s.ram_pct)}
            />
          </svg>
          <div class="chart-legend">
            <span style="color:#0a84ff">● GPU%</span>
            <span style="color:#30d158">● VRAM%</span>
            <span style="color:#ff9f0a">● RAM%</span>
            <span class="hint">({snap.telemetry.samples.length}/120)</span>
          </div>
        </div>
      {/if}
    {:else}
      <p class="hint">{t('telemetry_empty', '等待画布就绪后开始采集…')}</p>
    {/if}
  </section>

  {#if snap?.last_export_report}
    <section class="banner {snap.last_export_report.ok && snap.last_export_report.flush_ok ? 'ok' : 'warn'}">
      <p>{snap.last_export_report.summary}</p>
    </section>
  {/if}

  {#if snap?.last_stop_report}
    <section class="banner {snap.last_stop_report.billing_verified_stopped ? 'ok' : 'warn'}">
      <strong>停机核验报告（{snap.last_stop_report.session_name}）</strong>
      <p>{snap.last_stop_report.summary}</p>
      {#if snap.last_stop_report.warnings.length > 0}
        <ul>
          {#each snap.last_stop_report.warnings as w}
            <li>{w}</li>
          {/each}
        </ul>
      {/if}
    </section>
  {/if}

  <section class="card">
    <button class="link-btn" onclick={() => (logsExpanded = !logsExpanded)}>
      {logsExpanded ? '▼' : '▶'} {t('logs_title', '运行日志（已脱敏）')} ({snap?.logs.length ?? 0})
    </button>
    {#if logsExpanded}
      <pre class="log-box sensitive">{snap?.logs.length ? snap.logs.join('\n') : t('logs_empty', '暂无日志')}</pre>
    {/if}
  </section>
</main>

{#if snap?.drive_auth}
  <div class="modal-backdrop">
    <div class="modal">
      <h3>{t('drive_auth_title', '需要授权挂载 Google Drive')}</h3>
      <p>{t('drive_auth_body', '请在浏览器完成授权后点击「我已授权，继续挂载」。')}</p>
      {#if snap.drive_auth.auth_url}
        <p class="mono url-preview sensitive">{snap.drive_auth.auth_url}</p>
      {/if}
      {#if snap.drive_auth.status === 'Failed' && snap.drive_auth.grace_deadline_unix}
        <p class="hint">
          {t('drive_grace_countdown', '挂载失败，{secs}s 后将自动降级为无 Drive 模式继续启动（可重试、跳过或取消并停机）')
            .replace('{secs}', String(driveGraceRemainingSecs))}
        </p>
      {:else}
        <p class="hint">
          {t('drive_auth_countdown', '剩余等待时间：{secs}s（第 {attempt}/{max} 次）')
            .replace('{secs}', String(driveRemainingSecs))
            .replace('{attempt}', String(snap.drive_auth.attempt))
            .replace('{max}', String(snap.drive_auth.max_attempts))}
        </p>
      {/if}
      {#if snap.drive_auth.message}
        <p class="banner warn">{snap.drive_auth.message}</p>
      {/if}
      <div class="modal-actions">
        {#if snap.drive_auth.auth_url}
          <button class="secondary" onclick={() => runReopenDriveAuthUrl(snap?.drive_auth?.auth_url ?? null)}>
            {t('drive_auth_reopen', '重新打开授权页')}
          </button>
        {/if}
        {#if snap.drive_auth.status !== 'Failed'}
          <button
            class="primary"
            onclick={runConfirmDriveAuth}
            disabled={snap.drive_auth.status === 'Continuing'}
          >
            {t('drive_auth_continue', '我已授权，继续挂载')}
          </button>
        {:else if snap.drive_auth.attempt < snap.drive_auth.max_attempts}
          <button class="primary" onclick={runRetryDriveMount}>
            {t('drive_auth_retry', '重试挂载')}
          </button>
        {/if}
        <button class="secondary" onclick={runSkipDriveForSession}>
          {t('drive_auth_skip', '跳过 Drive（本次无持久化继续）')}
        </button>
        <button class="danger" onclick={runCancelStart} disabled={stopping}>
          {t('btn_cancel_and_stop', '取消并停机')}
        </button>
      </div>
    </div>
  </div>
{/if}

{#if showZeroCuModal}
  <div class="modal-backdrop">
    <div class="modal">
      <h3>{t('zero_balance_title', '计算单元余额为 0.00 CU')}</h3>
      <p>{t('zero_balance_body', '当前账号余额为 0 CU，GPU 配额可能受限。建议先充值，或确认仅作短时测试。')}</p>
      <div class="modal-actions">
        <button class="primary" onclick={runOpenPay}>{t('zero_balance_open_pay', '前往 colab pay')}</button>
        <button class="secondary" onclick={() => runStart(true)}>
          {t('zero_balance_allow_test', '已知风险，继续短测')}
        </button>
        <button class="secondary" onclick={() => (showZeroCuModal = false)}>
          {t('zero_balance_cancel', '取消')}
        </button>
      </div>
    </div>
  </div>
{/if}

{#if showExitModal}
  <div class="modal-backdrop">
    <div class="modal">
      <h3>{t('exit_title', '云端 Colab 会话仍在计费')}</h3>
      <p>{t('exit_body', '退出 App 不会停止 Colab 会话，计算单元会继续扣费。请选择：')}</p>
      <div class="modal-actions">
        <button class="danger" onclick={runStopAndExit} disabled={exitBusy}>
          {t('exit_stop_and_quit', '立即停机并退出')}
        </button>
        <button class="secondary" onclick={runLeaveRunningAndExit} disabled={exitBusy}>
          {t('exit_leave_running', '保留云端运行仅退出（继续计费）')}
        </button>
        <button class="secondary" onclick={() => (showExitModal = false)} disabled={exitBusy}>
          {t('exit_cancel', '取消')}
        </button>
      </div>
    </div>
  </div>
{/if}
</div>

<style>
  :global(body) {
    margin: 0;
    font: 13px/1.5 -apple-system, BlinkMacSystemFont, 'SF Pro Text', sans-serif;
    color-scheme: light dark;
    background: light-dark(#f5f5f7, #1c1c1e);
    color: light-dark(#1d1d1f, #f2f2f7);
  }

  main {
    max-width: 920px;
    margin: 0 auto;
    padding: 24px;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .topbar {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 16px;
  }

  h1 {
    margin: 0;
    font-size: 20px;
  }

  h2 {
    margin: 0 0 10px;
    font-size: 14px;
  }

  .subtitle {
    margin: 2px 0 0;
    opacity: 0.65;
    font-size: 12px;
  }

  .topbar-actions {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .phase-badge {
    padding: 3px 10px;
    border-radius: 999px;
    font-size: 12px;
    font-weight: 600;
    background: light-dark(#e5e5ea, #2c2c2e);
  }

  .phase-ready {
    background: #1f7a3e;
    color: #fff;
  }

  .phase-starting,
  .phase-reconnecting,
  .phase-stopping {
    background: #b25e09;
    color: #fff;
  }

  .phase-error {
    background: #b91c1c;
    color: #fff;
  }

  .grid-two {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
    gap: 14px;
  }

  .card {
    background: light-dark(#ffffff, #2c2c2e);
    border: 1px solid light-dark(#e5e5ea, #3a3a3c);
    border-radius: 10px;
    padding: 14px 16px;
  }

  .banner {
    border-radius: 10px;
    padding: 12px 14px;
    border: 1px solid transparent;
  }

  .banner p {
    margin: 4px 0 0;
  }

  .banner ul {
    margin: 6px 0 0;
    padding-left: 18px;
  }

  .banner.info {
    background: light-dark(#eef5ff, #1e2d44);
    border-color: light-dark(#bfdbfe, #2b4a78);
  }

  .banner.warn {
    background: light-dark(#fffbeb, #3b2d12);
    border-color: light-dark(#fde68a, #78591c);
  }

  .banner.danger {
    background: light-dark(#fef2f2, #3f1818);
    border-color: light-dark(#fecaca, #7f2929);
  }

  .banner.ok {
    background: light-dark(#ecfdf5, #143325);
    border-color: light-dark(#a7f3d0, #236345);
  }

  .row-between {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 12px;
  }

  .kv {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 6px 14px;
    margin: 0;
  }

  .kv dt {
    opacity: 0.65;
  }

  .kv dd {
    margin: 0;
    word-break: break-all;
  }

  .mono,
  code,
  pre {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }

  .form-row {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 10px;
  }

  .checkbox-row {
    flex-wrap: wrap;
  }

  .hint {
    font-size: 12px;
    opacity: 0.65;
  }

  .session-meta {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    font-size: 12px;
    opacity: 0.85;
  }

  .controls {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    margin-bottom: 12px;
  }

  button {
    border: 1px solid light-dark(#d1d1d6, #48484a);
    border-radius: 7px;
    padding: 7px 13px;
    font: inherit;
    font-weight: 500;
    cursor: pointer;
    background: light-dark(#ffffff, #3a3a3c);
    color: inherit;
  }

  button:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }

  button.primary {
    background: #0a84ff;
    border-color: #0a84ff;
    color: #fff;
  }

  button.danger {
    background: #d93829;
    border-color: #d93829;
    color: #fff;
  }

  button.link-btn {
    border: none;
    background: transparent;
    padding: 0;
    font-weight: 600;
  }

  .steps {
    list-style: none;
    padding: 0;
    margin: 0;
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
    gap: 8px;
  }

  .steps li {
    padding: 6px 10px;
    border-radius: 6px;
    font-size: 12px;
    background: light-dark(#f2f2f7, #1c1c1e);
    opacity: 0.65;
  }

  .steps li.active {
    opacity: 1;
    border: 1px solid #0a84ff;
    font-weight: 600;
  }

  .steps li.failed {
    opacity: 1;
    border: 1px solid #d93829;
    background: light-dark(#fef2f2, #3f1818);
    font-weight: 600;
  }

  .steps li.done {
    opacity: 1;
    background: light-dark(#e8f7ee, #173626);
  }

  .elapsed-hint {
    margin: 10px 0 0;
    font-size: 12px;
    opacity: 0.8;
  }

  .log-box {
    margin: 10px 0 0;
    padding: 10px;
    border-radius: 6px;
    max-height: 220px;
    overflow: auto;
    font-size: 11px;
    background: light-dark(#f2f2f7, #161618);
    white-space: pre-wrap;
    word-break: break-all;
  }

  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.45);
    display: grid;
    place-items: center;
    padding: 20px;
  }

  .modal {
    max-width: 440px;
    width: 100%;
    background: light-dark(#ffffff, #2c2c2e);
    border-radius: 12px;
    padding: 18px 20px;
    border: 1px solid light-dark(#d1d1d6, #48484a);
  }

  .modal h3 {
    margin: 0 0 8px;
  }

  .modal-actions {
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 16px;
  }

  .telemetry-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
    gap: 10px;
    margin-top: 8px;
  }

  .metric {
    padding: 8px 10px;
    border-radius: 7px;
    background: light-dark(#f2f2f7, #1c1c1e);
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .metric-label {
    font-size: 11px;
    opacity: 0.7;
  }

  .chart-wrap {
    margin-top: 12px;
  }

  .telem-svg {
    width: 100%;
    height: 80px;
    border-radius: 6px;
    background: light-dark(#f2f2f7, #1c1c1e);
  }

  .chart-legend {
    display: flex;
    gap: 14px;
    font-size: 11px;
    margin-top: 4px;
  }

  .url-preview {
    font-size: 11px;
    word-break: break-all;
    background: light-dark(#f2f2f7, #1c1c1e);
    padding: 8px;
    border-radius: 6px;
  }

  .app-shell.masked .sensitive {
    filter: blur(5px);
    user-select: none;
  }
</style>
