use image::{RgbaImage, imageops};
use smithay_client_toolkit::reexports::client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
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
    convert::TryInto,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const LEFT_BUTTON: u32 = 0x110;
const RIGHT_BUTTON: u32 = 0x111;

#[derive(Clone, Copy)]
struct OutputMetrics {
    width: u32,
    height: u32,
    scale: f32,
}

/// Start an independent process so the pin outlives the editor window.
pub fn launch(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!("贴图文件不存在: {}", path.display()));
    }

    let current = std::env::current_exe().map_err(|error| error.to_string())?;
    let sibling = current.with_file_name("babry-pin");
    let (program, args): (PathBuf, Vec<String>) = if sibling.is_file() {
        (sibling, vec![path.to_string_lossy().into_owned()])
    } else {
        (
            current,
            vec![
                "--pin-surface".to_string(),
                path.to_string_lossy().into_owned(),
            ],
        )
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

pub fn run_surface(path: &Path) -> Result<(), String> {
    let source = image::open(path)
        .map_err(|error| format!("无法读取贴图 {}: {error}", path.display()))?
        .to_rgba8();
    if source.width() == 0 || source.height() == 0 {
        return Err("贴图尺寸为空".to_string());
    }

    let metrics = focused_output_metrics();
    let (base_width, base_height) = fitted_size(&source, metrics);
    let display = imageops::resize(
        &source,
        base_width,
        base_height,
        imageops::FilterType::Lanczos3,
    );
    let x = ((metrics.width - base_width) / 2) as i32;
    let y = ((metrics.height - base_height) / 2) as i32;

    let connection =
        Connection::connect_to_env().map_err(|error| format!("无法连接 Wayland: {error}"))?;
    let (globals, mut event_queue) = registry_queue_init(&connection)
        .map_err(|error| format!("无法初始化 Wayland registry: {error}"))?;
    let qh = event_queue.handle();
    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|error| format!("wl_compositor 不可用: {error}"))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|error| format!("wlr-layer-shell 不可用: {error}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|error| format!("wl_shm 不可用: {error}"))?;

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Top, Some("babry-pin"), None);
    layer.set_anchor(Anchor::TOP | Anchor::LEFT);
    layer.set_size(base_width, base_height);
    layer.set_margin(y, 0, 0, x);
    layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    layer.commit();

    let initial_pool_size = (base_width as usize)
        .saturating_mul(base_height as usize)
        .saturating_mul(4)
        .max(4);
    let pool = SlotPool::new(initial_pool_size, &shm)
        .map_err(|error| format!("无法创建贴图缓冲区: {error}"))?;
    let mut pin = PinSurface {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        layer,
        pool,
        source,
        display,
        screen_width: metrics.width,
        screen_height: metrics.height,
        base_width,
        base_height,
        zoom: 1.0,
        x,
        y,
        dragging: false,
        last_pointer: None,
        first_configure: true,
        pending_redraw: true,
        exit: false,
        buffer: None,
        pointer: None,
        keyboard: None,
    };

    while !pin.exit {
        event_queue
            .blocking_dispatch(&mut pin)
            .map_err(|error| format!("Wayland 事件循环失败: {error}"))?;
    }
    Ok(())
}

struct PinSurface {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    layer: LayerSurface,
    pool: SlotPool,
    source: RgbaImage,
    display: RgbaImage,
    screen_width: u32,
    screen_height: u32,
    base_width: u32,
    base_height: u32,
    zoom: f32,
    x: i32,
    y: i32,
    dragging: bool,
    last_pointer: Option<(f64, f64)>,
    first_configure: bool,
    pending_redraw: bool,
    exit: bool,
    buffer: Option<smithay_client_toolkit::shm::slot::Buffer>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
}

impl PinSurface {
    fn draw(&mut self) -> Result<(), String> {
        let width = self.display.width();
        let height = self.display.height();
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

        for (pixel, chunk) in self.display.pixels().zip(canvas.chunks_exact_mut(4)) {
            let bytes: &mut [u8; 4] = chunk.try_into().expect("ARGB8888 chunk size");
            *bytes = [pixel[2], pixel[1], pixel[0], pixel[3]];
        }

        self.layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        buffer
            .attach_to(self.layer.wl_surface())
            .map_err(|error| format!("无法提交贴图缓冲区: {error}"))?;
        self.buffer = Some(buffer);
        self.pending_redraw = false;
        self.layer.commit();
        Ok(())
    }

    fn move_by(&mut self, dx: f64, dy: f64) {
        let max_x = self.screen_width.saturating_sub(self.display.width()) as i32;
        let max_y = self.screen_height.saturating_sub(self.display.height()) as i32;
        self.x = (self.x + dx.round() as i32).clamp(0, max_x);
        self.y = (self.y + dy.round() as i32).clamp(0, max_y);
        self.layer.set_margin(self.y, 0, 0, self.x);
        self.layer.commit();
    }

    fn change_zoom(&mut self, factor: f32) {
        let old_width = self.display.width();
        let old_height = self.display.height();
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

        self.display =
            imageops::resize(&self.source, width, height, imageops::FilterType::Lanczos3);
        self.x = center_x - width as i32 / 2;
        self.y = center_y - height as i32 / 2;
        let max_x = self.screen_width.saturating_sub(width) as i32;
        let max_y = self.screen_height.saturating_sub(height) as i32;
        self.x = self.x.clamp(0, max_x);
        self.y = self.y.clamp(0, max_y);
        self.layer.set_size(width, height);
        self.layer.set_margin(self.y, 0, 0, self.x);
        self.pending_redraw = true;
        self.layer.commit();
    }
}

fn focused_output_metrics() -> OutputMetrics {
    let fallback = OutputMetrics {
        width: 1920,
        height: 1080,
        scale: 1.0,
    };
    let Ok(output) = Command::new("niri")
        .args(["msg", "-j", "focused-output"])
        .output()
    else {
        return fallback;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return fallback;
    };
    let Some(logical) = value.get("logical") else {
        return fallback;
    };
    let width = logical
        .get("width")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    let height = logical
        .get("height")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    let scale = logical
        .get("scale")
        .and_then(serde_json::Value::as_f64)
        .map(|value| value as f32);
    match (width, height, scale) {
        (Some(width), Some(height), Some(scale)) if width > 0 && height > 0 && scale > 0.0 => {
            OutputMetrics {
                width,
                height,
                scale,
            }
        }
        _ => fallback,
    }
}

fn fitted_size(source: &RgbaImage, output: OutputMetrics) -> (u32, u32) {
    let natural_width = source.width() as f32 / output.scale;
    let natural_height = source.height() as f32 / output.scale;
    let fit = (output.width as f32 * 0.8 / natural_width)
        .min(output.height as f32 * 0.8 / natural_height)
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
        let mut zoom_factor = 1.0;

        for event in events {
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            match event.kind {
                PointerEventKind::Motion { .. } if self.dragging => {
                    if let Some(last) = self.last_pointer {
                        self.move_by(event.position.0 - last.0, event.position.1 - last.1);
                    }
                    self.last_pointer = Some(event.position);
                }
                PointerEventKind::Press { button, .. } if button == LEFT_BUTTON => {
                    self.dragging = true;
                    self.last_pointer = Some(event.position);
                }
                PointerEventKind::Press { button, .. } if button == RIGHT_BUTTON => {
                    self.exit = true;
                }
                PointerEventKind::Release { button, .. } if button == LEFT_BUTTON => {
                    self.dragging = false;
                    self.last_pointer = None;
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
        assert_eq!(size, (1638, 922));
    }
}
