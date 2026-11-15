#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
# No rolling branches or system Moonlight library: callback layout is part of the build.
while read -r path repo rev hash; do
    case "$path" in ''|'#'*) continue;; esac
    dest="vendor/$path"
    if [ -f "$dest/.source-revision" ] && [ "$(cat "$dest/.source-revision")" = "$rev $hash" ]; then continue; fi
    work=$(mktemp -d "${TMPDIR:-/tmp}/gamestream-source.XXXXXX")
    trap 'rm -rf "$work"' EXIT HUP INT TERM
    curl --fail --location --silent --show-error --proto '=https' "https://codeload.github.com/$repo/tar.gz/$rev" -o "$work/source.tar.gz"
    printf '%s  %s\n' "$hash" "$work/source.tar.gz" | sha256sum -c - >&2
    mkdir "$work/source"
    tar -xzf "$work/source.tar.gz" --strip-components=1 -C "$work/source"
    printf '%s %s\n' "$rev" "$hash" > "$work/source/.source-revision"
    mkdir -p "$(dirname "$dest")"
    rm -rf "$dest"
    mv "$work/source" "$dest"
    rm -rf "$work"
    trap - EXIT HUP INT TERM
done < moonlight.lock
