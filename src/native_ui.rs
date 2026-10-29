//! 原生编辑器的逻辑像素布局。绘制和命中测试共享全部控件几何。
use crate::{core::RectPx, font::Fonts};
use std::sync::OnceLock;
use svgtypes::{SimplePathSegment, SimplifyingPathParser};
use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LineJoin, LinearGradient, Paint, Path, PathBuilder,
    PixmapMut, Point, SpreadMode, Stroke, Transform,
};

#[path = "native_icons.rs"]
mod icons;

type ColorValue = [u8; 4];
const WHITE: ColorValue = [255, 255, 255, 255];
const INK: ColorValue = [201, 212, 227, 255];
pub const SWATCHES: [ColorValue; 26] = [
    [255, 59, 48, 255],
    [255, 149, 0, 255],
    [255, 204, 0, 255],
    [52, 199, 89, 255],
    [0, 199, 190, 255],
    [48, 176, 199, 255],
    [10, 132, 255, 255],
    [88, 86, 214, 255],
    [175, 82, 222, 255],
    [255, 45, 85, 255],
    [255, 255, 255, 255],
    [242, 242, 247, 255],
    [209, 209, 214, 255],
    [174, 174, 178, 255],
    [142, 142, 147, 255],
    [99, 99, 102, 255],
    [72, 72, 74, 255],
    [28, 28, 30, 255],
    [139, 94, 60, 255],
    [100, 210, 255, 255],
    [255, 107, 107, 255],
    [255, 209, 102, 255],
    [6, 214, 160, 255],
    [17, 138, 178, 255],
    [131, 56, 236, 255],
    [251, 86, 7, 255],
];
pub const QUICK_COLORS: [ColorValue; 5] = [
    [255, 59, 48, 255],
    [255, 214, 10, 255],
    [10, 132, 255, 255],
    [52, 199, 89, 255],
    WHITE,
];

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BoxF {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}
impl BoxF {
    pub fn contains(self, p: (f64, f64)) -> bool {
        p.0 >= f64::from(self.x)
            && p.0 < f64::from(self.x + self.width)
            && p.1 >= f64::from(self.y)
            && p.1 < f64::from(self.y + self.height)
    }
    fn inset(self, padding: f32) -> Self {
        Self {
            x: self.x + padding,
            y: self.y + padding,
            width: self.width - padding * 2.0,
            height: self.height - padding * 2.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Menu {
    Shapes,
    Effects,
    Colors,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Tool(u8),
    Open(Menu),
    Shape(u8),
    Effect(u8),
    Color(ColorValue),
    Undo,
    Redo,
    Pin,
    Save,
    Cancel,
    Copy,
    More,
    Grip,
    Width,
    Number,
    Strength,
    StrengthNumber,
    Blank,
    Dismiss,
}

pub struct EditorUi {
    pub editing: bool,
    pub tool: u8,
    pub shape: u8,
    pub effect: u8,
    pub color: ColorValue,
    pub width: u32,
    pub font_size: u32,
    pub strength: u32,
    pub next_number: u32,
    pub toolbar_position: Option<(f32, f32)>,
    pub menu: Option<Menu>,
    pub pointer: (f64, f64),
    pub pressed: Option<Action>,
    pub number_input: Option<(bool, String)>,
    pub error: Option<String>,
    /// 控件对齐的逻辑网格步长；使控件边缘落在整数物理像素上。
    pub grid: f32,
}
impl Default for EditorUi {
    fn default() -> Self {
        Self {
            editing: false,
            tool: 0,
            shape: 2,
            effect: 0,
            color: SWATCHES[0],
            width: 4,
            font_size: 24,
            strength: 8,
            next_number: 1,
            toolbar_position: None,
            menu: None,
            pointer: (-1.0, -1.0),
            pressed: None,
            number_input: None,
            error: None,
            grid: 1.0,
        }
    }
}
impl EditorUi {
    pub fn value(&self) -> u32 {
        if matches!(self.tool, 10 | 11) {
            self.font_size
        } else {
            self.width
        }
    }
    pub fn set_value(&mut self, value: u32) {
        if matches!(self.tool, 10 | 11) {
            self.font_size = value.clamp(12, 72);
        } else {
            self.width = value.clamp(1, 64);
        }
    }
    pub fn toolbar(&self, screen: (u32, u32), selection: BoxF) -> BoxF {
        let width = 600.0_f32.min((screen.0 as f32 - 16.0).max(1.0));
        let height = if self.tool == 0 {
            50.0
        } else if self.tool == 8 && self.effect == 1 {
            136.0
        } else {
            98.0
        };
        let (x, y) = self
            .toolbar_position
            .unwrap_or((selection.x, selection.y + selection.height + 12.0));
        BoxF {
            x: snap(x.clamp(0.0, (screen.0 as f32 - width).max(0.0)), self.grid),
            y: snap(
                y.clamp(0.0, (screen.1 as f32 - height - 8.0).max(0.0)),
                self.grid,
            ),
            width,
            height,
        }
    }
    pub fn menu_box(&self, screen: (u32, u32), selection: BoxF, menu: Menu) -> BoxF {
        let toolbar = self.toolbar(screen, selection);
        let (anchor_x, anchor_y, width, height) = match menu {
            Menu::Shapes => (toolbar.x + 70.0, toolbar.y + 8.0, 160.0, 122.0),
            Menu::Effects => (toolbar.x + 230.0, toolbar.y + 8.0, 160.0, 86.0),
            Menu::Colors => (toolbar.x + 283.0, toolbar.y + 54.0, 278.0, 174.0),
        };
        let y = if anchor_y >= height + 8.0 {
            anchor_y - height - 8.0
        } else {
            anchor_y + 42.0
        };
        BoxF {
            x: snap(
                (anchor_x - 34.0).clamp(8.0, (screen.0 as f32 - width - 8.0).max(8.0)),
                self.grid,
            ),
            y: snap(
                y.clamp(8.0, (screen.1 as f32 - height - 8.0).max(8.0)),
                self.grid,
            ),
            width,
            height,
        }
    }
    pub fn hit(&self, screen: (u32, u32), selection: BoxF, p: (f64, f64)) -> Option<Action> {
        if !self.editing {
            return None;
        }
        if let Some(menu) = self.menu {
            let bounds = self.menu_box(screen, selection, menu);
            if !bounds.contains(p) {
                return Some(Action::Dismiss);
            }
            match menu {
                Menu::Colors => {
                    for (index, _) in SWATCHES.iter().enumerate() {
                        if swatch_box(bounds, index).contains(p) {
                            return Some(Action::Color(SWATCHES[index]));
                        }
                    }
                }
                _ => {
                    let items: &[Action] = if menu == Menu::Shapes {
                        &[Action::Shape(2), Action::Shape(14), Action::Shape(13)]
                    } else {
                        &[Action::Effect(0), Action::Effect(1)]
                    };
                    for (index, action) in items.iter().enumerate() {
                        if menu_item_box(bounds, index).contains(p) {
                            return Some(*action);
                        }
                    }
                }
            }
            return Some(Action::Blank);
        }
        let b = self.toolbar(screen, selection);
        if !b.contains(p) {
            return None;
        }
        for (bounds, action) in self.controls(b) {
            if bounds.contains(p) {
                return Some(action);
            }
        }
        Some(Action::Blank)
    }
    fn controls(&self, b: BoxF) -> Vec<(BoxF, Action)> {
        let mut controls = vec![(
            BoxF {
                x: b.x + 8.0,
                y: b.y + 8.0,
                width: 26.0,
                height: 34.0,
            },
            Action::Grip,
        )];
        for &(x, action, _) in BUTTONS {
            controls.push((
                BoxF {
                    x: b.x + 36.0 + x,
                    y: b.y + 8.0,
                    width: 34.0,
                    height: 34.0,
                },
                action,
            ));
        }
        controls.push((
            BoxF {
                x: b.x + 70.0,
                y: b.y + 8.0,
                width: 16.0,
                height: 34.0,
            },
            Action::Open(Menu::Shapes),
        ));
        controls.push((
            BoxF {
                x: b.x + 230.0,
                y: b.y + 8.0,
                width: 16.0,
                height: 34.0,
            },
            Action::Open(Menu::Effects),
        ));
        if self.tool != 0 {
            controls.extend([
                (
                    BoxF {
                        x: b.x + 49.0,
                        y: b.y + 54.0,
                        width: 132.0,
                        height: 28.0,
                    },
                    Action::Width,
                ),
                (
                    BoxF {
                        x: b.x + 188.0,
                        y: b.y + 54.0,
                        width: 48.0,
                        height: 28.0,
                    },
                    Action::Number,
                ),
                (
                    BoxF {
                        x: b.x + 283.0,
                        y: b.y + 54.0,
                        width: 24.0,
                        height: 28.0,
                    },
                    Action::Open(Menu::Colors),
                ),
            ]);
            for (i, color) in QUICK_COLORS.into_iter().enumerate() {
                controls.push((
                    BoxF {
                        x: b.x + 314.0 + i as f32 * 25.0,
                        y: b.y + 54.0,
                        width: 24.0,
                        height: 28.0,
                    },
                    Action::Color(color),
                ));
            }
            if self.tool == 8 && self.effect == 1 {
                controls.push((
                    BoxF {
                        x: b.x + 65.0,
                        y: b.y + 96.0,
                        width: b.width - 140.0,
                        height: 28.0,
                    },
                    Action::Strength,
                ));
                controls.push((
                    BoxF {
                        x: b.x + b.width - 60.0,
                        y: b.y + 96.0,
                        width: 48.0,
                        height: 28.0,
                    },
                    Action::StrengthNumber,
                ));
            }
        }
        controls
    }
    pub fn slider(&mut self, screen: (u32, u32), selection: BoxF, p: (f64, f64), strength: bool) {
        let b = self.toolbar(screen, selection);
        let (x, w, min, max) = if strength {
            (b.x + 65.0, b.width - 140.0, 1.0, 20.0)
        } else if matches!(self.tool, 10 | 11) {
            (b.x + 49.0, 132.0, 12.0, 72.0)
        } else {
            (b.x + 49.0, 132.0, 1.0, 64.0)
        };
        let value = (min + ((p.0 as f32 - x) / w).clamp(0.0, 1.0) * (max - min)).round() as u32;
        if strength {
            self.strength = value;
        } else {
            self.set_value(value);
        }
    }
}
const BUTTONS: &[(f32, Action, &str)] = &[
    (0.0, Action::Tool(2), "shape-rectangle"),
    (52.0, Action::Tool(1), "pen"),
    (88.0, Action::Tool(10), "mark"),
    (124.0, Action::Tool(11), "number"),
    (160.0, Action::Tool(8), "effects"),
    (212.0, Action::Tool(12), "eraser"),
    (248.0, Action::Tool(7), "highlight"),
    (295.0, Action::Undo, "undo"),
    (331.0, Action::Redo, "redo"),
    (378.0, Action::Pin, "pin"),
    (414.0, Action::Save, "save"),
    (450.0, Action::Cancel, "cancle"),
    (486.0, Action::Copy, "copy"),
    (522.0, Action::More, "more"),
];
fn swatch_box(b: BoxF, index: usize) -> BoxF {
    let (x, y, w, h) = if index < 20 {
        (
            8.0 + (index % 10) as f32 * 26.0,
            36.0 + (index / 10) as f32 * 34.0,
            24.0,
            25.0,
        )
    } else {
        (76.0 + (index - 20) as f32 * 31.0, 112.0, 28.0, 34.0)
    };
    BoxF {
        x: b.x + x,
        y: b.y + y,
        width: w,
        height: h,
    }
}
fn menu_item_box(b: BoxF, index: usize) -> BoxF {
    BoxF {
        x: b.x + 7.0,
        y: b.y + 7.0 + index as f32 * 36.0,
        width: b.width - 14.0,
        height: 36.0,
    }
}
pub fn logical_selection(rect: RectPx, physical: (u32, u32), logical: (u32, u32)) -> BoxF {
    let sx = logical.0 as f32 / physical.0.max(1) as f32;
    let sy = logical.1 as f32 / physical.1.max(1) as f32;
    BoxF {
        x: rect.x as f32 * sx,
        y: rect.y as f32 * sy,
        width: rect.width as f32 * sx,
        height: rect.height as f32 * sy,
    }
}

// 图标只解析一次；使用旧版 SVG 的所有路径、圆和圆角矩形。
enum IconShape {
    Path(&'static str, f32),
    Circle(f32, f32, f32, f32),
    Rect(f32, f32, f32, f32, f32, f32),
}
struct Icon {
    name: &'static str,
    paths: Vec<(Path, f32)>,
}
impl Icon {
    fn new(name: &'static str, shapes: &[IconShape]) -> Self {
        let paths = shapes
            .iter()
            .filter_map(|shape| match *shape {
                IconShape::Path(data, width) => parse_path(data).map(|p| (p, width)),
                IconShape::Circle(x, y, r, width) => {
                    let mut b = PathBuilder::new();
                    b.push_circle(x, y, r);
                    b.finish().map(|p| (p, width))
                }
                IconShape::Rect(x, y, w, h, r, width) => rounded(
                    BoxF {
                        x,
                        y,
                        width: w,
                        height: h,
                    },
                    r,
                )
                .map(|p| (p, width)),
            })
            .collect();
        Self { name, paths }
    }
}
fn parse_path(data: &str) -> Option<Path> {
    let mut b = PathBuilder::new();
    for item in SimplifyingPathParser::from(data) {
        match item.ok()? {
            SimplePathSegment::MoveTo { x, y } => b.move_to(x as f32, y as f32),
            SimplePathSegment::LineTo { x, y } => b.line_to(x as f32, y as f32),
            SimplePathSegment::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => b.cubic_to(
                x1 as f32, y1 as f32, x2 as f32, y2 as f32, x as f32, y as f32,
            ),
            SimplePathSegment::Quadratic { x1, y1, x, y } => {
                b.quad_to(x1 as f32, y1 as f32, x as f32, y as f32)
            }
            SimplePathSegment::ClosePath => b.close(),
        }
    }
    b.finish()
}
fn rounded(b: BoxF, r: f32) -> Option<Path> {
    if b.width <= 0.0 || b.height <= 0.0 {
        return None;
    }
    let r = r.min(b.width / 2.0).min(b.height / 2.0).max(0.0);
    let k = r * 0.552_284_8;
    let (x, y, right, bottom) = (b.x, b.y, b.x + b.width, b.y + b.height);
    let mut p = PathBuilder::new();
    p.move_to(x + r, y);
    p.line_to(right - r, y);
    p.cubic_to(right - r + k, y, right, y + r - k, right, y + r);
    p.line_to(right, bottom - r);
    p.cubic_to(
        right,
        bottom - r + k,
        right - r + k,
        bottom,
        right - r,
        bottom,
    );
    p.line_to(x + r, bottom);
    p.cubic_to(x + r - k, bottom, x, bottom - r + k, x, bottom - r);
    p.line_to(x, y + r);
    p.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    p.close();
    p.finish()
}
fn paint(color: ColorValue) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(color[0], color[1], color[2], color[3]);
    p
}
struct Painter<'a> {
    pm: PixmapMut<'a>,
    transform: Transform,
}
impl Painter<'_> {
    fn fill(&mut self, path: &Path, p: &Paint<'_>) {
        self.pm
            .fill_path(path, p, FillRule::Winding, self.transform, None);
    }
    fn rect(&mut self, b: BoxF, r: f32, color: ColorValue) {
        if let Some(path) = rounded(b, r) {
            self.fill(&path, &paint(color));
        }
    }
    fn border(&mut self, b: BoxF, r: f32, color: ColorValue, width: f32) {
        if let Some(path) = rounded(b, r) {
            let stroke = Stroke {
                width,
                ..Stroke::default()
            };
            self.pm
                .stroke_path(&path, &paint(color), &stroke, self.transform, None);
        }
    }
    fn gradient(&mut self, b: BoxF, r: f32, colors: &[ColorValue]) {
        let Some(path) = rounded(b, r) else {
            return;
        };
        let Some(shader) = LinearGradient::new(
            Point::from_xy(b.x, b.y),
            Point::from_xy(b.x + b.width, b.y + b.height),
            colors
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    GradientStop::new(
                        i as f32 / (colors.len() - 1) as f32,
                        Color::from_rgba8(c[0], c[1], c[2], c[3]),
                    )
                })
                .collect(),
            SpreadMode::Pad,
            Transform::identity(),
        ) else {
            return;
        };
        self.fill(
            &path,
            &Paint {
                shader,
                ..Paint::default()
            },
        );
    }
    fn line(&mut self, a: (f32, f32), b: (f32, f32), color: ColorValue, width: f32) {
        let mut p = PathBuilder::new();
        p.move_to(a.0, a.1);
        p.line_to(b.0, b.1);
        if let Some(path) = p.finish() {
            self.pm.stroke_path(
                &path,
                &paint(color),
                &Stroke {
                    width,
                    line_cap: LineCap::Round,
                    ..Stroke::default()
                },
                self.transform,
                None,
            );
        }
    }
    fn text(
        &mut self,
        fonts: &mut Fonts,
        value: &str,
        x: f32,
        y: f32,
        size: f32,
        color: ColorValue,
    ) {
        let p = paint(color);
        fonts.text(value, x, y, size, 1000.0, |path| self.fill(path, &p));
    }
    fn icon(&mut self, name: &str, b: BoxF, color: ColorValue) {
        static ICONS: OnceLock<Vec<Icon>> = OnceLock::new();
        let Some(icon) = ICONS
            .get_or_init(icons::definitions)
            .iter()
            .find(|icon| icon.name == name)
        else {
            return;
        };
        let local = Transform::from_scale(b.width / 24.0, b.height / 24.0)
            .post_translate(b.x, b.y)
            .post_concat(self.transform);
        for (path, width) in &icon.paths {
            if *width > 0.0 {
                self.pm.stroke_path(
                    path,
                    &paint(color),
                    &Stroke {
                        width: *width,
                        line_cap: LineCap::Round,
                        line_join: LineJoin::Round,
                        ..Stroke::default()
                    },
                    local,
                    None,
                );
            } else {
                self.pm
                    .fill_path(path, &paint(color), FillRule::Winding, local, None);
            }
        }
    }
    fn shadow(&mut self, b: BoxF, r: f32) {
        for i in (1..=8).rev() {
            let spread = i as f32;
            self.rect(
                BoxF {
                    x: b.x - spread,
                    y: b.y + 4.0 - spread,
                    width: b.width + spread * 2.0,
                    height: b.height + spread * 2.0,
                },
                r + spread,
                [0, 0, 0, 7],
            );
        }
    }
    fn panel(&mut self, b: BoxF, r: f32) {
        self.shadow(b, r);
        self.gradient(b, r, &[[39, 49, 64, 245], [18, 25, 37, 245]]);
        self.border(b.inset(0.5), r, [255, 255, 255, 40], 1.0);
    }
    fn swatch(&mut self, b: BoxF, color: ColorValue, selected: bool, hover: bool) {
        if hover {
            self.rect(b, 6.0, [255, 255, 255, 18]);
        }
        let dot = BoxF {
            x: b.x + (b.width - 16.0) / 2.0,
            y: b.y + (b.height - 16.0) / 2.0,
            width: 16.0,
            height: 16.0,
        };
        self.rect(dot, 8.0, color);
        self.border(
            dot,
            8.0,
            if selected { WHITE } else { [255, 255, 255, 65] },
            if selected { 2.0 } else { 0.7 },
        );
    }
}

