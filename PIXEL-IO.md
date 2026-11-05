# Pixel readers and presentation migration

Contract revision 7 preserves protocol version 1, datagrams,
file headers, slot offsets or depth layout. Writers retain their mapped publication path.

## Presenting frames

A Frame event is an unvalidated announcement. Use read_latest (Rust/C), ReadLatest (C#),
or readLatest (Java) for presentation. Only a successful current-file copy advances the
number. Raw read APIs remain unordered for inspection and re-reading. Rust accepts an
optional depth destination; the other APIs can omit depth using their existing argument.
Output buffers supplied by callers are valid only on success and may contain partial or
previous bytes after a miss. Clear the displayed image and discard the old reader on a new
channel, session loss or a gone reader, including when opening the replacement fails.
Async results must still belong to the active reader before they are published.

Reader stale counters count duplicate/older nonzero presentation reads: stale_reads() in
Rust, pixel stats.stale in C, SkippedStale in C#, staleReads() in Java. Null frame arguments
are caller misses: C returns RBL_ERR_ARGS, C# and Java return null, without counters or state
changes. Rust's borrowed FrameReady cannot be null. Frame number zero is invalid, not a
presentation-staleness event.

## Unix and Windows I/O

Unix readers use bounded positional I/O and never dereference peer-owned mapped pages.
They validate the header and dimensions, observe the lock, copy requested planes, then read
both lock and frame again. Short reads are completed; EOF and I/O errors reject the entire
copy. The metadata size check also rejects shrinking unused tail bytes. It is an early
rejection, not the protection against later truncation.

Normal cropped planes use up to eight rows per vectored read on macOS/Linux (preadv in
Rust/C/C#, scatter read after positioning the private single-threaded FileChannel in Java).
Data goes directly into owned output in Rust/C/C#; padding goes into reusable scratch and
is discarded. Java reuses packed direct buffers before copying into its array results.
Native Rust and C builds require macOS 11.0 or newer. C on Android uses scalar
positional reads to avoid an API-24 preadv import. Rust on 32-bit Linux uses FileExt
positional reads instead of casting offsets to a narrow off_t. C# pixel APIs require
a 64-bit process and reject other pointer widths before native calls.
This reduces syscall count without a full-plane scratch allocation. Grouping is limited to
strides up to 16 KiB and spans up to four times the useful output. Wider or very sparse files
keep per-row reads: measurements showed that reading their padding costs more than the
saved syscalls. Full-width frames retain contiguous copies. Other Unix targets and a missing
C# preadv binding use bounded 32 KiB packed spans under the same limits, then per-row reads.
Rust/C retry interruptions and continue partial vectored reads, including within a vector.

Java owned direct scratch is reclaimed by the JVM without invoking Unsafe on Unix reader
close. The writer's existing cleaner path is unchanged. C# caches SafeFileHandle and uses
native positional I/O when available; missing or unsupported bindings switch to safe
FileStream copies with explicit positions and buffering disabled. I/O/provider exceptions
inside CopyAt are contained as misses. Both missing pread and missing preadv bindings are covered by fault injection; this
does not establish validation on Unity or IL2CPP. A managed frame-allocation failure
in C# returns null and increments SkippedAllocation without advancing order. This
cannot prevent an operating system from terminating a process under memory pressure;
other allocation policies remain those of each language and API.

Windows keeps a read-only map and the existing atomic lock observations: an active section
prevents ordinary truncation. Creation also checks that the actual mapping/file extent did
not shrink before protection took effect. Rust and managed ports read the header using
bounded I/O before mapping; C rechecks the extent with the section active before touching
it. Windows native tests run in the CI platform matrix; the benchmark tables below measure
macOS only. Java mapped-reader cleanup may depend on GC if the
runtime cannot release its section. Readers are not safe for concurrent close/read; Rust
serializes scratch and presentation state but callers still own reader lifetime.

The revised protocol explicitly distinguishes observation checks from a formal acquire
load: POSIX does not specify pread as an atomic acquire from the writer's memory. Tests
reject detected overlapping writes, not every possible publication violation on every
filesystem. ABA and hostile reuse of lock/frame remain outside that guarantee. I/O can
block. Writers still assume nobody truncates their own mapping. CI covers native Linux, macOS and Windows; WASI checks cover the transport without
pixel files. Mono, Unity and IL2CPP require separate runtime validation.

## Benchmark method

The benchmark repeats raw reads of one valid slot, so it measures copies rather than the
ordered API rejecting duplicates. It does not include presentation-order locking or counters.
Synthetic local files, warm cache, two warm-up reads, 30 samples for normal/cropped frames
and 5 for maximum-sized outputs. Times include header validation, requested planes and final
identity checks; exclude writer, GPU, IPC and rendering. Rust/C reuse destination buffers;
C#/Java allocate results according to their public APIs, with GC requested outside timed
regions. Other processes may compete for CPU and memory. p95 with five samples is only the
largest observed value. Do not compare allocation policies across languages as if identical.

Measured on macOS arm64. All numbers are milliseconds per frame.

| File | Frame | Depth | Before p50 | After p10 | After p50 | After p95 |
|---|---|---|---:|---:|---:|---:|
| 1280x720 | 1280x720 | false | 0.4968 | 0.1522 | 0.1526 | 0.1727 |
| 1280x720 | 1280x720 | true | 1.5158 | 0.2082 | 0.2522 | 0.2587 |
| 1920x1080 | 1920x1080 | false | 1.4751 | 0.2256 | 0.2298 | 0.2480 |
| 1920x1080 | 1920x1080 | true | 3.3305 | 0.5899 | 0.6012 | 0.6651 |
| 3840x2160 | 3840x2160 | false | 6.4099 | 1.8301 | 1.8499 | 1.8888 |
| 3840x2160 | 3840x2160 | true | 14.5998 | 3.9493 | 3.9925 | 4.0970 |
| 8192x8192 | 8192x8192 | false | 48.0035 | 15.7600 | 15.8223 | 15.9390 |
| 8192x8192 | 8192x8192 | true | 85.8562 | 34.5875 | 34.7322 | 34.7752 |
| 16384x1 | 16384x1 | false | 0.0065 | 0.0029 | 0.0029 | 0.0032 |
| 16384x1 | 16384x1 | true | 0.0107 | 0.0047 | 0.0050 | 0.0055 |
| 16384x4096 | 16384x4096 | false | 42.1884 | 17.0137 | 17.0209 | 17.0979 |
| 16384x4096 | 16384x4096 | true | 100.5034 | 34.3335 | 34.4738 | 34.5452 |
| 1920x1080 | 1280x720 | false | 1.2546 | 0.1754 | 0.1780 | 0.1830 |
| 1920x1080 | 1280x720 | true | 3.0183 | 0.3435 | 0.3728 | 0.3949 |
| 3840x2160 | 1920x1080 | false | 2.9198 | 0.4195 | 0.4310 | 0.4724 |
| 3840x2160 | 1920x1080 | true | 7.1001 | 1.3841 | 1.4058 | 1.4834 |
| 16384x4096 | 8192x4096 | false | 33.3756 | 9.8737 | 9.9758 | 10.1408 |
| 16384x4096 | 8192x4096 | true | 55.3882 | 20.8622 | 21.0036 | 21.1008 |
| 16384x4096 | 1x4096 | false | 3.9187 | 1.2012 | 1.2336 | 1.2635 |
| 16384x4096 | 1x4096 | true | 10.8237 | 2.4149 | 2.4715 | 2.5082 |

## Matched cropped-frame comparison

The same API was built with grouping disabled and enabled, one language at a time.
These sequential runs separate the optimization from other reader changes. Scheduling and
memory pressure still vary. Whole-plane scratch and unrestricted grouping were rejected
after measurements showed regressions on wide files. A one-column frame remains per-row.

| File | Frame | Depth | Row-wise p10 / p50 / p95 | Bounded p10 / p50 / p95 |
|---|---|---|---:|---:|
| 1920x1080 | 1280x720 | false | 0.4001 / 0.4342 / 0.4912 | 0.3424 / 0.3458 / 0.4659 |
| 1920x1080 | 1280x720 | true | 0.6265 / 0.6490 / 0.6907 | 0.4647 / 0.5092 / 0.6001 |
| 3840x2160 | 1920x1080 | false | 0.4991 / 0.5066 / 0.5715 | 0.4690 / 0.5039 / 0.5472 |
| 3840x2160 | 1920x1080 | true | 1.3080 / 1.3395 / 1.4634 | 1.3730 / 1.3939 / 1.4330 |
| 16384x4096 | 8192x4096 | false | 9.6492 / 9.6920 / 9.7774 | 9.6202 / 9.6837 / 9.7212 |
| 16384x4096 | 8192x4096 | true | 20.2334 / 20.3440 / 20.4456 | 20.2607 / 20.3262 / 20.6512 |
| 16384x4096 | 1x4096 | false | 1.1076 / 1.1213 / 1.1516 | 1.1230 / 1.1501 / 1.1766 |
| 16384x4096 | 1x4096 | true | 2.2341 / 2.2670 / 2.3030 | 2.2623 / 2.3379 / 2.3925 |
