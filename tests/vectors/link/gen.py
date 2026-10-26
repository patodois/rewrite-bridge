#!/usr/bin/env python3
"""Generates the link protocol test vectors (everything in this directory except this file and README.md).

Every byte comes from docs/CONTRACT.md (protocol v1, revision 5) through the independent encoder below, which
uses only the standard library. No implementation of the link (Rust, Java, C, C#) is involved, so a vector
cannot have inherited a bug from one of them. The implementations are the consumers.

    python3 tests/vectors/link/gen.py            # (re)write the vectors
    python3 tests/vectors/link/gen.py --check    # fail if the committed vectors are not what this script writes

The output is deterministic: no clock, no randomness. SHA256SUMS lists the vector files; consumer repositories
copy them with scripts/sync-link-vectors.sh and pin the digest of SHA256SUMS in their test runner.
"""
import hashlib
import json
import os
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
CONTRACT = "v1 revision 5"
INF = float("inf")

# ---------------------------------------------------------------------------------------------
# Constants of the contract (section numbers in the comments)

MAGIC = 0x4B4C4252            # 9.1
HEADER_LEN = 24               # 9.1
MAX_DATAGRAM = 8192           # 2
CAMERA_LEN = 152              # 9.3
POSE_LEN = 80                 # 9.4
MAX_POSES = 102               # 9.4
TRIANGLE_LEN = 36             # 9.5
MAX_TRIANGLES = 226           # 9.5
KIND = {"heartbeat": 1, "camera": 2, "pose": 3, "collision": 4, "frame": 5}


def u32(v):
    return struct.pack("<I", v)


def f32(v):
    return struct.pack("<f", v)


def fl(vs):
    return b"".join(f32(v) for v in vs)


def header(kind, session=0xABCD0001, seq=4, time_us=1_700_000_000_000_000, version=1, flags=0, magic=MAGIC):
    return struct.pack("<IHBBIIQ", magic, version, kind, flags, session, seq, time_us)


def hdr_dict(session=0xABCD0001, seq=4, time_us=1_700_000_000_000_000, version=1, flags=0):
    return {"version": version, "flags": flags, "session": session, "seq": seq, "time_us": str(time_us)}


# ---------------------------------------------------------------------------------------------
# Datagram bodies. The JSON form: u64 as decimal strings, f32 as numbers that are exact in binary32,
# +inf as the string "inf".

def jf(v):
    return "inf" if v == INF else v


def pose_record(entity, flags, time_us, t, r, s, v, w):
    return {"entity": entity, "flags": flags, "time_us": str(time_us), "translation": t, "rotation": r,
            "scale": s, "velocity": v, "angular_velocity": w}


def encode_pose(p):
    return (u32(p["entity"]) + u32(p["flags"]) + struct.pack("<Q", int(p["time_us"])) + fl(p["translation"])
            + fl(p["rotation"]) + fl(p["scale"]) + fl(p["velocity"]) + fl(p["angular_velocity"]))


def encode_body(body, reserved=0):
    k = body["kind"]
    if k == "heartbeat":
        return b""
    if k == "camera":
        far = INF if body["far"] == "inf" else body["far"]
        return (fl(body["view"]) + fl(body["proj"]) + fl([body["fov_y"], body["near"], far])
                + u32(body["viewport_width"]) + u32(body["viewport_height"]) + u32(body["depth_range"]))
    if k == "pose":
        return u32(len(body["poses"])) + u32(reserved) + b"".join(encode_pose(p) for p in body["poses"])
    if k == "collision":
        tris = body["triangles"]
        return (u32(body["query"]) + struct.pack("<HH", body["chunk_index"], body["chunk_count"])
                + fl(body["center"]) + f32(body["radius"]) + u32(len(tris)) + u32(reserved)
                + b"".join(fl(t) for t in tris))
    if k == "frame":
        return (struct.pack("<Q", int(body["frame"])) + u32(body["slot"]) + u32(body["camera_seq"])
                + u32(body["width"]) + u32(body["height"]))
    raise ValueError(k)


def datagram(body, **hdr):
    return header(KIND[body["kind"]], **hdr) + encode_body(body)


def camera_body(depth_range=0, far=256.0):
    view = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, -1.5, -2.0, -3.25, 1.0]
    proj = [1.5, 0.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 0.0, -1.0, -1.0, 0.0, 0.0, -0.25, 0.0]
    return {"kind": "camera", "view": view, "proj": proj, "fov_y": 1.0, "near": 0.125, "far": jf(far),
            "viewport_width": 1280, "viewport_height": 720, "depth_range": depth_range}


def pose_at(i, flags=0):
    return pose_record(1000 + i, flags, 1_700_000_000_000_000 + i, [0.5 * i, 1.0, -2.0], [0.0, 0.5, 0.0, 0.5],
                       [1.0, 1.0, 2.0], [0.25, 0.0, -0.25], [0.0, 1.5, 0.0])


def tri_at(i):
    b = float(i % 64)
    return [b, 0.0, 0.0, b + 1.0, 0.0, 0.0, b, 0.0, 1.0]


def collision_body(query=7, index=0, count=1, tris=3):
    return {"kind": "collision", "query": query, "chunk_index": index, "chunk_count": count,
            "center": [1.0, 2.0, 3.0], "radius": 64.0, "triangles": [tri_at(i) for i in range(tris)]}


def frame_body(frame=3, slot=0, camera_seq=4, width=320, height=240):
    return {"kind": "frame", "frame": str(frame), "slot": slot, "camera_seq": camera_seq, "width": width,
            "height": height}


# ---------------------------------------------------------------------------------------------
# datagrams.json

def datagram_cases():
    cases = []

    def valid(id_, section, body, hdr=None, encode=True, note=None, basis="MUST", raw=None, hdr_expect=None):
        hdr = hdr or {}
        data = raw if raw is not None else datagram(body, **hdr)
        c = {"id": id_, "section": section, "expect": "decode", "hex": data.hex(),
             "header": hdr_expect if hdr_expect is not None else hdr_dict(**hdr), "body": body, "encode": encode}
        if note:
            c["note"] = note
        cases.append(c)
        return data

    def invalid(id_, section, data, why):
        cases.append({"id": id_, "section": section, "expect": "reject", "hex": data.hex(), "why": why})

    def ignored(id_, section, data, kind, why):
        cases.append({"id": id_, "section": section, "expect": "ignore", "hex": data.hex(), "kind": kind,
                      "payload_len": len(data) - HEADER_LEN, "why": why})

    # --- the worked example, byte for byte as the contract prints it (section 11)
    ex = bytes.fromhex("52424C4B0100010001 00CDAB04000000 00401E18240A0600".replace(" ", ""))
    assert ex == datagram({"kind": "heartbeat"}, session=0xABCD0001, seq=4, time_us=1_700_000_000_000_000)
    valid("dg.heartbeat.contract_example", "9.2, 11", {"kind": "heartbeat"}, raw=ex, encode=True,
          hdr_expect=hdr_dict(session=0xABCD0001, seq=4, time_us=1_700_000_000_000_000))
    valid("dg.heartbeat.seq_wraps", "9.1", {"kind": "heartbeat"}, hdr=dict(session=1, seq=0xFFFFFFFF, time_us=0))
    valid("dg.heartbeat.max_values", "9.1", {"kind": "heartbeat"},
          hdr=dict(session=0xFFFFFFFF, seq=0xFFFFFFFF, time_us=2**64 - 1))
    valid("dg.heartbeat.flags_ignored", "9.1", {"kind": "heartbeat"}, hdr=dict(flags=0xFF), encode=False,
          note="flags are ignored by v1 receivers; the header's flags are not compared", hdr_expect={
              "version": 1, "session": 0xABCD0001, "seq": 4, "time_us": "1700000000000000"})

    # --- camera
    for dr in (0, 1, 2):
        valid("dg.camera.depth_range_%d" % dr, "9.3", camera_body(dr))
    valid("dg.camera.infinite_far", "9.3", camera_body(2, INF))
    valid("dg.camera.finite_far_range1", "9.3", camera_body(1, 1000.0))

    # --- pose
    valid("dg.pose.one", "9.4", {"kind": "pose", "poses": [pose_at(1)]})
    valid("dg.pose.two_teleport", "9.4", {"kind": "pose", "poses": [pose_at(1, 1), pose_at(2)]})
    full = valid("dg.pose.max_102_is_8192_bytes", "9.4", {"kind": "pose", "poses": [pose_at(i) for i in range(102)]})
    assert len(full) == 8192
    b = {"kind": "pose", "poses": [pose_at(5)]}
    raw = header(3) + u32(1) + u32(0xDEADBEEF) + encode_pose(b["poses"][0])
    valid("dg.pose.reserved_ignored", "3, 9.4", b, raw=raw, encode=False, note="reserved is ignored on read")

    # --- collision
    valid("dg.collision.empty_query", "9.5", collision_body(tris=0))
    valid("dg.collision.three_triangles", "9.5", collision_body(tris=3))
    valid("dg.collision.chunk_2_of_3", "9.5", collision_body(query=0xFFFFFFFF, index=2, count=3, tris=5))
    full = valid("dg.collision.max_226_is_8192_bytes", "9.5", collision_body(tris=226))
    assert len(full) == 8192
    b = collision_body(tris=1)
    raw = (header(4) + u32(7) + struct.pack("<HH", 0, 1) + fl(b["center"]) + f32(b["radius"]) + u32(1)
           + u32(0xCAFEBABE) + fl(b["triangles"][0]))
    valid("dg.collision.reserved_ignored", "3, 9.5", b, raw=raw, encode=False, note="reserved is ignored on read")

    # --- frame
    valid("dg.frame.basic", "9.6", frame_body())
    valid("dg.frame.no_camera", "9.6", frame_body(frame=1, slot=1, camera_seq=0, width=1, height=1),
          note="camera_seq 0 means none")
    valid("dg.frame.big_numbers", "9.6", frame_body(frame=2**64 - 1, slot=15, camera_seq=0xFFFFFFFF,
                                                    width=16384, height=4096))

    # --- unknown kinds are ignored, not errors (9.1)
    ignored("dg.unknown_kind.0", "9.1", header(0) + b"\x01\x02\x03", 0, "kind 0 is not one of 1..5")
    ignored("dg.unknown_kind.6_empty", "9.1", header(6), 6, "an unknown kind is ignored")
    ignored("dg.unknown_kind.9_payload", "9.1", header(9) + bytes(range(40)), 9, "an unknown kind is ignored")
    ignored("dg.unknown_kind.255_big", "9.1", header(255) + bytes(8192 - HEADER_LEN), 255,
            "an unknown kind is ignored whatever its payload length")

    # --- invalid: header
    hb = header(1)
    invalid("dg.bad.empty", "9", b"", "shorter than the header")
    invalid("dg.bad.one_byte", "9", b"R", "shorter than the header")
    invalid("dg.bad.header_minus_one", "9", hb[:-1], "shorter than the header")
    invalid("dg.bad.magic_RBLX", "9.1", header(1, magic=0x584C4252), "magic is 0x4B4C4252")
    invalid("dg.bad.magic_big_endian", "9.1", header(1, magic=0x52424C4B), "magic is little-endian 'RBLK'")
    invalid("dg.bad.magic_zero", "9.1", header(1, magic=0), "magic is 0x4B4C4252")
    invalid("dg.bad.version_0", "6, 9.1", header(1, version=0), "another version than the negotiated 1 is dropped")
    invalid("dg.bad.version_2", "6, 9.1", header(1, version=2), "another version than the negotiated 1 is dropped")
    invalid("dg.bad.version_big_endian_1", "9.1", header(1, version=0x0100), "version is a little-endian u16")
    # --- invalid: lengths
    invalid("dg.bad.heartbeat_25_bytes", "9.2", hb + b"\x00", "Heartbeat is exactly 24 bytes")
    cam = datagram(camera_body())
    invalid("dg.bad.camera_175_bytes", "9.3", cam[:-1], "Camera is exactly 176 bytes")
    invalid("dg.bad.camera_177_bytes", "9.3", cam + b"\x00", "Camera is exactly 176 bytes")
    invalid("dg.bad.camera_header_only", "9.3", header(2), "Camera is exactly 176 bytes")
    for dr in (3, 255, 0xFFFFFFFF):
        raw = cam[:-4] + u32(dr)
        invalid("dg.bad.camera_depth_range_%d" % dr, "9.3", raw, "depth_range other than 0, 1, 2 is malformed")
    p1 = datagram({"kind": "pose", "poses": [pose_at(1)]})
    p2 = datagram({"kind": "pose", "poses": [pose_at(1), pose_at(2)]})
    invalid("dg.bad.pose_count_0", "9.4", header(3) + u32(0) + u32(0), "count is at least 1")
    invalid("dg.bad.pose_header_only_no_count", "9.4", header(3), "a Pose payload has at least its 8 byte header")
    invalid("dg.bad.pose_truncated_count_field", "9.4", header(3) + b"\x01\x00", "payload shorter than its fields")
    invalid("dg.bad.pose_count_2_length_for_1", "9.4", p1[:24] + u32(2) + p1[28:], "length must be 24 + 8 + 80 * count")
    invalid("dg.bad.pose_count_1_length_for_2", "9.4", p2[:24] + u32(1) + p2[28:], "length must be 24 + 8 + 80 * count")
    invalid("dg.bad.pose_truncated_record", "9.4", p1[:-1], "length must be 24 + 8 + 80 * count")
    invalid("dg.bad.pose_one_trailing_byte", "9.4", p1 + b"\x00", "length must be 24 + 8 + 80 * count")
    raw = header(3) + u32(103) + u32(0) + b"".join(encode_pose(pose_at(i)) for i in range(103))
    invalid("dg.bad.pose_count_103", "9.4", raw, "count is at most 102 (and a datagram at most 8192 bytes)")
    invalid("dg.bad.pose_count_u32_max", "9.4", header(3) + u32(0xFFFFFFFF) + u32(0) + bytes(80),
            "count 4294967295 with one record's bytes: no overflow may make this pass")
    invalid("dg.bad.pose_count_2_pow_29_plus_1", "9.4", header(3) + u32(2**29 + 1) + u32(0) + bytes(80),
            "80 * count wraps in 32 bits to 80: the length check must not wrap")
    full = datagram({"kind": "pose", "poses": [pose_at(i) for i in range(102)]})
    invalid("dg.bad.pose_102_plus_one_byte", "2, 9.4", full + b"\x00", "8193 bytes: above MAX_DATAGRAM and not 24 + 8 + 80 * count")
    c0 = datagram(collision_body(tris=0))
    c1 = datagram(collision_body(tris=1))
    invalid("dg.bad.collision_header_only", "9.5", header(4), "a Collision payload has at least its 32 byte header")
    invalid("dg.bad.collision_31_byte_payload", "9.5", c0[:-1], "a Collision payload has at least its 32 byte header")
    invalid("dg.bad.collision_count_1_length_for_0", "9.5", c0[:24 + 24] + u32(1) + c0[24 + 28:],
            "length must be 24 + 32 + 36 * triangle_count")
    invalid("dg.bad.collision_count_0_length_for_1", "9.5", c1[:24 + 24] + u32(0) + c1[24 + 28:],
            "length must be 24 + 32 + 36 * triangle_count")
    invalid("dg.bad.collision_truncated_triangle", "9.5", c1[:-1], "length must be 24 + 32 + 36 * triangle_count")
    raw = header(4) + u32(7) + struct.pack("<HH", 0, 1) + fl([1.0, 2.0, 3.0]) + f32(64.0) + u32(227) + u32(0) \
        + b"".join(fl(tri_at(i)) for i in range(227))
    invalid("dg.bad.collision_227_triangles", "9.5", raw, "triangle_count is 0 to 226 (and a datagram at most 8192 bytes)")
    invalid("dg.bad.collision_count_u32_max", "9.5", c1[:24 + 24] + u32(0xFFFFFFFF) + c1[24 + 28:],
            "triangle_count 4294967295 with one triangle's bytes")
    invalid("dg.bad.collision_count_2_pow_28_plus_1", "9.5", c1[:24 + 24] + u32(2**28 + 1) + c1[24 + 28:],
            "36 * count wraps in 32 bits to 36: the length check must not wrap")
    c = collision_body(tris=0)
    for name, idx, cnt, why in (("chunk_count_0", 0, 0, "chunk_count is at least 1"),
                                ("chunk_index_equals_count", 3, 3, "chunk_index is 0 .. chunk_count-1"),
                                ("chunk_index_above_count", 9, 3, "chunk_index is 0 .. chunk_count-1"),
                                ("chunk_index_u16_max_count_u16_max", 65535, 65535, "chunk_index is 0 .. chunk_count-1")):
        raw = (header(4) + u32(7) + struct.pack("<HH", idx, cnt) + fl(c["center"]) + f32(c["radius"]) + u32(0) + u32(0))
        invalid("dg.bad.collision_" + name, "9.5", raw, why)
    fr = datagram(frame_body())
    invalid("dg.bad.frame_47_bytes", "9.6", fr[:-1], "Frame is exactly 48 bytes")
    invalid("dg.bad.frame_49_bytes", "9.6", fr + b"\x00", "Frame is exactly 48 bytes")
    invalid("dg.bad.frame_header_only", "9.6", header(5), "Frame is exactly 48 bytes")

    # --- revision 5 (section 9 and 9.6): what the contract now says in so many words
    invalid("dg.bad.frame_0", "9.6", datagram(frame_body(frame=0)),
            "frame numbers start at 1; a datagram naming frame 0 would match a slot nobody wrote")
    valid("dg.frame.slot_u32_max_is_not_checked_by_the_decoder", "9.6", frame_body(slot=0xFFFFFFFF),
          note="the reader checks slot against the file, not the decoder")
    valid("dg.frame.zero_size_is_not_checked_by_the_decoder", "9.6", frame_body(width=0, height=0),
          note="the reader compares the size with the slot header")
    # collision: chunk_count at most 4096 (9.5)
    valid("dg.collision.chunk_count_4096_first_chunk", "9.5", collision_body(index=0, count=4096, tris=0))
    valid("dg.collision.chunk_count_4096_last_chunk", "9.5", collision_body(index=4095, count=4096, tris=2))
    for cnt, idx in ((4097, 0), (4097, 4096), (9281, 5), (65535, 0), (65535, 65534)):
        invalid("dg.bad.collision_chunk_count_%d_index_%d" % (cnt, idx), "9.5",
                datagram(collision_body(index=idx, count=cnt, tris=0)),
                "chunk_count is at most 4096 (MAX_COLLISION_CHUNKS): a query holds at most 925696 triangles")
    # datagram size (2)
    ignored("dg.unknown_kind.9_8193_bytes", "2, 9.1", header(9) + bytes(8193 - HEADER_LEN), 9,
            "an unknown kind is ignored at any length")
    ignored("dg.unknown_kind.255_9000_bytes", "2, 9.1", header(255) + bytes(9000 - HEADER_LEN), 255,
            "an unknown kind is ignored at any length")
    invalid("dg.bad.heartbeat_8193_bytes", "2, 9.2", header(1) + bytes(8193 - HEADER_LEN),
            "a datagram of a known kind above 8192 bytes is malformed")
    invalid("dg.bad.camera_8193_bytes", "2, 9.3", header(2) + bytes(8193 - HEADER_LEN),
            "a datagram of a known kind above 8192 bytes is malformed")
    invalid("dg.bad.collision_226_plus_one_byte", "2, 9.5", datagram(collision_body(tris=226)) + b"\x00",
            "8193 bytes: the receiver's buffer must not cut it back to a valid 8192")
    invalid("dg.bad.collision_226_plus_808_bytes", "2, 9.5", datagram(collision_body(tris=226)) + bytes(808),
            "9000 bytes: the receiver's buffer must not cut it back to a valid 8192")
    # pose: flags bits other than TELEPORT are delivered as sent and ignored (9.4)
    for tag, fl_ in (("bit1", 2), ("bit31", 0x80000000), ("all_but_teleport", 0xFFFFFFFE), ("all", 0xFFFFFFFF)):
        p = pose_at(1)
        p["flags"] = fl_
        valid("dg.pose.flags_" + tag, "9.4", {"kind": "pose", "poses": [p]},
              note="bits other than bit 0 are ignored by the receiver; the decoder delivers the field as sent")
    for tag, field, value in (("rotation_not_unit", "rotation", [0.0, 0.0, 0.0, 2.0]),
                              ("scale_zero", "scale", [0.0, 0.0, 0.0]), ("scale_negative", "scale", [-1.0, 1.0, 1.0])):
        p = pose_at(1)
        p[field] = value
        valid("dg.pose." + tag, "9.4", {"kind": "pose", "poses": [p]},
              note="valid; the decoder checks no pose value (9.4, Values)")

    # the values the decoder does not check (9.3, 9.4, 9.5): the datagram decodes, the contents are not compared
    NAN = struct.pack("<I", 0x7FC00000)
    PINF, NINF = struct.pack("<f", INF), struct.pack("<f", -INF)

    def patch(data, off, raw):
        return data[:off] + raw + data[off + len(raw):]

    def accepted(id_, section, data, kind, why):
        cases.append({"id": id_, "section": section, "expect": "accept", "hex": data.hex(), "kind": kind, "why": why})

    P0 = HEADER_LEN
    camd = datagram(camera_body())
    for tag, off, raw in (("near_zero", 132, f32(0.0)), ("near_negative", 132, f32(-0.5)), ("near_nan", 132, NAN),
                          ("near_inf", 132, PINF), ("far_below_near", 136, f32(0.0625)),
                          ("far_equals_near", 136, f32(0.125)), ("far_nan", 136, NAN), ("far_negative_inf", 136, NINF),
                          ("fov_nan", 128, NAN), ("fov_negative", 128, f32(-1.0)), ("view_nan", 0, NAN),
                          ("view_inf", 60, PINF), ("proj_nan", 64 + 20, NAN), ("proj_negative_inf", 64 + 8, NINF),
                          ("viewport_width_zero", 140, u32(0)), ("viewport_height_zero", 144, u32(0)),
                          ("viewport_u32_max", 140, u32(0xFFFFFFFF))):
        accepted("dg.unchecked.camera_" + tag, "9.3", patch(camd, P0 + off, raw), "camera",
                 "a decoder checks only depth_range of a camera; the values are delivered as sent")
    posed = datagram({"kind": "pose", "poses": [pose_at(1)]})
    for tag, off, raw in (("translation_nan", 16, NAN), ("translation_inf", 20, PINF), ("rotation_nan", 28, NAN),
                          ("rotation_zero", 28, bytes(16)), ("scale_nan", 44, NAN), ("velocity_inf", 60, PINF),
                          ("angular_velocity_nan", 68, NAN)):
        accepted("dg.unchecked.pose_" + tag, "9.4", patch(posed, P0 + 8 + off, raw), "pose",
                 "a decoder checks no value of a pose record")
    accepted("dg.unchecked.pose_second_record_nan", "9.4",
             patch(datagram({"kind": "pose", "poses": [pose_at(1), pose_at(2)]}), P0 + 8 + 80 + 16, NAN), "pose",
             "a bad number in one record does not make the datagram malformed")
    cold = datagram(collision_body(tris=2))
    for tag, off, raw in (("center_nan", 8, NAN), ("center_inf", 12, PINF), ("radius_nan", 20, NAN),
                          ("radius_negative", 20, f32(-1.0)), ("radius_zero", 20, f32(0.0)),
                          ("vertex_nan", 32 + 4, NAN), ("vertex_inf", 32 + 36 + 8, PINF)):
        accepted("dg.unchecked.collision_" + tag, "9.5", patch(cold, P0 + off, raw), "collision",
                 "a decoder checks no collision value")
    return cases