/// 输入与输出均为预乘 RGBA；调用方提交到 Wayland 前只交换 R/B 通道。
/// 绘制平面的变换：平面左上角位于逻辑坐标 `origin`，按输出缩放映射到物理像素。
pub fn plane_transform(origin: (f32, f32), scale: f32) -> Transform {
    Transform::from_translate(-origin.0, -origin.1).post_scale(scale, scale)
}

fn snap(value: f32, grid: f32) -> f32 {
    if grid > 1.0 {
        (value / grid).round() * grid
    } else {
        value.round()
    }
}

/// 阴影超出控件边界的逻辑像素数；平面需包含这部分透明边距。
pub const SHADOW_MARGIN: f32 = 12.0;

/// 选区尺寸标签的位置。
pub fn label_box(s: BoxF, grid: f32) -> BoxF {
    BoxF {
        x: snap(s.x.max(8.0), grid),
        y: snap((s.y - 30.0).max(8.0), grid),
        width: 116.0,
        height: 24.0,
    }
}

/// 错误提示条的位置。
pub fn error_box(screen: (u32, u32), grid: f32) -> BoxF {
    BoxF {
        x: snap((screen.0 as f32 - 560.0).max(0.0) / 2.0, grid),
        y: 24.0,
        width: 560.0_f32.min(screen.0 as f32),
        height: 48.0,
    }
}

