#!/bin/sh
# Linker / C compiler for the macOS cross build (`make release-macos`).
#
# cargo-zigbuild hands zig an unversioned `-target x86_64-macos-none`, and zig
# then stamps every binary "minimum macOS 13" — which no Intel Mac older than
# 2017 can run, whatever MACOSX_DEPLOYMENT_TARGET says. This passes the versioned
# target instead (e.g. x86_64-macos.10.13-none) and leaves the rest of
# cargo-zigbuild's argument fixing as it is. The Makefile sets ZIG_MACOS_TARGET.
exec cargo-zigbuild zig cc -- -g -fno-sanitize=all -target "$ZIG_MACOS_TARGET" "$@"
