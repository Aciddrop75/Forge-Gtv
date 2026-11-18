#!/usr/bin/env bash
# The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
# SPDX-License-Identifier: GPL-3.0-or-later
# This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.
# Linux release: test, build an installer that runs on old distros (glibc 2.17+) when zig is available,
# then write dist/forge-gtv-setup-<version>-linux-x86_64, SHA256SUMS and the source + Windows-kit archive.
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')
OUT=${OUT:-dist}
mkdir -p "$OUT"

echo "== tests"
cargo test --release 2>&1 | grep -E "^test result|FAILED|error" || true
cargo test --release >/dev/null

BIN=target/release/forge-gtv
if command -v cargo-zigbuild >/dev/null && python3 -m ziglang version >/dev/null 2>&1; then
  echo "== build (glibc 2.17 baseline via zig)"
  cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.17
  BIN=target/x86_64-unknown-linux-gnu/release/forge-gtv
else
  echo "== build (this machine's glibc; install cargo-zigbuild + ziglang for a portable binary)"
  cargo build --release
fi

NAME="forge-gtv-setup-${VERSION}-linux-x86_64"
install -m 0755 "$BIN" "$OUT/$NAME"
echo "== needs glibc:"; objdump -T "$OUT/$NAME" 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1 || true

echo "== source + Windows build kit"
SRC="forge-gtv-${VERSION}-source"
rm -rf "$OUT/$SRC" && mkdir -p "$OUT/$SRC"
tar --exclude=./target --exclude=./dist --exclude=./.git -cf - . | tar -xf - -C "$OUT/$SRC"
(cd "$OUT" && zip -qr "${SRC}.zip" "$SRC" && rm -rf "$SRC")

cp LICENSE THIRD-PARTY-NOTICES.md "$OUT/"
(cd "$OUT" && sha256sum "$NAME" "${SRC}.zip" > SHA256SUMS)
ls -la "$OUT"
