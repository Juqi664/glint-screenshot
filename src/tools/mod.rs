//! Drawing tools and command stack module.
//!
//! ## Design
//! All annotations from the WeChat-style screenshot tool (rectangle / ellipse /
//! line / arrow / brush / mosaic / text) are modeled as immutable
//! [`DrawCommand`]s. Each finished stroke is pushed onto a "committed stack";
//! undo pops it onto a "redo stack"; redo pops it back. Rendering replays the
//! stack in order onto a Cairo context to produce the final image.
//!
//! ## Coordinate system
//! All command coordinates are **relative to the selection's top-left corner**
//! (i.e. selection-internal coordinates, origin = selection (x, y)). This way,
//! once the selection is confirmed, commands can be replayed directly onto the
//! "cropped surface" without a second translation.
//!
//! ## Rendering
//! [`render_command`] takes a Cairo context (already translated to the
//! selection origin) and an optional source surface (the mosaic tool needs to
//! sample source pixels). `cr.save/restore` isolates commands from each other.

use cairo::{Context, ImageSurface};

/// RGBA color (0.0-1.0).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color(pub f64, pub f64, pub f64, pub f64);

impl Color {
    // Palette aligned with the prototype UI (hex values from the design).
    pub const RED: Color = Color(0.929, 0.267, 0.267, 1.0); // #ef4444
    pub const ORANGE: Color = Color(0.976, 0.451, 0.086, 1.0); // #f97316
    pub const YELLOW: Color = Color(0.918, 0.702, 0.031, 1.0); // #eab308
    pub const GREEN: Color = Color(0.133, 0.773, 0.369, 1.0); // #22c55e
    pub const BLUE: Color = Color(0.231, 0.510, 0.965, 1.0); // #3b82f6
    pub const WHITE: Color = Color(0.973, 0.980, 0.988, 1.0); // #f8fafc
    pub const BLACK: Color = Color(0.094, 0.094, 0.106, 1.0); // #18181b

    /// Apply this color as the Cairo source color.
    pub fn apply(self, cr: &Context) {
        let _ = cr.set_source_rgba(self.0, self.1, self.2, self.3);
    }
}

/// Stroke style: color + line width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokeStyle {
    pub color: Color,
    pub width: f64,
}

impl Default for StrokeStyle {
    fn default() -> Self {
        Self {
            color: Color::RED,
            width: 4.0,
        }
    }
}

/// Tool kind (used for toolbar highlight and switching).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToolKind {
    /// Selection / move selection (not a drawing tool)
    Select,
    Rect,
    Ellipse,
    Line,
    Arrow,
    Brush,
    Mosaic,
    Text,
}

/// A finished drawing command. Coordinates are relative to the selection's
/// top-left corner.
#[derive(Debug, Clone)]
pub enum DrawCommand {
    /// Rectangle (stroked)
    Rect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        style: StrokeStyle,
    },
    /// Ellipse (stroked, inscribed in the given bounding box)
    Ellipse {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        style: StrokeStyle,
    },
    /// Straight line
    Line {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        style: StrokeStyle,
    },
    /// Arrow (line + triangular arrowhead)
    Arrow {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        style: StrokeStyle,
    },
    /// Free-hand brush (polyline)
    Brush {
        points: Vec<(f64, f64)>,
        style: StrokeStyle,
    },
    /// Mosaic rectangle: split the region into `block`-pixel cells and fill
    /// each cell with its average color.
    Mosaic {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        block: i32,
    },
    /// Text annotation
    Text {
        x: f64,
        y: f64,
        text: String,
        size: f64,
        color: Color,
    },
}

impl DrawCommand {
    /// The stroke style of this command (mosaic/text have no stroke; returns
    /// the default).
    pub fn style(&self) -> StrokeStyle {
        match self {
            DrawCommand::Rect { style, .. }
            | DrawCommand::Ellipse { style, .. }
            | DrawCommand::Line { style, .. }
            | DrawCommand::Arrow { style, .. }
            | DrawCommand::Brush { style, .. } => *style,
            _ => StrokeStyle::default(),
        }
    }
}

