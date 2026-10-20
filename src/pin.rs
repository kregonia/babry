use crate::compositor;
use image::{RgbaImage, imageops};
use smithay_client_toolkit::reexports::client::{
    Connection, EventQueue, QueueHandle, delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::{
    wp_viewport, wp_viewporter,
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
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const LEFT_BUTTON: u32 = 0x110;
const RIGHT_BUTTON: u32 = 0x111;
const MIN_PIN_SIZE: u32 = 64;
const RESIZE_EDGE_DISTANCE: f64 = 14.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResizeEdge {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// niri 的隐式鼠标抓取固定在按下时的表面坐标；KWin/Hyprland 使用移动后的表面坐标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PointerCoordinates {
    Grab,
    Surface,
}

impl PointerCoordinates {
    fn for_backend(backend: Option<compositor::Backend>) -> Self {
        if backend == Some(compositor::Backend::Niri) {
            Self::Grab
        } else {
            Self::Surface
        }
    }
}

#[derive(Clone, Copy)]
struct PinDrag {
    grab: (f64, f64),
    geometry: (i32, i32, u32, u32),
    edge: Option<ResizeEdge>,
    coordinates: PointerCoordinates,
}

impl PinDrag {
    fn delta(&self, position: (f64, f64), current_origin: (i32, i32)) -> (f64, f64) {
        let (press_x, press_y, ..) = self.geometry;
        let (origin_x, origin_y) = match self.coordinates {
            PointerCoordinates::Grab => (press_x, press_y),
            PointerCoordinates::Surface => current_origin,
        };
        (
            f64::from(origin_x) + position.0 - f64::from(press_x) - self.grab.0,
            f64::from(origin_y) + position.1 - f64::from(press_y) - self.grab.1,
        )
    }
}

#[derive(Clone, Copy)]
struct OutputMetrics {
    width: u32,
    height: u32,
    scale: f32,
}

/// Start an independent process so the pin outlives the editor window.
pub fn launch(path: &Path, output: Option<String>) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!("贴图文件不存在: {}", path.display()));
    }

    let current = std::env::current_exe().map_err(|error| error.to_string())?;
    let sibling = current.with_file_name("babry-pin");
    let mut args = vec![path.to_string_lossy().into_owned()];
    if let Some(output) = output {
        args.extend(["--output".to_string(), output]);
    }
    let (program, args): (PathBuf, Vec<String>) = if sibling.is_file() {
        (sibling, args)
    } else {
        let mut full = vec!["--pin-surface".to_string()];
        full.extend(args);
        (current, full)
    };

    Command::new(&program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法启动贴图窗口 {}: {error}", program.display()))
}

pub fn run_surface(path: &Path, output_name: Option<String>) -> Result<(), String> {
    let (mut pin, mut event_queue) = create_pin(path, output_name)?;
    while !pin.exit {
        event_queue
            .blocking_dispatch(&mut pin)
            .map_err(|error| format!("Wayland 事件循环失败: {error}"))?;
    }
    Ok(())
}

