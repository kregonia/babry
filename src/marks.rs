//! 标注命令与 tiny-skia 光栅化。
//!
//! 显示瓦片和导出使用同一个 `render_layer`：标注先画到透明图层，再叠加到截图上。
//! 橡皮擦清除图层像素以露出原图；马赛克/模糊读取“原图 + 之前的标注”后写回图层。
use crate::{core::RectPx, font::Fonts, shm_image::ShmImage};
use tiny_skia::{
    BlendMode, FillRule, FilterQuality, LineCap, LineJoin, Paint, Path, PathBuilder, Pattern,
    Pixmap, PixmapMut, PixmapPaint, Rect, SpreadMode, Stroke, Transform,
};

pub const PEN: u8 = 1;
pub const RECTANGLE: u8 = 2;
pub const HIGHLIGHTER: u8 = 7;
pub const MOSAIC: u8 = 8;
pub const BLUR: u8 = 9;
pub const TEXT: u8 = 10;
pub const NUMBER: u8 = 11;
pub const ERASER: u8 = 12;
pub const SQUARE: u8 = 13;
pub const CIRCLE: u8 = 14;

/// 一条标注。坐标与尺寸全部使用截图物理像素。
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub kind: u8,
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub points: Vec<(f32, f32)>,
    pub color: [u8; 4],
    pub width: u32,
    pub number: u32,
    pub font_size: f32,
    pub text: String,
    pub text_width: f32,
    /// 文字排版后的实际宽高，由编辑器用同一套字体测量后写入。
    pub extent: (f32, f32),
    pub strength: u32,
}

impl Mark {
    pub fn new(kind: u8, x: f32, y: f32, color: [u8; 4], width: u32) -> Self {
        Self {
            kind,
            x1: x,
            y1: y,
            x2: x,
            y2: y,
            points: if is_brush(kind) {
                vec![(x, y)]
            } else {
                Vec::new()
            },
            color,
            width: width.clamp(1, 256),
            number: 1,
            font_size: 24.0,
            text: String::new(),
            text_width: f32::INFINITY,
            extent: (0.0, 0.0),
            strength: 8,
        }
    }

    pub fn update(&mut self, x: f32, y: f32) {
        self.x2 = x;
        self.y2 = y;
        if is_brush(self.kind)
            && self
                .points
                .last()
                .is_none_or(|p| (p.0 - x).hypot(p.1 - y) >= 0.5)
        {
            self.points.push((x, y));
        }
    }

    fn stroke_width(&self) -> f32 {
        if self.kind == HIGHLIGHTER {
            self.width.saturating_mul(3).clamp(6, 192) as f32
        } else {
            self.width.max(1) as f32
        }
    }

    /// 方形和圆形以起点为锚，边长取拖动的较短边。
    fn shape_end(&self) -> (f32, f32) {
        if matches!(self.kind, SQUARE | CIRCLE) {
            let dx = self.x2 - self.x1;
            let dy = self.y2 - self.y1;
            let side = dx.abs().min(dy.abs());
            (self.x1 + side.copysign(dx), self.y1 + side.copysign(dy))
        } else {
            (self.x2, self.y2)
        }
    }

    /// 标注可能改变的物理像素范围（未裁剪到截图）。
    pub fn bounds(&self) -> Option<RectPx> {
        let (left, top, right, bottom, pad) = match self.kind {
            k if is_brush(k) => {
                let (mut l, mut t, mut r, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                for &(x, y) in &self.points {
                    l = l.min(x);
                    t = t.min(y);
                    r = r.max(x);
                    b = b.max(y);
                }
                (l, t, r, b, self.stroke_width() / 2.0 + 2.0)
            }
            NUMBER => {
                let radius = number_radius(self);
                (self.x1, self.y1, self.x1, self.y1, radius + 2.0)
            }
            TEXT => (
                self.x1,
                self.y1,
                self.x1 + self.extent.0,
                self.y1 + self.extent.1,
                self.font_size * 0.25 + 2.0,
            ),
            _ => {
                let end = self.shape_end();
                (
                    self.x1.min(end.0),
                    self.y1.min(end.1),
                    self.x1.max(end.0),
                    self.y1.max(end.1),
                    2.0,
                )
            }
        };
        if left > right || top > bottom {
            return None;
        }
        let x = (left - pad).floor() as i32;
        let y = (top - pad).floor() as i32;
        let r = (right + pad).ceil() as i32;
        let b = (bottom + pad).ceil() as i32;
        Some(RectPx::new(
            x,
            y,
            (r - x).max(1) as u32,
            (b - y).max(1) as u32,
        ))
    }
}

pub fn is_brush(kind: u8) -> bool {
    matches!(kind, PEN | HIGHLIGHTER | MOSAIC | BLUR | ERASER)
}

fn number_radius(mark: &Mark) -> f32 {
    let size = mark.font_size.max(12.0);
    let digits = mark.number.to_string().len() as f32;
    (size * 0.7).max(size * (digits * 0.32 + 0.25))
}

pub fn intersect(a: RectPx, b: RectPx) -> Option<RectPx> {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = a.right().min(b.right());
    let bottom = a.bottom().min(b.bottom());
    (right > left && bottom > top)
        .then(|| RectPx::new(left, top, (right - left) as u32, (bottom - top) as u32))
}

pub fn union(a: Option<RectPx>, b: RectPx) -> RectPx {
    let Some(a) = a else { return b };
    let left = a.x.min(b.x);
    let top = a.y.min(b.y);
    let right = a.right().max(b.right());
    let bottom = a.bottom().max(b.bottom());
    RectPx::new(left, top, (right - left) as u32, (bottom - top) as u32)
}

fn paint(color: [u8; 4]) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
    paint.anti_alias = true;
    paint
}

