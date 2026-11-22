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

工程刚搭好，**还不能用**。打开后的窗口只显示版本号，以及在这台 Mac 上找到的 `colab` 命令的位置。

打算做的事：

- 一键开机：建 Colab 会话、挂 Google Drive、装好 ComfyUI、接通到本机的隧道
- 在 app 的窗口里直接显示远端的 ComfyUI 界面
- 显示会话状态、GPU 占用、计算单元余额和消耗速率
- 停机前把工作流、设置和成品导出到 Drive，停机后核对已经不再计费
- 按需下载模型

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
