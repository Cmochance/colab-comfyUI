# Changelog

## 0.2.0（未发布 / unreleased）

### 简体中文

- 新增阶段一核心能力：通过 `colab` CLI 完成登录态与计算单元余额前置检查（0 CU 二次确认拦截）、按账号邮箱哈希命名并隔离会话（`comfy-<hash8>`），以及两阶段开机账本（`pending-session.json` → `active-session.json`）与启动对账。
- 新增 `ControlMaster` 单槽多路复用 SSH 隧道、重连前锁内三重门禁检查（防静默建 CPU 幽灵机）、`<data_dir>/run/comfy.sock` Unix Socket 转发与固定 `127.0.0.1:18188` `Host`/`Origin` 校验本地反向代理。
- 远端自动部署或热接管 ComfyUI（移除 `--enable-cors-header`，热接管发现旧参数且队列为空时自动重启清除）并用 `COMFY_READY=1` 哨兵判就绪。
- 新增无 Tauri 权限的独立 `canvas` 窗口加载 ComfyUI、拖放/下载名清洗/导航白名单/外链过滤、macOS 防 App Nap、退出三选一拦截与停机三重核验，`runner` 出口统一脱敏。
- 优化 GPU 开机可观测性与容量快败反馈：启用 `--logtostderr` 并过滤 `DEBUG` 噪音以实时透传脱敏后的分配重试告警，收敛 `colab new` 重试至 1 次 / 5s 退避并把 `502/503/504/ReadTimeout` 归类为 `CapacityOrSessionLimit` 引导切换 GPU，启动时自动展开日志并显示已耗时秒数，同时将脱敏日志持久化到 `0600` 权限 `~/.colab-comfyui/app.log`（`512 KiB` 轮转）。

### English

- Added Phase 1 core session lifecycle: preflight `colab` auth and compute-unit balance guard (with confirmation prompt on 0 CU), per-account hashed session naming (`comfy-<hash8>`), two-phase startup ledger (`pending-session.json` -> `active-session.json`), and startup reconciliation.
- Added single-slot `ControlMaster` SSH tunneling with locked triple-gate precondition checks (preventing silent CPU instance creation), Unix socket forwarding (`<data_dir>/run/comfy.sock`), and a fixed `127.0.0.1:18188` `Host`/`Origin`-validated reverse proxy.
- Added remote ComfyUI bootstrap and safe hot-takeover without `--enable-cors-header` (auto-restarting idle legacy instances that have `--enable-cors-header`), verified via the `COMFY_READY=1` sentinel.
- Added an isolated zero-capability `canvas` window for ComfyUI with drag-drop pass-through, sanitized download filenames, navigation/external-link allowlists, macOS App Nap inhibition during active sessions, three-way exit confirmation, triple-verified stop reports, and log redaction.
- Improved GPU provisioning observability and fast-fail feedback: streams filtered redacted retry warnings via `--logtostderr`, tunes `colab new` to 1 retry / 5s backoff and classifies `502`/`503`/`504`/`ReadTimeout` into `CapacityOrSessionLimit` with GPU-switch guidance, auto-expands logs with a live elapsed timer, and persists redacted diagnostics to `0600` `~/.colab-comfyui/app.log` (`512 KiB` rotation).

## 0.1.0（未发布 / unreleased）

### 简体中文

- 搭好工程：Rust workspace、Tauri 2 的壳、构建和发布流程。还不能开机，也不能出图。
- 打开后的窗口显示版本号，以及在这台 Mac 上找到的 `colab` 命令的位置。

### English

- Project setup: the Rust workspace, the Tauri 2 shell, and the build and release flow. It cannot start a session or generate anything yet.
- The window shows the version and where the `colab` command was found on this Mac.
