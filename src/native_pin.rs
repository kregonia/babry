//! 原生贴图：PNG 逐行解码进 memfd，同一块内存直接作为 wl_shm 缓冲区。
//!
//! 缩放和拉伸只改变 wp_viewport 目标尺寸，不在 CPU 上重采样，也不保留第二份图像。
use crate::{
    compositor::{self, Backend},
    shm_image::ShmImage,
};
use smithay_client_toolkit::reexports::client::{
    Connection, QueueHandle, delegate_noop,
    globals::registry_queue_init,
    protocol::{
        wl_buffer::WlBuffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_shm_pool,
        wl_surface,
    },
};
use smithay_client_toolkit::reexports::protocols::wp::{
    cursor_shape::v1::client::wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    viewporter::client::{wp_viewport, wp_viewporter},
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
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
    shm::{Shm, ShmHandler},
};
use std::path::Path;

const LEFT: u32 = 0x110;
const RIGHT: u32 = 0x111;
const MIN_PIN_SIZE: f64 = 64.0;
const RESIZE_EDGE_DISTANCE: f64 = 14.0;

/// niri 的隐式抓取固定使用按下时的表面坐标；KWin/Hyprland 使用移动后的表面坐标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PointerCoordinates {
    Grab,
    Surface,
}

#[derive(Clone, Copy, Debug)]
struct Drag {
    grab: (f64, f64),
    geometry: (i32, i32, u32, u32),
    /// (左, 上, 右, 下) 是否随指针移动；全部为 false 表示移动贴图。
    edges: (bool, bool, bool, bool),
}

pub fn run_surface(path: &Path, output: Option<String>) -> Result<(), String> {
    let image = ShmImage::read_png(path)?;
    let outputs = compositor::outputs()?;
    let selected = compositor::select_output(&outputs, output.as_deref())?;
    let coordinates = if Backend::detect().ok() == Some(Backend::Niri) {
        PointerCoordinates::Grab
    } else {
        PointerCoordinates::Surface
    };
    let connection = Connection::connect_to_env().map_err(|e| format!("无法连接 Wayland: {e}"))?;
    let (globals, mut queue) = registry_queue_init::<Pin>(&connection)
        .map_err(|e| format!("读取 Wayland 全局对象失败: {e}"))?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let shell = LayerShell::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let viewporter: wp_viewporter::WpViewporter = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("合成器缺少 wp_viewporter: {e}"))?;
    let cursor_manager = CursorShapeManager::bind(&globals, &qh).ok();

    // 按物理像素 1:1 显示，超出屏幕时等比缩小。
    let natural = (
        f64::from(image.width()) / f64::from(selected.scale),
        f64::from(image.height()) / f64::from(selected.scale),
    );
    let fit = (f64::from(selected.width) / natural.0)
        .min(f64::from(selected.height) / natural.1)
        .min(1.0);
    let width = (natural.0 * fit).round().max(1.0) as u32;
    let height = (natural.1 * fit).round().max(1.0) as u32;
    let x = (selected.width.saturating_sub(width) / 2) as i32;
    let y = (selected.height.saturating_sub(height) / 2) as i32;

    let surface = compositor.create_surface(&qh);
    let viewport = viewporter.get_viewport(&surface, &qh, ());
    viewport.set_destination(width as i32, height as i32);
    let layer = shell.create_layer_surface(&qh, surface, Layer::Top, Some("babry-pin"), None);
    layer.set_anchor(Anchor::TOP | Anchor::LEFT);
    layer.set_size(width, height);
    layer.set_margin(y, 0, 0, x);
    layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    layer.commit();
    let pool = shm
        .wl_shm()
        .create_pool(image.fd(), image.len() as i32, &qh, ());
    let format = if image.has_alpha() {
        wl_shm::Format::Argb8888
    } else {
        wl_shm::Format::Xrgb8888
    };
    let buffer = pool.create_buffer(
        0,
        image.width() as i32,
        image.height() as i32,
        image.stride() as i32,
        format,
        &qh,
        (),
    );
    let mut pin = Pin {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        layer,
        viewport,
        _viewporter: viewporter,
        _pool: pool,
        buffer,
        _image: image,
        x,
        y,
        width,
        height,
        base: (width, height),
        screen: (selected.width, selected.height),
        coordinates,
        drag: None,
        pointer: None,
        keyboard: None,
        cursor_manager,
        cursor_device: None,
        cursor_serial: 0,
        cursor_shape: None,
        configured: false,
        exit: false,
    };
    while !pin.exit {
        queue
            .blocking_dispatch(&mut pin)
            .map_err(|e| format!("贴图 Wayland 事件失败: {e}"))?;
    }
    Ok(())
}

