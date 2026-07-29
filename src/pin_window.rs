//! Pin window module.
//!
//! Implements the "pin to screen" feature: the image inside the selection
//! is fixed on the desktop as an independent **borderless** window.
//! Supports:
//! - mouse drag to move;
//! - mouse wheel to scale;
//! - a close button at the top-right to exit;
//! - Esc to close.
//!
//! ## Wayland implementation notes
//! There are two code paths, selected at runtime:
//!
//! 1. **layer-shell path** (Sway/Hyprland/KDE and any compositor advertising
//!    `zwlr_layer_shell_v1`): the window is put on the `OVERLAY` layer, which
//!    makes it always-on-top. Position is controlled via the four edge
//!    margins, so dragging adjusts the margins.
//!
//! 2. **normal-window path** (GNOME/mutter, which does NOT support
//!    `wlr-layer-shell`): the window is a plain borderless `GtkWindow`.
//!    Always-on-top is not available on GNOME Wayland, but the window is
//!    brought to the front on present. Dragging uses the Wayland
//!    `xdg_toplevel.move` request via `gdk_toplevel_begin_move`, which lets
//!    the compositor take over an interactive move of a borderless window.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use cairo::ImageSurface;
use gtk4::prelude::*;
use gtk4::{
    Application, Button, DrawingArea, EventControllerKey, EventControllerScroll, GestureDrag,
    Overlay, Window,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

/// The pin window.
pub struct PinWindow {
    app: Application,
    surface: Arc<ImageSurface>,
    /// Current zoom factor (1.0 = original size)
    scale: f64,
}

impl PinWindow {
    pub fn new(app: &Application, surface: ImageSurface) -> Self {
        Self {
            app: app.clone(),
            surface: Arc::new(surface),
            scale: 1.0,
        }
    }

    /// Show the pin window.
    pub fn show(self) {
        let window = Window::new();

        // Decide which path to take. `is_supported()` checks whether the
        // compositor advertises `zwlr_layer_shell_v1` in the global registry.
        // GNOME/mutter does not, so we fall back to a normal borderless window.
        let layer_supported = if gtk4_layer_shell::is_supported() {
            // init_layer_shell MUST be the very first call on the window,
            // before set_application / set_decorated / any property: it hooks
            // the window's "realize" to create the zwlr_layer_surface, and can
            // only do so if the window is not realized yet.
            window.init_layer_shell();
            let ok = window.is_layer_window();
            log::info!(
                "PinWindow layer-shell supported, init -> is_layer_window={}",
                ok
            );
            ok
        } else {
            log::warn!(
                "gtk4-layer-shell NOT supported on this compositor; pin window \
                 will be a normal borderless window (always-on-top unavailable \
                 on GNOME Wayland)"
            );
            false
        };

        window.set_application(Some(&self.app));
        window.set_decorated(false); // borderless

        if layer_supported {
            window.set_layer(Layer::Overlay);
            window.set_keyboard_mode(KeyboardMode::OnDemand);
            // exclusive_zone = -1 means it does not reserve workspace and may
            // cover other windows.
            window.set_exclusive_zone(-1);
        }

        let (iw, ih) = (self.surface.width(), self.surface.height());
        let surface = self.surface.clone();
        let scale = Rc::new(RefCell::new(self.scale));

        // ===== Drawing area: draw the pinned image at the current scale =====
        let area = DrawingArea::new();
        area.set_content_width(iw);
        area.set_content_height(ih);
        {
            let scale_c = scale.clone();
            area.set_draw_func(move |_, cr, _w, _h| {
                let s = *scale_c.borrow();
                cr.scale(s, s);
                cr.set_source_surface(&*surface, 0.0, 0.0).unwrap();
                cr.paint().unwrap();
            });
        }

        // ===== Drag to move =====
        let drag = GestureDrag::new();
        if layer_supported {
            // Wayland has no "global coordinates"; a layer-shell window's
            // position is determined by the four edge margins. On drag, record
            // the starting margins and the starting pointer; on update, add the
            // pointer delta to the margins.
            let start_pt = Rc::new(RefCell::new((0.0_f64, 0.0_f64)));
            let start_m = Rc::new(RefCell::new((0_i32, 0_i32)));
            {
                let start_pt_c = start_pt.clone();
                let start_m_c = start_m.clone();
                let win_inner = window.clone();
                drag.connect_drag_begin(move |_, x, y| {
                    *start_pt_c.borrow_mut() = (x, y);
                    *start_m_c.borrow_mut() =
                        (win_inner.margin(Edge::Left), win_inner.margin(Edge::Top));
                });
            }
            {
                let start_pt_c = start_pt.clone();
                let start_m_c = start_m.clone();
                let win_inner = window.clone();
                drag.connect_drag_update(move |_, ox, oy| {
                    let s = *start_pt_c.borrow();
                    let sm = *start_m_c.borrow();
                    let dx = (ox - s.0) as i32;
                    let dy = (oy - s.1) as i32;
                    win_inner.set_margin(Edge::Left, sm.0 + dx);
                    win_inner.set_margin(Edge::Top, sm.1 + dy);
                });
            }
        } else {
            // Normal borderless window on GNOME: ask the compositor to start an
            // interactive move (Wayland `xdg_toplevel.move`) using the triggering
            // button/device/timestamp. The compositor then moves the window
            // following the pointer until the button is released.
            let win_inner = window.clone();
            drag.connect_drag_begin(move |gesture, _x, _y| {
                let Some(event) = gesture.current_event() else {
                    return;
                };
                let Some(device) = event.device() else {
                    return;
                };
                let time = event.time();
                let (x, y) = event.position().unwrap_or((0.0, 0.0));
                let button = event
                    .downcast_ref::<gdk4::ButtonEvent>()
                    .map(|b| b.button() as i32)
                    .unwrap_or(1);
                let Some(surface) = win_inner.surface() else {
                    return;
                };
                // gdk4 0.9.6 does not expose `Surface: IsA<Toplevel>` in the
                // bindings, so call the C symbol directly. At the C level a
                // toplevel `GdkSurface` implements `GdkToplevel`, so the
                // instance pointer is valid as `GdkToplevel*`.
                let toplevel_ptr = surface.as_ptr() as *mut gdk4::ffi::GdkToplevel;
                let device_ptr = device.as_ptr() as *mut gdk4::ffi::GdkDevice;
                unsafe {
                    gdk4::ffi::gdk_toplevel_begin_move(
                        toplevel_ptr,
                        device_ptr,
                        button,
                        x,
                        y,
                        time,
                    );
                }
            });
        }
        area.add_controller(drag);

        // ===== Scroll-wheel scaling =====
        let scroll = EventControllerScroll::new(
            gtk4::EventControllerScrollFlags::VERTICAL
                | gtk4::EventControllerScrollFlags::HORIZONTAL,
        );
        {
            let scale_c = scale.clone();
            let area_c = area.clone();
            scroll.connect_scroll(move |_, _dx, dy| {
                let mut s = scale_c.borrow_mut();
                // ±10% per scroll, clamped to [0.2, 5.0]
                *s = (*s * (1.0 - dy * 0.1)).clamp(0.2, 5.0);
                area_c.queue_draw();
                glib::Propagation::Proceed
            });
        }
        area.add_controller(scroll);

        // ===== Esc to close =====
        {
            let win_c = window.clone();
            let key = EventControllerKey::new();
            key.connect_key_pressed(move |_, _key, _code, mods| {
                if mods == gtk4::gdk::ModifierType::empty()
                    && _key.name().as_deref() == Some("Escape")
                {
                    win_c.close();
                }
                glib::Propagation::Proceed
            });
            window.add_controller(key);
        }

        // ===== Close button (top-right overlay) =====
        // Each pin window carries its own close button so any single pinned
        // image can be removed individually without affecting the others.
        let close_btn = Button::from_icon_name("window-close-symbolic");
        close_btn.add_css_class("pin-close");
        close_btn.set_halign(gtk4::Align::End);
        close_btn.set_valign(gtk4::Align::Start);
        close_btn.set_tooltip_text(Some("Close this pin"));
        {
            let win_c = window.clone();
            close_btn.connect_clicked(move |_| {
                win_c.close();
            });
        }

        let overlay = Overlay::new();
        overlay.set_child(Some(&area));
        overlay.add_overlay(&close_btn);

        window.set_child(Some(&overlay));
        window.present();
    }
}
