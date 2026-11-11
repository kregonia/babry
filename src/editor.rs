//! 原生截图编辑器：SCTK layer-shell 主表面 + 子表面场景（见 `scene.rs`）。
//!
//! 内存模型：截图 memfd 是唯一的整屏像素副本，同时用于显示与导出；界面控件和
//! 标注瓦片只占用各自的小缓冲区，标注历史只保存矢量命令。
use crate::scene::{RetiredSlot, Scene, SceneInput, TileSlot};
use babry::{
    compositor::{self, Backend, BoundOutput},
    core::RectPx,
    editor_core::{Core, CoreInit, Cursor},
    shm_image::ShmImage,
    smart_select,
};
use smithay_client_toolkit::reexports::client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::registry_queue_init,
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_callback, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm_pool, wl_surface,
    },
};
use smithay_client_toolkit::reexports::protocols::wp::{
    cursor_shape::v1::client::wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    text_input::zv3::client::{
        zwp_text_input_manager_v3::ZwpTextInputManagerV3,
        zwp_text_input_v3::{self, ZwpTextInputV3},
    },
    viewporter::client::{wp_viewport, wp_viewporter},
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, delegate_subcompositor,
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
    subcompositor::SubcompositorState,
};
use std::{
    path::PathBuf,
    sync::{Arc, mpsc::Receiver},
    time::{Duration, Instant},
};

const LEFT: u32 = 0x110;
const RIGHT: u32 = 0x111;

pub struct Session {
    pub source: Arc<ShmImage>,
    pub direct_edit: bool,
    pub save_dir: PathBuf,
    pub output: Option<String>,
    pub logical: (u32, u32),
    pub selection: RectPx,
    /// 后台线程完成分析后送达；`None` 表示直接编辑全屏。
    pub candidates: Option<Receiver<Vec<smart_select::Candidate>>>,
}

pub struct NativeEditor {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    layer: LayerSurface,
    viewport: wp_viewport::WpViewport,
    parts: Option<(
        CompositorState,
        SubcompositorState,
        wp_viewporter::WpViewporter,
    )>,
    pub scene: Option<Scene>,
    core: Core,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    cursor_manager: Option<CursorShapeManager>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    cursor_serial: u32,
    cursor_shape: Option<Shape>,
    /// text-input-v3：中文等输入法的预编辑与提交。
    text_input_manager: Option<ZwpTextInputManagerV3>,
    text_input: Option<ZwpTextInputV3>,
    ime_focus: bool,
    ime_enabled: bool,
    ime_cursor: Option<(i32, i32, i32, i32)>,
    /// KWin 的输入法窗口位于 Overlay 层，输入时将编辑器临时降到 Top 层。
    kde: bool,
    ime_layer: bool,
    ime_pending: (Option<String>, Option<String>),
    modifiers: Modifiers,
    /// 部分虚拟键盘只发送修饰键按键而不更新修饰状态，单独跟踪按下状态。
    held: (bool, bool),
    configured: bool,
    frame_pending: bool,
    frame_requested: Instant,
    started: Instant,
    presented: bool,
    _output: Option<BoundOutput>,
}

fn shape(cursor: Cursor) -> Shape {
    match cursor {
        Cursor::Default => Shape::Default,
        Cursor::Crosshair => Shape::Crosshair,
        Cursor::Move => Shape::Move,
        Cursor::Grab => Shape::Grab,
        Cursor::Grabbing => Shape::Grabbing,
        Cursor::Text => Shape::Text,
        Cursor::Resize(5) => Shape::NwResize,
        Cursor::Resize(6) => Shape::NeResize,
        Cursor::Resize(7) => Shape::SeResize,
        Cursor::Resize(8) => Shape::SwResize,
        Cursor::Resize(1) => Shape::WResize,
        Cursor::Resize(2) => Shape::NResize,
        Cursor::Resize(3) => Shape::EResize,
        Cursor::Resize(_) => Shape::SResize,
    }
}

