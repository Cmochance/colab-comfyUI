# colab-comfyUI

<p align="center">
  <a href="README.md">简体中文</a> |
  <a href="README.en.md">English</a> |
  <a href="documents/CHANGELOG.md">Changelog</a>
</p>

<p align="center">
  <a href="LICENSE.txt"><img alt="License" src="https://img.shields.io/github/license/Cmochance/colab-comfyUI"></a>
  <a href="https://www.rust-lang.org/"><img alt="Rust" src="https://img.shields.io/badge/Rust-stable-DEA584?logo=rust&logoColor=white"></a>
  <a href="https://tauri.app/"><img alt="Tauri" src="https://img.shields.io/badge/Tauri-2-24C8DB?logo=tauri&logoColor=white"></a>
  <a href="https://developer.apple.com/macos/"><img alt="macOS" src="https://img.shields.io/badge/macOS-13%2B-000000?logo=apple&logoColor=white"></a>
</p>

**colab-comfyUI** 是一个 macOS 桌面 app：在 Google Colab 的 GPU 上开 [ComfyUI](https://github.com/comfyanonymous/ComfyUI)，在 app 的窗口里直接用它的界面生图、生视频，不用在终端里敲 `colab` 指令。

## 现在的状态

已可用（**v0.4.0 阶段三闭环**）：

- 一键开机：支持 `L4`（自动禁用 `High-RAM`）/ `T4` / `G4` / `A100` / `H100`，开机前核查登录态与计算单元余额（0 CU 拦截提示）
- `agy-cs` 多账号联动与防漂移：自动探测本机 `agy-cs` 与 `Agy Switcher.app`（未安装时优雅降级为单账号模式），展示多账号 CU 余额并支持刷新额度、切号与唤起账号管理；运行中禁止切号，外部切号漂移时支持按邮箱动态切回原号
- 双时钟睡眠感知闲置看门狗：默认开启 30 分钟（支持 `15 / 30 / 60 分钟 / 关闭`）；检测 Mac 睡眠唤醒跳变后先重查 `/queue` 再给足 3 分钟完整停机预告（可一键「延长 30 分钟」），同时提供剩余可用 `< 15 分钟` 低余额提醒
- Google Drive 持久化：零 Token 上云，支持交互式浏览器授权挂载、核心包版本约束冻结（`-c /tmp/colab_constraints.txt`）、自定义节点 commit 锁定、安全目录映射校验、`--no-drive` 残留清理，以及停机前/手动 `/history` 工作流导出与 `drive.flush_and_unmount()`（超时绝不阻塞停机）
- 实时算力与 CU 监控：3s 本地代理读显存/内存/队列、20s 复用 `ControlMaster` 采 GPU 利用率/温度/磁盘、60s 换算 CU 剩余可用时长，配合 120 点实时折线图、中英双语切换与隐私打码开关
- 安全隧道与双窗口隔离：`ControlMaster` 单连接复用 + 本地 Unix Socket + 固定 `127.0.0.1:18188` `Host` 校验反向代理，`canvas` 窗口零 Tauri IPC 权限
- 计费防护：两阶段本地账本（`~/.colab-comfyui/`）、启动对账、关窗/退出三选一拦截、停机三重核验与 `[?]` 孤儿实例管理入口

后续阶段计划：

- 精选模型/工作流下载中心与成品自动回传画廊
- 按需视觉反推提示词与固定端口本地网关

## 需要什么

- macOS 13 或更新的系统
- [`google-colab-cli`](https://pypi.org/project/google-colab-cli/) 提供的 `colab` 命令，并且已经登录

## 从源码运行

需要 Rust（stable）、Node.js 22 或更新的版本、Xcode 命令行工具。

```bash
git clone https://github.com/Cmochance/colab-comfyUI.git
cd colab-comfyUI
make dev      # 开发时用，改了代码自动重开
make test     # 测试
make app      # 生成 build/Colab ComfyUI.app
make dist     # 生成 dist/ 下的 dmg 和校验文件
```

编译、测试、打包都走 `make`。开发约定见 [AGENTS.md](AGENTS.md)。

## 许可证

[MIT](LICENSE.txt)
