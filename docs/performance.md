# Desktop presentation performance

## Measuring a live session

Set `FJERN_STATS=1` before launching Fjern to print a two-second interval line
for either RDP or VNC. Both report completed update rate, attempts to submit an
image to the window, and current process RSS in MiB. RDP also reports the
longest window submission, the longest wait between snapshot publication and
UI pickup, and the longest wait in the input queue before processing. These are
client-side segments, not input-to-display latency. VNC reports its longest
event batch and total scaling time per interval. `FJERN_VNC_STATS=1` remains an
alias for VNC diagnostics.

RDP additionally reports `published/s` snapshots created by the receiver,
`replaced/s` snapshots superseded before UI pickup, `picked/s` snapshots taken
by the UI, and `paint-new/s` picked revisions submitted to the window.
`published-row-changes/s` and `paint-row-changes/s` count distinct pixel rows
at the center of the image at those two stages. The row metrics are useful for
the moving-stripe fixture below, but changes elsewhere on the screen may be
missed. A successful window submission still does not prove a compositor
scanout; compare these counts with a screen recording for that last stage.

```sh
FJERN_STATS=1 cargo run --release -p fjern -- tui 2>fjern-stats.log
python3 tools/summarize_stats.py fjern-stats.log
```

Repeat the same 60-second workload three times at a fixed remote resolution:
10 seconds idle, 20 seconds scrolling a long page, 20 seconds moving a window,
and 10 seconds idle. Record the host software/version, desktop, codec, network,
resolution and whether scaling is active. Compare the update and paint-attempt
rates and peak RSS with an established client on the same host and workload.
For that client, use its own frame diagnostics or a repeatable screen capture;
its process RSS should be recorded separately. Do not equate completed VNC
updates or RDP bitmap batches with actual monitor frames. Window submission is
also not confirmed compositor scanout. To compare input-to-display latency,
record the same keyboard action and resulting display change with an external
high-frame-rate camera or synchronized screen/input capture on both clients.
The internal snapshot wait measures only one client-side segment of that delay.

### Windows 11 VM comparison, 2026-09-27

A Windows 11 VM on the local network ran `tools/rdp_benchmark.html` in Edge at
960×1056. The same signed-in desktop and bitmap graphics mode were used for
Fjern and FreeRDP 3.31.1 (`wlfreerdp3`, `-gfx -rfx`). The local page moves a
white stripe using `requestAnimationFrame`; Space changes a solid colour band.
Fjern's last 15 two-second intervals during the animation averaged 636.7 RDP
bitmap updates/s and 32.0 window paint attempts/s. Its peak RSS was 59.6 MiB,
with an 8.61 ms longest paint call and 8.53 ms longest snapshot wait. FreeRDP's
peak `/proc` RSS during the same page was 146.1 MiB. Both windows were shown at
the same size on the same Wayland desktop.

Both clients were also captured at 60 frames/s with the same Wayland region
recorder. Counting changes to the stripe on a middle scanline gave 31.98
visible changes/s over 32.8 s for Fjern and 32.00/s over 48.0 s for FreeRDP.
The median and 95th-percentile gap between changes were 33.3 ms for both.
This page and Windows host appear to limit the bitmap workload to about
32 distinct images/s; it does not establish either client's maximum FPS.

For ten Space presses per client, a one-pixel `grim` capture polled the colour
band until it changed. Median measured delays were 141.7 ms for Fjern and
132.1 ms for FreeRDP. The capture and process startup add substantial delay and
quantize these results; the 9.6 ms median difference is below the method's
precision. These values establish only that both clients responded on this
host. Fjern's 32.0/s paint count is a submission rate; the separate recording
above measured visible content changes.

The first live-host run also verified initial bitmap display, dynamic resolution,
and clipboard channel activation. A 512 MiB Windows-to-Linux file copy displayed
progressive byte counts in the title; another run cancelled before completion,
removed the staged partial file, and cleared the local file clipboard.
A 128 MiB Linux-to-Windows copy showed the upload completion title, and the
Windows file's SHA-256 matched its Linux source.

