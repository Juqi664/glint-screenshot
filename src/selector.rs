//! Selector, magnifier, drawing toolbar, and confirmation logic module.
//!
//! Responsibilities:
//! 1. Create one fullscreen window **per monitor** covering the full virtual
//!    desktop, drawing the corresponding crop of the screenshot + mask +
//!    selection highlight;
//! 2. Mouse-drag to select a region (coordinates are global / virtual-desktop);
//!    show the magnifier and color picker while moving;
//! 3. Once the selection is confirmed, float a toolbar supporting
//!    rectangle/ellipse/line/arrow/brush/mosaic/text annotations;
//! 4. Maintain a drawing command stack for undo/redo;
//! 5. Confirmation actions (save / copy / pin / exit): crop the selection,
//!    replay commands, and produce the final `ImageSurface`.
//!
//! ## Multi-monitor
//! On Wayland/GNOME, `Window::fullscreen()` only covers the monitor the window
//! is on. The Portal screenshot spans the whole virtual desktop (e.g. 5760x1200
//! for three side-by-side displays). We therefore open one
//! `fullscreen_on_monitor` window per `gdk::Monitor`, share global selection
//! state, and convert local pointer coords ↔ global desktop coords.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::{Context as _, Result};
use cairo::{Context, Format, ImageSurface};
use gdk4::prelude::{DisplayExt, MonitorExt};
use gio::prelude::ListModelExt;
use glib::prelude::Cast;
use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Application, DrawingArea, EventControllerKey, EventControllerMotion, GestureDrag, Overlay,
    Window,
};
use std::sync::Arc;

use crate::tools::{render_stack, DrawCommand, ToolKind};
use crate::ui::toolbar::{build_toolbar, ToolbarCallbacks, ToolbarState};

/// Selection state machine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SelectorState {
    Idle,
    Selecting,
    Selected,
}

/// Selection geometry in **global / virtual-desktop** coordinates.
#[derive(Debug, Clone, Copy, Default)]
pub struct SelectionRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl SelectionRect {
    pub fn is_trivial(&self) -> bool {
        self.w < 3.0 || self.h < 3.0
    }
    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
    /// Translate into a monitor-local coordinate system.
    fn to_local(&self, ox: f64, oy: f64) -> SelectionRect {
        SelectionRect {
            x: self.x - ox,
            y: self.y - oy,
            w: self.w,
            h: self.h,
        }
    }
}

/// Magnifier parameters.
#[derive(Debug, Clone, Copy)]
pub struct MagnifierConfig {
    pub radius: f64,
    pub zoom: f64,
    pub offset_x: f64,
    pub offset_y: f64,
}

