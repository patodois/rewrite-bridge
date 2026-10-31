#!/usr/bin/env bash
# Runs the scripted interop session (tests/interop/README.md) for every host/guest pair of the four implementations,
# in three modes: without a token, with a token handed over in an endpoint file, and with a wrong token.
#
#   tests/interop/run.sh [--build] [--modes plain,token,wrong-token] [HOST_IMPL:GUEST_IMPL ...]
#
# impls: rust java c cs; default: all 16 pairs, all three modes (48 runs).
# Where the peers are: --build builds them; or set RBL_PEER_RUST / RBL_PEER_JAVA / RBL_PEER_C / RBL_PEER_CS to the
# command that starts one (no spaces in paths). Defaults assume link-java, link-c and link-cs next to this
# repository (override with RBL_LINK_JAVA_DIR, RBL_LINK_C_DIR, RBL_LINK_CS_DIR). RBL_LOG_DIR keeps the logs
# (default: a new temporary directory), RBL_BASE_PORT is the first control port of the plain mode (47810).
set -u
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
JAVA_DIR="${RBL_LINK_JAVA_DIR:-$root/../link-java}"
C_DIR="${RBL_LINK_C_DIR:-$root/../link-c}"
CS_DIR="${RBL_LINK_CS_DIR:-$root/../link-cs}"
LOG_DIR="${RBL_LOG_DIR:-$(mktemp -d)}"
PORT="${RBL_BASE_PORT:-47810}"
mkdir -p "$LOG_DIR"
# The endpoint files hold tokens, so they do not go into LOG_DIR (which CI uploads); a private directory of their own.
EP_DIR="$(mktemp -d)"
chmod 700 "$EP_DIR"
trap 'rm -rf "$EP_DIR"' EXIT

build=0
modes="plain,token,wrong-token"
while [ $# -gt 0 ]; do
  case "$1" in
    --build) build=1; shift ;;
    --modes) modes="$2"; shift 2 ;;
    *) break ;;
  esac
done

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

# What the wrong-token guest sends (README.md, "Modes"): the file's token with its last character replaced.
wrong_token() {
  local t="$1" last="${1: -1}"
  if [ "$last" = "0" ]; then echo "${t%?}1"; else echo "${t%?}0"; fi
}

runs=0
failed=0
IFS=',' read -r -a mode_list <<< "$modes"
for mode in "${mode_list[@]}"; do
  case "$mode" in plain|token|wrong-token) ;; *) echo "unknown mode $mode" >&2; exit 2 ;; esac
  echo "== mode $mode"
  for pair in "${pairs[@]}"; do
    h="${pair%%:*}"; g="${pair##*:}"
    tag="$mode.$h-host+$g-guest"
    hl="$LOG_DIR/$tag.host.log"; gl="$LOG_DIR/$tag.guest.log"
    read -r -a hcmd <<< "$(peer_cmd "$h")" || exit 3
    read -r -a gcmd <<< "$(peer_cmd "$g")" || exit 3
    problems=""
    token=""
    ep="$EP_DIR/$tag.json"
    if [ "$mode" = "plain" ]; then
      port=$PORT; PORT=$((PORT + 1))
      "${hcmd[@]}" host --port "$port" --label "$h" > "$hl" 2>&1 &
      hpid=$!
      sleep 1
      "${gcmd[@]}" guest --port "$port" --label "$g" > "$gl" 2>&1 &
      gpid=$!
    else
      # The host binds any free port and writes the endpoint file after binding; the guest finds port and token there.
      hextra=(); gextra=()
      if [ "$mode" = "wrong-token" ]; then hextra=(--expect-unauthorized); gextra=(--wrong-token); fi
      "${hcmd[@]}" host --port 0 --label "$h" --endpoint "$ep" ${hextra[@]+"${hextra[@]}"} > "$hl" 2>&1 &
      hpid=$!
      for _ in $(seq 1 200); do [ -f "$ep" ] && break; sleep 0.05; done
      if [ -f "$ep" ]; then
        token=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["token"])' "$ep" 2>/dev/null)
        perm=$(python3 -c 'import os,stat,sys; print(oct(stat.S_IMODE(os.stat(sys.argv[1]).st_mode)))' "$ep" 2>/dev/null)
        [ "$perm" = "0o600" ] || problems+="endpoint file has mode $perm, expected 0o600"$'\n'
        [ -n "$token" ] || problems+="endpoint file has no token"$'\n'
      else
        problems+="the host wrote no endpoint file within 10 s"$'\n'
      fi
      "${gcmd[@]}" guest --port 0 --label "$g" --endpoint "$ep" ${gextra[@]+"${gextra[@]}"} > "$gl" 2>&1 &
      gpid=$!
    fi
    wait "$gpid"; grc=$?
    wait "$hpid"; hrc=$?
    if [ "$mode" != "plain" ]; then
      [ -e "$ep" ] && problems+="the host left its endpoint file behind"$'\n'
      if [ -n "$token" ]; then
        # The token is a secret: no log line of either peer may contain it, nor the wrong one the guest sent.
        for secret in "$token" "$(wrong_token "$token")"; do
          if grep -qF "$secret" "$hl" "$gl"; then problems+="a peer's log contains a token"$'\n'; fi
        done
      fi
      rm -f "$EP_DIR/$tag.json"
    fi
    runs=$((runs + 1))
    out=$(python3 "$here/check_results.py" --mode "$mode" --host-label "$h" --guest-label "$g" --host-exit "$hrc" --guest-exit "$grc" "$hl" "$gl")
    crc=$?
    if [ $crc -eq 0 ] && [ -z "$problems" ]; then
      echo "ok    $mode  $h host <-> $g guest"
    else
      echo "FAIL  $mode  $h host <-> $g guest"
      printf '%s\n%s' "$problems" "$out" | sed '/^$/d; s/^/        /'
      failed=$((failed + 1))
    fi
  done
done

echo
echo "$runs runs (${#pairs[@]} pairs x ${#mode_list[@]} modes), $failed failed (logs in $LOG_DIR)"
[ $failed -eq 0 ]