### Windows 11 higher-rate experiment, 2026-09-27

The same VM was rebooted with `DWMFRAMEINTERVAL=15` for a temporary test.
[Microsoft documents this as a way to raise the RDP maximum to 60 FPS on
Windows Server](https://learn.microsoft.com/en-us/troubleshoot/windows-server/remote/frame-rate-limited-to-30-fps);
its effect on this Windows 11 VM was tested empirically. The 960×1056 bitmap
session ran the same Edge animation. Fjern and FreeRDP 3.31.1 (`wlfreerdp3`,
`-gfx -rfx`) were recorded separately in the same 960×1056 Wayland region at
60 frames/s. The browser page was visible throughout each recording. The
center-scanline stripe changes were counted with
`python3 tools/analyze_rdp_capture.py <recording.mp4>`:

| Client | First capture | Second capture |
| --- | ---: | ---: |
| Fjern, unchanged 16 ms bitmap batch age | 49.38/s (36.63 s) | 48.62/s (35.85 s) |
| FreeRDP | 53.54/s (55.38 s) | 54.53/s (40.80 s) |

This establishes that the VM can deliver more than 32 visible changes/s with
both clients under this temporary setting. FreeRDP was about 5 changes/s ahead
in these sequential captures, but the later matched pair below narrowed this
gap substantially. These recordings do not establish a stable client advantage.
Fjern's internal paint-attempt rate was roughly 50–60/s; those
attempts are not confirmed compositor scanouts. A diagnostic run observed no
Wayland buffer-pool stalls. Shortening the maximum bitmap batch age from 16 to
12 ms yielded 49.37/s; 8 ms yielded 47.41/s. Neither change improved the
visible result, so both were reverted. The presentation handoff follow-up below
measured the stages between bitmap updates and window submission. The temporary
registry value, test user and page were removed, and the VM was returned to
stopped.

`analyze_rdp_capture.py` requires `ffmpeg` and `ffprobe` and assumes an
unscaled recording with the benchmark page visible at its center scanline. It
counts distinct stripe positions in the captured images, not RDP updates,
browser animation callbacks or physical display refreshes.

### Presentation handoff follow-up, 2026-09-27

With the same temporary host setting and page, `FJERN_STATS=1` measured
distinct center-row content at each client stage while a 60 frames/s screen
recording ran. A 30.08 s recorder calibration produced 29.73 s of video. The
final paired measurement ran the recorder and captured the log boundaries in
one process: 45.11 s of wall time yielded 44.72 s of video. At the normal
120 Hz UI polling target, Fjern published, picked up and submitted 55.12
distinct center rows/s without replacing a pending snapshot; the recording
showed 50.52 stripe changes/s. The difference is after window submission or
in compositor/recorder sampling. These measurements do not separate those
effects. FreeRDP on the same account, page, resolution and host setting showed
51.30 stripe changes/s in a subsequent 45.10 s wall-time capture (44.72 s of
video). The earlier roughly 5/s gap narrowed to 0.78/s here, so no stable
Fjern-specific visible rate regression is established.

Two temporary polling changes were measured on the same session. At 60 Hz, the
recording showed 50.41 stripe changes/s, while the UI sometimes replaced
pending snapshots and the maximum snapshot wait was around 16 ms. At 240 Hz,
the recording showed 46.69 changes/s and the maximum snapshot wait was around
4 ms. The 120 Hz run averaged 8.36 ms maximum snapshot wait per diagnostic
interval. The polling variants used log windows that extended beyond the
recording, so their internal rates should not be paired precisely with the
video rates. Neither variant establishes a worthwhile visible improvement;
120 Hz was restored. A follow-up should inspect Wayland frame callbacks and
presentation timing before changing UI scheduling.

## VNC tile scheduling and CopyRect

The ZRLE decoder emits one image event per 64×64 tile. A full 1920×1080
rectangle produces 510 events. The previous limit of 64 events per UI tick
required at least eight ticks to consume that image, even with every tile
already queued. VNC now processes queued events until 4 ms of work has elapsed,
with a secondary cap of 4096 events per tick. Every event remains ordered;
partial images and CopyRect dependencies are never discarded. The time budget
is checked after each event. Small Raw events (including ZRLE tiles up to
64×64) are applied in one iteration: a complete 4K frame takes 2040 tile
iterations, not 10,200 tile/substep iterations. Large Raw rectangles use batches of 16 rows
before processing later events; a following CopyRect or resize cannot overtake
unfinished pixels. Other single expensive events can still exceed the budget.

CopyRect now moves rows directly within the framebuffer using overlap-safe
copies. Downward moves run bottom-up; upward moves run top-down. This removes
the temporary rectangle allocation and the extra copy. An exhaustive small-grid
test compares all valid rectangle moves with a snapshot reference.

An optimized local CPU benchmark of a 1920×1064 rectangle scrolled down by
16 pixels measured 3.141 ms per update with the previous implementation and
0.283 ms with the new one. It alternates execution order across six batches
of 100 updates and reports the upper median. Reproduce with:

```sh
cargo test --release -p fjern benchmark_vnc_copy_rect --locked -- --ignored --nocapture
```

This is CPU copy time, not measured remote FPS. WayVNC's chosen encoding,
network transfer, decoding and compositor timing still affect the visible
result. CopyRect improvements apply only when the server sends CopyRect.
The existing 16 ms incremental refresh request cadence is unchanged.

Event processing enters the async runtime once per UI tick instead of once per
tile. The event count and time limits still apply, and cursor refresh requests
are awaited within the same runtime call.

Pixel conversion uses a bounded destination row and paired source/destination
iterators. A separate 1080p CPU benchmark measured 0.520 → 0.479 ms for 64-pixel
row segments and 0.380 → 0.324 ms for full-width rows. These are isolated
conversion costs, excluding allocation, transport, decoding and display. The
benchmark alternates old/new order across six batches of 100 images and checks
identical output. A regression checks nonzero tile offsets, untouched borders,
odd row widths and removal of the unused high byte. Reproduce with:

```sh
cargo test --release -p fjern benchmark_vnc_unpack_pixels --locked -- --ignored --nocapture
```

## VNC pipeline regression and measurements

ZRLE now reads from a 32 KiB decompressed buffer. An isolated release benchmark
of three-byte reads over a synthetic 1080p payload measured 43.335 → 18.161 ms.
This uses the vendored crate's release profile and measures decompression plus
reads/assertions, not an entire ZRLE frame or network FPS.

The frontend scaler caches fixed-point coordinate maps and target pixels.
Raw updates and CopyRect destinations explicitly invalidate source-row spans;
only target spans depending on those pixels are recomputed, including both
bilinear neighbors. There is no source snapshot or full-source comparison.
Native-size presentation bypasses scaling; pending damage remains until a
scaled presentation consumes it. Resize invalidates the cache, including a
same-sized desktop reset. Each image is capped at 16 million pixels and 8192
pixels per dimension.

The bilinear pixel calculation packs two channels into independent 32-bit
lanes of a u64, preserving the original integer rounding. A regression compares
all 65,536 fraction pairs for contrasting and maximum channel values with the
four-channel scalar implementation. `benchmark_vnc_blend` isolates that kernel;
it does not measure a complete frame.

Wayland buffers are persistently mapped shared memory. Changed row runs are
copied directly into released buffers without per-frame file writes. Surface
damage is calculated against the last submitted image, independently of the
older buffer selected for reuse. This distinction handles A → B → A changes
without stale compositor content. Busy buffers remain untouched; a full pool
retains the existing redraw/retry behavior. Fullscreen changes coalesce into
one contiguous copy and one damage rectangle.

ZRLE solid tiles and RLE runs use doubling block copies after a single palette
validation per run. The persistent stream and malformed-input checks remain.

Local isolated release measurements for this second pass:

| Workload | Previous path | New path |
| --- | ---: | ---: |
| 4K full-change buffer submission | 10.300 ms | 4.663 ms |
| Expand 2040 solid ZRLE tiles | 119.312 ms | 1.063 ms |

These alternate execution order over six batches and report the upper median.
The first includes both damage comparisons and copying but excludes compositor
presentation. The second only measures solid-tile expansion, not compression,
transport or arbitrary website content, using the vendored crate's release
profile. Neither result is a remote FPS measurement.

A previous row-snapshot implementation's sparse-change 1080p → 720p benchmark measured 9.521 ms for an independent
full-image scalar reference versus 0.623 ms for the cached renderer. The
reference includes output allocation and is not the compiled minifb C scaler;
this demonstrates avoided work on sparse damage, not a general 15× FPS gain.

```sh
cargo test --manifest-path vendor/vnc-rs/Cargo.toml --release --locked benchmark_buffered_inflate -- --ignored --nocapture
cargo test --release -p fjern benchmark_vnc_scaled_damage --locked -- --ignored --nocapture
cargo test --manifest-path vendor/minifb/Cargo.toml --release --locked --lib benchmark_fullscreen_submission -- --ignored --nocapture
cargo test --manifest-path vendor/vnc-rs/Cargo.toml --release --locked --lib benchmark_block_runs -- --ignored --nocapture
cargo build --release -p fjern --locked
python3 tools/vnc_pipeline_smoke.py target/release/fjern
python3 tools/vnc_pipeline_smoke.py target/release/fjern --4k-scroll
```

The graphical smoke test opens a native client against a loopback server. It
exercises large Raw updates, overlapping CopyRect, persistent ZRLE, an
ExtendedDesktopSize change and subsequent refresh dimensions. It deliberately
ends the server connection and checks client termination. A local run passed
and reported a maximum event batch of 2.32 ms. It does not compare screenshots
or measure scanout; pixel equivalence is covered by unit tests.

For real-server diagnostics:

```sh
FJERN_VNC_STATS=1 ./target/release/fjern vnc workstation.example 5900
```

Every two seconds, stderr reports completed server updates per second (including
empty updates), paint attempts per second, maximum UI event-batch time, total
scaling time in the interval, and whether a Raw rectangle remains pending.
Paint attempts are not confirmed compositor frames. These counters do not
measure network latency or isolate decoder queue wait time. Real WayVNC video
FPS still requires a repeatable workload and before/after comparison.

## RDP presentation

The default profile negotiates RGB565 bitmap updates with interleaved RLE and
fast-path output. This branch adds an optional [H.264 graphics profile](h264.md).
Client-side presentation optimizations do not
reduce network bandwidth or change the server's encoding rate.

The RGB565 bitmap path now traverses validated source and destination row
slices, avoiding repeated indexed bounds checks for each pixel. An isolated
release benchmark of 1920×1080 raw RGB565 bitmap updates, split into 16-row
packets to fit RDP's packet size, measured 14.481 → 11.329 ms per full frame
on the test machine. This is a 22% reduction in bitmap update CPU time for
that workload, not an observed remote FPS gain. Reproduce with:

```sh
cargo test --release -p linrdp-proto benchmark_rgb565_bitmap_update --locked -- --ignored --nocapture
```

The desktop receiver also walks all complete PDUs in a TLS read before moving
any trailing partial PDU. This avoids repeatedly shifting the remaining buffer
when the read contains several packets; the benefit depends on packet batching.

## Snapshot scheduling

The decoder applies every protocol update to its authoritative framebuffer.
When a batch is ready, the receiver composites the
changed pixel rows and cursor into a reusable staging buffer outside the display
mutex. Buffer ownership is swapped through one pending slot into the UI.
A newer eligible snapshot atomically replaces an unconsumed one, so a slow UI
does not first have to display the old queued image. Incoming protocol updates
are always decoded; only obsolete presentation snapshots are superseded.

Legacy bitmap updates do not provide a negotiated display-frame boundary.
Presentation waits for a 2 ms quiet gap without a partial packet/fast-path
fragment, or a 16 ms batch age under continuous traffic. Reads use a 2 ms budget
while batching instead of the usual idle 8 ms. Input and channel processing
continue between reads. The age limit is checked after decoding, not a hard
real-time deadline for native codec work. This reduces partial-scroll snapshots
but cannot guarantee atomic server frames on the legacy bitmap profile.

GFX EndFrame remains authoritative: completed frames bypass bitmap debounce.
In both modes, if an image is still pending, replacements are coalesced over
at least 8 ms to bound speculative snapshot work. No incomplete GFX frame is exposed.

Every snapshot retains the row generations actually stored in its allocation.
The generations travel with the pixels through the producer, pending slot and
UI, so a recycled snapshot catches up on all missed rows. Cursor backgrounds
are restored from the authoritative framebuffer before drawing the current
pointer; a cursor-only change no longer copies the whole desktop. Resets use
a new framebuffer identity, including same-area changes with a different stride.

GFX uses the same principle at EndFrame: it records surface row changes and
composes only rows missing from the selected output buffer. Mapping, deletion
and size changes invalidate the layout. Pixel storage and its generations are
exchanged together with the desktop; open frames remain invisible. These are
row-granular optimizations, not rectangle-perfect damage or zero-copy decoding.

AVC420 still decodes every H.264 reference update. Only the advertised regions
are converted from YUV directly into the persistent surface, after validating
all region bounds against both surface and decoded picture. This removes the
temporary full-frame RGB allocation and subsequent region copy. Conversion
retains the existing full-range BT.709 integer equations and padded strides.
GFX command bodies now borrow the reassembly buffer during processing rather
than allocating and copying every complete AVC/progressive command payload.
Fragmentation, message limits and error paths retain the same validation.

At the negotiated native resolution, the UI sends its pixel buffer directly to
the window backend. Scaling uses one horizontal lookup table per frame and
clears only letterbox borders. The native backend can still perform its own
copy into compositor storage; this is not an end-to-end zero-copy pipeline.

The UI polls at a target of 120 Hz and only uploads changed images or native
repaints. Network reads have an 8 ms idle poll budget, and TCP_NODELAY avoids
Nagle buffering of small outgoing input messages. These settings bound parts
of client scheduling, not network latency or actual remote FPS. Keyboard and
mouse-button transitions retain their existing ordered queues.

Encrypted desktop writes now run on a separate writer thread. The decoder owns
TLS state and enqueues ordered ciphertext into at most 256 blocks of 16 KiB
(4 MiB, plus one active block). Queue exhaustion is an explicit connection
error, never silent loss or a blocking enqueue. Each block has an absolute
five-second write deadline. Closing wakes the reader before joining the worker;
the writer gets a 100 ms graceful drain before socket shutdown cancels stalled
I/O. This bounds the write-related shutdown wait, not native codec execution.
The existing handshake/credential transport is unchanged.

Regression coverage includes three-buffer damage history, cursor restoration,
GFX frame boundaries/layout changes, AVC predictive references/odd region edges,
stalled output with live input, bounded writer shutdown and a verified TLS
round trip larger than the plaintext buffer limit.

Local release measurements for this follow-up (not remote FPS):

- GFX composition with one changed row: 0.010 ms at 1080p, 0.030 ms at 4K;
  full-height damage: 0.791 ms and 2.810 ms respectively (300 iterations).
- AVC conversion plus the former temporary-buffer copy: full 1080p
  9.299 → 8.043 ms; a 64×64 region within a 1080p decoded picture
  8.426 → 0.015 ms. Native H.264 decode time is excluded; conversion benchmarks
  alternate order over six batches of 20 calls and report the upper median.
- The existing 1024-delta/1080p snapshot benchmark measured 261.2 ms for
  per-delta full copies and 5.1 ms for 17 demand-produced, row-aware snapshots.
  This combines coalescing already present before this patch with row reuse;
  it is not an isolated comparison against the previous coalesced implementation.

```sh
cargo test --release -p linrdp-proto --locked benchmark_graphics_presentation -- --ignored --nocapture
cargo test --release -p linrdp-proto --locked benchmark_avc_region_conversion -- --ignored --nocapture
cargo test --release -p fjern --locked benchmark_snapshot_burst -- --ignored --nocapture
```

On Wayland, buffer ownership follows each `wl_buffer.release`: a buffer becomes
busy on submission, and resizing installs the replacement buffer's release
state. The compositor pool is capped at three buffers; when all are busy,
presentation is retried after event processing instead of allocating more
buffers or overwriting one still in use. Pending native configure events and
X11 exposure events request a repaint even if the remote image is unchanged.

## Reproducing CPU measurements

```sh
cargo test --release -p fjern benchmark_ -- --ignored --nocapture --test-threads=1
```

The viewport benchmark compares the previous renderer with the production
renderer, checks exact pixel equality, alternates execution order, and reports
the median of five batches of 100 frames. One development-machine run:

| Remote → window | Previous | Optimized |
| --- | ---: | ---: |
| 1920×1080 → 1920×1080 | 3.330 ms | 0.351 ms |
| 1920×1080 → 1280×720 | 1.533 ms | 0.534 ms |
| 1920×1080 → 1600×1000 | 2.313 ms | 0.806 ms |
| 1280×720 → 1920×1080 | 3.363 ms | 0.974 ms |

The actual viewer bypasses the native-size renderer entirely. A synthetic
1080p burst of 1024 deltas with UI consumption every 64 deltas reduced snapshot
copies from 1024 to 17 (343.4 ms to 8.7 ms in the same run). This intentionally
models a busy decoder and slower consumer; it is not a typical workload or
an end-to-end speedup claim. The regression also verifies accumulated pixels,
buffer ownership, dimensions after resize and input readiness.

The native Wayland lifecycle test also passed against the development machine's
compositor: it exhausted the three-buffer pool without dispatching releases,
then verified that event processing allowed the final image and a resized image
to reach busy shared-memory buffers. It does not measure physical scanout or
prove tear-free output on every compositor. Reproduce it in a graphical session
(it briefly opens a small test window):

```sh
cargo test --manifest-path vendor/minifb/Cargo.toml --locked --lib \
  native_wayland_busy_pool_retries_final_image -- --ignored --nocapture
```

These measurements exclude Windows encoding, network transfer, bitmap
decompression and compositor display timing. A before/after Windows scrolling
or video session has not yet established an actual FPS improvement.

## Better network compression

A new private codec cannot be used against an unmodified Windows RDP server.
The interoperable route is the RDP Graphics Pipeline Extension, including its
dynamic channel, capability negotiation, required codecs, surfaces, frame
acknowledgements and compression framing. H.264 modes are selected through
that extension's capabilities, independently of bitmap-codec capabilities.
See Microsoft's [graphics capability negotiation specification](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/31c6e2b1-335b-4a75-9454-bb2309958c21).

The experimental profile implements version 8.1 with software AVC420 decoding.
Hardware decoding and AVC444 remain future work. See [H.264](h264.md) for the
supported codecs and limits of current Windows validation.

## Native Wayland submission

The Wayland backend now submits packed window-sized input directly to its shared
memory writer. Previously it always ran the scalar resizer, even after the viewer
had already produced the exact window dimensions. This extra full-screen pass
affected both bitmap and experimental graphics sessions. Inputs with different
sizes or padded strides retain the existing scaling path.

An isolated optimized build of the previous C scaler took 3.107–3.688 ms per
1080p frame and 12.463–13.112 ms per 4K frame in three batches of 100 calls after
other builds completed. The new native-size path eliminates that pass; it still
copies into compositor storage. These measurements do not establish remote FPS.

A live Wayland regression checks the submitted pixel contents, verifies that the
intermediate scaling buffer is untouched for native-size input, and verifies the
padded-stride fallback. The existing compositor-buffer exhaustion and resize test
also passes. Run both with:

```sh
cargo test --manifest-path vendor/minifb/Cargo.toml --release native_ -- --ignored --nocapture --test-threads=1
```

For interactive performance testing, use the optimized binary:

```sh
cargo run --release -p fjern -- tui
```

A plain `cargo run` builds without release optimizations and is unsuitable for
comparing decoding or animation performance.
