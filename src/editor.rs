use image::{DynamicImage, GenericImageView, Rgba, RgbaImage, imageops};
use imageproc::drawing::{
    draw_filled_circle_mut, draw_hollow_ellipse_mut, draw_hollow_rect_mut, draw_line_segment_mut,
};
use imageproc::rect::Rect;
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug)]
pub struct Mark {
    pub kind: i32,
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub color: Rgba<u8>,
    pub width: u32,
    pub points: Vec<Point>,
}

impl Mark {
    pub fn new(kind: i32, x1: f32, y1: f32, x2: f32, y2: f32, color: Rgba<u8>, width: u32) -> Self {
        let mut mark = Self {
            kind,
            x1,
            y1,
            x2,
            y2,
            color,
            width: width.max(1),
            points: Vec::new(),
        };
        if kind == 1 {
            mark.points.push(Point { x: x1, y: y1 });
        }
        mark
    }

    pub fn update_endpoint(&mut self, x: f32, y: f32) {
        self.x2 = x;
        self.y2 = y;
        if self.kind == 1 {
            let point = Point { x, y };
            let should_add = self.points.last().is_none_or(|last| {
                let dx = point.x - last.x;
                let dy = point.y - last.y;
                dx * dx + dy * dy >= 0.00001
            });
            if should_add {
                self.points.push(point);
            }
        }
    }
}

pub fn export(
    source: &Path,
    output: &Path,
    selection: (f32, f32, f32, f32),
    marks: &[Mark],
) -> Result<(), String> {
    let image = image::open(source).map_err(|e| e.to_string())?;
    let (iw, ih) = image.dimensions();
    let (sx, sy, sw, sh) = selection;
    let px = (sx * iw as f32).round().clamp(0.0, iw as f32 - 1.0) as u32;
    let py = (sy * ih as f32).round().clamp(0.0, ih as f32 - 1.0) as u32;
    let pw = (sw * iw as f32).round().max(1.0).min((iw - px) as f32) as u32;
    let ph = (sh * ih as f32).round().max(1.0).min((ih - py) as f32) as u32;
    let mut canvas = imageops::crop_imm(&image, px, py, pw, ph).to_image();

    for mark in marks {
        draw_mark(
            &mut canvas,
            mark,
            iw as f32,
            ih as f32,
            px as f32,
            py as f32,
        );
    }
    DynamicImage::ImageRgba8(canvas)
        .save(output)
        .map_err(|e| e.to_string())
}

pub fn render_marks(mut image: RgbaImage, marks: &[Mark]) -> RgbaImage {
    for mark in marks {
        render_mark_onto(&mut image, mark);
    }
    image
}

pub fn render_mark_onto(image: &mut RgbaImage, mark: &Mark) {
    let (width, height) = image.dimensions();
    draw_mark(image, mark, width as f32, height as f32, 0.0, 0.0);
}

fn draw_mark(img: &mut RgbaImage, mark: &Mark, iw: f32, ih: f32, ox: f32, oy: f32) {
    let to_pixel = |x: f32, y: f32| (x * iw - ox, y * ih - oy);
    let (x1, y1) = to_pixel(mark.x1, mark.y1);
    let (x2, y2) = to_pixel(mark.x2, mark.y2);
    let width = mark.width.max(1);

    match mark.kind {
        1 => {
            let points: Vec<(f32, f32)> = if mark.points.is_empty() {
                vec![(x1, y1), (x2, y2)]
            } else {
                mark.points
                    .iter()
                    .map(|point| to_pixel(point.x, point.y))
                    .collect()
            };
            draw_freehand(img, &points, mark.color, width);
        }
        2 => draw_rectangle(img, (x1, y1), (x2, y2), mark.color, width),
        3 => draw_ellipse(img, (x1, y1), (x2, y2), mark.color, width),
        4 => draw_stroke(img, (x1, y1), (x2, y2), mark.color, width),
        5 => draw_arrow(img, (x1, y1), (x2, y2), mark.color, width),
        6 => draw_dashed(img, (x1, y1), (x2, y2), mark.color, width),
        _ => {}
    }
}

fn draw_freehand(img: &mut RgbaImage, points: &[(f32, f32)], color: Rgba<u8>, width: u32) {
    if points.is_empty() {
        return;
    }
    let radius = ((width as f32) / 2.0).ceil().max(1.0) as i32;
    if points.len() == 1 {
        draw_filled_circle_mut(img, (points[0].0 as i32, points[0].1 as i32), radius, color);
        return;
    }

    for pair in points.windows(2) {
        draw_round_segment(img, pair[0], pair[1], color, width);
    }
}

fn draw_rectangle(img: &mut RgbaImage, a: (f32, f32), b: (f32, f32), color: Rgba<u8>, width: u32) {
    let left = a.0.min(b.0);
    let right = a.0.max(b.0);
    let top = a.1.min(b.1);
    let bottom = a.1.max(b.1);
    let rect_width = (right - left).round().max(1.0) as u32;
    let rect_height = (bottom - top).round().max(1.0) as u32;
    let rect = Rect::at(left.round() as i32, top.round() as i32).of_size(rect_width, rect_height);

    let layers = width.min(rect.width().min(rect.height()).saturating_add(1) / 2);
    for layer in 0..layers.max(1) {
        let inset = layer as i32;
        let inner_width = rect.width().saturating_sub(layer * 2).max(1);
        let inner_height = rect.height().saturating_sub(layer * 2).max(1);
        draw_hollow_rect_mut(
            img,
            Rect::at(rect.left() + inset, rect.top() + inset).of_size(inner_width, inner_height),
            color,
        );
    }
}

