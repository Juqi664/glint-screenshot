//! Selector, magnifier, drawing toolbar, and confirmation logic module.
//!
//! Responsibilities:
//! 1. Create a fullscreen, borderless Wayland window and draw the underlying
//!    screenshot + a semi-transparent mask + the highlighted selection;
//! 2. Mouse-drag to select a region; show the magnifier and color picker while
//!    moving;
//! 3. Once the selection is confirmed, float a toolbar supporting
//!    rectangle/ellipse/line/arrow/brush/mosaic/text annotations;
//! 4. Maintain a drawing command stack for undo/redo;
//! 5. Confirmation actions (save / copy / pin / exit): crop the selection,
//!    replay commands, and produce the final `ImageSurface`.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{Context as _, Result};
use cairo::{Context, Format, ImageSurface};
use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Application, DrawingArea, EventControllerKey, EventControllerMotion, GestureDrag, Overlay,
    Window,
};
use std::sync::Arc;

use crate::tools::{render_command, render_stack, DrawCommand, ToolKind};
use crate::ui::toolbar::{build_toolbar, ToolbarCallbacks, ToolbarState};

/// Selection state machine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SelectorState {
    /// Waiting for the user to start dragging
    Idle,
    /// Currently dragging a selection
    Selecting,
    /// Selection confirmed, toolbar visible, ready to annotate
    Selected,
}

/// Selection geometry (screen coordinates).
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
    /// Whether a screen-coordinate point falls inside the selection
    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
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

/// Drag mode: selection, drawing, moving, or resizing.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DragMode {
    Selection,
    Drawing,
    Moving,
    Resizing,
}

/// Which edge/corner handle is being dragged during a resize.
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

/// Hit-test result for a press inside an existing selection.
#[derive(Debug, Clone, Copy)]
enum Hit {
    /// On a resize handle
    Edge(Handle),
    /// Inside the selection body -> move
    Inside,
    /// Outside the selection -> start a new selection
    Outside,
}

/// Hit radius (px) for resize handles.
const HANDLE_HIT: f64 = 8.0;

/// Test where a screen point falls relative to an existing selection.
fn hit_test(sel: &SelectionRect, x: f64, y: f64) -> Hit {
    let left = sel.x;
    let right = sel.x + sel.w;
    let top = sel.y;
    let bottom = sel.y + sel.h;
    let hs = HANDLE_HIT;

    // Corners first (they overlap edges)
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
    // Edges
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
    // Inside the body
    if x > left && x < right && y > top && y < bottom {
        return Hit::Inside;
    }
    Hit::Outside
}

/// Recompute the selection when dragging a handle to (px, py).
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

/// Final outcome of the selection flow.
enum Outcome {
    /// User cancelled (Esc / exit button)
    Cancel,
    /// Pin: carries the cropped and rendered surface for main to create a PinWindow
    Pin(ImageSurface),
    /// Save/copy already performed (side effect done); no surface to return
    Done,
}

/// Selector and magnifier controller.
pub struct Selector {
    app: Application,
    /// Underlying full-screen screenshot (ARGB32)
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

