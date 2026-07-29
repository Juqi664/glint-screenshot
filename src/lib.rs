//! # glint-screenshot
//!
//! An open-source Ubuntu GNOME Wayland-native screenshot and pinning tool,
//! aiming to faithfully reproduce all core interactions and features of the
//! Windows WeChat screenshot tool.
//!
//! ## Module overview
//! - [`screenshot`]: screenshot service. Obtains full-screen RGBA data via the
//!   XDG Desktop Portal or the GNOME Shell D-Bus interface and wraps it as a
//!   Cairo [`ImageSurface`](cairo::ImageSurface).
//! - [`selector`]: selection and magnifier. Creates a fullscreen,
//!   input-transparent Wayland window, draws a semi-transparent mask, and
//!   implements the magnifier and color picker via Cairo pixel sampling.
//! - [`pin_window`]: pinning window. Uses gtk4-layer-shell to create a
//!   borderless, always-on-top window with scroll-wheel scaling and
//!   drag-to-move.
//!
//! ## Wayland notes
//! This project **never uses any X11 API**. All "always-on-top / input
//! passthrough / borderless" needs are achieved via:
//! 1. GTK4-native [`gtk4::Window`] `set_decorated(false)` /
//!    `set_keep_above(true)`;
//! 2. the Wayland layer-shell protocol (`gtk4-layer-shell` crate) for true
//!    "always-on-top + input passthrough".

// Clippy policy: we keep `cargo clippy -- -D warnings` green in CI, but allow a
// small set of stylistic lints crate-wide rather than churning every call site.
// Each allow is intentional and documented:
//
// - `let_unit_value`: the Cairo bindings return `()` for many drawing methods,
//   so the `let _ = cr.foo()` pattern is harmless and reads clearly.
// - `arc_with_non_send_sync`: GUI code legitimately shares `Rc`/`Arc` in a
//   single-threaded GTK main-loop context where `Send + Sync` is not required.
// - `explicit_auto_deref` / `needless_borrow`: a few explicit derefs are kept
//   for readability around FFI and trait objects.
// - `unnecessary_cast`: a handful of `as i32` / pointer casts are kept for
//   explicitness at FFI boundaries.
// - `too_many_arguments`: one legacy drawing helper has 8 args; refactoring it
//   is tracked separately and not worth the churn now.
// - `map_entry` / `redundant_locals`: minor stylistic items.
#![allow(clippy::let_unit_value)]
#![allow(clippy::arc_with_non_send_sync)]
#![allow(clippy::explicit_auto_deref)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::map_entry)]
#![allow(clippy::redundant_locals)]

pub mod pin_window;
pub mod screencast;
pub mod screenshot;
pub mod selector;
pub mod tools;
pub mod ui;

pub use pin_window::PinWindow;
pub use screenshot::ScreenshotService;
pub use selector::Selector;
pub use tools::{Color, DrawCommand, StrokeStyle, ToolKind};
