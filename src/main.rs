//! Babry native Wayland editor.
//!
//! The GUI path is an SCTK layer-shell surface backed by wl_shm. The daemon is
//! only a Unix-socket launcher: it does not initialize Wayland while idle.

use babry::{
    cli, compositor, config, kde,
    marks::{self, Mark},
    native_pin,
    pixels::{self, PixelSize, PixelTarget, RgbaPixels, RgbaViewMut},
    screencopy,
};
use smithay_client_toolkit::reexports::client::{
    Connection, QueueHandle, delegate_noop,
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
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

#[path = "daemon.rs"]
mod daemon;

fn main() -> Result<(), String> {
    if let Err(error) = dispatch(cli::parse(std::env::args())?) {
        let _ = Command::new("notify-send")
            .args(["--app-name", "babry", "Babry", &error])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        eprintln!("babry: {error}");
        return Err(error);
    }
    Ok(())
}

fn dispatch(command: cli::Command) -> Result<(), String> {
    match command {
        cli::Command::Help => {
            println!("{}", cli::usage());
            Ok(())
        }
        cli::Command::ListOutputs => list_outputs(),
        cli::Command::Daemon => run_daemon(),
        cli::Command::DaemonStatus => daemon_status(),
        cli::Command::StopDaemon => stop_daemon(),
        cli::Command::Pin(path) => native_pin(&path, None),
        cli::Command::PinSurface(path) => native_pin(&path, None),
        cli::Command::Region => capture_or_forward(false, None, None),
        cli::Command::Screen => capture_or_forward(true, None, None),
        cli::Command::OnOutput { command, name } => match *command {
            cli::Command::Region => capture_or_forward(false, Some(name), None),
            cli::Command::Screen => capture_or_forward(true, Some(name), None),
            cli::Command::Pin(path) => native_pin(&path, Some(name)),
            cli::Command::PinSurface(path) => native_pin(&path, Some(name)),
            _ => Err("--output 组合无效".into()),
        },
        cli::Command::Standalone(command) => dispatch_standalone(*command, None),
        cli::Command::WithSaveDir { command, directory } => {
            dispatch_wrapped(*command, Some(directory))
        }
        cli::Command::DebugSelection { .. } => {
            Err("--debug-selection 将在原生 CV 阶段重新实现".into())
        }
    }
}

fn dispatch_wrapped(command: cli::Command, save_dir: Option<PathBuf>) -> Result<(), String> {
    match command {
        cli::Command::Standalone(command) => dispatch_standalone(*command, save_dir),
        cli::Command::Region => capture_or_forward(false, None, save_dir),
        cli::Command::Screen => capture_or_forward(true, None, save_dir),
        cli::Command::OnOutput { command, name } => match *command {
            cli::Command::Region => capture_or_forward(false, Some(name), save_dir),
            cli::Command::Screen => capture_or_forward(true, Some(name), save_dir),
            _ => Err("--save-dir 组合无效".into()),
        },
        _ => Err("--save-dir 仅用于截图".into()),
    }
}

fn dispatch_standalone(command: cli::Command, save_dir: Option<PathBuf>) -> Result<(), String> {
    match command {
        cli::Command::Region => capture_session(false, None, save_dir),
        cli::Command::Screen => capture_session(true, None, save_dir),
        cli::Command::OnOutput { command, name } => match *command {
            cli::Command::Region => capture_session(false, Some(name), save_dir),
            cli::Command::Screen => capture_session(true, Some(name), save_dir),
            _ => Err("--standalone 仅用于截图".into()),
        },
        _ => Err("--standalone 仅用于截图".into()),
    }
}

fn capture_or_forward(
    direct_edit: bool,
    output: Option<String>,
    save_dir: Option<PathBuf>,
) -> Result<(), String> {
    let directory = config::resolve_save_dir(save_dir.as_deref())?;
    let request = daemon::Request::Capture {
        direct_edit,
        output: output.clone(),
        save_dir: directory.clone(),
    };
    if let Some(reply) = daemon::send(&request)? {
        return reply.ok.then_some(()).ok_or(reply.message);
    }
    capture_session(direct_edit, output, Some(directory))
}

fn capture_session(
    direct_edit: bool,
    output: Option<String>,
    save_dir: Option<PathBuf>,
) -> Result<(), String> {
    let outputs = compositor::outputs()?;
    let selected = compositor::select_output(&outputs, output.as_deref())?;
    let (source, logical_width, logical_height) = match screencopy::capture(Some(&selected.name)) {
        Ok(frame) => (frame.pixels, selected.width, selected.height),
        Err(error) if compositor::Backend::detect()? == compositor::Backend::Kde => (
            kde::capture(&selected.name).map_err(|_| error)?,
            selected.width,
            selected.height,
        ),
        Err(error) => return Err(error),
    };
    let directory = config::resolve_save_dir(save_dir.as_deref())?;
    NativeEditor::run(
        source,
        direct_edit,
        directory,
        logical_width,
        logical_height,
    )
}

fn list_outputs() -> Result<(), String> {
    for output in compositor::outputs()? {
        println!(
            "{}: {}x{} at ({}, {}) scale {}{}",
            output.name,
            output.width,
            output.height,
            output.x,
            output.y,
            output.scale,
            if output.focused { " [focused]" } else { "" }
        );
    }
    Ok(())
}

fn native_pin(path: &Path, output: Option<String>) -> Result<(), String> {
    native_pin::run_surface(path, output)
}

fn daemon_status() -> Result<(), String> {
    match daemon::send(&daemon::Request::Status)? {
        Some(reply) if reply.ok => {
            println!("{}", reply.message);
            Ok(())
        }
        Some(reply) => Err(reply.message),
        None => {
            println!("后台未运行");
            Ok(())
        }
    }
}

fn stop_daemon() -> Result<(), String> {
    match daemon::send(&daemon::Request::Stop)? {
        Some(reply) if reply.ok => {
            println!("{}", reply.message);
            Ok(())
        }
        Some(reply) => Err(reply.message),
        None => {
            println!("后台未运行");
            Ok(())
        }
    }
}

fn run_daemon() -> Result<(), String> {
    let server = daemon::Server::bind()?;
    let busy = Arc::new(AtomicBool::new(false));
    let current = std::env::current_exe().map_err(|error| error.to_string())?;
    let worker_busy = busy.clone();
    server.run(|request| match request {
        daemon::Request::Status => daemon::Reply::ok(if busy.load(Ordering::Acquire) {
            "后台运行中：正在处理截图"
        } else {
            "后台运行中：空闲"
        }),
        daemon::Request::Stop => daemon::Reply::ok("后台正在退出"),
        daemon::Request::Capture {
            direct_edit,
            output,
            save_dir,
        } => {
            if busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return daemon::Reply::error("后台正在处理上一张截图，请先保存或取消");
            }
            let mut command = vec!["--standalone".to_string()];
            if direct_edit {
                command.push("--screen".into());
            }
            if let Some(output) = output {
                command.extend(["--output".into(), output]);
            }
            command.extend(["--save-dir".into(), save_dir.to_string_lossy().into_owned()]);
            let busy = worker_busy.clone();
            match Command::new(&current)
                .args(command)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
            {
                Ok(mut child) => {
                    thread::spawn(move || {
                        let _ = child.wait();
                        busy.store(false, Ordering::Release);
                    });
                    daemon::Reply::ok("截图请求已接收")
                }
                Err(error) => {
                    worker_busy.store(false, Ordering::Release);
                    daemon::Reply::error(format!("无法启动截图会话: {error}"))
                }
            }
        }
    })
}

