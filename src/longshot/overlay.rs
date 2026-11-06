//! 长截图期间的屏幕提示：四条 1×1 纯色缓冲区拉伸成的边框，加一个小控制条。
//!
//! 边框表面的输入区域为空，指针和滚轮直接落到下方窗口；所有表面都不抢键盘焦点，
//! 用户可以继续用 PageDown、空格等按键滚动页面。
use super::Progress;
use babry::{
    compositor::{self, OutputInfo},
    core::RectPx,
    font::Fonts,
    native_ui::{
        self, SHADOW_MARGIN, longshot_border_strips as border_strips,
        longshot_panel_origin as panel_origin,
    },
    screencopy::dispatch_until,
    stitch::{MAX_HEIGHT, Push, Stitcher},
};
use smithay_client_toolkit::reexports::client::{
    Connection, QueueHandle, delegate_noop,
    globals::registry_queue_init,
    protocol::{
        wl_buffer::WlBuffer, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface,
    },
};
use smithay_client_toolkit::reexports::protocols::wp::{
    cursor_shape::v1::client::wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    viewporter::client::{wp_viewport, wp_viewporter},
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{
            PointerEvent, PointerEventKind, PointerHandler, cursor_shape::CursorShapeManager,
        },
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{
        Shm, ShmHandler,
        raw::RawPool,
        slot::{Buffer, SlotPool},
    },
};
use std::{
    sync::mpsc::Receiver,
    thread::JoinHandle,
    time::{Duration, Instant},
};

const LEFT: u32 = 0x110;
/// 控制条文字；`first` 为第一帧高度，拼接高度未超过它时提示用户开始滚动。
fn status(progress: Option<Progress>, hint: Option<Push>, first: u32) -> String {
    let Some(progress) = progress else {
        return "长截图准备中…".into();
    };
    let height = progress.height;
    match hint {
        Some(Push::Full) => format!("已达到 {MAX_HEIGHT} px 上限"),
        Some(Push::Lost) => format!("已拼接 {height} px · 滚动过快，请回滚一点"),
        Some(Push::Mismatch) => format!("已拼接 {height} px · 区域尺寸变化"),
        _ if height <= first => "请滚动选区内的页面".into(),
        _ => format!("已拼接 {height} px"),
    }
}

struct Strip {
    layer: LayerSurface,
    _viewport: wp_viewport::WpViewport,
}

struct Overlay {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    strips: Vec<Strip>,
    solid: WlBuffer,
    _solid_pool: RawPool,
    panel: LayerSurface,
    panel_viewport: wp_viewport::WpViewport,
    panel_size: (u32, u32),
    /// 控制条在面板表面中的位置（阴影留白）。
    panel_offset: (f32, f32),
    pool: SlotPool,
    buffers: Vec<Buffer>,
    scale: f32,
    fonts: Fonts,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_manager: Option<CursorShapeManager>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    hover: Option<usize>,
    progress: Option<Progress>,
    hint: Option<Push>,
    first_height: u32,
    configured: bool,
    dirty: bool,
    exit: bool,
    finished: bool,
}