fn brush_path(points: &[(f32, f32)], width: f32) -> Option<(Path, bool)> {
    let mut builder = PathBuilder::new();
    match points {
        [] => return None,
        [(x, y)] => {
            builder.push_circle(*x, *y, (width / 2.0).max(0.5));
            return builder.finish().map(|path| (path, true));
        }
        [first, rest @ ..] => {
            builder.move_to(first.0, first.1);
            for point in rest {
                builder.line_to(point.0, point.1);
            }
        }
    }
    builder.finish().map(|path| (path, false))
}

fn round_stroke(width: f32) -> Stroke {
    Stroke {
        width,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    }
}

/// 在 `target` 上绘制透明标注图层；`origin` 是该图层左上角的截图物理坐标。
pub fn render_layer(
    target: &mut PixmapMut<'_>,
    origin: (i32, i32),
    marks: &[Mark],
    source: &ShmImage,
    fonts: &mut Fonts,
) {
    let area = RectPx::new(origin.0, origin.1, target.width(), target.height());
    let transform = Transform::from_translate(-origin.0 as f32, -origin.1 as f32);
    for (index, mark) in marks.iter().enumerate() {
        if mark.bounds().and_then(|b| intersect(b, area)).is_none() {
            continue;
        }
        match mark.kind {
            PEN | HIGHLIGHTER | ERASER => {
                let width = mark.stroke_width();
                let Some((path, dot)) = brush_path(&mark.points, width) else {
                    continue;
                };
                let mut paint = if mark.kind == HIGHLIGHTER {
                    paint([mark.color[0], mark.color[1], mark.color[2], 96])
                } else {
                    paint(mark.color)
                };
                if mark.kind == ERASER {
                    paint.blend_mode = BlendMode::Clear;
                }
                if dot {
                    target.fill_path(&path, &paint, FillRule::Winding, transform, None);
                } else {
                    target.stroke_path(&path, &paint, &round_stroke(width), transform, None);
                }
            }
            MOSAIC | BLUR => draw_effect(target, origin, area, marks, index, source, fonts),
            RECTANGLE | SQUARE | CIRCLE => draw_shape(target, mark, transform),
            NUMBER => draw_number(target, mark, transform, fonts),
            TEXT => {
                let paint = paint(mark.color);
                fonts.text(
                    &mark.text,
                    mark.x1,
                    mark.y1,
                    mark.font_size,
                    mark.text_width,
                    |path| target.fill_path(path, &paint, FillRule::Winding, transform, None),
                );
            }
            _ => {}
        }
    }
}

fn draw_shape(target: &mut PixmapMut<'_>, mark: &Mark, transform: Transform) {
    let end = mark.shape_end();
    let (left, right) = (mark.x1.min(end.0), mark.x1.max(end.0));
    let (top, bottom) = (mark.y1.min(end.1), mark.y1.max(end.1));
    if right - left < 1.0 && bottom - top < 1.0 {
        return;
    }
    let width = mark.width.max(1) as f32;
    let paint = paint(mark.color);
    // 线宽向内生长，与旧版逐层内缩的矩形边框一致。
    let half = width / 2.0;
    if right - left <= width || bottom - top <= width {
        if let Some(rect) = Rect::from_ltrb(left, top, right.max(left + 1.0), bottom.max(top + 1.0))
        {
            let path = if mark.kind == CIRCLE {
                PathBuilder::from_oval(rect)
            } else {
                Some(PathBuilder::from_rect(rect))
            };
            if let Some(path) = path {
                target.fill_path(&path, &paint, FillRule::Winding, transform, None);
            }
        }
        return;
    }
    let Some(rect) = Rect::from_ltrb(left + half, top + half, right - half, bottom - half) else {
        return;
    };
    let path = if mark.kind == CIRCLE {
        PathBuilder::from_oval(rect)
    } else {
        Some(PathBuilder::from_rect(rect))
    };
    if let Some(path) = path {
        let stroke = Stroke {
            width,
            line_join: LineJoin::Miter,
            ..Stroke::default()
        };
        target.stroke_path(&path, &paint, &stroke, transform, None);
    }
}