fn create_pin(
    path: &Path,
    output_name: Option<String>,
) -> Result<(PinSurface, EventQueue<PinSurface>), String> {
    let source = image::open(path)
        .map_err(|error| format!("无法读取贴图 {}: {error}", path.display()))?
        .into_rgba8();
    if source.width() == 0 || source.height() == 0 {
        return Err("贴图尺寸为空".to_string());
    }

    let metrics = focused_output_metrics(output_name.as_deref());
    // 默认使用截图在当前输出上的完整逻辑尺寸，只在超出输出时等比缩小。
    let (base_width, base_height) = fitted_size(&source, metrics);
    let x = ((metrics.width.saturating_sub(base_width)) / 2) as i32;
    let y = ((metrics.height.saturating_sub(base_height)) / 2) as i32;

    let connection =
        Connection::connect_to_env().map_err(|error| format!("无法连接 Wayland: {error}"))?;
    let (globals, event_queue) = registry_queue_init(&connection)
        .map_err(|error| format!("无法初始化 Wayland registry: {error}"))?;
    let qh = event_queue.handle();
    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|error| format!("wl_compositor 不可用: {error}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|error| format!("wlr-layer-shell 不可用: {error}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|error| format!("wl_shm 不可用: {error}"))?;

    let surface = compositor.create_surface(&qh);
    let viewporter: Option<wp_viewporter::WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    let viewport = viewporter
        .as_ref()
        .map(|manager| manager.get_viewport(&surface, &qh, ()));
    if let Some(viewport) = &viewport {
        viewport.set_destination(base_width as i32, base_height as i32);
    }
    let display = if viewport.is_none() {
        Some(imageops::resize(
            &source,
            base_width,
            base_height,
            imageops::FilterType::Lanczos3,
        ))
    } else {
        None
    };
    let buffer_size = if viewport.is_some() {
        source.dimensions()
    } else {
        (base_width, base_height)
    };
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Top, Some("babry-pin"), None);
    layer.set_anchor(Anchor::TOP | Anchor::LEFT);
    layer.set_size(base_width, base_height);
    layer.set_margin(y, 0, 0, x);
    layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    layer.commit();

    let initial_pool_size = (buffer_size.0 as usize)
        .saturating_mul(buffer_size.1 as usize)
        .saturating_mul(4)
        .max(4);
    let pool = SlotPool::new(initial_pool_size, &shm)
        .map_err(|error| format!("无法创建贴图缓冲区: {error}"))?;
    let pin = PinSurface {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        layer,
        pool,
        source: Some(source),
        display,
        viewport,
        _viewporter: viewporter,
        display_size: (base_width, base_height),
        screen_width: metrics.width,
        screen_height: metrics.height,
        base_width,
        base_height,
        zoom: 1.0,
        x,
        y,
        drag: None,
        pointer_coordinates: PointerCoordinates::for_backend(compositor::Backend::detect().ok()),
        first_configure: true,
        pending_redraw: true,
        exit: false,
        buffer: None,
        pointer: None,
        keyboard: None,
    };

    Ok((pin, event_queue))
}

struct PinSurface {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    layer: LayerSurface,
    pool: SlotPool,
    source: Option<RgbaImage>,
    display: Option<RgbaImage>, // 仅合成器不支持 viewporter 时使用 CPU 回退。
    viewport: Option<wp_viewport::WpViewport>,
    _viewporter: Option<wp_viewporter::WpViewporter>,
    display_size: (u32, u32),
    screen_width: u32,
    screen_height: u32,
    base_width: u32,
    base_height: u32,
    zoom: f32,
    x: i32,
    y: i32,
    drag: Option<PinDrag>,
    pointer_coordinates: PointerCoordinates,
    first_configure: bool,
    pending_redraw: bool,
    exit: bool,
    buffer: Option<smithay_client_toolkit::shm::slot::Buffer>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
}

impl PinSurface {
    fn draw(&mut self) -> Result<(), String> {
        // viewporter 路径的图像始终不变，只提交一次共享内存缓冲区。
        if self.viewport.is_some() && self.buffer.is_some() {
            return Ok(());
        }
        let image = if self.viewport.is_some() {
            self.source.as_ref().unwrap()
        } else {
            self.display.as_ref().unwrap()
        };
        let (width, height) = image.dimensions();
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| "贴图缓冲区行宽溢出".to_string())?;
        let (buffer, canvas) = self
            .pool
            .create_buffer(
                width as i32,
                height as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
            )
            .map_err(|error| format!("无法创建贴图缓冲区: {error}"))?;

