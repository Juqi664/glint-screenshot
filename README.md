<div align="center">

# glint-screenshot

**An open-source Ubuntu / GNOME / Wayland-native screenshot & pinning tool.**

Faithfully reproduces the Windows WeChat screenshot interactions — region
selection with a magnifier, on-the-fly annotation, copy / save, and "pin to
screen" — built entirely on Wayland APIs (no X11).

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2021-orange.svg)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/platform-GNOME%20%7C%20Wayland-9cf)](#compatibility)
[![PRs welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](CONTRIBUTING.md)

</div>

---

## ✨ Features

- **Region selection** with a live **magnifier** and **color picker** (Cairo
  pixel sampling), just like the WeChat screenshot.
- **Annotation tools**: rectangle, ellipse, line, arrow, freehand brush,
  mosaic/blur, and text — with a floating toolbar and color / stroke picker.
- **Pin to screen**: drop the selected region as a borderless, always-on-top
  floating window. Drag to move, scroll to scale, click ✕ (or press `Esc`) to
  close — each pin is independent and can be removed individually.
- **Copy to clipboard** (`Ctrl+C`) or **save to file** (`Ctrl+S`).
- **Daemon mode** + **global hotkey** (`Ctrl+Alt+A`): a background daemon stays
  alive so the shortcut triggers a capture instantly and the clipboard
  persists (Wayland clipboards are volatile — see [Limitations](#limitations)).
- **Wayland-native**: uses the XDG Desktop Portal (`ashpd`) and the GNOME Shell
  D-Bus interface for capture; `gtk4-layer-shell` for pinning where the
  compositor supports it. **No X11 code paths.**
- **Fast capture path**: SIMD-accelerated `zune-png` decoder; an optional
  PipeWire ScreenCast fast path (no PNG encode/round-trip) is available behind
  a flag.

## 📷 Screenshots

### Selection & annotation toolbar

Drag to select a region; the rest of the screen dims. The floating toolbar
offers shapes, brush, arrow, mosaic, text, a color palette, stroke thickness,
undo/redo, and the primary actions — **Copy**, **Save**, **Pin** (green), and
**✕** to cancel.

![Selection with the floating annotation toolbar](docs/screenshots/selection-toolbar.png)


## 🖥️ Compatibility

| Compositor | Screenshot | Pin (always-on-top) | Drag pin |
|---|---|---|---|
| GNOME 46+ (mutter) | ✅ via XDG Portal | ⚠️ no `wlr-layer-shell` — pin is a normal borderless window, brought to front on present | ✅ via `xdg_toplevel.move` |
| Sway / Hyprland / KDE | ✅ via XDG Portal | ✅ `gtk4-layer-shell` overlay layer | ✅ via layer margins |

The project targets **Ubuntu 24.04 LTS / GNOME 46 / Wayland** as the primary
platform. Other `wlr-layer-shell`-capable compositors should work for the pin
window's always-on-top behavior.

## 📦 Installation

### Option A — `.deb` package (Ubuntu)

```bash
cargo install cargo-deb
cargo deb                                # → target/debian/glint-screenshot_0.1.0_amd64.deb
sudo dpkg -i target/debian/glint-screenshot_0.1.0_amd64.deb
glint-setup-shortcut                     # register the Ctrl+Alt+A global keybinding
```

The `.deb` bundles `libgtk4-layer-shell` (not in Ubuntu apt repos) so it is
self-contained. Re-log-in (or run `glint-screenshot --daemon`) to start the
autostart daemon.

### Option B — Build from source

Runtime dependencies (Ubuntu 24.04 package names):

```
libgtk-4-1 (>= 4.14)  libadwaita-1-0 (>= 1.5)  libcairo2 (>= 1.18)
libglib2.0-0 (>= 2.80)  libpipewire-0.3-0 (>= 0.3.70)
```

```bash
cargo build --release
```

For real-mode captures GNOME must identify the app by its `systemd --user`
scope, so always launch via the bundled helper (see [Usage](#-usage)) rather
than running the binary directly from a terminal.

## 🚀 Usage

`run.sh` launches the app inside its own `app-io.github.glint.Screenshot-*.scope`
via `systemd-run --user --scope`, which is what lets the XDG Portal authorize
the screenshot.

```bash
./run.sh                 # single-shot capture (default)
./run.sh --daemon        # start the background daemon (also auto-started at login)
./run.sh --trigger       # signal the daemon to capture (bound to Ctrl+Alt+A)
GLINT_DEMO=1 ./run.sh    # demo mode: colorful test image, no Portal permission needed
```

### ⌨️ Shortcuts

| Shortcut | Action |
|---|---|
| `Ctrl+Alt+A` | Trigger a capture (global GNOME keybinding, registered by `glint-setup-shortcut`) |
| `Ctrl+C` | Copy the selected region to the clipboard |
| `Ctrl+S` | Save the selected region to a file |
| `Ctrl+P` | Pin the selected region to the screen |
| `Esc` | Cancel selection / close the focused pin window |

### 🔧 Environment variables

| Variable | Default | Description |
|---|---|---|
| `RUST_LOG` | `info` | Log level (`debug` for verbose capture/Portal diagnostics) |
| `GLINT_DEMO` | unset | Set to `1` to use a generated test image instead of the Portal |
| `GLINT_USE_SCREENCAST` | unset | Set to `1` to attempt the PipeWire ScreenCast fast path (experimental; falls back to the Portal on failure) |

## 🏗️ Architecture

```
src/
├── main.rs          Entry point: CLI parsing, GTK init, daemon/single-shot flow
├── lib.rs           Library root, re-exports modules
├── screenshot.rs    Screenshot service: XDG Portal / GNOME D-Bus → RGBA → Cairo ImageSurface
├── screencast.rs    Optional PipeWire ScreenCast fast path (single-frame capture)
├── selector.rs      Selection UI: fullscreen input-transparent window, mask, magnifier, color picker
├── pin_window.rs    Pin window: layer-shell overlay (or normal borderless window on GNOME)
├── tools/mod.rs     Drawing tools + command stack (rect/ellipse/line/arrow/brush/mosaic/text)
├── ui/              Floating toolbar and preferences
│   ├── mod.rs
│   └── toolbar.rs
└── style.css        Custom CSS for the toolbar and selection visuals
```

**Capture flow:** `main.rs` → `screenshot.rs` (Portal → PNG → `zune-png` decode
→ Cairo `ImageSurface`) → `selector.rs` (fullscreen selection + annotation) →
copy / save / `pin_window.rs`.

**Daemon vs single-shot:** in daemon mode a `gio::SocketService` listens on a
Unix socket; `--trigger` writes one byte to wake it. `Application::hold()` keeps
the daemon alive across captures so the Wayland clipboard isn't lost.

## ⚠️ Limitations

- **Always-on-top on GNOME**: GNOME/mutter does not implement the
  `wlr-layer-shell` protocol, so a pinned window is a normal borderless window
  that is brought to the front on `present()` but is not strictly enforced
  above other windows. On Sway/Hyprland/KDE the overlay layer is used.
- **Clipboard persistence**: on Wayland the clipboard is owned by the setting
  process and is lost when it exits. Single-shot mode keeps the app alive for
  10s after a copy; daemon mode holds the clipboard indefinitely.
- **PipeWire fast path**: experimental and flaky in-process; disabled by
  default. The Portal path (~600ms) is the reliable default.
- **GNOME scope authorization**: the Portal `Screenshot` call is authorized
  based on the app's `app-<id>-*.scope`. Launching the binary directly from a
  terminal (outside such a scope) may be denied — use `run.sh`.

## 🤝 Contributing

Contributions are welcome! Please read [CONTRIBUTING.md](CONTRIBUTING.md) and
the [Code of Conduct](CODE_OF_CONDUCT.md). Run `cargo fmt` and `cargo clippy`
before opening a PR (enforced by CI).

## 📄 License

Licensed under the [MIT License](LICENSE).

Copyright © 2026 Juqi664 and Glint Screenshot Contributors.
