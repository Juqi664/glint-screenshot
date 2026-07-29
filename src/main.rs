//! Application entry point.
//!
//! Initializes GTK4, parses CLI arguments, and drives the main flow.
//!
//! Run modes (see `--help`):
//! * `glint-screenshot` — single-shot capture (default).
//! * `glint-screenshot --daemon` — background daemon: stays alive, listens on a
//!   Unix socket, and runs the capture flow on each `--trigger` from a client.
//! * `glint-screenshot --trigger` — signal a running daemon to capture; if no
//!   daemon is running, falls back to single-shot.
//!
//! Global keyboard shortcut (registered by the packaging setup script, not
//! here): `Ctrl+Alt+A` runs `glint-screenshot --trigger`.
//!
//! In-selector shortcuts: `Ctrl+C` copy, `Ctrl+S` save, `Ctrl+P` pin, `Esc`
//! exit.

use glint_screenshot::{screencast, PinWindow, ScreenshotService, Selector};
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Capture,
    Daemon,
    Trigger,
}

fn main() -> anyhow::Result<()> {
    // Initialize logging for debugging Wayland / Portal issues
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mode = parse_mode();

    // --trigger: try to wake the running daemon. If there is no daemon, fall
    // through to a single-shot capture so the shortcut still works.
    if mode == Mode::Trigger {
        match send_trigger() {
            Ok(()) => return Ok(()),
            Err(e) => log::info!(
                "No daemon reachable on the trigger socket ({e}); running a single-shot capture"
            ),
        }
    }

    // GTK4 application instance. The application_id must follow the reverse
    // DNS name convention; GNOME Shell uses it to identify the app and grant
    // the corresponding Portal permissions.
    let app = Application::builder()
        .application_id("io.github.glint.Screenshot")
        // ApplicationFlags comes from gio (re-exported by GTK4); FLAGS_NONE
        // means no special flags.
        .flags(gio::ApplicationFlags::FLAGS_NONE)
        .build();

    let mode_c = mode;
    app.connect_activate(move |app| {
        // Load custom CSS for a polished toolbar and selection visuals.
        // Must run after GTK init (which happens during app::run before
        // activate fires).
        load_css();

        match mode_c {
            Mode::Daemon => {
                // Stay alive without any window; the socket service drives
                // captures. The hold guard is intentionally forgotten so it is
                // never dropped — the daemon lives until the user logs out or
                // kills the process.
                let hold = app.hold();
                std::mem::forget(hold);
                if let Err(e) = setup_socket_service(app) {
                    log::error!("Failed to start daemon socket service: {e:?}");
                    app.quit();
                }
            }
            Mode::Capture | Mode::Trigger => {
                // Single-shot: capture the screen BEFORE showing any window.
                // This avoids a fullscreen overlay flashing on/off and keeps
                // the overlay out of the captured frame. `hold()` keeps the
                // app alive during the windowless ~600ms capture and is
                // released once capture_flow returns.
                let app_c = app.clone();
                let hold = app.hold();
                glib::spawn_future_local(async move {
                    let result = capture_flow(&app_c).await;
                    drop(hold);
                    // Single-shot: quit the app once the flow ends.
                    handle_capture_result(&app_c, result, true);
                });
            }
        }
    });

    // run takes over the main loop; the exit code follows POSIX.
    // Pass only argv[0] so GApplication's built-in option parser does not
    // reject our custom flags (`--daemon` / `--trigger`); we already parsed
    // them above in `parse_mode`.
    let argv0 = std::env::args()
        .next()
        .unwrap_or_else(|| "glint-screenshot".into());
    let exit_code = app.run_with_args(&[argv0]);
    std::process::exit(exit_code.value() as i32);
}

fn parse_mode() -> Mode {
    let mut mode = Mode::Capture;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--trigger" => mode = Mode::Trigger,
            "--daemon" => mode = Mode::Daemon,
            "--capture" => mode = Mode::Capture,
            "-h" | "--help" => {
                eprintln!(
                    "glint-screenshot — GNOME Wayland screenshot & pin tool\n\n\
                     Usage:\n  \
                     glint-screenshot              Single-shot capture (default)\n  \
                     glint-screenshot --daemon     Run as a background daemon\n  \
                     glint-screenshot --trigger     Signal the daemon to capture\n\n\
                     In-selector shortcuts: Ctrl+C copy, Ctrl+S save, Ctrl+P pin, Esc exit"
                );
                std::process::exit(0);
            }
            _ => { /* ignore unknown args (e.g. GNOME passes some) */ }
        }
    }
    mode
}

