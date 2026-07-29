# Packaging

Assets and scripts used to build distributable packages for glint-screenshot.

## Debian (`.deb`)

Built with [`cargo-deb`](https://github.com/kornelski/cargo-deb):

```bash
cargo install cargo-deb
cargo deb
# → target/debian/glint-screenshot_<ver>_amd64.deb
sudo dpkg -i target/debian/glint-screenshot_*.deb
glint-setup-shortcut   # register the Ctrl+Alt+A GNOME keybinding
```

### What the package ships

| File | Destination | Purpose |
|---|---|---|
| `target/release/glint-screenshot` | `/usr/bin/glint-screenshot` | The binary |
| `io.github.glint.Screenshot.desktop` | `/usr/share/applications/` | App launcher (`--trigger`) |
| `autostart/io.github.glint.Screenshot.daemon.desktop` | `/etc/xdg/autostart/` | Starts the daemon at login (`--daemon`) |
| `icons/hicolor/scalable/apps/io.github.glint.Screenshot.svg` | `/usr/share/icons/hicolor/scalable/apps/` | App icon |
| `glint-setup-shortcut.sh` → `glint-setup-shortcut` | `/usr/bin/` | Registers the GNOME custom keybinding |
| `libs/libgtk4-layer-shell.so.0` | `/usr/lib/x86_64-linux-gnu/` | Bundled layer-shell lib (see below) |
| `debian/postinst` / `postrm` | maintainer scripts | `ldconfig`, icon/desktop cache refresh |

## Regenerating the bundled `libgtk4-layer-shell`

`libgtk4-layer-shell` is **not** in Ubuntu's apt repositories, so the `.deb`
bundles a prebuilt `libgtk4-layer-shell.so.0` to stay self-contained. If you
prefer to rebuild it from source (recommended for reproducible/verified
builds):

```bash
# Build dependencies
sudo apt install meson ninja-build libgtk-4-dev

git clone https://github.com/wmww/gtk4-layer-shell.git
cd gtk4-layer-shell
meson setup build --prefix=/usr/local
ninja -C build
sudo ninja -C build install
sudo ldconfig

# Copy the resulting soname link into packaging/libs/
cp -L /usr/local/lib/x86_64-linux-gnu/libgtk4-layer-shell.so.0 \
      packaging/libs/libgtk4-layer-shell.so.0
```

The CI workflow (`.github/workflows/ci.yml`) performs this same build from
source so the Rust crate links against a freshly built library.
