# colab-comfyUI

本仓库做一个 macOS 桌面 app：在 Google Colab 的 GPU 上开 ComfyUI，在 app 的窗口里直接用它的界面，并管开机、停机、余额和模型。它替代的是一组要在终端里敲的 `colab` 指令。后续实现不要偏离下面的约束。

约束的写法：每条写明原因。在本机试出来的结论标“实测”。用户定下来的事写明哪天定的，免得以后被当成还没定。

## 语言和界面

- 逻辑用 Rust，桌面壳用 Tauri 2，壳的界面用 Svelte + TypeScript + Vite，包管理用 npm（用户 2026-11-22 决定）。不要改成 Swift、Electron、Python 或纯网页。
- 工程是一个 Cargo workspace，分三块。`crates/core` 是全部逻辑：找 `colab`、会话、隧道、状态、停机。`src-tauri` 是壳，只把 core 的函数包成 Tauri command。`src/` 是壳的界面。界面要的新能力先在 core 里做成函数并带上测试，再接到界面上。壳和界面里不写业务逻辑，因为那里的代码测不到。
- ComfyUI 的界面不打进 app。窗口加载的是隧道接过来的 `http://127.0.0.1:<端口>`，界面由远端的 ComfyUI 提供，和浏览器里看到的一样，版本永远和远端一致（用户 2026-11-22 决定）。不要把 ComfyUI 的前端代码拷进仓库。
- 显示远端页面的窗口不给任何 Tauri 权限。`src-tauri/capabilities/` 里只列壳自己的窗口，不写 `remote`。远端页面里跑的是 ComfyUI 和它的插件的代码，不能让它们调用本机的指令。
- 同一时间只运行一个 app。用的是 `tauri-plugin-single-instance`：再打开一次时不出第二个进程，把已有的窗口调到前面（实测再 `open -n` 一次，进程仍是一个）。
- 目前只做 macOS，最低 13.0。

## 外部程序

- 操作 Colab 只通过 `colab` 命令（`google-colab-cli`，本机是 0.8.0）。不直接调它背后的接口，不读写它自己的登录文件和 `~/.config/colab-cli/`。
- 找 `colab` 的顺序：先 `PATH`，再 `~/.local/bin`、`/opt/homebrew/bin`、`/usr/local/bin`（`colab_comfy_core::find_colab`）。被系统打开的 app，`PATH` 只有 `/usr/bin:/bin:/usr/sbin:/sbin`（`agy-cli-switcher` 实测），而 `colab` 装在 `~/.local/bin`，只靠 `PATH` 找不到。
- `ssh` 固定用 `/usr/bin/ssh`，不从 `PATH` 里找。
- 本机 `~/alysechen/github/colab/` 里有一组跑通了的脚本（`start.sh`、`remote_setup.sh`、`stop.sh` 等），是实现时的参考：建会话、挂 Drive、上传并执行远端脚本、用 `ProxyCommand=colab ssh --proxy-mode` 开 `8188` 端口的隧道、停机前导出到 Drive。只参考，不拷进仓库，也不让 app 去执行它们。

## 计费

- 会话开着就在消耗计算单元，和 app 开没开着无关。退出 app、关窗口、断开隧道都不等于停机。
- 会话还开着时退出 app，要明确告诉用户会话没停，并给出停机的入口。不要悄悄留着它，也不要不问就停。
- 停机之后用 `colab usage` 核对速率已经是 0，核对不到就照实说没核对到，不说已停机。
- 默认的测试和 CI 不建会话、不联网。要花计算单元的测试用环境变量单独打开，见“验证”。

## 登录数据和密钥

- 仓库里不放任何登录数据。不要把 token、OAuth 文件的内容、会话的地址写进仓库、日志、提交说明或这份规则。
- `.gitignore` 里“登录和密钥”那一段不要放宽。
- app 自己要存东西时放在 `~/.colab-comfyui/`，目录权限 `700`，文件权限 `600`。这里不放 token。

## 构建

- 编译、测试、打包一律用 `make`，不直接执行 `cargo build`、`cargo test`、`npx tauri build`。直接执行会绕过下面的共用缓存和排队。
  - `make lint`：`cargo fmt --check` 和 `cargo clippy -D warnings`。
  - `make test`：`svelte-check` 加 `cargo test`。只跑一部分用 `make test FILTER=<测试名>`，环境变量照常写在 `make` 前面。
  - `make build`：只编译，不打包。
  - `make app`：生成 `build/Colab ComfyUI.app`。
  - `make dist`：生成 `dist/Colab-ComfyUI-v<版本号>-macOS-<arm64 或 x64>.dmg` 和同名加 `.sha256` 的校验文件。
  - `make dev`：开发时用，改了代码自动重开。它一直开着，不拿编译的锁。
