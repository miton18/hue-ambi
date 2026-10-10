# Architecture

Hue Ambi has two parts:

- **`engine/`**: `hue-ambi`, a Rust binary that captures the video, computes one colour per light and streams it to the Hue bridge. Everything latency-sensitive lives here.
- **`service.hueambi/`**: a Kodi service add-on in Python, laid out the LibreELEC way: `default.py` only adds `resources/lib` to the path and calls `hueambi_service.run()`. The add-on owns the settings UI (pairing, entertainment area choice, tuning), writes the engine configuration file and supervises the engine process (start, restart with backoff, stop on Kodi exit). `resources/lib/actions.py` runs the settings buttons (`RunScript`).

The engine source files:

| File | Role |
|---|---|
| `src/drm.rs` | finds the video plane, maps its framebuffer, decodes pixels |
| `src/color.rs` | YCbCr → linear RGB (SDR and HDR10), encoding for the lights |
| `src/hue.rs` | Hue CLIP v2 REST over HTTPS, pairing, Entertainment DTLS stream |
| `src/main.rs` | configuration, light → picture zone mapping, main loop, CLI |

## Video capture

### Where the picture is

With Kodi's DRM PRIME renderer in *Direct To Plane* mode, a decoded frame is a dma-buf exported by the HEVC/H.264 decoder. Kodi wraps it in a DRM framebuffer (`drmModeAddFB2WithModifiers`) and attaches it to a hardware plane of the display controller (`vc4` on the Pi 5). OpenGL never sees it, which is why `RenderCapture` returns nothing.

The engine reads it back through the same kernel API, as root:

1. At start-up, every `/dev/dri/cardN` is opened and `DRM_IOCTL_MODE_GETPLANERESOURCES` lists its planes. Render-only GPUs (`v3d`) have none and are skipped. Card numbers depend on driver probe order, so nothing is hard-coded: on a Pi 5 this finds `card1 (vc4, 56 planes)`.
2. `DRM_IOCTL_MODE_GETPLANE` on each plane gives its current `fb_id`, then `DRM_IOCTL_MODE_GETFB2` gives the pixel format. The plane whose framebuffer is YUV (`P030`, `NV12`, `YU12`, `P010`) is the video; the GUI is on an `AR24` plane.
3. Per frame: `GETPLANE` → `GETFB2` (returns GEM handles because we are root) → `DRM_IOCTL_PRIME_HANDLE_TO_FD` → `mmap(PROT_READ)` of the dma-buf. GEM handles are closed right away; the dma-buf fd keeps the buffer alive.

### Framebuffer ids are recycled every frame

Kodi creates a framebuffer when a buffer is displayed and removes it when the next one is shown, and the kernel hands out the lowest free id again. A given `fb_id` therefore points to a different buffer almost every frame. Caching mappings by `fb_id` would read stale pictures, or keep freed decoder buffers (16 MB each in 4K) alive in CMA memory.

Mappings are therefore cached by **the inode of the dma-buf**, which identifies the underlying buffer. The decoder works from a fixed pool of buffers, so the cache stays small. An entry not seen for 150 frames is unmapped, and everything is released when playback stops.

### debugfs only when needed

The plane's source rectangle (letterboxing) and its colour encoding (`BT.2020` → HDR) are read from `/sys/kernel/debug/dri/<N>/state`, N being the card number. Producing that file takes all modeset locks, so reading it on every frame could delay page flips. It is read only when the plane, frame size or pixel format changes.

### Pixel formats

The Pi 5 decoder outputs **Broadcom SAND128** tiled buffers (modifier vendor `0x07`, type `4`): the picture is cut into vertical columns 128 bytes wide; each column stores all its luma rows, then all its chroma rows. The column height in rows is encoded in the modifier (`(modifier & ((1<<56)-1)) >> 8`).

For `P030` (10-bit, used for HEVC Main10/HDR), each 32-bit little-endian word packs three 10-bit samples, so a column holds 96 pixels:

```text
column  = x / 96,  xi = x % 96
luma    = column * 128 * col_height + offset[0] + y * 128 + (xi / 3) * 4,  shift 10 * (xi % 3)
chroma  = column * 128 * col_height + offset[1] + (y / 2) * 128,  Cb sample 2k, Cr sample 2k+1, k = xi / 2
```

`NV12` SAND is the same with one byte per sample (128 pixels per column). Linear `NV12`, `YU12` and `P010` use the pitches from `GETFB2`.

