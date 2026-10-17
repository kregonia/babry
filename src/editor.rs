use crate::core::RectPx;
use ab_glyph::{Font, FontArc, FontVec, ScaleFont};
use image::{Rgba, RgbaImage, imageops};
use imageproc::drawing::{
    Canvas, draw_filled_circle_mut, draw_hollow_ellipse_mut, draw_hollow_rect_mut,
    draw_line_segment_mut, draw_text_mut, text_size,
};
use imageproc::rect::Rect;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

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
    pub text: String,
    pub font_size: f32,  // 截图像素中的字号，与笔刷粗细独立。
    pub text_width: f32, // 文字输入区的最大宽度，用于保持换行位置一致。
    pub effect_strength: u32,
    pub number: u32, // 数字标注的序号，随撤销/重做保留。
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
            width: width.clamp(1, 64),
            points: Vec::new(),
            text: String::new(),
            font_size: 24.0,
            text_width: f32::INFINITY,
            effect_strength: 8,
            number: 1,
        };
        if matches!(kind, 1 | 7 | 8 | 9 | 12) {
            mark.points.push(Point { x: x1, y: y1 });
        }
        mark
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
    }

    pub fn set_text_style(&mut self, color: Rgba<u8>, font_size: f32, text_width: f32) {
        self.color = color;
        self.font_size = font_size.max(1.0);
        self.text_width = text_width.max(1.0);
    }

    pub fn set_effect_strength(&mut self, strength: u32) {
        self.effect_strength = strength.clamp(1, 20);
    }

    pub fn update_endpoint(&mut self, x: f32, y: f32) {
        self.x2 = x;
        self.y2 = y;
        if matches!(self.kind, 1 | 7 | 8 | 9 | 12) {
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
    source: &RgbaImage,
    output: &Path,
    selection: (f32, f32, f32, f32),
    marks: &[Mark],
) -> Result<(), String> {
    if source.width() == 0 || source.height() == 0 {
        return Err("截图尺寸为空".into());
    }
    let region = selection_rect(source, selection);
    validate_fonts(marks)?;
    render_region(source, region, marks)
        .save(output)
        .map_err(|e| e.to_string())
}

pub(crate) fn validate_fonts(marks: &[Mark]) -> Result<(), String> {
    if marks.iter().any(|mark| {
        (mark.kind == 10 && !mark.text.trim().is_empty() && annotation_font(&mark.text).is_none())
            || (mark.kind == 11 && annotation_font("0123456789").is_none())
    }) {
        Err("无法加载文字标注字体，请设置 BABRY_FONT".into())
    } else {
        Ok(())
    }
}

pub fn render_marks(mut image: RgbaImage, marks: &[Mark]) -> RgbaImage {
    // 橡皮擦还原截图原像素，必须保留标注前的来源；普通绘制不创建这份副本。
    let source = marks
        .iter()
        .any(|mark| mark.kind == 12)
        .then(|| image.clone());
    for mark in marks {
        if let Some(source) = &source {
            render_mark_in_region(&mut image, mark, source, (0, 0));
        } else {
            render_mark_onto(&mut image, mark);
        }
    }
    image
}

pub fn render_mark_onto(image: &mut RgbaImage, mark: &Mark) {
    let (width, height) = image.dimensions();
    draw_mark(image, mark, width as f32, height as f32, 0.0, 0.0);
}

/// 所有编辑与导出共用相同的裁剪取整规则。
pub fn selection_rect(source: &RgbaImage, selection: (f32, f32, f32, f32)) -> RectPx {
    let (iw, ih) = source.dimensions();
    if iw == 0 || ih == 0 {
        return RectPx::new(0, 0, 0, 0);
    }
    let (sx, sy, sw, sh) = selection;
    let x = (sx * iw as f32).round().clamp(0.0, iw as f32 - 1.0) as u32;
    let y = (sy * ih as f32).round().clamp(0.0, ih as f32 - 1.0) as u32;
    let width = (sw * iw as f32).round().max(1.0).min((iw - x) as f32) as u32;
    let height = (sh * ih as f32).round().max(1.0).min((ih - y) as f32) as u32;
    RectPx::new(x as i32, y as i32, width, height)
}

pub fn render_region(source: &RgbaImage, region: RectPx, marks: &[Mark]) -> RgbaImage {
    let mut canvas = imageops::crop_imm(
        source,
        region.x as u32,
        region.y as u32,
        region.width,
        region.height,
    )
    .to_image();
    for mark in marks {
        render_mark_in_region(&mut canvas, mark, source, (region.x, region.y));
    }
    canvas
}

/// 标注位置始终相对原截图，局部画布只改变裁剪原点。
pub fn render_mark_in_region(
    canvas: &mut RgbaImage,
    mark: &Mark,
    source: &RgbaImage,
    origin: (i32, i32),
) {
    if mark.kind == 12 {
        erase_mark(canvas, mark, source, origin);
    } else {
        draw_mark(
            canvas,
            mark,
            source.width() as f32,
            source.height() as f32,
            origin.0 as f32,
            origin.1 as f32,
        );
    }
}

/// 保守包围盒包含笔宽、箭头、字形外伸和滤镜采样边距，用于恢复/上传局部预览。
pub fn mark_bounds(mark: &Mark, image_size: (u32, u32), clip: RectPx) -> Option<RectPx> {
    let iw = image_size.0 as f32;
    let ih = image_size.1 as f32;
    let (mut left, mut right) = (mark.x1.min(mark.x2) * iw, mark.x1.max(mark.x2) * iw);
    let (mut top, mut bottom) = (mark.y1.min(mark.y2) * ih, mark.y1.max(mark.y2) * ih);
    for point in &mark.points {
        left = left.min(point.x * iw);
        right = right.max(point.x * iw);
        top = top.min(point.y * ih);
        bottom = bottom.max(point.y * ih);
    }
    let padding = match mark.kind {
        5 => (mark.width as f32 * 5.0).max(16.0),
        7 => (mark.width.saturating_mul(3).clamp(6, 192) as f32 / 2.0).ceil() + 2.0,
        9 => (mark.width as f32 / 2.0).ceil() + mark.effect_strength as f32 * 2.0 + 2.0,
        11 => number_radius(mark) + 3.0,
        10 => {
            if let Some(font) = annotation_font(&mark.text) {
                let scale = font
                    .pt_to_px_scale(mark.font_size * 72.0 / 96.0)
                    .unwrap_or_else(|| mark.font_size.into());
                let lines = wrap_text(&mark.text, font, scale, mark.text_width);
                right = left
                    + lines
                        .iter()
                        .map(|line| text_size(scale, font, line).0)
                        .max()
                        .unwrap_or(0) as f32;
                let scaled = font.as_scaled(scale);
                bottom = top + lines.len() as f32 * (scaled.height() + scaled.line_gap());
            }
            mark.font_size.max(4.0)
        }
        _ => (mark.width as f32 / 2.0).ceil() + 2.0,
    };
    let x = (left - padding)
        .floor()
        .max(clip.x as f32)
        .min(clip.right() as f32) as i32;
    let y = (top - padding)
        .floor()
        .max(clip.y as f32)
        .min(clip.bottom() as f32) as i32;
    let right = (right + padding)
        .ceil()
        .min(clip.right() as f32)
        .max(x as f32) as i32;
    let bottom = (bottom + padding)
        .ceil()
        .min(clip.bottom() as f32)
        .max(y as f32) as i32;
    (right > x && bottom > y).then_some(RectPx::new(x, y, (right - x) as u32, (bottom - y) as u32))
}

fn draw_mark(img: &mut RgbaImage, mark: &Mark, iw: f32, ih: f32, ox: f32, oy: f32) {
    let (x1, y1) = (mark.x1 * iw, mark.y1 * ih);
    let (x2, y2) = (mark.x2 * iw, mark.y2 * ih);
    let width = mark.width.max(1);
    let mut target = RegionCanvas {
        image: img,
        origin: (ox as i32, oy as i32),
        size: (iw as u32, ih as u32),
    };

    match mark.kind {
        1 => {
            let points = mark_points(mark, iw, ih, 0.0, 0.0);
            draw_freehand(&mut target, &points, mark.color, width);
        }
        2 => draw_rectangle(&mut target, (x1, y1), (x2, y2), mark.color, width),
        3 => draw_ellipse(&mut target, (x1, y1), (x2, y2), mark.color, width),
        13 | 14 => {
            // 方形和圆形按截图像素约束，分数缩放和非正方形截图不会拉伸比例。
            let side = (x2 - x1).abs().min((y2 - y1).abs());
            let end = (x1 + side.copysign(x2 - x1), y1 + side.copysign(y2 - y1));
            if mark.kind == 13 {
                draw_rectangle(&mut target, (x1, y1), end, mark.color, width);
            } else {
                draw_ellipse(&mut target, (x1, y1), end, mark.color, width);
            }
        }
        4 => draw_stroke(&mut target, (x1, y1), (x2, y2), mark.color, width),
        5 => draw_arrow(&mut target, (x1, y1), (x2, y2), mark.color, width),
        6 => draw_dashed(&mut target, (x1, y1), (x2, y2), mark.color, width),
        7 => {
            let points = mark_points(mark, iw, ih, ox, oy);
            draw_highlight(target.image, &points, mark.color, width);
        }
        8 | 9 => {
            let points = mark_points(mark, iw, ih, ox, oy);
            draw_effect_brush(
                target.image,
                &points,
                width,
                mark.kind == 9,
                mark.effect_strength,
            );
        }
        11 => draw_number(&mut target, mark, (x1, y1)),
        10 => draw_text(
            &mut target,
            (x1, y1),
            &mark.text,
            mark.color,
            mark.font_size,
            mark.text_width,
        ),
        _ => {}
    }
}

fn number_radius(mark: &Mark) -> f32 {
    let size = mark.font_size.max(12.0);
    let digits = mark.number.to_string().len() as f32;
    (size * 0.7).max(size * (digits * 0.32 + 0.25))
}

fn draw_number(canvas: &mut impl Canvas<Pixel = Rgba<u8>>, mark: &Mark, position: (f32, f32)) {
    let Some(font) = annotation_font("0123456789") else {
        return;
    };
    let text = mark.number.to_string();
    let size = mark.font_size.max(12.0);
    let scale = font
        .pt_to_px_scale(size * 72.0 / 96.0)
        .unwrap_or_else(|| size.into());
    // 字体的 text_size 只返回墨迹高度，起笔还带 ascent 偏移；按实际墨迹边界居中。
    let scaled = font.as_scaled(scale);
    let (mut cursor, mut last) = (0.0, None);
    let (mut left, mut top) = (f32::INFINITY, f32::INFINITY);
    let (mut right, mut bottom) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
    for character in text.chars() {
        let id = scaled.glyph_id(character);
        let glyph = id.with_scale_and_position(scale, ab_glyph::point(cursor, scaled.ascent()));
        cursor += scaled.h_advance(id);
        if let Some(outline) = scaled.outline_glyph(glyph) {
            if let Some(last) = last {
                cursor += scaled.kern(id, last);
            }
            last = Some(id);
            let bounds = outline.px_bounds();
            left = left.min(bounds.min.x);
            top = top.min(bounds.min.y);
            right = right.max(bounds.max.x);
            bottom = bottom.max(bounds.max.y);
        }
    }
    if !left.is_finite() {
        return;
    }
    let center = (position.0.round() as i32, position.1.round() as i32);
    draw_filled_circle_mut(
        canvas,
        center,
        number_radius(mark).ceil() as i32,
        mark.color,
    );
    // 深色圆底使用白字，浅色圆底使用深字，保持编号可读。
    let light = u32::from(mark.color[0]) * 299
        + u32::from(mark.color[1]) * 587
        + u32::from(mark.color[2]) * 114;
    let ink = if light > 160_000 {
        Rgba([20, 25, 35, 255])
    } else {
        Rgba([255, 255, 255, 255])
    };
    draw_text_mut(
        canvas,
        ink,
        center.0 - ((left + right) / 2.0).round() as i32,
        center.1 - ((top + bottom) / 2.0).round() as i32,
        scale,
        font,
        &text,
    );
}

/// 橡皮擦仅还原经过笔迹遮罩的像素；不会把截图挖空或重绘其余标注。
fn erase_mark(canvas: &mut RgbaImage, mark: &Mark, source: &RgbaImage, origin: (i32, i32)) {
    let clip = RectPx::new(origin.0, origin.1, canvas.width(), canvas.height());
    let Some(bounds) = mark_bounds(mark, source.dimensions(), clip) else {
        return;
    };
    let mut mask = RgbaImage::new(bounds.width, bounds.height);
    let mut target = RegionCanvas {
        image: &mut mask,
        origin: (bounds.x, bounds.y),
        size: source.dimensions(),
    };
    let points = mark_points(
        mark,
        source.width() as f32,
        source.height() as f32,
        0.0,
        0.0,
    );
    draw_freehand(&mut target, &points, Rgba([255, 255, 255, 255]), mark.width);
    for (x, y, coverage) in mask.enumerate_pixels() {
        if coverage[3] != 0 {
            let source_x = bounds.x as u32 + x;
            let source_y = bounds.y as u32 + y;
            canvas[(
                (bounds.x - origin.0) as u32 + x,
                (bounds.y - origin.1) as u32 + y,
            )] = source[(source_x, source_y)];
        }
    }
}

// 在原图坐标中光栅化，最终整数像素再平移，避免局部预览的浮点减法改变线段栅格。
struct RegionCanvas<'a> {
    image: &'a mut RgbaImage,
    origin: (i32, i32),
    size: (u32, u32),
}
impl Canvas for RegionCanvas<'_> {
    type Pixel = Rgba<u8>;
    fn dimensions(&self) -> (u32, u32) {
        self.size
    }
    fn get_pixel(&self, x: u32, y: u32) -> Self::Pixel {
        let x = x as i64 - i64::from(self.origin.0);
        let y = y as i64 - i64::from(self.origin.1);
        if x >= 0
            && y >= 0
            && x < i64::from(self.image.width())
            && y < i64::from(self.image.height())
        {
            self.image[(x as u32, y as u32)]
        } else {
            Rgba([0, 0, 0, 0])
        }
    }
    fn draw_pixel(&mut self, x: u32, y: u32, color: Self::Pixel) {
        let x = x as i64 - i64::from(self.origin.0);
        let y = y as i64 - i64::from(self.origin.1);
        if x >= 0
            && y >= 0
            && x < i64::from(self.image.width())
            && y < i64::from(self.image.height())
        {
            self.image[(x as u32, y as u32)] = color;
        }
    }
}