# ---------------------------------------------------------------------------------------------
# control.json

def control_cases():
    cases = []

    def accept(id_, section, line, message, encode=False, note=None):
        c = {"id": id_, "section": section, "expect": "message", "line": line, "message": message, "encode": encode}
        if note:
            c["note"] = note
        cases.append(c)

    def unknown(id_, section, line, typ):
        cases.append({"id": id_, "section": section, "expect": "unknown", "line": line, "type": typ,
                      "why": "unknown types are ignored"})

    def reject(id_, section, line, why, line_hex=None):
        c = {"id": id_, "section": section, "expect": "reject", "why": why}
        if line_hex is not None:
            c["line_hex"] = line_hex
        else:
            c["line"] = line
        cases.append(c)

    J = lambda **kw: json.dumps(kw, separators=(",", ":"), ensure_ascii=False)

    # --- the contract's own examples (section 6, 8)
    hg = {"type": "hello", "role": "guest", "game": "Warframe rewrite", "protocol": 1, "min_protocol": 1,
          "udp_port": 53001, "heartbeat_ms": 250, "timeout_ms": 2000}
    accept("ctl.hello.guest_contract_example", "6", json.dumps(hg, separators=(",", ":")), hg, True)
    hh = {"type": "hello", "role": "host", "game": "War Thunder rewrite", "protocol": 1, "min_protocol": 1,
          "udp_port": 53000, "heartbeat_ms": 250, "timeout_ms": 2000, "session": 2882400001}
    accept("ctl.hello.host_contract_example", "6", json.dumps(hh, separators=(",", ":")), hh, True)
    sp = {"type": "spawn", "entity": 7, "provider": "Warframe", "asset": "/Lotus/Characters/Excalibur",
          "name": "Excalibur"}
    accept("ctl.spawn.contract_example", "8", json.dumps(sp, separators=(",", ":")), sp, True)
    accept("ctl.despawn.contract_example", "8", '{"type":"despawn","entity":7}', {"type": "despawn", "entity": 7}, True)
    accept("ctl.input.focus_guest", "8", '{"type":"input","focus":"guest"}', {"type": "input", "focus": "guest"}, True)
    accept("ctl.input.focus_host_with_reason", "8", '{"type":"input","focus":"host","reason":"menu opened"}',
           {"type": "input", "focus": "host", "reason": "menu opened"}, True)
    fc = {"type": "frame_channel", "path": "/tmp/rewrite-bridge-2882400001-guest.frames", "width": 1280,
          "height": 720, "slots": 3}
    accept("ctl.frame_channel.contract_example", "8", json.dumps(fc, separators=(",", ":")), fc, True)
    accept("ctl.bye.contract_example", "8", '{"type":"bye","reason":"shutting down"}',
           {"type": "bye", "reason": "shutting down"}, True)
    accept("ctl.bye.busy", "2, 8", '{"type":"bye","reason":"busy"}', {"type": "bye", "reason": "busy"}, True)

    # --- boundaries and optional fields
    accept("ctl.spawn.without_name", "8", '{"type":"spawn","entity":0,"provider":"P","asset":"a"}',
           {"type": "spawn", "entity": 0, "provider": "P", "asset": "a"}, True, "entity 0 is valid")
    accept("ctl.spawn.entity_u32_max", "8", '{"type":"spawn","entity":4294967295,"provider":"P","asset":"a"}',
           {"type": "spawn", "entity": 4294967295, "provider": "P", "asset": "a"}, True)
    accept("ctl.despawn.entity_u32_max", "8", '{"type":"despawn","entity":4294967295}',
           {"type": "despawn", "entity": 4294967295}, True)
    accept("ctl.hello.host_session_u32_max", "8", J(**dict(hh, session=4294967295)), dict(hh, session=4294967295), True)
    accept("ctl.hello.udp_port_65535", "8", J(**dict(hg, udp_port=65535)), dict(hg, udp_port=65535), True)
    accept("ctl.hello.heartbeat_bounds_are_not_a_parse_error_low", "8", J(**dict(hg, heartbeat_ms=10)),
           dict(hg, heartbeat_ms=10), True)
    accept("ctl.hello.empty_game", "8", J(**dict(hg, game="")), dict(hg, game=""), True)
    accept("ctl.frame_channel.windows_path", "8", J(**dict(fc, path="C:\\Temp\\rewrite-bridge-1-guest.frames")),
           dict(fc, path="C:\\Temp\\rewrite-bridge-1-guest.frames"), True)

    # --- JSON freedom: key order, whitespace, escapes, unicode
    accept("ctl.hello.key_order_and_spaces", "8",
           '{ "timeout_ms" : 2000 , "heartbeat_ms":250,"udp_port":53001,"min_protocol":1,"protocol":1,'
           '"game":"Warframe rewrite","role":"guest","type":"hello" }', hg)
    accept("ctl.bye.surrounding_whitespace", "8", '  \t{"type":"bye","reason":"x"}  ', {"type": "bye", "reason": "x"})
    accept("ctl.spawn.unicode_raw", "8", J(type="spawn", entity=1, provider="Pr\u00f3vider",
                                           asset="/\u65e5\u672c\u8a9e/\U0001f600", name="Caf\u00e9"),
           {"type": "spawn", "entity": 1, "provider": "Pr\u00f3vider", "asset": "/\u65e5\u672c\u8a9e/\U0001f600",
            "name": "Caf\u00e9"}, True)
    accept("ctl.spawn.unicode_escaped", "8",
           '{"type":"spawn","entity":1,"provider":"Pr\\u00f3vider","asset":"/\\u65e5\\u672c\\u8a9e/\\ud83d\\ude00",'
           '"name":"Caf\\u00e9"}',
           {"type": "spawn", "entity": 1, "provider": "Pr\u00f3vider", "asset": "/\u65e5\u672c\u8a9e/\U0001f600",
            "name": "Caf\u00e9"})
    accept("ctl.spawn.escapes", "8", '{"type":"spawn","entity":1,"provider":"a\\"b","asset":"c\\\\d\\/e\\n\\t","name":"x"}',
           {"type": "spawn", "entity": 1, "provider": "a\"b", "asset": "c\\d/e\n\t", "name": "x"}, True,
           "the encoder must escape quote, backslash and control characters so that the line stays one line")
    accept("ctl.bye.reason_with_newline", "6.1, 8", '{"type":"bye","reason":"line1\\nline2"}',
           {"type": "bye", "reason": "line1\nline2"}, True)
    # --- unknown fields are ignored
    accept("ctl.hello.unknown_fields_ignored", "8",
           '{"type":"hello","role":"guest","game":"g","protocol":1,"min_protocol":1,"udp_port":9,"heartbeat_ms":250,'
           '"timeout_ms":2000,"future":{"a":[1,2,{"b":null}]},"x":true}',
           {"type": "hello", "role": "guest", "game": "g", "protocol": 1, "min_protocol": 1, "udp_port": 9,
            "heartbeat_ms": 250, "timeout_ms": 2000})
    accept("ctl.spawn.unknown_fields_ignored", "8", '{"type":"spawn","entity":2,"provider":"P","asset":"a","scale":2}',
           {"type": "spawn", "entity": 2, "provider": "P", "asset": "a"})
    accept("ctl.input.reason_optional_and_extra", "8", '{"type":"input","focus":"guest","extra":1}',
           {"type": "input", "focus": "guest"})
    # --- unknown types
    unknown("ctl.unknown.ping", "8", '{"type":"ping"}', "ping")
    unknown("ctl.unknown.with_fields", "8", '{"type":"future_message","a":1,"b":{"c":[1,2,3]}}', "future_message")
    unknown("ctl.unknown.case_differs_Hello", "8", '{"type":"Hello","role":"guest"}', "Hello")
    unknown("ctl.unknown.prefix_hello2", "8", '{"type":"hello2"}', "hello2")
    unknown("ctl.unknown.empty_type", "8", '{"type":""}', "")
    unknown("ctl.unknown.camera", "8", '{"type":"camera"}', "camera")

    # --- not a JSON object with a string "type"
    for id_, line, why in (
            ("empty_line", "", "not a JSON object"),
            ("blank_line", "   ", "not a JSON object"),
            ("plain_word", "hello", "not JSON"),
            ("open_brace", "{", "not JSON"),
            ("truncated_object", '{"type":"hello"', "truncated JSON"),
            ("truncated_string", '{"type":"hel', "truncated JSON"),
            ("array", "[]", "not an object"),
            ("array_of_object", '[{"type":"bye"}]', "not an object"),
            ("empty_object", "{}", "no string type"),
            ("null", "null", "not an object"),
            ("number", "42", "not an object"),
            ("string", '"bye"', "not an object"),
            ("true", "true", "not an object"),
            ("type_number", '{"type":42}', "type is not a string"),
            ("type_null", '{"type":null}', "type is not a string"),
            ("type_array", '{"type":["bye"]}', "type is not a string"),
            ("type_object", '{"type":{"a":1}}', "type is not a string"),
            ("type_missing_capitalised_key", '{"Type":"bye"}', "no string type"),
            ("single_quotes", "{'type':'bye'}", "not JSON"),
            ("trailing_comma", '{"type":"bye",}', "not JSON"),
            ("trailing_garbage", '{"type":"bye"} x', "not a single JSON object"),
            ("two_objects", '{"type":"bye"}{"type":"bye"}', "one JSON object per line"),
            ("unquoted_key", '{type:"bye"}', "not JSON"),
            ("comment", '{"type":"bye"} // x', "not JSON"),
            ("unterminated_escape", '{"type":"bye","reason":"\\', "not JSON"),
            ("bad_escape", '{"type":"bye","reason":"\\q"}', "not JSON"),
            ("control_char_in_string", '{"type":"bye","reason":"a\tb"}', "not JSON (raw control characters)"),
            ):
        reject("ctl.bad." + id_, "8", line, why)
    reject("ctl.bad.invalid_utf8", "8", None, "control lines are UTF-8", line_hex=b'{"type":"bye","reason":"\xff\xfe"}'.hex())
    reject("ctl.bad.overlong_utf8", "8", None, "control lines are UTF-8", line_hex=b'{"type":"bye","reason":"\xc0\xaf"}'.hex())

    # --- known type, missing field
    full = {
        "hello": {"type": "hello", "role": "guest", "game": "g", "protocol": 1, "min_protocol": 1, "udp_port": 9,
                  "heartbeat_ms": 250, "timeout_ms": 2000},
        "spawn": {"type": "spawn", "entity": 1, "provider": "P", "asset": "a"},
        "despawn": {"type": "despawn", "entity": 1},
        "input": {"type": "input", "focus": "host"},
        "frame_channel": {"type": "frame_channel", "path": "/tmp/x.frames", "width": 4, "height": 2, "slots": 3},
    }
    for typ, msg in full.items():
        for k in msg:
            if k == "type":
                continue
            m = dict(msg)
            del m[k]
            reject("ctl.bad.%s_missing_%s" % (typ, k), "8", J(**m), "a known type with a missing field")
    # --- known type, wrong type or out-of-range value
    def bad_value(typ, key, value, tag, why):
        m = dict(full[typ])
        m[key] = value
        reject("ctl.bad.%s_%s_%s" % (typ, key, tag), "8", J(**m), why)

    for typ, key in (("spawn", "entity"), ("despawn", "entity")):
        for tag, v in (("string", "7"), ("negative", -1), ("above_u32", 4294967296), ("fraction", 1.5),
                       ("bool", True), ("null", None), ("array", [7]), ("huge", 18446744073709551616)):
            bad_value(typ, key, v, tag, "entity ids are integers in 0 ..= 4294967295")
    for tag, v in (("string", "53001"), ("negative", -1), ("above_u16", 65536), ("bool", False), ("null", None)):
        bad_value("hello", "udp_port", v, tag, "a UDP port is an integer in 0 ..= 65535")
    for key in ("protocol", "min_protocol", "heartbeat_ms", "timeout_ms"):
        for tag, v in (("string", "1"), ("negative", -1), ("null", None), ("bool", True)):
            bad_value("hello", key, v, tag, "an integer field")
    for tag, v in (("number", 7), ("null", None), ("array", ["g"])):
        bad_value("hello", "game", v, tag, "game is a string")
    for tag, v in (("other", "observer"), ("capitalised", "Guest"), ("number", 1), ("null", None), ("empty", "")):
        bad_value("hello", "role", v, tag, "role is \"host\" or \"guest\"")
    for tag, v in (("string", "1"), ("negative", -1), ("above_u32", 4294967296), ("fraction", 0.5)):
        m = dict(hh)
        m["session"] = v
        reject("ctl.bad.hello_session_%s" % tag, "8", J(**m), "session is a u32")
    for key in ("provider", "asset"):
        for tag, v in (("number", 5), ("null", None), ("object", {})):
            bad_value("spawn", key, v, tag, key + " is a string")
    bad_value("spawn", "name", 5, "number", "name is an optional string")
    for tag, v in (("both", "both"), ("capitalised", "Host"), ("number", 1), ("null", None), ("empty", "")):
        bad_value("input", "focus", v, tag, "focus is \"host\" or \"guest\"")
    bad_value("input", "reason", 5, "number", "reason is an optional string")
    for key in ("width", "height", "slots"):
        for tag, v in (("string", "4"), ("negative", -1), ("null", None), ("above_u32", 4294967296)):
            bad_value("frame_channel", key, v, tag, key + " is an integer")
    for tag, v in (("number", 5), ("null", None), ("array", [])):
        bad_value("frame_channel", "path", v, tag, "path is a string")

    # =========================== revision 5: the syntax of a line, settled (section 8) ===========================
    # --- bye: reason is optional, null counts as absent, another type is an error
    accept("ctl.bye.without_reason", "8", '{"type":"bye"}', {"type": "bye", "reason": ""},
           note="no reason is the empty reason")
    accept("ctl.bye.reason_null", "8", '{"type":"bye","reason":null}', {"type": "bye", "reason": ""})
    accept("ctl.bye.reason_empty", "8", '{"type":"bye","reason":""}', {"type": "bye", "reason": ""}, True)
    for tag, v in (("number", 5), ("true", True), ("array", ["x"]), ("object", {})):
        reject("ctl.bad.bye_reason_" + tag, "8", J(type="bye", reason=v), "reason is an optional string")

    # --- integers are integer literals
    for tag, lit in (("7_0", "7.0"), ("7e0", "7e0"), ("7E0", "7E0"), ("7e_plus_0", "7e+0"), ("70e_minus_1", "70e-1"),
                     ("0_0", "0.0"), ("neg_0", "-0"), ("neg_0_0", "-0.0"), ("u32_max_0", "4294967295.0"),
                     ("1e0", "1e0"), ("1e1", "1e1"), ("1e400", "1e400"), ("long_fraction", "7.00000000000000000000001"),
                     ("30_digits", "123456789012345678901234567890")):
        reject("ctl.bad.despawn_entity_%s" % tag, "8", '{"type":"despawn","entity":%s}' % lit,
               "an integer field takes digits only: no sign, fraction or exponent, whatever the value")
    hello_text = json.dumps(hg, separators=(",", ":"))
    for key, was, now in (("protocol", '"protocol":1,', '"protocol":1.0,'),
                          ("min_protocol", '"min_protocol":1,', '"min_protocol":1e0,'),
                          ("udp_port", '"udp_port":53001', '"udp_port":53001.0'),
                          ("heartbeat_ms", '"heartbeat_ms":250', '"heartbeat_ms":2.5e2'),
                          ("timeout_ms", '"timeout_ms":2000', '"timeout_ms":2000.0')):
        assert was in hello_text
        reject("ctl.bad.hello_%s_not_an_integer_literal" % key, "8", hello_text.replace(was, now),
               "an integer field takes digits only")
    reject("ctl.bad.frame_channel_slots_not_an_integer_literal", "8",
           '{"type":"frame_channel","path":"/tmp/x","width":4,"height":2,"slots":3.0}', "an integer field takes digits only")
    reject("ctl.bad.hello_session_not_an_integer_literal", "8", json.dumps(hh, separators=(",", ":")).replace(
        '"session":2882400001', '"session":2882400001.0'), "an integer field takes digits only")

    # --- widths of the hello integers
    accept("ctl.hello.protocol_and_min_protocol_65535", "8", J(**dict(hg, protocol=65535, min_protocol=65535)),
           dict(hg, protocol=65535, min_protocol=65535), True, "the version fields are u16")
    accept("ctl.hello.protocol_0", "8", J(**dict(hg, protocol=0, min_protocol=0)), dict(hg, protocol=0, min_protocol=0),
           True)
    for key, v in (("protocol", 65536), ("protocol", 4294967296), ("min_protocol", 65536), ("min_protocol", 4294967296)):
        reject("ctl.bad.hello_%s_%d" % (key, v), "8", J(**dict(hg, **{key: v})), "the version fields are u16")
    accept("ctl.hello.heartbeat_ms_u32_max_is_a_range_error_not_a_parse_error", "8", J(**dict(hg, heartbeat_ms=4294967295)),
           dict(hg, heartbeat_ms=4294967295), True, "10 ..= 10000 is checked by the handshake, not the parser")
    accept("ctl.hello.heartbeat_ms_0_is_a_range_error_not_a_parse_error", "8", J(**dict(hg, heartbeat_ms=0)),
           dict(hg, heartbeat_ms=0), True)
    reject("ctl.bad.hello_heartbeat_ms_above_u32", "8", J(**dict(hg, heartbeat_ms=4294967296)), "heartbeat_ms is a u32")
    accept("ctl.hello.timeout_ms_0", "8", J(**dict(hg, timeout_ms=0)), dict(hg, timeout_ms=0), True,
           "timeout_ms is informative and has no range beyond its type")
    accept("ctl.hello.timeout_ms_u32_max", "8", J(**dict(hg, timeout_ms=4294967295)), dict(hg, timeout_ms=4294967295), True)
    reject("ctl.bad.hello_timeout_ms_above_u32", "8", J(**dict(hg, timeout_ms=4294967296)), "timeout_ms is a u32")
    accept("ctl.hello.udp_port_0_is_a_range_error_not_a_parse_error", "8", J(**dict(hg, udp_port=0)),
           dict(hg, udp_port=0), True, "udp_port 0 is refused by the handshake, not the parser")
    accept("ctl.frame_channel.u32_max_sizes", "8",
           '{"type":"frame_channel","path":"/tmp/x","width":4294967295,"height":4294967295,"slots":4294967295}',
           {"type": "frame_channel", "path": "/tmp/x", "width": 4294967295, "height": 4294967295, "slots": 4294967295},
           True, "the file limits are checked when the file is opened, not by the parser")

    # --- duplicate keys: the last wins, everywhere, and the earlier values still have to be valid
    accept("ctl.json.duplicate_key_last_wins", "8", '{"type":"despawn","entity":1,"entity":2}',
           {"type": "despawn", "entity": 2})
    accept("ctl.json.duplicate_key_same_value", "8", '{"type":"despawn","entity":1,"entity":1}',
           {"type": "despawn", "entity": 1})
    accept("ctl.json.duplicate_type_last_wins", "8", '{"type":"despawn","type":"bye","entity":1}',
           {"type": "bye", "reason": ""})
    accept("ctl.json.duplicate_escaped_key_is_the_same_key", "8", '{"type":"despawn","entity":1,"\\u0065ntity":2}',
           {"type": "despawn", "entity": 2}, note="keys are compared after their escapes are decoded")
    accept("ctl.json.duplicate_unknown_key", "8", '{"type":"despawn","entity":1,"x":1,"x":2}',
           {"type": "despawn", "entity": 1})
    accept("ctl.json.duplicate_key_in_nested_object", "8", '{"type":"despawn","entity":1,"x":{"a":1,"a":2}}',
           {"type": "despawn", "entity": 1})
    reject("ctl.bad.duplicate_type_last_is_hello", "8", '{"type":"bye","type":"hello"}',
           "the last type wins, and a hello needs its fields")
    reject("ctl.bad.duplicate_key_hides_a_nul", "8", '{"type":"despawn","entity":1,"x":"\\u0000","x":1}',
           "the overridden value still has to be valid")
    reject("ctl.bad.duplicate_key_hides_a_lone_surrogate", "8", '{"type":"despawn","entity":1,"x":"\\ud83d","x":1}',
           "the overridden value still has to be valid")
    reject("ctl.bad.duplicate_key_hides_a_syntax_error", "8", '{"type":"despawn","entity":1,"x":01,"x":1}',
           "the overridden value still has to be valid")
    reject("ctl.bad.duplicate_key_hides_a_deep_value", "8", '{"type":"despawn","entity":1,"x":%s%s,"x":1}' % (
        "[" * 130, "]" * 130), "the overridden value still has to be valid")

    # --- null is "absent" for an optional field
    accept("ctl.spawn.name_null_is_absent", "8", '{"type":"spawn","entity":1,"provider":"P","asset":"a","name":null}',
           {"type": "spawn", "entity": 1, "provider": "P", "asset": "a"})
    accept("ctl.input.reason_null_is_absent", "8", '{"type":"input","focus":"host","reason":null}',
           {"type": "input", "focus": "host"})
    accept("ctl.hello.guest_session_null_is_absent", "8", hello_text[:-1] + ',"session":null}', hg)

    # --- strings: surrogates, NUL, escapes, UTF-8
    name_line = '{"type":"spawn","entity":1,"provider":"P","asset":"a","name":"%s"}'
    for tag, esc in (("lone_high", "\\ud83d"), ("lone_low", "\\ude00"), ("high_then_letter", "\\ud83dx"),
                     ("pair_reversed", "\\ude00\\ud83d"), ("high_high_low", "\\ud83d\\ud83d\\ude00"),
                     ("high_then_non_low_escape", "\\ud83d\\u0041"), ("lone_high_upper_case", "\\uD83D"),
                     ("lone_low_then_letter", "\\uDC00a")):
        reject("ctl.bad.string_surrogate_" + tag, "8", name_line % esc, "a surrogate escape must be a high one followed by a low one")
    reject("ctl.bad.string_surrogate_in_unknown_field", "8", '{"type":"despawn","entity":1,"x":"\\ud83d"}',
           "syntax applies to unknown fields too")
    reject("ctl.bad.string_surrogate_in_key", "8", '{"type":"despawn","entity":1,"\\ud83d":1}', "keys are strings too")
    reject("ctl.bad.string_surrogate_in_array_of_unknown_field", "8", '{"type":"despawn","entity":1,"x":[1,{"y":["\\udc00"]}]}',
           "syntax applies to unknown fields too")
    for tag, line in (("name", name_line % "\\u0000"), ("name_middle", name_line % "a\\u0000b"),
                      ("name_then_letter", name_line % "\\u0000a"),
                      ("asset", '{"type":"spawn","entity":1,"provider":"P","asset":"a\\u0000b"}'),
                      ("path", '{"type":"frame_channel","path":"/tmp/a\\u0000b","width":4,"height":2,"slots":3}'),
                      ("game", J(**dict(hg, game="a\u0000b"))), ("bye_reason", '{"type":"bye","reason":"\\u0000"}'),
                      ("unknown_field", '{"type":"despawn","entity":1,"x":"\\u0000"}'),
                      ("key", '{"type":"despawn","entity":1,"a\\u0000b":1}'),
                      ("nested_unknown", '{"type":"despawn","entity":1,"x":{"y":["\\u0000"]}}')):
        reject("ctl.bad.string_nul_" + tag, "8", line, "U+0000 in a string is an error: the C implementation cannot hold it")
    accept("ctl.spawn.escape_hex_digits_in_upper_case", "8", name_line % "\\u00E9",
           {"type": "spawn", "entity": 1, "provider": "P", "asset": "a", "name": "é"})
    accept("ctl.spawn.escaped_u0001_and_u001f", "8", name_line % "\\u0001\\u001f",
           {"type": "spawn", "entity": 1, "provider": "P", "asset": "a", "name": "\u0001\u001f"},
           note="every other control character may be escaped")
    accept("ctl.json.escaped_type", "8", '{"type":"desp\\u0061wn","entity":3}', {"type": "despawn", "entity": 3})
    accept("ctl.json.escaped_key", "8", '{"\\u0074ype":"despawn","entity":3}', {"type": "despawn", "entity": 3})
    accept("ctl.json.empty_key", "8", '{"":1,"type":"despawn","entity":3}', {"type": "despawn", "entity": 3})
    accept("ctl.json.del_in_string", "8", '{"type":"bye","reason":"a\x7fb"}', {"type": "bye", "reason": "a\x7fb"})
    accept("ctl.json.noncharacter_in_string", "8", '{"type":"bye","reason":"￾￿"}',
           {"type": "bye", "reason": "￾￿"}, note="U+FFFE is a valid scalar value")
    for tag, esc in (("x", "\\x41"), ("short_u", "\\u12"), ("bad_hex_u", "\\u12G4"), ("single_quote", "\\'")):
        reject("ctl.bad.string_escape_" + tag, "8", name_line % esc, "not a JSON escape")
    reject("ctl.bad.utf8_encoded_surrogate", "8", None, "UTF-8 may not encode a surrogate",
           line_hex=(b'{"type":"bye","reason":"' + b"\xed\xa0\x80" + b'"}').hex())
    reject("ctl.bad.utf8_above_10ffff", "8", None, "UTF-8 stops at U+10FFFF",
           line_hex=(b'{"type":"bye","reason":"' + b"\xf4\x90\x80\x80" + b'"}').hex())
    reject("ctl.bad.utf8_truncated_sequence", "8", None, "a sequence cut short is not UTF-8",
           line_hex=(b'{"type":"bye","reason":"' + b"\xe2\x82" + b'"}').hex())
    reject("ctl.bad.utf8_lone_continuation_byte", "8", None, "a continuation byte alone is not UTF-8",
           line_hex=(b'{"type":"bye","reason":"' + b"\x80" + b'"}').hex())
    reject("ctl.bad.byte_order_mark", "8", None, "EF BB BF is not whitespace",
           line_hex=(b"\xef\xbb\xbf" + b'{"type":"bye","reason":"x"}').hex())

    # --- whitespace
    ws = {"type": "bye", "reason": "x"}
    accept("ctl.json.cr_between_tokens", "8", '{"type":"bye",\r"reason":"x"}', ws, note="a CR inside the line is whitespace")
    accept("ctl.json.cr_before_the_object", "8", '\r{"type":"bye","reason":"x"}', ws)
    accept("ctl.json.cr_at_the_end", "8", '{"type":"bye","reason":"x"}\r', ws)
    accept("ctl.json.two_cr_at_the_end", "8", '{"type":"bye","reason":"x"}\r\r', ws)
    accept("ctl.json.tab_is_whitespace", "8", '\t{"type":\t"bye",\t"reason":"x"}\t', ws)
    reject("ctl.bad.form_feed_is_not_whitespace", "8", '{"type":"bye",\x0c"reason":"x"}', "only space, tab, CR and LF")
    reject("ctl.bad.vertical_tab_is_not_whitespace", "8", '{"type":"bye",\x0b"reason":"x"}', "only space, tab, CR and LF")
    reject("ctl.bad.nbsp_is_not_whitespace", "8", None, "only space, tab, CR and LF",
           line_hex=(b'{"type":"bye",' + b"\xc2\xa0" + b'"reason":"x"}').hex())
    reject("ctl.bad.nul_byte_between_tokens", "8", None, "only space, tab, CR and LF",
           line_hex=(b'{"type":"bye",\x00"reason":"x"}').hex())

    # --- number and literal grammar (RFC 8259), in known and unknown fields
    for tag, lit in (("leading_zero", "01"), ("plus", "+1"), ("trailing_dot", "1."), ("leading_dot", ".5"),
                     ("hex", "0x10"), ("nan", "NaN"), ("infinity", "Infinity"), ("minus_only", "-"),
                     ("exponent_without_digits", "1e"), ("exponent_sign_only", "1e+")):
        reject("ctl.bad.number_" + tag, "8", '{"type":"despawn","entity":%s}' % lit, "not a JSON number")
    reject("ctl.bad.number_leading_zero_in_unknown_field", "8", '{"type":"despawn","entity":1,"x":01}',
           "syntax applies to unknown fields too")
    for tag, lit in (("True", "True"), ("nul", "nul"), ("nullx", "nullx"), ("true1", "true1"), ("one_a", "1a")):
        reject("ctl.bad.literal_" + tag, "8", '{"type":"despawn","entity":1,"x":%s}' % lit, "not a JSON literal")

    # --- nesting (8, Syntax of a line, 3): the line's object counts as the first of 127 levels
    def deep(n, opener="[", closer="]", leaf=""):
        return '{"type":"despawn","entity":1,"x":%s%s%s}' % ((opener if opener == "[" else '{"a":') * n, leaf,
                                                            closer * n)

    despawn1 = {"type": "despawn", "entity": 1}
    accept("ctl.json.nesting_127_levels_arrays", "8", deep(126), despawn1, note="the object and 126 arrays: 127 levels")
    accept("ctl.json.nesting_127_levels_objects", "8", deep(126, "{", "}", "1"), despawn1,
           note="the object and 126 objects: 127 levels")
    accept("ctl.json.nesting_wide_is_not_deep", "8", '{"type":"despawn","entity":1,"x":[%s]}' % ",".join(["[1]"] * 5000),
           despawn1)
    reject("ctl.bad.nesting_128_levels_arrays", "8", deep(127), "at most 127 levels")
    reject("ctl.bad.nesting_128_levels_objects", "8", deep(127, "{", "}", "1"), "at most 127 levels")
    reject("ctl.bad.nesting_1000_levels", "8", deep(1000), "at most 127 levels")
    reject("ctl.bad.nesting_30000_levels", "8", deep(30000), "a 64 KiB line must not be able to overflow a stack")
    reject("ctl.bad.nesting_30000_levels_unclosed", "8", '{"type":"despawn","entity":1,"x":' + "[" * 30000,
           "the limit is checked before the line ends")
    reject("ctl.bad.nesting_12000_objects", "8", deep(12000, "{", "}", "1"), "a 64 KiB line must not be able to overflow a stack")
    return cases


