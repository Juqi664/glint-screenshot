//! Floating toolbar widget.
//!
//! Mirrors the prototype UI: dark glassy toolbar with drawing tools, a row of
//! preset color dots, stroke-thickness dots, undo/redo, and labeled action
//! buttons (Copy / Save / Pin / Exit). Color and stroke dots are plain `Box`
//! widgets (no GTK button chrome) driven by `GestureClick`; tool buttons use
//! `ToggleButton` for exclusive grouping.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, GestureClick, Image, Label, Orientation, Separator, ToggleButton,
};

use crate::tools::{Color, StrokeStyle, ToolKind};

/// Shared toolbar state (the same `Rc` is shared with `Selector`).
#[derive(Clone)]
pub struct ToolbarState {
    pub active_tool: Rc<RefCell<ToolKind>>,
    pub style: Rc<RefCell<StrokeStyle>>,
}

impl Default for ToolbarState {
    fn default() -> Self {
        Self {
            active_tool: Rc::new(RefCell::new(ToolKind::Select)),
            style: Rc::new(RefCell::new(StrokeStyle::default())),
        }
    }
}

/// Toolbar action callbacks. All closures are `Rc<dyn Fn()>` so they can be
/// shared and cloned across multiple buttons.
pub struct ToolbarCallbacks {
    pub on_undo: Rc<dyn Fn()>,
    pub on_redo: Rc<dyn Fn()>,
    pub on_save: Rc<dyn Fn()>,
    pub on_copy: Rc<dyn Fn()>,
    pub on_pin: Rc<dyn Fn()>,
    pub on_exit: Rc<dyn Fn()>,
}

/// A named symbolic icon scaled to 18px. Vector symbolic icons stay crisp at
/// any size and inherit the button color via `currentColor`.
fn named_icon(name: &str) -> Image {
    let img = Image::from_icon_name(name);
    img.set_pixel_size(18);
    img
}

/// A small labeled action button (icon + text).
fn action_button(icon_name: &str, label: &str, extra_class: &str) -> Button {
    let btn = Button::new();
    btn.add_css_class("glint-action");
    btn.add_css_class(extra_class);
    let row = GtkBox::new(Orientation::Horizontal, 5);
    let ico = Image::from_icon_name(icon_name);
    ico.set_pixel_size(14);
    row.append(&ico);
    row.append(&Label::new(Some(label)));
    btn.set_child(Some(&row));
    btn
}

