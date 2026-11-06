#!/usr/bin/env bash
# Build macOS and Linux tarballs, publish them as a GitHub Release on the tap
# repository, and update the Homebrew formula there.
#
#   scripts/release.sh [--build-only] [path-to-homebrew-tildy-checkout]
#
# The public tap holds the formula and release assets. Source visibility is
# managed separately from binary releases.
# Requires: rustup targets aarch64-apple-darwin and x86_64-apple-darwin, gh
# logged in, Docker for Linux builds, and a clean tap checkout.
# --build-only stages all four archives and tildy.rb without publishing.
set -euo pipefail

cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="$PWD/target"
# Keep developer home/source paths out of panic locations embedded in binaries.
# Cargo treats plain RUSTFLAGS as whitespace-separated; preserve that convention.
if [ -z "${CARGO_ENCODED_RUSTFLAGS+x}" ] && [ -n "${RUSTFLAGS:-}" ]; then
  CARGO_ENCODED_RUSTFLAGS=$(python3 -c 'import os; print(chr(31).join(os.environ["RUSTFLAGS"].split()), end="")')
fi
export CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS:+${CARGO_ENCODED_RUSTFLAGS}$'\x1f'}--remap-path-prefix=$HOME=/home/build"$'\x1f'"--remap-path-prefix=$PWD=/tildy"
# Do not ship macOS AppleDouble/resource-fork metadata with the executable.
export COPYFILE_DISABLE=1
BUILD_ONLY=0
if [ "${1:-}" = --build-only ]; then BUILD_ONLY=1; shift; fi
TAP_DIR=${1:-../homebrew-tildy}
TAP_REPO=${TILDY_TAP_REPO:-WatsonHS/homebrew-tildy}
VERSION=$(grep -m1 '^version = ' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
TAG="v$VERSION"
OUT=target/release-assets/$VERSION
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)

if [ "$BUILD_ONLY" = 0 ]; then
  [ -d "$TAP_DIR/.git" ] || { echo "tap checkout not found at $TAP_DIR (git clone git@github.com:$TAP_REPO.git)" >&2; exit 2; }
  [ -z "$(git status --porcelain)" ] || { echo "source tree is not clean" >&2; exit 2; }
  [ -z "$(git -C "$TAP_DIR" status --porcelain)" ] || { echo "tap tree is not clean" >&2; exit 2; }
  if gh release view "$TAG" --repo "$TAP_REPO" >/dev/null 2>&1; then
    echo "release $TAG already exists; use a new version (published assets are immutable)" >&2
    exit 2
  fi
fi

rm -rf "$OUT"; mkdir -p "$OUT"
cp LICENSE NOTICE "$OUT/"
cp -R licenses "$OUT/licenses"
python3 scripts/collect-licenses.py "$OUT/licenses/dependencies"
sha_for() { shasum -a 256 "$OUT/tildy-$VERSION-$1.tar.gz" | cut -d' ' -f1; }
for t in "${TARGETS[@]}"; do
  echo "== build $t"
  cargo build --locked --release --target "$t"
  asset="tildy-$VERSION-$t.tar.gz"
  python3 scripts/package-archive.py "$OUT/$asset" "target/$t/release/tildy" "$OUT"
  echo "   $asset $(sha_for "$t")"
done
TILDY_LICENSE_DIR="$PWD/$OUT" bash scripts/package-linux.sh "$OUT"

base="https://github.com/$TAP_REPO/releases/download/$TAG"
cat > "$OUT/tildy.rb" <<EOF
class Tildy < Formula
  desc "Async, agent-friendly Zsh prompt"
  homepage "https://github.com/$TAP_REPO"
  version "$VERSION"
  license "MIT"

  on_macos do
    on_arm do
      url "$base/tildy-$VERSION-aarch64-apple-darwin.tar.gz"
      sha256 "$(sha_for aarch64-apple-darwin)"
    end
    on_intel do
      url "$base/tildy-$VERSION-x86_64-apple-darwin.tar.gz"
      sha256 "$(sha_for x86_64-apple-darwin)"
    end
  end

  on_linux do
    on_arm do
      url "$base/tildy-$VERSION-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "$(sha_for aarch64-unknown-linux-gnu)"
    end
    on_intel do
      url "$base/tildy-$VERSION-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "$(sha_for x86_64-unknown-linux-gnu)"
    end
  end

  def install
    if OS.linux? && OS::Linux::Glibc.system_version < Version.new("2.39")
      odie "Tildy requires system glibc 2.39 or newer (for example, Ubuntu 24.04)."
    end
    bin.install "tildy"
    doc.install "LICENSE", "NOTICE", "licenses"
  end

  def caveats
    <<~EOS
      Enable Tildy in Zsh (backs up .zshrc; respects ZDOTDIR):
        tildy init
      Then open a new terminal.

      Choose a theme, colors, layout and blank lines (optional):
        tildy theme

      Inspect or manage the same config (advanced use and scripts):
        tildy config --help

      Preview shell setup without writing: tildy init --dry-run
      Default: pastel-powerline, two rows, with a blank line before the prompt.
      A Nerd Font is needed for this preset. Use tildy theme to choose another.
      UI language: English by default; export TILDY_LANG=zh for Chinese,
      or TILDY_LANG=auto to follow the system locale.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tildy --version")
    theme_help = shell_output("#{bin}/tildy theme --help")
    assert_match "Usage: tildy theme", theme_help
    assert_equal theme_help, shell_output("#{bin}/tildy configure --help")
    ENV["TILDY_CONFIG"] = (testpath/"config.json").to_s
    ENV["XDG_STATE_HOME"] = (testpath/"state").to_s
    effective = JSON.parse(shell_output("#{bin}/tildy config get --effective --json"))["data"]["config"]
    assert_equal "pastel-powerline", effective["preset"]
    assert_equal true, effective["add_newline"]
    assert_equal 2, effective["layout"]["rows"].length
    system bin/"tildy", "theme", "--preset", "plain", "--add-newline"
    assert_equal true, JSON.parse((testpath/"config.json").read)["add_newline"]
    system bin/"tildy", "theme", "--preset", "plain", "--no-add-newline"
    assert_equal false, JSON.parse((testpath/"config.json").read)["add_newline"]
    ENV["ZDOTDIR"] = testpath.to_s
    system bin/"tildy", "init"
    initial = (testpath/".zshrc").read
    assert_match "# >>> tildy instant >>>", initial
    assert_match "# >>> tildy prompt >>>", initial
    system bin/"tildy", "init"
    assert_equal initial, (testpath/".zshrc").read
  end
end
EOF

if [ "$BUILD_ONLY" = 1 ]; then
  echo "built locally: $OUT (not published)"
  exit 0
fi
echo "== release $TAG on $TAP_REPO"
gh release create "$TAG" --repo "$TAP_REPO" --draft --title "tildy $VERSION" \
  --notes "macOS and Linux ARM64/x86_64 binaries. Linux requires system glibc 2.39 or newer (for example, Ubuntu 24.04). Built from source commit $(git rev-parse HEAD)." "$OUT"/*.tar.gz
gh release edit "$TAG" --repo "$TAP_REPO" --draft=false
cp "$OUT/tildy.rb" "$TAP_DIR/Formula/tildy.rb"
echo "== push formula"
git -C "$TAP_DIR" add Formula/tildy.rb
git -C "$TAP_DIR" commit -q -m "tildy $VERSION"
git -C "$TAP_DIR" push -q
echo "done: brew tap ${TAP_REPO%/homebrew-*}/tildy && brew trust ${TAP_REPO%/homebrew-*}/tildy && brew install tildy"
