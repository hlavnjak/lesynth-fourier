VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
PLUGIN := LeSynthFourier

# ── Linux (native) ──────────────────────────────────────────────────────────
LINUX_TARGET := x86_64-unknown-linux-gnu

# ── Windows (cross via mingw) ───────────────────────────────────────────────
WIN_TARGET := x86_64-pc-windows-gnu

# ── macOS (cross via zig) ───────────────────────────────────────────────────
# Both architectures are built and fused with lipo into one universal binary,
# so the same plugin loads on Intel Macs and on Apple silicon.
MAC_X86 := x86_64-apple-darwin
MAC_ARM := aarch64-apple-darwin
# Oldest macOS each slice runs on. arm64 Macs shipped with 11.0.
MAC_X86_MIN := 10.13
MAC_ARM_MIN := 11.0
# The SDK supplies the framework headers and link stubs (AppKit, OpenGL…) zig
# does not ship. Override with `make release-macos MACOS_SDK=...`.
MACOS_SDK ?= $(HOME)/.local/opt/MacOSX26.1.sdk
MAC_CC := $(CURDIR)/tools/zig-macos-cc.sh
LIPO ?= llvm-lipo
MAC_OUT := target/universal-apple-darwin/release

# `cargo zigbuild` for one Mac slice: $(1) = Rust target, $(2) = zig target
# (which carries the minimum version — see tools/zig-macos-cc.sh), $(3) = that
# minimum again, for rustc. The linker variables override .cargo/config.toml.
mac_cargo = SDKROOT=$(MACOS_SDK) MACOSX_DEPLOYMENT_TARGET=$(3) ZIG_MACOS_TARGET=$(2) \
	CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER=$(MAC_CC) \
	CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=$(MAC_CC) \
	cargo zigbuild --release --target $(1)
mac_cargo_x86 = $(call mac_cargo,$(MAC_X86),x86_64-macos.$(MAC_X86_MIN)-none,$(MAC_X86_MIN))
mac_cargo_arm = $(call mac_cargo,$(MAC_ARM),aarch64-macos.$(MAC_ARM_MIN)-none,$(MAC_ARM_MIN))

.PHONY: build build-windows build-macos macos-toolchain clean

# The plugin for this machine (what ReSynth Studio's `make build` embeds too).
build:
	cargo build --release --target $(LINUX_TARGET)

build-windows:
	cargo build --release --target $(WIN_TARGET)

# Fail up front, with the fix, rather than deep inside a linker error.
macos-toolchain:
	@command -v zig >/dev/null || { echo "error: zig not found (https://ziglang.org/download/)"; exit 1; }
	@command -v cargo-zigbuild >/dev/null || { echo "error: run 'cargo install cargo-zigbuild'"; exit 1; }
	@command -v $(LIPO) >/dev/null || { echo "error: $(LIPO) not found (part of LLVM); set LIPO=..."; exit 1; }
	@test -d "$(MACOS_SDK)/System/Library/Frameworks" || { \
		echo "error: no macOS SDK at $(MACOS_SDK); unpack one there or pass MACOS_SDK=..."; exit 1; }
	@rustup target list --installed | grep -qx $(MAC_X86) && \
		rustup target list --installed | grep -qx $(MAC_ARM) || { \
		echo "error: run 'rustup target add $(MAC_X86) $(MAC_ARM)'"; exit 1; }

# The universal plugin binary.
build-macos: | macos-toolchain
	$(mac_cargo_x86)
	$(mac_cargo_arm)
	@mkdir -p $(MAC_OUT)
	$(LIPO) -create -output $(MAC_OUT)/liblesynth_fourier.dylib \
		target/$(MAC_X86)/release/liblesynth_fourier.dylib \
		target/$(MAC_ARM)/release/liblesynth_fourier.dylib
	@$(LIPO) -info $(MAC_OUT)/liblesynth_fourier.dylib

