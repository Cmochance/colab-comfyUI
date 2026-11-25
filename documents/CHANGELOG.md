# Changelog

## 0.6.0（未发布 / unreleased）

### 简体中文

- 新增按需远端视觉 Sidecar 与看图反推提示词（`llm_sidecar.rs`）：钉死 `llama.cpp` `b12722` CUDA 12.8 预编译包与 `sha256` 校验，启动前通过主 `ControlMaster` 探针核验 VM NVIDIA 驱动与 `glibc` 版本；通过 `ssh -S <ctl> -O forward` 复用主隧道建 `run/llm.sock`（绝不新增 `colab ssh`）；`G4`/`H100`/`A100` 支持与 ComfyUI 显存共驻，`L4`/`T4` 紧凑显存自动调用 ComfyUI `POST /free` 释放缓存；支持直接对本地图片或画廊成品反推 FLUX / ComfyUI Tags / Wan 2.2 Motion 提示词。
- 新增固定端口本地鉴权透传 API 网关（`gateway.rs`）：固定监听 `127.0.0.1:18080`（严禁 `0.0.0.0`），透明反向代理至 `llm.sock`，直接透传 `llama-server` 原生 Anthropic `/v1/messages`、`/v1/messages/count_tokens` 与 OpenAI `/v1/chat/completions`、`/v1/models`（含 SSE 流式）；强制校验 `Bearer` / `x-api-key` 密钥（存 macOS 钥匙串 `gateway_api_key`，不在 `~/.colab-comfyui/` 落盘）与 `Host`/`Origin`，并提供一键复制 Claude Code / Cursor 配置（注明小参数视觉模型主打看图反推）。

### English

- Added on-demand remote vision sidecar & image-to-prompt interrogation (`llm_sidecar.rs`): pins `llama.cpp` `b12722` CUDA 12.8 prebuilt binary + `sha256`, probes VM NVIDIA driver and `glibc` versions before launch, reuses the main `ControlMaster` via `ssh -S <ctl> -O forward` to `run/llm.sock` (no extra `colab ssh`), supports VRAM coexistence on `G4`/`H100`/`A100` and automatic ComfyUI `POST /free` eviction on `L4`/`T4`, and generates FLUX / ComfyUI Tags / Wan 2.2 Motion prompts from local or gallery images.
- Added fixed-port authenticated local API gateway (`gateway.rs`): binds strictly to `127.0.0.1:18080` and transparently proxies to `llm.sock` using `llama-server`'s native Anthropic `/v1/messages` & `/v1/messages/count_tokens` and OpenAI `/v1/chat/completions` & `/v1/models` (including SSE streaming); enforces keychain-backed `Bearer` / `x-api-key` auth (`gateway_api_key`) and `Host`/`Origin` validation, with one-click Claude Code / Cursor snippet copy.

## 0.5.0（未发布 / unreleased）

### 简体中文

- 新增已核实模型与工作流中心（`catalog.rs`）：内置 `FLUX.1-dev FP8`、`FLUX.1-schnell FP8`、`Wan 2.2 TI2V 5B` 已核实字节数预设与配套工作流模板，并支持自定义 Hugging Face 直链 / `<owner>/<repo>:<file>` 导入。下载前执行显存提示与双盘配额校验（`/content` 与 Drive 均留 2 GiB 余量），先下载至 `/content` 核验字节后再原子拷入 Drive（避免多线程直写 FUSE），并支持存在即跳过与 `aria2c` 实时进度展示。
- 新增 `HF_TOKEN` 钥匙串安全注入（`hf_keychain.rs`）：仅读写本应用专属钥匙串条目（`io.github.cmochance.colab-comfyui` / `hf_token`），经 `/usr/bin/security -i` stdin 写入，下载时经 `ControlMaster` stdin 喂入 `aria2c --input-file=-`（不进 `argv` 与 `colab exec` 历史），并在设置处诚实声明 Token 会短暂进入远端 VM 内存。
- 新增成品自动回传与本地作品画廊（`sync.rs`）：监听 `/ws` `executed` 事件并在重连时补扫 `/history`，按 `~/.colab-comfyui/sync-cursor.json` 去重流式保存至 `~/Pictures/ColabComfyUI/` 与 `~/Movies/ColabComfyUI/`；纯 Rust 解析 PNG `tEXt` / `iTXt` 中的 ComfyUI `prompt` 与 `workflow` JSON，支持参数查看、复制 Prompt 与 Finder 显示。

### English

- Added a verified Model & Workflow Catalog (`catalog.rs`): includes verified presets (`FLUX.1-dev FP8`, `FLUX.1-schnell FP8`, `Wan 2.2 TI2V 5B`) with workflow templates plus custom Hugging Face URL / `<owner>/<repo>:<file>` import. Enforces dual-disk 2 GiB quota checks, downloads to `/content` first with byte-size verification before atomic copy to Drive, skips existing files, and streams `aria2c` progress.
- Added keychain-backed `HF_TOKEN` management (`hf_keychain.rs`): stores `hf_token` via `/usr/bin/security -i` stdin under `io.github.cmochance.colab-comfyui` and injects it via `ControlMaster` stdin to `aria2c --input-file=-` (never in `argv` or `colab exec` history), with an explicit UI notice about VM memory exposure.
- Added automatic output sync and a pure-Rust PNG metadata gallery (`sync.rs`): listens on `/ws` (`executed`) and reconciles `/history` using `~/.colab-comfyui/sync-cursor.json`, saves images to `~/Pictures/ColabComfyUI/` and videos to `~/Movies/ColabComfyUI/`, and extracts `tEXt`/`iTXt` `prompt` & `workflow` parameters for inspection, prompt copying, and Finder reveal.

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
