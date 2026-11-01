#!/usr/bin/env python3
"""Builds the initial fuzz corpora under fuzz/corpus/ from the repository's own synthetic test vectors
(tests/vectors/link) and a few hand-built handshake sessions. Nothing here comes from a game."""
import json, os, struct, sys

root = os.path.dirname(os.path.abspath(__file__))
vec = os.path.join(root, "..", "tests", "vectors", "link")
corpus = os.path.join(root, "corpus")


def put(target, name, data):
    d = os.path.join(corpus, target)
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, name), "wb") as f:
        f.write(data)


def cases(name):
    with open(os.path.join(vec, name)) as f:
        return json.load(f)["cases"]


for c in cases("datagrams.json"):
    if "hex" in c:
        put("decode_datagram", c["id"], bytes.fromhex(c["hex"]))
for c in cases("control.json"):
    if "line" in c:
        put("control_parse", c["id"], c["line"].encode())
for c in cases("endpoints.json"):
    if "text" in c:
        put("endpoint_parse", c["id"], c["text"].encode())

pf = os.path.join(vec, "pixelfiles")
for fn in sorted(os.listdir(pf)):
    data = open(os.path.join(pf, fn), "rb").read()
    if len(data) > 4096 or len(data) < 24:
        continue
    w, h = struct.unpack_from("<II", data, 16)
    for frame, slot in ((1, 0), (1, 1), (2, 2), (3, 1)):
        ready = struct.pack("<QIIII", frame, slot, 7, w, h)
        put("frame_reader", f"{fn}.{frame}.{slot}", ready + data)

# handshake sessions: flags, then (op, len16, payload)*
def op(o, p=b""):
    return bytes([o]) + struct.pack("<H", len(p)) + p

def line(s):
    return op(0, s.encode() + b"\n")

host_flags, guest_flags = 0, 1
put("handshake", "host_hello_ok", bytes([0]) + op(3, b"\0") + op(4))
put("handshake", "host_token_ok", bytes([2]) + op(3, b"\1") + op(4) + op(2, bytes(24)))
put("handshake", "host_token_none", bytes([2]) + op(3, b"\0") + op(4))
put("handshake", "host_token_wrong", bytes([2]) + op(3, b"\2") + op(4))
put("handshake", "host_token_short", bytes([2]) + op(3, b"\3") + op(4))
put("handshake", "host_garbage", bytes([0]) + op(0, b"\xff\xfe\n") + op(4))
put("handshake", "guest_hello_ok", bytes([1]) + op(3, b"\0" + struct.pack("<I", 77)) + op(4))
spawn = '{"type":"spawn","entity":7,"provider":"p","asset":"a","name":"n"}'
focus = '{"type":"input","focus":"guest","reason":"r"}'
chan = '{"type":"frame_channel","path":"/tmp/x.bin","width":4,"height":2,"slots":3}'
heartbeat = bytes.fromhex("52424c4b010001000100cdab0400000000401e18240a0600")
pose_hdr = bytes.fromhex("52424c4b010003000100cdab0400000000401e18240a0600") + struct.pack("<II", 1, 0) + struct.pack("<IIQ", 7, 1, 5) + bytes(64)
pose_hdr = pose_hdr[:24] + struct.pack("<II", 1, 0) + struct.pack("<II", 7, 1) + struct.pack("<Q", 5) + bytes(64)
put(
    "handshake",
    "host_session",
    bytes([0]) + op(3, b"\0") + op(4) + line(spawn) + line(focus) + line(chan) + op(2, pose_hdr) + op(2, heartbeat)
    + line('{"type":"despawn","entity":7}') + line('{"type":"bye","reason":"x"}'),
)
put(
    "handshake",
    "guest_session",
    bytes([1]) + op(3, b"\0" + struct.pack("<I", 9)) + op(4) + line(spawn) + line(focus) + line(chan) + op(2, heartbeat),
)
print("corpora written to", corpus)
