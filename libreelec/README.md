# LibreELEC package

`hueambi/` is the recipe for the [LibreELEC add-on repository](https://github.com/LibreELEC/LibreELEC.tv/tree/master/packages/addons). It goes to `packages/addons/service/hueambi/` in LibreELEC.tv and produces the add-on `service.hueambi`, enabled for Raspberry Pi 5 builds (`PKG_ADDON_PROJECTS="RPi5"`).

Differences from the zip published on GitHub:

- The engine is built by LibreELEC's toolchain, `cargo build --no-default-features`, and links LibreELEC's OpenSSL instead of embedding it.
- `addon.xml` is generated from `package.mk` with LibreELEC's `xbmc.service` template: version `<LibreELEC version>.<PKG_REV>`, English metadata, `default.py` entry point, icon from `icon/icon.png`.
- The Python side (`default.py`, `resources/`) is copied unchanged from the release tarball.

## Release checklist

1. Tag the release in this repository (`vX.Y.Z`) so that `https://github.com/miton18/hue-ambi/archive/vX.Y.Z.tar.gz` exists.
2. Set `PKG_VERSION`, and put the tarball checksum in `PKG_SHA256`:

   ```bash
   curl -sL https://github.com/miton18/hue-ambi/archive/vX.Y.Z.tar.gz | sha256sum
   ```

3. Copy `libreelec/hueambi/` to `LibreELEC.tv/packages/addons/service/hueambi/`, add `icon/icon.png` (from `service.hueambi/resources/icon.png`), then build:

   ```bash
   PROJECT=RPi DEVICE=RPi5 ARCH=aarch64 scripts/create_addon hueambi
   ```

4. Install the resulting zip on a Pi 5 and check `kodi.log`, then open the pull request against LibreELEC.tv.