struct NativeEditor {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    layer: LayerSurface,
    pool: SlotPool,
    _viewport: Option<wp_viewport::WpViewport>,
    _viewporter: Option<wp_viewporter::WpViewporter>,
    source: RgbaPixels,
    buffers: Vec<smithay_client_toolkit::shm::slot::Buffer>,
    width: u32,
    height: u32,
    logical_width: u32,
    logical_height: u32,
    selection: babry::core::RectPx,
    marks: Vec<Mark>,
    pending_mark: Option<Mark>,
    active_tool: u8,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pressed: bool,
    drag_start: Option<(f64, f64)>,
    modifiers: Modifiers,
    control_down: bool,
    direct_edit: bool,
    save_dir: PathBuf,
    first_configure: bool,
    dirty: bool,
    exit: bool,
}

impl NativeEditor {
    fn run(
        source: RgbaPixels,
        direct_edit: bool,
        save_dir: PathBuf,
        logical_width: u32,
        logical_height: u32,
    ) -> Result<(), String> {
        let connection =
            Connection::connect_to_env().map_err(|error| format!("无法连接 Wayland: {error}"))?;
        let (globals, mut queue) = registry_queue_init::<NativeEditor>(&connection)
            .map_err(|error| format!("读取 Wayland 全局对象失败: {error}"))?;
        let qh = queue.handle();
        let compositor = CompositorState::bind(&globals, &qh)
            .map_err(|error| format!("wl_compositor 不可用: {error}"))?;
        let shell = LayerShell::bind(&globals, &qh)
            .map_err(|error| format!("wlr-layer-shell 不可用: {error}"))?;
        let shm = Shm::bind(&globals, &qh).map_err(|error| format!("wl_shm 不可用: {error}"))?;
        let width = source.width();
        let height = source.height();
        let surface = compositor.create_surface(&qh);
        let viewporter: Option<wp_viewporter::WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
        let viewport = viewporter
            .as_ref()
            .map(|manager| manager.get_viewport(&surface, &qh, ()));
        if let Some(viewport) = &viewport {
            viewport.set_source(0.0, 0.0, width as f64, height as f64);
            viewport.set_destination(logical_width as i32, logical_height as i32);
        }
        let layer = shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("babry"), None);
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_size(logical_width, logical_height);
        layer.commit();
        let pool = SlotPool::new(width as usize * height as usize * 4, &shm)
            .map_err(|error| format!("无法创建编辑共享内存: {error}"))?;
        let mut editor = Self {
            registry_state: RegistryState::new(&globals),
            seat_state: SeatState::new(&globals, &qh),
            output_state: OutputState::new(&globals, &qh),
            shm,
            layer,
            pool,
            _viewport: viewport,
            _viewporter: viewporter,
            source,
            buffers: Vec::new(),
            width,
            height,
            logical_width,
            logical_height,
            selection: babry::core::RectPx::new(0, 0, width, height),
            marks: Vec::new(),
            pending_mark: None,
            active_tool: 0,
            pointer: None,
            keyboard: None,
            pressed: false,
            drag_start: None,
            modifiers: Modifiers::default(),
            control_down: false,
            direct_edit,
            save_dir,
            first_configure: true,
            dirty: true,
            exit: false,
        };
        while !editor.exit {
            queue
                .blocking_dispatch(&mut editor)
                .map_err(|error| format!("Wayland 事件循环失败: {error}"))?;
        }
        Ok(())
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        if !self.dirty && !self.buffers.is_empty() {
            return Ok(());
        }
        let index = self
            .buffers
            .iter()
            .position(|buffer| buffer.canvas(&mut self.pool).is_some())
            .unwrap_or_else(|| {
                let (buffer, _) = self
                    .pool
                    .create_buffer(
                        self.width as i32,
                        self.height as i32,
                        self.width as i32 * 4,
                        wl_shm::Format::Argb8888,
                    )
                    .expect("editor buffer allocation");
                self.buffers.push(buffer);
                self.buffers.len() - 1
            });
        let buffer = &self.buffers[index];
        let canvas = buffer.canvas(&mut self.pool).ok_or("编辑帧仍在使用")?;
        for (index, dst) in canvas.chunks_exact_mut(4).enumerate() {
            let x = index as u32 % self.width;
            let y = index as u32 / self.width;
            let source = self.source.pixel(x, y).unwrap_or([0, 0, 0, 255]);
            let dark = !self.direct_edit
                && !self.selection.contains(babry::core::PointPx {
                    x: x as f32,
                    y: y as f32,
                });
            let factor = if dark { 2 } else { 1 };
            let alpha = u16::from(source[3]);
            dst.copy_from_slice(&[
                ((u16::from(source[2]) * alpha / 255 / factor as u16) as u8),
                ((u16::from(source[1]) * alpha / 255 / factor as u16) as u8),
                ((u16::from(source[0]) * alpha / 255 / factor as u16) as u8),
                source[3],
            ]);
        }
        let mut view = RgbaViewMut::new(PixelSize::new(self.width, self.height)?, canvas)?;
        marks::render_marks(&mut view, &self.marks, (0, 0));
        if let Some(mark) = &self.pending_mark {
            marks::render_marks(&mut view, std::slice::from_ref(mark), (0, 0));
        }
        draw_toolbar(&mut view, self.width, self.height, self.active_tool);
        buffer
            .attach_to(self.layer.wl_surface())
            .map_err(|error| format!("附加编辑帧失败: {error}"))?;
        self.dirty = false;
        self.layer
            .wl_surface()
            .frame(qh, self.layer.wl_surface().clone());
        self.layer
            .wl_surface()
            .damage_buffer(0, 0, self.width as i32, self.height as i32);
        self.layer.commit();
        Ok(())
    }

    fn save(&mut self) -> Result<(), String> {
        let rect = if self.direct_edit {
            babry::core::RectPx::new(0, 0, self.source.width(), self.source.height())
        } else {
            self.selection
        };
        let output = config::output_path(&self.save_dir)?;
        pixels::write_png(&output, &self.source.crop(rect)?)?;
        eprintln!("已保存: {}", output.display());
        self.exit = true;
        Ok(())
    }

    fn copy(&mut self) -> Result<(), String> {
        let rect = if self.direct_edit {
            babry::core::RectPx::new(0, 0, self.source.width(), self.source.height())
        } else {
            self.selection
        };
        let output = config::output_path(&self.save_dir)?;
        pixels::write_png(&output, &self.source.crop(rect)?)?;
        let file = std::fs::File::open(&output).map_err(|error| error.to_string())?;
        let status = Command::new("wl-copy")
            .args(["--type", "image/png"])
            .stdin(file)
            .status()
            .map_err(|error| error.to_string())?;
        if !status.success() {
            return Err(format!("wl-copy 失败: {status}"));
        }
        self.exit = true;
        Ok(())
    }
}

