#!/usr/bin/env bash
set -euo pipefail

PLUGIN_NAME="LeSynthFourier"
# Rust cdylib on Linux usually produces "lib<crate>.so"
SO_CANDIDATES=("liblesynth_fourier.so" "lesynth_fourier.so")
VERSION="1.2.0"
TARGET="x86_64-unknown-linux-gnu"
BUILD_DIR="target/${TARGET}/release"
DIST_DIR="dist"

# 1) Build the plugin .so
cargo build --release --target "$TARGET"

# 2) Pick the produced .so (handles both lib-prefixed and non-prefixed)
SO_NAME=""
for cand in "${SO_CANDIDATES[@]}"; do
  if [[ -f "${BUILD_DIR}/${cand}" ]]; then
    SO_NAME="$cand"
    break
  fi
done
if [[ -z "$SO_NAME" ]]; then
  echo "Could not find built .so. Looked for: ${SO_CANDIDATES[*]} in ${BUILD_DIR}"
  exit 1
fi

# 3) Create the VST3 bundle structure
rm -rf "${DIST_DIR:?}/${PLUGIN_NAME}.vst3"
mkdir -p "${DIST_DIR}/${PLUGIN_NAME}.vst3/Contents/x86_64-linux"

# 4) Copy the .so into the bundle
cp "${BUILD_DIR}/${SO_NAME}" \
   "${DIST_DIR}/${PLUGIN_NAME}.vst3/Contents/x86_64-linux/${PLUGIN_NAME}.so"

# (Optional) Strip for smaller size — comment out if you prefer full symbols
strip --strip-unneeded "${DIST_DIR}/${PLUGIN_NAME}.vst3/Contents/x86_64-linux/${PLUGIN_NAME}.so" || true

# 5) Create CLAP plugin
CLAP_NAME="${PLUGIN_NAME}.clap"
cp "${BUILD_DIR}/${SO_NAME}" "${DIST_DIR}/${CLAP_NAME}"

# Strip CLAP plugin too
strip --strip-unneeded "${DIST_DIR}/${CLAP_NAME}" || true

# 6) Create separate archives
(
  cd "$DIST_DIR"
  # VST3 archive
  zip -r "lesynth_fourier-v${VERSION}-vst3-linux.zip" "${PLUGIN_NAME}.vst3"
  
  # CLAP archive
  zip "lesynth_fourier-v${VERSION}-clap-linux.zip" "${CLAP_NAME}"
)

echo "Linux bundles created:"
echo "  VST3: ${DIST_DIR}/lesynth_fourier-v${VERSION}-vst3-linux.zip"
echo "  CLAP: ${DIST_DIR}/lesynth_fourier-v${VERSION}-clap-linux.zip"
