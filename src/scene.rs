//! 编辑器的 Wayland 合成层。
//!
//! 截图本身是主表面的 wl_shm 缓冲区，只提交一次；遮罩、边框、手柄、工具栏、
//! 菜单和标注瓦片都是 wl_subsurface。选区变化只改变子表面的位置和视口尺寸，
//! 不重绘整帧，合成器也不必重新上传整张截图。
use crate::editor::NativeEditor;
use babry::{
    core::RectPx,
    font::Fonts,
    marks::{self, Mark},
    native_ui::{self, BoxF, EditorUi},
    shm_image::{self, ShmImage},
};
use smithay_client_toolkit::{
    compositor::{CompositorState, Region},
    reexports::{
        client::{
            QueueHandle,
            protocol::{
                wl_buffer::WlBuffer, wl_shm, wl_shm_pool::WlShmPool, wl_subsurface::WlSubsurface,
                wl_surface::WlSurface,
            },
        },
        protocols::wp::viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    },
    shm::{
        Shm,
        raw::RawPool,
        slot::{Buffer, SlotPool},
    },
    subcompositor::SubcompositorState,
};
use std::os::fd::AsFd;
use tiny_skia::PixmapMut;

/// 逻辑坐标矩形 (x, y, width, height)。
pub type IRect = (i32, i32, i32, i32);

/// 标注瓦片缓冲区的 wl_buffer 用户数据：全局槽位编号。
#[derive(Clone, Copy, Debug)]
pub struct TileSlot(pub usize);

pub struct Plane {
    pub surface: WlSurface,
    subsurface: WlSubsurface,
    viewport: WpViewport,
    shown: bool,
    geometry: IRect,
}

impl Plane {
    fn new(
        compositor: &CompositorState,
        subcompositor: &SubcompositorState,
        viewporter: &WpViewporter,
        parent: &WlSurface,
        qh: &QueueHandle<NativeEditor>,
    ) -> Result<Self, String> {
        let (subsurface, surface) = subcompositor.create_subsurface(parent.clone(), qh);
        // 空输入区域：所有指针事件都落到主表面，坐标系保持唯一。
        let region = Region::new(compositor).map_err(|error| error.to_string())?;
        surface.set_input_region(Some(region.wl_region()));
        let viewport = viewporter.get_viewport(&surface, qh, ());
        Ok(Self {
            surface,
            subsurface,
            viewport,
            shown: false,
            geometry: (i32::MIN, i32::MIN, 0, 0),
        })
    }

    /// 用 1×1 纯色缓冲区覆盖一个逻辑矩形。返回是否产生了需要父表面提交的变化。
    fn show_solid(&mut self, buffer: &WlBuffer, rect: IRect) -> bool {
        if rect.2 <= 0 || rect.3 <= 0 {
            return self.hide();
        }
        let mut commit = false;
        if !self.shown {
            self.surface.attach(Some(buffer), 0, 0);
            self.surface.damage_buffer(0, 0, 1, 1);
            self.shown = true;
            commit = true;
        }
        let moved = self.move_to(rect.0, rect.1);
        if (rect.2, rect.3) != (self.geometry.2, self.geometry.3) {
            self.viewport.set_destination(rect.2, rect.3);
            self.geometry.2 = rect.2;
            self.geometry.3 = rect.3;
            commit = true;
        }
        if commit {
            self.surface.commit();
        }
        commit || moved
    }

    fn move_to(&mut self, x: i32, y: i32) -> bool {
        if (x, y) == (self.geometry.0, self.geometry.1) {
            return false;
        }
        self.subsurface.set_position(x, y);
        self.geometry.0 = x;
        self.geometry.1 = y;
        true
    }

    fn hide(&mut self) -> bool {
        if !self.shown {
            return false;
        }
        self.surface.attach(None, 0, 0);
        self.surface.commit();
        self.shown = false;
        self.geometry.2 = 0;
        self.geometry.3 = 0;
        true
    }
}

/// 带双缓冲内容的小平面（工具栏、菜单、标签、错误提示）。
struct Panel {
    plane: Plane,
    buffers: Vec<Buffer>,
    size: (u32, u32),
}