impl Default for MagnifierConfig {
    fn default() -> Self {
        Self {
            radius: 60.0,
            zoom: 8.0,
            offset_x: 80.0,
            offset_y: 80.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DragMode {
    Selection,
    Drawing,
    Moving,
    Resizing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Handle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

#[derive(Debug, Clone, Copy)]
enum Hit {
    Edge(Handle),
    Inside,
    Outside,
}

const HANDLE_HIT: f64 = 8.0;

fn hit_test(sel: &SelectionRect, x: f64, y: f64) -> Hit {
    let left = sel.x;
    let right = sel.x + sel.w;
    let top = sel.y;
    let bottom = sel.y + sel.h;
    let hs = HANDLE_HIT;

    let near_l = (x - left).abs() < hs;
    let near_r = (x - right).abs() < hs;
    let near_t = (y - top).abs() < hs;
    let near_b = (y - bottom).abs() < hs;
    if near_l && near_t {
        return Hit::Edge(Handle::TopLeft);
    }
    if near_r && near_t {
        return Hit::Edge(Handle::TopRight);
    }
    if near_r && near_b {
        return Hit::Edge(Handle::BottomRight);
    }
    if near_l && near_b {
        return Hit::Edge(Handle::BottomLeft);
    }
    if x >= left - hs && x <= right + hs && near_t {
        return Hit::Edge(Handle::Top);
    }
    if x >= left - hs && x <= right + hs && near_b {
        return Hit::Edge(Handle::Bottom);
    }
    if y >= top - hs && y <= bottom + hs && near_l {
        return Hit::Edge(Handle::Left);
    }
    if y >= top - hs && y <= bottom + hs && near_r {
        return Hit::Edge(Handle::Right);
    }
    if x > left && x < right && y > top && y < bottom {
        return Hit::Inside;
    }
    Hit::Outside
}

fn resize_rect(orig: &SelectionRect, handle: Handle, px: f64, py: f64) -> SelectionRect {
    let (mut x0, mut y0, mut x1, mut y1) = (orig.x, orig.y, orig.x + orig.w, orig.y + orig.h);
    match handle {
        Handle::TopLeft => {
            x0 = px;
            y0 = py;
        }
        Handle::Top => {
            y0 = py;
        }
        Handle::TopRight => {
            x1 = px;
            y0 = py;
        }
        Handle::Right => {
            x1 = px;
        }
        Handle::BottomRight => {
            x1 = px;
            y1 = py;
        }
        Handle::Bottom => {
            y1 = py;
        }
        Handle::BottomLeft => {
            x0 = px;
            y1 = py;
        }
        Handle::Left => {
            x0 = px;
        }
    }
    SelectionRect {
        x: x0.min(x1),
        y: y0.min(y1),
        w: (x1 - x0).abs(),
        h: (y1 - y0).abs(),
    }
}

enum Outcome {
    Cancel,
    Pin(ImageSurface),
    Done,
}

/// One physical / logical monitor's place in the virtual desktop.
#[derive(Clone)]
struct MonitorGeom {
    monitor: gdk4::Monitor,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// Widgets belonging to a single per-monitor overlay window.
struct MonitorSurface {
    window: Window,
    area: DrawingArea,
    overlay: Overlay,
    geom: MonitorGeom,
    /// Pre-cropped background for this monitor (ARGB32), avoids sampling the
    /// full multi-monitor stitch on every redraw.
    bg: ImageSurface,
}

pub struct Selector {
    app: Application,
    surface: Arc<ImageSurface>,
    magnifier: MagnifierConfig,
}

impl Selector {
    pub fn new(app: &Application, surface: ImageSurface) -> Self {
        Self {
            app: app.clone(),
            surface: Arc::new(surface),
            magnifier: MagnifierConfig::default(),
        }
    }

    pub async fn run(self) -> Result<Option<ImageSurface>> {
        let monitors = list_monitors();
        if monitors.is_empty() {
            anyhow::bail!("No monitors found on the default GdkDisplay");
        }

        let (sw, sh) = (self.surface.width(), self.surface.height());
        let (desk_x, desk_y, desk_w, desk_h) = desktop_bounds(&monitors);
        log::info!(
            "Selector: capture {}x{}, desktop {}x{} at ({},{}), {} monitor(s)",
            sw,
            sh,
            desk_w,
            desk_h,
            desk_x,
            desk_y,
            monitors.len()
        );
        for (i, m) in monitors.iter().enumerate() {
            log::info!(
                "  monitor[{}]: {}x{} at ({},{}) connector={:?}",
                i,
                m.w,
                m.h,
                m.x,
                m.y,
                m.monitor.connector()
            );
        }

        // Scale factor when Portal image size ≠ virtual desktop (HiDPI edge case).
        let scale_x = if desk_w > 0 {
            sw as f64 / desk_w as f64
        } else {
            1.0
        };
        let scale_y = if desk_h > 0 {
            sh as f64 / desk_h as f64
        } else {
            1.0
        };
        if (scale_x - 1.0).abs() > 0.01 || (scale_y - 1.0).abs() > 0.01 {
            log::warn!(
                "Capture/desktop size mismatch — using scale ({scale_x:.3}, {scale_y:.3})"
            );
        }

        // Shared state (global / virtual-desktop coordinates, in capture-pixel space).
        let surface = self.surface.clone();
        let state = Rc::new(RefCell::new(SelectorState::Idle));
        let selection = Rc::new(RefCell::new(SelectionRect::default()));
        let cursor = Rc::new(RefCell::new((0.0_f64, 0.0_f64)));
        let commands: Rc<RefCell<Vec<DrawCommand>>> = Rc::new(RefCell::new(Vec::new()));
        let redo: Rc<RefCell<Vec<DrawCommand>>> = Rc::new(RefCell::new(Vec::new()));
        let current: Rc<RefCell<Option<DrawCommand>>> = Rc::new(RefCell::new(None));
        let toolbar_state = ToolbarState::default();
        let magnifier = self.magnifier;
        let outcome: Rc<RefCell<Option<Outcome>>> = Rc::new(RefCell::new(None));
        let move_orig: Rc<RefCell<SelectionRect>> = Rc::new(RefCell::new(SelectionRect::default()));
        let resize_orig: Rc<RefCell<SelectionRect>> =
            Rc::new(RefCell::new(SelectionRect::default()));
        let active_handle: Rc<RefCell<Option<Handle>>> = Rc::new(RefCell::new(None));
        let start = Rc::new(RefCell::new((0.0_f64, 0.0_f64)));
        let mode = Rc::new(RefCell::new(None::<DragMode>));
        let last_cursor_name: Rc<Cell<&'static str>> = Rc::new(Cell::new(""));

        // Capture bounds used to clamp selection (in capture-pixel space).
        let screen_w = sw as f64;
        let screen_h = sh as f64;

        // Build per-monitor surfaces (pre-crop backgrounds).
        let mut surfaces: Vec<MonitorSurface> = Vec::with_capacity(monitors.len());
        for m in &monitors {
            let bg = crop_monitor_bg(
                &surface,
                m,
                desk_x,
                desk_y,
                scale_x,
                scale_y,
            )
            .with_context(|| format!("Failed to crop background for monitor at ({},{})", m.x, m.y))?;

            let window = Window::new();
            window.set_decorated(false);
            window.set_application(Some(&self.app));
            // Must be called before present(); targets this specific monitor.
            window.fullscreen_on_monitor(&m.monitor);

            let area = DrawingArea::new();
            area.set_content_width(m.w);
            area.set_content_height(m.h);
            area.set_hexpand(true);
            area.set_vexpand(true);

            let overlay = Overlay::new();
            overlay.set_child(Some(&area));

            window.set_child(Some(&overlay));

            surfaces.push(MonitorSurface {
                window,
                area,
                overlay,
                geom: m.clone(),
                bg,
            });
        }

        // Collect DrawingAreas for coalesced redraw.
        let areas: Rc<Vec<DrawingArea>> = Rc::new(surfaces.iter().map(|s| s.area.clone()).collect());
        let windows: Rc<Vec<Window>> = Rc::new(surfaces.iter().map(|s| s.window.clone()).collect());
        let overlays: Rc<Vec<Overlay>> =
            Rc::new(surfaces.iter().map(|s| s.overlay.clone()).collect());
        let geoms: Rc<Vec<MonitorGeom>> =
            Rc::new(surfaces.iter().map(|s| s.geom.clone()).collect());

        // Coalesced redraw: at most one queue_draw pass per main-context iteration.
        let redraw_pending = Rc::new(Cell::new(false));
        let schedule_redraw = {
            let areas = areas.clone();
            let pending = redraw_pending.clone();
            Rc::new(move || {
                if pending.get() {
                    return;
                }
                pending.set(true);
                let areas = areas.clone();
                let pending = pending.clone();
                glib::idle_add_local_once(move || {
                    pending.set(false);
                    for a in areas.iter() {
                        a.queue_draw();
                    }
                });
            }) as Rc<dyn Fn()>
        };

        // Wire draw funcs per monitor.
        for surf in &surfaces {
            let bg = surf.bg.clone();
            // ImageSurface isn't Clone in older cairo; use a fresh reference via
            // unsafe pointer trick — actually cairo::ImageSurface is clone via
            // glib? In gtk-rs cairo, ImageSurface implements Clone (refcount).
            let d_state = state.clone();
            let d_sel = selection.clone();
            let d_cur = cursor.clone();
            let d_cmds = commands.clone();
            let d_cur_cmd = current.clone();
            let d_surface = surface.clone();
            let ox = (surf.geom.x - desk_x) as f64 * scale_x;
            let oy = (surf.geom.y - desk_y) as f64 * scale_y;
            let mw = surf.geom.w as f64;
            let mh = surf.geom.h as f64;
            let mag = magnifier;

            // Keep a clone of the pre-cropped bg. ImageSurface is refcounted.
            let bg_for_draw = clone_surface(&bg)?;

            surf.area.set_draw_func(move |_, cr, _w, _h| {
                // 1. Pre-cropped monitor background
                let _ = cr.set_source_surface(&bg_for_draw, 0.0, 0.0);
                let _ = cr.paint();

                let st = *d_state.borrow();
                let sel_global = *d_sel.borrow();
                let sel = sel_global.to_local(ox, oy);

                // 2. Mask + selection chrome (local coords)
                draw_mask(cr, mw, mh, &sel, st);
                if st == SelectorState::Selecting && !sel_global.is_trivial() {
                    draw_size_readout(cr, &sel);
                    draw_selection_border(cr, &sel);
                } else if st == SelectorState::Selected && !sel_global.is_trivial() {
                    // Annotations: clip to selection, translate to selection origin
                    let _ = cr.save();
                    cr.rectangle(sel.x, sel.y, sel.w, sel.h);
                    let _ = cr.clip();
                    cr.translate(sel.x, sel.y);
                    render_stack(
                        cr,
                        &d_cmds.borrow(),
                        Some(&*d_surface),
                        (sel_global.x, sel_global.y),
                    );
                    if let Some(cmd) = d_cur_cmd.borrow().as_ref() {
                        crate::tools::render_command(
                            cr,
                            cmd,
                            Some(&*d_surface),
                            (sel_global.x, sel_global.y),
                        );
                    }
                    let _ = cr.restore();
                    draw_selection_border(cr, &sel);
                }

                // 3. Magnifier only on the monitor that currently holds the cursor
                if st != SelectorState::Selected {
                    let (cx, cy) = *d_cur.borrow();
                    let local_x = cx - ox;
                    let local_y = cy - oy;
                    if local_x >= -mag.radius
                        && local_y >= -mag.radius
                        && local_x <= mw + mag.radius
                        && local_y <= mh + mag.radius
                    {
                        // Sample from the full capture (global coords)
                        draw_magnifier(cr, &*d_surface, cx, cy, local_x, local_y, &mag);
                    }
                }
            });
        }

        // Motion + drag controllers per monitor. Keep the GestureDrag handles so
        // we can attach drag_end after the toolbar helpers exist.
        let mut drags: Vec<GestureDrag> = Vec::with_capacity(surfaces.len());
        for surf in &surfaces {
            let ox = (surf.geom.x - desk_x) as f64 * scale_x;
            let oy = (surf.geom.y - desk_y) as f64 * scale_y;

            let motion = EventControllerMotion::new();
            {
                let cursor_c = cursor.clone();
                let sel_m = selection.clone();
                let state_m = state.clone();
                let tb_m = toolbar_state.clone();
                let area_c = surf.area.clone();
                let last_cur = last_cursor_name.clone();
                let redraw = schedule_redraw.clone();
                motion.connect_motion(move |_, x, y| {
                    let gx = x + ox;
                    let gy = y + oy;
                    *cursor_c.borrow_mut() = (gx, gy);

                    let st = *state_m.borrow();
                    let tool = *tb_m.active_tool.borrow();
                    let desired: &'static str =
                        if st == SelectorState::Selected && tool == ToolKind::Select {
                            let sel = *sel_m.borrow();
                            match hit_test(&sel, gx, gy) {
                                Hit::Edge(Handle::TopLeft) | Hit::Edge(Handle::BottomRight) => {
                                    "nwse-resize"
                                }
                                Hit::Edge(Handle::TopRight) | Hit::Edge(Handle::BottomLeft) => {
                                    "nesw-resize"
                                }
                                Hit::Edge(Handle::Top) | Hit::Edge(Handle::Bottom) => "ns-resize",
                                Hit::Edge(Handle::Left) | Hit::Edge(Handle::Right) => "ew-resize",
                                Hit::Inside => "move",
                                Hit::Outside => "crosshair",
                            }
                        } else if st == SelectorState::Idle || st == SelectorState::Selecting {
                            "crosshair"
                        } else {
                            "default"
                        };
                    if last_cur.get() != desired {
                        last_cur.set(desired);
                        if let Some(cur) = gdk::Cursor::from_name(desired, None) {
                            area_c.set_cursor(Some(&cur));
                        }
                    }
                    // Magnifier follows the cursor only before the selection is
                    // confirmed. Afterwards motion only updates the cursor shape.
                    if st != SelectorState::Selected {
                        redraw();
                    }
                });
            }
            surf.area.add_controller(motion);

            let drag = GestureDrag::new();
            {
                let state_c = state.clone();
                let sel_c = selection.clone();
                let start_c = start.clone();
                let mode_c = mode.clone();
                let cur_cmd_c = current.clone();
                let redo_c = redo.clone();
                let tb_state = toolbar_state.clone();
                let move_orig_c = move_orig.clone();
                let resize_orig_c = resize_orig.clone();
                let handle_c = active_handle.clone();
                let commands_begin = commands.clone();
                let redraw = schedule_redraw.clone();
                drag.connect_drag_begin(move |_, x, y| {
                    let gx = x + ox;
                    let gy = y + oy;
                    let tool = *tb_state.active_tool.borrow();
                    let st = *state_c.borrow();
                    let sel = *sel_c.borrow();

                    if st == SelectorState::Selected && tool == ToolKind::Select && !sel.is_trivial()
                    {
                        match hit_test(&sel, gx, gy) {
                            Hit::Inside => {
                                *mode_c.borrow_mut() = Some(DragMode::Moving);
                                *move_orig_c.borrow_mut() = sel;
                                *start_c.borrow_mut() = (gx, gy);
                            }
                            Hit::Edge(h) => {
                                *mode_c.borrow_mut() = Some(DragMode::Resizing);
                                *resize_orig_c.borrow_mut() = sel;
                                *handle_c.borrow_mut() = Some(h);
                                *start_c.borrow_mut() = (gx, gy);
                            }
                            Hit::Outside => {
                                *mode_c.borrow_mut() = Some(DragMode::Selection);
                                *start_c.borrow_mut() = (gx, gy);
                                *state_c.borrow_mut() = SelectorState::Selecting;
                                *sel_c.borrow_mut() = SelectionRect::default();
                                cur_cmd_c.borrow_mut().take();
                                commands_begin.borrow_mut().clear();
                                redo_c.borrow_mut().clear();
                            }
                        }
                        redraw();
                        return;
                    }

                    if tool == ToolKind::Select || st != SelectorState::Selected {
                        *mode_c.borrow_mut() = Some(DragMode::Selection);
                        *start_c.borrow_mut() = (gx, gy);
                        *state_c.borrow_mut() = SelectorState::Selecting;
                        *sel_c.borrow_mut() = SelectionRect::default();
                        cur_cmd_c.borrow_mut().take();
                        commands_begin.borrow_mut().clear();
                        redo_c.borrow_mut().clear();
                    } else {
                        let rx = gx - sel.x;
                        let ry = gy - sel.y;
                        *mode_c.borrow_mut() = Some(DragMode::Drawing);
                        *start_c.borrow_mut() = (rx, ry);
                        let style = *tb_state.style.borrow();
                        *cur_cmd_c.borrow_mut() = match tool {
                            ToolKind::Rect => Some(DrawCommand::Rect {
                                x: rx,
                                y: ry,
                                w: 0.0,
                                h: 0.0,
                                style,
                            }),
                            ToolKind::Ellipse => Some(DrawCommand::Ellipse {
                                x: rx,
                                y: ry,
                                w: 0.0,
                                h: 0.0,
                                style,
                            }),
                            ToolKind::Line => Some(DrawCommand::Line {
                                x1: rx,
                                y1: ry,
                                x2: rx,
                                y2: ry,
                                style,
                            }),
                            ToolKind::Arrow => Some(DrawCommand::Arrow {
                                x1: rx,
                                y1: ry,
                                x2: rx,
                                y2: ry,
                                style,
                            }),
                            ToolKind::Brush => Some(DrawCommand::Brush {
                                points: vec![(rx, ry)],
                                style,
                            }),
                            ToolKind::Mosaic => Some(DrawCommand::Mosaic {
                                x: rx,
                                y: ry,
                                w: 0.0,
                                h: 0.0,
                                block: 12,
                            }),
                            _ => None,
                        };
                    }
                    redraw();
                });
            }
            {
                let mode_c = mode.clone();
                let start_c = start.clone();
                let sel_c = selection.clone();
                let cur_cmd_c = current.clone();
                let move_orig_c = move_orig.clone();
                let resize_orig_c = resize_orig.clone();
                let handle_c = active_handle.clone();
                let redraw = schedule_redraw.clone();
                drag.connect_drag_update(move |_, ox_d, oy_d| {
                    match *mode_c.borrow() {
                        Some(DragMode::Selection) => {
                            let s = *start_c.borrow();
                            *sel_c.borrow_mut() = SelectionRect {
                                x: s.0.min(s.0 + ox_d),
                                y: s.1.min(s.1 + oy_d),
                                w: ox_d.abs(),
                                h: oy_d.abs(),
                            };
                        }
                        Some(DragMode::Moving) => {
                            let orig = *move_orig_c.borrow();
                            let mut nx = orig.x + ox_d;
                            let mut ny = orig.y + oy_d;
                            nx = nx.clamp(0.0, (screen_w - orig.w).max(0.0));
                            ny = ny.clamp(0.0, (screen_h - orig.h).max(0.0));
                            *sel_c.borrow_mut() = SelectionRect {
                                x: nx,
                                y: ny,
                                w: orig.w,
                                h: orig.h,
                            };
                        }
                        Some(DragMode::Resizing) => {
                            let s = *start_c.borrow();
                            let orig = *resize_orig_c.borrow();
                            let px = s.0 + ox_d;
                            let py = s.1 + oy_d;
                            let mut new = resize_rect(&orig, handle_c.borrow().unwrap(), px, py);
                            new.x = new.x.max(0.0);
                            new.y = new.y.max(0.0);
                            if new.x + new.w > screen_w {
                                new.w = (screen_w - new.x).max(0.0);
                            }
                            if new.y + new.h > screen_h {
                                new.h = (screen_h - new.y).max(0.0);
                            }
                            *sel_c.borrow_mut() = new;
                        }
                        Some(DragMode::Drawing) => {
                            let s = *start_c.borrow();
                            let cx = s.0 + ox_d;
                            let cy = s.1 + oy_d;
                            let mut cc = cur_cmd_c.borrow_mut();
                            if let Some(cmd) = cc.as_mut() {
                                match cmd {
                                    DrawCommand::Rect { x, y, w, h, .. }
                                    | DrawCommand::Ellipse { x, y, w, h, .. }
                                    | DrawCommand::Mosaic { x, y, w, h, .. } => {
                                        *x = s.0.min(cx);
                                        *y = s.1.min(cy);
                                        *w = (cx - s.0).abs();
                                        *h = (cy - s.1).abs();
                                    }
                                    DrawCommand::Line { x2, y2, .. }
                                    | DrawCommand::Arrow { x2, y2, .. } => {
                                        *x2 = cx;
                                        *y2 = cy;
                                    }
                                    DrawCommand::Brush { points, .. } => {
                                        points.push((cx, cy));
                                    }
                                    DrawCommand::Text { .. } => {}
                                }
                            }
                        }
                        None => {}
                    }
                    redraw();
                });
            }

            surf.area.add_controller(drag.clone());
            drags.push(drag);
        }

        let main_loop = glib::MainLoop::new(None, false);

        // Toolbar lives on one overlay at a time (reparented to the monitor
        // that contains the selection).
        let toolbar_host: Rc<Cell<usize>> = Rc::new(Cell::new(0));
        let toolbar_state_for_tb = toolbar_state.clone();
        let cmds_c = commands.clone();
        let redo_c = redo.clone();
        let redraw_u = schedule_redraw.clone();

        let callbacks = ToolbarCallbacks {
            on_undo: Rc::new({
                let cmds_c = cmds_c.clone();
                let redo_c = redo_c.clone();
                let redraw = redraw_u.clone();
                move || {
                    if let Some(c) = cmds_c.borrow_mut().pop() {
                        redo_c.borrow_mut().push(c);
                        log::info!("Undo: {} commands remaining", cmds_c.borrow().len());
                        redraw();
                    }
                }
            }),
            on_redo: {
                let cmds = commands.clone();
                let redo = redo.clone();
                let redraw = schedule_redraw.clone();
                Rc::new(move || {
                    if let Some(c) = redo.borrow_mut().pop() {
                        cmds.borrow_mut().push(c);
                        log::info!("Redo: {} commands remaining", cmds.borrow().len());
                        redraw();
                    }
                })
            },
            on_save: {
                let surf = self.surface.clone();
                let sel = selection.clone();
                let cmds = commands.clone();
                let outcome = outcome.clone();
                let loopc = main_loop.clone();
                let windows = windows.clone();
                let host = toolbar_host.clone();
                Rc::new(move || {
                    log::info!("Save button clicked");
                    let s = *sel.borrow();
                    let cmds_snapshot = cmds.borrow().clone();
                    let rendered = match crop_and_render(&surf, s, &cmds_snapshot) {
                        Ok(r) => r,
                        Err(e) => {
                            log::error!("Render failed: {e}");
                            return;
                        }
                    };
                    let win = windows[host.get().min(windows.len() - 1)].clone();
                    let outcome = outcome.clone();
                    let loopc = loopc.clone();
                    let windows = windows.clone();
                    // Hide overlays while the portal file chooser is up so it
                    // isn't covered by our fullscreen windows on Wayland.
                    for w in windows.iter() {
                        w.set_visible(false);
                    }
                    // Callback-based FileDialog — do NOT use spawn_future_local
                    // here: we are inside a nested MainLoop and the glib local
                    // executor cannot re-enter (EnterError panic).
                    prompt_save_path(&win, move |path| {
                        let Some(path) = path else {
                            log::info!("Save cancelled — restoring selector");
                            for w in windows.iter() {
                                w.set_visible(true);
                                w.present();
                            }
                            return;
                        };
                        if let Err(e) = save_png(&rendered, &path) {
                            log::error!("Save failed: {e}");
                            for w in windows.iter() {
                                w.set_visible(true);
                                w.present();
                            }
                            return;
                        }
                        log::info!("Saved to {}", path.display());
                        *outcome.borrow_mut() = Some(Outcome::Done);
                        loopc.quit();
                    });
                })
            },
            on_copy: {
                let surf = self.surface.clone();
                let sel = selection.clone();
                let cmds = commands.clone();
                let outcome = outcome.clone();
                let loopc = main_loop.clone();
                let windows = windows.clone();
                Rc::new(move || {
                    log::info!("Copy button clicked");
                    let s = *sel.borrow();
                    let cmds_snapshot = cmds.borrow().clone();
                    match crop_and_render(&surf, s, &cmds_snapshot) {
                        Ok(rendered) => {
                            if let Err(e) = copy_to_clipboard(&rendered) {
                                log::error!("Copy failed: {e}");
                            } else {
                                log::info!("Copied to clipboard");
                            }
                        }
                        Err(e) => log::error!("Render failed: {e}"),
                    }
                    *outcome.borrow_mut() = Some(Outcome::Done);
                    for w in windows.iter() {
                        w.set_visible(false);
                    }
                    let loopc = loopc.clone();
                    glib::source::timeout_add_local_once(
                        std::time::Duration::from_secs(10),
                        move || {
                            loopc.quit();
                        },
                    );
                })
            },
            on_pin: {
                let surf = self.surface.clone();
                let sel = selection.clone();
                let cmds = commands.clone();
                let outcome = outcome.clone();
                let loopc = main_loop.clone();
                Rc::new(move || {
                    log::info!("Pin button clicked");
                    let s = *sel.borrow();
                    let cmds_snapshot = cmds.borrow().clone();
                    match crop_and_render(&surf, s, &cmds_snapshot) {
                        Ok(rendered) => *outcome.borrow_mut() = Some(Outcome::Pin(rendered)),
                        Err(e) => log::error!("Render failed: {e}"),
                    }
                    loopc.quit();
                })
            },
            on_exit: {
                let outcome = outcome.clone();
                let loopc = main_loop.clone();
                Rc::new(move || {
                    log::info!("Exit button clicked");
                    *outcome.borrow_mut() = Some(Outcome::Cancel);
                    loopc.quit();
                })
            },
        };

        let toolbar = build_toolbar(&toolbar_state_for_tb, &callbacks);
        toolbar.set_halign(gtk4::Align::Start);
        toolbar.set_valign(gtk4::Align::Start);
        toolbar.set_visible(false);
        // Start on the primary / first monitor overlay.
        overlays[0].add_overlay(&toolbar);
        let toolbar_c = Rc::new(toolbar);

        let cb_copy = callbacks.on_copy.clone();
        let cb_save = callbacks.on_save.clone();
        let cb_pin = callbacks.on_pin.clone();
        let cb_exit = callbacks.on_exit.clone();

        // Helper to place toolbar on the monitor that owns the selection.
        let place_toolbar = {
            let toolbar = toolbar_c.clone();
            let overlays = overlays.clone();
            let geoms = geoms.clone();
            let host = toolbar_host.clone();
            let desk_x = desk_x;
            let desk_y = desk_y;
            let scale_x = scale_x;
            let scale_y = scale_y;
            Rc::new(move |sel: SelectionRect| {
                let cx = sel.x + sel.w / 2.0;
                let cy = sel.y + sel.h / 2.0;
                let mut idx = 0usize;
                for (i, g) in geoms.iter().enumerate() {
                    let gx0 = (g.x - desk_x) as f64 * scale_x;
                    let gy0 = (g.y - desk_y) as f64 * scale_y;
                    let gx1 = gx0 + g.w as f64 * scale_x;
                    let gy1 = gy0 + g.h as f64 * scale_y;
                    if cx >= gx0 && cx < gx1 && cy >= gy0 && cy < gy1 {
                        idx = i;
                        break;
                    }
                }
                if host.get() != idx {
                    // Reparent toolbar to the target overlay.
                    overlays[host.get()].remove_overlay(&*toolbar);
                    overlays[idx].add_overlay(&*toolbar);
                    host.set(idx);
                }
                let g = &geoms[idx];
                let ox = (g.x - desk_x) as f64 * scale_x;
                let oy = (g.y - desk_y) as f64 * scale_y;
                let local = sel.to_local(ox, oy);
                position_toolbar(&toolbar, &local, g.h as f64 * scale_y);
                toolbar.set_visible(true);
            }) as Rc<dyn Fn(SelectionRect)>
        };

        for drag in &drags {
            let mode_c = mode.clone();
            let sel_c = selection.clone();
            let state_c = state.clone();
            let cur_cmd_c = current.clone();
            let cmds_c = commands.clone();
            let redo_c = redo.clone();
            let tb_state = toolbar_state.clone();
            let redraw = schedule_redraw.clone();
            let toolbar_ref = toolbar_c.clone();
            let place = place_toolbar.clone();
            let handle_ref = active_handle.clone();
            let start_c = start.clone();
            let windows_ref = windows.clone();
            let host = toolbar_host.clone();
            drag.connect_drag_end(move |_, _ox, _oy| {
                let m = mode_c.borrow_mut().take();
                match m {
                    Some(DragMode::Selection) => {
                        let sel = *sel_c.borrow();
                        if sel.is_trivial() {
                            *state_c.borrow_mut() = SelectorState::Idle;
                            toolbar_ref.set_visible(false);
                        } else {
                            *state_c.borrow_mut() = SelectorState::Selected;
                            place(sel);
                        }
                    }
                    Some(DragMode::Moving) | Some(DragMode::Resizing) => {
                        let sel = *sel_c.borrow();
                        if !sel.is_trivial() {
                            *state_c.borrow_mut() = SelectorState::Selected;
                            place(sel);
                        }
                        *handle_ref.borrow_mut() = None;
                    }
                    Some(DragMode::Drawing) => {
                        let tool = *tb_state.active_tool.borrow();
                        if tool == ToolKind::Text {
                            let s = *start_c.borrow();
                            let style = *tb_state.style.borrow();
                            let cmds = cmds_c.clone();
                            let redo = redo_c.clone();
                            let redraw = redraw.clone();
                            let win = windows_ref[host.get().min(windows_ref.len() - 1)].clone();
                            open_text_input(
                                &win,
                                (s.0, s.1),
                                style,
                                Rc::new(move |cmd| {
                                    cmds.borrow_mut().push(cmd);
                                    redo.borrow_mut().clear();
                                    redraw();
                                }),
                            );
                            *cur_cmd_c.borrow_mut() = None;
                        } else if let Some(c) = cur_cmd_c.borrow_mut().take() {
                            if !is_command_trivial(&c) {
                                log::info!("Committed drawing command: {:?}", c);
                                cmds_c.borrow_mut().push(c);
                                redo_c.borrow_mut().clear();
                            }
                        }
                    }
                    None => {}
                }
                redraw();
            });
        }

        // Keyboard shortcuts on every window (focus may be on any monitor).
        for w in windows.iter() {
            let loop_c = main_loop.clone();
            let outcome_c = outcome.clone();
            let cb_copy = cb_copy.clone();
            let cb_save = cb_save.clone();
            let cb_pin = cb_pin.clone();
            let cb_exit = cb_exit.clone();
            let key = EventControllerKey::new();
            key.connect_key_pressed(move |_, keyval, _kc, mods| {
                let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
                if ctrl {
                    match keyval {
                        gdk::Key::c | gdk::Key::C => {
                            log::info!("Ctrl+C: copy");
                            cb_copy();
                        }
                        gdk::Key::s | gdk::Key::S => {
                            log::info!("Ctrl+S: save");
                            cb_save();
                        }
                        gdk::Key::p | gdk::Key::P => {
                            log::info!("Ctrl+P: pin");
                            cb_pin();
                        }
                        _ => return glib::Propagation::Proceed,
                    }
                    return glib::Propagation::Stop;
                }
                if keyval == gdk::Key::Escape {
                    log::info!("User pressed Esc, exiting selector");
                    cb_exit();
                    *outcome_c.borrow_mut() = Some(Outcome::Cancel);
                    loop_c.quit();
                }
                glib::Propagation::Proceed
            });
            w.add_controller(key);

            let loop_c = main_loop.clone();
            w.connect_close_request(move |_| {
                loop_c.quit();
                glib::Propagation::Proceed
            });
        }

        for w in windows.iter() {
            w.present();
        }

        main_loop.run();

        for w in windows.iter() {
            w.close();
        }

        let result = match outcome.borrow_mut().take() {
            Some(Outcome::Pin(s)) => Ok(Some(s)),
            _ => Ok(None),
        };
        result
    }
}

// ===================== Monitor helpers =====================

fn list_monitors() -> Vec<MonitorGeom> {
    let Some(display) = gdk4::Display::default() else {
        return Vec::new();
    };
    let model = display.monitors();
    let n = model.n_items();
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let Some(obj) = model.item(i) else { continue };
        let Ok(m) = obj.downcast::<gdk4::Monitor>() else {
            continue;
        };
        let g = m.geometry();
        out.push(MonitorGeom {
            monitor: m,
            x: g.x(),
            y: g.y(),
            w: g.width(),
            h: g.height(),
        });
    }
    out
}

fn desktop_bounds(monitors: &[MonitorGeom]) -> (i32, i32, i32, i32) {
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for m in monitors {
        min_x = min_x.min(m.x);
        min_y = min_y.min(m.y);
        max_x = max_x.max(m.x + m.w);
        max_y = max_y.max(m.y + m.h);
    }
    if monitors.is_empty() {
        return (0, 0, 0, 0);
    }
    (min_x, min_y, max_x - min_x, max_y - min_y)
}

/// Pre-crop the region of the full capture that belongs to one monitor.
fn crop_monitor_bg(
    src: &ImageSurface,
    m: &MonitorGeom,
    desk_x: i32,
    desk_y: i32,
    scale_x: f64,
    scale_y: f64,
) -> Result<ImageSurface> {
    let src_x = ((m.x - desk_x) as f64 * scale_x).round() as i32;
    let src_y = ((m.y - desk_y) as f64 * scale_y).round() as i32;
    let dst_w = (m.w as f64 * scale_x).round().max(1.0) as i32;
    let dst_h = (m.h as f64 * scale_y).round().max(1.0) as i32;

    let out = ImageSurface::create(Format::ARgb32, dst_w, dst_h)
        .map_err(|e| anyhow::anyhow!("create crop surface: {e}"))?;
    let cr = Context::new(&out).map_err(|e| anyhow::anyhow!("cairo ctx: {e}"))?;
    cr.set_source_surface(src, -src_x as f64, -src_y as f64)
        .map_err(|e| anyhow::anyhow!("set_source_surface: {e}"))?;
    cr.paint()
        .map_err(|e| anyhow::anyhow!("paint crop: {e}"))?;
    out.flush();
    Ok(out)
}

fn clone_surface(src: &ImageSurface) -> Result<ImageSurface> {
    // ImageSurface is a GObject-style refcounted handle; clone bumps the ref.
    Ok(src.clone())
}

// ===================== Drawing helpers =====================

fn draw_mask(cr: &Context, w: f64, h: f64, sel: &SelectionRect, state: SelectorState) {
    if state == SelectorState::Idle || sel.is_trivial() {
        let _ = cr.set_source_rgba(0.03, 0.04, 0.055, 0.65);
        cr.rectangle(0.0, 0.0, w, h);
        let _ = cr.fill();
        return;
    }
    // Selection may extend outside this monitor — intersect for the hole.
    let x0 = sel.x.clamp(0.0, w);
    let y0 = sel.y.clamp(0.0, h);
    let x1 = (sel.x + sel.w).clamp(0.0, w);
    let y1 = (sel.y + sel.h).clamp(0.0, h);
    let hw = (x1 - x0).max(0.0);
    let hh = (y1 - y0).max(0.0);

    let _ = cr.save();
    cr.set_fill_rule(cairo::FillRule::EvenOdd);
    let _ = cr.set_source_rgba(0.03, 0.04, 0.055, 0.65);
    cr.rectangle(0.0, 0.0, w, h);
    if hw > 0.0 && hh > 0.0 {
        cr.rectangle(x0, y0, hw, hh);
    }
    let _ = cr.fill();
    let _ = cr.restore();
}

fn draw_selection_border(cr: &Context, sel: &SelectionRect) {
    if sel.is_trivial() {
        return;
    }
    let _ = cr.save();
    let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 0.20);
    cr.set_line_width(6.0);
    cr.rectangle(sel.x, sel.y, sel.w, sel.h);
    let _ = cr.stroke();
    let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 0.95);
    cr.set_line_width(2.0);
    cr.rectangle(sel.x, sel.y, sel.w, sel.h);
    let _ = cr.stroke();

    let hw = 12.0;
    let pts = [
        (sel.x, sel.y),
        (sel.x + sel.w / 2.0, sel.y),
        (sel.x + sel.w, sel.y),
        (sel.x + sel.w, sel.y + sel.h / 2.0),
        (sel.x + sel.w, sel.y + sel.h),
        (sel.x + sel.w / 2.0, sel.y + sel.h),
        (sel.x, sel.y + sel.h),
        (sel.x, sel.y + sel.h / 2.0),
    ];
    for (px, py) in pts {
        let _ = cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        cr.rectangle(px - hw / 2.0, py - hw / 2.0, hw, hw);
        let _ = cr.fill();
        let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 0.95);
        cr.set_line_width(2.0);
        cr.rectangle(px - hw / 2.0, py - hw / 2.0, hw, hw);
        let _ = cr.stroke();
    }
    let _ = cr.restore();
}

fn draw_size_readout(cr: &Context, sel: &SelectionRect) {
    let w_text = format!("{:.0}", sel.w);
    let h_text = format!("{:.0}", sel.h);
    let sep = " × ";
    cr.set_font_size(12.0);
    let w_ext = cr.text_extents(&w_text).map(|e| e.width()).unwrap_or(20.0);
    let s_ext = cr.text_extents(sep).map(|e| e.width()).unwrap_or(20.0);
    let h_ext = cr.text_extents(&h_text).map(|e| e.width()).unwrap_or(20.0);
    let th = cr.text_extents(&w_text).map(|e| e.height()).unwrap_or(12.0);
    let total_w = w_ext + s_ext + h_ext;
    let pad_x = 10.0;
    let pad_y = 5.0;
    let box_w = total_w + pad_x * 2.0;
    let box_h = th + pad_y * 2.0;
    let bx = sel.x;
    let by = if sel.y - box_h - 6.0 >= 0.0 {
        sel.y - box_h - 6.0
    } else {
        sel.y + 6.0
    };

    let _ = cr.save();
    let _ = cr.set_source_rgba(0.0, 0.0, 0.0, 0.75);
    cr.rectangle(bx, by, box_w, box_h);
    let _ = cr.fill();
    let _ = cr.set_source_rgba(1.0, 1.0, 1.0, 0.10);
    cr.set_line_width(1.0);
    cr.rectangle(bx, by, box_w, box_h);
    let _ = cr.stroke();

    let text_y = by + pad_y + th;
    let mut x = bx + pad_x;
    let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 1.0);
    cr.move_to(x, text_y);
    let _ = cr.show_text(&w_text);
    x += w_ext;
    let _ = cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.move_to(x, text_y);
    let _ = cr.show_text(sep);
    x += s_ext;
    let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 1.0);
    cr.move_to(x, text_y);
    let _ = cr.show_text(&h_text);
    let _ = cr.restore();
}