- `src-tauri` 编译时要把 `web/` 里的界面编进二进制，没有 `web/` 就编不过。所以 `build`、`test`、`lint` 都先执行 `npm run build`。
- 依赖的版本以 `Cargo.lock` 和 `package-lock.json` 为准，两个都进仓库。`make` 里的 cargo 都带 `--locked`，npm 用 `npm ci`。加依赖或升级时选发布满 7 天的版本，并在 PR 里写明升了什么。
- Tauri 一族（`tauri`、`tauri-build`、`tauri-runtime`、`tauri-runtime-wry`、`tauri-macros`、`tauri-codegen`、`tauri-utils`、`wry`、`tao`）和 npm 的 `@tauri-apps/cli`、`@tauri-apps/api` 要一起升。只升其中一个会得到一套没人配过的组合：`cargo update -p tauri --precise` 只动 `tauri` 自己，别的留在新版本上（实测）。
- 根目录 `Cargo.toml` 里 `[profile.release]` 的 `strip = false` 不要去掉。`tauri build` 按 `minimumSystemVersion` 设置 `MACOSX_DEPLOYMENT_TARGET`；它是 13.0、14.0 或 15.0 时，release 默认的 strip 会把过程宏的 dylib 和最终的二进制变成 `dyld_info` 报 `mis-aligned LINKEDIT string pool` 的文件，编译停在 `can't find crate for phf_macros`（macOS 27.3 实测；不设或者设成 10.13 时没有这个问题，关不关 sccache 都一样）。
- 图标在 `src-tauri/icons/`，现在是占位的。换图标时准备一张 1024×1024 的 png，执行 `npx tauri icon <png>`，只留 `tauri.conf.json` 里列着的那几个文件。

## 版本和发布

- 版本号只写在根目录 `Cargo.toml` 的 `[workspace.package]` 里。两个 crate 用 `version.workspace = true`，`tauri.conf.json` 不写 `version`，`package.json` 不写 `version`，`Makefile` 和 workflow 都从 `Cargo.toml` 读。每次升版本同时写 `documents/CHANGELOG.md`，中英文各一段。
- 发布：推 `v` 加版本号的 tag，`release.yml` 在 Apple 芯片和 Intel 的机器上各跑一遍测试、各执行一次 `make dist`，然后建 draft release。它只建 draft，不要在 workflow 里加发正式版的步骤。
- 发布包按架构分成两个，不出通用包：一个包 8 MB 左右，合起来翻倍。
- 签名是 ad-hoc，没有公证，因为没有 Developer ID 证书；浏览器下载的 app 第一次打开要到“系统设置 → 隐私与安全性”里放行。以后有证书时用 `make dist SIGN_IDENTITY=…`，公证另加。
- README 是中英两份（`README.md`、`README.en.md`），结构对应。改了用户看得到的行为，在同一个 PR 里把两份都改掉。

## 验证

- 说完成之前跑 `make lint` 和 `make test`，并引用它们的输出。
- 改了打包或启动方式之后执行 `make app`，核对三件事：`codesign --verify --deep --strict "build/Colab ComfyUI.app"` 通过；`dyld_info "build/Colab ComfyUI.app/Contents/MacOS/colab-comfyui"` 不报错；`open "build/Colab ComfyUI.app"` 之后进程留着、窗口出现。
- 这台机器上代理没有截屏和辅助功能的权限（实测 `screencapture` 和 System Events 都被拒绝），看不到窗口里的内容。要确认界面真的画出来、能调到 Rust，就临时在被调用的 command 里加一行 `eprintln!`，在终端里直接运行包里的二进制看有没有打印，看完去掉。
- 要花计算单元的真实往返单独打开：环境变量 `COLAB_LIVE=1` 加 `make test FILTER=<测试名>`。这类测试没开这个变量时直接跳过；开了的话，结束时自己停掉建的会话，并核对 `colab usage` 的速率。它们用单独的会话名，不碰用户正在用的会话。
- 输出里不打印 token 和会话的地址。

## 方案和留档

- 要改行为的任务，先给出完整的实现方案，写进 Linear 上对应 issue 的评论，然后停下来。用户明确同意之后才动代码。方案要写到能照着做：改哪些地方、每种情况下怎么表现、哪几处是自己定的、怎么验证、哪些不做。
- issue 放在 Linear 的 `colab-comfyUI` project 里。没有对应的 issue 时自己新建一个，不用先问。
- 调研结果、分析过程和相关决策都要在对应的 issue 下留档，不只留在对话里。这些东西会有好几轮：每一轮新的内容接在后面新写一条评论，不回头改旧评论，也不为了归类把不同轮的内容并进同一条。只有和已有评论明显重复的才不再写。决定不做的事同样记一条，写明是谁在哪天决定的。
- 状态跟着进度改，不用先问：方案定下来、开始动代码时改成 `In Progress`；PR 合并之后改成 `Done`。一个 issue 分几个 PR 做时，最后一个合并了才改成 `Done`。
- Linear 的写法按 `linear-index` skill 的约定，动手前先加载它。

## 本地开发用 worktree