        for (pixel, chunk) in image.pixels().zip(canvas.as_chunks_mut::<4>().0) {
            // Wayland ARGB8888 使用预乘 alpha；截图通常不透明，也支持透明 PNG。
            let alpha = u16::from(pixel[3]);
            chunk.copy_from_slice(&[
                ((u16::from(pixel[2]) * alpha + 127) / 255) as u8,
                ((u16::from(pixel[1]) * alpha + 127) / 255) as u8,
                ((u16::from(pixel[0]) * alpha + 127) / 255) as u8,
                pixel[3],
            ]);
        }

        self.layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        buffer
            .attach_to(self.layer.wl_surface())
            .map_err(|error| format!("无法提交贴图缓冲区: {error}"))?;
        self.buffer = Some(buffer);
        if self.viewport.is_some() {
            self.source = None;
        }
        self.pending_redraw = false;
        self.layer.commit();
        Ok(())
    }

    fn move_to(&mut self, x: f64, y: f64) {
        let max_x = self.screen_width.saturating_sub(self.display_size.0) as i32;
        let max_y = self.screen_height.saturating_sub(self.display_size.1) as i32;
        let x = (x.round() as i32).clamp(0, max_x);
        let y = (y.round() as i32).clamp(0, max_y);
        if (x, y) == (self.x, self.y) {
            return;
        }
        self.x = x;
        self.y = y;
        self.layer.set_margin(self.y, 0, 0, self.x);
        self.layer.commit();
    }

    fn resize_edge_at(&self, position: (f64, f64)) -> Option<ResizeEdge> {
        resize_edge_at(position, self.display_size.0, self.display_size.1)
    }
}

fn resize_edge_at(position: (f64, f64), width_px: u32, height_px: u32) -> Option<ResizeEdge> {
    let width = width_px as f64;
    let height = height_px as f64;
    let left = position.0 <= RESIZE_EDGE_DISTANCE;
    let right = position.0 >= width - RESIZE_EDGE_DISTANCE;
    let top = position.1 <= RESIZE_EDGE_DISTANCE;
    let bottom = position.1 >= height - RESIZE_EDGE_DISTANCE;
    match (left, right, top, bottom) {
        (true, false, true, false) => Some(ResizeEdge::TopLeft),
        (false, true, true, false) => Some(ResizeEdge::TopRight),
        (true, false, false, true) => Some(ResizeEdge::BottomLeft),
        (false, true, false, true) => Some(ResizeEdge::BottomRight),
        (true, false, false, false) => Some(ResizeEdge::Left),
        (false, true, false, false) => Some(ResizeEdge::Right),
        (false, false, true, false) => Some(ResizeEdge::Top),
        (false, false, false, true) => Some(ResizeEdge::Bottom),
        _ => None,
    }
}

impl PinSurface {
    fn apply_pointer_motion(&mut self, position: (f64, f64)) {
        let Some(drag) = self.drag else { return };
        let (dx, dy) = drag.delta(position, (self.x, self.y));
        if let Some(edge) = drag.edge {
            self.resize_from_press(drag.geometry, edge, dx, dy);
        } else {
            self.move_to(
                f64::from(drag.geometry.0) + dx,
                f64::from(drag.geometry.1) + dy,
            );
        }
    }

    /// 从按下时的几何出发应用缩放总量，保证拖动反向时不残留累计误差。
    fn resize_from_press(
        &mut self,
        geometry: (i32, i32, u32, u32),
        edge: ResizeEdge,
        total_dx: f64,
        total_dy: f64,
    ) {
        let (press_x, press_y, press_width, press_height) = geometry;
        let old_width = press_width as f64;
        let old_height = press_height as f64;
        let (mut left, mut top, mut right, mut bottom) = (
            press_x as f64,
            press_y as f64,
            press_x as f64 + old_width,
            press_y as f64 + old_height,
        );
        match edge {
            ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft => left += total_dx,
            ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight => right += total_dx,
            _ => {}
        }
        match edge {
            ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight => top += total_dy,
            ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight => {
                bottom += total_dy
            }
            _ => {}
        }
        let min = MIN_PIN_SIZE as f64;
        if right - left < min {
            if matches!(
                edge,
                ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft
            ) {
                left = right - min;
            } else {
                right = left + min;
            }
        }
        if bottom - top < min {
            if matches!(
                edge,
                ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight
            ) {
                top = bottom - min;
            } else {
                bottom = top + min;
            }
        }
        let width = ((right - left).round().max(min) as u32).min(self.screen_width);
        let height = ((bottom - top).round().max(min) as u32).min(self.screen_height);
        let max_x = self.screen_width.saturating_sub(width) as i32;
        let max_y = self.screen_height.saturating_sub(height) as i32;
        let x = (left.round() as i32).clamp(0, max_x);
        let y = (top.round() as i32).clamp(0, max_y);
        if (x, y, width, height) == (self.x, self.y, self.display_size.0, self.display_size.1) {
            return;
        }
        self.x = x;
        self.y = y;
        self.apply_size(width, height);
    }