/// Magnifier: `cx/cy` are global (capture) coords for sampling; `local_x/y`
/// are where to draw the lens on this monitor.
fn draw_magnifier(
    cr: &Context,
    src: &ImageSurface,
    cx: f64,
    cy: f64,
    local_x: f64,
    local_y: f64,
    cfg: &MagnifierConfig,
) {
    let mx = local_x + cfg.offset_x;
    let my = local_y + cfg.offset_y;
    let r = cfg.radius;

    let _ = cr.save();
    cr.arc(mx, my, r, 0.0, std::f64::consts::TAU);
    let _ = cr.clip();

    let _ = cr.set_source_rgba(0.10, 0.11, 0.14, 1.0);
    cr.rectangle(mx - r, my - r, r * 2.0, r * 2.0);
    let _ = cr.fill();

    let grid = 8;
    let cell = 12.0;
    let half = (grid / 2) as i32;
    let origin_x = mx - (grid as f64) * cell / 2.0;
    let origin_y = my - (grid as f64) * cell / 2.0;

    // Batch-read the 8×8 neighbourhood under a single with_data lock.
    let pixels = read_pixel_block(src, cx as i32, cy as i32, grid, half);
    let mut center_rgb = (0u8, 0u8, 0u8);
    for gy in 0..grid {
        for gx in 0..grid {
            let (pr, pg, pb) = pixels[gy * grid + gx];
            if gx == half as usize && gy == half as usize {
                center_rgb = (pr, pg, pb);
            }
            let _ =
                cr.set_source_rgba(pr as f64 / 255.0, pg as f64 / 255.0, pb as f64 / 255.0, 1.0);
            cr.rectangle(
                origin_x + gx as f64 * cell,
                origin_y + gy as f64 * cell,
                cell,
                cell,
            );
            let _ = cr.fill();
        }
    }

    let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 0.95);
    cr.set_line_width(2.0);
    let c0 = half as f64 * cell;
    cr.rectangle(origin_x + c0, origin_y + c0, cell * 2.0, cell * 2.0);
    let _ = cr.stroke();
    let _ = cr.restore();

    let info = format!("RGB({}, {}, {})", center_rgb.0, center_rgb.1, center_rgb.2);
    let _ = cr.save();
    cr.set_font_size(10.0);
    let tw = cr.text_extents(&info).map(|e| e.width()).unwrap_or(80.0);
    let tx = mx - tw / 2.0;
    let ty = my + r - 8.0;
    let _ = cr.set_source_rgba(0.0, 0.0, 0.0, 0.6);
    cr.rectangle(tx - 4.0, ty - 10.0, tw + 8.0, 14.0);
    let _ = cr.fill();
    let _ = cr.set_source_rgba(0.85, 0.88, 0.92, 1.0);
    cr.move_to(tx, ty);
    let _ = cr.show_text(&info);
    let _ = cr.restore();

    let _ = cr.save();
    let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 0.25);
    cr.set_line_width(6.0);
    cr.arc(mx, my, r, 0.0, std::f64::consts::TAU);
    let _ = cr.stroke();
    let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 0.95);
    cr.set_line_width(3.0);
    cr.arc(mx, my, r, 0.0, std::f64::consts::TAU);
    let _ = cr.stroke();
    let _ = cr.restore();
}