pub fn run(
    output: &OutputInfo,
    region: RectPx,
    progress: Receiver<Progress>,
    worker: &JoinHandle<Result<Stitcher, String>>,
) -> Result<bool, String> {
    let connection = Connection::connect_to_env().map_err(|e| format!("无法连接 Wayland: {e}"))?;
    let (globals, mut queue) = registry_queue_init::<Overlay>(&connection)
        .map_err(|e| format!("读取 Wayland 全局对象失败: {e}"))?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let shell = LayerShell::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let viewporter: wp_viewporter::WpViewporter = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("合成器缺少 wp_viewporter: {e}"))?;
    let cursor_manager = CursorShapeManager::bind(&globals, &qh).ok();
    let bound = compositor::bind_output(&connection, &output.name)?;
    let screen = (output.width, output.height);
    // 空输入区域：指针事件穿透到下方窗口。
    let empty = compositor.wl_compositor().create_region(&qh, ());
    let layer = |rect: RectPx, name: &'static str| {
        let surface = compositor.create_surface(&qh);
        let viewport = viewporter.get_viewport(&surface, &qh, ());
        viewport.set_destination(rect.width as i32, rect.height as i32);
        let layer = shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some(name),
            Some(&bound.output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(rect.width, rect.height);
        layer.set_margin(rect.y, 0, 0, rect.x);
        (layer, viewport)
    };
    let strips = border_strips(region, screen)
        .into_iter()
        .map(|rect| {
            let (layer, viewport) = layer(rect, "babry-longshot-border");
            layer.wl_surface().set_input_region(Some(&empty));
            layer.commit();
            Strip {
                layer,
                _viewport: viewport,
            }
        })
        .collect();
    let mut solid_pool = RawPool::new(4, &shm).map_err(|e| format!("无法创建边框缓冲区: {e}"))?;
    // 与编辑器选区边框相同的颜色，内存顺序 BGRA。
    solid_pool.mmap()[..4].copy_from_slice(&[0xff, 0xaa, 0x55, 0xff]);
    let solid = solid_pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, (), &qh);

    let (pw, ph) = native_ui::LONGSHOT_PANEL;
    let origin = panel_origin(region, screen);
    let surface_x = (origin.0 - SHADOW_MARGIN).max(0.0);
    let surface_y = (origin.1 - SHADOW_MARGIN).max(0.0);
    let panel_rect = RectPx::new(
        surface_x as i32,
        surface_y as i32,
        ((origin.0 + pw + SHADOW_MARGIN).min(screen.0 as f32) - surface_x) as u32,
        ((origin.1 + ph + SHADOW_MARGIN).min(screen.1 as f32) - surface_y) as u32,
    );
    let (panel, panel_viewport) = layer(panel_rect, "babry-longshot");
    panel.commit();
    let pool = SlotPool::new(256 * 1024, &shm).map_err(|e| format!("无法创建界面共享内存: {e}"))?;
    let mut overlay = Overlay {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        strips,
        solid,
        _solid_pool: solid_pool,
        panel,
        panel_viewport,
        panel_size: (panel_rect.width, panel_rect.height),
        panel_offset: (origin.0 - surface_x, origin.1 - surface_y),
        pool,
        buffers: Vec::new(),
        scale: output.scale.max(1.0),
        fonts: Fonts::new(),
        pointer: None,
        cursor_manager,
        cursor_device: None,
        hover: None,
        progress: None,
        hint: None,
        first_height: 0,
        configured: false,
        dirty: true,
        exit: false,
        finished: false,
    };
    while !overlay.exit {
        dispatch_until(
            &mut queue,
            &mut overlay,
            Instant::now() + Duration::from_millis(100),
        )?;
        while let Ok(update) = progress.try_recv() {
            overlay.update(update);
        }
        // 取帧线程出错时立即结束，由调用方报告错误。
        if worker.is_finished() {
            overlay.exit = true;
        }
        if overlay.configured && overlay.dirty {
            overlay.draw()?;
        }
    }
    Ok(overlay.finished)
}

impl Overlay {
    fn update(&mut self, progress: Progress) {
        let hint = match progress.push {
            Push::Started => {
                self.first_height = progress.height;
                None
            }
            Push::Appended(_) => None,
            // 没有新内容时保留上一次的提示，直到真正追加。
            Push::Unchanged => self.hint,
            other => Some(other),
        };
        let before = status(self.progress, self.hint, self.first_height);
        self.progress = Some(progress);
        self.hint = hint;
        if status(self.progress, self.hint, self.first_height) != before {
            self.dirty = true;
        }
    }

    fn hit(&self, position: (f64, f64)) -> Option<usize> {
        native_ui::longshot_buttons().iter().position(|b| {
            native_ui::BoxF {
                x: b.x + self.panel_offset.0,
                y: b.y + self.panel_offset.1,
                ..*b
            }
            .contains(position)
        })
    }

