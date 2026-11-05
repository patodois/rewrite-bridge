#!/usr/bin/env bash
# Packs the release builds of the three test peers into dist/rewrite-bridge-peers-<version>-<platform>.(tar.gz|zip), next
# to the README, the licenses and an INSTALL.txt. Used by .github/workflows/release.yml, and runnable by hand:
#
#   cargo build --release --features link --example fake_host --example fake_guest --example interop_peer
#   VERSION=v0.3.1 PLATFORM=macos-arm64 BIN_DIR=target/release/examples packaging/package.sh
#
# PLATFORM is macos-arm64, macos-x86_64, linux-x86_64 or windows-x86_64. Optional: OUT_DIR (default dist) and BINS (the
# programs, space separated; default the three peers).
set -euo pipefail

: "${VERSION:?}" "${PLATFORM:?}" "${BIN_DIR:?}"
OUT_DIR="${OUT_DIR:-dist}"
BINS="${BINS:-fake_host fake_guest interop_peer}"
case "$PLATFORM" in
  windows-*) exe=.exe ;;
  *) exe= ;;
esac

name="rewrite-bridge-peers-$VERSION-$PLATFORM"
stage="$(mktemp -d)"
trap 'rm -rf "${stage:?}"' EXIT
root="$stage/$name"
mkdir -p "$root" "$OUT_DIR"
for b in $BINS; do
  cp "$BIN_DIR/$b$exe" "$root/$b$exe"
  chmod +x "$root/$b$exe"
done
cp README.md LICENSE-APACHE LICENSE-MIT "$root/"

{
  echo "rewrite-bridge test peers $VERSION ($PLATFORM)"
  echo
  echo "This archive holds three programs of the rewrite-bridge crate, the README and the two license files. It"
  echo "contains no game data and needs no Rust toolchain."
  echo
  echo "  fake_host     a host: sends a camera and collision, presents the guest's frames with a depth test"
  echo "  fake_guest    a guest: a cube at a fixed place; with --frames it sends colour and depth"
  echo "  interop_peer  host or guest of the scripted session that all four link libraries are tested with"
  echo
  echo "The guide to adding a game says how to use them against your game:"
  echo "https://github.com/patodois/rewrite-bridge/blob/main/docs/INTEGRATING.md"
  echo
  case "$PLATFORM" in
    windows-*)
      echo "Run it"
      echo "------"
      echo "Open PowerShell in this folder. Start a host, then a guest in a second window:"
      echo
      echo "    .\\fake_host$exe --seconds 10"
      echo "    .\\fake_guest$exe --seconds 4 --frames"
      echo
      echo "SmartScreen"
      echo "-----------"
      echo "The programs are not code-signed (that needs a paid certificate). Windows may show"
      echo "\"Windows protected your PC\" the first time. Choose \"More info\", then \"Run anyway\". To avoid"
      echo "the warning, right-click the downloaded .zip, open Properties, tick \"Unblock\" and press OK"
      echo "before you extract it."
      echo
      echo "Check the download"
      echo "------------------"
      echo "Compare the output of this with the line for the archive in SHA256SUMS:"
      echo
      echo "    Get-FileHash -Algorithm SHA256 $name.zip"
      ;;
    *)
      echo "Run it"
      echo "------"
      echo "From a terminal, in this folder. Start a host, then a guest in a second terminal:"
      echo
      echo "    ./fake_host --seconds 10"
      echo "    ./fake_guest --seconds 4 --frames"
      echo
      case "$PLATFORM" in
        macos-*)
          echo "macOS: Gatekeeper"
          echo "-----------------"
          echo "The programs are not signed or notarized (that needs a paid Apple developer account). If the"
          echo "archive came from a browser, macOS marks them as quarantined and refuses to open them"
          echo "(\"Apple could not verify ... is free of malware\"). Remove the mark once:"
          echo
          echo "    xattr -d com.apple.quarantine ./fake_host ./fake_guest ./interop_peer"
          echo
          echo "Or run one once, then open System Settings > Privacy & Security and press \"Open Anyway\"."
          echo "Files downloaded with curl or the gh command line tool are not quarantined and need none of this."
          echo
          ;;
        linux-*)
          echo "Linux"
          echo "-----"
          echo "Built on Ubuntu 22.04 for x86_64, so they need glibc 2.35 or newer (Ubuntu 22.04, Debian 12,"
          echo "Fedora 36 and later). On an older system, build from source (see the README)."
          echo
          ;;
      esac
      echo "Check the download"
      echo "------------------"
      echo "Compare the output of this with the line for the archive in SHA256SUMS:"
      echo
      if [ "${PLATFORM%%-*}" = macos ]; then
        echo "    shasum -a 256 $name.tar.gz"
      else
        echo "    sha256sum $name.tar.gz"
      fi
      ;;
  esac
} > "$root/INSTALL.txt"

case "$PLATFORM" in
  windows-*)
    archive="$name.zip"
    out_abs="$(cd "$OUT_DIR" && pwd)"
    rm -f "${out_abs:?}/${archive:?}"
    if command -v 7z >/dev/null 2>&1; then
      (cd "$stage" && 7z a -tzip -bso0 -bsp0 "$out_abs/$archive" "$name")
    elif command -v zip >/dev/null 2>&1; then
      (cd "$stage" && zip -qr "$out_abs/$archive" "$name")
    else
      echo "no 7z or zip on this machine" >&2
      exit 1
    fi
    ;;
  *)
    archive="$name.tar.gz"
    # COPYFILE_DISABLE keeps macOS from adding ._ files to the archive
    COPYFILE_DISABLE=1 tar -C "$stage" --numeric-owner -czf "$OUT_DIR/$archive" "$name"
    ;;
esac
echo "$OUT_DIR/$archive"