impl NativeEditor {
    /// 运行编辑器；返回 `Some(选区)` 表示用户选择对该选区做长截图。
    pub fn run(session: Session) -> Result<Option<RectPx>, String> {
        let started = Instant::now();
        let connection =
            Connection::connect_to_env().map_err(|error| format!("无法连接 Wayland: {error}"))?;
        let (globals, mut queue) = registry_queue_init::<NativeEditor>(&connection)
            .map_err(|error| format!("读取 Wayland 全局对象失败: {error}"))?;
        let qh = queue.handle();
        let compositor = CompositorState::bind(&globals, &qh)
            .map_err(|error| format!("wl_compositor 不可用: {error}"))?;
        let subcompositor =
            SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh)
                .map_err(|error| format!("wl_subcompositor 不可用: {error}"))?;
        let shell = LayerShell::bind(&globals, &qh)
            .map_err(|error| format!("wlr-layer-shell 不可用: {error}"))?;
        let shm = Shm::bind(&globals, &qh).map_err(|error| format!("wl_shm 不可用: {error}"))?;
        let viewporter: wp_viewporter::WpViewporter = globals
            .bind(&qh, 1..=1, ())
            .map_err(|error| format!("合成器缺少 wp_viewporter: {error}"))?;
        let cursor_manager = CursorShapeManager::bind(&globals, &qh).ok();
        let text_input_manager: Option<ZwpTextInputManagerV3> = globals.bind(&qh, 1..=1, ()).ok();
        let output_state = OutputState::new(&globals, &qh);
        let registry_state = RegistryState::new(&globals);
        let seat_state = SeatState::new(&globals, &qh);
        // 把覆盖层绑定到截图所在的输出，多显示器时不会出现在错误屏幕。
        let output = session
            .output
            .as_deref()
            .map(|name| compositor::bind_output(&connection, name))
            .transpose()?;
        let surface = compositor.create_surface(&qh);
        let viewport = viewporter.get_viewport(&surface, &qh, ());
        let layer = shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some("babry"),
            output.as_ref().map(|binding| &binding.output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_size(0, 0);
        layer.commit();
        let physical = (session.source.width(), session.source.height());
        let core = Core::new(CoreInit {
            source: session.source,
            physical,
            logical: session.logical,
            selection: session.selection,
            direct_edit: session.direct_edit,
            canvas: false,
            save_dir: session.save_dir,
            output: session.output,
            candidates: session.candidates,
        });
        let mut editor = Self {
            registry_state,
            seat_state,
            output_state,
            shm,
            layer,
            viewport,
            parts: Some((compositor, subcompositor, viewporter)),
            scene: None,
            core,
            pointer: None,
            keyboard: None,
            cursor_manager,
            cursor_device: None,
            cursor_serial: 0,
            cursor_shape: None,
            text_input_manager,
            text_input: None,
            ime_focus: false,
            ime_enabled: false,
            ime_cursor: None,
            kde: Backend::detect().ok() == Some(Backend::Kde),
            ime_layer: false,
            ime_pending: (None, None),
            modifiers: Modifiers::default(),
            held: (false, false),
            configured: false,
            frame_pending: false,
            frame_requested: Instant::now(),
            started,
            presented: false,
            _output: output,
        };
        while !editor.core.exit {
            queue
                .blocking_dispatch(&mut editor)
                .map_err(|error| format!("Wayland 事件循环失败: {error}"))?;
            editor.draw(&qh)?;
        }
        Ok(editor.core.longshot)
    }
}