/// 八个选区手柄：四角 8×8，四边中点 10×8 / 8×10，圆角 4。
pub fn handle_boxes(s: BoxF) -> [BoxF; 8] {
    let (right, bottom) = (s.x + s.width, s.y + s.height);
    [
        (s.x, s.y, 8.0, 8.0),
        (right, s.y, 8.0, 8.0),
        (right, bottom, 8.0, 8.0),
        (s.x, bottom, 8.0, 8.0),
        (s.x + s.width / 2.0, s.y, 10.0, 8.0),
        (right, s.y + s.height / 2.0, 8.0, 10.0),
        (s.x + s.width / 2.0, bottom, 10.0, 8.0),
        (s.x, s.y + s.height / 2.0, 8.0, 10.0),
    ]
    .map(|(x, y, w, h)| BoxF {
        x: x - w / 2.0,
        y: y - h / 2.0,
        width: w,
        height: h,
    })
}

fn painter<'a>(
    bytes: &'a mut [u8],
    size: (u32, u32),
    transform: Transform,
) -> Result<Painter<'a>, String> {
    let pm = PixmapMut::from_bytes(bytes, size.0, size.1).ok_or("绘制平面尺寸无效")?;
    Ok(Painter { pm, transform })
}

/// 绘制一个手柄精灵；`b` 为手柄在平面中的逻辑位置。
pub fn draw_handle(
    bytes: &mut [u8],
    size: (u32, u32),
    transform: Transform,
    b: BoxF,
) -> Result<(), String> {
    let mut p = painter(bytes, size, transform)?;
    p.rect(b, 4.0, WHITE);
    Ok(())
}

