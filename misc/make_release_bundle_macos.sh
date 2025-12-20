#!/usr/bin/env bash
set -euo pipefail

# --- configurable bits ---
PLUGIN_NAME="LeSynthFourier"             # bundle + executable name (no extension)
CRATE_DYLIB_NAME="liblesynth_fourier.dylib"  # produced by cargo for macOS
VERSION="1.2.0"
CARGO_TOOLCHAIN="+1.88.0"                # or "" to use default toolchain

# Rust targets
TARGET_X86="x86_64-apple-darwin"
TARGET_ARM="aarch64-apple-darwin"

# Paths
BUILD_DIR_X86="target/${TARGET_X86}/release"
BUILD_DIR_ARM="target/${TARGET_ARM}/release"
UNIV_DIR="target/universal2/release"
DIST_DIR="dist"
BUNDLE_ROOT="${DIST_DIR}/${PLUGIN_NAME}.vst3"
CONTENTS_DIR="${BUNDLE_ROOT}/Contents"
MACOS_DIR="${CONTENTS_DIR}/MacOS"
RES_DIR="${CONTENTS_DIR}/Resources"

# Tools (allow override via env)
: "${LIPO:=lipo}"

# --- build both architectures ---
echo "==> Building ${PLUGIN_NAME} (${VERSION}) for macOS targets"
cargo ${CARGO_TOOLCHAIN} build --release --target "${TARGET_X86}"
cargo ${CARGO_TOOLCHAIN} build --release --target "${TARGET_ARM}"

# --- verify artifacts ---
BIN_X86="${BUILD_DIR_X86}/${CRATE_DYLIB_NAME}"
BIN_ARM="${BUILD_DIR_ARM}/${CRATE_DYLIB_NAME}"
[[ -f "${BIN_X86}" ]] || { echo "missing ${BIN_X86}"; exit 1; }
[[ -f "${BIN_ARM}" ]] || { echo "missing ${BIN_ARM}"; exit 1; }

# --- lipo into universal2 ---
echo "==> Creating universal2 binary"
mkdir -p "${UNIV_DIR}"
UNIV_BIN="${UNIV_DIR}/${CRATE_DYLIB_NAME}"
"${LIPO}" -create "${BIN_X86}" "${BIN_ARM}" -output "${UNIV_BIN}"

# --- (re)create bundle layout ---
echo "==> Assembling .vst3 bundle"
rm -rf "${BUNDLE_ROOT}"
mkdir -p "${MACOS_DIR}" "${RES_DIR}"

# Copy the universal dylib into bundle and rename to executable name (no extension)
cp "${UNIV_BIN}" "${MACOS_DIR}/${PLUGIN_NAME}"
chmod +x "${MACOS_DIR}/${PLUGIN_NAME}"

# --- write Info.plist ---
cat > "${CONTENTS_DIR}/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <!-- human-friendly name -->
  <key>CFBundleName</key>
  <string>__CFBUNDLE_NAME__</string>

  <!-- reverse-DNS identifier (no spaces or hyphens, adjust as you like) -->
  <key>CFBundleIdentifier</key>
  <string>com.hlavnjak.lesynthfourier</string>

  <!-- marketing version -->
  <key>CFBundleShortVersionString</key>
  <string>__VERSION__</string>

  <!-- build/version code -->
  <key>CFBundleVersion</key>
  <string>__VERSION__</string>

  <!-- bundle metadata -->
  <key>CFBundlePackageType</key>
  <string>BNDL</string>
  <key>CFBundleExecutable</key>
  <string>__EXECUTABLE__</string>

  <!-- optional but nice to have -->
  <key>LSMinimumSystemVersion</key>
  <string>10.13</string>
</dict>
</plist>
PLIST

# inject variables into Info.plist
sed -i \
  -e "s#__CFBUNDLE_NAME__#${PLUGIN_NAME}#g" \
  -e "s#__EXECUTABLE__#${PLUGIN_NAME}#g" \
  -e "s#__VERSION__#${VERSION}#g" \
  "${CONTENTS_DIR}/Info.plist"

echo "  Bundle ready at: ${BUNDLE_ROOT}"
echo "   Contents:"
command -v tree >/dev/null 2>&1 && tree -a "${BUNDLE_ROOT}" || find "${BUNDLE_ROOT}" -print