    fn apply_size(&mut self, width: u32, height: u32) {
        self.display_size = (width, height);
        if let Some(viewport) = &self.viewport {
            viewport.set_destination(width as i32, height as i32);
        } else {
            self.display = Some(imageops::resize(
                self.source.as_ref().unwrap(),
                width,
                height,
                imageops::FilterType::Lanczos3,
            ));
            self.pending_redraw = true;
        }
        self.layer.set_size(width, height);
        self.layer.set_margin(self.y, 0, 0, self.x);
        self.layer.commit();
    }

    fn change_zoom(&mut self, factor: f32) {
        let (old_width, old_height) = self.display_size;
        let center_x = self.x + old_width as i32 / 2;
        let center_y = self.y + old_height as i32 / 2;

        let max_zoom = (self.screen_width as f32 / self.base_width as f32)
            .min(self.screen_height as f32 / self.base_height as f32)
            .max(0.2);
        self.zoom = (self.zoom * factor).clamp(0.2, max_zoom);
        let width = (self.base_width as f32 * self.zoom).round().max(1.0) as u32;
        let height = (self.base_height as f32 * self.zoom).round().max(1.0) as u32;
        if width == old_width && height == old_height {
            return;
        }

        self.x = center_x - width as i32 / 2;
        self.y = center_y - height as i32 / 2;
        let max_x = self.screen_width.saturating_sub(width) as i32;
        let max_y = self.screen_height.saturating_sub(height) as i32;
        self.x = self.x.clamp(0, max_x);
        self.y = self.y.clamp(0, max_y);
        self.apply_size(width, height);
    }
}

impl Drop for PinSurface {
    fn drop(&mut self) {
        if let Some(viewport) = &self.viewport {
            viewport.destroy();
        }
    }
}

delegate_noop!(PinSurface: ignore wp_viewporter::WpViewporter);
delegate_noop!(PinSurface: ignore wp_viewport::WpViewport);

fn focused_output_metrics(name: Option<&str>) -> OutputMetrics {
    let fallback = OutputMetrics {
        width: 1920,
        height: 1080,
        scale: 1.0,
    };
    let Ok(outputs) = compositor::outputs() else {
        return fallback;
    };
    let Ok(output) = compositor::select_output(&outputs, name) else {
        return fallback;
    };
    OutputMetrics {
        width: output.width,
        height: output.height,
        scale: output.scale,
    }
}

fn fitted_size(source: &RgbaImage, output: OutputMetrics) -> (u32, u32) {
    let natural_width = source.width() as f32 / output.scale;
    let natural_height = source.height() as f32 / output.scale;
    let fit = (output.width as f32 / natural_width)
        .min(output.height as f32 / natural_height)
        .min(1.0);
    (
        (natural_width * fit).round().max(1.0) as u32,
        (natural_height * fit).round().max(1.0) as u32,
    )
}