/// Path of the Unix socket used for daemon <-> trigger IPC. Lives in the user's
/// runtime directory (`$XDG_RUNTIME_DIR`, typically `/run/user/<uid>`).
fn socket_path() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return std::path::PathBuf::from(dir).join("glint-screenshot.sock");
    }
    // Fallback (rare on systemd sessions): a per-user file in /tmp.
    let user = std::env::var("USER").unwrap_or_else(|_| "anon".into());
    std::env::temp_dir().join(format!("glint-screenshot-{user}.sock"))
}

/// Client side: open the daemon socket and write a single trigger byte.
fn send_trigger() -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let path = socket_path();
    let mut s = UnixStream::connect(&path).map_err(|e| anyhow::anyhow!("connect {path:?}: {e}"))?;
    s.write_all(b"1")
        .map_err(|e| anyhow::anyhow!("write {path:?}: {e}"))?;
    Ok(())
}

/// Daemon side: bind a `gio::SocketService` on the IPC socket. Each incoming
/// connection spawns the capture flow on the GTK main loop.
fn setup_socket_service(app: &Application) -> anyhow::Result<()> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let service = gio::SocketService::new();
    let addr = gio::UnixSocketAddress::new(&path);
    service.add_address(
        &addr,
        gio::SocketType::Stream,
        gio::SocketProtocol::Default,
        None::<&glib::Object>,
    )?;
    let app_c = app.clone();
    service.connect_incoming(move |_svc, _conn, _src| {
        let app_c2 = app_c.clone();
        log::info!("Daemon triggered, starting capture...");
        glib::spawn_future_local(async move {
            let result = capture_flow(&app_c2).await;
            // Daemon must stay alive after each capture (do not quit on
            // cancel/copy); pass false so handle_capture_result never calls
            // app.quit() here.
            handle_capture_result(&app_c2, result, false);
        });
        // Keep the service running for the next trigger.
        false
    });
    service.start();
    log::info!("Daemon listening on {}", path.display());
    Ok(())
}

/// Handle the outcome of `capture_flow`: pin the surface, or quit on
/// cancel/error. `quit_after` is true for single-shot mode (quit when done)
/// and false for daemon mode (keep the daemon alive across captures).
fn handle_capture_result(
    app: &Application,
    result: anyhow::Result<Option<cairo::ImageSurface>>,
    quit_after: bool,
) {
    match result {
        Ok(Some(surface)) => {
            let pin = PinWindow::new(app, surface);
            pin.show();
        }
        Ok(None) => {
            log::info!("User cancelled selection");
            if quit_after {
                app.quit();
            }
        }
        Err(e) => {
            log::error!("Screenshot flow failed: {e:?}");
            show_error_dialog(app, &e.to_string());
            if quit_after {
                app.quit();
            }
        }
    }
}

/// Run the capture + selection flow. Returns the user's confirmed screenshot
/// surface (when pinning) or `None` (cancelled / already saved or copied).
async fn capture_flow(app: &Application) -> anyhow::Result<Option<ImageSurface>> {
    let t_start = std::time::Instant::now();
    let service = ScreenshotService::new();

    // Capture path selection.
    //
    // Default: XDG Desktop Portal `Screenshot` (reliable, full virtual desktop,
    // ~600ms in release). A fullscreen loading overlay is already shown so the
    // capture feels instant to the user.
    //
    // Optional fast path: ScreenCast + PipeWire (no PNG encode/disk). It is
    // flaky in the app context (works in the standalone probe but the stream
    // often goes `Connecting -> Unconnected` due to an unresolved mutter/portal
    // quirk), so it is disabled by default. Set `GLINT_USE_SCREENCAST=1` to
    // experiment with it; on failure it falls back to the Portal path.
    let use_screencast = std::env::var("GLINT_USE_SCREENCAST")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let surface = if use_screencast {
        log::info!("Attempting ScreenCast (PipeWire) fast path...");
        match run_screencast_on_async_std_thread().await {
            Ok(captured) => {
                log::info!(
                    "ScreenCast capture ready in {:?} (PipeWire fast path)",
                    t_start.elapsed()
                );
                match screencast::stitch(captured) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!(
                            "ScreenCast stitch failed: {e}; falling back to Screenshot portal"
                        );
                        capture_via_portal(app, &service, t_start).await?
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "ScreenCast fast path failed in {:?}: {e}; falling back to Screenshot portal",
                    t_start.elapsed()
                );
                capture_via_portal(app, &service, t_start).await?
            }
        }
    } else {
        capture_via_portal(app, &service, t_start).await?
    };
    log::info!(
        "Screenshot captured, size {}x{}",
        surface.width(),
        surface.height()
    );

    // Start the selector window (uses a MainLoop internally; Esc to quit).
    let selector = Selector::new(app, surface);
    selector.run().await
}