/// 绘制选区尺寸标签。
pub fn draw_label(
    bytes: &mut [u8],
    size: (u32, u32),
    transform: Transform,
    b: BoxF,
    text: &str,
    fonts: &mut Fonts,
) -> Result<(), String> {
    let mut p = painter(bytes, size, transform)?;
    p.rect(b, 4.0, [32, 36, 43, 221]);
    p.text(fonts, text, b.x + 10.0, b.y + 4.0, 12.0, WHITE);
    Ok(())
}

/// 绘制错误提示条。
pub fn draw_error(
    bytes: &mut [u8],
    size: (u32, u32),
    transform: Transform,
    b: BoxF,
    error: &str,
    fonts: &mut Fonts,
) -> Result<(), String> {
    let mut p = painter(bytes, size, transform)?;
    p.rect(b, 9.0, [78, 28, 38, 250]);
    p.text(fonts, error, b.x + 12.0, b.y + 12.0, 13.0, WHITE);
    Ok(())
}

/// 绘制工具栏（含阴影）。`logical` 为输出逻辑尺寸，`s` 为逻辑选区。
pub fn draw_toolbar(
    bytes: &mut [u8],
    size: (u32, u32),
    transform: Transform,
    logical: (u32, u32),
    s: BoxF,
    ui: &EditorUi,
    fonts: &mut Fonts,
) -> Result<(), String> {
    let mut p = painter(bytes, size, transform)?;
    let b = ui.toolbar(logical, s);
    p.panel(b, 13.0);
    let hovered = ui.hit(logical, s, ui.pointer);
    for (control, action) in ui.controls(b) {
        if matches!(
            action,
            Action::Tool(_)
                | Action::Open(_)
                | Action::Undo
                | Action::Redo
                | Action::Pin
                | Action::Save
                | Action::Cancel
                | Action::Copy
                | Action::More
                | Action::Grip
        ) {
            let selected = match action {
                Action::Tool(2) => matches!(ui.tool, 2 | 13 | 14),
                Action::Tool(t) => ui.tool == t,
                _ => false,
            };
            if selected {
                p.gradient(control, 9.0, &[[115, 85, 174, 255], [67, 107, 176, 255]]);
                p.border(control.inset(0.5), 9.0, [196, 181, 253, 136], 1.0);
            } else if hovered == Some(action) {
                p.rect(
                    control,
                    9.0,
                    [
                        255,
                        255,
                        255,
                        if ui.pressed == Some(action) { 36 } else { 18 },
                    ],
                );
            }
        }
    }
    p.icon(
        "grip",
        BoxF {
            x: b.x + 12.0,
            y: b.y + 14.0,
            width: 17.0,
            height: 21.0,
        },
        [146, 161, 183, 255],
    );
    for &(x, action, default_icon) in BUTTONS {
        let name = match action {
            Action::Tool(2) => match ui.shape {
                14 => "shape-circle",
                13 => "shape-square",
                _ => default_icon,
            },
            Action::Tool(8) if ui.effect == 1 => "blur",
            _ => default_icon,
        };
        let color = if action == Action::Cancel {
            [255, 153, 153, 255]
        } else if hovered == Some(action) {
            WHITE
        } else {
            INK
        };
        p.icon(
            name,
            BoxF {
                x: b.x + 36.0 + x + 8.0,
                y: b.y + 16.0,
                width: 18.0,
                height: 18.0,
            },
            color,
        );
    }
    for x in [70.0, 230.0] {
        p.icon(
            "chevron",
            BoxF {
                x: b.x + x + 1.0,
                y: b.y + 19.0,
                width: 14.0,
                height: 14.0,
            },
            INK,
        );
    }
    for x in [324.0, 407.0] {
        p.line(
            (b.x + x, b.y + 14.0),
            (b.x + x, b.y + 36.0),
            [255, 255, 255, 26],
            1.0,
        );
    }
    if ui.tool != 0 {
        let y = b.y + 54.0;
        p.text(
            fonts,
            if matches!(ui.tool, 10 | 11) {
                "字号"
            } else {
                "粗细"
            },
            b.x + 12.0,
            y + 5.0,
            12.0,
            [170, 185, 206, 255],
        );
        let (min, max) = if matches!(ui.tool, 10 | 11) {
            (12.0, 72.0)
        } else {
            (1.0, 64.0)
        };
        draw_slider(
            &mut p,
            BoxF {
                x: b.x + 49.0,
                y,
                width: 132.0,
                height: 28.0,
            },
            ui.value() as f32,
            min,
            max,
        );
        let value = ui
            .number_input
            .as_ref()
            .filter(|(strength, _)| !*strength)
            .map(|(_, s)| s.clone())
            .unwrap_or_else(|| ui.value().to_string());
        number(
            &mut p,
            fonts,
            BoxF {
                x: b.x + 188.0,
                y,
                width: 48.0,
                height: 28.0,
            },
            &value,
            ui.number_input.as_ref().is_some_and(|(s, _)| !*s),
        );
        p.text(
            fonts,
            "px",
            b.x + 243.0,
            y + 6.0,
            11.0,
            [117, 134, 159, 255],
        );
        p.line(
            (b.x + 271.0, y + 3.0),
            (b.x + 271.0, y + 25.0),
            [255, 255, 255, 26],
            1.0,
        );
        let rainbow = BoxF {
            x: b.x + 285.0,
            y: y + 4.0,
            width: 20.0,
            height: 20.0,
        };
        p.gradient(
            rainbow,
            10.0,
            &[
                SWATCHES[0],
                SWATCHES[1],
                SWATCHES[2],
                SWATCHES[3],
                SWATCHES[6],
                SWATCHES[7],
                SWATCHES[8],
            ],
        );
        p.border(rainbow, 10.0, [255, 255, 255, 185], 1.3);
        for (i, color) in QUICK_COLORS.into_iter().enumerate() {
            let boxf = BoxF {
                x: b.x + 314.0 + i as f32 * 25.0,
                y,
                width: 24.0,
                height: 28.0,
            };
            p.swatch(boxf, color, color == ui.color, boxf.contains(ui.pointer));
        }
        let label = match ui.tool {
            1 => "画笔".into(),
            2 => "矩形".into(),
            7 => "荧光笔".into(),
            8 if ui.effect == 1 => "高斯模糊".into(),
            8 => "马赛克".into(),
            10 => "文字".into(),
            11 => format!("下一个：{}", ui.next_number),
            12 => "橡皮擦".into(),
            13 => "方形".into(),
            14 => "圆形".into(),
            _ => String::new(),
        };
        p.text(
            fonts,
            &label,
            b.x + 492.0,
            y + 6.0,
            11.0,
            [167, 185, 211, 255],
        );
        if ui.tool == 8 && ui.effect == 1 {
            let y = b.y + 96.0;
            p.text(
                fonts,
                "模糊度",
                b.x + 12.0,
                y + 5.0,
                12.0,
                [170, 185, 206, 255],
            );
            draw_slider(
                &mut p,
                BoxF {
                    x: b.x + 65.0,
                    y,
                    width: b.width - 140.0,
                    height: 28.0,
                },
                ui.strength as f32,
                1.0,
                20.0,
            );
            let value = ui
                .number_input
                .as_ref()
                .filter(|(strength, _)| *strength)
                .map(|(_, s)| s.clone())
                .unwrap_or_else(|| ui.strength.to_string());
            number(
                &mut p,
                fonts,
                BoxF {
                    x: b.x + b.width - 60.0,
                    y,
                    width: 48.0,
                    height: 28.0,
                },
                &value,
                ui.number_input.as_ref().is_some_and(|(s, _)| *s),
            );
        }
    }
    Ok(())
}