impl CompositorHandler for PinSurface {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for PinSurface {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for PinSurface {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        _configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        if self.first_configure || self.pending_redraw {
            self.first_configure = false;
            if let Err(error) = self.draw() {
                eprintln!("babry pin: {error}");
                self.exit = true;
            }
        }
    }
}

impl SeatHandler for PinSurface {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
                Err(error) => eprintln!("babry pin: 无法获取鼠标输入: {error}"),
            }
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(keyboard) => self.keyboard = Some(keyboard),
                Err(error) => eprintln!("babry pin: 无法获取键盘输入: {error}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
        }
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl KeyboardHandler for PinSurface {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        if event.keysym == Keysym::Escape {
            self.exit = true;
        }
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
    }
}

impl PointerHandler for PinSurface {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        self.handle_pointer_events(events);
    }
}

impl PinSurface {
    fn handle_pointer_events(&mut self, events: &[PointerEvent]) {
        let mut zoom_factor = 1.0;
        // 连续 motion 只提交最后一次；按键边界前先应用，不能丢掉松手所在帧的末次移动。
        let mut last_motion: Option<(f64, f64)> = None;

        for event in events {
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            match event.kind {
                PointerEventKind::Motion { .. } if self.drag.is_some() => {
                    last_motion = Some(event.position);
                }
                PointerEventKind::Press { button, .. } if button == LEFT_BUTTON => {
                    if let Some(position) = last_motion.take() {
                        self.apply_pointer_motion(position);
                    }
                    self.drag = Some(PinDrag {
                        grab: event.position,
                        geometry: (self.x, self.y, self.display_size.0, self.display_size.1),
                        edge: self.resize_edge_at(event.position),
                        coordinates: self.pointer_coordinates,
                    });
                }
                PointerEventKind::Press { button, .. } if button == RIGHT_BUTTON => {
                    self.exit = true;
                }
                PointerEventKind::Release { button, .. } if button == LEFT_BUTTON => {
                    if let Some(position) = last_motion.take() {
                        self.apply_pointer_motion(position);
                    }
                    self.drag = None;
                }
                PointerEventKind::Axis { vertical, .. } if !vertical.is_none() => {
                    let delta = if vertical.value120 != 0 {
                        vertical.value120 as f64
                    } else {
                        vertical.absolute
                    };
                    zoom_factor = if delta < 0.0 { 1.1 } else { 0.9 };
                }
                _ => {}
            }
        }

        if let Some(position) = last_motion {
            self.apply_pointer_motion(position);
        }

        if zoom_factor != 1.0 {
            self.change_zoom(zoom_factor);
        }
    }
}

impl ShmHandler for PinSurface {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(PinSurface);
delegate_output!(PinSurface);
delegate_shm!(PinSurface);
delegate_seat!(PinSurface);
delegate_keyboard!(PinSurface);
delegate_pointer!(PinSurface);
delegate_layer!(PinSurface);
delegate_registry!(PinSurface);

impl ProvidesRegistryState for PinSurface {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_edge_detection_distinguishes_corners() {
        assert_eq!(
            resize_edge_at((2.0, 2.0), 400, 300),
            Some(ResizeEdge::TopLeft)
        );
        assert_eq!(resize_edge_at((200.0, 150.0), 400, 300), None);
    }

    #[test]
    fn drag_coordinates_follow_backend_and_ignore_replayed_motion() {
        for backend in [
            compositor::Backend::Niri,
            compositor::Backend::Kde,
            compositor::Backend::Hyprland,
        ] {
            let drag = PinDrag {
                grab: (200.0, 140.0),
                geometry: (824, 436, 400, 280),
                edge: None,
                coordinates: PointerCoordinates::for_backend(Some(backend)),
            };
            assert_eq!(drag.delta((280.0, 190.0), (824, 436)), (80.0, 50.0));
            // niri 抓取期间重复原位置；KWin/Hyprland 把位置换算到已移动的表面。
            let replay = if backend == compositor::Backend::Niri {
                (280.0, 190.0)
            } else {
                (200.0, 140.0)
            };
            for _ in 0..8 {
                assert_eq!(drag.delta(replay, (904, 486)), (80.0, 50.0));
            }
            let reverse = if backend == compositor::Backend::Niri {
                (230.0, 160.0)
            } else {
                (150.0, 110.0)
            };
            assert_eq!(drag.delta(reverse, (904, 486)), (30.0, 20.0));
        }
    }

