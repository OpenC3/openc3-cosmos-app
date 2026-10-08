# openc3-cosmos-app assets

## logo.png, icon.png + icons/

- `logo.png` — the raw 1024×1024 OpenC3 mark on a transparent background (the
  source art).
- `icon.png` — the composed 1024×1024 **app tile** (the white mark on a rounded
  brand-blue tile). This is the master the installer icons are derived from.
- `icons/` — the per-platform installer icons referenced by
  `[package.metadata.packager].icons` in Cargo.toml:
  - `32x32.png`, `128x128.png`, `128x128@2x.png`, `256x256.png`, `512x512.png` —
    Linux (.deb / AppImage; AppImage needs a square PNG)
  - `icon.icns` — macOS (.app / .dmg)
  - `icon.ico` — Windows (.exe / .msi)

`tools/gen_app_icons.py` composes `logo.png` into the tile and regenerates the
whole set (needs `pillow`; `.icns` needs macOS `iconutil`). The tile color is
auto-sampled from the mark; tweak `MARGIN`/`RADIUS`/`MARK_FRACTION` in the script
to adjust the look.

```bash
uv run --with pillow python tools/gen_app_icons.py
```

Commit the regenerated `assets/icon.png` and `assets/icons/*`.

## tray.png

The **"COS/MOS" two-line badge** used for the system-tray / menu-bar icon
(Windows/macOS), decoded at runtime with the `image` crate. A purpose-built
badge is used rather than a downscaled `logo.png` (blurry at 16px). Regenerated
by `tools/gen_tray_icon.py` (needs `pillow`):

```bash
uv run --with pillow python tools/gen_tray_icon.py
```

Commit the regenerated `assets/tray.png`.