# ---------------------------------------------------------------------------------------------
# pixel files (9.6)

def rup64(v):
    return (v + 63) // 64 * 64


FILE_MAGIC = b"RBFRAME\x00"


def file_header(slots=3, width=4, height=2, stride=None, fmt=1, slot_size=None, depth_format=0, depth_stride=None,
                depth_offset=None, magic=FILE_MAGIC, version=1, reserved=b"\x00" * 8):
    stride = width * 4 if stride is None else stride
    colour_end = 64 + stride * height
    if depth_format == 1:
        depth_stride = width * 4 if depth_stride is None else depth_stride
        depth_offset = rup64(colour_end) if depth_offset is None else depth_offset
        slot_size = rup64(depth_offset + depth_stride * height) if slot_size is None else slot_size
    else:
        depth_stride = 0 if depth_stride is None else depth_stride
        depth_offset = 0 if depth_offset is None else depth_offset
        slot_size = rup64(colour_end) if slot_size is None else slot_size
    return (magic + struct.pack("<IIIIIIQIIQ", version, slots, width, height, stride, fmt, slot_size, depth_format,
                                depth_stride, depth_offset) + reserved)


assert len(file_header()) == 64


def px_rgba(frame, w, h):
    """Deterministic colour for a frame; pixel 0 is transparent (alpha 0)."""
    b = bytearray(((i * 37 + frame * 11 + 5) & 0xFF) for i in range(w * h * 4))
    b[0:4] = b"\x00\x00\x00\x00"
    return bytes(b)