- 主目录 `~/alysechen/github/colab-comfyUI` 只停在 `main`，不在这里建分支、改跟踪的文件、commit。它只做这几件事：读规则、`git pull --ff-only`、放不进仓库的本地文件。
- 每个开发分支各用一个 worktree，放在 `~/alysechen/github/colab-comfyUI-worktree/colab-comfyUI-<分支后缀>`。分支后缀是分支名把 `/` 换成 `-`，比如 `feat/session-panel` 对应 `colab-comfyUI-feat-session-panel`。几个 worktree 可以同时开发，互不影响。
- 建 worktree：在主目录先 `git pull --ff-only`，再执行 `git worktree add -b <分支> <worktree 路径> main`。不要用 `cp` 拷文件夹。同一个分支不能同时在两个文件夹里检出，所以主目录不离开 `main`。
- 会话的工作目录保持主目录，这样读到的始终是主目录的 `AGENTS.md`。读写文件一律用 worktree 的绝对路径；`make`、`git` 用 worktree 路径作为执行目录。不要用主目录的路径改跟踪的文件。
- 规则以主目录的 `AGENTS.md` 为准。要改规则时改 worktree 里的那一份，合并之后主目录 pull 下来才生效。
- 编译缓存只有一份，是主目录的 `target/`。`Makefile` 用 `git rev-parse --git-common-dir` 找到主目录，把它的 `target` 设成 `CARGO_TARGET_DIR`；worktree 里不会另外生成 `target/`（实测）。
- 同一时间只有一个文件夹在编译。`build`、`test`、`lint`、`app`、`dist` 先用 `/usr/bin/lockf` 拿主目录 `target/colab-comfyui-make.lock` 上的锁，再做事；锁被占着时说一声，然后等前一个做完。锁要把“打包”和“把 app 拷到自己的 `build/`”一起包住：几个 worktree 打出来的 app 在 `target/` 里的同一个位置，只锁编译的话，这边还没拷走，那边就换成了另一个分支的。实测 worktree 里 `make app`、主目录同时 `make dist`，后一个等前一个，各自拿到自己的包。
- `node_modules/` 和 `web/` 每个 worktree 各有一份，由 `make` 自己装、自己生成，不用手动处理。
- 要手测就 `open "<worktree 路径>/build/Colab ComfyUI.app"`。已经有一个 Colab ComfyUI 在运行时先退出它，不然新开的这个只会把旧的窗口调到前面。
- 不进仓库的本地文件只放主目录，不放 worktree。只在本机用的，把路径写进主目录的 `.git/info/exclude`，它对所有 worktree 生效，不用开 PR。忽略规则要进仓库时才开 worktree 改 `.gitignore`。worktree 里要用这些文件时按主目录的绝对路径读，不拷贝。

## 合并之后

- 合并 PR 仍要用户当条消息明确说合并。合并完成后，下面几步默认接着做，不用再问：
  1. 确认 worktree 的分支和合并后的 `main` 内容一致（先 `git fetch`，再 `git diff --quiet origin/main <分支>`）。不一致就停下来报告，不删。
  2. 在 worktree 里执行 `git status --short --ignored`。有未提交的改动就停下来报告。列出的未跟踪和被忽略的文件，除了 `build/`、`dist/`、`web/`、`node_modules/`、`src-tauri/gen/`，逐个确认可以丢；要留的先搬到主目录。`git worktree remove` 遇到未跟踪的文件会拒绝，被忽略的文件却会不加提示地一起删掉（`agy-cli-switcher` 实测）。
  3. 主目录 `git pull --ff-only`。
  4. 把这个 worktree 自己打出来的测试 app 一起清掉（只管这个 worktree 的，别处的 app 不动），再 `git worktree remove <worktree 路径>`。不带 `--force`；它拒绝就回到第 2 步。清 app 分三步：正在运行的 Colab ComfyUI 如果是这个 worktree 里的那一份（`pgrep -fl "Colab ComfyUI.app"` 看路径），先退出它；对 `<worktree 路径>/build/Colab ComfyUI.app` 执行一次 `lsregister -u`（在 `/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/` 下），把它从系统的 app 名单里去掉；`git worktree remove` 会连同 `build/` 和 `dist/` 一起删掉文件，删完核对文件夹已经不在。只删文件不去登记的话，系统里会留下指向已删路径的 app（`agy-cli-switcher` 实测）。
  5. 删除本地分支（squash 合并后要用 `git branch -D`）和远端分支（`git push origin --delete <分支>`），再 `git fetch --prune`。
  6. 用 `git worktree list` 和 `git branch -a` 核对 worktree 和两个分支都已消失。
- 只删刚合并的那个 PR 的 worktree 和分支，其他的不动。

## 清理残留

- 用户说清理构建和测试残留时，删这些：`build/`、`dist/`、`target/release/bundle/`、`/tmp` 下本项目留下的文件。删之前核对内容确实是本项目产生的。
- 不删主目录的 `target/` 其余部分。它是所有 worktree 共用的编译缓存，删掉之后下次要从头编译。
- 不删 `~/.colab-comfyui/`、`~/.config/colab-cli/`，也不停用户的 Colab 会话。
