# Security Policy

## Supported versions

glint-screenshot is pre-1.0 software. Security fixes are applied only to the
latest `master` / release.

| Version | Supported |
|---------|----------|
| latest  | ✅        |
| < latest | ❌       |

## Reporting a vulnerability

If you discover a security vulnerability, **please do not open a public issue**.

Instead, email the maintainer at **Juqi664@users.noreply.github.com** with:

- a description of the issue and its potential impact,
- steps to reproduce (proof-of-concept if possible),
- the GNOME/mutter version and OS you tested on.

You should receive an acknowledgment within 72 hours. Once the issue is
triaged and a fix is prepared, we will publish a patched release and credit
you (unless you prefer to remain anonymous).

## Scope

This project captures screen contents via the XDG Desktop Portal (which itself
shows the user a permission prompt) and the GNOME Shell D-Bus interface. It
does not transmit any data over the network. Reported issues should relate to
local privilege/permission handling, unsafe code, or handling of untrusted
image data.

## Known security-relevant notes

- The few `unsafe` FFI blocks (`pin_window.rs`, `screencast.rs`) are reviewed
  to stay within the documented `gdk_toplevel_begin_move` / PipeWire contracts.
- The `.deb` bundles a prebuilt `libgtk4-layer-shell.so.0`. See
  `packaging/README.md` for how to rebuild it from source if you prefer not to
  trust the bundled binary.