    /// Start the selection flow. Returns the user's confirmed screenshot
    /// `ImageSurface` (when pinning) or `None` (cancelled / already saved or
    /// copied).
    pub async fn run(self) -> Result<Option<ImageSurface>> {
        let window = Window::new();
        window.set_decorated(false);
        window.fullscreen();
        window.set_application(Some(&self.app));

        let (w, h) = (self.surface.width(), self.surface.height());
        let area = DrawingArea::new();
        area.set_content_width(w);
        area.set_content_height(h);

        // ===== Shared state (used by both draw and event closures) =====
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
        // Move/resize scratch state
        let move_orig: Rc<RefCell<SelectionRect>> = Rc::new(RefCell::new(SelectionRect::default()));
        let resize_orig: Rc<RefCell<SelectionRect>> =
            Rc::new(RefCell::new(SelectionRect::default()));
        let active_handle: Rc<RefCell<Option<Handle>>> = Rc::new(RefCell::new(None));

        // ===== Draw closure =====
        let d_state = state.clone();
        let d_sel = selection.clone();
        let d_cur = cursor.clone();
        let d_cmds = commands.clone();
        let d_cur_cmd = current.clone();
        area.set_draw_func(move |_, cr, _w, _h| {
            // 1. Underlying full-screen screenshot
            let _ = cr.set_source_surface(&*surface, 0.0, 0.0);
            let _ = cr.paint();

            let st = *d_state.borrow();
            let sel = *d_sel.borrow();

            // 2. Mask + selection border
            draw_mask(cr, w as f64, h as f64, &sel, st);
            if st == SelectorState::Selecting && !sel.is_trivial() {
                draw_size_readout(cr, &sel);
                let _ = cr.save();
                // Outer glow
                let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 0.20);
                cr.set_line_width(6.0);
                cr.rectangle(sel.x, sel.y, sel.w, sel.h);
                let _ = cr.stroke();
                // Main border (2px solid blue)
                let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 0.95);
                cr.set_line_width(2.0);
                cr.rectangle(sel.x, sel.y, sel.w, sel.h);
                let _ = cr.stroke();
                let _ = cr.restore();
            }

            // 3. Selected state: replay the command stack + the in-progress command inside the selection
            if st == SelectorState::Selected && !sel.is_trivial() {
                let _ = cr.save();
                cr.translate(sel.x, sel.y);
                render_stack(cr, &d_cmds.borrow(), Some(&*surface), (sel.x, sel.y));
                if let Some(c) = d_cur_cmd.borrow().as_ref() {
                    render_command(cr, c, Some(&*surface), (sel.x, sel.y));
                }
                let _ = cr.restore();
                draw_selection_border(cr, &sel);
            }

            // 4. Magnifier: shown before the selection is confirmed (afterwards
            //    the user is annotating, so hide it). The magnifier itself
            //    renders the pixelated grid + RGB readout.
            if st != SelectorState::Selected {
                let cur = *d_cur.borrow();
                draw_magnifier(cr, &*surface, cur.0, cur.1, &magnifier);
            }
        });

        // ===== Mouse motion: update cursor + trigger redraw =====
        let motion = EventControllerMotion::new();
        {
            let cursor_c = cursor.clone();
            let area_c = area.clone();
            let sel_m = selection.clone();
            let state_m = state.clone();
            let tb_m = toolbar_state.clone();
            motion.connect_motion(move |_, x, y| {
                *cursor_c.borrow_mut() = (x, y);
                // Update cursor for move/resize affordance when a selection exists
                let st = *state_m.borrow();
                let tool = *tb_m.active_tool.borrow();
                if st == SelectorState::Selected && tool == ToolKind::Select {
                    let sel = *sel_m.borrow();
                    let cursor_name = match hit_test(&sel, x, y) {
                        Hit::Edge(Handle::TopLeft) | Hit::Edge(Handle::BottomRight) => {
                            Some("nwse-resize")
                        }
                        Hit::Edge(Handle::TopRight) | Hit::Edge(Handle::BottomLeft) => {
                            Some("nesw-resize")
                        }
                        Hit::Edge(Handle::Top) | Hit::Edge(Handle::Bottom) => Some("ns-resize"),
                        Hit::Edge(Handle::Left) | Hit::Edge(Handle::Right) => Some("ew-resize"),
                        Hit::Inside => Some("move"),
                        Hit::Outside => Some("crosshair"),
                    };
                    if let Some(name) = cursor_name {
                        if let Some(cur) = gtk4::gdk::Cursor::from_name(name, None) {
                            area_c.set_cursor(Some(&cur));
                        }
                    }
                } else if st == SelectorState::Idle || st == SelectorState::Selecting {
                    if let Some(cur) = gtk4::gdk::Cursor::from_name("crosshair", None) {
                        area_c.set_cursor(Some(&cur));
                    }
                }
                area_c.queue_draw();
            });
        }
        area.add_controller(motion);

        // ===== Drag gesture: selection or drawing (branched by the active tool) =====
        let drag = GestureDrag::new();
        let start = Rc::new(RefCell::new((0.0_f64, 0.0_f64)));
        let mode = Rc::new(RefCell::new(None::<DragMode>));

        // drag_begin
        {
            let state_c = state.clone();
            let sel_c = selection.clone();
            let start_c = start.clone();
            let mode_c = mode.clone();
            let cur_cmd_c = current.clone();
            let redo_c = redo.clone();
            let tb_state = toolbar_state.clone();
            let area_c = area.clone();
            let move_orig_c = move_orig.clone();
            let resize_orig_c = resize_orig.clone();
            let handle_c = active_handle.clone();
            let commands_begin = commands.clone();
            drag.connect_drag_begin(move |_, x, y| {
                let tool = *tb_state.active_tool.borrow();
                let st = *state_c.borrow();
                let sel = *sel_c.borrow();

                // When a selection already exists and the Select tool is active,
                // a press inside the selection moves it, on an edge resizes it.
                if st == SelectorState::Selected && tool == ToolKind::Select && !sel.is_trivial() {
                    match hit_test(&sel, x, y) {
                        Hit::Inside => {
                            *mode_c.borrow_mut() = Some(DragMode::Moving);
                            *move_orig_c.borrow_mut() = sel;
                            *start_c.borrow_mut() = (x, y);
                        }
                        Hit::Edge(h) => {
                            *mode_c.borrow_mut() = Some(DragMode::Resizing);
                            *resize_orig_c.borrow_mut() = sel;
                            *handle_c.borrow_mut() = Some(h);
                            *start_c.borrow_mut() = (x, y);
                        }
                        Hit::Outside => {
                            // Start a fresh selection (clears existing annotations)
                            *mode_c.borrow_mut() = Some(DragMode::Selection);
                            *start_c.borrow_mut() = (x, y);
                            *state_c.borrow_mut() = SelectorState::Selecting;
                            *sel_c.borrow_mut() = SelectionRect::default();
                            cur_cmd_c.borrow_mut().take();
                            commands_begin.borrow_mut().clear();
                            redo_c.borrow_mut().clear();
                        }
                    }
                    area_c.queue_draw();
                    return;
                }

                if tool == ToolKind::Select || st != SelectorState::Selected {
                    // Selection mode: start a new selection
                    *mode_c.borrow_mut() = Some(DragMode::Selection);
                    *start_c.borrow_mut() = (x, y);
                    *state_c.borrow_mut() = SelectorState::Selecting;
                    *sel_c.borrow_mut() = SelectionRect::default();
                    // Clear existing annotations when re-selecting
                    cur_cmd_c.borrow_mut().take();
                    commands_begin.borrow_mut().clear();
                    redo_c.borrow_mut().clear();
                } else {
                    // Drawing mode: record the start point relative to the selection
                    let rx = x - sel.x;
                    let ry = y - sel.y;
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
                area_c.queue_draw();
            });
        }

        // drag_update
        {
            let mode_c = mode.clone();
            let start_c = start.clone();
            let sel_c = selection.clone();
            let cur_cmd_c = current.clone();
            let move_orig_c = move_orig.clone();
            let resize_orig_c = resize_orig.clone();
            let handle_c = active_handle.clone();
            let area_c = area.clone();
            let screen_w = w as f64;
            let screen_h = h as f64;
            drag.connect_drag_update(move |_, ox, oy| {
                let m = *mode_c.borrow();
                match m {
                    Some(DragMode::Selection) => {
                        let s = *start_c.borrow();
                        *sel_c.borrow_mut() = SelectionRect {
                            x: s.0.min(s.0 + ox),
                            y: s.1.min(s.1 + oy),
                            w: ox.abs(),
                            h: oy.abs(),
                        };
                    }
                    Some(DragMode::Moving) => {
                        let orig = *move_orig_c.borrow();
                        let mut nx = orig.x + ox;
                        let mut ny = orig.y + oy;
                        // Clamp so the selection stays on screen
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
                        let px = s.0 + ox;
                        let py = s.1 + oy;
                        let mut new = resize_rect(&orig, handle_c.borrow().unwrap(), px, py);
                        // Clamp to screen bounds
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
                        let cx = s.0 + ox;
                        let cy = s.1 + oy;
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
                area_c.queue_draw();
            });
        }

        // ===== Main loop (blocks until the user confirms/cancels) =====
        let main_loop = glib::MainLoop::new(None, false);

        // ===== Floating toolbar (Overlay child, initially hidden) =====
        let overlay = Overlay::new();
        overlay.set_child(Some(&area));

        let toolbar_state_for_tb = toolbar_state.clone();
        let cmds_c = commands.clone();
        let redo_c = redo.clone();
        let area_c2 = area.clone();

        let callbacks = ToolbarCallbacks {
            on_undo: Rc::new(move || {
                if let Some(c) = cmds_c.borrow_mut().pop() {
                    redo_c.borrow_mut().push(c);
                    log::info!("Undo: {} commands remaining", cmds_c.borrow().len());
                    area_c2.queue_draw();
                }
            }),
            on_redo: {
                let cmds = commands.clone();
                let redo = redo.clone();
                let area = area.clone();
                Rc::new(move || {
                    if let Some(c) = redo.borrow_mut().pop() {
                        cmds.borrow_mut().push(c);
                        log::info!("Redo: {} commands remaining", cmds.borrow().len());
                        area.queue_draw();
                    }
                })
            },
            on_save: {
                let surf = self.surface.clone();
                let sel = selection.clone();
                let cmds = commands.clone();
                let outcome = outcome.clone();
                let loopc = main_loop.clone();
                let win = window.clone();
                Rc::new(move || {
                    log::info!("Save button clicked");
                    let s = *sel.borrow();
                    let cmds_snapshot = cmds.borrow().clone();
                    let surf_clone = surf.clone();
                    let outcome = outcome.clone();
                    let loopc = loopc.clone();
                    let win = win.clone();
                    glib::spawn_future_local(async move {
                        match crop_and_render(&surf_clone, s, &cmds_snapshot) {
                            Ok(rendered) => {
                                if let Some(path) = prompt_save_path(&win).await {
                                    if let Err(e) = save_png(&rendered, &path) {
                                        log::error!("Save failed: {e}");
                                    } else {
                                        log::info!("Saved to {}", path.display());
                                    }
                                }
                            }
                            Err(e) => log::error!("Render failed: {e}"),
                        }
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
                let win = window.clone();
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
                    // Wayland clipboard is volatile: the content is owned by
                    // this process and is lost when it exits. If we quit right
                    // after `set_texture`, Ctrl+V in another app pastes
                    // nothing. So hide the window but keep the app alive
                    // (still owning the clipboard) for a few seconds, giving
                    // the user time to paste elsewhere before we finally quit.
                    //
                    // Use a source callback (timeout_add_local_once), NOT
                    // spawn_future_local: this callback runs inside the
                    // selector's nested MainLoop, where the glib LocalExecutor
                    // cannot re-enter the main context (it panics with
                    // EnterError).
                    win.set_visible(false);
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
        toolbar.set_visible(false);
        overlay.add_overlay(&toolbar);
        let toolbar_c = Rc::new(toolbar.clone());

        // Keep clones of the toolbar action callbacks so the keyboard
        // shortcuts below can trigger the same logic as the toolbar buttons.
        let cb_copy = callbacks.on_copy.clone();
        let cb_save = callbacks.on_save.clone();
        let cb_pin = callbacks.on_pin.clone();
        let cb_exit = callbacks.on_exit.clone();

        // ===== drag_end: confirm selection / commit drawing command / text input =====
        {
            let mode_c = mode.clone();
            let sel_c = selection.clone();
            let state_c = state.clone();
            let cur_cmd_c = current.clone();
            let cmds_c = commands.clone();
            let redo_c = redo.clone();
            let tb_state = toolbar_state.clone();
            let area_c = area.clone();
            let toolbar_ref = toolbar_c.clone();
            let window_ref = window.clone();
            let handle_ref = active_handle.clone();
            drag.connect_drag_end(move |_, ox, oy| {
                let m = *mode_c.borrow();
                match m {
                    Some(DragMode::Selection) => {
                        let sel = *sel_c.borrow();
                        if sel.is_trivial() {
                            *state_c.borrow_mut() = SelectorState::Idle;
                            toolbar_ref.set_visible(false);
                        } else {
                            *state_c.borrow_mut() = SelectorState::Selected;
                            position_toolbar(&toolbar_ref, &sel, h as f64);
                            toolbar_ref.set_visible(true);
                        }
                    }
                    Some(DragMode::Moving) | Some(DragMode::Resizing) => {
                        // Keep the selection; just reposition the toolbar.
                        let sel = *sel_c.borrow();
                        if !sel.is_trivial() {
                            *state_c.borrow_mut() = SelectorState::Selected;
                            position_toolbar(&toolbar_ref, &sel, h as f64);
                            toolbar_ref.set_visible(true);
                        }
                        *handle_ref.borrow_mut() = None;
                    }
                    Some(DragMode::Drawing) => {
                        let tool = *tb_state.active_tool.borrow();
                        if tool == ToolKind::Text {
                            // Text tool: on click (almost no movement), pop up an input box
                            if ox.abs() < 3.0 && oy.abs() < 3.0 {
                                let s = *start.borrow();
                                let style = *tb_state.style.borrow();
                                let cmds = cmds_c.clone();
                                let redo = redo_c.clone();
                                let area = area_c.clone();
                                let win = window_ref.clone();
                                open_text_input(
                                    &win,
                                    (s.0, s.1),
                                    style,
                                    Rc::new(move |cmd| {
                                        cmds.borrow_mut().push(cmd);
                                        redo.borrow_mut().clear();
                                        area.queue_draw();
                                    }),
                                );
                            }
                            *cur_cmd_c.borrow_mut() = None;
                        } else {
                            // Commit the current command (if non-trivial)
                            let cc = cur_cmd_c.borrow_mut().take();
                            if let Some(c) = cc {
                                if !is_command_trivial(&c) {
                                    log::info!("Committed drawing command: {:?}", c);
                                    cmds_c.borrow_mut().push(c);
                                    redo_c.borrow_mut().clear();
                                }
                            }
                        }
                    }
                    None => {}
                }
                area_c.queue_draw();
            });
        }
        area.add_controller(drag);

        // ===== Keyboard shortcuts =====
        {
            let loop_c = main_loop.clone();
            let outcome_c = outcome.clone();
            let window_c = window.clone();
            let cb_copy = cb_copy.clone();
            let cb_save = cb_save.clone();
            let cb_pin = cb_pin.clone();
            let cb_exit = cb_exit.clone();
            let key = EventControllerKey::new();
            key.connect_key_pressed(move |_, keyval, _kc, state| {
                let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
                if ctrl {
                    // Ctrl+C -> copy to clipboard, Ctrl+S -> save to file,
                    // Ctrl+P -> pin on top. (Esc handled below without Ctrl.)
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
                    window_c.close();
                }
                glib::Propagation::Proceed
            });
            window.add_controller(key);
        }

        // Also quit the MainLoop when the window is closed, to avoid hanging
        {
            let loop_c = main_loop.clone();
            window.connect_close_request(move |_| {
                loop_c.quit();
                glib::Propagation::Proceed
            });
        }

        window.set_child(Some(&overlay));
        window.present();

        // ===== Block the main loop until the user confirms/cancels =====
        main_loop.run();

        // Always close the selector window once the nested MainLoop has quit.
        // In daemon mode the app does NOT quit after a capture, so without
        // this the fullscreen selector would stay mapped and hide everything
        // (e.g. a PinWindow created afterwards, or just the desktop) — which
        // looks like the Pin/Exit buttons "do nothing". Closing here guarantees
        // the selector disappears for every terminal action.
        window.close();

        // Take the outcome
        let out = outcome.borrow_mut().take();
        match out {
            Some(Outcome::Pin(s)) => Ok(Some(s)),
            _ => Ok(None),
        }
    }
}

// ===================== Drawing helpers =====================

/// Semi-transparent mask: dark outside the selection, transparent inside
/// (highlighted). Matches the prototype's darker overlay (~0.65 alpha).
fn draw_mask(cr: &Context, w: f64, h: f64, sel: &SelectionRect, state: SelectorState) {
    if state == SelectorState::Idle {
        let _ = cr.set_source_rgba(0.03, 0.04, 0.055, 0.65);
        cr.rectangle(0.0, 0.0, w, h);
        let _ = cr.fill();
        return;
    }
    let _ = cr.save();
    cr.set_fill_rule(cairo::FillRule::EvenOdd);
    let _ = cr.set_source_rgba(0.03, 0.04, 0.055, 0.65);
    cr.rectangle(0.0, 0.0, w, h);
    cr.rectangle(sel.x, sel.y, sel.w, sel.h);
    let _ = cr.fill();
    let _ = cr.restore();
}

/// Selection border: 2px blue with a soft outer glow, matching the prototype.
fn draw_selection_border(cr: &Context, sel: &SelectionRect) {
    let _ = cr.save();
    // Outer glow (wider, low-alpha blue)
    let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 0.20);
    cr.set_line_width(6.0);
    cr.rectangle(sel.x, sel.y, sel.w, sel.h);
    let _ = cr.stroke();
    // Main border (2px solid blue)
    let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 0.95);
    cr.set_line_width(2.0);
    cr.rectangle(sel.x, sel.y, sel.w, sel.h);
    let _ = cr.stroke();

    // Resize handles: white squares with a blue border (12px).
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

/// Selection size badge (top-left, above the selection). Format "W × H" with
/// the numbers in accent blue, matching the prototype.
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
    // Position above the selection's top-left; if no room, fall below.
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
    // Width number (blue)
    let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 1.0);
    cr.move_to(x, text_y);
    let _ = cr.show_text(&w_text);
    x += w_ext;
    // " × " (white)
    let _ = cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.move_to(x, text_y);
    let _ = cr.show_text(sep);
    x += s_ext;
    // Height number (blue)
    let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 1.0);
    cr.move_to(x, text_y);
    let _ = cr.show_text(&h_text);
    let _ = cr.restore();
}

/// Magnifier: a circular region at the cursor's bottom-right that shows a
/// pixelated 8x8 grid of the pixels around the cursor, plus the center pixel's
/// RGB. Blue accent border, matching the prototype.
fn draw_magnifier(cr: &Context, src: &ImageSurface, cx: f64, cy: f64, cfg: &MagnifierConfig) {
    let mx = cx + cfg.offset_x;
    let my = cy + cfg.offset_y;
    let r = cfg.radius;

    let _ = cr.save();
    // Clip to the circle so the grid stays round.
    cr.arc(mx, my, r, 0.0, std::f64::consts::TAU);
    let _ = cr.clip();

    // Dark canvas behind the grid.
    let _ = cr.set_source_rgba(0.10, 0.11, 0.14, 1.0);
    cr.rectangle(mx - r, my - r, r * 2.0, r * 2.0);
    let _ = cr.fill();

    // 8x8 pixelated grid sampled from the source around the cursor.
    let grid = 8;
    let cell = 12.0;
    let half = (grid / 2) as i32;
    let origin_x = mx - (grid as f64) * cell / 2.0;
    let origin_y = my - (grid as f64) * cell / 2.0;
    let mut center_rgb = (0u8, 0u8, 0u8);
    for gy in 0..grid {
        for gx in 0..grid {
            let sx = cx as i32 + (gx as i32 - half);
            let sy = cy as i32 + (gy as i32 - half);
            let (pr, pg, pb) = read_pixel(src, sx, sy);
            if gx == half && gy == half {
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

    // Highlight the center 2x2 cells with a blue inset border.
    let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 0.95);
    cr.set_line_width(2.0);
    let c0 = half as f64 * cell;
    cr.rectangle(origin_x + c0, origin_y + c0, cell * 2.0, cell * 2.0);
    let _ = cr.stroke();
    let _ = cr.restore();

    // RGB text below the grid, inside the circle.
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

    // Blue border + soft outer glow around the circle.
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

/// Color picker: show the current pixel's RGB and coordinates near the cursor,
/// in a small dark badge.
#[allow(dead_code)]
fn draw_color_readout(cr: &Context, src: &ImageSurface, cx: f64, cy: f64) {
    let (r, g, b) = read_pixel(src, cx as i32, cy as i32);
    let text = format!("RGB({}, {}, {})  ({:.0},{:.0})", r, g, b, cx, cy);
    let _ = cr.save();
    cr.set_font_size(11.0);
    let th = cr.text_extents(&text).map(|e| e.height()).unwrap_or(11.0);
    let tw = cr.text_extents(&text).map(|e| e.width()).unwrap_or(120.0);
    let bx = cx + 16.0;
    let by = cy + 16.0;
    let _ = cr.set_source_rgba(0.0, 0.0, 0.0, 0.75);
    cr.rectangle(bx, by, tw + 16.0, th + 10.0);
    let _ = cr.fill();
    let _ = cr.set_source_rgba(0.231, 0.510, 0.965, 1.0);
    cr.move_to(bx + 8.0, by + 5.0 + th);
    let _ = cr.show_text(&text);
    let _ = cr.restore();
}

/// Read a pixel's RGB from an ImageSurface (Cairo ARGB32 little-endian = B G R A).
fn read_pixel(surf: &ImageSurface, x: i32, y: i32) -> (u8, u8, u8) {
    let stride = surf.stride() as usize;
    let (w, h) = (surf.width(), surf.height());
    if x < 0 || y < 0 || x >= w || y >= h {
        return (0, 0, 0);
    }
    let mut rgb = (0u8, 0u8, 0u8);
    let _ = surf.with_data(|data: &[u8]| {
        let off = y as usize * stride + x as usize * 4;
        rgb = (data[off + 2], data[off + 1], data[off]);
    });
    rgb
}

// ===================== Toolbar positioning =====================

/// Position the toolbar below the selection (or above it when there is not
/// enough room).
fn position_toolbar(toolbar: &gtk4::Box, sel: &SelectionRect, screen_h: f64) {
    use gtk4::prelude::WidgetExt as _;
    let tb_h = 52.0; // estimated toolbar height (including margins)
    let below = sel.y + sel.h + 4.0;
    let y = if below + tb_h > screen_h {
        (sel.y - tb_h - 4.0).max(0.0)
    } else {
        below
    };
    toolbar.set_margin_start(sel.x as i32);
    toolbar.set_margin_top(y as i32);
}

// ===================== Crop and render =====================

/// Crop the selection and replay the drawing commands, producing the final
/// `ImageSurface` (size = selection size).
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
    // Translate the source surface to (-sel.x,-sel.y) so the selection maps
    // to the target's (0,0)
    cr.set_source_surface(src, -sel.x, -sel.y)
        .context("Failed to set source surface")?;
    cr.paint().context("Failed to paint the base image")?;
    // Replay commands: the cr origin is the selection's top-left; commands
    // use selection-relative coords; src_offset is the selection's position in
    // the source.
    render_stack(&cr, cmds, Some(src), (sel.x, sel.y));
    target.flush();
    Ok(target)
}

/// Whether a command is trivial (too small a movement), used to filter out
/// accidental empty commands.
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

// ===================== Save / copy =====================

/// Pop up a save dialog (GTK4 FileDialog) and return the user's chosen path.
async fn prompt_save_path(parent: &Window) -> Option<std::path::PathBuf> {
    let dialog = gtk4::FileDialog::new();
    dialog.set_title("Save screenshot");
    let filter = gtk4::FileFilter::new();
    filter.set_name(Some("PNG image"));
    filter.add_pattern("*.png");
    let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>();
    filters.append(&filter);
    dialog.set_filters(Some(&filters));
    match dialog.save_future(Some(parent)).await {
        Ok(file) => {
            let mut p = file.path().unwrap_or_default();
            if p.extension().is_none() {
                p.set_extension("png");
            }
            Some(p)
        }
        Err(_) => None,
    }
}

/// Write the surface to a PNG file.
fn save_png(surf: &ImageSurface, path: &std::path::Path) -> Result<()> {
    let mut f = std::fs::File::create(path).context("Failed to create file")?;
    surf.write_to_png(&mut f).context("Failed to write PNG")?;
    Ok(())
}

/// Convert the surface to a `gdk4::Texture` and write it to the clipboard.
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

// ===================== Text input =====================

/// Pop up a simple text-input window; on Enter, call back to produce a
/// `DrawCommand::Text`.
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
