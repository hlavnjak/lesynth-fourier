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

# Copy the DLL into bundle
cp "${BUILD_DIR}/${DLL_NAME}" \
   "${DIST_DIR}/${PLUGIN_NAME}.vst3/Contents/x86_64-win/"

# Package into zip with new name
(
  cd "$DIST_DIR"
  zip -r "lesynth_fourier_win_${VERSION}.zip" "${PLUGIN_NAME}.vst3"
)

echo "Bundle created at ${DIST_DIR}/lesynth_fourier_win_${VERSION}.zip"
