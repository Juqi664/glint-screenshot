# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- Save now opens a native path picker (FileDialog). The previous async
  future panicked inside the selector's nested MainLoop, so no dialog
  appeared. Cancel keeps the selector open.

## [0.1.1] - 2026-08-03

### Added
- Per-pin close button (X overlay) so a single pinned image can be removed
  without affecting the others.
- Runtime layer-shell detection: the pin window falls back to a normal
  borderless window on compositors without wlr-layer-shell (e.g. GNOME),
  using gdk_toplevel_begin_move for drag-to-move.
- Multi-monitor selection: one fullscreen overlay per GdkMonitor so every
  display in the virtual desktop can be selected (fixes Wayland's
  single-monitor fullscreen limitation).

### Changed
- Selection redraw coalesces to one pass per main-context iteration, paints
  pre-cropped per-monitor backgrounds, and skips full redraws while only
  the cursor shape needs updating.
- README / CONTRIBUTING / CHANGELOG wording trimmed to a plainer tone.

## [0.1.0] - 2026-07-29

First release.

### Added
- Region selection with a live magnifier and color picker (Cairo pixel
  sampling).
- Annotation tools: rectangle, ellipse, line, arrow, freehand brush, mosaic,
  and text, with a floating toolbar and color / stroke picker.
- Pin-to-screen: borderless always-on-top window (layer-shell where supported)
  with drag-to-move, scroll-to-scale, and Esc / X to close.
- Copy to clipboard (Ctrl+C) and save to file (Ctrl+S).
- Daemon mode (--daemon) with Unix-socket IPC and a --trigger client.
- Global GNOME keybinding Ctrl+Alt+A (registered by glint-setup-shortcut).
- XDG Desktop Portal capture via ashpd with a GNOME Shell D-Bus fallback.
- Fast PNG decode via zune-png; optional PipeWire ScreenCast fast path
  (GLINT_USE_SCREENCAST=1).
- Demo mode (GLINT_DEMO=1) using a generated test image.
- Debian packaging via cargo-deb, autostart entry, app icon, and postinst
  scripts; bundles libgtk4-layer-shell.
- Project scaffolding: README, LICENSE (MIT), CONTRIBUTING, CODE_OF_CONDUCT,
  CHANGELOG, CI, issue/PR templates.

[Unreleased]: https://github.com/Juqi664/glint-screenshot/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/Juqi664/glint-screenshot/releases/tag/v0.1.1
[0.1.0]: https://github.com/Juqi664/glint-screenshot/releases/tag/v0.1.0