fn read_pixel_block(
    surf: &ImageSurface,
    cx: i32,
    cy: i32,
    grid: usize,
    half: i32,
) -> Vec<(u8, u8, u8)> {
    let stride = surf.stride() as usize;
    let (w, h) = (surf.width(), surf.height());
    let mut out = vec![(0u8, 0u8, 0u8); grid * grid];
    let _ = surf.with_data(|data: &[u8]| {
        for gy in 0..grid {
            for gx in 0..grid {
                let x = cx + (gx as i32 - half);
                let y = cy + (gy as i32 - half);
                if x < 0 || y < 0 || x >= w || y >= h {
                    continue;
                }
                let off = y as usize * stride + x as usize * 4;
                out[gy * grid + gx] = (data[off + 2], data[off + 1], data[off]);
            }
        }
    });
    out
}

fn position_toolbar(toolbar: &gtk4::Box, sel: &SelectionRect, screen_h: f64) {
    use gtk4::prelude::WidgetExt as _;
    let tb_h = 52.0;
    let below = sel.y + sel.h + 4.0;
    let y = if below + tb_h > screen_h {
        (sel.y - tb_h - 4.0).max(0.0)
    } else {
        below
    };
    toolbar.set_margin_start(sel.x.max(0.0) as i32);
    toolbar.set_margin_top(y as i32);
}