fn draw_ellipse(img: &mut RgbaImage, a: (f32, f32), b: (f32, f32), color: Rgba<u8>, width: u32) {
    let cx = ((a.0 + b.0) / 2.0).round() as i32;
    let cy = ((a.1 + b.1) / 2.0).round() as i32;
    let rx = ((a.0 - b.0).abs() / 2.0).round().max(1.0) as i32;
    let ry = ((a.1 - b.1).abs() / 2.0).round().max(1.0) as i32;
    let layers = width.min(rx.max(1) as u32).min(ry.max(1) as u32);

    for layer in 0..layers.max(1) {
        draw_hollow_ellipse_mut(
            img,
            (cx, cy),
            (rx - layer as i32).max(1),
            (ry - layer as i32).max(1),
            color,
        );
    }
}

fn draw_stroke(img: &mut RgbaImage, a: (f32, f32), b: (f32, f32), color: Rgba<u8>, width: u32) {
    draw_round_segment(img, a, b, color, width);
}

fn draw_arrow(img: &mut RgbaImage, a: (f32, f32), b: (f32, f32), color: Rgba<u8>, width: u32) {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let length = (dx * dx + dy * dy).sqrt();
    if length < 0.5 {
        draw_filled_circle_mut(img, (b.0 as i32, b.1 as i32), (width as i32).max(1), color);
        return;
    }

    let angle = dy.atan2(dx);
    let head_length = (width as f32 * 4.0).max(12.0).min(length * 0.55);
    let head_angle = 0.48_f32;
    let wing_a = (
        b.0 - head_length * (angle - head_angle).cos(),
        b.1 - head_length * (angle - head_angle).sin(),
    );
    let wing_b = (
        b.0 - head_length * (angle + head_angle).cos(),
        b.1 - head_length * (angle + head_angle).sin(),
    );

    draw_round_segment(img, a, b, color, width);
    draw_round_segment(img, b, wing_a, color, width);
    draw_round_segment(img, b, wing_b, color, width);
}

fn draw_dashed(img: &mut RgbaImage, a: (f32, f32), b: (f32, f32), color: Rgba<u8>, width: u32) {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let length = (dx * dx + dy * dy).sqrt();
    if length < 0.5 {
        return;
    }

    let ux = dx / length;
    let uy = dy / length;
    let dash = (width as f32 * 3.5).max(9.0);
    let gap = (width as f32 * 2.5).max(7.0);
    let mut cursor = 0.0;
    while cursor < length {
        let end = (cursor + dash).min(length);
        draw_round_segment(
            img,
            (a.0 + ux * cursor, a.1 + uy * cursor),
            (a.0 + ux * end, a.1 + uy * end),
            color,
            width,
        );
        cursor += dash + gap;
    }
}

fn draw_round_segment(
    img: &mut RgbaImage,
    a: (f32, f32),
    b: (f32, f32),
    color: Rgba<u8>,
    width: u32,
) {
    let radius = ((width as f32) / 2.0).ceil().max(1.0) as i32;
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let length = (dx * dx + dy * dy).sqrt();
    let steps = (length / (radius.max(1) as f32)).ceil().max(1.0) as u32;

    draw_line_segment_mut(img, a, b, color);
    for index in 0..=steps {
        let t = index as f32 / steps as f32;
        let x = a.0 + dx * t;
        let y = a.1 + dy * t;
        draw_filled_circle_mut(img, (x.round() as i32, y.round() as i32), radius, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red() -> Rgba<u8> {
        Rgba([255, 0, 0, 255])
    }

    fn has_ink(image: &RgbaImage, x: u32, y: u32) -> bool {
        image.get_pixel(x, y).0[0] > 200 && image.get_pixel(x, y).0[1] < 80
    }

    #[test]
    fn freehand_uses_intermediate_points() {
        let mut image = RgbaImage::new(100, 100);
        let mut mark = Mark::new(1, 0.1, 0.1, 0.1, 0.1, red(), 4);
        mark.update_endpoint(0.5, 0.5);
        mark.update_endpoint(0.9, 0.1);
        render_marks(image.clone(), &[mark]).clone_into(&mut image);
        assert!(has_ink(&image, 50, 50));
        assert!(has_ink(&image, 20, 20));
    }

    #[test]
    fn dashed_line_contains_gaps() {
        let image = render_marks(
            RgbaImage::new(160, 30),
            &[Mark::new(6, 0.05, 0.5, 0.95, 0.5, red(), 3)],
        );
        assert!(has_ink(&image, 12, 15));
        assert!(has_ink(&image, 35, 15));
        assert!(!has_ink(&image, 22, 15));
    }

    #[test]
    fn arrow_has_head_wings() {
        let image = render_marks(
            RgbaImage::new(120, 120),
            &[Mark::new(5, 0.15, 0.5, 0.85, 0.5, red(), 3)],
        );
        assert!(has_ink(&image, 100, 60));
        assert!(has_ink(&image, 93, 54));
        assert!(has_ink(&image, 93, 66));
    }
}
