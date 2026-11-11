//! 整帧软件合成：截图/长图 + 标注 + 选区遮罩 + 工具栏，输出一块屏幕大小的
//! 预乘 RGBA 缓冲区。
//!
//! Wayland 截图编辑使用子表面场景（`scene.rs`），只重绘变化的瓦片；长图画布需要
//! 滚动、X11 没有子表面，这两种情况用这里的整帧合成。标注图层缓存起来，只在标注
//! 或视图变化时重新光栅化。
use crate::{
    core::RectPx,
    editor_core::Core,
    marks::{self, Mark},
    native_ui::{self, BoxF},
};
use tiny_skia::{Color, Paint, PixmapMut, PixmapPaint, Rect, Transform};

const BACKGROUND: [u8; 4] = [24, 26, 30, 255];
const MASK: [u8; 4] = [0, 0, 0, 0x99];
const BORDER: [u8; 4] = [0x55, 0xaa, 0xff, 0xff];
const CARET: [u8; 4] = [0x9d, 0xc8, 0xff, 0xff];

#[derive(Default)]
pub struct SoftCache {
    layer: Option<tiny_skia::Pixmap>,
    /// 缓存图层对应的视图位置和标注数量；任一变化都重新光栅化。
    key: Option<((i32, i32), usize)>,
}

impl SoftCache {
    pub fn invalidate(&mut self) {
        self.key = None;
    }
}

fn fill(target: &mut PixmapMut<'_>, rect: (f32, f32, f32, f32), color: [u8; 4]) {
    if let Some(r) = Rect::from_xywh(rect.0, rect.1, rect.2, rect.3) {
        let mut paint = Paint::default();
        paint.set_color_rgba8(color[0], color[1], color[2], color[3]);
        target.fill_rect(r, &paint, Transform::identity(), None);
    }
}