impl Panel {
    fn render(
        &mut self,
        pool: &mut SlotPool,
        rect: IRect,
        physical: (u32, u32),
        draw: impl FnOnce(&mut [u8], (u32, u32)) -> Result<(), String>,
    ) -> Result<bool, String> {
        if self.size != physical {
            self.buffers.clear();
            self.size = physical;
        }
        let index = match self.buffers.iter().position(|b| b.canvas(pool).is_some()) {
            Some(index) => index,
            None if self.buffers.len() < 2 => {
                let (buffer, _) = pool
                    .create_buffer(
                        physical.0 as i32,
                        physical.1 as i32,
                        physical.0 as i32 * 4,
                        wl_shm::Format::Argb8888,
                    )
                    .map_err(|error| format!("无法创建界面缓冲区: {error}"))?;
                self.buffers.push(buffer);
                self.buffers.len() - 1
            }
            // 两个缓冲区都仍由合成器持有：保留脏状态，release 后再画。
            None => return Ok(false),
        };
        let canvas = self.buffers[index]
            .canvas(pool)
            .ok_or("界面缓冲区仍在使用")?;
        canvas.fill(0);
        draw(canvas, physical)?;
        rgba_to_bgra(canvas);
        self.buffers[index]
            .attach_to(&self.plane.surface)
            .map_err(|error| format!("附加界面缓冲区失败: {error:?}"))?;
        self.plane
            .surface
            .damage_buffer(0, 0, physical.0 as i32, physical.1 as i32);
        self.plane.viewport.set_destination(rect.2, rect.3);
        self.plane.geometry.2 = rect.2;
        self.plane.geometry.3 = rect.3;
        self.plane.move_to(rect.0, rect.1);
        self.plane.surface.commit();
        self.plane.shown = true;
        Ok(true)
    }

    fn hide(&mut self) -> bool {
        self.plane.hide()
    }
}