impl NativeEditor {
    fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        self.core.poll_candidates();
        if !self.configured || self.core.exit || !self.core.dirty {
            return Ok(());
        }
        // 帧回调节流；合成器长时间不回调时也不让界面卡死。
        if self.frame_pending && self.frame_requested.elapsed() < Duration::from_millis(100) {
            return Ok(());
        }
        // 预编辑文字只用于显示，不进入标注历史。
        let committed_len = match self.core.pending.as_mut() {
            Some(mark) if self.core.text_editing && !self.core.preedit.is_empty() => {
                let len = mark.text.len();
                mark.text.push_str(&self.core.preedit);
                Some(len)
            }
            _ => None,
        };
        let caret = self.core.caret_rect();
        self.sync_ime(caret);
        let Some(scene) = self.scene.as_mut() else {
            return Ok(());
        };
        for rect in self.core.damaged.drain(..) {
            scene.damage(rect);
        }
        let core = &mut self.core;
        let pending = core.pending.take();
        let has_pending = pending.is_some();
        core.marks.extend(pending);
        let result = scene.update(
            SceneInput {
                selection: core.selection,
                ui: &core.ui,
                marks: &core.marks,
                source: &core.source,
                fonts: &mut core.fonts,
                preview: has_pending,
                caret,
            },
            qh,
        );
        if has_pending {
            core.pending = core.marks.pop();
        }
        if let (Some(len), Some(mark)) = (committed_len, self.core.pending.as_mut()) {
            mark.text.truncate(len);
        }
        let (changed, complete) = result?;
        self.core.dirty = !complete;
        if changed || !self.presented {
            let surface = self.layer.wl_surface();
            surface.frame(qh, surface.clone());
            self.layer.commit();
            self.frame_pending = true;
            self.frame_requested = Instant::now();
        }
        Ok(())
    }

    /// 文字编辑期间启用输入法，并把光标矩形（表面逻辑坐标）告诉输入法定位候选窗。
    fn sync_ime(&mut self, caret: Option<RectPx>) {
        let lower = self.kde && self.core.text_editing && self.ime_focus;
        if lower != self.ime_layer {
            self.layer
                .set_layer(if lower { Layer::Top } else { Layer::Overlay });
            self.ime_layer = lower;
            // 同步主表面状态，输入法候选窗和状态提示始终显示在编辑器上方。
            self.layer.commit();
        }
        let Some(text_input) = &self.text_input else {
            return;
        };
        let want = self.core.text_editing && self.ime_focus;
        if !want {
            if self.ime_enabled {
                text_input.disable();
                text_input.commit();
                self.ime_enabled = false;
                self.ime_cursor = None;
            }
            return;
        }
        if !self.ime_enabled {
            text_input.enable();
            text_input.set_content_type(
                zwp_text_input_v3::ContentHint::None,
                zwp_text_input_v3::ContentPurpose::Normal,
            );
            self.ime_enabled = true;
            self.ime_cursor = None;
        }
        let scale = self.core.scale();
        let cursor = caret.map(|c| {
            (
                (c.x as f32 / scale).round() as i32,
                (c.y as f32 / scale).round() as i32,
                2,
                ((c.height as f32 / scale).round() as i32).max(1),
            )
        });
        if cursor != self.ime_cursor {
            if let Some((x, y, w, h)) = cursor {
                text_input.set_cursor_rectangle(x, y, w, h);
            }
            text_input.commit();
            self.ime_cursor = cursor;
        }
    }

    fn update_cursor(&mut self, position: (f64, f64)) {
        let shape = shape(self.core.cursor_for(position));
        if self.cursor_shape != Some(shape)
            && let Some(device) = &self.cursor_device
        {
            device.set_shape(self.cursor_serial, shape);
            self.cursor_shape = Some(shape);
        }
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
        self.frame_pending = false;
        if !self.presented {
            self.presented = true;
            if std::env::var_os("BABRY_TRACE").is_some() {
                eprintln!(
                    "babry: 首帧已呈现 {:.1}ms",
                    self.started.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
        if let Err(error) = self.draw(qh) {
            self.core.ui.error = Some(error);
        }
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
            if let (Some(manager), Some(pointer)) = (&self.cursor_manager, &self.pointer) {
                self.cursor_device = Some(manager.get_shape_device(pointer, qh));
            }
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
            if let Some(manager) = &self.text_input_manager {
                self.text_input = Some(manager.get_text_input(&seat, qh, ()));
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
        self.held = (false, false);
    }
    fn press_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.key(event);
        let _ = self.draw(qh);
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.key(event);
        let _ = self.draw(qh);
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        match event.keysym {
            Keysym::Control_L | Keysym::Control_R => self.held.0 = false,
            Keysym::Shift_L | Keysym::Shift_R => self.held.1 = false,
            _ => {}
        }
        self.sync_modifiers();
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
        self.sync_modifiers();
    }
}

impl NativeEditor {
    fn key(&mut self, event: KeyEvent) {
        match event.keysym {
            Keysym::Control_L | Keysym::Control_R => self.held.0 = true,
            Keysym::Shift_L | Keysym::Shift_R => self.held.1 = true,
            _ => {}
        }
        self.sync_modifiers();
        self.core.key(event.keysym, event.utf8.as_deref());
    }

    fn sync_modifiers(&mut self) {
        self.core.ctrl = self.modifiers.ctrl || self.held.0;
        self.core.shift = self.modifiers.shift || self.held.1;
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
            // 指针事件中的 Shift 状态影响直线吸附，先同步修饰键。
            self.sync_modifiers();
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    self.cursor_serial = serial;
                    self.cursor_shape = None;
                    self.core.pointer_enter(event.position);
                }
                PointerEventKind::Leave { .. } => self.core.pointer_leave(),
                PointerEventKind::Press { button: LEFT, .. } => {
                    self.core.handle_press(event.position)
                }
                PointerEventKind::Press { button: RIGHT, .. } => self.core.right_press(),
                PointerEventKind::Release { button: LEFT, .. } => {
                    self.core.handle_release(event.position)
                }
                PointerEventKind::Motion { .. } => self.core.pointer_motion(event.position),
                PointerEventKind::Axis { vertical, .. } => {
                    let steps = if vertical.value120 != 0 {
                        vertical.value120.signum()
                    } else if vertical.discrete != 0 {
                        vertical.discrete.signum()
                    } else {
                        (vertical.absolute.signum()) as i32
                    };
                    self.core
                        .axis(event.position, steps, vertical.absolute * 4.0);
                }
                _ => {}
            }
            self.update_cursor(event.position);
        }
        if let Err(error) = self.draw(qh) {
            self.core.ui.error = Some(error);
        }
    }
}