/// 把 `core` 的当前状态合成到 `frame`（屏幕物理尺寸 `size`，预乘 RGBA）。
/// `marks` 是已提交标注加上正在绘制的标注，`caret` 为图像坐标中的文字光标。
pub fn render(
    frame: &mut [u8],
    size: (u32, u32),
    core: &mut Core,
    marks: &[Mark],
    caret: Option<RectPx>,
    cache: &mut SoftCache,
) -> Result<(), String> {
    let (width, height) = size;
    if core.capturing {
        return render_capture(frame, size, core);
    }
    let view = core.view;
    let row_bytes = width as usize * 4;
    // 长图画布：页面只显示在窗口（原选区）里，窗口外是原截图；普通截图整屏就是图像。
    let area = if core.canvas {
        core.window
    } else {
        RectPx::new(0, 0, width, height)
    };
    let origin = (area.x + view.0, area.y + view.1);
    // 1) 底图：图像行直接读入，图像外部填充背景色。
    for (y, line) in frame.chunks_exact_mut(row_bytes).enumerate() {
        let y = y as i32;
        match &core.backdrop {
            Some(backdrop) if core.canvas => backdrop.read_row(0, y, line),
            _ => line.fill(0),
        }
        if y >= area.y && y < area.bottom() {
            let start = area.x.max(0) as usize * 4;
            let end = (area.right().min(width as i32).max(0) as usize * 4).max(start);
            core.source.read_row(
                origin.0 + (start / 4) as i32 - area.x,
                origin.1 + y - area.y,
                &mut line[start..end],
            );
        }
        for pixel in line.chunks_exact_mut(4) {
            if pixel[3] == 0 {
                pixel.copy_from_slice(&BACKGROUND);
            }
        }
    }
    // 2) 标注图层（窗口大小）：标注或视图变化时重画，否则复用。
    let damaged = !core.damaged.is_empty();
    core.damaged.clear();
    let key = (view, marks.len());
    let stale = damaged || cache.key != Some(key) || core.pending.is_some();
    let layer_size = (area.width.max(1), area.height.max(1));
    if cache.layer.as_ref().map(|l| (l.width(), l.height())) != Some(layer_size) {
        cache.layer = tiny_skia::Pixmap::new(layer_size.0, layer_size.1);
        cache.key = None;
    }
    let layer = cache.layer.as_mut().ok_or("标注图层尺寸无效")?;
    if stale || cache.key.is_none() {
        layer.fill(Color::TRANSPARENT);
        marks::render_layer(
            &mut layer.as_mut(),
            origin,
            marks,
            &core.source,
            &mut core.fonts,
        );
        cache.key = Some(key);
    }
    let mut target = PixmapMut::from_bytes(frame, width, height).ok_or("帧尺寸无效")?;
    target.draw_pixmap(
        area.x,
        area.y,
        layer.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    // 滚动条：长图画布窗口右侧的细条，表示当前可见的位置。
    if core.canvas && core.source.height() > area.height {
        let total = core.source.height() as f32;
        let thumb = (area.height as f32 * area.height as f32 / total).max(24.0);
        let offset = (origin.1 as f32 / (total - area.height as f32).max(1.0))
            * (area.height as f32 - thumb);
        let bar = (4.0 * core.scale()).round();
        fill(
            &mut target,
            (
                (area.right() as f32 - bar - 3.0),
                area.y as f32 + offset,
                bar,
                thumb,
            ),
            [255, 255, 255, 150],
        );
    }
    // 3) 选区遮罩、边框和手柄（长图画布的选区就是整张图，不显示）。
    let scale = core.scale();
    let transform = Transform::from_scale(scale, scale);
    let s = core.ui_selection();
    {
        let (w, h) = (width as f32, height as f32);
        let l = (s.x * scale).round().clamp(0.0, w);
        let t = (s.y * scale).round().clamp(0.0, h);
        let r = ((s.x + s.width) * scale).round().clamp(l, w);
        let b = ((s.y + s.height) * scale).round().clamp(t, h);
        for rect in [
            (0.0, 0.0, w, t),
            (0.0, b, w, h - b),
            (0.0, t, l, b - t),
            (r, t, w - r, b - t),
        ] {
            fill(&mut target, rect, MASK);
        }
        let border = (2.0 * scale).round().max(1.0);
        let half = border / 2.0;
        for rect in [
            (l - half, t - half, r - l + border, border),
            (l - half, b - half, r - l + border, border),
            (l - half, t - half, border, b - t + border),
            (r - half, t - half, border, b - t + border),
        ] {
            fill(&mut target, rect, BORDER);
        }
        // 长图画布的窗口不能拉伸，不显示手柄。
        if !core.canvas {
            for handle in native_ui::handle_boxes(s) {
                native_ui::draw_handle(frame, size, transform, handle)?;
            }
        }
    }
    // 4) 文字光标。
    if let Some(c) = caret {
        let mut target = PixmapMut::from_bytes(frame, width, height).ok_or("帧尺寸无效")?;
        fill(
            &mut target,
            (
                (c.x - view.0) as f32,
                (c.y - view.1) as f32,
                (2.0 * scale).round(),
                (c.height as f32).max(8.0),
            ),
            CARET,
        );
    }
    // 5) 尺寸标签、工具栏、菜单和错误提示，与场景路径使用同一套绘制函数。
    let logical = core.logical;
    let ui = &core.ui;
    let fonts = &mut core.fonts;
    if !ui.editing {
        let b = native_ui::label_box(s, ui.grid);
        let text = format!("{} × {}", core.selection.width, core.selection.height);
        native_ui::draw_label(frame, size, transform, b, &text, fonts)?;
    } else {
        native_ui::draw_toolbar(frame, size, transform, logical, s, ui, fonts)?;
        if ui.menu.is_some() {
            native_ui::draw_menu(frame, size, transform, logical, s, ui, fonts)?;
        }
    }
    if let Some(error) = &ui.error {
        let b: BoxF = native_ui::error_box(logical, ui.grid);
        native_ui::draw_error(frame, size, transform, b, error, fonts)?;
    }
    Ok(())
}

/// 长截图进行中：整帧透明，只在截图区域外画边框、进度和工具栏，
/// 这样合成器截到的选区内容就是下方窗口本身。
fn render_capture(frame: &mut [u8], size: (u32, u32), core: &mut Core) -> Result<(), String> {
    frame.fill(0);
    let scale = core.scale();
    let transform = Transform::from_scale(scale, scale);
    let s = core.ui_selection();
    {
        let mut target = PixmapMut::from_bytes(frame, size.0, size.1).ok_or("帧尺寸无效")?;
        // 边框整体放在选区外侧，并留出 1 个逻辑像素间隙，分数缩放下也不会被截到。
        let (gap, border) = ((1.0 * scale).ceil(), (2.0 * scale).round().max(1.0));
        let l = (s.x * scale).floor() - gap - border;
        let t = (s.y * scale).floor() - gap - border;
        let r = ((s.x + s.width) * scale).ceil() + gap;
        let b = ((s.y + s.height) * scale).ceil() + gap;
        for rect in [
            (l, t, r - l + border, border),
            (l, b, r - l + border, border),
            (l, t, border, b - t + border),
            (r, t, border, b - t + border),
        ] {
            fill(&mut target, rect, BORDER);
        }
    }
    let logical = core.logical;
    let fonts = &mut core.fonts;
    let label = native_ui::label_box(s, core.ui.grid);
    let label = BoxF {
        width: label
            .width
            .max(fonts.measure(&core.capture_status, 12.0, f32::INFINITY).0 + 20.0),
        ..label
    };
    native_ui::draw_label(frame, size, transform, label, &core.capture_status, fonts)?;
    native_ui::draw_toolbar(frame, size, transform, logical, s, &core.ui, fonts)
}
