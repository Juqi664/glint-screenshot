# Contributing to glint-screenshot

This document covers the dev setup and the conventions to follow when
submitting changes.

## Development setup

You need an Ubuntu 24.04 (or similar GNOME/Wayland) machine with:

- Rust toolchain (stable, >= 1.75) — <https://rustup.rs>
- System libraries (Ubuntu package names):

      sudo apt install libgtk-4-dev libadwaita-1-dev libcairo2-dev \
                       libglib2.0-dev libpipewire-0.3-dev

- cargo-deb, only if you want to build the .deb:

      cargo install cargo-deb

Build and run in demo mode (no Portal permission needed, good for UI work):

    cargo build
    GLINT_DEMO=1 ./run.sh

For real captures, always launch through ./run.sh so GNOME authorizes the
Portal call (see the README's note on the systemd scope).

## Pre-commit checklist

CI enforces all of the following; run them locally first:

    cargo fmt --all
    cargo clippy --all-targets -- -D warnings
    cargo build --release
    cargo test --all

## Submitting changes

1. Open an issue first for non-trivial changes — a short design discussion
   saves everyone time.
2. Fork the repo and branch off main: git checkout -b feat/my-feature.
3. Make your change with focused, well-described commits. Conventional Commits
   style is appreciated, e.g. feat(pin): add double-click to close.
4. Update CHANGELOG.md under the [Unreleased] section.
5. Open a pull request against main and fill in the PR template.
6. Make sure CI is green.

## Code conventions

- Edition 2021, stable Rust. Avoid unsafe unless there's no alternative. The
  few existing unsafe FFI blocks in pin_window.rs and screencast.rs are
  documented — keep them minimal and commented.
- Wayland-only. Never add X11 code paths.
- Comments in English; explain why, not what.
- Logging via the log crate (log::info! / warn! / error!), not println!.
- Keep UI CSS in src/style.css; don't inline styles in code.

## Reporting bugs

Open an issue using the Bug report template and include:

- GNOME/mutter version (gnome-shell --version)
- Whether you ran via ./run.sh or the binary directly
- RUST_LOG=debug ./run.sh output around the failure
- Steps to reproduce

## Screenshots / assets

When adding or changing UI, attach a screenshot or short GIF to your PR
description so reviewers can see the result without building.

## License

By contributing you agree that your contributions are licensed under the
[MIT License](LICENSE) that covers the project.