impl LayerShellHandler for NativeEditor {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.core.exit = true;
    }
    fn configure(
        &mut self,
        connection: &Connection,
        qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if self.configured {
            return;
        }
        let (w, h) = configure.new_size;
        if w > 0 && h > 0 {
            self.core.logical = (w, h);
        }
        self.viewport
            .set_destination(self.core.logical.0 as i32, self.core.logical.1 as i32);
        let Some((compositor, subcompositor, viewporter)) = self.parts.take() else {
            return;
        };
        match Scene::new(
            connection,
            compositor,
            subcompositor,
            viewporter,
            &self.shm,
            self.layer.wl_surface(),
            &self.core.source,
            self.core.logical,
            qh,
        ) {
            Ok(scene) => {
                self.core.ui.grid = scene.grid();
                self.scene = Some(scene);
                self.configured = true;
                self.core.dirty = true;
                if let Err(error) = self.draw(qh) {
                    eprintln!("babry: {error}");
                    self.core.exit = true;
                }
            }
            Err(error) => {
                eprintln!("babry: {error}");
                self.core.exit = true;
            }
        }
    }
}

impl ShmHandler for NativeEditor {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl Dispatch<WlBuffer, TileSlot> for NativeEditor {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        slot: &TileSlot,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(scene) = state.scene.as_mut()
        {
            scene.tile_released(slot.0, qh);
            // 之前因槽位占用而推迟的瓦片在 release 后重试。
            state.core.dirty = true;
        }
    }
}

impl Dispatch<wl_callback::WlCallback, RetiredSlot> for NativeEditor {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        slot: &RetiredSlot,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(scene) = state.scene.as_mut() {
            scene.tile_retired(slot.0);
            state.core.dirty = true;
        }
    }
}

impl Dispatch<ZwpTextInputV3, ()> for NativeEditor {
    fn event(
        state: &mut Self,
        _: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                state.ime_focus = surface == *state.layer.wl_surface();
                state.ime_enabled = false;
                state.core.dirty = true;
            }
            zwp_text_input_v3::Event::Leave { .. } => {
                state.ime_focus = false;
                state.ime_enabled = false;
            }
            zwp_text_input_v3::Event::PreeditString { text, .. } => {
                state.ime_pending.1 = Some(text.unwrap_or_default());
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                state.ime_pending.0 = text;
            }
            zwp_text_input_v3::Event::Done { .. } => {
                let (commit, preedit) = std::mem::take(&mut state.ime_pending);
                state.core.preedit = preedit.unwrap_or_default();
                let commit = commit.unwrap_or_default();
                state.core.refresh_text(|mark| mark.text.push_str(&commit));
            }
            _ => {}
        }
    }
}

delegate_noop!(NativeEditor: ignore ZwpTextInputManagerV3);
delegate_compositor!(NativeEditor);
delegate_subcompositor!(NativeEditor);
delegate_output!(NativeEditor);
delegate_shm!(NativeEditor);
delegate_noop!(NativeEditor: ignore wp_viewporter::WpViewporter);
delegate_noop!(NativeEditor: ignore wp_viewport::WpViewport);
delegate_noop!(NativeEditor: ignore wl_shm_pool::WlShmPool);
delegate_noop!(NativeEditor: ignore WlBuffer);
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