fn draw_number(target: &mut PixmapMut<'_>, mark: &Mark, transform: Transform, fonts: &mut Fonts) {
    let radius = number_radius(mark);
    let mut builder = PathBuilder::new();
    builder.push_circle(mark.x1, mark.y1, radius);
    if let Some(circle) = builder.finish() {
        target.fill_path(
            &circle,
            &paint(mark.color),
            FillRule::Winding,
            transform,
            None,
        );
    }
    // 深色圆底使用白字，浅色圆底使用深字。
    let light = u32::from(mark.color[0]) * 299
        + u32::from(mark.color[1]) * 587
        + u32::from(mark.color[2]) * 114;
    let ink = if light > 160_000 {
        [20, 25, 35, 255]
    } else {
        [255, 255, 255, 255]
    };
    let text = mark.number.to_string();
    let size = mark.font_size.max(12.0);
    // 先测量真实墨迹边界，再按圆心居中，避免字体升部造成偏移。
    let (mut l, mut t, mut r, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    fonts.text(&text, 0.0, 0.0, size, f32::INFINITY, |path| {
        let bounds = path.bounds();
        l = l.min(bounds.left());
        t = t.min(bounds.top());
        r = r.max(bounds.right());
        b = b.max(bounds.bottom());
    });
    if l > r {
        return;
    }
    let paint = paint(ink);
    fonts.text(
        &text,
        mark.x1 - (l + r) / 2.0,
        mark.y1 - (t + b) / 2.0,
        size,
        f32::INFINITY,
        |path| target.fill_path(path, &paint, FillRule::Winding, transform, None),
    );
}

/// 读取截图并叠加 `marks`，得到某个区域的最终像素（预乘 RGBA）。
pub fn composited(
    region: RectPx,
    marks: &[Mark],
    source: &ShmImage,
    fonts: &mut Fonts,
) -> Option<Pixmap> {
    let mut base = Pixmap::new(region.width, region.height)?;
    let row = region.width as usize * 4;
    for (y, line) in base.data_mut().chunks_exact_mut(row).enumerate() {
        source.read_row(region.x, region.y + y as i32, line);
    }
    if marks
        .iter()
        .any(|mark| mark.bounds().and_then(|b| intersect(b, region)).is_some())
    {
        let mut layer = Pixmap::new(region.width, region.height)?;
        render_layer(
            &mut layer.as_mut(),
            (region.x, region.y),
            marks,
            source,
            fonts,
        );
        base.draw_pixmap(
            0,
            0,
            layer.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }
    Some(base)
}

fn blur_sigma(mark: &Mark) -> f32 {
    (mark.strength.clamp(1, 20) as f32 / 2.0).clamp(0.5, 10.0)
}

fn mosaic_block(mark: &Mark) -> i32 {
    mark.strength.clamp(4, 32) as i32
}

#[allow(clippy::too_many_arguments)]
fn draw_effect(
    target: &mut PixmapMut<'_>,
    origin: (i32, i32),
    area: RectPx,
    marks: &[Mark],
    index: usize,
    source: &ShmImage,
    fonts: &mut Fonts,
) {
    let mark = &marks[index];
    let image = RectPx::new(0, 0, source.width(), source.height());
    let Some(needed) = mark
        .bounds()
        .and_then(|b| intersect(b, area))
        .and_then(|b| intersect(b, image))
    else {
        return;
    };
    // 马赛克块对齐到整张截图的网格；模糊额外读取 3σ 邻域。相邻瓦片和导出
    // 条带因此得到完全相同的像素。
    let region = if mark.kind == BLUR {
        let margin = (blur_sigma(mark) * 3.0).ceil() as i32 + 3;
        RectPx::new(
            needed.x - margin,
            needed.y - margin,
            needed.width + margin as u32 * 2,
            needed.height + margin as u32 * 2,
        )
    } else {
        let block = mosaic_block(mark);
        let left = needed.x.div_euclid(block) * block;
        let top = needed.y.div_euclid(block) * block;
        let right = (needed.right() + block - 1).div_euclid(block) * block;
        let bottom = (needed.bottom() + block - 1).div_euclid(block) * block;
        RectPx::new(left, top, (right - left) as u32, (bottom - top) as u32)
    };
    let Some(region) = intersect(region, image) else {
        return;
    };
    let Some(mut local) = composited(region, &marks[..index], source, fonts) else {
        return;
    };
    if mark.kind == BLUR {
        gaussian(&mut local, blur_sigma(mark));
    } else {
        pixelate(&mut local, (region.x, region.y), mosaic_block(mark));
    }
    let width = mark.stroke_width();
    let Some((path, dot)) = brush_path(&mark.points, width) else {
        return;
    };
    let mut paint = Paint {
        shader: Pattern::new(
            local.as_ref(),
            SpreadMode::Pad,
            FilterQuality::Nearest,
            1.0,
            Transform::from_translate(region.x as f32, region.y as f32),
        ),
        anti_alias: true,
        ..Paint::default()
    };
    paint.blend_mode = BlendMode::Source;
    let transform = Transform::from_translate(-origin.0 as f32, -origin.1 as f32);
    if dot {
        target.fill_path(&path, &paint, FillRule::Winding, transform, None);
    } else {
        target.stroke_path(&path, &paint, &round_stroke(width), transform, None);
    }
}

fn pixelate(pixmap: &mut Pixmap, origin: (i32, i32), block: i32) {
    let (width, height) = (pixmap.width() as i32, pixmap.height() as i32);
    let data = pixmap.data_mut();
    // origin 已按 block 对齐（或位于截图边缘），局部网格即全局网格。
    let start_x = origin.0.rem_euclid(block);
    let start_y = origin.1.rem_euclid(block);
    let mut top = -start_y;
    while top < height {
        let mut left = -start_x;
        while left < width {
            let (x0, y0) = (left.max(0), top.max(0));
            let (x1, y1) = ((left + block).min(width), (top + block).min(height));
            let mut sum = [0u32; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * width + x) as usize * 4;
                    for channel in 0..4 {
                        sum[channel] += u32::from(data[i + channel]);
                    }
                }
            }
            let count = ((x1 - x0) * (y1 - y0)).max(1) as u32;
            let average = sum.map(|value| (value / count) as u8);
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * width + x) as usize * 4;
                    data[i..i + 4].copy_from_slice(&average);
                }
            }
            left += block;
        }
        top += block;
    }
}