/// Build the toolbar widget.
pub fn build_toolbar(state: &ToolbarState, cb: &ToolbarCallbacks) -> GtkBox {
    let bar = GtkBox::new(Orientation::Horizontal, 2);
    bar.add_css_class("glint-toolbar");
    bar.set_halign(Align::Start);
    bar.set_valign(Align::Start);

    // ===== Drawing tool buttons (exclusive toggle group) =====
    // All icons are vector symbolic icons (crisp at any size, inherit color).
    let tools: &[(&str, &str, ToolKind)] = &[
        ("edit-select-symbolic", "Select / move", ToolKind::Select),
        ("glint-tool-rect-symbolic", "Rectangle", ToolKind::Rect),
        ("glint-tool-ellipse-symbolic", "Ellipse", ToolKind::Ellipse),
        ("glint-tool-line-symbolic", "Line", ToolKind::Line),
        ("glint-tool-arrow-symbolic", "Arrow", ToolKind::Arrow),
        ("glint-tool-brush-symbolic", "Brush", ToolKind::Brush),
        ("glint-tool-mosaic-symbolic", "Mosaic", ToolKind::Mosaic),
        ("insert-text-symbolic", "Text", ToolKind::Text),
    ];

    let mut group: Option<ToggleButton> = None;
    for (icon_name, tip, kind) in tools {
        let btn = ToggleButton::new();
        btn.set_tooltip_text(Some(tip));
        btn.add_css_class("glint-tbbtn");
        btn.set_child(Some(&named_icon(icon_name)));
        let state_c = state.clone();
        let kind = *kind;
        btn.connect_toggled(move |b| {
            if b.is_active() {
                *state_c.active_tool.borrow_mut() = kind;
                log::info!("Switched tool: {:?}", kind);
            }
        });
        if let Some(g) = group.as_ref() {
            btn.set_group(Some(g));
        } else {
            btn.set_active(true);
            group = Some(btn.clone());
        }
        bar.append(&btn);
    }

    bar.append(&Separator::new(Orientation::Vertical));

    // ===== Color picker dots (plain Box + GestureClick) =====
    let colors: &[(&str, &str, Color)] = &[
        ("glint-color-red", "Red", Color::RED),
        ("glint-color-orange", "Orange", Color::ORANGE),
        ("glint-color-yellow", "Yellow", Color::YELLOW),
        ("glint-color-green", "Green", Color::GREEN),
        ("glint-color-blue", "Blue", Color::BLUE),
        ("glint-color-white", "White", Color::WHITE),
        ("glint-color-black", "Black", Color::BLACK),
    ];
    let color_dots: Vec<GtkBox> = colors
        .iter()
        .map(|(cls, tip, _)| {
            let dot = GtkBox::new(Orientation::Horizontal, 0);
            dot.add_css_class("glint-color-dot");
            dot.add_css_class(cls);
            dot.set_tooltip_text(Some(tip));
            // Prevent the dot from stretching to the toolbar height (which
            // would turn the circle into a vertical pill).
            dot.set_valign(Align::Center);
            dot.set_halign(Align::Center);
            dot
        })
        .collect();
    for (i, dot) in color_dots.iter().enumerate() {
        let state_c = state.clone();
        let color = colors[i].2;
        let all = color_dots.clone();
        let click = GestureClick::new();
        dot.add_controller(click.clone());
        click.connect_released(move |_, _, _, _| {
            state_c.style.borrow_mut().color = color;
            for (j, d) in all.iter().enumerate() {
                if i == j {
                    d.add_css_class("glint-color-active");
                } else {
                    d.remove_css_class("glint-color-active");
                }
            }
        });
    }
    // Red is active by default (matches StrokeStyle::default color).
    color_dots[0].add_css_class("glint-color-active");
    for d in &color_dots {
        bar.append(d);
    }

    bar.append(&Separator::new(Orientation::Vertical));

    // ===== Stroke thickness dots (plain Box + GestureClick) =====
    let sizes: &[(f64, &str)] = &[
        (2.0, "glint-stroke-2"),
        (4.0, "glint-stroke-4"),
        (6.0, "glint-stroke-6"),
        (8.0, "glint-stroke-8"),
    ];
    let stroke_btns: Vec<GtkBox> = sizes
        .iter()
        .map(|(_, dot_cls)| {
            let btn = GtkBox::new(Orientation::Horizontal, 0);
            btn.add_css_class("glint-stroke-btn");
            btn.set_valign(Align::Center);
            btn.set_halign(Align::Center);
            let d = GtkBox::new(Orientation::Horizontal, 0);
            // Expand so the dot fills the button, then center it; otherwise the
            // horizontal box packs the dot to the left and the highlight looks
            // off-center.
            d.set_hexpand(true);
            d.set_vexpand(true);
            d.set_halign(Align::Center);
            d.set_valign(Align::Center);
            d.add_css_class("glint-stroke-dot");
            d.add_css_class(dot_cls);
            btn.append(&d);
            btn
        })
        .collect();
    for (i, btn) in stroke_btns.iter().enumerate() {
        let state_c = state.clone();
        let w = sizes[i].0;
        let all = stroke_btns.clone();
        let click = GestureClick::new();
        btn.add_controller(click.clone());
        click.connect_released(move |_, _, _, _| {
            state_c.style.borrow_mut().width = w;
            for (j, b) in all.iter().enumerate() {
                if i == j {
                    b.add_css_class("glint-stroke-active");
                } else {
                    b.remove_css_class("glint-stroke-active");
                }
            }
        });
    }
    // Default width is 4.0 -> index 1 active.
    stroke_btns[1].add_css_class("glint-stroke-active");
    for b in &stroke_btns {
        bar.append(b);
    }

    bar.append(&Separator::new(Orientation::Vertical));

    // ===== Undo / redo =====
    let undo_btn = Button::new();
    undo_btn.add_css_class("glint-tbbtn");
    undo_btn.set_tooltip_text(Some("Undo"));
    undo_btn.set_child(Some(&named_icon("edit-undo-symbolic")));
    let redo_btn = Button::new();
    redo_btn.add_css_class("glint-tbbtn");
    redo_btn.set_tooltip_text(Some("Redo"));
    redo_btn.set_child(Some(&named_icon("edit-redo-symbolic")));
    {
        let cb = cb.on_undo.clone();
        undo_btn.connect_clicked(move |_| {
            cb();
        });
    }
    {
        let cb = cb.on_redo.clone();
        redo_btn.connect_clicked(move |_| {
            cb();
        });
    }
    bar.append(&undo_btn);
    bar.append(&redo_btn);

    bar.append(&Separator::new(Orientation::Vertical));

    // ===== Action buttons: Copy / Save / Pin (primary) / Exit (danger) =====
    let copy_btn = action_button("edit-copy-symbolic", "Copy", "glint-action-secondary");
    let save_btn = action_button("document-save-symbolic", "Save", "glint-action-secondary");
    let pin_btn = action_button("view-pin-symbolic", "Pin", "glint-action-primary");
    let exit_btn = Button::new();
    exit_btn.add_css_class("glint-action");
    exit_btn.add_css_class("glint-action-danger");
    exit_btn.set_tooltip_text(Some("Exit"));
    exit_btn.set_child(Some(&named_icon("window-close-symbolic")));
    {
        let cb = cb.on_copy.clone();
        copy_btn.connect_clicked(move |_| {
            cb();
        });
    }
    {
        let cb = cb.on_save.clone();
        save_btn.connect_clicked(move |_| {
            cb();
        });
    }
    {
        let cb = cb.on_pin.clone();
        pin_btn.connect_clicked(move |_| {
            cb();
        });
    }
    {
        let cb = cb.on_exit.clone();
        exit_btn.connect_clicked(move |_| {
            cb();
        });
    }
    bar.append(&copy_btn);
    bar.append(&save_btn);
    bar.append(&pin_btn);
    bar.append(&exit_btn);

    bar
}