impl CompositorHandler for NativeEditor {
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
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        let _ = self.draw(qh);
    }
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
impl OutputHandler for NativeEditor {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}
impl SeatHandler for NativeEditor {
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
        }
        if capability == Capability::Keyboard {
            self.keyboard = None;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}
impl KeyboardHandler for NativeEditor {
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
        if event.keysym == Keysym::Control_L || event.keysym == Keysym::Control_R {
            self.control_down = true;
        }
        if event.keysym == Keysym::Escape {
            self.exit = true;
        }
        if (self.modifiers.ctrl || self.control_down) && event.keysym == Keysym::s {
            let _ = self.save();
        }
        if (self.modifiers.ctrl || self.control_down) && event.keysym == Keysym::c {
            let _ = self.copy();
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
        event: KeyEvent,
    ) {
        if event.keysym == Keysym::Control_L || event.keysym == Keysym::Control_R {
            self.control_down = false;
        }
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
    }
}
impl PointerHandler for NativeEditor {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if event.surface != *self.layer.wl_surface() {
                continue;
            }
            let physical = (
                event.position.0 * self.width as f64 / self.logical_width.max(1) as f64,
                event.position.1 * self.height as f64 / self.logical_height.max(1) as f64,
            );
            match event.kind {
                PointerEventKind::Press { button: 0x110, .. } => {
                    if let Some(tool) =
                        toolbar_hit(event.position, self.logical_width, self.logical_height)
                    {
                        self.active_tool = tool;
                        self.dirty = true;
                        continue;
                    }
                    self.pressed = true;
                    self.drag_start = Some(physical);
                    if self.direct_edit && self.active_tool != 0 {
                        self.pending_mark = Some(Mark::new(
                            self.active_tool,
                            physical.0 as f32,
                            physical.1 as f32,
                            [255, 59, 48, 255],
                            6,
                        ));
                    }
                }
                PointerEventKind::Release { button: 0x110, .. } => {
                    self.pressed = false;
                    self.drag_start = None;
                    if let Some(mark) = self.pending_mark.take() {
                        self.marks.push(mark);
                        self.dirty = true;
                    }
                }
                PointerEventKind::Motion { .. } if self.pressed => {
                    if let Some(start) = self.drag_start {
                        if let Some(mark) = &mut self.pending_mark {
                            mark.update(physical.0 as f32, physical.1 as f32);
                        } else {
                            let x = start.0.min(physical.0).max(0.0) as u32;
                            let y = start.1.min(physical.1).max(0.0) as u32;
                            let right = start.0.max(physical.0).min(self.width as f64) as u32;
                            let bottom = start.1.max(physical.1).min(self.height as f64) as u32;
                            self.selection = babry::core::RectPx::new(
                                x as i32,
                                y as i32,
                                right.saturating_sub(x),
                                bottom.saturating_sub(y),
                            );
                        }
                        self.dirty = true;
                    }
                }
                _ => {}
            }
        }
        let _ = self.draw(qh);
    }
}
fn toolbar_hit(position: (f64, f64), width: u32, height: u32) -> Option<u8> {
    if position.1 < height as f64 - 60.0 || position.0 < width as f64 - 590.0 {
        return None;
    }
    let index = ((position.0 - (width as f64 - 584.0)) / 36.0).floor() as usize;
    [1, 2, 11, 7].get(index).copied()
}
fn draw_toolbar<T: PixelTarget>(view: &mut T, width: u32, height: u32, active: u8) {
    let left = width.saturating_sub(600);
    let top = height.saturating_sub(72);
    for y in top..height {
        for x in left..width {
            if let Some(p) = view.pixel_mut(x, y) {
                p.copy_from_slice(&[24, 31, 43, 235]);
            }
        }
    }
    for (i, tool) in [1u8, 2, 11, 7].iter().enumerate() {
        let x = left + 16 + i as u32 * 36;
        if *tool == active {
            for y in top + 8..top + 48 {
                for xx in x..x + 30 {
                    if let Some(p) = view.pixel_mut(xx, y) {
                        p.copy_from_slice(&[90, 75, 170, 255]);
                    }
                }
            }
        }
    }
}
impl LayerShellHandler for NativeEditor {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        if self.first_configure {
            self.first_configure = false;
            let _ = self.draw(qh);
        }
    }
}
impl ShmHandler for NativeEditor {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(NativeEditor);
delegate_noop!(NativeEditor: ignore wp_viewporter::WpViewporter);
delegate_noop!(NativeEditor: ignore wp_viewport::WpViewport);
delegate_output!(NativeEditor);
delegate_shm!(NativeEditor);
delegate_seat!(NativeEditor);
delegate_keyboard!(NativeEditor);
delegate_pointer!(NativeEditor);
delegate_layer!(NativeEditor);
delegate_registry!(NativeEditor);
impl ProvidesRegistryState for NativeEditor {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}
