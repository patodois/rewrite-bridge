# What the contract does not say

The vectors in this directory cover what `docs/CONTRACT.md` (v1, revision 4) says. These are the places where
the text is silent or ambiguous; there is **no vector** for any of them, because a vector would only record what
one implementation happens to do. Each is a candidate for a contract clarification (a later revision), after
which a vector can be added.

## Control channel

1. **What a receiver does with a first line that is valid but is not `hello`** (a `spawn` before the handshake).
   Section 6 says `hello` "has to be the first line in each direction" but not the reaction.
2. **A second `hello` after the handshake.**
3. **`bye` without a `reason`, or with one that is not a string.** Section 8 shows `{"type":"bye","reason":"..."}`
   but has no field table for `bye`, so it does not say whether `reason` is required.
4. **JSON numbers that are integral but not written as integers** (`7.0`, `7e0`) in integer fields, and **duplicate
   keys** in one object (`{"type":"bye","type":"hello"}`). "Integer" is not defined at the JSON level.
5. **`null` for an optional field** (`"name":null`). The tables say "string, optional", not whether `null` counts as
   absent. (The vectors do require an encoder to omit an absent optional field, which is allowed whichever way
   this is settled.)
6. **Integer widths of `protocol`, `min_protocol`, `heartbeat_ms`, `timeout_ms` and of `width`, `height`, `slots` in
   `frame_channel`.** Only entity ids (`u32`), `session` (`u32`) and, by the nature of the field, `udp_port` have
   a stated range. A `hello` with `"protocol": 4294967296` is neither shown valid nor invalid.
7. **`timeout_ms` bounds.** `heartbeat_ms` is 10 to 10000; `timeout_ms` has no range (is 0 allowed?).
8. **`udp_port` 0 in a `hello`.** The configuration "0 means any free port", but the announced port must be the
   bound one; the text does not say a `hello` announcing 0 is invalid.
9. **Lone surrogate escapes** (`"\ud83d"`) and **NUL** (`"\u0000"`) inside strings; a **byte order mark** at the
   start of a line; a lone `\r` inside a line.
10. **A `despawn` of an entity that was never spawned**: ignored silently, or an error? Section 8 says only that a
    `despawn` "frees the place".
11. **Whether an implementation must tell its peer** that a `spawn` beyond 1024 live entities was ignored. The text
    says "ignored and logged".
12. **A guest reconnecting after a session ends** is "may", so no vector requires it.

## Datagrams

13. **`Frame` with `frame` 0.** Section 9.6 numbers frames from 1 but does not say a datagram with 0 is malformed.
14. **`Camera` field validity**: `near` not above 0, `far` below `near`, NaN or infinite matrix elements, a
    viewport of 0. The table says "near plane distance, > 0" but not what a decoder does with a camera that breaks it.
15. **`Pose` field validity**: `flags` bits other than bit 0 ("others 0"), a rotation that is not unit length, a zero
    scale, NaN values.
16. **`Collision` chunks of one query that disagree** on `center`, `radius` or `chunk_count` ("All chunks of a query
    carry the same ..." says what senders do, not what a receiver does), a chunk delivered twice, `chunk_count`
    up to 65535 (no limit on what a receiver should buffer; the C implementation has a configurable one).
17. **A `Frame` whose `slot` is not `frame % slots`**, or whose `width` and `height` differ from the slot header's.
    The Rust reader refuses a datagram whose size differs from the slot's.
18. **Datagrams of 8193 bytes or more** other than as a length mismatch: no separate rule is stated.

## Pixel file

19. **`slot_size` smaller than `64 + stride * height`, or not a multiple of 64**, and **a wrong `magic`, `version`
    or `format`**: the field tables fix these values, but the list of checks a reader must make ("Compatibility
    and checks", "Opening and creating") does not name them. The vectors have them with `basis: "table"`.
20. **Slot header `width` or `height` above the file's maximum** (the table says "pixels used, `<=` file width"):
    `basis: "table"`.
21. **A slot index outside `0 .. slots-1`** in a `Frame` datagram: implied, not stated.
22. **The seqlock "overwritten while copying" case** (`l2 != l1`) needs a writer running during the copy, so no static
    file can produce it. The vectors cover an odd lock and a slot holding another frame; the live tests of each
    repository cover contention.
23. **What a writer does when asked for a frame larger than its file**, or **for a depth plane it has not got**
    (the Rust and C writers refuse; the contract is silent).
24. **Creating a writer with parameters outside the limits** (`basis: "table"`): section 9.6 says a sender "stays
    within both limits" but not that its constructor must fail.
25. **`depth_format` 1 with `reserved` header bytes 56 to 63 not zero**, and non-zero bytes in the slot header's
    `reserved`: "ignored on read" is stated for datagrams, not for the pixel file.

## Sessions

26. **The text of a `bye` reason.** Only `"busy"` is fixed. The vectors check that a `bye` with a string reason
    arrives, never what it says.
