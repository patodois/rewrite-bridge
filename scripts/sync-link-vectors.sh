#!/usr/bin/env bash
# Copies the link protocol test vectors (tests/vectors/link, without gen.py) into another repository, or checks
# that the copy there is identical.
#
#   scripts/sync-link-vectors.sh REPO_DIR            copy, make sure .gitattributes keeps the files byte for byte,
#                                                    print the digest to pin in the repository's test runner
#   scripts/sync-link-vectors.sh --check REPO_DIR    exit 1 if REPO_DIR/tests/vectors/link differs
#
# The digest is the SHA-256 of the SHA256SUMS file: pin it as VECTORS_DIGEST in the runner, so the vectors can only
# change together with a deliberate edit of that constant.
set -eu
check=0
if [ "${1:-}" = "--check" ]; then check=1; shift; fi
dest_repo="${1:?usage: sync-link-vectors.sh [--check] REPO_DIR}"
src="$(cd "$(dirname "$0")/.." && pwd)/tests/vectors/link"
dest="$dest_repo/tests/vectors/link"

sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

# the files that belong to the vectors: exactly the ones SHA256SUMS lists, plus SHA256SUMS
files=$(awk '{print $2}' "$src/SHA256SUMS"; echo SHA256SUMS)

if [ $check -eq 1 ]; then
  bad=0
  for f in $files; do
    if ! cmp -s "$src/$f" "$dest/$f"; then echo "differs or missing: $dest/$f"; bad=1; fi
  done
  if [ -d "$dest" ]; then
    (cd "$dest" && find . -type f | sed 's|^\./||' | sort) > /tmp/rbl-have.$$
    echo "$files" | sort > /tmp/rbl-want.$$
    if ! diff -q /tmp/rbl-want.$$ /tmp/rbl-have.$$ >/dev/null; then
      echo "different file set in $dest:"; diff /tmp/rbl-want.$$ /tmp/rbl-have.$$ || true; bad=1
    fi
    rm -f /tmp/rbl-have.$$ /tmp/rbl-want.$$
  fi
  [ $bad -eq 0 ] && echo "vectors in $dest_repo are identical"
  exit $bad
fi

rm -rf "$dest"
mkdir -p "$dest"
for f in $files; do
  mkdir -p "$dest/$(dirname "$f")"
  cp "$src/$f" "$dest/$f"
done
attr="$dest_repo/.gitattributes"
line='tests/vectors/link/** -text'
if ! { [ -f "$attr" ] && grep -qxF "$line" "$attr"; }; then
  echo "$line" >> "$attr"
  echo "added to $attr: $line"
fi
echo "copied $(echo "$files" | wc -l | tr -d ' ') files to $dest"
echo "VECTORS_DIGEST = $(sha "$src/SHA256SUMS" | awk '{print $1}')"