/// Replay a single command onto the given Cairo context.
///
/// `cr` should already be translated to the selection origin (i.e. (0,0) =
/// selection top-left). `src` is the underlying screenshot surface; the mosaic
/// tool samples pixels from it. `src_offset` is the selection's top-left
/// coordinate within `src` (screen coordinates), so mosaic samples the right
/// pixels.
pub fn render_command(
    cr: &Context,
    cmd: &DrawCommand,
    src: Option<&ImageSurface>,
    src_offset: (f64, f64),
) {
    let _ = cr.save();
    match cmd {
        DrawCommand::Rect { x, y, w, h, style } => {
            cr.set_line_width(style.width);
            style.color.apply(cr);
            cr.rectangle(*x, *y, *w, *h);
            let _ = cr.stroke();
        }
        DrawCommand::Ellipse { x, y, w, h, style } => {
            cr.set_line_width(style.width);
            style.color.apply(cr);
            // Cairo has no direct ellipse API; approximate with 4 cubic Bezier
            // segments.
            let (cx, cy) = (*x + *w / 2.0, *y + *h / 2.0);
            let (rx, ry) = (*w / 2.0, *h / 2.0);
            let k = 0.5522847498; // (4/3)*tan(pi/8)
            cr.new_path();
            cr.move_to(cx + rx, cy);
            cr.curve_to(cx + rx, cy + ry * k, cx + rx * k, cy + ry, cx, cy + ry);
            cr.curve_to(cx - rx * k, cy + ry, cx - rx, cy + ry * k, cx - rx, cy);
            cr.curve_to(cx - rx, cy - ry * k, cx - rx * k, cy - ry, cx, cy - ry);
            cr.curve_to(cx + rx * k, cy - ry, cx + rx, cy - ry * k, cx + rx, cy);
            cr.close_path();
            let _ = cr.stroke();
        }
        DrawCommand::Line {
            x1,
            y1,
            x2,
            y2,
            style,
        } => {
            cr.set_line_width(style.width);
            cr.set_line_cap(cairo::LineCap::Round);
            style.color.apply(cr);
            cr.move_to(*x1, *y1);
            cr.line_to(*x2, *y2);
            let _ = cr.stroke();
        }
        DrawCommand::Arrow {
            x1,
            y1,
            x2,
            y2,
            style,
        } => {
            cr.set_line_width(style.width);
            cr.set_line_cap(cairo::LineCap::Round);
            style.color.apply(cr);
            // Shaft
            cr.move_to(*x1, *y1);
            cr.line_to(*x2, *y2);
            let _ = cr.stroke();
            // Arrowhead: ±25° from the end direction, length scaled by line width.
            let dx = *x2 - *x1;
            let dy = *y2 - *y1;
            let len = dx.hypot(dy).max(1.0);
            let ux = dx / len;
            let uy = dy / len;
            let head = (style.width * 4.0).max(12.0);
            let ang = 25.0_f64.to_radians();
            let cos = head * ang.cos();
            let sin = head * ang.sin();
            // Rotate (ux, uy) to get the two arrowhead endpoints.
            let lx = *x2 - (ux * cos + uy * sin);
            let ly = *y2 - (uy * cos - ux * sin);
            let rx = *x2 - (ux * cos - uy * sin);
            let ry = *y2 - (uy * cos + ux * sin);
            cr.move_to(*x2, *y2);
            cr.line_to(lx, ly);
            cr.move_to(*x2, *y2);
            cr.line_to(rx, ry);
            let _ = cr.stroke();
        }
        DrawCommand::Brush { points, style } => {
            if points.len() < 2 {
                return;
            }
            cr.set_line_width(style.width);
            cr.set_line_cap(cairo::LineCap::Round);
            cr.set_line_join(cairo::LineJoin::Round);
            style.color.apply(cr);
            cr.move_to(points[0].0, points[0].1);
            for p in &points[1..] {
                cr.line_to(p.0, p.1);
            }
            let _ = cr.stroke();
        }
        DrawCommand::Mosaic { x, y, w, h, block } => {
            if let Some(surf) = src {
                render_mosaic(cr, surf, *x, *y, *w, *h, *block, src_offset);
            }
        }
        DrawCommand::Text {
            x,
            y,
            text,
            size,
            color,
        } => {
            cr.set_font_size(*size);
            color.apply(cr);
            // Text baseline sits at y; show_text draws from the current point.
            cr.move_to(*x, *y + *size); // Treat y as the text box's top-left.
            let _ = cr.show_text(text);
        }
    }
    let _ = cr.restore();
}

/// Mosaic rendering: within the selection-relative region (x, y, w, h), split
/// into `block`-pixel cells and fill each cell with the average color sampled
/// from the source surface.
///
/// `src_offset` is the selection's top-left coordinate within the source
/// surface (screen coordinates). A selection-relative point (bx, by) maps to
/// source pixel (src_offset.0 + bx, src_offset.1 + by).
fn render_mosaic(
    cr: &Context,
    src: &ImageSurface,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    block: i32,
    src_offset: (f64, f64),
) {
    let stride = src.stride() as usize;
    let sw = src.width();
    let sh = src.height();
    let ox = src_offset.0 as i32;
    let oy = src_offset.1 as i32;

    let block = block.max(2);
    let _ = src.with_data(|data: &[u8]| {
        let mut by = y as i32;
        while (by as f64) < y + h {
            let mut bx = x as i32;
            while (bx as f64) < x + w {
                // Compute this cell's pixel range within the source surface.
                let sx0 = (ox + bx).max(0).min(sw);
                let sy0 = (oy + by).max(0).min(sh);
                let sx1 = (ox + bx + block).max(0).min(sw);
                let sy1 = (oy + by + block).max(0).min(sh);
                if sx1 > sx0 && sy1 > sy0 {
                    let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
                    for py in sy0..sy1 {
                        for px in sx0..sx1 {
                            let off = py as usize * stride + px as usize * 4;
                            // ARGB32 little-endian = B G R A
                            b += data[off] as u32;
                            g += data[off + 1] as u32;
                            r += data[off + 2] as u32;
                            n += 1;
                        }
                    }
                    if n > 0 {
                        let _ = cr.set_source_rgb(
                            r as f64 / (n * 255) as f64,
                            g as f64 / (n * 255) as f64,
                            b as f64 / (n * 255) as f64,
                        );
                        let _ = cr.rectangle(bx as f64, by as f64, block as f64, block as f64);
                        let _ = cr.fill();
                    }
                }
                bx += block;
            }
            by += block;
        }
    });
}

/// Replay the whole command stack onto `cr` (already translated to the
/// selection origin).
pub fn render_stack(
    cr: &Context,
    stack: &[DrawCommand],
    src: Option<&ImageSurface>,
    src_offset: (f64, f64),
) {
    for cmd in stack {
        render_command(cr, cmd, src, src_offset);
    }
}