fn crop_and_render(
    src: &ImageSurface,
    sel: SelectionRect,
    cmds: &[DrawCommand],
) -> Result<ImageSurface> {
    let w = sel.w as i32;
    let h = sel.h as i32;
    if w <= 0 || h <= 0 {
        anyhow::bail!("Empty selection");
    }
    let target =
        ImageSurface::create(Format::ARgb32, w, h).context("Failed to create target surface")?;
    let cr = Context::new(&target).context("Failed to create Cairo context")?;
    cr.set_source_surface(src, -sel.x, -sel.y)
        .context("Failed to set source surface")?;
    cr.paint().context("Failed to paint the base image")?;
    render_stack(&cr, cmds, Some(src), (sel.x, sel.y));
    target.flush();
    Ok(target)
}

fn is_command_trivial(c: &DrawCommand) -> bool {
    match c {
        DrawCommand::Rect { w, h, .. }
        | DrawCommand::Ellipse { w, h, .. }
        | DrawCommand::Mosaic { w, h, .. } => *w < 3.0 || *h < 3.0,
        DrawCommand::Line { x1, y1, x2, y2, .. } | DrawCommand::Arrow { x1, y1, x2, y2, .. } => {
            (x2 - x1).abs() < 3.0 && (y2 - y1).abs() < 3.0
        }
        DrawCommand::Brush { points, .. } => points.len() < 2,
        DrawCommand::Text { text, .. } => text.is_empty(),
    }
}

