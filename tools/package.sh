#!/bin/sh
# Packages the already-built executable and its complete, offline-buildable corresponding source.
set -eu
cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
exe=target/aarch64-unknown-linux-gnu/release/lumagame-gamestream
python3 tools/check-elf.py "$exe"
work=$(mktemp -d "${TMPDIR:-/tmp}/gamestream-package.XXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM
mkdir -p "$work/binary/gamestream" "$work/source/.cargo" dist
cp "$exe" engine.json LICENSE NOTICE LUMATV-LICENSE "$work/binary/gamestream/"
cat > "$work/binary/gamestream/SOURCE.txt" <<EOF
LumaGame GameStream $version — GPL-3.0-only, without warranty.
Source: https://github.com/nphil/lumagame-gamestream/tree/v$version
Complete corresponding source (including dependencies):
https://github.com/nphil/lumagame-gamestream/releases/download/v$version/lumagame-gamestream-v$version-source.tar.gz
Mirror: https://git.nateshome.net/nphilip89/lumagame-gamestream

Build: Rust, a native C compiler, Zig, cargo-zigbuild, Perl and Make are required.
Host build: sh tools/build.sh --host
Host tests: sh tools/build.sh --test
webOS arm64 build (glibc 2.35): sh tools/build.sh
The complete source archive vendors C and Rust dependencies for offline builds.
EOF
git archive HEAD | tar -xf - -C "$work/source"
sh tools/fetch-moonlight.sh
mkdir -p "$work/source/vendor"
cp -R vendor/moonlight-common-c "$work/source/vendor/"
(cd "$work/source" && cargo vendor --locked --versioned-dirs cargo-vendor > .cargo/config.toml)
cp "$work/binary/gamestream/SOURCE.txt" "$work/source/"
find "$work/source/cargo-vendor" "$work/source/vendor" -type f \( -iname 'license*' -o -iname 'copying*' -o -iname 'notice*' -o -iname 'copyright*' \) | while IFS= read -r file; do
    relative=${file#"$work/source/"}
    mkdir -p "$work/binary/gamestream/licenses/$(dirname "$relative")"
    cp "$file" "$work/binary/gamestream/licenses/$relative"
done
for kind in binary source; do
    name="lumagame-gamestream-v$version-source.tar.gz"
    if [ "$kind" = binary ]; then name="lumagame-gamestream-v$version-aarch64.tar.gz"; fi
    tar --sort=name --mtime='UTC 2026-01-01' --owner=0 --group=0 --numeric-owner -C "$work/$kind" -cf - . | gzip -n > "dist/$name"
done
(cd dist && sha256sum "lumagame-gamestream-v$version-aarch64.tar.gz" "lumagame-gamestream-v$version-source.tar.gz")
