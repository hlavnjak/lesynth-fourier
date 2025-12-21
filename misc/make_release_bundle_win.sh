#!/usr/bin/env bash
set -euo pipefail

PLUGIN_NAME="LeSynthFourier"
DLL_NAME="lesynth_fourier.dll"
VERSION="1.2.0"
TARGET="x86_64-pc-windows-gnu"
BUILD_DIR="target/${TARGET}/release"
DIST_DIR="dist"

# Build the plugin DLL
cargo +1.88.0 build --release --target "$TARGET"

# Clean and recreate dist structure
rm -rf "${DIST_DIR:?}/${PLUGIN_NAME}.vst3"
mkdir -p "${DIST_DIR}/${PLUGIN_NAME}.vst3/Contents/x86_64-win"

# Copy the DLL into bundle as VST3
cp "${BUILD_DIR}/${DLL_NAME}" \
   "${DIST_DIR}/${PLUGIN_NAME}.vst3/Contents/x86_64-win/${PLUGIN_NAME}.vst3"

# Create CLAP plugin
CLAP_NAME="${PLUGIN_NAME}.clap"
cp "${BUILD_DIR}/${DLL_NAME}" "${DIST_DIR}/${CLAP_NAME}"

# Package into separate archives
(
  cd "$DIST_DIR"
  # VST3 archive
  zip -r "lesynth_fourier-v${VERSION}-vst3-win.zip" "${PLUGIN_NAME}.vst3"
  
  # CLAP archive
  zip "lesynth_fourier-v${VERSION}-clap-win.zip" "${CLAP_NAME}"
)

echo "Windows bundles created:"
echo "  VST3: ${DIST_DIR}/lesynth_fourier-v${VERSION}-vst3-win.zip"
echo "  CLAP: ${DIST_DIR}/lesynth_fourier-v${VERSION}-clap-win.zip"