    fn draw(&mut self) -> Result<(), String> {
        let physical = (
            (self.panel_size.0 as f32 * self.scale).ceil() as u32,
            (self.panel_size.1 as f32 * self.scale).ceil() as u32,
        );
        // 两块缓冲区轮换；都被合成器占用时等下一轮再画。
        let index = match self
            .buffers
            .iter()
            .position(|b| b.canvas(&mut self.pool).is_some())
        {
            Some(index) => index,
            None if self.buffers.len() < 2 => {
                let (buffer, _) = self
                    .pool
                    .create_buffer(
                        physical.0 as i32,
                        physical.1 as i32,
                        physical.0 as i32 * 4,
                        wl_shm::Format::Argb8888,
                    )
                    .map_err(|e| format!("无法创建控制条缓冲区: {e}"))?;
                self.buffers.push(buffer);
                self.buffers.len() - 1
            }
            None => return Ok(()),
        };
        let text = status(self.progress, self.hint, self.first_height);
        let canvas = self.buffers[index]
            .canvas(&mut self.pool)
            .ok_or("控制条缓冲区仍在使用")?;
        canvas.fill(0);
        native_ui::draw_longshot_panel(
            canvas,
            physical,
            tiny_skia::Transform::from_scale(
                physical.0 as f32 / self.panel_size.0 as f32,
                physical.1 as f32 / self.panel_size.1 as f32,
            ),
            self.panel_offset,
            &text,
            self.hover,
            &mut self.fonts,
        )?;
        // tiny-skia 输出预乘 RGBA；wl_shm Argb8888 的内存顺序为 BGRA。
        for pixel in canvas.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        let surface = self.panel.wl_surface();
        self.buffers[index]
            .attach_to(surface)
            .map_err(|e| format!("附加控制条缓冲区失败: {e:?}"))?;
        surface.damage_buffer(0, 0, physical.0 as i32, physical.1 as i32);
        self.panel_viewport
            .set_destination(self.panel_size.0 as i32, self.panel_size.1 as i32);
        surface.commit();
        self.dirty = false;
        Ok(())
    }
}

impl CompositorHandler for Overlay {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Overlay {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for Overlay {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
            if let (Some(manager), Some(pointer)) = (&self.cursor_manager, &self.pointer) {
                self.cursor_device = Some(manager.get_shape_device(pointer, qh));
            }
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointer = None;
            self.cursor_device = None;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for Overlay {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if event.surface != *self.panel.wl_surface() {
                continue;
            }
            let hover = match event.kind {
                PointerEventKind::Enter { serial } => {
                    if let Some(device) = &self.cursor_device {
                        device.set_shape(serial, Shape::Default);
                    }
                    self.hit(event.position)
                }
                PointerEventKind::Leave { .. } => None,
                PointerEventKind::Motion { .. } => self.hit(event.position),
                PointerEventKind::Press { button: LEFT, .. } => {
                    match self.hit(event.position) {
                        Some(0) => self.exit = true,
                        Some(_) => {
                            self.finished = true;
                            self.exit = true;
                        }
                        None => {}
                    }
                    self.hover
                }
                _ => self.hover,
            };
            if hover != self.hover {
                self.hover = hover;
                self.dirty = true;
            }
        }
    }
}

impl LayerShellHandler for Overlay {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        if *layer == self.panel {
            self.configured = true;
            self.dirty = true;
            return;
        }
        if let Some(strip) = self.strips.iter().find(|strip| strip.layer == *layer) {
            let surface = strip.layer.wl_surface();
            surface.attach(Some(&self.solid), 0, 0);
            surface.damage_buffer(0, 0, 1, 1);
            surface.commit();
        }
    }
}

impl ShmHandler for Overlay {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(Overlay);
delegate_output!(Overlay);
delegate_shm!(Overlay);
delegate_seat!(Overlay);
delegate_pointer!(Overlay);
delegate_layer!(Overlay);
delegate_registry!(Overlay);
delegate_noop!(Overlay: ignore wp_viewporter::WpViewporter);
delegate_noop!(Overlay: ignore wp_viewport::WpViewport);
delegate_noop!(Overlay: ignore wl_region::WlRegion);
delegate_noop!(Overlay: ignore WlBuffer);
impl ProvidesRegistryState for Overlay {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_asks_to_scroll_until_content_is_added() {
        let first = Progress {
            height: 300,
            push: Push::Started,
        };
        assert_eq!(status(Some(first), None, 300), "请滚动选区内的页面");
        let grown = Progress {
            height: 420,
            push: Push::Appended(120),
        };
        assert_eq!(status(Some(grown), None, 300), "已拼接 420 px");
        assert!(status(Some(grown), Some(Push::Lost), 300).contains("滚动过快"));
    }
}
