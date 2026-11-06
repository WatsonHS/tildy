#!/usr/bin/env bash
# Run inside the target architecture's Linux userland, with an isolated HOME.
set -euo pipefail
archive=$(realpath "$1")
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
tar -xzf "$archive" -C "$root"
export HOME="$root/home" ZDOTDIR="$root/home"
export XDG_CONFIG_HOME="$HOME/config" XDG_STATE_HOME="$HOME/state" XDG_CACHE_HOME="$HOME/cache"
export TILDY_CONFIG="$XDG_CONFIG_HOME/tildy/config.json"
mkdir -p "$HOME"
bin="$root/tildy"
uname -m
sha256sum "$archive" "$bin"
"$bin" --version
"$bin" theme --help > "$root/theme-help"
"$bin" configure --help > "$root/configure-help"
cmp "$root/theme-help" "$root/configure-help"
"$bin" config get --effective --json > "$root/effective.json"
grep -q 'pastel-powerline' "$root/effective.json"
"$bin" theme --preset plain --no-add-newline
"$bin" config validate --json
"$bin" preview --scenario git-dirty --json
"$bin" init
cp "$HOME/.zshrc" "$root/first.zshrc"
"$bin" init
cmp "$HOME/.zshrc" "$root/first.zshrc"
echo 'package smoke: PASS'
