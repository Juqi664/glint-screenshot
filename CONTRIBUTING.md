# Contributing to glint-screenshot

Thanks for your interest in improving glint-screenshot! This document explains
how to set up a development environment and the conventions to follow when
submitting changes.

## 🛠️ Development setup

You need an Ubuntu 24.04 (or similar GNOME/Wayland) machine with:

- Rust toolchain (stable, ≥ 1.75) — <https://rustup.rs>
- System libraries (Ubuntu package names):

  ```bash
  sudo apt install libgtk-4-dev libadwaita-1-dev libcairo2-dev \
                   libglib2.0-dev libpipewire-0.3-dev
  ```

- `cargo-deb` (only if you want to build the `.deb`):

  ```bash
  cargo install cargo-deb
  ```

Build and run in demo mode (no Portal permission needed, great for UI work):

```bash
cargo build
GLINT_DEMO=1 ./run.sh
```

For real-mode captures, always launch through `./run.sh` so GNOME authorizes the
Portal call (see the README's "GNOME scope authorization" note).

## ✅ Pre-commit checklist

CI enforces all of the following; please run them locally first:

```bash
cargo fmt --all            # formatting
cargo clippy --all-targets -- -D warnings   # lints, warnings are errors
cargo build --release       # the release profile is what we ship
cargo test --all            # unit tests (if any)
```

## 📬 Submitting changes

1. **Open an issue first** for non-trivial changes — a short design discussion
   saves everyone time.
2. Fork the repo and create a branch off `master`:
   `git checkout -b feat/my-feature`.
3. Make your change with focused, well-described commits
   ([Conventional Commits](https://www.conventionalcommits.org/) style is
   appreciated, e.g. `feat(pin): add double-click to close`).
4. Update `CHANGELOG.md` under the **[Unreleased]** section.
5. Open a Pull Request against `master` and fill in the PR template.
6. Make sure CI is green.

## 🧱 Code conventions

- **Edition 2021**, stable Rust. Avoid `unsafe` unless there is no alternative
  (the few existing `unsafe` FFI blocks in `pin_window.rs` and `screencast.rs`
  are documented — keep them minimal and commented).
- **Wayland-only.** Never add X11 code paths.
- **Comments**: write comments in **English**; explain *why*, not *what*.
- **Logging**: use the `log` crate macros (`log::info!` / `warn!` / `error!`),
  not `println!`.
- Keep the UI CSS in `src/style.css`; do not inline styles in code.

## 🐛 Reporting bugs

Open an issue using the **Bug report** template and include:

- GNOME/mutter version (`gnome-shell --version`)
- Whether you ran via `./run.sh` or the binary directly
- `RUST_LOG=debug ./run.sh` output around the failure
- Steps to reproduce

## 🎨 Screenshots / assets

When adding or changing UI, please attach a screenshot or short GIF to your PR
description so reviewers can see the result without building.

## 📜 License

By contributing you agree that your contributions will be licensed under the
[MIT License](LICENSE) that covers the project.
