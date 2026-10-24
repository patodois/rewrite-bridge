#!/usr/bin/env python3
"""Judges the two result lines of one interop pair (see README.md).

    check_results.py --host-label rust --guest-label java HOST_LOG GUEST_LOG [--host-exit N] [--guest-exit N]

Prints one line per problem; exit status 0 if there is none.
"""
import argparse
import json
import sys

MARK = "RBL-INTEROP-RESULT "
ROUNDS = 5


def last_result(path):
    found = None
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            for line in f:
                if line.startswith(MARK):
                    found = line[len(MARK):]
    except OSError as e:
        return None, "cannot read %s: %s" % (path, e)
    if found is None:
        return None, "%s has no %s line" % (path, MARK.strip())
    try:
        return json.loads(found), None
    except ValueError as e:
        return None, "%s: result line is not JSON: %s" % (path, e)


def expect(problems, who, r, key, want):
    if r.get(key) != want:
        problems.append("%s: %s is %r, expected %r" % (who, key, r.get(key), want))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host-label", required=True)
    ap.add_argument("--guest-label", required=True)
    ap.add_argument("--host-exit", type=int, default=0)
    ap.add_argument("--guest-exit", type=int, default=0)
    ap.add_argument("host_log")
    ap.add_argument("guest_log")
    a = ap.parse_args()

    problems = []
    host, err = last_result(a.host_log)
    if err:
        problems.append("host: " + err)
    guest, err = last_result(a.guest_log)
    if err:
        problems.append("guest: " + err)
    if a.host_exit != 0:
        problems.append("host exited with %d" % a.host_exit)
    if a.guest_exit != 0:
        problems.append("guest exited with %d" % a.guest_exit)
    if host:
        for f in host.get("failures", []):
            problems.append("host reports: " + f)
        expect(problems, "host", host, "role", "host")
        expect(problems, "host", host, "ok", True)
        expect(problems, "host", host, "protocol", 1)
        expect(problems, "host", host, "peer_game", "interop guest (%s)" % a.guest_label)
        expect(problems, "host", host, "frames_ok", ROUNDS)
        expect(problems, "host", host, "frames_bad", 0)
        expect(problems, "host", host, "poses", ROUNDS)
        expect(problems, "host", host, "spawns", [7])
        expect(problems, "host", host, "despawns", [7])
        expect(problems, "host", host, "focus", ["host:request"])
        expect(problems, "host", host, "cameras", 0)
        expect(problems, "host", host, "collision_triangles", 0)
        if "bye" not in (host.get("lost_reason") or ""):
            problems.append("host: lost_reason %r does not mention bye" % host.get("lost_reason"))
    if guest:
        for f in guest.get("failures", []):
            problems.append("guest reports: " + f)
        expect(problems, "guest", guest, "role", "guest")
        expect(problems, "guest", guest, "ok", True)
        expect(problems, "guest", guest, "protocol", 1)
        expect(problems, "guest", guest, "peer_game", "interop host (%s)" % a.host_label)
        expect(problems, "guest", guest, "cameras", ROUNDS)
        expect(problems, "guest", guest, "poses", ROUNDS)
        expect(problems, "guest", guest, "spawns", [3])
        expect(problems, "guest", guest, "despawns", [3])
        expect(problems, "guest", guest, "focus", ["guest:set", "host:set"])
        expect(problems, "guest", guest, "collision_triangles", 300)
        expect(problems, "guest", guest, "frames_ok", 0)
        expect(problems, "guest", guest, "lost_reason", None)
    if host and guest:
        hs, gs = host.get("session"), guest.get("session")
        if hs != gs:
            problems.append("sessions differ: host %r, guest %r" % (hs, gs))
        if not (isinstance(hs, int) and not isinstance(hs, bool) and 1 <= hs <= 0xFFFFFFFF):
            problems.append("session %r is not a nonzero u32 number" % (hs,))
    for who, r in (("host", host), ("guest", guest)):
        stats = r.get("stats") if r else None
        if stats:
            for k, v in sorted(stats.items()):
                # One datagram may legitimately be counted foreign by the guest: the host sends a Heartbeat right
                # after its hello line, on another socket, so the guest can read that datagram before it has read
                # the line, while it has no session yet (seen as state=Handshake, kind 1, seq 0 on the C# guest
                # against the C host; every library has this window). The guest sends nothing before the host's
                # hello, so the host's count stays exactly 0, as does every other counter.
                allowed = 1 if (who == "guest" and k == "foreign") else 0
                if v > allowed:
                    problems.append("%s: stats.%s is %r, expected at most %d" % (who, k, v, allowed))
    for p in problems:
        print(p)
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
