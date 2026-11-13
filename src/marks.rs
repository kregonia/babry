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
pub const LINE: u8 = 4;
pub const ARROW: u8 = 5;
pub const HIGHLIGHTER: u8 = 7;
pub const MOSAIC: u8 = 8;
pub const BLUR: u8 = 9;
pub const TEXT: u8 = 10;
pub const NUMBER: u8 = 11;
pub const ERASER: u8 = 12;
pub const SQUARE: u8 = 13;
pub const CIRCLE: u8 = 14;
/// 仅用于显示的选中框（不会进入标注历史或导出）：`x1..y2` 为未旋转的框，
/// 按 `angle` 绕框中心旋转，带四角缩放点和顶部旋转手柄。
pub const FRAME: u8 = 15;

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
    /// 绕 `center()` 的旋转角（弧度）。只有矩形、方形、圆形、文字和序号使用；
    /// 线条和笔迹旋转时直接改写坐标，角度保持为 0。
    pub angle: f32,
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
            angle: 0.0,
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

    /// 标注可能改变的物理像素范围（未裁剪到截图）；旋转后取四角的外接矩形。
    pub fn bounds(&self) -> Option<RectPx> {
        let raw = self.raw_bounds()?;
        if self.angle == 0.0 {
            return Some(raw);
        }
        let (cx, cy) = self.center();
        let (sin, cos) = self.angle.sin_cos();
        let (mut l, mut t, mut r, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in [
            (raw.x, raw.y),
            (raw.right(), raw.y),
            (raw.x, raw.bottom()),
            (raw.right(), raw.bottom()),
        ] {
            let (dx, dy) = (x as f32 - cx, y as f32 - cy);
            let (x, y) = (cx + dx * cos - dy * sin, cy + dx * sin + dy * cos);
            l = l.min(x);
            t = t.min(y);
            r = r.max(x);
            b = b.max(y);
        }
        let (x, y) = (l.floor() as i32 - 1, t.floor() as i32 - 1);
        let (r, b) = (r.ceil() as i32 + 1, b.ceil() as i32 + 1);
        Some(RectPx::new(
            x,
            y,
            (r - x).max(1) as u32,
            (b - y).max(1) as u32,
        ))
    }

    fn raw_bounds(&self) -> Option<RectPx> {
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
            // 选中框：外侧描边、角点和旋转手柄都在框外，`width` 为手柄外伸长度。
            FRAME => (
                self.x1,
                self.y1 - self.width as f32,
                self.x2,
                self.y2,
                self.font_size + 2.0,
            ),
            TEXT => (
                self.x1,
                self.y1,
                self.x1 + self.extent.0,
                self.y1 + self.extent.1,
                self.font_size * 0.25 + 2.0,
            ),
            LINE | ARROW => (
                self.x1.min(self.x2),
                self.y1.min(self.y2),
                self.x1.max(self.x2),
                self.y1.max(self.y2),
                if self.kind == ARROW {
                    arrow_head(self.width) + 2.0
                } else {
                    self.width.max(1) as f32 / 2.0 + 2.0
                },
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

impl Mark {
    /// 可被重新选中并拖动的标注：图形、线条、文字、序号和画笔笔迹。
    /// 橡皮擦与马赛克/模糊依赖下方像素，移动后语义不清，不参与选择。
    pub fn movable(&self) -> bool {
        !matches!(self.kind, ERASER | MOSAIC | BLUR | FRAME)
    }

    /// 整体平移标注，保留内容与样式。
    pub fn translate(&mut self, dx: f32, dy: f32) {
        self.x1 += dx;
        self.y1 += dy;
        self.x2 += dx;
        self.y2 += dy;
        for point in &mut self.points {
            point.0 += dx;
            point.1 += dy;
        }
    }

    /// 未旋转、不含线宽的几何框 `(左, 上, 右, 下)`；缩放和旋转都以它的中心为基准。
    pub fn geometry(&self) -> (f32, f32, f32, f32) {
        match self.kind {
            k if is_brush(k) => {
                let (mut l, mut t, mut r, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                for &(x, y) in &self.points {
                    l = l.min(x);
                    t = t.min(y);
                    r = r.max(x);
                    b = b.max(y);
                }
                if l > r {
                    (self.x1, self.y1, self.x1, self.y1)
                } else {
                    (l, t, r, b)
                }
            }
            NUMBER => {
                let radius = number_radius(self);
                (
                    self.x1 - radius,
                    self.y1 - radius,
                    self.x1 + radius,
                    self.y1 + radius,
                )
            }
            TEXT => (
                self.x1,
                self.y1,
                self.x1 + self.extent.0,
                self.y1 + self.extent.1,
            ),
            _ => {
                let end = self.shape_end();
                (
                    self.x1.min(end.0),
                    self.y1.min(end.1),
                    self.x1.max(end.0),
                    self.y1.max(end.1),
                )
            }
        }
    }

    pub fn center(&self) -> (f32, f32) {
        let (l, t, r, b) = self.geometry();
        ((l + r) / 2.0, (t + b) / 2.0)
    }

    /// 线条和笔迹的旋转直接改写坐标；其余标注记录角度，绘制时旋转。
    fn bakes_rotation(&self) -> bool {
        matches!(self.kind, LINE | ARROW) || is_brush(self.kind)
    }

    /// 以 `center` 为中心等比缩放。线宽保持不变，文字和序号的字号随之缩放。
    pub fn scale_about(&mut self, factor: f32, center: (f32, f32)) {
        let f = |v: f32, c: f32| c + (v - c) * factor;
        self.x1 = f(self.x1, center.0);
        self.y1 = f(self.y1, center.1);
        self.x2 = f(self.x2, center.0);
        self.y2 = f(self.y2, center.1);
        for point in &mut self.points {
            *point = (f(point.0, center.0), f(point.1, center.1));
        }
        if matches!(self.kind, TEXT | NUMBER) {
            self.font_size *= factor;
            self.extent = (self.extent.0 * factor, self.extent.1 * factor);
            if self.text_width.is_finite() {
                self.text_width *= factor;
            }
        }
    }

    /// 绕 `center` 旋转 `delta` 弧度。
    pub fn rotate_about(&mut self, delta: f32, center: (f32, f32)) {
        if !self.bakes_rotation() {
            // 角度绕自身中心生效；中心不同时先把自身中心转到新位置。
            let own = self.center();
            let moved = rotate_point(own, delta, center);
            self.translate(moved.0 - own.0, moved.1 - own.1);
            self.angle = normalize_angle(self.angle + delta);
            return;
        }
        let r = |p: (f32, f32)| rotate_point(p, delta, center);
        (self.x1, self.y1) = r((self.x1, self.y1));
        (self.x2, self.y2) = r((self.x2, self.y2));
        for point in &mut self.points {
            *point = r(*point);
        }
    }

    /// 绘制时附加的旋转（绕自身中心）。
    fn rotation(&self, transform: Transform) -> Transform {
        if self.angle == 0.0 {
            return transform;
        }
        let (cx, cy) = self.center();
        transform.pre_concat(Transform::from_rotate_at(self.angle.to_degrees(), cx, cy))
    }

    /// 物理坐标 `(x, y)` 是否落在标注上；`tolerance` 为额外的命中容差。
    /// 线条和笔迹按到路径的距离判断，图形、文字和序号按外接范围判断。
    pub fn hit(&self, x: f32, y: f32, tolerance: f32) -> bool {
        if !self.movable() {
            return false;
        }
        // 旋转过的标注：把指针反向转回标注自身的坐标系再判断。
        let (x, y) = if self.angle == 0.0 {
            (x, y)
        } else {
            rotate_point((x, y), -self.angle, self.center())
        };
        match self.kind {
            LINE | ARROW => {
                let reach = self.width.max(1) as f32 / 2.0 + tolerance;
                segment_distance((x, y), (self.x1, self.y1), (self.x2, self.y2)) <= reach
            }
            k if is_brush(k) => {
                let reach = self.stroke_width() / 2.0 + tolerance;
                match self.points.as_slice() {
                    [] => false,
                    [p] => (p.0 - x).hypot(p.1 - y) <= reach,
                    points => points
                        .windows(2)
                        .any(|w| segment_distance((x, y), w[0], w[1]) <= reach),
                }
            }
            _ => self.raw_bounds().is_some_and(|b| {
                let t = tolerance;
                x >= b.x as f32 - t
                    && y >= b.y as f32 - t
                    && x <= b.right() as f32 + t
                    && y <= b.bottom() as f32 + t
            }),
        }
    }
}

pub fn rotate_point(p: (f32, f32), angle: f32, center: (f32, f32)) -> (f32, f32) {
    let (sin, cos) = angle.sin_cos();
    let (dx, dy) = (p.0 - center.0, p.1 - center.1);
    (
        center.0 + dx * cos - dy * sin,
        center.1 + dx * sin + dy * cos,
    )
}

fn normalize_angle(angle: f32) -> f32 {
    let turn = std::f32::consts::TAU;
    let a = angle.rem_euclid(turn);
    if a > turn / 2.0 { a - turn } else { a }
}

fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx * dx + dy * dy;
    let t = if len <= f32::EPSILON {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len).clamp(0.0, 1.0)
    };
    (p.0 - a.0 - t * dx).hypot(p.1 - a.1 - t * dy)
}

/// 箭头头部长度：随线宽增长，细线也保持清晰可见。
fn arrow_head(width: u32) -> f32 {
    (width.max(1) as f32 * 4.0).max(14.0)
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
            RECTANGLE | SQUARE | CIRCLE => draw_shape(target, mark, mark.rotation(transform)),
            LINE | ARROW => draw_line(target, mark, transform),
            NUMBER => draw_number(target, mark, mark.rotation(transform), fonts),
            FRAME => draw_frame(target, mark, mark.rotation(transform)),
            TEXT => {
                let transform = mark.rotation(transform);
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

/// 选中框：白底蓝线的细框、四角方形缩放点、顶边中点向外的旋转手柄。
fn draw_frame(target: &mut PixmapMut<'_>, mark: &Mark, transform: Transform) {
    let (l, t, r, b) = (mark.x1, mark.y1, mark.x2, mark.y2);
    let half = mark.font_size;
    let unit = (half / 5.0).max(1.0);
    let blue = paint([10, 132, 255, 255]);
    let white = paint([255, 255, 255, 255]);
    let stroke = |width: f32| Stroke {
        width,
        ..Stroke::default()
    };
    let top_mid = ((l + r) / 2.0, t);
    let knob = (top_mid.0, t - mark.width as f32);
    let mut lines = PathBuilder::new();
    if let Some(rect) = Rect::from_ltrb(l, t, r.max(l + 1.0), b.max(t + 1.0)) {
        lines.push_rect(rect);
    }
    lines.move_to(top_mid.0, top_mid.1);
    lines.line_to(knob.0, knob.1);
    if let Some(path) = lines.finish() {
        target.stroke_path(&path, &white, &stroke(unit * 3.0), transform, None);
        target.stroke_path(&path, &blue, &stroke(unit * 1.5), transform, None);
    }
    let mut handles = PathBuilder::new();
    for (x, y) in [(l, t), (r, t), (l, b), (r, b)] {
        if let Some(rect) = Rect::from_xywh(x - half, y - half, half * 2.0, half * 2.0) {
            handles.push_rect(rect);
        }
    }
    handles.push_circle(knob.0, knob.1, half * 1.1);
    if let Some(path) = handles.finish() {
        target.fill_path(&path, &white, FillRule::Winding, transform, None);
        target.stroke_path(&path, &blue, &stroke(unit * 1.5), transform, None);
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

/// 直线圆头；箭头的线身止于箭头底边，头部为实心三角形。
fn draw_line(target: &mut PixmapMut<'_>, mark: &Mark, transform: Transform) {
    let (dx, dy) = (mark.x2 - mark.x1, mark.y2 - mark.y1);
    let length = dx.hypot(dy);
    if length < 1.0 {
        return;
    }
    let width = mark.width.max(1) as f32;
    let paint = paint(mark.color);
    let (ux, uy) = (dx / length, dy / length);
    let head = if mark.kind == ARROW {
        arrow_head(mark.width).min(length)
    } else {
        0.0
    };
    // 箭头头部较宽，线身缩回一点避免圆头从三角形两侧露出。
    let shaft = (length - head * 0.8).max(0.0);
    if shaft > 0.0 {
        let mut builder = PathBuilder::new();
        builder.move_to(mark.x1, mark.y1);
        builder.line_to(mark.x1 + ux * shaft, mark.y1 + uy * shaft);
        if let Some(path) = builder.finish() {
            let stroke = Stroke {
                width,
                line_cap: if head > 0.0 {
                    LineCap::Butt
                } else {
                    LineCap::Round
                },
                ..Stroke::default()
            };
            target.stroke_path(&path, &paint, &stroke, transform, None);
        }
    }
    if head > 0.0 {
        let half = (head * 0.5).max(width * 1.5);
        let (bx, by) = (mark.x2 - ux * head, mark.y2 - uy * head);
        let mut builder = PathBuilder::new();
        builder.move_to(mark.x2, mark.y2);
        builder.line_to(bx - uy * half, by + ux * half);
        builder.line_to(bx + uy * half, by - ux * half);
        builder.close();
        if let Some(path) = builder.finish() {
            target.fill_path(&path, &paint, FillRule::Winding, transform, None);
        }
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

    #[test]
    fn arrow_draws_shaft_and_filled_head() {
        let src = source(80, 40);
        let mut arrow = Mark::new(ARROW, 6.0, 20.0, [255, 0, 0, 255], 3);
        arrow.update(74.0, 20.0);
        let pixmap = layer(&[arrow.clone()], &src, (0, 0), (80, 40));
        assert_eq!(pixmap.pixel(30, 20).unwrap().alpha(), 255, "线身");
        // 头部比线身宽：距中线 4 像素处仍有颜色，线身同位置没有。
        assert!(pixmap.pixel(63, 24).unwrap().alpha() > 200, "箭头头部");
        assert_eq!(pixmap.pixel(30, 24).unwrap().alpha(), 0);
        let bounds = arrow.bounds().unwrap();
        assert!(bounds.y <= 20 - 7 && bounds.bottom() >= 20 + 7);
        let mut line = Mark::new(LINE, 6.0, 20.0, [0, 0, 255, 255], 3);
        line.update(74.0, 20.0);
        let pixmap = layer(&[line], &src, (0, 0), (80, 40));
        assert_eq!(pixmap.pixel(73, 20).unwrap().alpha(), 255);
        assert_eq!(pixmap.pixel(63, 24).unwrap().alpha(), 0);
    }

    #[test]
    fn hit_testing_follows_shape_and_translate_moves_everything() {
        let mut line = Mark::new(LINE, 0.0, 0.0, [0, 0, 0, 255], 4);
        line.update(100.0, 100.0);
        assert!(line.hit(50.0, 52.0, 2.0));
        assert!(!line.hit(50.0, 70.0, 2.0));
        let mut pen = Mark::new(PEN, 10.0, 10.0, [0, 0, 0, 255], 4);
        pen.update(60.0, 10.0);
        pen.update(60.0, 60.0);
        assert!(pen.hit(60.0, 35.0, 1.0));
        assert!(!pen.hit(30.0, 35.0, 1.0));
        let mut rect = Mark::new(RECTANGLE, 10.0, 10.0, [0, 0, 0, 255], 2);
        rect.update(40.0, 30.0);
        assert!(rect.hit(25.0, 20.0, 0.0), "图形内部也可以选中");
        let mut eraser = Mark::new(ERASER, 0.0, 0.0, [0, 0, 0, 255], 10);
        eraser.update(10.0, 10.0);
        assert!(!eraser.hit(5.0, 5.0, 2.0));
        pen.translate(5.0, -3.0);
        assert_eq!(pen.points, vec![(15.0, 7.0), (65.0, 7.0), (65.0, 57.0)]);
        assert_eq!((pen.x1, pen.y1, pen.x2, pen.y2), (15.0, 7.0, 65.0, 57.0));
    }

    #[test]
    fn rotated_rectangle_renders_hits_and_bounds_follow_angle() {
        let src = source(120, 120);
        // 80×20 的横条绕中心 (60, 60) 转 90° 后变成竖条。
        let mut rect = Mark::new(RECTANGLE, 20.0, 50.0, [255, 0, 0, 255], 4);
        rect.update(100.0, 70.0);
        rect.rotate_about(std::f32::consts::FRAC_PI_2, rect.center());
        let drawn = layer(&[rect.clone()], &src, (0, 0), (120, 120));
        // 竖条的上边缘在 y≈21，原来横条的左边缘 x≈21 处不再有像素。
        assert_eq!(drawn.pixel(60, 21).unwrap().alpha(), 255);
        assert_eq!(drawn.pixel(21, 60).unwrap().alpha(), 0);
        assert!(rect.hit(60.0, 22.0, 2.0));
        assert!(!rect.hit(22.0, 60.0, 2.0));
        let b = rect.bounds().unwrap();
        assert!(
            b.x >= 45 && b.right() <= 75 && b.y <= 20 && b.bottom() >= 100,
            "{b:?}"
        );
    }

    #[test]
    fn scaling_keeps_aspect_and_lines_rotate_their_endpoints() {
        let mut rect = Mark::new(RECTANGLE, 10.0, 10.0, [255, 0, 0, 255], 4);
        rect.update(50.0, 30.0);
        rect.scale_about(2.0, rect.center());
        assert_eq!(rect.geometry(), (-10.0, 0.0, 70.0, 40.0));
        let mut text = Mark::new(TEXT, 0.0, 0.0, [0; 4], 1);
        text.font_size = 20.0;
        text.extent = (40.0, 25.0);
        text.scale_about(0.5, (0.0, 0.0));
        assert_eq!((text.font_size, text.extent), (10.0, (20.0, 12.5)));
        let mut arrow = Mark::new(ARROW, 0.0, 0.0, [255, 0, 0, 255], 4);
        arrow.update(100.0, 0.0);
        arrow.rotate_about(std::f32::consts::FRAC_PI_2, arrow.center());
        assert_eq!(arrow.angle, 0.0, "线条直接改写坐标");
        assert!((arrow.x1 - 50.0).abs() < 1e-3 && (arrow.y1 + 50.0).abs() < 1e-3);
        assert!((arrow.x2 - 50.0).abs() < 1e-3 && (arrow.y2 - 50.0).abs() < 1e-3);
    }
}