/// Open a native "Save As" dialog and invoke `on_done` with the chosen path
/// (or `None` if the user cancelled).
///
/// Uses the callback form of `FileDialog::save` so it works inside the
/// selector's nested `MainLoop` (async `spawn_future_local` cannot re-enter).
fn prompt_save_path(parent: &Window, on_done: impl FnOnce(Option<std::path::PathBuf>) + 'static) {
    let dialog = gtk4::FileDialog::new();
    dialog.set_title("Save screenshot");
    dialog.set_modal(true);
    dialog.set_accept_label(Some("Save"));

    let filter = gtk4::FileFilter::new();
    filter.set_name(Some("PNG image"));
    filter.add_pattern("*.png");
    filter.add_mime_type("image/png");
    let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>();
    filters.append(&filter);
    dialog.set_filters(Some(&filters));
    dialog.set_default_filter(Some(&filter));

    // Default filename: Screenshot_YYYY-MM-DD_HH-MM-SS.png
    let stamp = chrono_like_stamp();
    dialog.set_initial_name(Some(&format!("Screenshot_{stamp}.png")));

    // Prefer ~/Pictures, then ~/, then leave unset.
    if let Some(dir) = default_save_folder() {
        dialog.set_initial_folder(Some(&dir));
    }

    dialog.save(Some(parent), None::<&gio::Cancellable>, move |result| {
        let path = match result {
            Ok(file) => {
                let mut p = file.path().unwrap_or_default();
                if p.as_os_str().is_empty() {
                    None
                } else {
                    if p.extension().is_none() {
                        p.set_extension("png");
                    }
                    Some(p)
                }
            }
            Err(e) => {
                // User cancel is reported as a glib error with code Cancelled /
                // dismissed — treat any error as "no path".
                log::debug!("Save dialog closed: {e}");
                None
            }
        };
        on_done(path);
    });
}