fn mark_points(mark: &Mark, iw: f32, ih: f32, ox: f32, oy: f32) -> Vec<(f32, f32)> {
    let to_pixel = |x: f32, y: f32| (x * iw - ox, y * ih - oy);
    if mark.points.len() < 2 {
        vec![to_pixel(mark.x1, mark.y1), to_pixel(mark.x2, mark.y2)]
    } else {
        mark.points
            .iter()
            .map(|point| to_pixel(point.x, point.y))
            .collect()
    }
}

fn draw_freehand(
    img: &mut impl Canvas<Pixel = Rgba<u8>>,
    points: &[(f32, f32)],
    color: Rgba<u8>,
    width: u32,
) {
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

fn draw_highlight(img: &mut RgbaImage, points: &[(f32, f32)], color: Rgba<u8>, width: u32) {
    if points.is_empty() || img.width() == 0 || img.height() == 0 {
        return;
    }
    let width = width.saturating_mul(3).clamp(6, 192);
    let radius = ((width as f32) / 2.0).ceil().max(1.0) as i32;
    let min_x = points
        .iter()
        .map(|point| point.0)
        .fold(f32::INFINITY, f32::min)
        .floor() as i32
        - radius;
    let min_y = points
        .iter()
        .map(|point| point.1)
        .fold(f32::INFINITY, f32::min)
        .floor() as i32
        - radius;
    let max_x = points
        .iter()
        .map(|point| point.0)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil() as i32
        + radius;
    let max_y = points
        .iter()
        .map(|point| point.1)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil() as i32
        + radius;
    let left = min_x.clamp(0, img.width() as i32 - 1);
    let top = min_y.clamp(0, img.height() as i32 - 1);
    let right = max_x.clamp(left, img.width() as i32 - 1);
    let bottom = max_y.clamp(top, img.height() as i32 - 1);
    let mut mask = image::GrayImage::new((right - left + 1) as u32, (bottom - top + 1) as u32);
    let local = |point: (f32, f32)| (point.0 - left as f32, point.1 - top as f32);

    if points.len() == 1 {
        let point = local(points[0]);
        draw_filled_circle_mut(
            &mut mask,
            (point.0.round() as i32, point.1.round() as i32),
            radius,
            image::Luma([255]),
        );
    } else {
        for pair in points.windows(2) {
            draw_mask_segment(&mut mask, local(pair[0]), local(pair[1]), radius);
        }
    }

    let color = Rgba([color[0], color[1], color[2], 96]);
    for (x, y, coverage) in mask.enumerate_pixels() {
        if coverage[0] != 0 {
            let image_x = left as u32 + x;
            let image_y = top as u32 + y;
            img.put_pixel(
                image_x,
                image_y,
                alpha_blend(*img.get_pixel(image_x, image_y), color),
            );
        }
    }
}

fn draw_mask_segment(mask: &mut image::GrayImage, a: (f32, f32), b: (f32, f32), radius: i32) {
    let length = (b.0 - a.0).hypot(b.1 - a.1);
    let steps = (length / radius.max(1) as f32).ceil().max(1.0) as u32;
    for index in 0..=steps {
        let t = index as f32 / steps as f32;
        draw_filled_circle_mut(
            mask,
            (
                (a.0 + (b.0 - a.0) * t).round() as i32,
                (a.1 + (b.1 - a.1) * t).round() as i32,
            ),
            radius,
            image::Luma([255]),
        );
    }
}

fn alpha_blend(background: Rgba<u8>, foreground: Rgba<u8>) -> Rgba<u8> {
    let alpha = foreground[3] as f32 / 255.0;
    let inverse = 1.0 - alpha;
    Rgba([
        (foreground[0] as f32 * alpha + background[0] as f32 * inverse).round() as u8,
        (foreground[1] as f32 * alpha + background[1] as f32 * inverse).round() as u8,
        (foreground[2] as f32 * alpha + background[2] as f32 * inverse).round() as u8,
        255,
    ])
}

fn draw_effect_brush(
    img: &mut RgbaImage,
    points: &[(f32, f32)],
    width: u32,
    blur: bool,
    strength: u32,
) {
    if points.is_empty() || img.width() == 0 || img.height() == 0 {
        return;
    }

    let radius = ((width as f32) / 2.0).ceil().max(1.0) as i32;
    let strength = strength.clamp(1, 20);
    let padding = if blur {
        (strength.saturating_mul(2)) as i32
    } else {
        0
    };
    let min_x = points
        .iter()
        .map(|point| point.0)
        .fold(f32::INFINITY, f32::min)
        .floor() as i32
        - radius
        - padding;
    let min_y = points
        .iter()
        .map(|point| point.1)
        .fold(f32::INFINITY, f32::min)
        .floor() as i32
        - radius
        - padding;
    let max_x = points
        .iter()
        .map(|point| point.0)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil() as i32
        + radius
        + padding;
    let max_y = points
        .iter()
        .map(|point| point.1)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil() as i32
        + radius
        + padding;
    let left = min_x.clamp(0, img.width() as i32 - 1);
    let top = min_y.clamp(0, img.height() as i32 - 1);
    let right = max_x.clamp(left, img.width() as i32 - 1);
    let bottom = max_y.clamp(top, img.height() as i32 - 1);
    let region_width = (right - left + 1) as u32;
    let region_height = (bottom - top + 1) as u32;
    let source =
        imageops::crop_imm(img, left as u32, top as u32, region_width, region_height).to_image();
    let effected = if blur {
        imageops::blur(&source, (strength as f32 / 2.0).clamp(0.5, 10.0))
    } else {
        pixelate(&source, strength.max(4))
    };

    let mut mask = image::GrayImage::new(region_width, region_height);
    let local = |point: (f32, f32)| (point.0 - left as f32, point.1 - top as f32);
    if points.len() == 1 {
        let point = local(points[0]);
        draw_filled_circle_mut(
            &mut mask,
            (point.0.round() as i32, point.1.round() as i32),
            radius,
            image::Luma([255]),
        );
    } else {
        for pair in points.windows(2) {
            draw_mask_segment(&mut mask, local(pair[0]), local(pair[1]), radius);
        }
    }

    for (x, y, coverage) in mask.enumerate_pixels() {
        if coverage[0] != 0 {
            img.put_pixel(left as u32 + x, top as u32 + y, *effected.get_pixel(x, y));
        }
    }
}

fn pixelate(source: &RgbaImage, block: u32) -> RgbaImage {
    let block = block.clamp(4, 32);
    let mut output = source.clone();
    for top in (0..source.height()).step_by(block as usize) {
        for left in (0..source.width()).step_by(block as usize) {
            let right = (left + block).min(source.width());
            let bottom = (top + block).min(source.height());
            let mut sum = [0u32; 4];
            let mut count = 0u32;
            for y in top..bottom {
                for x in left..right {
                    let pixel = source.get_pixel(x, y);
                    for channel in 0..4 {
                        sum[channel] += pixel[channel] as u32;
                    }
                    count += 1;
                }
            }
            if count == 0 {
                continue;
            }
            let average = Rgba([
                (sum[0] / count) as u8,
                (sum[1] / count) as u8,
                (sum[2] / count) as u8,
                (sum[3] / count) as u8,
            ]);
            for y in top..bottom {
                for x in left..right {
                    output.put_pixel(x, y, average);
                }
            }
        }
    }
    output
}

fn draw_text(
    img: &mut impl Canvas<Pixel = Rgba<u8>>,
    position: (f32, f32),
    text: &str,
    color: Rgba<u8>,
    size: f32,
    max_width: f32,
) {
    let Some(font) = annotation_font(text) else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }
    // Slint 字号以 em 为单位，ab_glyph 的 PxScale 以升部与降部总高度为单位。
    let scale = font
        .pt_to_px_scale(size * 72.0 / 96.0)
        .unwrap_or_else(|| ab_glyph::PxScale::from(size));
    let scaled_font = font.as_scaled(scale);
    let line_height = scaled_font.height() + scaled_font.line_gap();
    let x = position.0.round() as i32;
    let lines = wrap_text(text, font, scale, max_width);
    for (line_index, line) in lines.iter().enumerate() {
        let y = (position.1 + scaled_font.line_gap() / 2.0 + line_index as f32 * line_height)
            .round() as i32;
        draw_text_mut(img, color, x, y, scale, font, line);
    }
}

