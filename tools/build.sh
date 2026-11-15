#!/bin/sh
# Host: tools/build.sh --host. TV: tools/build.sh. Requires Rust, a native C compiler, Zig, cargo-zigbuild, Perl, make, curl and tar.
set -eu
cd "$(dirname "$0")/.."
here=$PWD
if ! command -v make >/dev/null 2>&1 && [ -d "$HOME/toolchains/bin" ]; then PATH="$PATH:$HOME/toolchains/bin"; export PATH; fi
export CARGO_TARGET_DIR="$here/target"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="${HOST_CC:-cc}"
export CC_x86_64_unknown_linux_gnu="${HOST_CC:-cc}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
case "${1:-tv}" in
    --host) cargo build --locked --release ;;
    --test) cargo test --locked; cargo clippy --locked --all-targets -- -D warnings ;;
    tv) cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu.2.35 ;;
    *) echo 'usage: tools/build.sh [--host|--test]' >&2; exit 2 ;;
esac