/// Run the ScreenCast capture on a dedicated thread driven by an `async-std`
/// runtime (via `block_on`), bridging the result back to the glib future via a
/// channel polled with `glib::timeout_future`. The captured raw frames are
/// `Send`, so they cross threads safely; stitching into a Cairo surface happens
/// later on the GUI thread.
async fn run_screencast_on_async_std_thread() -> anyhow::Result<screencast::CapturedRaw> {
    let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<screencast::CapturedRaw>>();
    std::thread::spawn(move || {
        let res = async_std::task::block_on(screencast::capture_desktop_raw());
        let _ = tx.send(res);
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match rx.try_recv() {
            Ok(res) => return res,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err(anyhow::anyhow!("screencast worker thread died"));
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow::anyhow!("screencast worker thread timed out"));
        }
        glib::timeout_future(std::time::Duration::from_millis(20)).await;
    }
}

/// Fallback: capture via the XDG Desktop Portal `Screenshot` interface (slower,
/// GNOME encodes a full-desktop PNG to disk). Tries without a parent window
/// first, then with a realized temp window if that fails.
async fn capture_via_portal(
    app: &Application,
    service: &ScreenshotService,
    t_start: std::time::Instant,
) -> anyhow::Result<ImageSurface> {
    match service.capture_full_screen(None).await {
        Ok(s) => {
            log::info!(
                "Portal capture ready in {:?} (no parent window)",
                t_start.elapsed()
            );
            Ok(s)
        }
        Err(e) => {
            log::warn!(
                "Portal capture without parent failed in {:?}: {e}; retrying with a parent window",
                t_start.elapsed()
            );
            let (tmp, identifier) = realize_identifier(app).await;
            let surface = service.capture_full_screen(identifier).await?;
            log::info!(
                "Portal capture ready in {:?} (with parent window)",
                t_start.elapsed()
            );
            tmp.close();
            Ok(surface)
        }
    }
}

/// Create and present a tiny temporary window, let it realize, and return it
/// together with a `WindowIdentifier` for it. Used only as a fallback when the
/// Portal capture without a parent window fails.
async fn realize_identifier(app: &Application) -> (gtk4::Window, Option<ashpd::WindowIdentifier>) {
    let tmp_window = gtk4::Window::new();
    tmp_window.set_application(Some(app));
    tmp_window.set_title(Some("glint-screenshot"));
    tmp_window.set_default_size(1, 1);
    tmp_window.present();

    // Let the window realize for a frame. Because we run on the main loop
    // (spawn_future_local), the main loop is iterating and the window
    // realizes naturally — no nested MainLoop needed.
    glib::timeout_future(std::time::Duration::from_millis(50)).await;
    let mut identifier = None;
    if let Some(n) = tmp_window.native() {
        identifier = ashpd::WindowIdentifier::from_native(&n).await;
    }
    log::info!("Obtained window identifier: {}", identifier.is_some());
    (tmp_window, identifier)
}

/// Load application-wide CSS for the floating toolbar and selection visuals.
fn load_css() {
    let css = include_str!("style.css");
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(css);
    gtk4::style_context_add_provider_for_display(
        &gdk4::Display::default().expect("default display"),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn show_error_dialog(app: &Application, msg: &str) {
    let window: gtk4::Window = app.active_window().unwrap_or_else(|| {
        let w = ApplicationWindow::new(app);
        w.present();
        w.upcast()
    });
    // AlertDialog buttons take a &str slice (StrV implements
    // From<&[&str]> but not From<&[&str; N]>).
    let buttons: [&str; 1] = ["OK"];
    let dialog = gtk4::AlertDialog::builder()
        .message("Screenshot failed")
        .detail(msg)
        .buttons(buttons.as_slice())
        .build();
    dialog.show(Some(&window));
}

// Re-export the cairo type used in this module's signatures.
use cairo::ImageSurface;