def px_depth(frame, w, h):
    """Linear metres; +inf where alpha is 0 (the writer stores +inf there, 9.6)."""
    return [INF] + [0.5 * (i + 1) + frame for i in range(1, w * h)]


def slot_bytes(lock, frame, time_us, camera_seq, hdr_w, hdr_h, w, h, rgba, depth, width, height, depth_offset,
               slot_size):
    """One slot: header (the claimed width/height are hdr_w/hdr_h), then w x h pixels laid out with the file's stride."""
    s = struct.pack("<QQQIII", lock, frame, time_us, camera_seq, hdr_w, hdr_h) + bytes(28)
    assert len(s) == 64
    stride = width * 4
    pix = bytearray(stride * height)
    for y in range(h):
        pix[y * stride:y * stride + w * 4] = rgba[y * w * 4:(y + 1) * w * 4]
    s += bytes(pix)
    if depth is not None:
        s = s.ljust(depth_offset, b"\x00")
        dp = bytearray(width * 4 * height)
        for y in range(h):
            for x in range(w):
                struct.pack_into("<f", dp, (y * width + x) * 4, depth[y * w + x])
        s += bytes(dp)
    return s.ljust(slot_size, b"\x00")


class Fixture:
    def __init__(self, width=4, height=2, slots=3, depth=True, depth_format=None, hdr_over=None):
        self.width, self.height, self.slots, self.depth = width, height, slots, depth
        over = dict(hdr_over or {})
        df = (1 if depth else 0) if depth_format is None else depth_format
        self.hdr = file_header(slots=slots, width=width, height=height, depth_format=df, **over)
        self.slot_size = struct.unpack("<Q", self.hdr[32:40])[0]
        self.depth_offset = struct.unpack("<Q", self.hdr[48:56])[0]
        self.slots_data = {}
        self.frames = {}

    def put(self, slot, frame, lock=None, time_us=None, camera_seq=None, w=None, h=None, hdr_wh=None):
        w = self.width if w is None else w
        h = self.height if h is None else h
        rgba = px_rgba(frame, w, h)
        depth = px_depth(frame, w, h) if self.depth else None
        lock = 2 * ((frame - 1) // self.slots + 1) if lock is None else lock
        time_us = 1_700_000_000_000_000 + frame * 16_666 if time_us is None else time_us
        camera_seq = 100 + frame if camera_seq is None else camera_seq
        hw, hh = hdr_wh if hdr_wh else (w, h)
        self.slots_data[slot] = slot_bytes(lock, frame, time_us, camera_seq, hw, hh, w, h, rgba, depth, self.width,
                                           self.height, self.depth_offset, self.slot_size)
        self.frames[slot] = dict(frame=frame, lock=lock, time_us=time_us, camera_seq=camera_seq, w=hw, h=hh,
                                 rgba=rgba, depth=depth)

    def bytes(self):
        out = bytearray(self.hdr)
        for i in range(self.slots):
            out += self.slots_data.get(i, bytes(self.slot_size))
        return bytes(out)

    def read_expect(self, slot, want_depth=True):
        f = self.frames[slot]
        r = {"frame": str(f["frame"]), "slot": slot, "camera_seq": f["camera_seq"], "width": f["w"], "height": f["h"],
             "expect": "ok", "time_us": str(f["time_us"]), "rgba": f["rgba"].hex()}
        r["depth"] = [jf(v) for v in f["depth"]] if (want_depth and f["depth"] is not None) else None
        r["want_depth"] = want_depth
        return r


def skip_read(frame, slot, why, w=4, h=2, camera_seq=0, want_depth=True):
    return {"frame": str(frame), "slot": slot, "camera_seq": camera_seq, "width": w, "height": h, "expect": "skip",
            "why": why, "want_depth": want_depth}


def pixel_cases(outdir):
    files = {}
    cases = []

    def fixture_file(name, fx):
        files[name] = fx.bytes()
        return "pixelfiles/" + name

    def info_of(fx):
        return {"slots": fx.slots, "width": fx.width, "height": fx.height, "stride": fx.width * 4,
                "slot_size": fx.slot_size, "has_depth": fx.hdr[40:44] == struct.pack("<I", 1)}

    # A: depth, 4x2, 3 slots, frames 1..3 written (frame f lives in slot f % 3, 9.6)
    A = Fixture()
    for f in (1, 2, 3):
        A.put(f % 3, f)
    assert A.slot_size == 192 and len(A.bytes()) == 640 and A.depth_offset == 128
    # B: colour only: exactly what revisions 1 and 2 wrote
    B = Fixture(depth=False)
    for f in (1, 2, 3):
        B.put(f % 3, f)
    assert B.slot_size == 128 and len(B.bytes()) == 448 and B.hdr[40:64] == bytes(24)
    # C: A after a fourth write: frame 4 reuses slot 1 and its lock goes from 2 to 4
    C = Fixture()
    for f in (1, 2, 3, 4):
        C.put(f % 3, f)
    assert C.frames[1]["lock"] == 4
    fa = fixture_file("depth_4x2_3slots.bin", A)
    fb = fixture_file("colour_4x2_3slots.bin", B)
    fc = fixture_file("depth_4x2_3slots_after_4_writes.bin", C)

    def read(fx, slot, want_depth=True):
        return fx.read_expect(slot, want_depth)

    cases.append({"id": "pf.read.depth_file_frames_1_to_3", "section": "9.6", "basis": "MUST", "file": fa,
                  "open": "ok", "info": info_of(A),
                  "reads": [read(A, 1), read(A, 2), read(A, 0), read(A, 1, False),
                            skip_read(1, 0, "slot 0 holds frame 3: the copied frame is not the datagram's", camera_seq=101),
                            skip_read(4, 1, "slot 1 holds frame 1, not 4"),
                            skip_read(3, 3, "slot 3 does not exist in a 3 slot file")]})
    cases.append({"id": "pf.read.colour_file_has_no_depth", "section": "9.6", "basis": "MUST", "file": fb,
                  "open": "ok", "info": info_of(B),
                  "reads": [read(B, 1, True), read(B, 2, False), read(B, 0, True)]})
    cases.append({"id": "pf.read.slot_reused_after_wrap", "section": "9.6", "basis": "MUST", "file": fc,
                  "open": "ok", "info": info_of(C),
                  "reads": [read(C, 1), read(C, 2), read(C, 0),
                            skip_read(1, 1, "slot 1 was overwritten with frame 4 (lock 4)")]})

    # D: a frame smaller than the file's maximum (slot header width/height <= file width/height)
    D = Fixture()
    D.put(1, 1, w=3, h=1)
    cases.append({"id": "pf.read.smaller_frame_than_file", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_smaller_frame.bin", D), "open": "ok", "info": info_of(D),
                  "reads": [dict(read(D, 1), width=3, height=1)]})
    # E: odd lock = being written; a never written slot
    E = Fixture()
    E.put(1, 1)
    E.put(2, 2, lock=3)
    cases.append({"id": "pf.read.seqlock_odd_and_unwritten", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_odd_lock.bin", E), "open": "ok", "info": info_of(E),
                  "reads": [read(E, 1),
                            skip_read(2, 2, "lock is odd: the slot is being written", camera_seq=102),
                            skip_read(3, 0, "slot 0 was never written: its frame is 0")]})
    # E2: a lock above 2^32 (the counter is 64 bits)
    E2 = Fixture()
    E2.put(1, 1, lock=(1 << 40) + 2)
    cases.append({"id": "pf.read.lock_is_64_bit", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_lock_2_pow_40.bin", E2), "open": "ok", "info": info_of(E2),
                  "reads": [read(E2, 1)]})
    E3 = Fixture()
    E3.put(1, 1, lock=(1 << 40) + 3)
    cases.append({"id": "pf.read.lock_odd_above_2_pow_32", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_lock_odd_2_pow_40.bin", E3), "open": "ok", "info": info_of(E3),
                  "reads": [skip_read(1, 1, "a 64-bit lock with its low bit set is odd", camera_seq=101)]})
    # F: an unknown depth_format reads as colour only, whatever depth_stride and depth_offset say
    F = Fixture(depth=False, depth_format=2, hdr_over={"depth_stride": 7, "depth_offset": 3})
    for f in (1, 2, 3):
        F.put(f % 3, f)
    cases.append({"id": "pf.open.unknown_depth_format_is_colour_only", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("unknown_depth_format_2.bin", F), "open": "ok",
                  "info": dict(info_of(F), has_depth=False),
                  "reads": [read(F, 1, True), read(F, 2, True)]})
    G = Fixture(depth=False, depth_format=0, hdr_over={"depth_stride": 123, "depth_offset": 999})
    for f in (1, 2, 3):
        G.put(f % 3, f)
    cases.append({"id": "pf.open.depth_format_0_ignores_depth_fields", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_format_0_with_junk.bin", G), "open": "ok",
                  "info": dict(info_of(G), has_depth=False), "reads": [read(G, 1, True)]})
    # H: slot header larger than the file maximum (table: "pixels used, <= file width")
    H = Fixture()
    H.put(1, 1, hdr_wh=(5, 2))
    H2 = Fixture()
    H2.put(1, 1, hdr_wh=(4, 3))
    cases.append({"id": "pf.read.slot_header_wider_than_file", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_slot_wider_than_file.bin", H), "open": "ok", "info": info_of(H),
                  "reads": [skip_read(1, 1, "the slot header says 5 pixels used, the file's maximum is 4", w=5, h=2,
                                      camera_seq=101)]})
    cases.append({"id": "pf.read.slot_header_taller_than_file", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_slot_taller_than_file.bin", H2), "open": "ok", "info": info_of(H2),
                  "reads": [skip_read(1, 1, "the slot header says 3 rows used, the file's maximum is 2", w=4, h=3,
                                      camera_seq=101)]})
    # smallest and the limits that fit in a small file
    S = Fixture(width=1, height=1, slots=1)
    S.put(0, 1)  # frame 1 goes to slot 1 % 1 == 0
    cases.append({"id": "pf.open.minimum_1x1_1slot", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_1x1_1slot.bin", S), "open": "ok", "info": info_of(S),
                  "reads": [dict(read(S, 0), width=1, height=1)]})
    W = Fixture(width=16384, height=1, slots=1, depth=False)
    W.put(0, 1, w=16384, h=1)
    cases.append({"id": "pf.open.width_16384", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("colour_16384x1.bin", W), "open": "ok", "info": info_of(W),
                  "reads": [dict(read(W, 0), rgba_sha256=hashlib.sha256(px_rgba(1, 16384, 1)).hexdigest(),
                                 rgba=None)]})
    T = Fixture(width=1, height=16384, slots=1, depth=False)
    T.put(0, 1, w=1, h=16384)
    cases.append({"id": "pf.open.height_16384", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("colour_1x16384.bin", T), "open": "ok", "info": info_of(T),
                  "reads": [dict(read(T, 0), rgba_sha256=hashlib.sha256(px_rgba(1, 1, 16384)).hexdigest(),
                                 rgba=None)]})
    M16 = Fixture(width=2, height=1, slots=16, depth=False)
    for f in range(1, 17):
        M16.put(f % 16, f, w=2, h=1)
    cases.append({"id": "pf.open.16_slots", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("colour_2x1_16slots.bin", M16), "open": "ok", "info": info_of(M16),
                  "reads": [dict(read(M16, 1), width=2, height=1), dict(read(M16, 0), width=2, height=1),
                            dict(read(M16, 15), width=2, height=1)]})

    # --- recipes: a header and a length, everything else zero (sparse where large)
    def recipe(id_, section, header, length, why, basis="MUST", expect="refused", platform=None, info=None):
        c = {"id": id_, "section": section, "basis": basis, "recipe": {"header_hex": header.hex(), "length": length},
             "open": expect, "why": why}
        if info:
            c["info"] = info
        cases.append(c)

    def hdr_len(**kw):
        h = file_header(**kw)
        slot_size = struct.unpack("<Q", h[32:40])[0]
        slots = struct.unpack("<I", h[12:16])[0]
        return h, 64 + slots * slot_size

    h, n = hdr_len()
    recipe("pf.ok.recipe_colour_exact_length", "9.6", h, n, "the file is exactly as long as its header says",
           expect="ok", info={"slots": 3, "width": 4, "height": 2, "stride": 16, "slot_size": 128, "has_depth": False})
    recipe("pf.bad.one_byte_short", "9.6", h, n - 1, "the file is at least 64 + slots * slot_size bytes")
    recipe("pf.bad.empty_file", "9.6", h, 0, "no header", )
    cases[-1]["recipe"]["header_hex"] = ""
    recipe("pf.bad.shorter_than_header", "9.6", h[:63], 63, "no complete 64 byte header")
    cases[-1]["recipe"]["header_hex"] = h[:63].hex()
    h, n = hdr_len(depth_format=1)
    recipe("pf.ok.recipe_depth_exact_length", "9.6", h, n, "depth file exactly as long as its header says",
           expect="ok", info={"slots": 3, "width": 4, "height": 2, "stride": 16, "slot_size": 192, "has_depth": True})
    recipe("pf.bad.depth_one_byte_short", "9.6", h, n - 1, "the file is at least 64 + slots * slot_size bytes")
    for s in (0, 17, 255, 0xFFFFFFFF):
        h, n = hdr_len(slots=s, slot_size=128)
        recipe("pf.bad.slots_%d" % s, "9.6", h, min(n, 1 << 20), "slots is 1 to 16")
    for w, hh_ in ((0, 2), (4, 0), (16385, 1), (1, 16385)):
        ss = rup64(64 + 4 * w * hh_)
        h = file_header(width=w, height=hh_, slot_size=ss)
        recipe("pf.bad.size_%dx%d" % (w, hh_), "9.6", h, 64 + 3 * ss, "width and height are 1 to 16384")
    for w, hh_ in ((0xFFFFFFFF, 1), (1, 0xFFFFFFFF)):
        h = file_header(width=w, height=hh_, stride=(w * 4) & 0xFFFFFFFF, slot_size=4096)
        recipe("pf.bad.size_%dx%d" % (w, hh_), "9.6", h, 64 + 3 * 4096, "width and height are 1 to 16384")
    # pixels above 2^26 with both sides within 16384: 8193 x 8192 = 67117056
    ss = rup64(64 + 8193 * 4 * 8192)
    h = file_header(slots=1, width=8193, height=8192, slot_size=ss)
    recipe("pf.bad.pixels_8193x8192", "9.6", h, 64 + ss, "width * height is at most 67108864 (the file is long enough, "
           "so only the limit can refuse it)")
    cases[-1]["large"] = True
    ss = rup64(64 + 16384 * 4 * 16384)
    h = file_header(slots=1, width=16384, height=16384, slot_size=ss)
    recipe("pf.bad.pixels_16384x16384", "9.6", h, 64 + ss, "width * height is at most 67108864 (1 GiB of colour; the "
           "file is long enough, so only the limit can refuse it)")
    cases[-1]["large"] = True
    h, n = hdr_len(slots=1, width=16384, height=4096)
    recipe("pf.ok.pixels_16384x4096_is_2_pow_26", "9.6", h, n, "exactly 67108864 pixels is allowed (sparse file)",
           expect="ok", info={"slots": 1, "width": 16384, "height": 4096, "stride": 65536,
                              "slot_size": rup64(64 + 65536 * 4096), "has_depth": False})
    cases[-1]["large"] = True
    h, n = hdr_len(slots=1, width=8192, height=8192)
    recipe("pf.ok.pixels_8192x8192", "9.6", h, n, "8192 x 8192 is allowed (sparse file)", expect="ok",
           info={"slots": 1, "width": 8192, "height": 8192, "stride": 32768, "slot_size": rup64(64 + 32768 * 8192),
                 "has_depth": False})
    cases[-1]["large"] = True
    for name, stride in (("larger", 20), ("smaller", 12), ("zero", 0), ("double", 32)):
        h = file_header(width=4, height=2, stride=stride, slot_size=128)
        recipe("pf.bad.stride_%s" % name, "9.6", h, 64 + 3 * 128, "stride is exactly width * 4")
    # slot_size that makes slots * slot_size wrap in 64 bits (revision 4)
    for tag, ss in (("2_pow_60", 1 << 60), ("2_pow_64_minus_64", 2**64 - 64), ("2_pow_63", 1 << 63)):
        h = file_header(slots=16, width=4, height=2, slot_size=ss)
        recipe("pf.bad.slot_size_%s_wraps" % tag, "9.6", h, 4096,
               "64 + slots * slot_size must be computed with overflow checks")
    h = file_header(slots=2, width=4, height=2, slot_size=(1 << 63) + 64)
    recipe("pf.bad.slot_size_2_pow_63_plus_64_x2", "9.6", h, 4096, "slots * slot_size wraps to 128 in 64 bits")
    h = file_header(slots=3, width=4, height=2, slot_size=6148914691236517312)  # 3 * x = 2^64 + 192... wraps
    recipe("pf.bad.slot_size_times_3_wraps", "9.6", h, 4096, "slots * slot_size wraps in 64 bits")
    # depth plane checks (9.6 "Compatibility and checks")
    base = dict(slots=3, width=4, height=2, depth_format=1)
    for tag, over, why in (
            ("depth_stride_not_width_x4", dict(depth_stride=20), "depth_stride is width * 4"),
            ("depth_stride_zero", dict(depth_stride=0), "depth_stride is width * 4"),
            ("depth_offset_not_multiple_of_64", dict(depth_offset=130, slot_size=256), "depth_offset is a multiple of 64"),
            ("depth_offset_inside_colour", dict(depth_offset=64, slot_size=256),
             "depth_offset is at least 64 + stride * height"),
            ("depth_offset_zero", dict(depth_offset=0, slot_size=256), "depth_offset is at least 64 + stride * height"),
            ("depth_plane_past_slot_end", dict(depth_offset=128, slot_size=128),
             "the plane ends within slot_size"),
            ("depth_offset_overflows", dict(depth_offset=2**64 - 64, slot_size=256),
             "depth_offset + depth_stride * height overflows 64 bits"),
            ("depth_offset_near_2_pow_63", dict(depth_offset=1 << 63, slot_size=256), "the plane ends within slot_size"),
            ("depth_stride_huge", dict(depth_stride=0xFFFFFFFF, slot_size=256), "depth_stride is width * 4")):
        kw = dict(base)
        kw.update(over)
        h = file_header(**kw)
        ss = struct.unpack("<Q", h[32:40])[0]
        recipe("pf.bad.%s" % tag, "9.6", h, 64 + 3 * ss if ss < (1 << 20) else 1 << 20, why)
    # fields whose value the table fixes
    for tag, over, why in (("magic", dict(magic=b"RBFRAMEX"), "magic is RBFRAME followed by a zero byte"),
                           ("magic_without_nul", dict(magic=b"RBFRAME!"), "magic is RBFRAME followed by a zero byte"),
                           ("version_2", dict(version=2), "version is 1"),
                           ("version_0", dict(version=0), "version is 1"),
                           ("format_0", dict(fmt=0), "format 1 = RGBA8"),
                           ("format_2", dict(fmt=2), "format 1 = RGBA8")):
        h = file_header(**over)
        recipe("pf.bad.%s" % tag, "9.6", h, 64 + 3 * 128, why)
    for tag, ss in (("slot_size_not_multiple_of_64", 130), ("slot_size_smaller_than_colour", 64)):
        h = file_header(slots=3, width=4, height=2, slot_size=ss)
        recipe("pf.bad.%s" % tag, "9.6", h, 64 + 3 * ss, "slot_size is a multiple of 64 and holds the slot header and "
               "the colour rows")
    # --- the path (9.6 "Opening and creating")
    for kind, why in (("relative", "the path must be absolute"), ("missing", "there is no such file"),
                      ("empty", "the path is empty"), ("directory", "a directory is not a regular file"),
                      ("symlink", "a symlink is refused"), ("fifo", "a FIFO is refused (and must not block)")):
        c = {"id": "pf.bad.path_%s" % kind, "section": "9.6", "basis": "MUST", "path_kind": kind,
             "open": "refused", "why": why}
        if kind in ("symlink",):
            c["file"] = fa
        if kind == "relative":
            c["file"] = fa
            c["note"] = "the runner opens a relative path that would name a valid copy of this file"
        cases.append(c)
    # --- the file shrinks under the reader (9.6 "Opening and creating" 4)
    cases.append({"id": "pf.read.file_truncated_after_open", "section": "9.6", "basis": "MUST", "file": fa,
                  "open": "ok", "info": info_of(A), "skip_on": ["windows"],
                  "reads": [read(A, 1)],
                  "then": [{"op": "truncate", "length": 64 + 192},
                           {"op": "read", **skip_read(2, 2, "the file is shorter than the mapping: gone")}]})

    # --- writer: the bytes a conforming writer produces (9.6)
    def writes_of(fx, n):
        out = []
        for f in range(1, n + 1):
            w, h = fx.width, fx.height
            out.append({"camera_seq": 100 + f, "time_us": str(1_700_000_000_000_000 + f * 16_666), "width": w,
                        "height": h, "rgba": px_rgba(f, w, h).hex(),
                        "depth": [jf(v) for v in px_depth(f, w, h)] if fx.depth else None})
        return out

    # frame f is written into slot f % slots, so the 4 writes of C end with slot 1 holding frame 4
    cases.append({"id": "pw.depth_3_writes", "section": "9.6", "basis": "MUST", "writer": {
        "width": 4, "height": 2, "slots": 3, "with_depth": True, "writes": writes_of(A, 3)}, "file": fa})
    cases.append({"id": "pw.colour_3_writes", "section": "9.6", "basis": "MUST", "writer": {
        "width": 4, "height": 2, "slots": 3, "with_depth": False, "writes": writes_of(B, 3)}, "file": fb})
    cases.append({"id": "pw.depth_4_writes_reuse_slot_1", "section": "9.6", "basis": "MUST", "writer": {
        "width": 4, "height": 2, "slots": 3, "with_depth": True, "writes": writes_of(C, 4)}, "file": fc})
    cases.append({"id": "pw.refuses_existing_path", "section": "9.6", "basis": "MUST", "writer": {
        "width": 4, "height": 2, "slots": 3, "with_depth": True, "writes": []}, "exists_already": True,
        "expect": "create_fails_and_file_untouched",
        "why": "a sender creates the file exclusively and never truncates what is there"})
    for tag, (w, hh_, s) in (("width_16385", (16385, 1, 3)), ("height_16385", (1, 16385, 3)), ("slots_0", (4, 2, 0)),
                             ("slots_17", (4, 2, 17)), ("width_0", (0, 2, 3)), ("pixels_8193x8192", (8193, 8192, 1))):
        cases.append({"id": "pw.refuses_%s" % tag, "section": "9.6", "basis": "MUST", "writer": {
            "width": w, "height": hh_, "slots": s, "with_depth": False, "writes": []}, "expect": "create_fails",
            "why": "a sender stays within the limits every receiver enforces"})
    # =========================== revision 5 ===========================
    # frame 0 names a slot nobody wrote (frame 0, lock 0): never delivered (9.6)
    Z = Fixture()
    Z.put(1, 1)
    cases.append({"id": "pf.read.frame_0_names_a_slot_nobody_wrote", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_one_frame_in_slot_1.bin", Z), "open": "ok", "info": info_of(Z),
                  "reads": [skip_read(0, 0, "frame 0 is malformed; slot 0 is clean (lock 0, frame 0) and would match", camera_seq=0),
                            skip_read(0, 2, "frame 0 is malformed; slot 2 is clean (lock 0, frame 0) and would match", camera_seq=0),
                            read(Z, 1)]})
    # the reader finds the slot by `slot`; it does not require slot == frame % slots (9.6, reader step 0)
    M = Fixture()
    M.put(2, 1)
    cases.append({"id": "pf.read.slot_need_not_be_frame_modulo_slots", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_frame_1_in_slot_2.bin", M), "open": "ok", "info": info_of(M),
                  "reads": [read(M, 2), skip_read(1, 1, "slot 1 was never written", camera_seq=101)]})
    # the datagram's size must equal the slot header's (9.6, reader step 2)
    D2 = Fixture()
    D2.put(1, 1)
    cases.append({"id": "pf.read.datagram_size_differs_from_the_slot_header", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_one_frame_in_slot_1_again.bin", D2), "open": "ok", "info": info_of(D2),
                  "reads": [skip_read(1, 1, "the datagram says 3 pixels wide, the slot header says 4", w=3, h=2, camera_seq=101),
                            skip_read(1, 1, "the datagram says 1 row, the slot header says 2", w=4, h=1, camera_seq=101),
                            skip_read(1, 1, "the datagram says 5 x 3, above the file's maximum", w=5, h=3, camera_seq=101),
                            skip_read(1, 1, "the datagram says 0 x 0", w=0, h=0, camera_seq=101),
                            read(D2, 1)]})
    # the slot header's size is 1 .. the file's (9.6, reader step 2)
    H0 = Fixture()
    H0.put(1, 1, hdr_wh=(0, 2))
    cases.append({"id": "pf.read.slot_header_width_0", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_slot_width_0.bin", H0), "open": "ok", "info": info_of(H0),
                  "reads": [skip_read(1, 1, "the slot header says 0 pixels wide", w=0, h=2, camera_seq=101)]})
    H1 = Fixture()
    H1.put(1, 1, hdr_wh=(4, 0))
    cases.append({"id": "pf.read.slot_header_height_0", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_slot_height_0.bin", H1), "open": "ok", "info": info_of(H1),
                  "reads": [skip_read(1, 1, "the slot header says 0 rows", w=4, h=0, camera_seq=101)]})
    H3 = Fixture()
    H3.put(1, 1, hdr_wh=(0, 0))
    cases.append({"id": "pf.read.slot_header_size_0_x_0", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_slot_size_0.bin", H3), "open": "ok", "info": info_of(H3),
                  "reads": [skip_read(1, 1, "the slot header says 0 x 0 and so does the datagram", w=0, h=0, camera_seq=101)]})
    # reserved bytes are ignored on read (9.6 Header checks)
    R1 = Fixture(hdr_over={"reserved": b"\xff" * 8})
    R1.put(1, 1)
    cases.append({"id": "pf.read.file_header_reserved_bytes_ignored_depth", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_file_reserved_ff.bin", R1), "open": "ok", "info": info_of(R1),
                  "reads": [read(R1, 1)]})
    R1b = Fixture(depth=False, hdr_over={"reserved": b"\xff" * 8})
    R1b.put(1, 1)
    cases.append({"id": "pf.read.file_header_reserved_bytes_ignored_colour", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("colour_file_reserved_ff.bin", R1b), "open": "ok", "info": info_of(R1b),
                  "reads": [read(R1b, 1)]})
    R2 = Fixture()
    R2.put(1, 1)
    sd = bytearray(R2.slots_data[1])
    sd[36:64] = b"\xff" * 28
    R2.slots_data[1] = bytes(sd)
    cases.append({"id": "pf.read.slot_header_reserved_bytes_ignored", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("depth_slot_reserved_ff.bin", R2), "open": "ok", "info": info_of(R2),
                  "reads": [read(R2, 1)]})
    R3 = Fixture(depth=False, depth_format=7, hdr_over={"reserved": b"\xff" * 8, "depth_stride": 1, "depth_offset": 2})
    R3.put(1, 1)
    cases.append({"id": "pf.read.unknown_depth_format_with_reserved_bytes_set", "section": "9.6", "basis": "MUST",
                  "file": fixture_file("unknown_depth_format_7_reserved_ff.bin", R3), "open": "ok",
                  "info": dict(info_of(R3), has_depth=False), "reads": [read(R3, 1, False)]})

    # writers: a refused write changes nothing (9.6)
    def writes_for(fx, n):
        out = []
        for f in range(1, n + 1):
            w, h = fx.width, fx.height
            out.append({"camera_seq": 100 + f, "time_us": str(1_700_000_000_000_000 + f * 16_666), "width": w,
                        "height": h, "rgba": px_rgba(f, w, h).hex(),
                        "depth": [jf(v) for v in px_depth(f, w, h)] if fx.depth else None})
        return out

    def refused(width, height, rgba=b"", depth=None, why=""):
        return {"expect": "refused", "why": why, "camera_seq": 99, "time_us": "99", "width": width, "height": height,
                "rgba": rgba.hex(), "depth": None if depth is None else [jf(v) for v in depth]}

    wc = writes_for(B, 3)
    cases.append({"id": "pw.colour_refused_writes_change_nothing", "section": "9.6", "basis": "MUST", "writer": {
        "width": 4, "height": 2, "slots": 3, "with_depth": False, "writes": [wc[0]] + [
            refused(5, 2, bytes(40), why="wider than the file"), refused(4, 3, bytes(48), why="taller than the file"),
            refused(0, 2, why="width 0"), refused(4, 0, why="height 0"), refused(0, 0, why="0 x 0"),
            refused(5, 3, bytes(60), why="both above the file's"),
            refused(4, 2, bytes(31), why="one byte short"), refused(4, 2, bytes(33), why="one byte over"),
            refused(4, 2, bytes(32), depth=[1.0] * 8, why="depth for a file without a depth plane")] + wc[1:]},
        "file": fb})
    wd = writes_for(A, 3)
    cases.append({"id": "pw.depth_refused_writes_change_nothing", "section": "9.6", "basis": "MUST", "writer": {
        "width": 4, "height": 2, "slots": 3, "with_depth": True, "writes": [wd[0]] + [
            refused(5, 2, bytes(40), depth=[1.0] * 10, why="wider than the file"),
            refused(4, 3, bytes(48), depth=[1.0] * 12, why="taller than the file"),
            refused(0, 2, depth=[], why="width 0"), refused(4, 0, depth=[], why="height 0"),
            refused(4, 2, bytes(31), depth=[1.0] * 8, why="colour one byte short"),
            refused(4, 2, bytes(32), depth=[1.0] * 7, why="depth one value short"),
            refused(4, 2, bytes(32), depth=[1.0] * 9, why="depth one value over")] + wd[1:]},
        "file": fa})
    for name, data in files.items():
        with open(os.path.join(outdir, "pixelfiles", name), "wb") as f:
            f.write(data)
    return cases, files


# ---------------------------------------------------------------------------------------------
# sessions.json

def sessions():
    cases = []
    PORT = "@PORT@"

    def jline(d):
        s = json.dumps(d, separators=(",", ":"), ensure_ascii=True)
        return s.replace('"@PORT@"', "${peer_udp_port}") + "\n"

    def hello(role="guest", game="vector peer", protocol=1, min_protocol=1, udp_port=PORT, heartbeat_ms=250,
              timeout_ms=2000, session=None, drop=(), **extra):
        d = {"type": "hello", "role": role, "game": game, "protocol": protocol, "min_protocol": min_protocol,
             "udp_port": udp_port, "heartbeat_ms": heartbeat_ms, "timeout_ms": timeout_ms}
        if session is not None:
            d["session"] = session
        for k in drop:
            d.pop(k, None)
        d.update(extra)
        return jline(d)

    SESSION = 2882400001

    def case(id_, section, role, steps, config=None, slow=False, basis="MUST", note=None):
        c = {"id": id_, "section": section, "role_under_test": role, "basis": basis}
        if config:
            c["config"] = config
        if slow:
            c["slow"] = True
        if note:
            c["note"] = note
        c["steps"] = steps
        cases.append(c)

    # ---- helpers for steps
    def send(text, conn=None):
        s = {"op": "send_line", "text": text}
        if conn:
            s["conn"] = conn
        return s

    def expect_line(match, capture=None, conn=None, exact=False):
        s = {"op": "expect_line", "match": match}
        if exact:
            s["exact"] = True
        if capture:
            s["capture"] = capture
        if conn:
            s["conn"] = conn
        return s

    def closed(conn=None, allow_lines=False):
        s = {"op": "expect_closed"}
        if allow_lines:
            s["allow_lines"] = True
        if conn:
            s["conn"] = conn
        return s

    def ev(event, match=None, count=None):
        s = {"op": "expect_event", "event": event}
        if match is not None:
            s["match"] = match
        if count is not None:
            s["count"] = count
        return s

    def noev(event, ms=300):
        return {"op": "expect_no_event", "event": event, "ms": ms}

    host_reply = {"type": "hello", "role": "host", "protocol": 1, "min_protocol": 1, "udp_port": "$u16nz",
                  "heartbeat_ms": "$hb", "timeout_ms": "$int", "session": "$u32nz", "game": "$string"}
    HS = [{"op": "connect"}, send(hello()),
          expect_line(host_reply, capture={"session": "session", "ut_udp_port": "udp_port"}),
          ev("connected", {"role": "guest", "protocol": 1})]
    spawn_line = lambda e: '{"type":"spawn","entity":%d,"provider":"P","asset":"a"}\n' % e
    bye_closed = [expect_line({"type": "bye", "reason": "$string"}), closed()]

    # ======================= host under test =======================
    case("sess.host.handshake.reply", "6", "host", HS + [
        {"op": "expect_no_line", "ms": 200}], note="the reply carries the chosen version, a nonzero session and the "
                                                    "host's own udp_port and heartbeat_ms in 10 ..= 10000")
    for hb in (10, 10000):
        case("sess.host.handshake.heartbeat_%d_accepted" % hb, "8", "host", [
            {"op": "connect"}, send(hello(heartbeat_ms=hb)), expect_line(host_reply), ev("connected")])
    for hb in (0, 1, 9, 10001, 4294967295):
        case("sess.host.handshake.heartbeat_%d_refused" % hb, "8", "host", [
            {"op": "connect"}, send(hello(heartbeat_ms=hb))] + bye_closed + [ev("handshake_failed")])
    for role in ("host", "observer", "Guest"):
        case("sess.host.handshake.role_%s_refused" % role, "6", "host", [
            {"op": "connect"}, send(hello(role=role))] + bye_closed + [ev("handshake_failed")])
    for p, mp in ((2, 1), (7, 1), (1, 0)):
        case("sess.host.handshake.guest_range_%d_%d_accepted" % (mp, p), "6", "host", [
            {"op": "connect"}, send(hello(protocol=p, min_protocol=mp)),
            expect_line(dict(host_reply, protocol=1)), ev("connected", {"protocol": 1})])
    for p, mp in ((2, 2), (0, 0), (5, 3), (4294967295, 4294967295)):
        case("sess.host.handshake.guest_range_%d_%d_refused" % (mp, p), "6", "host", [
            {"op": "connect"}, send(hello(protocol=p, min_protocol=mp))] + bye_closed + [ev("handshake_failed")])
    case("sess.host.handshake.not_json", "6, 8", "host", [{"op": "connect"}, send("this is not json\n")] + bye_closed
         + [ev("handshake_failed")])
    case("sess.host.handshake.missing_field", "8", "host", [
        {"op": "connect"}, send(hello(drop=("udp_port",)))] + bye_closed + [ev("handshake_failed")])
    case("sess.host.handshake.wrong_type_field", "8", "host", [
        {"op": "connect"}, send(hello(heartbeat_ms="250"))] + bye_closed + [ev("handshake_failed")])
    case("sess.host.handshake.crlf_hello", "8", "host", [
        {"op": "connect"}, send(hello().replace("\n", "\r\n")), expect_line(host_reply), ev("connected")])
    case("sess.host.handshake.hello_in_two_writes", "8", "host", [
        {"op": "connect"}, {"op": "send_hex", "hex": b'{"type":"hello","role":"gu'.hex()}, {"op": "sleep", "ms": 60},
        {"op": "send_line", "text": 'est","game":"g","protocol":1,"min_protocol":1,"udp_port":${peer_udp_port},'
                                    '"heartbeat_ms":250,"timeout_ms":2000}\n'},
        expect_line(host_reply), ev("connected")])
    case("sess.host.handshake.unknown_field_in_hello", "8", "host", [
        {"op": "connect"}, send(hello(future={"a": [1, 2]})), expect_line(host_reply), ev("connected")])
    case("sess.host.handshake.busy", "2", "host", HS + [
        {"op": "connect", "conn": "b"},
        expect_line({"type": "bye", "reason": "busy"}, conn="b"), closed("b"),
        send(spawn_line(5)), ev("spawn", {"entity": 5}), {"op": "expect_no_line", "ms": 200}],
         note="the second connection gets bye/busy without having sent anything; the first session is unharmed")
    case("sess.host.handshake.no_hello_before_timeout", "6", "host", [
        {"op": "connect"}, closed(allow_lines=True)], config={"timeout_ms": 300, "heartbeat_ms": 100},
         note="a side that has not received the peer's hello within its own timeout_ms closes the connection")
    case("sess.host.handshake.new_guest_after_bye", "6", "host", HS + [
        send('{"type":"bye","reason":"first"}\n'), closed(), ev("lost"),
        {"op": "connect", "conn": "b"}, send(hello(), conn="b"),
        expect_line(host_reply, conn="b", capture={"session": "session"}), ev("connected")],
         note="after the session ends the host listens for a new guest")

    # --- control channel, established session
    case("sess.host.control.unknown_type_ignored", "8", "host", HS + [
        send('{"type":"future_thing","x":[1,2,3]}\n'), send(spawn_line(1)), ev("spawn", {"entity": 1}),
        {"op": "expect_no_line", "ms": 200}])
    case("sess.host.control.unknown_field_ignored", "8", "host", HS + [
        send('{"type":"spawn","entity":2,"provider":"P","asset":"a","future":{"k":1}}\n'),
        ev("spawn", {"entity": 2, "provider": "P", "asset": "a"})])
    case("sess.host.control.crlf_tolerated", "8", "host", HS + [
        send('{"type":"spawn","entity":3,"provider":"P","asset":"a","name":"n"}\r\n'),
        ev("spawn", {"entity": 3, "name": "n"})])
    case("sess.host.control.two_lines_one_write", "8", "host", HS + [
        send(spawn_line(1) + spawn_line(2)), ev("spawn", {"entity": 1}), ev("spawn", {"entity": 2})])
    case("sess.host.control.line_split_across_writes", "8", "host", HS + [
        {"op": "send_hex", "hex": b'{"type":"spawn","enti'.hex()}, {"op": "sleep", "ms": 60},
        {"op": "send_hex", "hex": b'ty":9,"provider":"P","asset":"a"}\n'.hex()}, ev("spawn", {"entity": 9})])
    case("sess.host.control.unicode_name", "8", "host", HS + [
        send('{"type":"spawn","entity":4,"provider":"P","asset":"a","name":"Caf\\u00e9 \\ud83d\\ude00"}\n'),
        ev("spawn", {"entity": 4, "name": "Caf\u00e9 \U0001f600"})])
    case("sess.host.control.garbage_line_ends_session", "8", "host", HS + [
        send("garbage that is not json\n")] + bye_closed + [ev("lost")])
    case("sess.host.control.json_array_ends_session", "8", "host", HS + [send("[1,2]\n")] + bye_closed + [ev("lost")])
    case("sess.host.control.missing_field_ends_session", "8", "host", HS + [
        send('{"type":"spawn","entity":1,"provider":"P"}\n')] + bye_closed + [ev("lost")])
    case("sess.host.control.entity_above_u32_ends_session", "8", "host", HS + [
        send('{"type":"despawn","entity":4294967296}\n')] + bye_closed + [ev("lost")])
    case("sess.host.control.type_not_string_ends_session", "8", "host", HS + [
        send('{"type":7}\n')] + bye_closed + [ev("lost")])
    case("sess.host.control.invalid_utf8_ends_session", "8", "host", HS + [
        {"op": "send_hex", "hex": b'{"type":"bye","reason":"\xff"}\n'.hex()}] + bye_closed + [ev("lost")])
    pad_total = 65536
    pre, suf = '{"type":"pad","p":"', '"}\n'
    case("sess.host.control.line_of_65536_bytes_accepted", "8", "host", HS + [
        {"op": "send_fill", "prefix": pre, "fill": "a", "count": pad_total - len(pre) - len(suf), "suffix": suf},
        send(spawn_line(1)), ev("spawn", {"entity": 1}), {"op": "expect_no_line", "ms": 200}],
         note="65536 bytes including the newline is the longest allowed line")
    case("sess.host.control.line_of_65537_bytes_ends_session", "8", "host", HS + [
        {"op": "send_fill", "prefix": pre, "fill": "a", "count": pad_total + 1 - len(pre) - len(suf), "suffix": suf}]
         + bye_closed + [ev("lost")])
    case("sess.host.control.65536_bytes_without_newline_ends_session", "8", "host", HS + [
        {"op": "send_fill", "prefix": pre, "fill": "a", "count": pad_total - len(pre), "suffix": ""}]
         + bye_closed + [ev("lost")],
         note="no line can complete within 65536 bytes any more: its newline would be byte 65537")
    case("sess.host.control.bye_from_peer", "6.1", "host", HS + [
        send('{"type":"bye","reason":"going"}\n'), ev("lost"),
        {"op": "expect_closed"}], note="a received bye is answered by closing, without a bye of its own (expect_closed "
                                       "fails if a line arrives first)")
    case("sess.host.control.peer_closes_without_bye", "7", "host", HS + [{"op": "close"}, ev("lost")])
    case("sess.host.control.peer_half_close_without_bye", "7", "host", HS + [{"op": "close", "half": True}, ev("lost")],
         note="a read of 0 bytes ends the session")
    case("sess.host.control.input_request_from_guest", "8", "host", HS + [
        send('{"type":"input","focus":"guest","reason":"want it"}\n'), ev("focus", {"focus": "guest", "requested": True}),
        send('{"type":"input","focus":"host"}\n'), ev("focus", {"focus": "host", "requested": True})])
    case("sess.host.control.frame_channel_event", "8", "host", HS + [
        send('{"type":"frame_channel","path":"/tmp/x-1-guest.frames","width":4,"height":2,"slots":3}\n'),
        ev("frame_channel", {"path": "/tmp/x-1-guest.frames", "width": 4, "height": 2, "slots": 3})])
    case("sess.host.control.despawn_event", "8", "host", HS + [
        send(spawn_line(7)), ev("spawn", {"entity": 7}), send('{"type":"despawn","entity":7}\n'),
        ev("despawn", {"entity": 7})])
    case("sess.host.control.spawn_again_replaces", "8", "host", HS + [
        send(spawn_line(7)), send('{"type":"spawn","entity":7,"provider":"Q","asset":"b"}\n'),
        ev("spawn", {"entity": 7, "provider": "P"}), ev("spawn", {"entity": 7, "provider": "Q"})])
    case("sess.host.control.entity_limit_1024", "8", "host", HS + [
        {"op": "send_range", "template": '{"type":"spawn","entity":${i},"provider":"P","asset":"a"}\n', "from": 1,
         "to": 1024},
        ev("spawn", None, 1024),
        send(spawn_line(1025)), noev("spawn", 400),
        send(spawn_line(1)), ev("spawn", {"entity": 1}),
        send('{"type":"despawn","entity":2}\n'), ev("despawn", {"entity": 2}),
        send(spawn_line(1025)), ev("spawn", {"entity": 1025}),
        {"op": "expect_no_line", "ms": 100}],
         note="1024 live entities at most: a 1025th id is ignored, a live id is replaced, a despawn frees a place")

    case("sess.host.control.focus_returns_to_host_when_peer_is_lost", "7, 8", "host", HS + [
        {"op": "expect_focus", "focus": "host"},
        {"op": "ut", "do": "set_focus", "focus": "guest"}, expect_line({"type": "input", "focus": "guest"}),
        {"op": "expect_focus", "focus": "guest"},
        {"op": "close"}, ev("lost"), {"op": "expect_focus", "focus": "host"}],
         note="the host is the authority and has the focus when a session starts; it gets it back when the peer is lost")

    # ---- UDP into the host
    def dg(body_hex_after_header, kind, session="{session_le}", seq=1, version=1, magic=0x4B4C4252, time_us=1000):
        return (struct.pack("<I", magic) + struct.pack("<HBB", version, kind, 0)).hex() + session + \
            struct.pack("<IQ", seq, time_us).hex() + body_hex_after_header

    cam = encode_body(camera_body()).hex()

    def sendd(hexs, frm=None):
        s = {"op": "send_datagram", "hex": hexs}
        if frm:
            s["from"] = frm
        return s

    cam_ev = {"viewport_width": 1280, "viewport_height": 720, "depth_range": 0}

    # ---- watchdog (7)
    wd = {"timeout_ms": 400, "heartbeat_ms": 100}
    HSW = [{"op": "connect"}, send(hello(heartbeat_ms=10)),
           expect_line(host_reply, capture={"session": "session"}), ev("connected")]
    case("sess.host.watchdog.silence_ends_session_with_bye", "6.1, 7", "host", HSW + [
        ev("lost")] + bye_closed, config=wd, slow=True,
         note="nothing heard for longer than the timeout: the peer is lost, and the closing side says bye (6.1)")
    case("sess.host.watchdog.heartbeats_keep_session_alive", "7", "host", HSW + [
        s for _ in range(15) for s in (sendd(dg("", 1, seq=1)), {"op": "sleep", "ms": 100})] + [
        noev("lost", 10), {"op": "sleep", "ms": 10}, ev("lost")], config=wd, slow=True)
    case("sess.host.watchdog.control_lines_keep_session_alive", "7", "host", HSW + [
        s for _ in range(15) for s in (send('{"type":"future_thing"}\n'), {"op": "sleep", "ms": 100})] + [
        noev("lost", 10), ev("lost")], config=wd, slow=True,
         note="the last valid datagram or control line counts")
    case("sess.host.watchdog.timeout_is_at_least_four_peer_heartbeats", "7", "host", [
        {"op": "connect"}, send(hello(heartbeat_ms=600)),
        expect_line(host_reply, capture={"session": "session"}), ev("connected"),
        {"op": "sleep", "ms": 1500}, noev("lost", 10), ev("lost")], config=wd, slow=True,
         note="the timeout is max(own timeout_ms, 4 * peer heartbeat_ms) = 2400 ms here, not the host's own 400 ms")

    case("sess.host.udp.camera_arrives", "9.3", "host", HS + [
        sendd(dg(cam, 2, seq=1)), ev("camera", dict(cam_ev, seq=1))])
    case("sess.host.udp.wrong_session_dropped", "5, 6", "host", HS + [
        sendd(dg(cam, 2, session="{session_le_plus1}")), noev("camera", 300),
        sendd(dg(cam, 2, seq=2)), ev("camera", {"seq": 2})])
    case("sess.host.udp.wrong_version_dropped", "6, 9.1", "host", HS + [
        sendd(dg(cam, 2, version=2)), noev("camera", 300), sendd(dg(cam, 2, seq=2)), ev("camera", {"seq": 2})])
    case("sess.host.udp.wrong_magic_dropped", "9.1", "host", HS + [
        sendd(dg(cam, 2, magic=0x584C4252)), noev("camera", 300), sendd(dg(cam, 2, seq=2)), ev("camera", {"seq": 2})])
    case("sess.host.udp.truncated_camera_dropped", "9", "host", HS + [
        sendd(dg(cam, 2)[:-8]), noev("camera", 300), sendd(dg(cam, 2, seq=2)), ev("camera", {"seq": 2})])
    case("sess.host.udp.camera_with_extra_byte_dropped", "9.3", "host", HS + [
        sendd(dg(cam, 2) + "00"), noev("camera", 300), sendd(dg(cam, 2, seq=2)), ev("camera", {"seq": 2})])
    case("sess.host.udp.bad_depth_range_dropped", "9.3", "host", HS + [
        sendd(dg(cam[:-8] + "03000000", 2)), noev("camera", 300), sendd(dg(cam, 2, seq=2)), ev("camera", {"seq": 2})])
    case("sess.host.udp.shorter_than_header_dropped", "9", "host", HS + [
        sendd("52424c4b0100"), sendd(""), noev("camera", 200), sendd(dg(cam, 2, seq=2)), ev("camera", {"seq": 2})])
    case("sess.host.udp.from_other_address_dropped", "2", "host", HS + [
        sendd(dg(cam, 2, seq=1), frm="other"), noev("camera", 300), sendd(dg(cam, 2, seq=2)), ev("camera", {"seq": 2})],
         note="only datagrams from the exact address announced in hello are accepted")
    case("sess.host.udp.camera_older_or_equal_seq_dropped", "7", "host", HS + [
        sendd(dg(cam, 2, seq=10)), ev("camera", {"seq": 10}),
        sendd(dg(cam, 2, seq=10)), sendd(dg(cam, 2, seq=9)), sendd(dg(cam, 2, seq=5)), noev("camera", 300),
        sendd(dg(cam, 2, seq=11)), ev("camera", {"seq": 11})])
    case("sess.host.udp.camera_seq_wraps", "7", "host", HS + [
        sendd(dg(cam, 2, seq=4294967294)), ev("camera", {"seq": 4294967294}),
        sendd(dg(cam, 2, seq=4294967295)), ev("camera", {"seq": 4294967295}),
        sendd(dg(cam, 2, seq=0)), ev("camera", {"seq": 0}),
        sendd(dg(cam, 2, seq=4294967295)), noev("camera", 300)],
         note="RFC 1982 serial arithmetic on seq: 0 is newer than 4294967295")
    case("sess.host.udp.unknown_kind_ignored", "9.1", "host", HS + [
        sendd(dg("00" * 16, 9)), sendd(dg("", 0)), sendd(dg("", 255)), {"op": "expect_no_line", "ms": 200},
        sendd(dg(cam, 2, seq=2)), ev("camera", {"seq": 2})])

    def pose_hex(entities, time_us=1_700_000_000_000_000):
        recs = [pose_record(e, 0, time_us, [1.0, 2.0, 3.0], [0.0, 0.0, 0.0, 1.0], [1.0, 1.0, 1.0], [0.0, 0.0, 0.0],
                            [0.0, 0.0, 0.0]) for e in entities]
        return encode_body({"kind": "pose", "poses": recs}).hex()

    def pose_t(entity, t):
        return encode_body({"kind": "pose", "poses": [pose_record(entity, 0, t, [0.0] * 3, [0.0, 0.0, 0.0, 1.0],
                                                                  [1.0] * 3, [0.0] * 3, [0.0] * 3)]}).hex()

    case("sess.host.udp.pose_of_unspawned_entity_ignored", "8, 9.4", "host", HS + [
        sendd(dg(pose_hex([99]), 3)), noev("poses", 300),
        send(spawn_line(99)), ev("spawn", {"entity": 99}),
        sendd(dg(pose_hex([99]), 3, seq=2)), ev("poses", {"count": 1, "entities": [99]}),
        send('{"type":"despawn","entity":99}\n'), ev("despawn", {"entity": 99}),
        sendd(dg(pose_hex([99]), 3, seq=3)), noev("poses", 300)])
    case("sess.host.udp.pose_record_mix_keeps_spawned", "8, 9.4", "host", HS + [
        send(spawn_line(5)), ev("spawn"), sendd(dg(pose_hex([6, 5, 7]), 3)),
        ev("poses", {"count": 1, "entities": [5]})])
    case("sess.host.udp.pose_older_time_dropped", "7", "host", HS + [
        send(spawn_line(5)), ev("spawn"),
        sendd(dg(pose_t(5, 1000), 3, seq=1)), ev("poses", {"count": 1}),
        sendd(dg(pose_t(5, 999), 3, seq=2)), noev("poses", 300),
        sendd(dg(pose_t(5, 1000), 3, seq=3)), ev("poses", {"count": 1}),
        sendd(dg(pose_t(5, 1001), 3, seq=4)), ev("poses", {"count": 1})], basis="derived",
         note="'older than the last one used' is dropped; an equal time is not older")
    case("sess.host.udp.pose_count_mismatch_dropped", "9.4", "host", HS + [
        send(spawn_line(5)), ev("spawn"), sendd(dg(pose_hex([5, 5]), 3)[:-160]), noev("poses", 300),
        sendd(dg(pose_hex([5]), 3, seq=2)), ev("poses", {"count": 1})])

    def fr(frame, slot=0, cam=0):
        return encode_body(frame_body(frame=frame, slot=slot, camera_seq=cam, width=4, height=2)).hex()

    case("sess.host.udp.frame_staleness_and_channel_reset", "7, 8", "host", HS + [
        sendd(dg(fr(5), 5, seq=1)), ev("frame", {"frame": "5"}),
        sendd(dg(fr(5), 5, seq=2)), sendd(dg(fr(4), 5, seq=3)), noev("frame", 300),
        sendd(dg(fr(6), 5, seq=4)), ev("frame", {"frame": "6"}),
        send('{"type":"frame_channel","path":"/tmp/x-2-guest.frames","width":4,"height":2,"slots":3}\n'),
        ev("frame_channel"), sendd(dg(fr(1), 5, seq=5)), ev("frame", {"frame": "1"})],
         note="a frame must be newer than the newest shown from the current pixel file; a new frame_channel resets that")
    case("sess.host.udp.frame_fields", "9.6", "host", HS + [
        sendd(dg(encode_body(frame_body(frame=9, slot=0, camera_seq=77, width=320, height=240)).hex(), 5)),
        ev("frame", {"frame": "9", "slot": 0, "camera_seq": 77, "width": 320, "height": 240})])

    def coll(query, index, count, tris, center=None, radius=None, first_tri=0):
        b = collision_body(query=query, index=index, count=count, tris=0)
        if center is not None:
            b["center"] = list(center)
        if radius is not None:
            b["radius"] = radius
        b["triangles"] = [tri_at(first_tri + i) for i in range(tris)]
        return encode_body(b).hex()

    case("sess.host.udp.collision_reassembled_out_of_order", "7, 9.5", "host", HS + [
        sendd(dg(coll(1, 1, 3, 2), 4, seq=1)), sendd(dg(coll(1, 2, 3, 5), 4, seq=2)), noev("collision", 200),
        sendd(dg(coll(1, 0, 3, 3), 4, seq=3)), ev("collision", {"query": 1, "triangles": 10})],
         note="the collision set is replaced only once every chunk of a newer query has arrived")
    case("sess.host.udp.collision_empty_query", "9.5", "host", HS + [
        sendd(dg(coll(2, 0, 1, 0), 4)), ev("collision", {"query": 2, "triangles": 0})])
    case("sess.host.udp.collision_newer_query_discards_incomplete", "7", "host", HS + [
        sendd(dg(coll(2, 0, 2, 1), 4, seq=1)), sendd(dg(coll(3, 0, 1, 4), 4, seq=2)),
        ev("collision", {"query": 3, "triangles": 4}),
        sendd(dg(coll(2, 1, 2, 1), 4, seq=3)), noev("collision", 300)])
    case("sess.host.udp.collision_older_query_dropped", "7", "host", HS + [
        sendd(dg(coll(5, 0, 1, 2), 4, seq=1)), ev("collision", {"query": 5, "triangles": 2}),
        sendd(dg(coll(4, 0, 1, 2), 4, seq=2)), noev("collision", 300),
        sendd(dg(coll(5, 0, 1, 2), 4, seq=3)), noev("collision", 100),
        sendd(dg(coll(6, 0, 1, 1), 4, seq=4)), ev("collision", {"query": 6, "triangles": 1})])
    case("sess.host.udp.collision_malformed_chunk_dropped", "9.5", "host", HS + [
        sendd(dg(coll(1, 3, 3, 1), 4, seq=1)), noev("collision", 300),
        sendd(dg(coll(1, 0, 1, 1), 4, seq=2)), ev("collision", {"query": 1, "triangles": 1})])
    case("sess.host.udp.malformed_datagrams_do_not_end_session", "9", "host", HS + [
        sendd(dg("00" * n, 2, seq=n)) for n in (0, 1, 3, 151, 153, 400)] + [
        sendd(dg(cam, 2, seq=100)), ev("camera", {"seq": 100}), {"op": "expect_no_line", "ms": 200}])
    case("sess.host.udp.unknown_kind_is_proof_of_life", "7, 9.1", "host", [
        {"op": "connect"}, send(hello(heartbeat_ms=10, timeout_ms=400)),
        expect_line(host_reply, capture={"session": "session"}), ev("connected")] + [
        s for _ in range(18) for s in (sendd(dg("00" * 8, 9, seq=1)), {"op": "sleep", "ms": 100})] + [
        noev("lost", 10), ev("lost")],
         config={"timeout_ms": 1000, "heartbeat_ms": 100}, slow=True,
         note="for 1.8 s only unknown-kind datagrams arrive (the watchdog is 1000 ms): the session lives; then they "
              "stop and the session is lost")
    case("sess.host.udp.foreign_session_is_not_proof_of_life", "7", "host", [
        {"op": "connect"}, send(hello(heartbeat_ms=10, timeout_ms=400)),
        expect_line(host_reply, capture={"session": "session"}), ev("connected")] + [
        s for _ in range(12) for s in (sendd(dg("00" * 8, 9, session="{session_le_plus1}", seq=1)),
                                       {"op": "sleep", "ms": 100})] + [ev("lost")],
         config={"timeout_ms": 1000, "heartbeat_ms": 100}, slow=True,
         note="datagrams with another session do not feed the watchdog")

    # ---- host sends
    case("sess.host.sends.heartbeats", "7", "host", HS + [
        {"op": "expect_datagram", "kind": 1, "count": 4, "timeout_ms": 2500}],
         config={"heartbeat_ms": 50}, note="a heartbeat every heartbeat_ms, from the udp_port in hello, with the "
                                           "session and version")
    case("sess.host.sends.camera_datagram", "9.3", "host", HS + [
        {"op": "ut", "do": "send_camera", "like": "dg.camera.depth_range_0"},
        {"op": "expect_datagram", "like": "dg.camera.depth_range_0", "timeout_ms": 2000}])
    case("sess.host.sends.camera_infinite_far", "9.3", "host", HS + [
        {"op": "ut", "do": "send_camera", "like": "dg.camera.infinite_far"},
        {"op": "expect_datagram", "like": "dg.camera.infinite_far", "timeout_ms": 2000}])
    case("sess.host.sends.pose_datagram", "9.4", "host", HS + [
        {"op": "ut", "do": "spawn", "entity": 1001, "provider": "P", "asset": "a", "name": None},
        expect_line({"type": "spawn", "entity": 1001, "provider": "P", "asset": "a"}),
        {"op": "ut", "do": "spawn", "entity": 1002, "provider": "P", "asset": "a", "name": None},
        expect_line({"type": "spawn", "entity": 1002, "provider": "P", "asset": "a"}),
        {"op": "ut", "do": "send_poses", "like": "dg.pose.two_teleport"},
        {"op": "expect_datagram", "like": "dg.pose.two_teleport", "timeout_ms": 2000}])
    case("sess.host.sends.frame_datagram", "9.6", "host", HS + [
        {"op": "ut", "do": "send_frame", "like": "dg.frame.basic"},
        {"op": "expect_datagram", "like": "dg.frame.basic", "timeout_ms": 2000}])
    case("sess.host.sends.seq_increments_per_datagram", "9.1", "host", HS + [
        {"op": "ut", "do": "send_camera", "like": "dg.camera.depth_range_0"},
        {"op": "ut", "do": "send_camera", "like": "dg.camera.depth_range_1"},
        {"op": "ut", "do": "send_camera", "like": "dg.camera.depth_range_2"},
        {"op": "expect_datagram", "kind": 2, "count": 3, "timeout_ms": 2000, "seq_consecutive": True}],
         config={"heartbeat_ms": 10000}, note="seq is +1 per datagram, any kind; with a 10 s heartbeat only the "
                                               "three cameras are sent in this window")
    case("sess.host.sends.control_lines", "8", "host", HS + [
        {"op": "ut", "do": "spawn", "entity": 7, "provider": "Warframe", "asset": "/Lotus/Characters/Excalibur",
         "name": "Excalibur"},
        expect_line({"type": "spawn", "entity": 7, "provider": "Warframe", "asset": "/Lotus/Characters/Excalibur",
                     "name": "Excalibur"}),
        {"op": "ut", "do": "spawn", "entity": 8, "provider": "P", "asset": "a", "name": None},
        expect_line({"type": "spawn", "entity": 8, "provider": "P", "asset": "a"}),
        {"op": "ut", "do": "despawn", "entity": 7}, expect_line({"type": "despawn", "entity": 7}),
        {"op": "ut", "do": "set_focus", "focus": "guest"}, expect_line({"type": "input", "focus": "guest"}),
        {"op": "ut", "do": "announce_frames", "path": "/tmp/rbl-1-host.frames", "width": 1280, "height": 720, "slots": 3},
        expect_line({"type": "frame_channel", "path": "/tmp/rbl-1-host.frames", "width": 1280, "height": 720,
                     "slots": 3}),
        {"op": "expect_no_line", "ms": 100}])
    case("sess.host.sends.spawn_line_has_no_null_fields", "8", "host", HS + [
        {"op": "ut", "do": "spawn", "entity": 8, "provider": "P", "asset": "a", "name": None},
        expect_line({"type": "spawn", "entity": 8, "provider": "P", "asset": "a"}, exact=True)], basis="derived",
         note="an absent optional field is omitted, not null: null is not a string")
    case("sess.host.sends.bye_on_disconnect", "6.1", "host", HS + [
        {"op": "ut", "do": "disconnect", "reason": "shutting down"},
        expect_line({"type": "bye", "reason": "shutting down"}), closed()],
         note="bye, half-close, drain: the peer sees the bye and then end of stream")
    case("sess.host.sends.eof_after_bye_even_if_peer_keeps_sending", "6.1", "host", HS + [
        {"op": "ut", "do": "disconnect", "reason": "x"}, expect_line({"type": "bye", "reason": "x"}),
        send(spawn_line(1)), {"op": "expect_closed", "within_ms": 1000}],
         note="the closing side drains what the peer still sends and closes within 200 ms of the half-close")

    # ======================= guest under test =======================
    guest_hello = {"type": "hello", "role": "guest", "protocol": 1, "min_protocol": 1, "udp_port": "$u16nz",
                   "heartbeat_ms": "$hb", "timeout_ms": "$int", "game": "$string"}
    GS = [{"op": "accept"}, expect_line(guest_hello, capture={"ut_udp_port": "udp_port"}),
          {"op": "expect_no_datagram", "ms": 300},
          {"op": "set", "vars": {"session": SESSION}},
          send(hello(role="host", game="vector host", session=SESSION)), ev("connected", {"role": "host",
                                                                                          "session": SESSION})]
    case("sess.guest.handshake.hello_and_silence", "6", "guest", GS + [
        {"op": "expect_datagram", "kind": 1, "count": 2, "timeout_ms": 2500}],
         config={"heartbeat_ms": 50},
         note="the guest sends its hello first and nothing over UDP before it has the host's hello; afterwards it "
              "beats with the host's session")
    for hb in (10, 10000):
        case("sess.guest.handshake.host_heartbeat_%d_accepted" % hb, "8", "guest", [
            {"op": "accept"}, expect_line(guest_hello), send(hello(role="host", session=SESSION, heartbeat_ms=hb)),
            ev("connected", {"role": "host"})])
    for hb in (0, 9, 10001):
        case("sess.guest.handshake.host_heartbeat_%d_refused" % hb, "8", "guest", [
            {"op": "accept"}, expect_line(guest_hello), send(hello(role="host", session=SESSION, heartbeat_ms=hb))]
             + bye_closed + [ev("handshake_failed")])
    case("sess.guest.handshake.host_role_guest_refused", "6", "guest", [
        {"op": "accept"}, expect_line(guest_hello), send(hello(role="guest", session=SESSION))] + bye_closed
         + [ev("handshake_failed")])
    for p in (0, 2, 4294967295):
        case("sess.guest.handshake.host_protocol_%d_refused" % p, "6", "guest", [
            {"op": "accept"}, expect_line(guest_hello), send(hello(role="host", session=SESSION, protocol=p,
                                                                   min_protocol=0))] + bye_closed
             + [ev("handshake_failed")], note="the chosen version must be within the guest's own range 1..=1")
    case("sess.guest.handshake.host_session_missing_refused", "6, 8", "guest", [
        {"op": "accept"}, expect_line(guest_hello), send(hello(role="host"))] + bye_closed + [ev("handshake_failed")],
         note="the datagrams need the session; a host hello carries one")
    case("sess.guest.handshake.host_session_zero_refused", "8", "guest", [
        {"op": "accept"}, expect_line(guest_hello), send(hello(role="host", session=0))] + bye_closed
         + [ev("handshake_failed")], note="session is a nonzero u32")
    case("sess.guest.handshake.host_session_above_u32_refused", "8", "guest", [
        {"op": "accept"}, expect_line(guest_hello), send(hello(role="host", session=4294967296))] + bye_closed
         + [ev("handshake_failed")])
    case("sess.guest.handshake.not_json_refused", "6, 8", "guest", [
        {"op": "accept"}, expect_line(guest_hello), send("nope\n")] + bye_closed + [ev("handshake_failed")])
    case("sess.guest.handshake.host_hello_missing_field_refused", "8", "guest", [
        {"op": "accept"}, expect_line(guest_hello), send(hello(role="host", session=SESSION, drop=("timeout_ms",)))]
         + bye_closed + [ev("handshake_failed")])
    case("sess.guest.handshake.unknown_field_ignored", "8", "guest", [
        {"op": "accept"}, expect_line(guest_hello), send(hello(role="host", session=SESSION, future=[1, 2])),
        ev("connected")])
    case("sess.guest.handshake.no_hello_before_timeout", "6", "guest", [
        {"op": "accept"}, expect_line(guest_hello), closed(allow_lines=True)], config={"timeout_ms": 300},
         note="a side that has not received the peer's hello within its own timeout_ms closes the connection")
    case("sess.guest.control.focus", "7, 8", "guest", GS + [
        {"op": "expect_focus", "focus": "host"},
        send('{"type":"input","focus":"guest"}\n'), ev("focus", {"focus": "guest"}),
        {"op": "expect_focus", "focus": "guest"},
        send('{"type":"bye","reason":"x"}\n'), ev("lost"), {"op": "expect_focus", "focus": "host"}])
    case("sess.guest.control.bye_from_host", "6.1", "guest", GS + [
        send('{"type":"bye","reason":"going"}\n'), ev("lost"), {"op": "expect_closed"}])
    case("sess.guest.control.garbage_ends_session", "8", "guest", GS + [send("{{{\n")] + bye_closed + [ev("lost")])
    case("sess.guest.control.input_grant", "8", "guest", GS + [
        send('{"type":"input","focus":"guest"}\n'), ev("focus", {"focus": "guest", "requested": False}),
        send('{"type":"input","focus":"host","reason":"menu"}\n'), ev("focus", {"focus": "host", "requested": False})])
    case("sess.guest.control.host_spawn_and_despawn", "8", "guest", GS + [
        send(spawn_line(3)), ev("spawn", {"entity": 3}), send('{"type":"despawn","entity":3}\n'),
        ev("despawn", {"entity": 3})])
    case("sess.guest.sends.focus_request", "8", "guest", GS + [
        {"op": "ut", "do": "set_focus", "focus": "guest"}, expect_line({"type": "input", "focus": "guest"})])
    case("sess.guest.sends.bye_on_disconnect", "6.1", "guest", GS + [
        {"op": "ut", "do": "disconnect", "reason": "guest done"}, expect_line({"type": "bye", "reason": "guest done"}),
        closed()])
    case("sess.guest.sends.camera_poses_frame", "9", "guest", GS + [
        {"op": "ut", "do": "spawn", "entity": 1001, "provider": "P", "asset": "a", "name": None},
        expect_line({"type": "spawn", "entity": 1001}),
        {"op": "ut", "do": "send_poses", "like": "dg.pose.one"},
        {"op": "expect_datagram", "like": "dg.pose.one", "timeout_ms": 2000},
        {"op": "ut", "do": "send_frame", "like": "dg.frame.no_camera"},
        {"op": "expect_datagram", "like": "dg.frame.no_camera", "timeout_ms": 2000}])
    case("sess.guest.udp.camera_after_handshake", "9.3", "guest", GS + [
        sendd(dg(cam, 2, session="{session_le}", seq=1)), ev("camera", {"seq": 1})])
    case("sess.guest.udp.wrong_session_dropped", "5, 6", "guest", GS + [
        sendd(dg(cam, 2, session="{session_le_minus1}", seq=1)), noev("camera", 300),
        sendd(dg(cam, 2, session="{session_le}", seq=2)), ev("camera", {"seq": 2})])

    # =========================== revision 5 ===========================
    # ---- before and after the handshake (6)
    not_connected = {"op": "expect_no_event", "event": "connected", "ms": 200}
    first_lines = (("spawn", spawn_line(5)), ("unknown_type", '{"type":"ping"}\n'),
                   ("input", '{"type":"input","focus":"guest"}\n'), ("despawn", '{"type":"despawn","entity":1}\n'),
                   ("frame_channel", '{"type":"frame_channel","path":"/tmp/x","width":4,"height":2,"slots":3}\n'))
    for tag, first in first_lines:
        case("sess.host.handshake.first_line_%s_is_a_protocol_error" % tag, "6", "host", [
            {"op": "connect"}, send(first)] + bye_closed + [ev("handshake_failed")],
             note="a first line that is not hello or bye is refused with a bye, known type or not")
        case("sess.guest.handshake.first_line_%s_is_a_protocol_error" % tag, "6", "guest", [
            {"op": "accept"}, expect_line(guest_hello), send(first), not_connected] + bye_closed
             + [ev("handshake_failed")])
    case("sess.host.handshake.first_line_hello_after_garbage_does_not_count", "6", "host", [
        {"op": "connect"}, send(spawn_line(5)), send(hello())] + bye_closed + [ev("handshake_failed")],
         note="the first line decides; a hello behind it is not read")
    case("sess.host.handshake.first_line_bye_ends_quietly", "6", "host", [
        {"op": "connect"}, send('{"type":"bye","reason":"x"}\n'), closed(), ev("handshake_failed")],
         note="a bye is a valid first line; the receiver closes without a bye of its own")
    case("sess.guest.handshake.first_line_bye_ends_quietly", "6", "guest", [
        {"op": "accept"}, expect_line(guest_hello), send('{"type":"bye","reason":"busy"}\n'), closed(),
        ev("handshake_failed")])
    case("sess.guest.handshake.first_line_bye_without_reason", "6, 8", "guest", [
        {"op": "accept"}, expect_line(guest_hello), send('{"type":"bye"}\n'), closed(), ev("handshake_failed")])
    case("sess.host.control.second_hello_is_a_protocol_error", "6", "host", HS + [send(hello())] + bye_closed
         + [ev("lost")])
    case("sess.host.control.second_hello_with_other_values_is_a_protocol_error", "6", "host", HS + [
        send(hello(protocol=7, min_protocol=1, udp_port=9))] + bye_closed + [ev("lost")])
    case("sess.guest.control.second_hello_is_a_protocol_error", "6", "guest", GS + [
        send(hello(role="host", session=SESSION))] + bye_closed + [ev("lost")])

    # ---- bye (8)
    for tag, line in (("without_reason", '{"type":"bye"}\n'), ("reason_null", '{"type":"bye","reason":null}\n'),
                      ("reason_empty", '{"type":"bye","reason":""}\n')):
        case("sess.host.control.bye_%s_ends_the_session" % tag, "6.1, 8", "host", HS + [
            send(line), ev("lost"), closed()], note="a bye needs no reason; it is answered by closing, not by a bye")
        case("sess.guest.control.bye_%s_ends_the_session" % tag, "6.1, 8", "guest", GS + [
            send(line), ev("lost"), closed()])
    for tag, line in (("reason_number", '{"type":"bye","reason":5}\n'), ("reason_array", '{"type":"bye","reason":["x"]}\n'),
                      ("reason_true", '{"type":"bye","reason":true}\n')):
        case("sess.host.control.bye_%s_is_a_protocol_error" % tag, "8", "host", HS + [send(line)] + bye_closed
             + [ev("lost")])
        case("sess.guest.control.bye_%s_is_a_protocol_error" % tag, "8", "guest", GS + [send(line)] + bye_closed
             + [ev("lost")])

    # ---- hello: widths and ranges (8)
    def host_accepts(tag, text, note=None):
        case("sess.host.handshake.%s_accepted" % tag, "8", "host", [
            {"op": "connect"}, send(text), expect_line(host_reply), ev("connected")], note=note)

    def host_refuses(tag, text, note=None):
        case("sess.host.handshake.%s_refused" % tag, "8", "host", [
            {"op": "connect"}, send(text), not_connected] + bye_closed + [ev("handshake_failed")], note=note)

    def guest_accepts(tag, text, note=None):
        case("sess.guest.handshake.%s_accepted" % tag, "8", "guest", [
            {"op": "accept"}, expect_line(guest_hello), send(text), ev("connected")], note=note)

    def guest_refuses(tag, text, note=None):
        case("sess.guest.handshake.%s_refused" % tag, "8", "guest", [
            {"op": "accept"}, expect_line(guest_hello), send(text), not_connected] + bye_closed
             + [ev("handshake_failed")], note=note)

    host_accepts("protocol_65535_min_1", hello(protocol=65535, min_protocol=1),
                 "the version fields are u16; the session version is min(1, 65535) = 1")
    host_accepts("min_protocol_0_protocol_1", hello(protocol=1, min_protocol=0))
    host_refuses("protocol_65536", hello(protocol=65536, min_protocol=1), "65536 does not fit a u16")
    host_refuses("protocol_u32_max_plus_1", hello(protocol=4294967296, min_protocol=1))
    host_refuses("min_protocol_65536", hello(protocol=1, min_protocol=65536))
    host_refuses("min_protocol_65535_above_1", hello(protocol=1, min_protocol=65535),
                 "the ranges do not overlap: the session version 1 is below the guest's minimum")
    host_refuses("heartbeat_u32_max", hello(heartbeat_ms=4294967295), "a u32, so not a parse error, but not 10 ..= 10000")
    host_refuses("heartbeat_u32_max_plus_1", hello(heartbeat_ms=4294967296))
    host_accepts("timeout_0", hello(timeout_ms=0), "timeout_ms is informative: any u32")
    host_accepts("timeout_1", hello(timeout_ms=1))
    host_accepts("timeout_u32_max", hello(timeout_ms=4294967295))
    host_refuses("timeout_u32_max_plus_1", hello(timeout_ms=4294967296))
    host_refuses("udp_port_0", hello(udp_port=0), "nothing can be sent to port 0")
    host_refuses("udp_port_65536", hello(udp_port=65536))
    host_accepts("udp_port_1", hello(udp_port=1))
    host_refuses("protocol_not_an_integer_literal", hello().replace('"protocol":1,', '"protocol":1.0,'))
    host_refuses("heartbeat_not_an_integer_literal", hello().replace('"heartbeat_ms":250', '"heartbeat_ms":2.5e2'))
    host_refuses("udp_port_negative_zero", hello().replace('"udp_port":${peer_udp_port}', '"udp_port":-0'))
    host_accepts("duplicate_key_last_wins", hello().replace('"type":"hello",', '"type":"hello","udp_port":0,'),
                 "the first udp_port 0 is overridden by the real one; the last duplicate wins")
    host_refuses("duplicate_key_last_is_bad", hello().replace('"type":"hello",', '"type":"hello","udp_port":${peer_udp_port},')
                 .replace('"timeout_ms":2000}', '"timeout_ms":2000,"udp_port":0}'))
    host_refuses("nul_in_game", hello(game="a\u0000b"), "U+0000 in a string is a protocol error")
    host_refuses("lone_surrogate_in_game", hello().replace('"game":"vector peer"', '"game":"\\ud83d"'))
    host_refuses("nul_in_an_unknown_field", hello(future="x\u0000y"))
    host_refuses("deeply_nested_unknown_field", hello(future=json.loads("[" * 130 + "]" * 130)),
                 "nesting above 127 levels")
    host_accepts("nested_unknown_field_at_the_limit", hello(future=json.loads("[" * 126 + "]" * 126)),
                 "the object and 126 arrays: 127 levels")
    host_accepts("session_null_is_absent", hello().replace('"type":"hello",', '"type":"hello","session":null,'))
    guest_accepts("timeout_0", hello(role="host", session=SESSION, timeout_ms=0))
    guest_accepts("timeout_u32_max", hello(role="host", session=SESSION, timeout_ms=4294967295))
    guest_refuses("timeout_u32_max_plus_1", hello(role="host", session=SESSION, timeout_ms=4294967296))
    guest_refuses("udp_port_0", hello(role="host", session=SESSION, udp_port=0))
    guest_refuses("protocol_65536", hello(role="host", session=SESSION, protocol=65536, min_protocol=0))
    guest_accepts("session_u32_max", hello(role="host", session=4294967295))
    guest_refuses("nul_in_game", hello(role="host", session=SESSION, game="a\u0000b"))
    case("sess.host.handshake.byte_order_mark_is_not_whitespace", "8", "host", [
        {"op": "connect"}, {"op": "send_hex", "hex": b"\xef\xbb\xbf".hex() + hello().encode().hex()}] + bye_closed
         + [ev("handshake_failed")])
    case("sess.host.handshake.lone_cr_is_whitespace", "8", "host", [
        {"op": "connect"}, send(hello().replace('"role":"guest",', '"role":"guest",\r'))] + [
        expect_line(host_reply), ev("connected")])
    case("sess.host.handshake.line_ending_in_two_crs", "8", "host", [
        {"op": "connect"}, send(hello().replace("\n", "\r\r\n")), expect_line(host_reply), ev("connected")])

    # ---- despawn of an id that is not live (8)
    case("sess.host.control.despawn_of_an_unknown_entity_is_ignored", "8", "host", HS + [
        send('{"type":"despawn","entity":99}\n'), noev("despawn", 300), send(spawn_line(5)), ev("spawn", {"entity": 5}),
        {"op": "expect_no_line", "ms": 100}], note="no event, no error, nothing sent")
    case("sess.guest.control.despawn_of_an_unknown_entity_is_ignored", "8", "guest", GS + [
        send('{"type":"despawn","entity":99}\n'), noev("despawn", 300), send(spawn_line(5)), ev("spawn", {"entity": 5}),
        {"op": "expect_no_line", "ms": 100}])
    case("sess.host.control.second_despawn_of_an_entity_is_ignored", "8", "host", HS + [
        send(spawn_line(5)), ev("spawn", {"entity": 5}), send('{"type":"despawn","entity":5}\n'),
        ev("despawn", {"entity": 5}), send('{"type":"despawn","entity":5}\n'), noev("despawn", 300),
        {"op": "expect_no_line", "ms": 100}])
    case("sess.host.control.despawn_of_a_spawn_ignored_over_the_limit", "8", "host", HS + [
        {"op": "send_range", "template": '{"type":"spawn","entity":${i},"provider":"P","asset":"a"}\n', "from": 1,
         "to": 1025},
        ev("spawn", None, 1024), noev("spawn", 300), send('{"type":"despawn","entity":1025}\n'), noev("despawn", 300),
        {"op": "expect_no_line", "ms": 100}, send('{"type":"despawn","entity":1}\n'), ev("despawn", {"entity": 1})],
         note="the 1025th spawn was ignored; its despawn must not end the session")

    # ---- collision assembly (9.5)
    c1 = (9.0, 2.0, 3.0)
    case("sess.host.udp.collision_center_and_radius_come_from_the_first_chunk_to_arrive", "9.5", "host", HS + [
        sendd(dg(coll(1, 1, 2, 2, center=c1, radius=65.0), 4, seq=1)), sendd(dg(coll(1, 0, 2, 3), 4, seq=2)),
        ev("collision", {"query": 1, "triangles": 5, "center": [9.0, 2.0, 3.0], "radius": 65.0})],
         note="the chunks of a query are expected to agree; when they do not, the first one decides")
    case("sess.host.udp.collision_center_and_radius_of_later_chunks_are_not_compared", "9.5", "host", HS + [
        sendd(dg(coll(1, 0, 2, 3), 4, seq=1)), sendd(dg(coll(1, 1, 2, 2, center=c1, radius=65.0), 4, seq=2)),
        ev("collision", {"query": 1, "triangles": 5, "center": [1.0, 2.0, 3.0], "radius": 64.0})])
    case("sess.host.udp.collision_chunk_with_another_chunk_count_is_dropped", "9.5", "host", HS + [
        sendd(dg(coll(1, 0, 2, 3), 4, seq=1)), sendd(dg(coll(1, 1, 3, 2), 4, seq=2)), noev("collision", 300),
        sendd(dg(coll(1, 1, 2, 4), 4, seq=3)), ev("collision", {"query": 1, "triangles": 7})],
         note="the assembly goes on: the odd chunk is lost, the rest completes")
    case("sess.host.udp.collision_first_chunk_decides_chunk_count", "9.5", "host", HS + [
        sendd(dg(coll(1, 2, 3, 2), 4, seq=1)), sendd(dg(coll(1, 1, 2, 2), 4, seq=2)), noev("collision", 300),
        sendd(dg(coll(1, 0, 3, 2), 4, seq=3)), noev("collision", 300),
        sendd(dg(coll(1, 1, 3, 2), 4, seq=4)), ev("collision", {"query": 1, "triangles": 6})])
    case("sess.host.udp.collision_duplicate_chunk_does_not_complete_a_query", "9.5", "host", HS + [
        sendd(dg(coll(1, 0, 2, 3), 4, seq=1)), sendd(dg(coll(1, 0, 2, 3), 4, seq=2)), noev("collision", 300),
        sendd(dg(coll(1, 1, 2, 2), 4, seq=3)), ev("collision", {"query": 1, "triangles": 5}), noev("collision", 100)])
    case("sess.host.udp.collision_first_copy_of_a_chunk_wins", "9.5", "host", HS + [
        sendd(dg(coll(1, 0, 2, 3), 4, seq=1)), sendd(dg(coll(1, 0, 2, 1, first_tri=5), 4, seq=2)),
        sendd(dg(coll(1, 1, 2, 2), 4, seq=3)), ev("collision", {"query": 1, "triangles": 5})])
    case("sess.host.udp.collision_chunk_count_above_4096_is_malformed", "9.5", "host", HS + [
        sendd(dg(coll(1, 0, 4097, 2), 4, seq=1)), sendd(dg(coll(1, 0, 65535, 2), 4, seq=2)), noev("collision", 300),
        sendd(dg(coll(1, 0, 1, 1), 4, seq=3)), ev("collision", {"query": 1, "triangles": 1})],
         note="a malformed chunk does not start an assembly: query 1 is still free")
    case("sess.host.udp.collision_chunk_count_4096_is_accepted", "9.5", "host", HS + [
        sendd(dg(coll(1, 4095, 4096, 1), 4, seq=1)), noev("collision", 300),
        sendd(dg(coll(2, 0, 1, 1), 4, seq=2)), ev("collision", {"query": 2, "triangles": 1})],
         note="4096 chunks are allowed; the incomplete query 1 is discarded when query 2 starts")
    case("sess.host.udp.collision_of_8292_bytes_is_not_cut_to_8192", "2, 9.5", "host", HS + [
        sendd(dg(coll(1, 0, 1, 226) + "00" * 100, 4, seq=1)), noev("collision", 400),
        sendd(dg(coll(2, 0, 1, 226), 4, seq=2)), ev("collision", {"query": 2, "triangles": 226})],
         note="a full 8192 byte chunk is accepted; the same with 100 more bytes is malformed, not a cut-down valid one")
    case("sess.host.udp.pose_of_9000_bytes_is_not_cut_to_8192", "2, 9.4", "host", HS + [
        {"op": "send_range", "template": '{"type":"spawn","entity":${i},"provider":"P","asset":"a"}\n', "from": 1000,
         "to": 1101}, ev("spawn", None, 102),
        sendd(dg(pose_hex(list(range(1000, 1102))) + "00" * 808, 3, seq=1)), noev("poses", 400),
        sendd(dg(pose_hex(list(range(1000, 1102))), 3, seq=2)), ev("poses", {"count": 102})])
    case("sess.host.udp.unknown_kind_above_8192_bytes_is_proof_of_life", "2, 7, 9.1", "host", [
        {"op": "connect"}, send(hello(heartbeat_ms=10, timeout_ms=400)),
        expect_line(host_reply, capture={"session": "session"}), ev("connected")] + [
        s for _ in range(18) for s in (sendd(dg("00" * (9000 - 24), 9, seq=1)), {"op": "sleep", "ms": 100})] + [
        noev("lost", 10), ev("lost")],
         config={"timeout_ms": 1000, "heartbeat_ms": 100}, slow=True,
         note="an unknown kind is ignored at any length and still proves the peer is there")
    return cases


# ---------------------------------------------------------------------------------------------

def dump(path, doc_head, cases):
    lines = ["{"]
    for k, v in doc_head.items():
        lines.append("  %s: %s," % (json.dumps(k), json.dumps(v, ensure_ascii=True)))
    lines.append('  "cases": [')
    body = []
    for c in cases:
        body.append("    " + json.dumps(c, ensure_ascii=True, separators=(",", ":")))
    lines.append(",\n".join(body))
    lines.append("  ]")
    lines.append("}")
    return "\n".join(lines) + "\n"


def main():
    check = "--check" in sys.argv
    out = {}
    (pixel, pfiles) = pixel_cases(HERE)
    docs = {
        "control.json": ("control", control_cases()),
        "datagrams.json": ("datagrams", datagram_cases()),
        "pixelfiles.json": ("pixelfiles", pixel),
        "sessions.json": ("sessions", sessions()),
    }
    ids = set()
    for name, (kind, cases) in docs.items():
        for c in cases:
            assert c["id"] not in ids, "duplicate id " + c["id"]
            ids.add(c["id"])
        head = {"format": "rbl-link-vectors/1", "kind": kind, "contract": CONTRACT}
        out[name] = dump(os.path.join(HERE, name), head, cases).encode("ascii")
    for name, data in pfiles.items():
        out["pixelfiles/" + name] = data
    sums = "".join("%s  %s\n" % (hashlib.sha256(out[n]).hexdigest(), n) for n in sorted(out))
    # README.md and GAPS.md are hand written but are part of what consumers copy and pin
    for hand in ("README.md", "GAPS.md"):
        path = os.path.join(HERE, hand)
        if os.path.exists(path):
            out[hand] = open(path, "rb").read()
    sums = "".join("%s  %s\n" % (hashlib.sha256(out[n]).hexdigest(), n) for n in sorted(out))
    out["SHA256SUMS"] = sums.encode("ascii")
    bad = 0
    for name, data in sorted(out.items()):
        path = os.path.join(HERE, name)
        if check:
            if not os.path.exists(path) or open(path, "rb").read() != data:
                print("out of date:", name)
                bad += 1
        else:
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "wb", ) as f:
                f.write(data)
    if check:
        extra = set()
        for root, _, files in os.walk(os.path.join(HERE, "pixelfiles")):
            for f in files:
                extra.add("pixelfiles/" + f)
        for e in sorted(extra - set(out)):
            print("stale file:", e)
            bad += 1
        sys.exit(1 if bad else 0)
    counts = {n: len(c) for n, (_, c) in docs.items()}
    print("wrote", counts, "and", len(pfiles), "pixel files; digest of SHA256SUMS:",
          hashlib.sha256(out["SHA256SUMS"]).hexdigest())


if __name__ == "__main__":
    main()
