# Changelog

## 0.4.0（未发布 / unreleased）

### 简体中文

- 新增 `agy-cs` 公开 CLI 多账号联动（`agy_bridge.rs`）：自动探测本机 `agy-cs` 与 `Agy Switcher.app`（未安装则优雅降级为单账号模式）；支持展示多账号 CU 余额、刷新额度与切换账号（有活跃或待核对托管会话时禁止切号防漏停）；外部切号漂移时按 `owner_email` 从 `agy-cs colab ls` 现查名字一键切回原号。
- 新增双时钟睡眠感知本地闲置看门狗（`watchdog.rs`）：默认开启 30 分钟（支持 `15 / 30 / 60 分钟 / 关闭`）；通过单调时钟与墙上时钟比对识别 Mac 睡眠唤醒，唤醒后先重查 `/queue` 再给足完整 3 分钟停机预告（支持一键「延长 30 分钟」），不在唤醒瞬间直接停机。
- 新增按当前消耗速率估算剩余可用 `< 15 分钟` 的低余额高危提醒，并在 `AGENTS.md` 中明确区分“退出 App 不问不停”与“本地闲置看门狗默认 30 分钟开启”。

### English

- Added `agy-cs` public CLI multi-account integration (`agy_bridge.rs`): detects local `agy-cs` and `Agy Switcher.app` (gracefully degrading to single-account mode when absent), displays per-account CU balances, refreshes quotas, blocks switching while a managed session is active, and supports one-click switch-back by dynamically resolving `owner_email` via `agy-cs colab ls`.
- Added a dual-clock sleep-aware local idle watchdog (`watchdog.rs`): enabled at 30 minutes by default (`15 / 30 / 60 min / Off`); compares monotonic and wall clocks to detect macOS sleep/wake jumps, re-checks `/queue` after wake, and always provides a full 3-minute countdown warning (with a "+30 min" extend action) before stopping.
- Added low-balance warning when estimated remaining runtime drops below 15 minutes, and documented the distinction between app-exit behavior and the 30-minute idle watchdog in `AGENTS.md`.

## 0.3.0（未发布 / unreleased）

### 简体中文

- 新增零 Token 上云的 Google Drive 持久化闭环（`persistence.rs`）：挂载前核对 `colab whoami`、以 `setsid()` 脱离控制终端拉起 `colab drivemount` 并提供浏览器授权引导弹窗（10 分钟倒计时、最多 2 次重试、支持跳过降级为无 Drive 模式）。
- 开机冻结 Colab 预装核心包（`torch` / `torchvision` / `torchaudio` / `numpy` / `transformers` / `pillow` / `safetensors`）为 `/tmp/colab_constraints.txt`，所有依赖安装带 `-c` 约束；`custom_nodes_repos.txt` 锁定 `<repo_url> <commit_sha>`；目录映射拷贝先核对文件数与总字节再 `shutil.rmtree`；`--no-drive` 自动清理悬空软链与 `extra_model_paths.yaml` 并在队列为空时热重启生效。
- 停机前与手动支持捕获 `/history` 最新 `create_time` 工作流导出并调用 `drive.flush_and_unmount()`（90s 独立超时）；导出或刷盘超时绝不阻塞 `colab stop`，并在报告中如实标记“Drive 同步未确认”。
- 新增三级零额外 SSH 开销算力/CU 实时监控（`telemetry.rs`）：3s 本地回环读 `/system_stats` 与 `/queue`、20s 复用 `ControlMaster` 采集 `nvidia-smi` 与磁盘空间、60s 刷新 `colab usage` 并换算剩余可用时长，主界面提供 120 点 SVG 实时折线图、中英双语实时切换（`i18n.rs`）与隐私打码开关。

### English

- Added zero-token-upload Google Drive persistence (`persistence.rs`): verifies `colab whoami` before mount, spawns `colab drivemount` detached via `setsid()` with an interactive browser authorization modal (10-minute countdown, max 2 retries, and graceful fallback to `--no-drive` mode).
- Froze Colab preinstalled core packages (`torch`, `torchvision`, `torchaudio`, `numpy`, `transformers`, `pillow`, `safetensors`) into `/tmp/colab_constraints.txt` for all `pip`/`uv` installs; pinned custom nodes as `<repo_url> <commit_sha>` in `custom_nodes_repos.txt`; verified file counts and byte sizes before `shutil.rmtree`; and added `--no-drive` cleanup of dangling symlinks and `extra_model_paths.yaml` with idle auto-restart.
- Added manual and pre-stop workflow snapshot export from `/history` (latest `create_time`) with `drive.flush_and_unmount()` under a 90s timeout; failures or timeouts never block `colab stop` and honestly report unconfirmed Drive sync.
- Added three-tier zero-extra-SSH telemetry (`telemetry.rs`): 3s local loopback `/system_stats` & `/queue`, 20s `ControlMaster`-reused `nvidia-smi` & disk space probe, and 60s `colab usage` remaining-time estimation, rendered with a 120-sample SVG chart, live Chinese/English switching (`i18n.rs`), and a privacy-mask toggle.

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
