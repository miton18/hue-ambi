# Hue Ambi

Philips Hue ambilight for Kodi on the Raspberry Pi 5: a Rust engine reads the DRM video plane and streams per-light colours to a Hue entertainment area; a Python Kodi add-on configures and supervises it. Read `docs/architecture.md` before touching capture, colour or streaming code.

## Layout

- `engine/`: Rust crate `hue-ambi` (`drm.rs` capture, `color.rs` colour pipeline, `hue.rs` REST + DTLS, `main.rs` loop and CLI). Target is static `aarch64-unknown-linux-musl`, OpenSSL vendored.
- `service.hueambi/`: the Kodi add-on, zipped as is. `bin/hue-ambi` is a build output, not committed.
- `build.sh`: `cargo test` + `cargo build --release` in `rust:1-alpine`, then the zip in `dist/`.

## Conventions

- Code comments, log messages and engine CLI output are in French; user-facing Kodi strings go through `resources/language/*/strings.po` (en_gb is the source, fr_fr the translation, ids 30000–30999). Never hard-code UI text in the Python.
- Settings use the legacy `settings.xml` format with numeric labels.
- Keep the hot path (`Capture::grab`, `encode`, `Stream::send`) allocation-free and free of syscalls beyond the per-frame DRM ioctls.
- Never cache mapped buffers by framebuffer id: Kodi recycles fb ids every frame. Key by dma-buf inode.
- Never read debugfs in the per-frame path (it takes all modeset locks).
- Never commit bridge keys, clientkeys, entertainment area ids or LAN addresses. Put machine-specific notes in `CLAUDE.local.md` (gitignored).

## Testing

- `cargo test` covers colour maths, HueStream framing and parsing; it runs inside `build.sh`.
- `hue-ambi dtls-test <ip> <identity> <psk_hex> <area_id>` exercises the DTLS stream against any PSK server, e.g. `sleep 60 | openssl s_server -dtls1_2 -accept 2100 -nocert -psk <hex> -cipher PSK-AES128-GCM-SHA256` (s_server exits when its stdin closes).
- `hue-ambi test` on the device prints the light → zone mapping and, while a video plays, benchmarks capture in the three cache-sync modes.
- Playing a video on a Kodi box with HDMI-CEC enabled may switch on the TV and amplifier: don't start playback remotely unless someone is in front of it.