// 与原位输入的逐字换行对应，同时保留用户主动输入的空行。
fn wrap_text(text: &str, font: &FontArc, scale: ab_glyph::PxScale, max_width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let scaled = font.as_scaled(scale);
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut width = 0.0;
        let mut last = None;
        for character in paragraph.chars().filter(|character| *character != '\r') {
            let id = scaled.glyph_id(character);
            let advance = scaled.h_advance(id);
            let outlined = font.outline(id).is_some();
            // 与 imageproc::text_size 的前进量和字距累计顺序一致，每个字形只测量一次。
            let mut next_width = width + advance;
            if outlined && let Some(last) = last {
                next_width += scaled.kern(id, last);
            }
            if !line.is_empty() && (next_width as u32) as f32 > max_width {
                lines.push(std::mem::take(&mut line));
                width = advance;
                last = outlined.then_some(id);
            } else {
                width = next_width;
                if outlined {
                    last = Some(id);
                }
            }
            line.push(character);
        }
        lines.push(line);
    }
    lines
}

// 字体文件在实际需要文字时才打开；回退字体仅在主字体缺字时加载。
#[derive(Clone)]
pub struct AnnotationFont {
    pub font: FontArc,
    pub index: u32,
}

impl AsRef<[u8]> for AnnotationFont {
    fn as_ref(&self) -> &[u8] {
        self.font.font_data()
    }
}

