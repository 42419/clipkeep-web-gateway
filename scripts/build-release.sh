#!/usr/bin/env bash
# 编译 x86_64 / aarch64 两个 musl 静态二进制，产物落到 dist/。
#
# 只为 CI 服务：x86_64 用系统 musl-tools，aarch64 用 musl.cc 交叉工具链。
# 本地不需要跑（本地多半编不了 ring，见 README 的说明）。

set -euo pipefail

DIST="${DIST:-dist}"
# CI 里可能把 target 目录指到别处（CNB 设了 CARGO_TARGET_DIR=/target）。
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
mkdir -p "$DIST"

# 用户要的命名：clipkeep-gateway-<arch>-linux（musl 静态，README 里说明）
build_target() {
  local target="$1" arch="$2"
  echo "==> building $target"
  cargo build --release --locked --target "$target"
  cp "$TARGET_DIR/$target/release/clipkeep-gateway" "$DIST/clipkeep-gateway-$arch-linux"
  chmod +x "$DIST/clipkeep-gateway-$arch-linux"
}

rustup target add x86_64-unknown-linux-musl
rustup target add aarch64-unknown-linux-musl

build_target x86_64-unknown-linux-musl x86_64

# aarch64 需要独立的 musl 交叉工具链，Debian 仓库里没有现成的。
TOOLCHAIN=/opt/aarch64-linux-musl-cross
if [[ ! -d "$TOOLCHAIN" ]]; then
  echo "==> fetching aarch64 musl cross toolchain"
  curl -fsSL --retry 3 --max-time 300 \
    -o /tmp/aarch64-linux-musl-cross.tgz \
    https://musl.cc/aarch64-linux-musl-cross.tgz
  tar -xzf /tmp/aarch64-linux-musl-cross.tgz -C /opt
fi
export PATH="$TOOLCHAIN/bin:$PATH"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc
export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
export AR_aarch64_unknown_linux_musl=aarch64-linux-musl-ar

build_target aarch64-unknown-linux-musl aarch64

cd "$DIST"
sha256sum ./* > SHA256SUMS
ls -lh
cat SHA256SUMS