struct Pin {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    layer: LayerSurface,
    viewport: wp_viewport::WpViewport,
    _viewporter: wp_viewporter::WpViewporter,
    _pool: wl_shm_pool::WlShmPool,
    buffer: WlBuffer,
    _image: ShmImage,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    base: (u32, u32),
    screen: (u32, u32),
    coordinates: PointerCoordinates,
    drag: Option<Drag>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    cursor_manager: Option<CursorShapeManager>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    cursor_serial: u32,
    cursor_shape: Option<Shape>,
    configured: bool,
    exit: bool,
}

fn edges_at(position: (f64, f64), width: u32, height: u32) -> (bool, bool, bool, bool) {
    let (w, h) = (f64::from(width), f64::from(height));
    // 贴图很小时把边缘区域限制在三分之一以内，中间仍可拖动。
    let d = RESIZE_EDGE_DISTANCE.min(w / 3.0).min(h / 3.0);
    (
        position.0 <= d,
        position.1 <= d,
        position.0 >= w - d,
        position.1 >= h - d,
    )
}

fn cursor_for(edges: (bool, bool, bool, bool)) -> Shape {
    match edges {
        (true, true, _, _) => Shape::NwResize,
        (_, true, true, _) => Shape::NeResize,
        (_, _, true, true) => Shape::SeResize,
        (true, _, _, true) => Shape::SwResize,
        (true, _, _, _) => Shape::WResize,
        (_, true, _, _) => Shape::NResize,
        (_, _, true, _) => Shape::EResize,
        (_, _, _, true) => Shape::SResize,
        _ => Shape::Move,
    }
}

impl Pin {
    fn apply_geometry(&mut self, x: i32, y: i32, width: u32, height: u32) {
        let width = width.clamp(1, self.screen.0.max(1));
        let height = height.clamp(1, self.screen.1.max(1));
        let x = x.clamp(0, self.screen.0.saturating_sub(width) as i32);
        let y = y.clamp(0, self.screen.1.saturating_sub(height) as i32);
        if (x, y, width, height) == (self.x, self.y, self.width, self.height) {
            return;
        }
        if (width, height) != (self.width, self.height) {
            self.viewport.set_destination(width as i32, height as i32);
            self.layer.set_size(width, height);
        }
        self.x = x;
        self.y = y;
        self.width = width;
        self.height = height;
        self.layer.set_margin(y, 0, 0, x);
        self.layer.commit();
    }

    fn motion(&mut self, position: (f64, f64)) {
        let Some(drag) = self.drag else {
            return;
        };
        let (px, py, pw, ph) = drag.geometry;
        let origin = match self.coordinates {
            PointerCoordinates::Grab => (px, py),
            PointerCoordinates::Surface => (self.x, self.y),
        };
        // 从按下时的几何计算总位移，反向拖动不会累计误差。
        let dx = f64::from(origin.0) + position.0 - f64::from(px) - drag.grab.0;
        let dy = f64::from(origin.1) + position.1 - f64::from(py) - drag.grab.1;
        let (left_edge, top_edge, right_edge, bottom_edge) = drag.edges;
        if !(left_edge || top_edge || right_edge || bottom_edge) {
            self.apply_geometry(
                (f64::from(px) + dx).round() as i32,
                (f64::from(py) + dy).round() as i32,
                pw,
                ph,
            );
            return;
        }
        let (mut left, mut top) = (f64::from(px), f64::from(py));
        let (mut right, mut bottom) = (left + f64::from(pw), top + f64::from(ph));
        if left_edge {
            left = (left + dx).min(right - MIN_PIN_SIZE);
        }
        if right_edge {
            right = (right + dx).max(left + MIN_PIN_SIZE);
        }
        if top_edge {
            top = (top + dy).min(bottom - MIN_PIN_SIZE);
        }
        if bottom_edge {
            bottom = (bottom + dy).max(top + MIN_PIN_SIZE);
        }
        self.apply_geometry(
            left.round() as i32,
            top.round() as i32,
            (right - left).round() as u32,
            (bottom - top).round() as u32,
        );
    }