# ── Release ─────────────────────────────────────────────────────────────────
# `make release` writes releases/$(VERSION)/lesynth_fourier-v$(VERSION)-<format>-
# <platform>.zip for VST3 and CLAP on Linux, Windows and macOS, plus SHA256SUMS.
# Each zip is one folder: the plugin, LICENSE, README.txt and
# THIRD-PARTY-LICENSES.txt. The macOS bundles are signed and notarised on the
# Mac that packaging/macos/sign.env names (gitignored; see tools/macos-sign.sh).
#
# The Linux binary is not the `make build` one: that links against this
# machine's glibc and refuses to load on any older one (a rolling distro's is
# newer than every LTS). zig links it against glibc $(PORTABLE_GLIBC) instead —
# Ubuntu 20.04 / Debian 11 and later — in a target dir of its own, so it never
# mixes with the native build. ReSynth Studio's `make release` embeds this same
# binary.
RELEASE_DIR := releases/$(VERSION)
RELEASE_STAGE := target/release-stage
PORTABLE_GLIBC := 2.31
PORTABLE_DIR := target/portable
PORTABLE_CARGO = PKG_CONFIG_ALLOW_CROSS=1 cargo zigbuild --release \
	--target $(LINUX_TARGET).$(PORTABLE_GLIBC) --target-dir $(PORTABLE_DIR)
PORTABLE_SO := $(PORTABLE_DIR)/$(LINUX_TARGET)/release/liblesynth_fourier.so
THIRD_PARTY = python3 tools/third-party-licenses.py
MAC_SIGN_ENV ?= packaging/macos/sign.env

# Folder (and zip) name for $(1) = format, $(2) = platform.
release_name = lesynth_fourier-v$(VERSION)-$(1)-$(2)
# README.txt for $(1) = platform, $(2) = format, $(3) = the plugin's file name,
# $(4) = the folder it is installed into.
release_readme = sed -e 's|@VERSION@|$(VERSION)|' -e 's|@FORMAT@|$(2)|' \
	-e 's|@PLUGIN@|$(3)|' -e 's|@DIR@|$(4)|' packaging/release/readme/head.txt \
	packaging/release/readme/$(1).txt packaging/release/readme/tail.txt
# LICENSE, README.txt and THIRD-PARTY-LICENSES.txt into the release folder
# $(1), for $(2) = platform, $(3) = format, $(4) = plugin file, $(5) = install
# folder; THIRD-PARTY-LICENSES.txt is copied from $(RELEASE_STAGE)/$(2).txt.
release_extras = cp LICENSE $(1)/ && \
	$(call release_readme,$(2),$(3),$(4),$(5)) > $(1)/README.txt && \
	cp $(RELEASE_STAGE)/$(2).txt $(1)/THIRD-PARTY-LICENSES.txt
# Zip the release folder $(2) staged under $(RELEASE_STAGE)/$(1).
release_zip = rm -f $(RELEASE_DIR)/$(2).zip && cd $(RELEASE_STAGE)/$(1) && \
	zip -q -r -X -9 $(CURDIR)/$(RELEASE_DIR)/$(2).zip $(2)

VST3_LINUX := $(call release_name,vst3,linux)
CLAP_LINUX := $(call release_name,clap,linux)
VST3_WIN := $(call release_name,vst3,win)
CLAP_WIN := $(call release_name,clap,win)
VST3_MAC := $(call release_name,vst3,macos)
CLAP_MAC := $(call release_name,clap,macos)

.PHONY: release release-check release-linux release-windows release-macos

release: release-linux release-windows release-macos
	cd $(RELEASE_DIR) && sha256sum lesynth_fourier-v$(VERSION)-*.zip > SHA256SUMS
	@cat $(RELEASE_DIR)/SHA256SUMS

release-check:
	@command -v cargo-zigbuild >/dev/null || { echo "error: run 'cargo install cargo-zigbuild'"; exit 1; }
	@command -v zip >/dev/null || { echo "error: zip not found"; exit 1; }
	@mkdir -p $(RELEASE_DIR) $(RELEASE_STAGE)

release-linux: release-check
	$(PORTABLE_CARGO)
	rm -rf $(RELEASE_STAGE)/linux
	mkdir -p $(RELEASE_STAGE)/linux/$(VST3_LINUX)/$(PLUGIN).vst3/Contents/x86_64-linux \
		$(RELEASE_STAGE)/linux/$(CLAP_LINUX)
	strip --strip-unneeded -o $(RELEASE_STAGE)/linux/$(PLUGIN).so $(PORTABLE_SO)
	cp $(RELEASE_STAGE)/linux/$(PLUGIN).so \
		$(RELEASE_STAGE)/linux/$(VST3_LINUX)/$(PLUGIN).vst3/Contents/x86_64-linux/$(PLUGIN).so
	cp $(RELEASE_STAGE)/linux/$(PLUGIN).so $(RELEASE_STAGE)/linux/$(CLAP_LINUX)/$(PLUGIN).clap
	$(THIRD_PARTY) $(RELEASE_STAGE)/linux.txt $(LINUX_TARGET) .
	$(call release_extras,$(RELEASE_STAGE)/linux/$(VST3_LINUX),linux,VST3,$(PLUGIN).vst3,~/.vst3/)
	$(call release_extras,$(RELEASE_STAGE)/linux/$(CLAP_LINUX),linux,CLAP,$(PLUGIN).clap,~/.clap/)
	$(call release_zip,linux,$(VST3_LINUX))
	$(call release_zip,linux,$(CLAP_LINUX))