Each light zone is sampled on a 12 × 8 grid: 96 pixels per light, i.e. 288 word reads in `P030` (Y, Cb, Cr).

### Cache coherency

The decoder writes the buffers by DMA. If the mapping is CPU-cacheable, the CPU may keep serving cache lines from an earlier frame in the same buffer, so the colours would freeze.

- `sync=dc` (default): the engine cleans and invalidates only the cache lines it is about to read, with `dc civac`, which Linux allows from user space on arm64. It does it in two passes over the sample grid: first issue every `dc civac`, then a single `dsb ish`, then do all the reads. One function computes the addresses for both passes (`Frame::yuv_with`), so they always match.
- `sync=dmabuf`: `DMA_BUF_IOCTL_SYNC` around the reads, the portable method. Depending on the exporter it may flush the whole 16 MB buffer.
- `sync=none`: no maintenance, for comparison.

`hue-ambi test` benchmarks the three modes on the frame currently on screen.

**Measured so far**: version 0.1.0 (Pi 5, 5 lights) spent 7–11 ms per update. It issued a `dsb ish` after each of the ~1,400 `dc civac` of a frame, serialising them; 0.2.0 batches them as described above. The engine now logs capture time and calculation-plus-send time separately every 60 s, and `hue-ambi test` compares the three modes on a playing video; the new figures are still to be measured.

## Colour

`color.rs` turns limited-range YCbCr into **linear light** where 1.0 means reference white:

- **SDR**: BT.709 matrix, then gamma 2.4.
- **HDR10**: BT.2020 non-constant-luminance matrix, then the SMPTE ST 2084 (PQ) EOTF to nits, divided by 203 nits, the HDR reference white of ITU-R BT.2408. That level is 58 % of the PQ signal, 10-bit code 572. The result then goes through the BT.2020 → BT.709 primaries matrix.

Each zone is averaged **in linear light**, so a small bright highlight weighs what it physically weighs, then:

1. temporal smoothing: exponential moving average, factor `smoothing`;
2. saturation around the BT.709 luminance;
3. `gain`;
4. a soft knee: linear up to 0.7, then `0.7 + 0.3 · (1 − e^(−(v − 0.7)/0.3))`, which approaches 1.0. HDR highlights brighten the lights without clipping a whole zone to white;
5. gamma `1/2.2` and 16-bit scaling for the Hue stream.

## Light → picture mapping

The entertainment area defines each light ("channel") position with `x` from −1 (left) to +1 (right), `y` (depth, ignored) and `z` from −1 (bottom) to +1 (top). A light maps to the picture point `u = (x + 1) / 2`, `v = (1 − z) / 2`. Its zone is a box of ±0.2 × ±0.25 around that point, clamped to the picture, so a light at a corner samples the corner and a light at the top centre samples the top band.

## Hue Entertainment streaming

1. **Pairing** (`POST /api` with `generateclientkey: true` after a button press) returns an application key and a 16-byte `clientkey`.
2. The **PSK identity** for DTLS is the `hue-application-id` response header of `GET /auth/v1`.
3. `PUT /clip/v2/resource/entertainment_configuration/<id>` with `{"action":"start"}` puts the area in streaming mode.
4. **DTLS 1.2** to UDP port 2100, cipher `TLS_PSK_WITH_AES_128_GCM_SHA256`, PSK = `clientkey`.
5. One **HueStream v2** message per frame:

   ```text
   "HueStream"                     9 bytes
   02 00                           version 2.0
   seq                             1 byte, wraps
   00 00                           reserved
   00                              colour space: RGB
   00                              reserved
   <entertainment area id>         36 ASCII bytes
   then per light: channel id (1 byte) + R, G, B (3 × 16-bit big endian)
   ```

6. `{"action":"stop"}` when the video ends or the engine receives SIGTERM.

The bridge drops the stream after about 10 s without packets. The engine keeps sending while a video is on screen, including when paused.

## Main loop and timing

```text
idle: every 250 ms, look for a YUV plane (a few ioctls, no debugfs)
video found: start streaming (HTTPS) + DTLS handshake
each 1/fps:  grab → smooth → encode → send
no frame for fps/2 consecutive ticks (0.5 s): stop streaming, release buffers, back to idle
```

The engine raises its priority to nice −5. It uses `SCHED_OTHER`, not real time, so it never competes with Kodi's video pipeline. Every 60 s it logs to the Kodi log the achieved frame rate, the capture time per frame, and the calculation-plus-send time per frame.