    /// 滚轮以贴图中心为锚点、基于当前尺寸等比缩放（保留手动拉伸的比例）。
    fn zoom_by(&mut self, factor: f64) {
        let max = (f64::from(self.screen.0) / f64::from(self.width))
            .min(f64::from(self.screen.1) / f64::from(self.height));
        let min = (f64::from(self.base.0) * 0.2 / f64::from(self.width))
            .max(MIN_PIN_SIZE / f64::from(self.width.min(self.height)));
        let factor = factor.clamp(min.min(1.0), max.max(1.0));
        let width = (f64::from(self.width) * factor).round().max(1.0) as u32;
        let height = (f64::from(self.height) * factor).round().max(1.0) as u32;
        let cx = self.x + self.width as i32 / 2;
        let cy = self.y + self.height as i32 / 2;
        self.apply_geometry(cx - width as i32 / 2, cy - height as i32 / 2, width, height);
    }

    fn update_cursor(&mut self, position: (f64, f64)) {
        let shape = match self.drag {
            Some(drag) if drag.edges == (false, false, false, false) => Shape::Grabbing,
            Some(drag) => cursor_for(drag.edges),
            None => cursor_for(edges_at(position, self.width, self.height)),
        };
        if self.cursor_shape != Some(shape)
            && let Some(device) = &self.cursor_device
        {
            device.set_shape(self.cursor_serial, shape);
            self.cursor_shape = Some(shape);
        }
    }
}

impl CompositorHandler for Pin {
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

impl OutputHandler for Pin {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for Pin {
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
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
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
        if capability == Capability::Keyboard {
            self.keyboard = None;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for Pin {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if event.keysym == Keysym::Escape {
            self.exit = true;
        }
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }
}

impl PointerHandler for Pin {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        // 同一帧内只应用最后一次移动；松开前先应用它，避免丢失最后一段位移。
        let mut last_motion = None;
        for event in events {
            if event.surface != *self.layer.wl_surface() {
                continue;
            }
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    self.cursor_serial = serial;
                    self.cursor_shape = None;
                }
                PointerEventKind::Press { button: LEFT, .. } => {
                    self.drag = Some(Drag {
                        grab: event.position,
                        geometry: (self.x, self.y, self.width, self.height),
                        edges: edges_at(event.position, self.width, self.height),
                    });
                }
                PointerEventKind::Release { button: LEFT, .. } => {
                    if let Some(position) = last_motion.take() {
                        self.motion(position);
                    }
                    self.drag = None;
                }
                PointerEventKind::Press { button: RIGHT, .. } => self.exit = true,
                PointerEventKind::Motion { .. } => last_motion = Some(event.position),
                PointerEventKind::Axis { vertical, .. } => {
                    let steps = if vertical.value120 != 0 {
                        f64::from(vertical.value120) / 120.0
                    } else if vertical.discrete != 0 {
                        f64::from(vertical.discrete)
                    } else {
                        vertical.absolute / 15.0
                    };
                    if steps != 0.0 {
                        self.zoom_by(0.9_f64.powf(steps));
                    }
                }
                _ => {}
            }
            self.update_cursor(event.position);
        }
        if let Some(position) = last_motion {
            self.motion(position);
        }
    }
}

impl LayerShellHandler for Pin {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        if !self.configured {
            // 贴图内容只提交一次；后续拉伸由视口完成。
            let surface = self.layer.wl_surface();
            surface.attach(Some(&self.buffer), 0, 0);
            surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            self.layer.commit();
            self.configured = true;
        }
    }
}

impl ShmHandler for Pin {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(Pin);
delegate_output!(Pin);
delegate_shm!(Pin);
delegate_noop!(Pin: ignore wp_viewporter::WpViewporter);
delegate_noop!(Pin: ignore wp_viewport::WpViewport);
delegate_noop!(Pin: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Pin: ignore WlBuffer);
delegate_seat!(Pin);
delegate_keyboard!(Pin);
delegate_pointer!(Pin);
delegate_layer!(Pin);
delegate_registry!(Pin);
impl ProvidesRegistryState for Pin {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_detection_keeps_a_draggable_center_on_small_pins() {
        assert_eq!(edges_at((2.0, 2.0), 200, 100), (true, true, false, false));
        assert_eq!(
            edges_at((100.0, 50.0), 200, 100),
            (false, false, false, false)
        );
        assert_eq!(
            edges_at((199.0, 50.0), 200, 100),
            (false, false, true, false)
        );
        // 30 像素宽的贴图：边缘区域缩到 10 像素，中心仍可移动。
        assert_eq!(edges_at((15.0, 15.0), 30, 30), (false, false, false, false));
    }
}