    #[test]
    #[ignore = "requires a live Wayland session with layer-shell and viewporter"]
    fn viewport_resize_reuses_one_buffer_and_releases_source_pixels() {
        use smithay_client_toolkit::reexports::client::Proxy;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pin.png");
        RgbaImage::from_pixel(400, 300, image::Rgba([30, 120, 200, 255]))
            .save(&path)
            .unwrap();
        let (mut pin, mut queue) = create_pin(&path, None).unwrap();
        assert!(pin.viewport.is_some(), "live compositor has no viewporter");
        for _ in 0..8 {
            queue.roundtrip(&mut pin).unwrap();
            if pin.buffer.is_some() {
                break;
            }
        }
        assert!(pin.buffer.is_some(), "surface was not configured");
        let id = pin.buffer.as_ref().unwrap().wl_buffer().id();
        assert!(pin.source.is_none());
        assert!(pin.display.is_none());
        for (width, height) in [(300, 200), (600, 450), (400, 300)] {
            pin.apply_size(width, height);
            queue.roundtrip(&mut pin).unwrap();
            assert_eq!(pin.display_size, (width, height));
            assert_eq!(pin.buffer.as_ref().unwrap().wl_buffer().id(), id);
            assert!(!pin.pending_redraw);
            assert!(pin.source.is_none());
            assert!(pin.display.is_none());
        }
    }

