.PHONY: build test lint dev app dist frontend locked-build locked-test locked-lint locked-app locked-dist

APP_NAME := Colab ComfyUI

# 编译缓存只有一份，放在主目录的 target 里；git worktree 里执行时也用主目录的那一份，不各存一份。
# 主目录是 .git 所在的文件夹；不在 git 仓库里时就用当前文件夹。
MAIN_DIR := $(shell common="$$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" && dirname "$$common")
TARGET := $(if $(MAIN_DIR),$(MAIN_DIR),$(CURDIR))/target
export CARGO_TARGET_DIR := $(TARGET)

# 同一时间只让一个文件夹编译。几个 worktree 共用一份缓存，打出来的 app 和 dmg 也在同一个位置：
# 不排队的话，这边刚打完还没拷走，那边就把它换成了另一个分支的。
# 锁被占着时先说一声，再等前一个做完。$(1) 是拿到锁之后要执行的目标。
LOCK := $(TARGET)/colab-comfyui-make.lock
define queued
mkdir -p "$(TARGET)" && \
{ /usr/bin/lockf -k -t 0 "$(LOCK)" true 2>/dev/null || echo "另一个文件夹正在用 $(TARGET) 编译，等它做完…"; } && \
/usr/bin/lockf -k "$(LOCK)" $(MAKE) --no-print-directory $(1)
endef

# 签名身份。默认 ad-hoc；有 Developer ID 证书时用 make dist SIGN_IDENTITY="Developer ID Application: …"。
SIGN_IDENTITY ?= -
export APPLE_SIGNING_IDENTITY := $(SIGN_IDENTITY)

# 发布包的名字带版本号和平台。版本号只写在根目录 Cargo.toml 的 [workspace.package] 里。
VERSION := $(shell sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' Cargo.toml)
PLAT := macOS-$(if $(filter arm64,$(shell uname -m)),arm64,x64)
DMG := Colab-ComfyUI-v$(VERSION)-$(PLAT).dmg
BUNDLE := $(TARGET)/release/bundle

# 每个文件夹各装一份 node_modules。package-lock.json 变了才重装。
node_modules/.package-lock.json: package-lock.json
	npm ci
	@touch $@

# src-tauri 编译时要把 web/ 里的界面编进二进制，所以先有 web/ 才能编译。
frontend: node_modules/.package-lock.json
	npm run build

build: frontend
	@$(call queued,locked-build)

locked-build:
	cargo build --workspace --release --locked

# 只跑一部分测试：make test FILTER=path_wins。环境变量照常写在 make 前面。
test: frontend
	npm run check
	@$(call queued,locked-test)

locked-test:
	cargo test --workspace --locked $(if $(FILTER),'$(subst ','"'"',$(FILTER))')

lint:
	@$(call queued,locked-lint)

locked-lint: frontend
	cargo fmt --all --check
	cargo clippy --workspace --all-targets --locked -- -D warnings

# 开发时用。它一直开着，所以不拿编译的锁。
dev: node_modules/.package-lock.json
	npx tauri dev

app: node_modules/.package-lock.json
	@$(call queued,locked-app)

# 先删掉旧的再拷：直接覆盖时会留下上一版里已经不存在的文件。
locked-app:
	npx tauri build --bundles app
	rm -rf "build/$(APP_NAME).app"
	mkdir -p build
	ditto "$(BUNDLE)/macos/$(APP_NAME).app" "build/$(APP_NAME).app"

dist: node_modules/.package-lock.json
	@$(call queued,locked-dist)

# dmg 里是 app 和一个指向 /Applications 的链接。打之前清掉 target 里上一次的 dmg，免得拷错。
locked-dist:
	rm -rf dist "$(BUNDLE)/dmg"
	npx tauri build --bundles app,dmg
	mkdir -p dist
	cp "$(BUNDLE)"/dmg/*.dmg "dist/$(DMG)"
	cd dist && shasum -a 256 "$(DMG)" > "$(DMG).sha256"
