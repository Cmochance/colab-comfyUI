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
    created_at_unix: number;
  }

  interface PendingSession {
    session_name: string;
    owner_email: string;
    gpu: GpuTier;
    high_mem: boolean;
    cancel_requested: boolean;
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
    endpoint_absent: boolean;
    owner_verified: boolean;
    usage: UsageInfo | null;
    billing_verified_stopped: boolean;
    summary: string;
    warnings: string[];
  }

  interface AppSnapshot {
    phase: SessionPhase;
    step: StartStep | null;
    env: EnvReport;
    config: {
      locale: string;
      default_gpu: GpuTier;
      high_mem: boolean;
    };
    account_email: string | null;
    auth_required: boolean;
    usage: UsageInfo | null;
    active_session: ActiveSession | null;
    pending_session: PendingSession | null;
    orphan_count: number;
    startup_notice: StartupNotice | null;
    warning_banner: string | null;
    last_error: UiError | null;
    last_stop_report: StopReport | null;
    tunnel_connected: boolean;
    canvas_url: string | null;
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
  let configSynced = $state(false);
  let busy = $state(false);
  let logsExpanded = $state(false);
  let showZeroCuModal = $state(false);
  let showExitModal = $state(false);
  let actionError = $state<UiError | null>(null);

  const t = (key: string, fallback: string) => snap?.i18n?.[key] ?? fallback;

  const highMemSupported = $derived(selectedGpu !== 'L4');
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
    let unlistenSnapshot: (() => void) | undefined;
    let unlistenExit: (() => void) | undefined;

    getVersion().then((v) => (version = v));
    invoke<AppSnapshot>('reconcile_startup').then((s) => {
      snap = s;
      if (!configSynced) {
        selectedGpu = s.config.default_gpu;
        highMem = s.config.default_gpu !== 'L4' && s.config.high_mem;
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
    busy = true;
    actionError = null;
    try {
      snap = await invoke<AppSnapshot>('refresh_status');
    } catch (err) {
      actionError = err as UiError;
    } finally {
      busy = false;
    }
  }

  async function runStart(allowZeroBalance: boolean) {
    busy = true;
    actionError = null;
    showZeroCuModal = false;
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
      }
    } finally {
      busy = false;
    }
  }

  async function runCancelStart() {
    actionError = null;
    snap = await invoke<AppSnapshot>('cancel_start');
  }

  async function runReconnect() {
    busy = true;
    actionError = null;
    try {
      snap = await invoke<AppSnapshot>('reconnect_tunnel');
    } catch (err) {
      actionError = err as UiError;
    } finally {
      busy = false;
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
    busy = true;
    actionError = null;
    try {
      await invoke<StopReport>('stop_session');
      snap = await invoke<AppSnapshot>('get_snapshot');
    } catch (err) {
      actionError = err as UiError;
    } finally {
      busy = false;
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
    busy = true;
    actionError = null;
    try {
      await invoke('stop_and_exit');
    } catch (err) {
      showExitModal = false;
      actionError = err as UiError;
      busy = false;
    }
  }

  async function runLeaveRunningAndExit() {
    await invoke('leave_running_and_exit');
  }
</script>

<main>
  <header class="topbar">
    <div>
      <h1>{t('app_title', 'Colab ComfyUI')}</h1>
      <p class="subtitle">
        {t('subtitle', '在 Google Colab GPU 上安全运行 ComfyUI')} · {version ? `v${version}` : ''}
      </p>
    </div>
    <div class="topbar-actions">
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
      <strong>启动对账提醒</strong>
      <p>{snap.startup_notice.message}</p>
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

  {#if actionError || snap?.last_error}
    <section class="banner danger">
      <strong>{(actionError ?? snap?.last_error)?.code}</strong>
      <p>{(actionError ?? snap?.last_error)?.message}</p>
    </section>
  {/if}

  <section class="grid-two">
    <div class="card">
      <h2>环境与账号</h2>
      <dl class="kv">
        <dt>colab CLI</dt>
        <dd class="mono">{snap?.env.colab_path ?? t('env_colab_missing', '未找到 colab')}</dd>
        <dt>OpenSSH</dt>
        <dd>{snap?.env.ssh_available && snap?.env.ssh_keygen_available ? '可用 (/usr/bin/ssh)' : '缺失'}</dd>
        <dt>{t('account_label', '当前账号')}</dt>
        <dd>{snap?.account_email ?? t('account_not_signed_in', '未登录')}</dd>
        <dt>{t('balance_label', '计算单元余额')}</dt>
        <dd>
          {#if snap?.usage}
            {snap.usage.current_balance.toFixed(2)} CU
            （{snap.usage.usage_rate_hourly.toFixed(2)}/hr · 活跃 {snap.usage.active_assignments}）
          {:else}
            --
          {/if}
        </dd>
      </dl>
    </div>

    <div class="card">
      <h2>会话与 GPU 配置</h2>
      <div class="form-row">
        <label for="gpu-select">{t('gpu_label', 'GPU 规格')}</label>
        <select
          id="gpu-select"
          bind:value={selectedGpu}
          disabled={busy || Boolean(snap?.active_session) || Boolean(snap?.pending_session)}
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
            disabled={!highMemSupported || busy || Boolean(snap?.active_session) || Boolean(snap?.pending_session)}
          />
          {t('high_ram_label', 'High-RAM 高内存')}
        </label>
        {#if !highMemSupported}
          <span class="hint">{t('high_ram_l4_disabled', 'L4 仅提供单档内存，不支持 High-RAM')}</span>
        {/if}
      </div>
      {#if snap?.active_session}
        <div class="session-meta">
          <span>会话: <code>{snap.active_session.session_name}</code></span>
          <span>硬件: {snap.active_session.hardware_summary}</span>
          <span>本地代理: <code>127.0.0.1:{snap.active_session.local_port}</code></span>
        </div>
      {/if}
    </div>
  </section>

  <section class="card">
    <div class="controls">
      {#if !snap?.active_session && !snap?.pending_session}
        <button
          class="primary"
          onclick={() => runStart(false)}
          disabled={busy || !snap?.env.colab_path || !snap?.env.ssh_available}
        >
          {t('btn_start', '一键启动 ComfyUI')}
        </button>
      {/if}

      {#if snap?.phase === 'Starting'}
        <button class="danger" onclick={runCancelStart}>
          {t('btn_cancel_start', '取消开机（等待分配后停机）')}
        </button>
      {/if}

      {#if snap?.active_session}
        <button class="primary" onclick={runOpenCanvas} disabled={!snap.tunnel_connected}>
          {t('btn_open_canvas', '打开 ComfyUI 画布')}
        </button>
        <button class="secondary" onclick={runReconnect} disabled={busy}>
          {t('btn_reconnect', '重连本地隧道')}
        </button>
        <button class="secondary" onclick={runOpenColabUrl} disabled={busy}>
          {t('btn_open_colab_url', '在浏览器打开 Colab 页')}
        </button>
        <button class="danger" onclick={runStop} disabled={busy}>
          {t('btn_stop', '停机并核验停止计费')}
        </button>
      {/if}
    </div>

    <ol class="steps">
      {#each STEPS as step, idx}
        <li
          class:done={activeStepIndex > idx || (snap?.phase === 'Ready' && idx === STEPS.length - 1)}
          class:active={activeStepIndex === idx && snap?.phase !== 'Ready'}
        >
          {t(step.labelKey, step.fallback)}
        </li>
      {/each}
    </ol>
  </section>

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
      <pre class="log-box">{snap?.logs.length ? snap.logs.join('\n') : t('logs_empty', '暂无日志')}</pre>
    {/if}
  </section>
</main>

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
        <button class="danger" onclick={runStopAndExit} disabled={busy}>
          {t('exit_stop_and_quit', '立即停机并退出')}
        </button>
        <button class="secondary" onclick={runLeaveRunningAndExit} disabled={busy}>
          {t('exit_leave_running', '保留云端运行仅退出（继续计费）')}
        </button>
        <button class="secondary" onclick={() => (showExitModal = false)} disabled={busy}>
          {t('exit_cancel', '取消')}
        </button>
      </div>
    </div>
  </div>
{/if}

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

  .steps li.done {
    opacity: 1;
    background: light-dark(#e8f7ee, #173626);
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
</style>
