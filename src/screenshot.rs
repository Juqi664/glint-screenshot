//! Screenshot service module.
//!
//! Obtains full-screen RGBA image data via the **XDG Desktop Portal**
//! (preferred) or the **GNOME Shell D-Bus** interface (fallback) and wraps it
//! as a Cairo [`ImageSurface`](cairo::ImageSurface) for uniform use by the
//! selector, magnifier, and pin-window modules.
//!
//! ## Why not read DRM / `/dev/fb0` directly?
//! Under Wayland, clients have **no** direct access to the screen framebuffer
//! (this is the whole point of Wayland's security model). The only legitimate
//! ways to capture the full screen are via compositor-authorized interfaces:
//! - the XDG Desktop Portal `Screenshot` interface (cross-desktop standard);
//! - GNOME Shell's private D-Bus interface `org.gnome.Shell.Screenshot`
//!   (GNOME-only, faster).
//!
//! ## Portal vs D-Bus trade-offs
//! | Aspect     | Portal (`ashpd`)                  | GNOME Shell D-Bus (`zbus`)         |
//! |------------|-----------------------------------|------------------------------------|
//! | Cross-DE   | supports KDE/Sway/Hyprland...     | GNOME only                         |
//! | User prompt| shows an "allow screenshot" dialog | none (needs non-interactive setting) |
//! | Performance| medium (one extra D-Bus round-trip)| high (reads Mutter internals)      |
//!
//! We therefore default to the Portal and fall back to D-Bus for maximum
//! compatibility.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use ashpd::desktop::screenshot::Screenshot;
use cairo::ImageSurface;
use gdk4::prelude::TextureExt;
// Demo mode needs DisplayExt (monitors), ListModelExt (item), Cast (downcast)
use gdk4::prelude::DisplayExt as _;
use gdk4::prelude::MonitorExt as _;
use gio::prelude::FileExt as _;
use gio::prelude::ListModelExt as _;
use glib::prelude::Cast as _;

/// The screenshot service. Stateless, safe to share across threads.
#[derive(Debug, Clone)]
pub struct ScreenshotService {
    /// Whether falling back to GNOME Shell's private D-Bus interface is allowed
    allow_gnome_dbus_fallback: bool,
}

impl ScreenshotService {
    /// Create a screenshot service with default configuration.
    pub fn new() -> Self {
        Self {
            allow_gnome_dbus_fallback: true,
        }
    }