/// tiny-skia 输出预乘 RGBA；wl_shm Argb8888 的内存顺序为 BGRA。
fn rgba_to_bgra(bytes: &mut [u8]) {
    for pixel in bytes.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

struct SlotState {
    buffer: Option<WlBuffer>,
    busy: bool,
    resident: bool,
}

struct Tile {
    plane: Plane,
    attached: Option<usize>,
}

/// 标注瓦片：每个瓦片固定两个槽位，整池按需分配物理页。
struct Tiles {
    pool: RawPool,
    logical: u32,
    cols: u32,
    rows: u32,
    slot_bytes: usize,
    tiles: Vec<Option<Tile>>,
    slots: Vec<SlotState>,
    pending: std::collections::BTreeSet<usize>,
}

impl Tiles {
    fn new(shm: &Shm, logical: (u32, u32), scale: f32, step: u32) -> Result<Self, String> {
        // 瓦片约 256 物理像素见方，逻辑边长对齐到缩放网格，保证瓦片边缘落在整数物理像素。
        let size = (((256.0 / scale) / step as f32).ceil() as u32).max(1) * step;
        let max_physical = (size as f32 * scale).ceil() as u32;
        let cols = logical.0.div_ceil(size);
        let rows = logical.1.div_ceil(size);
        let slot_bytes = (max_physical as usize * max_physical as usize * 4).next_multiple_of(4096);
        let count = (cols * rows) as usize;
        let len = slot_bytes * count * 2;
        if len > i32::MAX as usize {
            return Err("标注瓦片池超过 wl_shm 上限".into());
        }
        // memfd 是稀疏文件：未绘制的瓦片不占物理内存。
        let pool = RawPool::new(len.max(4096), shm)
            .map_err(|error| format!("无法创建标注共享内存: {error}"))?;
        Ok(Self {
            pool,
            logical: size,
            cols,
            rows,
            slot_bytes,
            tiles: (0..count).map(|_| None).collect(),
            slots: (0..count * 2)
                .map(|_| SlotState {
                    buffer: None,
                    busy: false,
                    resident: false,
                })
                .collect(),
            pending: Default::default(),
        })
    }

    fn release_slot(&mut self, slot: usize) {
        let offset = slot * self.slot_bytes;
        shm_image::release_pages(self.pool.as_fd(), offset, self.slot_bytes);
        self.slots[slot].resident = false;
    }

    fn released(&mut self, slot: usize) {
        let Some(state) = self.slots.get_mut(slot) else {
            return;
        };
        state.busy = false;
        let attached = self.tiles[slot / 2].as_ref().and_then(|tile| tile.attached);
        if attached != Some(slot) && state.resident {
            self.release_slot(slot);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct ToolbarKey {
    tool: u8,
    shape: u8,
    effect: u8,
    color: [u8; 4],
    width: u32,
    font_size: u32,
    strength: u32,
    next_number: u32,
    hovered: Option<native_ui::Action>,
    pressed: Option<native_ui::Action>,
    number_input: Option<(bool, String)>,
    size: (i32, i32),
}

pub struct Scene {
    pub logical: (u32, u32),
    pub physical: (u32, u32),
    scale: f32,
    step: i32,
    compositor: CompositorState,
    subcompositor: SubcompositorState,
    viewporter: WpViewporter,
    parent: WlSurface,
    _screenshot_pool: WlShmPool,
    _screenshot: WlBuffer,
    _solid_pool: RawPool,
    mask_buffer: WlBuffer,
    border_buffer: WlBuffer,
    caret_buffer: WlBuffer,
    panel_pool: SlotPool,
    handle_sprites: Vec<(Buffer, (i32, i32))>,
    masks: Vec<Plane>,
    borders: Vec<Plane>,
    handles: Vec<Plane>,
    label: Panel,
    toolbar: Panel,
    menu: Panel,
    caret: Plane,
    error: Panel,
    tiles: Tiles,
    label_key: Option<(String, IRect)>,
    toolbar_key: Option<ToolbarKey>,
    menu_key: Option<(ToolbarKey, Option<native_ui::Menu>, IRect)>,
    error_key: Option<(String, IRect)>,
}

pub struct SceneInput<'a> {
    pub selection: RectPx,
    pub ui: &'a EditorUi,
    pub marks: &'a [Mark],
    pub source: &'a ShmImage,
    pub fonts: &'a mut Fonts,
    /// 文字光标的物理坐标矩形。
    pub caret: Option<RectPx>,
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

impl Scene {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        compositor: CompositorState,
        subcompositor: SubcompositorState,
        viewporter: WpViewporter,
        shm: &Shm,
        parent: &WlSurface,
        source: &ShmImage,
        logical: (u32, u32),
        qh: &QueueHandle<NativeEditor>,
    ) -> Result<Self, String> {
        let physical = (source.width(), source.height());
        let scale = physical.0 as f32 / logical.0.max(1) as f32;
        // 找到最小的逻辑步长，使其乘以缩放后正好是整数物理像素（1.25 倍时为 4）。
        let step_x = logical.0 / gcd(physical.0, logical.0).max(1);
        let step_y = logical.1 / gcd(physical.1, logical.1).max(1);
        let lcm = step_x / gcd(step_x, step_y).max(1) * step_y;
        let step = if (1..=24).contains(&lcm) { lcm } else { 1 };

        // 截图 memfd 直接成为主表面缓冲区；不额外复制显示帧。
        let screenshot_pool = shm
            .wl_shm()
            .create_pool(source.fd(), source.len() as i32, qh, ());
        let screenshot = screenshot_pool.create_buffer(
            0,
            physical.0 as i32,
            physical.1 as i32,
            source.stride() as i32,
            wl_shm::Format::Xrgb8888,
            qh,
            (),
        );
        parent.attach(Some(&screenshot), 0, 0);
        parent.damage_buffer(0, 0, physical.0 as i32, physical.1 as i32);

        // 纯色平面共用 1×1 缓冲区，由视口拉伸；预乘 BGRA。
        let mut solid_pool =
            RawPool::new(64, shm).map_err(|error| format!("无法创建纯色缓冲区: {error}"))?;
        let colors: [[u8; 4]; 3] = [
            [0, 0, 0, 0x99],
            [0xff, 0xaa, 0x55, 0xff],
            [0xff, 0xc8, 0x9d, 0xff],
        ];
        for (index, color) in colors.iter().enumerate() {
            solid_pool.mmap()[index * 4..index * 4 + 4].copy_from_slice(color);
        }
        let mut solid = |index: i32| {
            solid_pool.create_buffer(index * 4, 1, 1, 4, wl_shm::Format::Argb8888, (), qh)
        };
        let mask_buffer = solid(0);
        let border_buffer = solid(1);
        let caret_buffer = solid(2);

        let mut panel_pool = SlotPool::new(512 * 1024, shm)
            .map_err(|error| format!("无法创建界面共享内存: {error}"))?;
        let mut handle_sprites = Vec::new();
        for (w, h) in [(8, 8), (10, 8), (8, 10)] {
            let size = (
                (w as f32 * scale).ceil() as u32,
                (h as f32 * scale).ceil() as u32,
            );
            let (buffer, canvas) = panel_pool
                .create_buffer(
                    size.0 as i32,
                    size.1 as i32,
                    size.0 as i32 * 4,
                    wl_shm::Format::Argb8888,
                )
                .map_err(|error| format!("无法创建手柄缓冲区: {error}"))?;
            canvas.fill(0);
            native_ui::draw_handle(
                canvas,
                size,
                tiny_skia::Transform::from_scale(
                    size.0 as f32 / w as f32,
                    size.1 as f32 / h as f32,
                ),
                BoxF {
                    x: 0.0,
                    y: 0.0,
                    width: w as f32,
                    height: h as f32,
                },
            )?;
            rgba_to_bgra(canvas);
            handle_sprites.push((buffer, (w, h)));
        }

        let plane = || Plane::new(&compositor, &subcompositor, &viewporter, parent, qh);
        let panel = || -> Result<Panel, String> {
            Ok(Panel {
                plane: plane()?,
                buffers: Vec::new(),
                size: (0, 0),
            })
        };
        // 创建顺序即叠放顺序（自下而上）；标注瓦片稍后插入到遮罩之下。
        let masks = (0..4).map(|_| plane()).collect::<Result<Vec<_>, _>>()?;
        let borders = (0..4).map(|_| plane()).collect::<Result<Vec<_>, _>>()?;
        let handles = (0..8).map(|_| plane()).collect::<Result<Vec<_>, _>>()?;
        let label = panel()?;
        let toolbar = panel()?;
        let menu = panel()?;
        let caret = plane()?;
        let error = panel()?;
        let tiles = Tiles::new(shm, logical, scale, step)?;
        Ok(Self {
            logical,
            physical,
            scale,
            step: step as i32,
            compositor,
            subcompositor,
            viewporter,
            parent: parent.clone(),
            _screenshot_pool: screenshot_pool,
            _screenshot: screenshot,
            _solid_pool: solid_pool,
            mask_buffer,
            border_buffer,
            caret_buffer,
            panel_pool,
            handle_sprites,
            masks,
            borders,
            handles,
            label,
            toolbar,
            menu,
            caret,
            error,
            tiles,
            label_key: None,
            toolbar_key: None,
            menu_key: None,
            error_key: None,
        })
    }

    /// 界面控件对齐的逻辑网格步长。
    pub fn grid(&self) -> f32 {
        self.step as f32
    }

    /// 控件盒外扩阴影边距后对齐到网格，得到平面矩形。
    fn panel_rect(&self, b: BoxF, margin: f32) -> IRect {
        let step = self.step as f32;
        let margin = (margin / step).ceil() * step;
        let x = ((b.x - margin) / step).floor() * step;
        let y = ((b.y - margin) / step).floor() * step;
        let right = ((b.x + b.width + margin) / step).ceil() * step;
        let bottom = ((b.y + b.height + margin) / step).ceil() * step;
        (x as i32, y as i32, (right - x) as i32, (bottom - y) as i32)
    }

    fn physical_size(&self, rect: IRect) -> (u32, u32) {
        (
            ((rect.2 as f32 * self.scale).round() as u32).max(1),
            ((rect.3 as f32 * self.scale).round() as u32).max(1),
        )
    }

    pub fn tile_released(&mut self, slot: usize) {
        self.tiles.released(slot);
    }

    /// 标记物理像素范围内的标注瓦片需要重绘。
    pub fn damage(&mut self, rect: RectPx) {
        let t = &mut self.tiles;
        let size = t.logical as f32 * self.scale;
        let x0 = (rect.x.max(0) as f32 / size).floor() as u32;
        let y0 = (rect.y.max(0) as f32 / size).floor() as u32;
        let x1 = ((rect.right().max(0) as f32 / size).ceil() as u32).min(t.cols);
        let y1 = ((rect.bottom().max(0) as f32 / size).ceil() as u32).min(t.rows);
        for row in y0..y1 {
            for col in x0..x1 {
                t.pending.insert((row * t.cols + col) as usize);
            }
        }
    }

    fn tile_rects(&self, index: usize) -> (IRect, RectPx) {
        let t = &self.tiles;
        let (col, row) = (index as u32 % t.cols, index as u32 / t.cols);
        let lx0 = col * t.logical;
        let ly0 = row * t.logical;
        let lx1 = ((col + 1) * t.logical).min(self.logical.0);
        let ly1 = ((row + 1) * t.logical).min(self.logical.1);
        let px = |v: u32, limit: u32| ((v as f32 * self.scale).round() as u32).min(limit);
        let (px0, py0) = (px(lx0, self.physical.0), px(ly0, self.physical.1));
        let (px1, py1) = (px(lx1, self.physical.0), px(ly1, self.physical.1));
        (
            (
                lx0 as i32,
                ly0 as i32,
                (lx1 - lx0) as i32,
                (ly1 - ly0) as i32,
            ),
            RectPx::new(px0 as i32, py0 as i32, px1 - px0, py1 - py0),
        )
    }
}

impl ToolbarKey {
    fn new(ui: &EditorUi, logical: (u32, u32), s: BoxF, size: (i32, i32)) -> Self {
        Self {
            tool: ui.tool,
            shape: ui.shape,
            effect: ui.effect,
            color: ui.color,
            width: ui.width,
            font_size: ui.font_size,
            strength: ui.strength,
            next_number: ui.next_number,
            hovered: ui.hit(logical, s, ui.pointer),
            pressed: ui.pressed,
            number_input: ui.number_input.clone(),
            size,
        }
    }
}

impl Scene {
    /// 根据编辑器状态更新所有平面；返回 (是否需要提交父表面, 是否全部完成)。
    /// 缓冲区仍被合成器占用时对应内容保持待绘制，release 后再次调用即可。
    pub fn update(
        &mut self,
        input: SceneInput<'_>,
        qh: &QueueHandle<NativeEditor>,
    ) -> Result<(bool, bool), String> {
        let SceneInput {
            selection,
            ui,
            marks,
            source,
            fonts,
            caret,
        } = input;
        let s = native_ui::logical_selection(selection, self.physical, self.logical);
        let mut changed = self.place_selection(s);
        let mut complete = true;
        let scale = self.scale;

        if ui.editing {
            changed |= self.label.hide();
            self.label_key = None;
        } else {
            let b = native_ui::label_box(s, self.grid());
            let rect = self.panel_rect(b, 0.0);
            let text = format!("{} × {}", selection.width, selection.height);
            if self.label_key.as_ref().is_some_and(|(t, _)| *t == text) {
                changed |= self.label.plane.move_to(rect.0, rect.1);
            } else {
                let size = self.physical_size(rect);
                let transform = native_ui::plane_transform((rect.0 as f32, rect.1 as f32), scale);
                let local = BoxF {
                    x: rect.0 as f32,
                    y: rect.1 as f32,
                    ..b
                };
                if self
                    .label
                    .render(&mut self.panel_pool, rect, size, |bytes, size| {
                        native_ui::draw_label(bytes, size, transform, local, &text, fonts)
                    })?
                {
                    self.label_key = Some((text, rect));
                    changed = true;
                } else {
                    complete = false;
                }
            }
        }

        if ui.editing {
            let b = ui.toolbar(self.logical, s);
            let rect = self.panel_rect(b, native_ui::SHADOW_MARGIN);
            let key = ToolbarKey::new(ui, self.logical, s, (rect.2, rect.3));
            if self.toolbar_key.as_ref() == Some(&key) {
                changed |= self.toolbar.plane.move_to(rect.0, rect.1);
            } else {
                let size = self.physical_size(rect);
                let transform = native_ui::plane_transform((rect.0 as f32, rect.1 as f32), scale);
                let logical = self.logical;
                if self
                    .toolbar
                    .render(&mut self.panel_pool, rect, size, |bytes, size| {
                        native_ui::draw_toolbar(bytes, size, transform, logical, s, ui, fonts)
                    })?
                {
                    self.toolbar_key = Some(key);
                    changed = true;
                } else {
                    complete = false;
                }
            }
        } else {
            changed |= self.toolbar.hide();
            self.toolbar_key = None;
        }

        if let (true, Some(menu)) = (ui.editing, ui.menu) {
            let b = ui.menu_box(self.logical, s, menu);
            let rect = self.panel_rect(b, native_ui::SHADOW_MARGIN);
            let key = (
                ToolbarKey::new(ui, self.logical, s, (rect.2, rect.3)),
                Some(menu),
                rect,
            );
            if self.menu_key.as_ref() != Some(&key) {
                let size = self.physical_size(rect);
                let transform = native_ui::plane_transform((rect.0 as f32, rect.1 as f32), scale);
                let logical = self.logical;
                if self
                    .menu
                    .render(&mut self.panel_pool, rect, size, |bytes, size| {
                        native_ui::draw_menu(bytes, size, transform, logical, s, ui, fonts)
                    })?
                {
                    self.menu_key = Some(key);
                    changed = true;
                } else {
                    complete = false;
                }
            }
        } else {
            changed |= self.menu.hide();
            self.menu_key = None;
        }

        changed |= match caret {
            Some(c) => {
                let x = (c.x as f32 / scale).floor() as i32;
                let y = (c.y as f32 / scale).floor() as i32;
                let h = ((c.height as f32 / scale).ceil() as i32).max(8);
                self.caret.show_solid(&self.caret_buffer, (x, y, 2, h))
            }
            None => self.caret.hide(),
        };

        if let Some(error) = &ui.error {
            let b = native_ui::error_box(self.logical, self.grid());
            let rect = self.panel_rect(b, 0.0);
            if self.error_key.as_ref() != Some(&(error.clone(), rect)) {
                let size = self.physical_size(rect);
                let transform = native_ui::plane_transform((rect.0 as f32, rect.1 as f32), scale);
                if self
                    .error
                    .render(&mut self.panel_pool, rect, size, |bytes, size| {
                        native_ui::draw_error(bytes, size, transform, b, error, fonts)
                    })?
                {
                    self.error_key = Some((error.clone(), rect));
                    changed = true;
                } else {
                    complete = false;
                }
            }
        } else {
            changed |= self.error.hide();
            self.error_key = None;
        }

        let pending: Vec<usize> = self.tiles.pending.iter().copied().collect();
        for index in pending {
            if let Some(tile_changed) = self.render_tile(index, marks, source, fonts, qh)? {
                self.tiles.pending.remove(&index);
                changed |= tile_changed;
            }
        }
        complete &= self.tiles.pending.is_empty();
        Ok((changed, complete))
    }

    fn place_selection(&mut self, s: BoxF) -> bool {
        let (lw, lh) = (self.logical.0 as i32, self.logical.1 as i32);
        let l = (s.x.floor() as i32).clamp(0, lw);
        let t = (s.y.floor() as i32).clamp(0, lh);
        let r = ((s.x + s.width).ceil() as i32).clamp(l, lw);
        let b = ((s.y + s.height).ceil() as i32).clamp(t, lh);
        let masks = [
            (0, 0, lw, t),
            (0, b, lw, lh - b),
            (0, t, l, b - t),
            (r, t, lw - r, b - t),
        ];
        let borders = [
            (l - 1, t - 1, r - l + 2, 2),
            (l - 1, b - 1, r - l + 2, 2),
            (l - 1, t - 1, 2, b - t + 2),
            (r - 1, t - 1, 2, b - t + 2),
        ];
        let mut changed = false;
        for (plane, rect) in self.masks.iter_mut().zip(masks) {
            changed |= plane.show_solid(&self.mask_buffer, rect);
        }
        for (plane, rect) in self.borders.iter_mut().zip(borders) {
            changed |= plane.show_solid(&self.border_buffer, rect);
        }
        for (index, (plane, b)) in self
            .handles
            .iter_mut()
            .zip(native_ui::handle_boxes(s))
            .enumerate()
        {
            let kind = match index {
                0..=3 => 0,
                4 | 6 => 1,
                _ => 2,
            };
            let (buffer, size) = &self.handle_sprites[kind];
            // 手柄精灵在 8 个平面间共享且从不重写，直接附加 wl_buffer。
            changed |= plane.show_solid(
                buffer.wl_buffer(),
                (b.x.round() as i32, b.y.round() as i32, size.0, size.1),
            );
        }
        changed
    }

    /// 返回 `None` 表示两个槽位都被合成器占用，需要稍后重试。
    fn render_tile(
        &mut self,
        index: usize,
        marks: &[Mark],
        source: &ShmImage,
        fonts: &mut Fonts,
        qh: &QueueHandle<NativeEditor>,
    ) -> Result<Option<bool>, String> {
        let (logical, physical) = self.tile_rects(index);
        if physical.width == 0 || physical.height == 0 {
            return Ok(Some(false));
        }
        let hit = marks.iter().any(|mark| {
            mark.bounds()
                .and_then(|b| marks::intersect(b, physical))
                .is_some()
        });
        if !hit {
            return Ok(Some(self.clear_tile(index)));
        }
        let attached = self.tiles.tiles[index]
            .as_ref()
            .and_then(|tile| tile.attached);
        let Some(slot) = (index * 2..index * 2 + 2)
            .find(|&slot| !self.tiles.slots[slot].busy && Some(slot) != attached)
        else {
            return Ok(None);
        };
        let (w, h) = (physical.width, physical.height);
        let offset = slot * self.tiles.slot_bytes;
        let len = w as usize * h as usize * 4;
        let empty = {
            let canvas = &mut self.tiles.pool.mmap()[offset..offset + len];
            canvas.fill(0);
            let mut pixmap = PixmapMut::from_bytes(canvas, w, h).ok_or("瓦片尺寸无效")?;
            marks::render_layer(&mut pixmap, (physical.x, physical.y), marks, source, fonts);
            let canvas = &mut self.tiles.pool.mmap()[offset..offset + len];
            let empty = canvas.chunks_exact(4).all(|pixel| pixel[3] == 0);
            if !empty {
                rgba_to_bgra(canvas);
            }
            empty
        };
        self.tiles.slots[slot].resident = true;
        if empty {
            self.tiles.release_slot(slot);
            return Ok(Some(self.clear_tile(index)));
        }
        if self.tiles.tiles[index].is_none() {
            let plane = Plane::new(
                &self.compositor,
                &self.subcompositor,
                &self.viewporter,
                &self.parent,
                qh,
            )?;
            plane.subsurface.place_below(&self.masks[0].surface);
            self.tiles.tiles[index] = Some(Tile {
                plane,
                attached: None,
            });
        }
        let buffer = self.tiles.slots[slot]
            .buffer
            .get_or_insert_with(|| {
                self.tiles.pool.create_buffer(
                    offset as i32,
                    w as i32,
                    h as i32,
                    w as i32 * 4,
                    wl_shm::Format::Argb8888,
                    TileSlot(slot),
                    qh,
                )
            })
            .clone();
        let tile = self.tiles.tiles[index].as_mut().expect("瓦片刚刚创建");
        tile.plane.surface.attach(Some(&buffer), 0, 0);
        tile.plane.surface.damage_buffer(0, 0, w as i32, h as i32);
        if !tile.plane.shown {
            tile.plane.viewport.set_destination(logical.2, logical.3);
            tile.plane.geometry.2 = logical.2;
            tile.plane.geometry.3 = logical.3;
            tile.plane.shown = true;
        }
        tile.plane.move_to(logical.0, logical.1);
        tile.plane.surface.commit();
        tile.attached = Some(slot);
        self.tiles.slots[slot].busy = true;
        Ok(Some(true))
    }

    fn clear_tile(&mut self, index: usize) -> bool {
        let Some(tile) = self.tiles.tiles[index].as_mut() else {
            return false;
        };
        let changed = tile.plane.hide();
        tile.attached = None;
        for slot in index * 2..index * 2 + 2 {
            if !self.tiles.slots[slot].busy && self.tiles.slots[slot].resident {
                self.tiles.release_slot(slot);
            }
        }
        changed
    }
}