struct FontCandidate {
    path: PathBuf,
    index: u32,
    loaded: OnceLock<Option<AnnotationFont>>,
}

pub fn annotation_font_for(text: &str) -> Option<(usize, &'static AnnotationFont)> {
    static FONTS: OnceLock<Vec<FontCandidate>> = OnceLock::new();
    let candidates = FONTS.get_or_init(font_candidates);
    let mut fallback = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let loaded = candidate.loaded.get_or_init(|| {
            let data = std::fs::read(&candidate.path).ok()?;
            let font = FontVec::try_from_vec_and_index(data, candidate.index).ok()?;
            Some(AnnotationFont {
                font: FontArc::new(font),
                index: candidate.index,
            })
        });
        if let Some(entry) = loaded {
            fallback.get_or_insert((index, entry));
            if text
                .chars()
                .filter(|c| !c.is_whitespace())
                .all(|c| entry.font.glyph_id(c).0 != 0)
            {
                return Some((index, entry));
            }
        }
    }
    fallback
}

fn annotation_font(text: &str) -> Option<&'static FontArc> {
    annotation_font_for(text).map(|(_, entry)| &entry.font)
}

fn font_candidates() -> Vec<FontCandidate> {
    let default_index = std::env::var("BABRY_FONT_INDEX")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let mut candidates: Vec<(PathBuf, u32)> = Vec::new();
    if let Some(path) = std::env::var_os("BABRY_FONT") {
        candidates.push((PathBuf::from(path), default_index));
    }
    candidates.extend([
        (
            PathBuf::from("/usr/share/fonts/wenquanyi/wqy-zenhei/wqy-zenhei.ttc"),
            0,
        ),
        (
            PathBuf::from("/usr/share/fonts/noto/NotoSansCJK-Regular.ttc"),
            0,
        ),
        (
            PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
            0,
        ),
        (PathBuf::from("/usr/share/fonts/TTF/DejaVuSans.ttf"), 0),
        (
            PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
            0,
        ),
        (
            PathBuf::from("/usr/share/fonts/Adwaita/AdwaitaSans-Regular.ttf"),
            0,
        ),
        (
            PathBuf::from("/usr/share/fonts/liberation/LiberationSans-Regular.ttf"),
            0,
        ),
    ]);

    for pattern in [":charset=4e00", "sans-serif"] {
        if let Ok(output) = Command::new("fc-match")
            .args(["--format=%{file}", pattern])
            .output()
            && output.status.success()
        {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                candidates.push((PathBuf::from(path), 0));
            }
        }
    }

    let mut loaded = std::collections::HashSet::new();
    candidates
        .into_iter()
        .filter(|candidate| loaded.insert(candidate.clone()))
        .map(|(path, index)| FontCandidate {
            path,
            index,
            loaded: OnceLock::new(),
        })
        .collect()
}