    /// Capture the full screen and return a Cairo `ImageSurface` (`ARGB32`).
    ///
    /// Order: Portal -> on failure, try GNOME Shell D-Bus.
    ///
    /// `identifier`: the caller should pass a `WindowIdentifier` for an
    /// **already realized** window; the Portal needs it as the parent of its
    /// authorization dialog. When `None`, the Portal may fail due to the
    /// missing parent window.
    pub async fn capture_full_screen(
        &self,
        identifier: Option<ashpd::WindowIdentifier>,
    ) -> Result<ImageSurface> {
        // Demo mode: when GLINT_DEMO=1, generate a colorful gradient test image,
        // bypassing Portal/D-Bus so the selector/magnifier/mask interactions
        // can be verified without screenshot permission.
        if std::env::var("GLINT_DEMO").ok().as_deref() == Some("1") {
            log::info!("[demo mode] generating test gradient image, skipping Portal/D-Bus");
            return Ok(demo_surface());
        }

        let t0 = std::time::Instant::now();
        match self.capture_via_portal(identifier).await {
            Ok(s) => {
                log::info!("Portal capture done in {:?}", t0.elapsed());
                Ok(s)
            }
            Err(e) => {
                log::warn!(
                    "Portal screenshot failed in {:?}, trying GNOME Shell D-Bus fallback: {e}",
                    t0.elapsed()
                );
                if self.allow_gnome_dbus_fallback {
                    self.capture_via_gnome_dbus().await
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Capture the full screen via the XDG Desktop Portal.
    ///
    /// `interactive=false` means we do not force the interactive selection
    /// window to appear (the Portal decides whether user confirmation is
    /// needed). On GNOME, the first call shows an "allow screenshot"
    /// authorization dialog; the grant is cached afterwards.
    ///
    /// Permission is granted lazily: we attempt the capture first, and only
    /// on failure do we write the `screenshot=yes` PermissionStore entry and
    /// retry once. This avoids a D-Bus round-trip on every launch.
    async fn capture_via_portal(
        &self,
        identifier: Option<ashpd::WindowIdentifier>,
    ) -> Result<ImageSurface> {
        log::info!("Requesting full-screen screenshot via XDG Desktop Portal...");
        match self.portal_request(identifier).await {
            Ok(s) => Ok(s),
            Err(e) => {
                log::warn!("Portal capture failed ({e}); granting permission and retrying once");
                if let Err(pe) = ensure_portal_permission().await {
                    log::warn!("ensure_portal_permission failed: {pe}");
                }
                // Retry without an identifier: once the PermissionStore entry
                // is in place, GNOME's non-interactive Portal accepts a call
                // with no parent window. (WindowIdentifier is not Clone, so we
                // cannot reuse the original; None is the correct retry value.)
                self.portal_request(None).await
            }
        }
    }

    /// Issue a single Portal screenshot request and decode the result.
    async fn portal_request(
        &self,
        identifier: Option<ashpd::WindowIdentifier>,
    ) -> Result<ImageSurface> {
        let t0 = std::time::Instant::now();
        // ashpd 0.10 uses the builder pattern: Screenshot::request() -> ScreenshotRequest
        // .identifier(...) sets the parent window (optional for non-interactive
        //   captures; GNOME's Portal ignores it when no dialog is shown)
        // .interactive(false) does not show the interactive selection window
        // .modal(true) makes the authorization dialog modal
        let mut request = Screenshot::request().interactive(false).modal(true);
        if let Some(id) = identifier {
            request = request.identifier(id);
        }
        let response = request
            .send()
            .await
            .context("Portal screenshot request rejected or failed")?
            .response()
            .map_err(|e| anyhow!("Portal screenshot response error: {e}"))?;
        log::info!("Portal DBus round-trip: {:?}", t0.elapsed());

        // The Portal returns a URI (e.g. `file:///run/user/1000/.../screenshot.png`)
        let uri = response.uri();
        log::debug!("Portal returned screenshot URI: {uri}");

        // Decode the PNG file directly with Cairo, skipping the intermediate
        // gdk4::Texture (which would force a PNG re-encode + re-decode). This
        // is one PNG decode instead of decode -> re-encode -> decode.
        let t1 = std::time::Instant::now();
        let file = gio::File::for_uri(uri.as_str());
        let path = file
            .path()
            .ok_or_else(|| anyhow!("Portal returned a non-local URI: {uri}"))?;
        let bytes = std::fs::read(&path)
            .with_context(|| format!("Failed to read screenshot file {}", path.display()))?;
        log::info!("Read PNG file ({} bytes): {:?}", bytes.len(), t1.elapsed());

        let t2 = std::time::Instant::now();
        let surface = png_bytes_to_surface(&bytes).or_else(|e| {
            log::warn!("zune-png decode failed ({e:?}); falling back to Cairo PNG decoder");
            ImageSurface::create_from_png(&mut std::io::Cursor::new(bytes))
                .map_err(|e| anyhow!("Failed to decode PNG into ImageSurface: {e}"))
        })?;
        log::info!("PNG decode: {:?}", t2.elapsed());
        Ok(surface)
    }

    /// Capture the full screen via GNOME Shell's private D-Bus interface.
    ///
    /// Interface: `org.gnome.Shell.Screenshot`
    /// Actual method signature (GNOME 46+): `Screenshot(in b include_cursor, in b flash, in s filename,
    ///                                          out b success, out s filename_used)`
    /// Note: the third argument is a **string filename** (not a bool inhibit),
    /// unlike older versions. An empty string makes GNOME generate a temp
    /// filename automatically.
    async fn capture_via_gnome_dbus(&self) -> Result<ImageSurface> {
        log::info!("Requesting full-screen screenshot via GNOME Shell D-Bus...");

        // zbus connection
        let connection = zbus::Connection::session()
            .await
            .context("Failed to connect to the session bus")?;

        // Call the `Screenshot` method of `org.gnome.Shell.Screenshot`
        // args: (include_cursor=true, flash=false, filename="/tmp/glint-screenshot-<ts>.png")
        let tmp_path = format!(
            "/tmp/glint-screenshot-{}.png",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );
        let reply: zbus::Result<(bool, String)> = connection
            .call_method(
                Some("org.gnome.Shell.Screenshot"),
                "/org/gnome/Shell/Screenshot",
                Some("org.gnome.Shell.Screenshot"),
                "Screenshot",
                &(true, false, tmp_path.as_str()),
            )
            .await?
            .body()
            .deserialize();

        let reply = reply.context("Failed to deserialize GNOME Shell D-Bus reply")?;
        let (success, filename_used) = reply;
        if !success {
            return Err(anyhow!(
                "GNOME Shell D-Bus screenshot failed (success=false)"
            ));
        }

        log::debug!("GNOME Shell screenshot saved to: {filename_used}");

        let texture = gdk4::Texture::from_filename(&filename_used)
            .context("Failed to load the screenshot file returned by GNOME Shell")?;

        texture_to_image_surface(&texture)
    }
}

impl Default for ScreenshotService {
    fn default() -> Self {
        Self::new()
    }
}

/// This app's app id. Must match `Application::builder().application_id(...)`
/// in `main.rs` and the systemd scope unit name `app-<this value>-<pid>.scope`
/// created by the launch script. The Portal resolves the caller's app id from
/// the scope name and then looks up permissions in the PermissionStore.
const APP_ID: &str = "io.github.glint.Screenshot";

/// Ensure the PermissionStore authorizes this app with `screenshot=yes`.
///
/// GNOME 46's Portal `Screenshot(interactive=false)` does not show an
/// authorization dialog; it queries the PermissionStore directly and returns
/// NOT_FOUND (mapped by ashpd to "Other") when no "yes" record is found. We
/// therefore write the grant up front.
///
/// This method is idempotent: it tries to write on every launch, overwriting
/// any existing value with "yes".
async fn ensure_portal_permission() -> Result<()> {
    let connection = zbus::Connection::session()
        .await
        .context("Failed to connect to the session bus")?;

    // SetPermission(s table, b create, s id, s app, as permissions)
    // table="screenshot", id="screenshot", app=APP_ID, permissions=["yes"]
    // Note: zbus serializes `[T; N]` (fixed-size array) as a struct; a slice
    // `&[T]` is required to get `as`.
    let perms: [&str; 1] = ["yes"];
    let _: zbus::Result<()> = connection
        .call_method(
            Some("org.freedesktop.impl.portal.PermissionStore"),
            "/org/freedesktop/impl/portal/PermissionStore",
            Some("org.freedesktop.impl.portal.PermissionStore"),
            "SetPermission",
            &("screenshot", true, "screenshot", APP_ID, perms.as_slice()),
        )
        .await?
        .body()
        .deserialize();
    log::info!("Ensured Portal permission: {APP_ID} -> screenshot=yes");
    Ok(())
}

/// Decode PNG bytes with the fast `zune-png` decoder and build a Cairo
/// `ARGB32` `ImageSurface` directly from the raw RGBA pixels.
///
/// This replaces Cairo's built-in PNG decoder on the hot capture path:
/// `zune-png` is pure-Rust + SIMD and is several times faster, and we hand the
/// decoded RGBA bytes straight to Cairo's pixel buffer (with the required
/// RGBA→BGRA byte-swap and alpha premultiplication), avoiding any extra
/// copy/re-encode.
fn png_bytes_to_surface(bytes: &[u8]) -> Result<ImageSurface> {
    use zune_png::zune_core::colorspace::ColorSpace;
    use zune_png::zune_core::result::DecodingResult;

    let mut decoder = zune_png::PngDecoder::new(bytes);
    let decoded = decoder
        .decode()
        .map_err(|e| anyhow!("zune-png decode failed: {e:?}"))?;
    let raw = match decoded {
        DecodingResult::U8(v) => v,
        DecodingResult::U16(_) => return Err(anyhow!("16-bit PNG not supported")),
        _ => return Err(anyhow!("unsupported PNG decode result")),
    };
    let (w, h) = decoder
        .get_dimensions()
        .ok_or_else(|| anyhow!("PNG has no dimensions"))?;
    let cs = decoder.get_colorspace().unwrap_or(ColorSpace::RGBA);
    let (w, h) = (w as i32, h as i32);
    if w <= 0 || h <= 0 {
        return Err(anyhow!("Invalid PNG dimensions: {w}x{h}"));
    }

    // Normalize to RGBA8 (expand RGB -> RGBA with alpha=255).
    let tnorm = std::time::Instant::now();
    let rgba: Vec<u8> = match cs {
        ColorSpace::RGBA => raw,
        ColorSpace::RGB => {
            let n = (w as usize) * (h as usize);
            let mut out = Vec::with_capacity(n * 4);
            for i in 0..n {
                out.push(raw[i * 3]);
                out.push(raw[i * 3 + 1]);
                out.push(raw[i * 3 + 2]);
                out.push(255);
            }
            out
        }
        other => return Err(anyhow!("unsupported PNG colorspace: {other:?}")),
    };
    log::debug!("normalize colorspace: {:?}", tnorm.elapsed());

    let mut surface = ImageSurface::create(cairo::Format::ARgb32, w, h)
        .map_err(|e| anyhow!("Failed to create ImageSurface: {e}"))?;
    let stride = surface.stride() as usize;

    let tconv = std::time::Instant::now();
    // RGBA8 -> Cairo ARGB32. Cairo stores each pixel as a native-endian u32
    // 0xAARRGGBB (on little-endian the in-memory byte order is B,G,R,A). We
    // premultiply by alpha and swap R/B. Uses raw pointers to stay fast even in
    // unoptimized debug builds, and a fast path for the common opaque case.
    {
        let mut data = surface
            .data()
            .map_err(|e| anyhow!("Failed to borrow ImageSurface data: {e}"))?;
        let buf: &mut [u8] = &mut *data;
        let row_pixels = w as usize;
        let stride = stride;

        // Detect opaque (alpha == 255 everywhere) once; screenshots are opaque.
        let opaque = rgba.chunks_exact(4).all(|px| px[3] == 255);

        let src = rgba.as_ptr();
        let dst = buf.as_mut_ptr();
        if opaque {
            for y in 0..h as usize {
                let dst_row = y * stride;
                let src_row = y * row_pixels * 4;
                for x in 0..row_pixels {
                    let s = src_row + x * 4;
                    let d = dst_row + x * 4;
                    // pixel = 0xFFRRGGBB; LE bytes: B, G, R, FF
                    unsafe {
                        let r = *src.add(s);
                        let g = *src.add(s + 1);
                        let b = *src.add(s + 2);
                        *dst.add(d) = b;
                        *dst.add(d + 1) = g;
                        *dst.add(d + 2) = r;
                        *dst.add(d + 3) = 0xFF;
                    }
                }
            }
        } else {
            for y in 0..h as usize {
                let dst_row = y * stride;
                let src_row = y * row_pixels * 4;
                for x in 0..row_pixels {
                    let s = src_row + x * 4;
                    let d = dst_row + x * 4;
                    unsafe {
                        let r = *src.add(s) as u32;
                        let g = *src.add(s + 1) as u32;
                        let b = *src.add(s + 2) as u32;
                        let a = *src.add(s + 3) as u32;
                        let pr = r * a / 255;
                        let pg = g * a / 255;
                        let pb = b * a / 255;
                        let pixel: u32 = (a << 24) | (pr << 16) | (pg << 8) | pb;
                        let pb = pixel.to_ne_bytes();
                        *dst.add(d) = pb[0];
                        *dst.add(d + 1) = pb[1];
                        *dst.add(d + 2) = pb[2];
                        *dst.add(d + 3) = pb[3];
                    }
                }
            }
        }
    }
    surface.mark_dirty();
    log::debug!("RGBA->ARGB32 convert: {:?}", tconv.elapsed());
    Ok(surface)
}

/// Convert a `gdk4::Texture` to a Cairo `ImageSurface`.
///
/// This is the **key bridge** between GTK's image objects and the Cairo
/// drawing pipeline:
/// - `gdk4::Texture` is GTK4's immutable image abstraction (a GL texture
///   uploaded to the GPU);
/// - `cairo::ImageSurface` is a CPU-side pixel buffer readable/writable by
///   any Cairo drawing primitive.
///
/// Conversion path: `Texture` -> PNG bytes (`save_to_png_bytes`) ->
/// `ImageSurface` (Cairo's PNG decoder). We go through PNG rather than raw
/// RGBA because GDK4 does not expose the raw pixel layout, while PNG decoding
/// guarantees Cairo yields an `ARGB32` surface for uniform downstream handling.
fn texture_to_image_surface(texture: &gdk4::Texture) -> Result<ImageSurface> {
    // Get image dimensions (width/height come from the TextureExt trait)
    let width = texture.width();
    let height = texture.height();

    if width <= 0 || height <= 0 {
        return Err(anyhow!("Invalid screenshot size: {width}x{height}"));
    }

    // Download to a CPU-side byte stream (PNG-encoded). glib::Bytes implements AsRef<[u8]>.
    let bytes = texture.save_to_png_bytes();
    let png_data: Vec<u8> = <glib::Bytes as AsRef<[u8]>>::as_ref(&bytes).to_vec();

    // Decode with Cairo's PNG loader to get an ARGB32 ImageSurface
    let surface = ImageSurface::create_from_png(&mut std::io::Cursor::new(png_data))
        .map_err(|e| anyhow!("Failed to decode PNG byte stream into ImageSurface: {e}"))?;

    Ok(surface)
}

/// Wrap an `ImageSurface` in a thread-safe reference-counted pointer.
///
/// Cairo `Surface` itself is not `Send`/`Sync`, but under Wayland/GTK's
/// single-thread main-loop model we only pass references within the main
/// thread. `Arc` lets us share the same screenshot data across modules.
#[allow(dead_code)]
pub fn wrap_surface(surface: ImageSurface) -> Arc<ImageSurface> {
    Arc::new(surface)
}

/// Generate a colorful test image matching the primary monitor's size, used
/// for demo mode (`GLINT_DEMO=1`).
///
/// Contents: rainbow gradient background, grid lines, color swatches, and
/// coordinate text, giving the magnifier and color picker rich content to
/// verify against.
fn demo_surface() -> ImageSurface {
    // Get the primary monitor's geometry via gdk4 so the test image matches
    // the screen. Display::primary_monitor does not exist in GTK4; use
    // monitors() (a gio::ListModel) and take item 0.
    let (w, h) = gdk4::Display::default()
        .and_then(|d| {
            let monitors = d.monitors(); // gio::ListModel
            monitors
                .item(0)
                .and_then(|o| o.downcast::<gdk4::Monitor>().ok())
        })
        .map(|m| m.geometry())
        .map(|g| (g.width(), g.height()))
        .unwrap_or((1920, 1080));

    let surface = ImageSurface::create(cairo::Format::ARgb32, w, h)
        .expect("Failed to create demo ImageSurface");
    let cr = cairo::Context::new(&surface).expect("Failed to create Cairo Context");

    // 1. Rainbow horizontal gradient background
    let pat = cairo::LinearGradient::new(0.0, 0.0, w as f64, h as f64);
    pat.add_color_stop_rgb(0.0, 1.0, 0.2, 0.2);
    pat.add_color_stop_rgb(0.25, 0.9, 0.9, 0.2);
    pat.add_color_stop_rgb(0.5, 0.2, 0.9, 0.4);
    pat.add_color_stop_rgb(0.75, 0.2, 0.4, 0.9);
    pat.add_color_stop_rgb(1.0, 0.6, 0.2, 0.8);
    cr.set_source(pat).unwrap();
    cr.paint().unwrap();

    // 2. Grid lines (every 80px) so pixels are visible in the magnifier
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.25);
    cr.set_line_width(1.0);
    let step = 80;
    let mut x = 0;
    while x <= w {
        cr.move_to(x as f64, 0.0);
        cr.line_to(x as f64, h as f64);
        x += step;
    }
    let mut y = 0;
    while y <= h {
        cr.move_to(0.0, y as f64);
        cr.line_to(w as f64, y as f64);
        y += step;
    }
    cr.stroke().unwrap();

    // 3. Color swatch grid (for verifying RGB readouts in the color picker)
    let palette = [
        (1.0, 0.0, 0.0),
        (0.0, 1.0, 0.0),
        (0.0, 0.0, 1.0),
        (1.0, 1.0, 0.0),
        (0.0, 1.0, 1.0),
        (1.0, 0.0, 1.0),
        (0.0, 0.0, 0.0),
        (1.0, 1.0, 1.0),
    ];
    let bs = 120;
    for (i, (r, g, b)) in palette.iter().enumerate() {
        let bx = ((i % 4) * (bs + 20) + 40) as f64;
        let by = ((i / 4) * (bs + 20) + 40) as f64;
        cr.set_source_rgb(*r, *g, *b);
        cr.rectangle(bx, by, bs as f64, bs as f64);
        cr.fill().unwrap();
    }

    // 4. Title text
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.7);
    cr.set_font_size(48.0);
    cr.move_to(40.0, h as f64 - 60.0);
    cr.show_text(
        "glint-screenshot demo mode - move mouse for magnifier, drag to select, Esc to quit",
    )
    .unwrap();

    surface
}
