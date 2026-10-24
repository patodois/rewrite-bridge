#!/usr/bin/env bash
# Runs the scripted interop session (tests/interop/README.md) for every host/guest pair of the four implementations.
#
#   tests/interop/run.sh [--build] [HOST_IMPL:GUEST_IMPL ...]        impls: rust java c cs; default: all 16 pairs
#
# Where the peers are: --build builds them; or set RBL_PEER_RUST / RBL_PEER_JAVA / RBL_PEER_C / RBL_PEER_CS to the
# command that starts one (no spaces in paths). Defaults assume link-java, link-c and link-cs next to this
# repository (override with RBL_LINK_JAVA_DIR, RBL_LINK_C_DIR, RBL_LINK_CS_DIR). RBL_LOG_DIR keeps the logs
# (default: a new temporary directory), RBL_BASE_PORT is the first control port (47810).
set -u
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
JAVA_DIR="${RBL_LINK_JAVA_DIR:-$root/../link-java}"
C_DIR="${RBL_LINK_C_DIR:-$root/../link-c}"
CS_DIR="${RBL_LINK_CS_DIR:-$root/../link-cs}"
LOG_DIR="${RBL_LOG_DIR:-$(mktemp -d)}"
PORT="${RBL_BASE_PORT:-47810}"
mkdir -p "$LOG_DIR"

build=0
if [ "${1:-}" = "--build" ]; then build=1; shift; fi

if [ $build -eq 1 ]; then
  echo "== building the peers"
  (cd "$root" && cargo build --release --features link --example interop_peer) || exit 3
  (cd "$JAVA_DIR" && ./gradlew --no-daemon -q testClasses) || exit 3
  (cd "$C_DIR" && make build/interop_peer) || exit 3
  (cd "$CS_DIR" && dotnet build RewriteLink.sln -c Release -v q) || exit 3
fi

peer_cmd() { # impl -> command words on stdout
  case "$1" in
    rust) echo "${RBL_PEER_RUST:-$root/target/release/examples/interop_peer}" ;;
    java) echo "${RBL_PEER_JAVA:-java -cp $JAVA_DIR/build/classes/java/main:$JAVA_DIR/build/classes/java/test dev.patodois.rewritelink.InteropPeer}" ;;
    c)    echo "${RBL_PEER_C:-$C_DIR/build/interop_peer}" ;;
    cs)   echo "${RBL_PEER_CS:-dotnet $CS_DIR/tests/RewriteLink.InteropPeer/bin/Release/net8.0/InteropPeer.dll}" ;;
    *)    echo "unknown implementation $1" >&2; return 1 ;;
  esac
}

pairs=("$@")
if [ ${#pairs[@]} -eq 0 ]; then
  for h in rust java c cs; do for g in rust java c cs; do pairs+=("$h:$g"); done; done
fi

failed=0
declare -a summary
for pair in "${pairs[@]}"; do
  h="${pair%%:*}"; g="${pair##*:}"
  hl="$LOG_DIR/$h-host+$g-guest.host.log"; gl="$LOG_DIR/$h-host+$g-guest.guest.log"
  port=$PORT; PORT=$((PORT + 1))
  read -r -a hcmd <<< "$(peer_cmd "$h")" || exit 3
  read -r -a gcmd <<< "$(peer_cmd "$g")" || exit 3
  "${hcmd[@]}" host --port "$port" --label "$h" > "$hl" 2>&1 &
  hpid=$!
  sleep 1
  "${gcmd[@]}" guest --port "$port" --label "$g" > "$gl" 2>&1 &
  gpid=$!
  wait "$gpid"; grc=$?
  wait "$hpid"; hrc=$?
  if out=$(python3 "$here/check_results.py" --host-label "$h" --guest-label "$g" --host-exit "$hrc" --guest-exit "$grc" "$hl" "$gl"); then
    echo "ok    $h host <-> $g guest"
    summary+=("$h:$g ok")
  else
    echo "FAIL  $h host <-> $g guest"
    echo "$out" | sed 's/^/        /'
    summary+=("$h:$g FAIL")
    failed=$((failed + 1))
  fi
done

echo
echo "${#pairs[@]} pairs, $failed failed (logs in $LOG_DIR)"
[ $failed -eq 0 ]