/// 绘制当前打开的下拉菜单（含阴影）。
pub fn draw_menu(
    bytes: &mut [u8],
    size: (u32, u32),
    transform: Transform,
    logical: (u32, u32),
    s: BoxF,
    ui: &EditorUi,
    fonts: &mut Fonts,
) -> Result<(), String> {
    let Some(menu) = ui.menu else {
        return Ok(());
    };
    let mut p = painter(bytes, size, transform)?;
    let m = ui.menu_box(logical, s, menu);
    p.shadow(m, 10.0);
    p.rect(m, 10.0, [29, 39, 55, 255]);
    p.border(m.inset(0.5), 10.0, [255, 255, 255, 48], 1.0);
    match menu {
        Menu::Colors => {
            p.text(
                fonts,
                "选择颜色",
                m.x + 12.0,
                m.y + 8.0,
                12.0,
                [232, 239, 251, 255],
            );
            p.text(
                fonts,
                "当前",
                m.x + 180.0,
                m.y + 8.0,
                11.0,
                [143, 161, 187, 255],
            );
            p.rect(
                BoxF {
                    x: m.x + 218.0,
                    y: m.y + 7.0,
                    width: 42.0,
                    height: 20.0,
                },
                5.0,
                ui.color,
            );
            for (index, color) in SWATCHES.into_iter().enumerate() {
                let dot = swatch_box(m, index);
                p.swatch(dot, color, color == ui.color, dot.contains(ui.pointer));
            }
            p.line(
                (m.x + 10.0, m.y + 107.0),
                (m.x + 268.0, m.y + 107.0),
                [255, 255, 255, 24],
                1.0,
            );
            p.text(
                fonts,
                "常用颜色",
                m.x + 12.0,
                m.y + 119.0,
                11.0,
                [143, 161, 187, 255],
            );
        }
        _ => {
            let items: &[(&str, &str, bool)] = if menu == Menu::Shapes {
                &[
                    ("矩形", "shape-rectangle", ui.shape == 2),
                    ("圆形", "shape-circle", ui.shape == 14),
                    ("方形", "shape-square", ui.shape == 13),
                ]
            } else {
                &[
                    ("马赛克", "effects", ui.effect == 0),
                    ("高斯模糊", "blur", ui.effect == 1),
                ]
            };
            for (i, (label, icon, selected)) in items.iter().enumerate() {
                let r = menu_item_box(m, i);
                if r.contains(ui.pointer) {
                    p.rect(r, 6.0, [255, 255, 255, 22]);
                } else if *selected {
                    p.rect(r, 6.0, [83, 103, 170, 85]);
                }
                p.icon(
                    icon,
                    BoxF {
                        x: r.x + 10.0,
                        y: r.y + 9.0,
                        width: 18.0,
                        height: 18.0,
                    },
                    INK,
                );
                p.text(
                    fonts,
                    label,
                    r.x + 40.0,
                    r.y + 9.0,
                    12.0,
                    [228, 236, 249, 255],
                );
            }
        }
    }
    Ok(())
}
fn draw_slider(p: &mut Painter<'_>, b: BoxF, value: f32, min: f32, max: f32) {
    let fraction = ((value - min) / (max - min)).clamp(0.0, 1.0);
    let y = b.y + 14.0;
    let x = b.x + fraction * b.width;
    p.line((b.x, y), (b.x + b.width, y), [90, 108, 133, 255], 3.0);
    p.line((b.x, y), (x, y), [135, 170, 232, 255], 3.0);
    p.rect(
        BoxF {
            x: x - 6.0,
            y: y - 6.0,
            width: 12.0,
            height: 12.0,
        },
        6.0,
        [183, 207, 255, 255],
    );
}
fn number(p: &mut Painter<'_>, fonts: &mut Fonts, b: BoxF, value: &str, focused: bool) {
    p.rect(b, 6.0, [17, 23, 34, 255]);
    p.border(
        b.inset(0.5),
        6.0,
        if focused {
            [157, 190, 255, 255]
        } else {
            [255, 255, 255, 34]
        },
        1.0,
    );
    p.text(
        fonts,
        value,
        b.x + 12.0,
        b.y + 6.0,
        12.0,
        [238, 243, 255, 255],
    );
}