# Text files get CRLF line endings, which is what Notepad on an older Windows
# needs to show them as more than one line.
release-windows: release-check build-windows
	rm -rf $(RELEASE_STAGE)/windows
	mkdir -p $(RELEASE_STAGE)/windows/$(VST3_WIN)/$(PLUGIN).vst3/Contents/x86_64-win \
		$(RELEASE_STAGE)/windows/$(CLAP_WIN)
	cp target/$(WIN_TARGET)/release/lesynth_fourier.dll \
		$(RELEASE_STAGE)/windows/$(VST3_WIN)/$(PLUGIN).vst3/Contents/x86_64-win/$(PLUGIN).vst3
	cp target/$(WIN_TARGET)/release/lesynth_fourier.dll $(RELEASE_STAGE)/windows/$(CLAP_WIN)/$(PLUGIN).clap
	$(THIRD_PARTY) $(RELEASE_STAGE)/windows.txt $(WIN_TARGET) .
	$(call release_extras,$(RELEASE_STAGE)/windows/$(VST3_WIN),windows,VST3,$(PLUGIN).vst3,C:\\Program Files\\Common Files\\VST3\\)
	$(call release_extras,$(RELEASE_STAGE)/windows/$(CLAP_WIN),windows,CLAP,$(PLUGIN).clap,C:\\Program Files\\Common Files\\CLAP\\)
	for d in $(RELEASE_STAGE)/windows/$(VST3_WIN) $(RELEASE_STAGE)/windows/$(CLAP_WIN); do \
		sed -i 's/(LICENSE)/(LICENSE.txt)/; s/$$/\r/' $$d/README.txt $$d/THIRD-PARTY-LICENSES.txt; \
		sed 's/$$/\r/' $$d/LICENSE > $$d/LICENSE.txt && rm $$d/LICENSE; done
	$(call release_zip,windows,$(VST3_WIN))
	$(call release_zip,windows,$(CLAP_WIN))

# On macOS both formats are bundles: Contents/MacOS/$(PLUGIN) is the universal
# binary. That is what lets the notarisation ticket be stapled to the CLAP too
# (a bare .clap file could be notarised, but not stapled).
# $(1) = bundle path, $(2) = bundle identifier.
mac_bundle = mkdir -p $(1)/Contents/MacOS && \
	cp $(MAC_OUT)/liblesynth_fourier.dylib $(1)/Contents/MacOS/$(PLUGIN) && \
	sed -e 's/@VERSION@/$(VERSION)/' -e 's/@MIN_MACOS@/$(MAC_X86_MIN)/' \
		-e 's/@BUNDLE_ID@/$(2)/' packaging/macos/Info.plist > $(1)/Contents/Info.plist && \
	printf 'BNDL????' > $(1)/Contents/PkgInfo

release-macos: release-check build-macos
	rm -rf $(RELEASE_STAGE)/macos
	mkdir -p $(RELEASE_STAGE)/macos
	$(call mac_bundle,$(RELEASE_STAGE)/macos/$(VST3_MAC)/$(PLUGIN).vst3,com.hlavnjak.lesynthfourier)
	$(call mac_bundle,$(RELEASE_STAGE)/macos/$(CLAP_MAC)/$(PLUGIN).clap,com.hlavnjak.lesynthfourier.clap)
	$(THIRD_PARTY) $(RELEASE_STAGE)/macos.txt $(MAC_ARM) .
	$(call release_extras,$(RELEASE_STAGE)/macos/$(VST3_MAC),macos,VST3,$(PLUGIN).vst3,/Library/Audio/Plug-Ins/VST3/)
	$(call release_extras,$(RELEASE_STAGE)/macos/$(CLAP_MAC),macos,CLAP,$(PLUGIN).clap,/Library/Audio/Plug-Ins/CLAP/)
	MAC_SIGN_ENV=$(MAC_SIGN_ENV) tools/macos-sign.sh $(RELEASE_STAGE)/macos $(RELEASE_DIR)

clean:
	cargo clean
