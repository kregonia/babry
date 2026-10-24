//! 无依赖的标注命令和 CPU 像素光栅化。
use crate::{core::RectPx, pixels::RgbaPixels};

#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub kind: u8, // 1 pen, 2 rectangle, 7 highlighter, 11 number
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub points: Vec<(f32, f32)>,
    pub color: [u8; 4],
    pub width: u32,
    pub number: u32,
}

impl Mark {
    pub fn new(kind: u8, x1: f32, y1: f32, color: [u8; 4], width: u32) -> Self {
        Self {
            kind,
            x1,
            y1,
            x2: x1,
            y2: y1,
            points: vec![(x1, y1)],
            color,
            width: width.clamp(1, 64),
            number: 1,
        }
    }
    pub fn update(&mut self, x: f32, y: f32) {
        self.x2 = x;
        self.y2 = y;
        if (self.kind == 1 || self.kind == 7)
            && self
                .points
                .last()
                .is_none_or(|p| (p.0 - x).hypot(p.1 - y) > 0.5)
        {
            self.points.push((x, y));
        }
    }
}

pub fn render_marks(image: &mut RgbaPixels, marks: &[Mark], origin: (i32, i32)) {
    for mark in marks {
        render_mark(image, mark, origin);
    }
}

fn render_mark(image: &mut RgbaPixels, mark: &Mark, origin: (i32, i32)) {
    let translate = |p: (f32, f32)| (p.0 - origin.0 as f32, p.1 - origin.1 as f32);
    match mark.kind {
        1 | 7 => {
            let color = if mark.kind == 7 {
                [mark.color[0], mark.color[1], mark.color[2], 105]
            } else {
                mark.color
            };
            let points: Vec<_> = mark.points.iter().copied().map(translate).collect();
            for pair in points.windows(2) {
                draw_segment(
                    image,
                    pair[0],
                    pair[1],
                    color,
                    if mark.kind == 7 {
                        mark.width.saturating_mul(3)
                    } else {
                        mark.width
                    },
                );
            }
            if points.len() == 1 {
                draw_disk(image, points[0], mark.width, color);
            }
        }
        2 => {
            let a = translate((mark.x1, mark.y1));
            let b = translate((mark.x2, mark.y2));
            let (left, right) = (a.0.min(b.0), a.0.max(b.0));
            let (top, bottom) = (a.1.min(b.1), a.1.max(b.1));
            draw_segment(image, (left, top), (right, top), mark.color, mark.width);
            draw_segment(image, (right, top), (right, bottom), mark.color, mark.width);
            draw_segment(
                image,
                (right, bottom),
                (left, bottom),
                mark.color,
                mark.width,
            );
            draw_segment(image, (left, bottom), (left, top), mark.color, mark.width);
        }
        11 => {
            let center = translate((mark.x1, mark.y1));
            let radius = (mark.width.max(14) as f32 * 2.0).max(16.0);
            draw_circle(image, center, radius, mark.color, mark.width.max(2));
            draw_digit(image, center, mark.number, mark.color, mark.width.max(2));
        }
        _ => {}
    }
}

fn draw_segment(image: &mut RgbaPixels, a: (f32, f32), b: (f32, f32), color: [u8; 4], width: u32) {
    let distance = (b.0 - a.0).hypot(b.1 - a.1);
    let steps = distance.ceil().max(1.0) as u32;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        draw_disk(
            image,
            (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t),
            width,
            color,
        );
    }
}
fn draw_disk(image: &mut RgbaPixels, center: (f32, f32), width: u32, color: [u8; 4]) {
    let r = (width as f32 / 2.0).max(0.5);
    let left = (center.0 - r).floor().max(0.0) as u32;
    let top = (center.1 - r).floor().max(0.0) as u32;
    let right = (center.0 + r).ceil().min(image.width() as f32) as u32;
    let bottom = (center.1 + r).ceil().min(image.height() as f32) as u32;
    for y in top..bottom {
        for x in left..right {
            if (x as f32 + 0.5 - center.0).hypot(y as f32 + 0.5 - center.1) <= r {
                blend(image.pixel_mut(x, y).unwrap(), color);
            }
        }
    }
}
fn draw_circle(image: &mut RgbaPixels, c: (f32, f32), r: f32, color: [u8; 4], width: u32) {
    let steps = (r * std::f32::consts::TAU).ceil().max(16.0) as u32;
    for i in 0..steps {
        let a = i as f32 / steps as f32 * std::f32::consts::TAU;
        let p = (c.0 + a.cos() * r, c.1 + a.sin() * r);
        draw_disk(image, p, width, color);
    }
}
fn draw_digit(image: &mut RgbaPixels, c: (f32, f32), number: u32, color: [u8; 4], width: u32) {
    let glyph = number % 10;
    let segments = [
        [true, true, true, true, true, true, false],
        [false, true, true, false, false, false, false],
        [true, true, false, true, true, false, true],
        [true, true, true, true, false, false, true],
        [false, true, true, false, false, true, true],
        [true, false, true, true, false, true, true],
        [true, false, true, true, true, true, true],
        [true, true, true, false, false, false, false],
        [true, true, true, true, true, true, true],
        [true, true, true, true, false, true, true],
    ][glyph as usize];
    let r = width as f32 * 1.8;
    let h = r * 1.6;
    let seg = [
        ((c.0 - r, c.1 - h), (c.0, c.1 - h)),
        ((c.0, c.1 - h), (c.0 + r, c.1 - h)),
        ((c.0 + r, c.1 - h), (c.0 + r, c.1)),
        ((c.0 + r, c.1), (c.0 + r, c.1 + h)),
        ((c.0 - r, c.1 + h), (c.0, c.1 + h)),
        ((c.0, c.1 + h), (c.0 + r, c.1 + h)),
        ((c.0 - r, c.1), (c.0, c.1)),
    ];
    for (i, (a, b)) in seg.into_iter().enumerate() {
        if segments[i] {
            draw_segment(image, a, b, color, width.max(2));
        }
    }
}
fn blend(dst: &mut [u8], src: [u8; 4]) {
    let a = src[3] as u16;
    let inv = 255 - a;
    dst[0] = ((src[0] as u16 * a + dst[0] as u16 * inv) / 255) as u8;
    dst[1] = ((src[1] as u16 * a + dst[1] as u16 * inv) / 255) as u8;
    dst[2] = ((src[2] as u16 * a + dst[2] as u16 * inv) / 255) as u8;
    dst[3] = 255;
}

pub fn selection_rect(width: u32, height: u32, selection: (f32, f32, f32, f32)) -> RectPx {
    let (x, y, w, h) = selection;
    RectPx::new(
        (x.clamp(0., 1.) * width as f32).round() as i32,
        (y.clamp(0., 1.) * height as f32).round() as i32,
        (w.max(0.) * width as f32).round().max(1.) as u32,
        (h.max(0.) * height as f32).round().max(1.) as u32,
    )
    .clamp_to(RectPx::new(0, 0, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renders_strokes_without_extra_images() {
        let mut p = RgbaPixels::new(crate::pixels::PixelSize::new(32, 32).unwrap());
        let mut m = Mark::new(1, 2., 2., [255, 0, 0, 255], 4);
        m.update(28., 28.);
        render_marks(&mut p, &[m], (0, 0));
        assert!(p.bytes.chunks_exact(4).any(|px| px[0] > 200 && px[1] < 50));
    }
}