fn chrono_like_stamp() -> String {
    glib::DateTime::now_local()
        .ok()
        .and_then(|dt| dt.format("%Y-%m-%d_%H-%M-%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "screenshot".to_string())
}

fn default_save_folder() -> Option<gio::File> {
    // XDG Pictures, then home.
    if let Some(pics) = glib::user_special_dir(glib::UserDirectory::Pictures) {
        if pics.is_dir() {
            return Some(gio::File::for_path(pics));
        }
    }
    let home = glib::home_dir();
    if !home.as_os_str().is_empty() {
        return Some(gio::File::for_path(home));
    }
    None
}

fn save_png(surf: &ImageSurface, path: &std::path::Path) -> Result<()> {
    let mut f = std::fs::File::create(path).context("Failed to create file")?;
    surf.write_to_png(&mut f).context("Failed to write PNG")?;
    Ok(())
}

fn copy_to_clipboard(surf: &ImageSurface) -> Result<()> {
    let mut png = Vec::new();
    surf.write_to_png(&mut png)
        .context("Failed to encode PNG")?;
    let bytes = gtk4::glib::Bytes::from(&png);
    let texture = gdk4::Texture::from_bytes(&bytes).context("Failed to create Texture")?;
    let display = gdk4::Display::default().context("No default Display")?;
    display.clipboard().set_texture(&texture);
    Ok(())
}

fn open_text_input(
    parent: &Window,
    pos: (f64, f64),
    style: crate::tools::StrokeStyle,
    on_done: Rc<dyn Fn(DrawCommand)>,
) {
    let dlg = gtk4::Window::new();
    dlg.set_transient_for(Some(parent));
    dlg.set_modal(true);
    dlg.set_title(Some("Text input"));
    dlg.set_default_size(300, 0);
    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Type annotation text, press Enter to confirm"));
    dlg.set_child(Some(&entry));
    dlg.present();
    entry.grab_focus();

    let pos_c = pos;
    let style_c = style;
    let on_done_c = on_done.clone();
    let dlg_c = dlg.clone();
    let entry_c = entry.clone();
    entry.connect_activate(move |_| {
        let text = entry_c.text().to_string();
        if !text.is_empty() {
            on_done_c(DrawCommand::Text {
                x: pos_c.0,
                y: pos_c.1,
                text,
                size: (style_c.width * 5.0).max(16.0),
                color: style_c.color,
            });
        }
        dlg_c.close();
    });
}