fn draw_rectangle(
    img: &mut impl Canvas<Pixel = Rgba<u8>>,
    a: (f32, f32),
    b: (f32, f32),
    color: Rgba<u8>,
    width: u32,
) {
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

fn draw_ellipse(
    img: &mut impl Canvas<Pixel = Rgba<u8>>,
    a: (f32, f32),
    b: (f32, f32),
    color: Rgba<u8>,
    width: u32,
) {
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

fn draw_stroke(
    img: &mut impl Canvas<Pixel = Rgba<u8>>,
    a: (f32, f32),
    b: (f32, f32),
    color: Rgba<u8>,
    width: u32,
) {
    draw_round_segment(img, a, b, color, width);
}

fn draw_arrow(
    img: &mut impl Canvas<Pixel = Rgba<u8>>,
    a: (f32, f32),
    b: (f32, f32),
    color: Rgba<u8>,
    width: u32,
) {
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

fn draw_dashed(
    img: &mut impl Canvas<Pixel = Rgba<u8>>,
    a: (f32, f32),
    b: (f32, f32),
    color: Rgba<u8>,
    width: u32,
) {
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
    img: &mut impl Canvas<Pixel = Rgba<u8>>,
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

    #[test]
    fn highlight_blends_once_and_preserves_opaque_alpha() {
        let image = render_marks(
            RgbaImage::from_pixel(100, 100, Rgba([255, 255, 255, 255])),
            &[Mark::new(7, 0.1, 0.5, 0.9, 0.5, Rgba([255, 0, 0, 255]), 4)],
        );
        let pixel = image.get_pixel(50, 50);
        assert!(pixel[1] < 220);
        assert_eq!(pixel[3], 255);
        assert_eq!(image.get_pixel(0, 0), &Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn mosaic_changes_only_the_selected_region() {
        let mut source = RgbaImage::new(80, 80);
        for y in 0..80 {
            for x in 0..80 {
                source.put_pixel(
                    x,
                    y,
                    if (x + y) % 2 == 0 {
                        Rgba([0, 0, 0, 255])
                    } else {
                        Rgba([255, 255, 255, 255])
                    },
                );
            }
        }
        let image = render_marks(
            source.clone(),
            &[Mark::new(8, 0.25, 0.25, 0.75, 0.75, red(), 8)],
        );
        assert_eq!(image.get_pixel(0, 0), source.get_pixel(0, 0));
        let center = image.get_pixel(40, 40);
        assert!(center[0] > 60 && center[0] < 195);
    }

    #[test]
    fn text_mark_keeps_annotation_content() {
        let mut mark = Mark::new(10, 0.2, 0.3, 0.2, 0.3, red(), 4);
        mark.set_text("Babry 标注".to_string());
        assert_eq!(mark.text, "Babry 标注");
    }

    #[test]
    fn blur_softens_a_high_contrast_region() {
        let mut source = RgbaImage::from_pixel(80, 80, Rgba([0, 0, 0, 255]));
        source.put_pixel(40, 40, Rgba([255, 255, 255, 255]));
        let image = render_marks(source, &[Mark::new(9, 0.25, 0.25, 0.75, 0.75, red(), 8)]);
        assert!(image.get_pixel(40, 40)[0] < 255);
        assert!(image.get_pixel(39, 40)[0] > 0);
        assert_eq!(image.get_pixel(0, 0), &Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn effect_brush_uses_its_entire_continuous_path() {
        let mut source = RgbaImage::new(120, 60);
        for y in 0..source.height() {
            for x in 0..source.width() {
                source.put_pixel(x, y, Rgba([(x * 2) as u8, y as u8, 0, 255]));
            }
        }
        let mut mark = Mark::new(8, 0.1, 0.5, 0.1, 0.5, red(), 10);
        mark.update_endpoint(0.5, 0.5);
        mark.update_endpoint(0.9, 0.5);
        let image = render_marks(source.clone(), &[mark]);
        assert_ne!(image.get_pixel(60, 30), source.get_pixel(60, 30));
        assert_eq!(image.get_pixel(0, 0), source.get_pixel(0, 0));
    }

    #[test]
    fn effect_brush_changes_a_single_click() {
        let mut source = RgbaImage::new(80, 80);
        for y in 0..80 {
            for x in 0..80 {
                source.put_pixel(
                    x,
                    y,
                    if (x + y) % 2 == 0 {
                        Rgba([20, 20, 20, 255])
                    } else {
                        Rgba([240, 240, 240, 255])
                    },
                );
            }
        }
        let mark = Mark::new(8, 0.5, 0.5, 0.5, 0.5, red(), 20);
        let image = render_marks(source.clone(), &[mark]);
        assert_ne!(image.get_pixel(40, 40), source.get_pixel(40, 40));
        assert_eq!(image.get_pixel(0, 0), source.get_pixel(0, 0));
    }

    #[test]
    fn blur_strength_changes_the_rendered_effect() {
        let mut source = RgbaImage::from_pixel(100, 100, Rgba([0, 0, 0, 255]));
        for y in 35..65 {
            for x in 35..65 {
                source.put_pixel(x, y, Rgba([255, 255, 255, 255]));
            }
        }
        let mut low = Mark::new(9, 0.35, 0.5, 0.65, 0.5, red(), 18);
        low.set_effect_strength(1);
        let mut high = low.clone();
        high.set_effect_strength(20);
        let low_image = render_marks(source.clone(), &[low]);
        let high_image = render_marks(source, &[high]);
        assert_ne!(low_image, high_image);
    }

    #[test]
    fn linear_wrapping_matches_existing_measurement_and_font_data_is_shared() {
        let Some((_, entry)) = annotation_font_for("Babry") else {
            return;
        };
        let clone = entry.clone();
        assert_eq!(entry.as_ref().as_ptr(), clone.as_ref().as_ptr());
        let font = &entry.font;
        let scale = font.pt_to_px_scale(18.0).unwrap();
        for text in [
            "AVATAR To Wo\nBabry",
            "Hello  world\r\n\nabc",
            "中文逐字换行，字号保持一致",
            "Longline",
        ] {
            for max_width in [0.0, 1.0, 25.0, 80.0, 1000.0, f32::INFINITY] {
                let mut expected = Vec::new();
                for paragraph in text.split('\n') {
                    let mut line = String::new();
                    for c in paragraph.chars().filter(|c| *c != '\r') {
                        let mut next = line.clone();
                        next.push(c);
                        if !line.is_empty() && text_size(scale, font, &next).0 as f32 > max_width {
                            expected.push(std::mem::take(&mut line));
                        }
                        line.push(c);
                    }
                    expected.push(line);
                }
                assert_eq!(wrap_text(text, font, scale, max_width), expected);
            }
        }
    }

    #[test]
    fn export_renders_text_after_a_crop_offset() {
        if annotation_font("A").is_none() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let output_path = directory.path().join("output.png");
        let source = RgbaImage::from_pixel(100, 80, Rgba([20, 30, 40, 255]));
        let mut mark = Mark::new(10, 0.3, 0.35, 0.3, 0.35, red(), 4);
        mark.set_text("A".to_string());
        export(&source, &output_path, (0.2, 0.25, 0.6, 0.5), &[mark]).unwrap();

        let output = image::open(&output_path).unwrap().to_rgba8();
        assert_eq!(output.dimensions(), (60, 40));
        let mut min_x = output.width();
        let mut max_x = 0;
        let mut found = false;
        for (x, _y, pixel) in output.enumerate_pixels() {
            if pixel[0] > 120 && pixel[0] > pixel[1].saturating_add(60) {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                found = true;
            }
        }
        assert!(found);
        assert!(min_x < 20, "text was not offset with the crop: {min_x}");
        assert!(
            max_x < 35,
            "text was rendered at the source coordinate: {max_x}"
        );
    }
}