    #[test]
    #[ignore = "requires a live Wayland session with layer-shell and viewporter"]
    fn pointer_frames_preserve_release_motion_and_all_resize_edges() {
        use smithay_client_toolkit::reexports::client::Proxy;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pointer.png");
        RgbaImage::from_pixel(400, 300, image::Rgba([30, 120, 200, 255]))
            .save(&path)
            .unwrap();
        let (mut pin, mut queue) = create_pin(&path, None).unwrap();
        for _ in 0..8 {
            queue.roundtrip(&mut pin).unwrap();
            if pin.buffer.is_some() {
                break;
            }
        }
        assert!(pin.viewport.is_some());
        // 几何算法不依赖宿主输出大小，低分辨率测试会话也使用相同的边界场景。
        pin.screen_width = 1920;
        pin.screen_height = 1080;
        let buffer = pin.buffer.as_ref().unwrap().wl_buffer().id();
        let surface = pin.layer.wl_surface().clone();
        let motion = |position| PointerEvent {
            surface: surface.clone(),
            position,
            kind: PointerEventKind::Motion { time: 1 },
        };
        let button = |position, pressed| PointerEvent {
            surface: surface.clone(),
            position,
            kind: if pressed {
                PointerEventKind::Press {
                    time: 1,
                    button: LEFT_BUTTON,
                    serial: 1,
                }
            } else {
                PointerEventKind::Release {
                    time: 1,
                    button: LEFT_BUTTON,
                    serial: 1,
                }
            },
        };
        for coordinates in [PointerCoordinates::Grab, PointerCoordinates::Surface] {
            pin.pointer_coordinates = coordinates;
            pin.x = 500;
            pin.y = 300;
            pin.apply_size(400, 300);
            pin.handle_pointer_events(&[button((200.0, 150.0), true)]);
            pin.handle_pointer_events(&[motion((280.0, 200.0))]);
            assert_eq!((pin.x, pin.y), (580, 350));
            let stationary = if coordinates == PointerCoordinates::Grab {
                (280.0, 200.0)
            } else {
                (200.0, 150.0)
            };
            for _ in 0..8 {
                pin.handle_pointer_events(&[motion(stationary)]);
                assert_eq!((pin.x, pin.y), (580, 350), "stationary pointer drifted");
            }
            let final_position = if coordinates == PointerCoordinates::Grab {
                (300.0, 215.0)
            } else {
                (220.0, 165.0)
            };
            // 多次移动与松手可以同帧到达，必须先提交末次移动再清理抓取状态。
            pin.handle_pointer_events(&[
                motion(stationary),
                motion(final_position),
                button(final_position, false),
            ]);
            assert_eq!((pin.x, pin.y), (600, 365));
            assert!(pin.drag.is_none());
            pin.handle_pointer_events(&[motion((0.0, 0.0))]);
            assert_eq!((pin.x, pin.y), (600, 365));
            for (grab, delta, expected) in [
                ((2.0, 150.0), (-40.0, 0.0), (460, 300, 440, 300)),
                ((398.0, 150.0), (40.0, 0.0), (500, 300, 440, 300)),
                ((200.0, 2.0), (0.0, -30.0), (500, 270, 400, 330)),
                ((200.0, 298.0), (0.0, 30.0), (500, 300, 400, 330)),
                ((2.0, 2.0), (-40.0, -30.0), (460, 270, 440, 330)),
                ((398.0, 2.0), (40.0, -30.0), (500, 270, 440, 330)),
                ((2.0, 298.0), (-40.0, 30.0), (460, 300, 440, 330)),
                ((398.0, 298.0), (40.0, 30.0), (500, 300, 440, 330)),
            ] {
                pin.x = 500;
                pin.y = 300;
                pin.apply_size(400, 300);
                pin.handle_pointer_events(&[button(grab, true)]);
                let position = (grab.0 + delta.0, grab.1 + delta.1);
                pin.handle_pointer_events(&[motion(position)]);
                let stationary = if coordinates == PointerCoordinates::Grab {
                    position
                } else {
                    (
                        position.0 - f64::from(pin.x - 500),
                        position.1 - f64::from(pin.y - 300),
                    )
                };
                for _ in 0..8 {
                    pin.handle_pointer_events(&[motion(stationary)]);
                    assert_eq!(
                        (pin.x, pin.y, pin.display_size.0, pin.display_size.1),
                        expected
                    );
                }
                // 反向回到按下位置，边界钳制或历史几何不能残留在新尺寸里。
                let reverse = if coordinates == PointerCoordinates::Grab {
                    grab
                } else {
                    (
                        grab.0 - f64::from(pin.x - 500),
                        grab.1 - f64::from(pin.y - 300),
                    )
                };
                pin.handle_pointer_events(&[motion(reverse), button(reverse, false)]);
                assert_eq!(
                    (pin.x, pin.y, pin.display_size.0, pin.display_size.1),
                    (500, 300, 400, 300)
                );
            }
            // 夹到屏幕边缘后再反向，使用按下总量而不是已截断的累计位移。
            pin.x = 500;
            pin.y = 300;
            pin.apply_size(400, 300);
            pin.handle_pointer_events(&[button((200.0, 150.0), true)]);
            pin.handle_pointer_events(&[motion((-900.0, -900.0))]);
            assert_eq!((pin.x, pin.y), (0, 0));
            let reverse = if coordinates == PointerCoordinates::Grab {
                (230.0, 170.0)
            } else {
                (730.0, 470.0)
            };
            pin.handle_pointer_events(&[motion(reverse), button(reverse, false)]);
            assert_eq!((pin.x, pin.y), (530, 320));
        }
        queue.roundtrip(&mut pin).unwrap();
        assert_eq!(pin.buffer.as_ref().unwrap().wl_buffer().id(), buffer);
        assert!(pin.source.is_none() && pin.display.is_none());
    }

    #[test]
    fn fits_physical_image_to_logical_output() {
        let image = RgbaImage::new(2560, 1440);
        let size = fitted_size(
            &image,
            OutputMetrics {
                width: 2048,
                height: 1152,
                scale: 1.25,
            },
        );
        assert_eq!(size, (2048, 1152));
    }
}