/// 三次盒式模糊逼近高斯模糊，边缘按区域边界延伸。
fn gaussian(pixmap: &mut Pixmap, sigma: f32) {
    let (width, height) = (pixmap.width() as usize, pixmap.height() as usize);
    let ideal = (12.0 * sigma * sigma / 3.0 + 1.0).sqrt();
    let mut lower = ideal.floor() as usize;
    if lower.is_multiple_of(2) {
        lower = lower.saturating_sub(1);
    }
    let upper = lower + 2;
    let m = ((12.0 * sigma * sigma - 3.0 * (lower * lower) as f32 - 12.0 * lower as f32 - 9.0)
        / (-4.0 * lower as f32 - 4.0))
        .round() as usize;
    let data = pixmap.data_mut();
    let mut scratch = vec![0u8; width.max(height) * 4];
    for pass in 0..3 {
        let size = if pass < m { lower } else { upper };
        let radius = size / 2;
        if radius == 0 {
            continue;
        }
        for y in 0..height {
            box_line(data, y * width * 4, 4, width, radius, &mut scratch);
        }
        for x in 0..width {
            box_line(data, x * 4, width * 4, height, radius, &mut scratch);
        }
    }
}

fn box_line(
    data: &mut [u8],
    start: usize,
    step: usize,
    len: usize,
    radius: usize,
    scratch: &mut [u8],
) {
    if len == 0 {
        return;
    }
    let at = |i: isize| start + (i.clamp(0, len as isize - 1) as usize) * step;
    let window = (radius * 2 + 1) as u32;
    let mut sum = [0u32; 4];
    for i in -(radius as isize)..=(radius as isize) {
        let p = at(i);
        for c in 0..4 {
            sum[c] += u32::from(data[p + c]);
        }
    }
    for i in 0..len {
        for c in 0..4 {
            scratch[i * 4 + c] = ((sum[c] + window / 2) / window) as u8;
        }
        let add = at(i as isize + radius as isize + 1);
        let remove = at(i as isize - radius as isize);
        for c in 0..4 {
            sum[c] = sum[c] + u32::from(data[add + c]) - u32::from(data[remove + c]);
        }
    }
    for i in 0..len {
        let p = start + i * step;
        data[p..p + 4].copy_from_slice(&scratch[i * 4..i * 4 + 4]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pixels::{PixelSize, RgbaPixels};

    fn source(width: u32, height: u32) -> ShmImage {
        let mut bytes = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let v = if (x / 4 + y / 4) % 2 == 0 { 230 } else { 20 };
                bytes.extend([v, v, v, 255]);
            }
        }
        ShmImage::from_rgba(
            &RgbaPixels::from_rgba(PixelSize::new(width, height).unwrap(), bytes).unwrap(),
        )
        .unwrap()
    }

    fn layer(marks: &[Mark], src: &ShmImage, origin: (i32, i32), size: (u32, u32)) -> Pixmap {
        let mut pixmap = Pixmap::new(size.0, size.1).unwrap();
        render_layer(&mut pixmap.as_mut(), origin, marks, src, &mut Fonts::new());
        pixmap
    }

    #[test]
    fn pen_and_eraser_follow_history_order() {
        let src = source(64, 64);
        let mut pen = Mark::new(PEN, 4.0, 32.0, [255, 0, 0, 255], 6);
        pen.update(60.0, 32.0);
        let mut eraser = Mark::new(ERASER, 32.0, 10.0, [0, 0, 0, 255], 12);
        eraser.update(32.0, 54.0);
        let drawn = layer(&[pen.clone()], &src, (0, 0), (64, 64));
        assert!(drawn.pixel(10, 32).unwrap().alpha() == 255);
        let erased = layer(&[pen, eraser], &src, (0, 0), (64, 64));
        assert_eq!(erased.pixel(32, 32).unwrap().alpha(), 0);
        assert_eq!(erased.pixel(10, 32).unwrap().alpha(), 255);
    }

    #[test]
    fn tiles_and_full_layer_render_identical_effects() {
        let src = source(96, 64);
        let mut blur = Mark::new(BLUR, 10.0, 30.0, [0, 0, 0, 255], 20);
        blur.strength = 6;
        blur.update(86.0, 34.0);
        let mut mosaic = Mark::new(MOSAIC, 10.0, 10.0, [0, 0, 0, 255], 16);
        mosaic.update(80.0, 50.0);
        let marks = [blur, mosaic];
        let full = composited(RectPx::new(0, 0, 96, 64), &marks, &src, &mut Fonts::new()).unwrap();
        for (x, y) in [(0, 0), (40, 0), (0, 24), (40, 24)] {
            let tile =
                composited(RectPx::new(x, y, 56, 40), &marks, &src, &mut Fonts::new()).unwrap();
            let (mut mismatched, mut worst) = (0, 0);
            for ty in 0..40 {
                for tx in 0..56 {
                    let a = tile.pixel(tx, ty).unwrap();
                    let b = full.pixel(x as u32 + tx, y as u32 + ty).unwrap();
                    let diff = a
                        .red()
                        .abs_diff(b.red())
                        .max(a.green().abs_diff(b.green()))
                        .max(a.blue().abs_diff(b.blue()))
                        .max(a.alpha().abs_diff(b.alpha()));
                    mismatched += usize::from(diff > 2);
                    worst = worst.max(diff);
                }
            }
            // 被裁剪路径的抗锯齿边缘允许少量舍入差异；效果内容本身必须一致。
            assert!(mismatched * 100 < 56 * 40, "tile ({x},{y}) {mismatched}");
            assert!(worst <= 48, "tile ({x},{y}) worst {worst}");
        }
    }

    #[test]
    fn shapes_keep_their_interior_transparent() {
        let src = source(64, 64);
        let mut rect = Mark::new(RECTANGLE, 8.0, 8.0, [0, 0, 255, 255], 4);
        rect.update(56.0, 40.0);
        let mut circle = Mark::new(CIRCLE, 8.0, 8.0, [0, 255, 0, 255], 2);
        circle.update(40.0, 60.0);
        let pixmap = layer(&[rect, circle], &src, (0, 0), (64, 64));
        assert_eq!(pixmap.pixel(9, 20).unwrap().alpha(), 255);
        assert_eq!(pixmap.pixel(30, 30).unwrap().alpha(), 0);
        assert!(pixmap.pixel(24, 9).unwrap().green() > 0);
    }
}
